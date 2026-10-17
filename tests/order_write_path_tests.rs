//! OPT-B (capacity, 2026-10-17): the order write path in fewer statements.
//!
//! A sale's lines, add-ons, optional fields, payment legs, stock movements and
//! kitchen lines are written in multi-row statements, and the changefeed
//! triggers of the tables a sale writes many rows into fire once per
//! statement. These pin what that must NOT change: the parent a row emits,
//! the stock balances a run of movements chains through, the station a line
//! routes to, the child rows a ticket line mirrors into — and the storage
//! settings the migration sets.

use sqlx::PgPool;
use uuid::Uuid;

struct Shop {
    org: Uuid,
    branch: Uuid,
    teller: Uuid,
    till: Uuid,
}

async fn shop(pool: &PgPool) -> Shop {
    let s = Shop {
        org: Uuid::new_v4(),
        branch: Uuid::new_v4(),
        teller: Uuid::new_v4(),
        till: Uuid::new_v4(),
    };
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations (id, name, slug) VALUES ('{org}', 'OB', 'ob-{org}');
         INSERT INTO users (id, name, role, org_id, pin_hash) VALUES ('{teller}', 'T', 'teller', '{org}', 'x');
         INSERT INTO branches (id, org_id, name, code) VALUES ('{branch}', '{org}', 'B', 'OB');
         INSERT INTO tills (id, branch_id, teller_id, opening_cash) VALUES ('{till}', '{branch}', '{teller}', 0);",
        org = s.org,
        teller = s.teller,
        branch = s.branch,
        till = s.till,
    ))
    .execute(pool)
    .await
    .unwrap();
    s
}

async fn order(pool: &PgPool, s: &Shop, n: i32) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO orders (branch_id, till_id, teller_id, order_number, status, \
                             payment_method, subtotal, total_amount, order_ref) \
         VALUES ($1, $2, $3, $4, 'completed', 'cash', 1000, 1000, gen_random_uuid()::text) RETURNING id",
    )
    .bind(s.branch)
    .bind(s.till)
    .bind(s.teller)
    .bind(n)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn seq_of(pool: &PgPool, kind: &str, id: Uuid) -> Option<i64> {
    sqlx::query_scalar("SELECT seq FROM sync_changes WHERE type = $1 AND entity_id = $2")
        .bind(kind)
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
}

async fn seq_now(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COALESCE(max(seq), 0) FROM sync_changes")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The five many-rows-per-statement tables emit once per statement, under the
/// names and functions the emitter coverage test reads.
#[sqlx::test]
async fn their_changefeed_triggers_are_statement_level(pool: PgPool) {
    let rows: Vec<(String, String, bool, String)> = sqlx::query_as(
        "SELECT c.relname::text, t.tgname::text, (t.tgtype & 1) = 0, p.proname::text
           FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_proc p ON p.oid = t.tgfoid
          WHERE c.relname IN ('order_items', 'order_payments', 'order_deals',
                              'kitchen_ticket_items', 'open_ticket_items')
            AND t.tgname LIKE 'sync_emit%' ORDER BY 1, 2",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    for t in ["kitchen_ticket_items", "open_ticket_items", "order_deals", "order_items", "order_payments"] {
        let mine: Vec<_> = rows.iter().filter(|r| r.0 == t).collect();
        let names: Vec<&str> = mine.iter().map(|r| r.1.as_str()).collect();
        assert_eq!(names, ["sync_emit", "sync_emit_del", "sync_emit_upd"], "{t}");
        assert!(mine.iter().all(|r| r.2), "{t}: every one FOR EACH STATEMENT");
        assert!(mine.iter().all(|r| r.3 == format!("sync_emit_{t}")), "{t}: its own function");
    }
}

/// Three lines in one INSERT bump their order's changefeed row once (one
/// nextval), where the row trigger bumped it three times. The row is the same.
#[sqlx::test]
async fn a_multi_line_insert_bumps_its_order_once(pool: PgPool) {
    let s = shop(&pool).await;
    let lines = |o: Uuid, n: i32| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO order_items (order_id, item_name, unit_price, quantity, line_total)
                 SELECT $1, 'L' || g, 100, 1, 100 FROM generate_series(1, $2) g",
            )
            .bind(o)
            .bind(n)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    // What one emit costs the sequence (an upsert draws more than one value).
    let one = order(&pool, &s, 9).await;
    let mark = seq_now(&pool).await;
    lines(one, 1).await;
    let per_emit = seq_now(&pool).await - mark;
    assert!(per_emit > 0);

    let o = order(&pool, &s, 1).await;
    let before = seq_of(&pool, "order", o).await.expect("the order emitted");
    lines(o, 3).await;
    let after = seq_of(&pool, "order", o).await.unwrap();
    assert_eq!(after, before + per_emit, "one bump for the statement, not one per line");
    let op: String = sqlx::query_scalar("SELECT op FROM sync_changes WHERE entity_id = $1")
        .bind(o)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(op, "upsert");

    // Two payment legs in one statement: one bump, and the order still turns
    // `mixed` (that row trigger is unchanged).
    sqlx::query(
        "INSERT INTO order_payments (order_id, method, amount, is_cash)
         VALUES ($1, 'cash', 500, true), ($1, 'card', 500, false)",
    )
    .bind(o)
    .execute(&pool)
    .await
    .unwrap();
    let method: String = sqlx::query_scalar("SELECT payment_method::text FROM orders WHERE id = $1")
        .bind(o)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(method, "mixed");
    assert!(seq_of(&pool, "order", o).await.unwrap() > after);
}

/// UPDATE and DELETE emit what the row trigger did: a moved line its old AND
/// its new order, a changed line its order, a deleted line its order; a
/// statement that matches nothing emits nothing.
#[sqlx::test]
async fn line_updates_and_deletes_emit_their_orders(pool: PgPool) {
    let s = shop(&pool).await;
    let (a, b) = (order(&pool, &s, 1).await, order(&pool, &s, 2).await);
    let line: Uuid = sqlx::query_scalar(
        "INSERT INTO order_items (order_id, item_name, unit_price, quantity, line_total)
         VALUES ($1, 'L', 100, 1, 100) RETURNING id",
    )
    .bind(a)
    .fetch_one(&pool)
    .await
    .unwrap();

    let mark = seq_now(&pool).await;
    sqlx::query("UPDATE order_items SET notes = 'x' WHERE id = $1")
        .bind(line)
        .execute(&pool)
        .await
        .unwrap();
    assert!(seq_of(&pool, "order", a).await.unwrap() > mark, "an edited line emits its order");
    assert!(seq_of(&pool, "order", b).await.unwrap() < mark, "and only its order");

    let mark = seq_now(&pool).await;
    sqlx::query("UPDATE order_items SET order_id = $2 WHERE id = $1")
        .bind(line)
        .bind(b)
        .execute(&pool)
        .await
        .unwrap();
    assert!(seq_of(&pool, "order", a).await.unwrap() > mark, "a moved line emits the order it left");
    assert!(seq_of(&pool, "order", b).await.unwrap() > mark, "and the one it joined");

    let mark = seq_now(&pool).await;
    sqlx::query("UPDATE order_items SET notes = 'y' WHERE id = $1")
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(seq_now(&pool).await, mark, "no row, no emit");

    sqlx::query("DELETE FROM order_items WHERE id = $1")
        .bind(line)
        .execute(&pool)
        .await
        .unwrap();
    assert!(seq_of(&pool, "order", b).await.unwrap() > mark, "a deleted line emits its order");
}

/// Kitchen lines and ticket lines in one statement emit their parent once;
/// ticket lines still mirror their add-ons / optional fields into the child
/// tables, line by line, in position order — and an edited line re-mirrors.
#[sqlx::test]
async fn kitchen_and_ticket_lines_emit_their_parents_and_mirror_children(pool: PgPool) {
    let s = shop(&pool).await;
    let o = order(&pool, &s, 1).await;
    let kt: Uuid = sqlx::query_scalar(
        "INSERT INTO kitchen_tickets (org_id, branch_id, order_id) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(s.org)
    .bind(s.branch)
    .bind(o)
    .fetch_one(&pool)
    .await
    .unwrap();
    let kitchen_lines = |n: i32| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO kitchen_ticket_items (kitchen_ticket_id, line, qty)
                 SELECT $1, '{}'::jsonb, g FROM generate_series(1, $2) g",
            )
            .bind(kt)
            .bind(n)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    let mark = seq_now(&pool).await;
    kitchen_lines(1).await;
    let per_emit = seq_now(&pool).await - mark;
    let mark = seq_now(&pool).await;
    kitchen_lines(2).await;
    assert_eq!(seq_of(&pool, "kitchen_ticket", kt).await, Some(mark + per_emit), "one bump");

    let ticket: Uuid = sqlx::query_scalar(
        "INSERT INTO open_tickets (org_id, branch_id, opened_by) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(s.org)
    .bind(s.branch)
    .bind(s.teller)
    .fetch_one(&pool)
    .await
    .unwrap();
    let round: Uuid = sqlx::query_scalar(
        "INSERT INTO open_ticket_rounds (open_ticket_id, round_number, fired_by) VALUES ($1, 1, $2) RETURNING id",
    )
    .bind(ticket)
    .bind(s.teller)
    .fetch_one(&pool)
    .await
    .unwrap();
    let (a1, a2, opt) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let line = |qty: i32, addons: serde_json::Value, opts: serde_json::Value| {
        serde_json::json!({
            "qty": qty, "name": "Latte", "unit_price": 100, "line_total": 100 * qty, "modifiers": [],
            "input": {"quantity": qty, "addons": addons, "optional_field_ids": opts}
        })
    };
    let l1 = line(1, serde_json::json!([{"addon_item_id": a1, "quantity": 2, "unit_price": 50},
                                        {"addon_item_id": a2}]), serde_json::json!([opt]));
    let l2 = line(2, serde_json::json!([]), serde_json::json!([]));
    let mark = seq_now(&pool).await;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "INSERT INTO open_ticket_items (open_ticket_id, round_id, line, line_total)
         VALUES ($1, $2, $3, 100), ($1, $2, $4, 200) RETURNING id",
    )
    .bind(ticket)
    .bind(round)
    .bind(&l1)
    .bind(&l2)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(seq_of(&pool, "open_ticket", ticket).await.unwrap() > mark);
    let addons: Vec<(Uuid, i32, Uuid, i32, i32)> = sqlx::query_as(
        "SELECT item_id, position, addon_item_id, quantity, unit_price FROM open_ticket_item_addons
          WHERE item_id = ANY($1) ORDER BY item_id, position",
    )
    .bind(&ids)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(addons, vec![(ids[0], 0, a1, 2, 50), (ids[0], 1, a2, 1, 0)]);
    let opts: Vec<(Uuid, i32, Uuid)> = sqlx::query_as(
        "SELECT item_id, position, option_id FROM open_ticket_item_optionals WHERE item_id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(opts, vec![(ids[0], 0, opt)]);

    // An edit of the line (the row trigger, unchanged) replaces its children.
    sqlx::query("UPDATE open_ticket_items SET line = $2 WHERE id = $1")
        .bind(ids[0])
        .bind(line(1, serde_json::json!([{"addon_item_id": a2, "quantity": 3}]), serde_json::json!([])))
        .execute(&pool)
        .await
        .unwrap();
    let addons: Vec<(Uuid, i32)> = sqlx::query_as(
        "SELECT addon_item_id, quantity FROM open_ticket_item_addons WHERE item_id = $1",
    )
    .bind(ids[0])
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(addons, vec![(a2, 3)]);
}

/// A run of movements in one statement chains exactly as one statement per
/// movement: two entries for one ingredient see each other's balance, and
/// each comes back in the order given.
#[sqlx::test]
async fn record_movements_chains_like_one_statement_each(pool: PgPool) {
    use madar_rust::inventory::movements::{MovementParams, record_movement, record_movements};
    let s = shop(&pool).await;
    let other = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name, code) VALUES ($1, $2, 'B2', 'OB2')")
        .bind(other)
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let (milk, beans): (Uuid, Uuid) = (Uuid::new_v4(), Uuid::new_v4());
    for (id, name) in [(milk, "Milk"), (beans, "Beans")] {
        sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit) VALUES ($1, $2, $3, 'g')")
            .bind(id)
            .bind(s.org)
            .bind(name)
            .execute(&pool)
            .await
            .unwrap();
    }
    let entries = |branch: Uuid| {
        [(milk, -100.0), (milk, -50.5), (beans, -18.0), (milk, 20.0)].map(|(ing, q)| MovementParams {
            branch_id: branch,
            org_ingredient_id: ing,
            movement_type: "sale",
            quantity: q,
            unit_cost: rust_decimal::Decimal::from_f64_retain(4.25),
            reason: None,
            source_type: Some("order"),
            source_id: None,
            note: None,
            created_by: Some(s.teller),
        })
    };
    let mut tx = pool.begin().await.unwrap();
    let batch = record_movements(&mut *tx, Some(s.org), &entries(s.branch)).await.unwrap();
    let mut one_by_one = Vec::new();
    for p in entries(other) {
        one_by_one.push(record_movement(&mut *tx, p).await.unwrap());
    }
    tx.commit().await.unwrap();
    let got: Vec<(f64, bool)> = batch.iter().map(|m| (m.balance_after, m.below_zero)).collect();
    let want: Vec<(f64, bool)> = one_by_one.iter().map(|m| (m.balance_after, m.below_zero)).collect();
    assert_eq!(got, want);
    assert_eq!(got, vec![(-100.0, true), (-150.5, true), (-18.0, true), (-130.5, true)]);
    // The stored rows match too, column for column (ids, branch and time aside).
    let rows = |branch: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (String, String, i64, String, Option<i64>, String, bool, Uuid)>(
                "SELECT org_ingredient_id::text, quantity::text, 0::bigint, balance_after::text, unit_cost,
                        unit_cost_exact::text, below_zero, org_id
                   FROM inventory_movements WHERE branch_id = $1 ORDER BY balance_after, quantity",
            )
            .bind(branch)
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(rows(s.branch).await, rows(other).await);
}

/// The batched station lookup gives every line the station the one-line
/// lookup gives it: item route, then category route, then the default, and
/// an item-less line the default.
#[sqlx::test]
async fn resolve_stations_matches_resolve_station(pool: PgPool) {
    use madar_rust::kitchen::{resolve_station, resolve_stations};
    let s = shop(&pool).await;
    let cat = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'Hot')")
        .bind(cat)
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let items: Vec<Uuid> = (0..4).map(|_| Uuid::new_v4()).collect();
    for (i, id) in items.iter().enumerate() {
        sqlx::query(
            "INSERT INTO menu_items (id, org_id, category_id, name, base_price) VALUES ($1, $2, $3, $4, 100)",
        )
        .bind(id)
        .bind(s.org)
        .bind((i != 3).then_some(cat))
        .bind(format!("I{i}"))
        .execute(&pool)
        .await
        .unwrap();
    }
    let station = |name: &'static str, default: bool, active: bool| {
        let pool = pool.clone();
        let (org, branch) = (s.org, s.branch);
        async move {
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO kitchen_stations (org_id, branch_id, name, is_default, is_active)
                 VALUES ($1, $2, $3, $4, $5) RETURNING id",
            )
            .bind(org)
            .bind(branch)
            .bind(name)
            .bind(default)
            .bind(active)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let (grill, bar, off, pass) = (
        station("Grill", false, true).await,
        station("Bar", false, true).await,
        station("Off", false, false).await,
        station("Pass", true, true).await,
    );
    sqlx::raw_sql(&format!(
        "INSERT INTO menu_item_station_routes (branch_id, menu_item_id, station_id) VALUES ('{b}', '{i0}', '{grill}');
         INSERT INTO menu_item_station_routes (branch_id, menu_item_id, station_id) VALUES ('{b}', '{i2}', '{off}');
         INSERT INTO category_station_routes (branch_id, category_id, station_id) VALUES ('{b}', '{cat}', '{bar}');",
        b = s.branch,
        i0 = items[0],
        i2 = items[2],
    ))
    .execute(&pool)
    .await
    .unwrap();
    let lines: Vec<Option<Uuid>> = items.iter().copied().map(Some).chain([None, Some(Uuid::new_v4())]).collect();
    let mut tx = pool.begin().await.unwrap();
    let batch = resolve_stations(&mut tx, s.branch, &lines).await.unwrap();
    let mut single = Vec::new();
    for l in &lines {
        single.push(resolve_station(&mut tx, s.branch, *l).await.unwrap());
    }
    assert_eq!(batch, single);
    assert_eq!(
        batch,
        vec![Some(grill), Some(bar), Some(bar), Some(pass), Some(pass), Some(pass)],
        "item route; category; inactive item route falls to the category; no category; no item; unknown item"
    );
}

/// The duplicate updated_at trigger is gone and an order UPDATE still stamps
/// updated_at; the storage settings are on the hot tables.
#[sqlx::test]
async fn one_updated_at_trigger_and_the_storage_settings(pool: PgPool) {
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_trigger t JOIN pg_proc p ON p.oid = t.tgfoid
          WHERE t.tgrelid = 'orders'::regclass AND p.proname = 'set_updated_at'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 1);
    let s = shop(&pool).await;
    let o = order(&pool, &s, 1).await;
    sqlx::query("UPDATE orders SET updated_at = now() - interval '1 day' WHERE id = $1")
        .bind(o)
        .execute(&pool)
        .await
        .unwrap();
    let fresh: bool = sqlx::query_scalar("SELECT updated_at > now() - interval '1 minute' FROM orders WHERE id = $1")
        .bind(o)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(fresh, "BEFORE UPDATE still sets updated_at = now()");

    let opts: Vec<(String, Vec<String>)> = sqlx::query_as(
        "SELECT relname::text, reloptions FROM pg_class
          WHERE relname IN ('branch_stock', 'sync_changes', 'ref_counters', 'orders') AND reloptions IS NOT NULL
          ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let of = |t: &str| opts.iter().find(|o| o.0 == t).map(|o| o.1.clone()).unwrap_or_default();
    assert!(of("branch_stock").contains(&"fillfactor=70".to_string()), "{opts:?}");
    assert!(of("ref_counters").contains(&"fillfactor=50".to_string()), "{opts:?}");
    assert!(of("sync_changes").contains(&"autovacuum_vacuum_scale_factor=0.01".to_string()), "{opts:?}");
    assert!(of("orders").contains(&"autovacuum_vacuum_insert_scale_factor=0.05".to_string()), "{opts:?}");
}
