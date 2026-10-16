//! DB architecture Phase 1 (DB_ARCHITECTURE_AUDIT_2026-10-16 §8): the org_id
//! fill triggers, the composite tenant keys, the discriminator CHECKs, the
//! rollups, the compat counters, the CONCURRENTLY-index boot guard, the
//! transient-error classifier and the (inert) minimum-client gate.

mod common;

use actix_web::{App, HttpResponse, test as atest, web};
use madar_rust::version_gate::{self, Policy, Verdict};
use sqlx::PgPool;
use uuid::Uuid;

struct Seed {
    org: Uuid,
    branch: Uuid,
    till: Uuid,
    order: Uuid,
    item: Uuid,
}

async fn seed(pool: &PgPool, label: &str) -> Seed {
    let (org, teller, branch, till, order, item) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, $2, $3)")
        .bind(org)
        .bind(format!("Org {label}"))
        .bind(format!("org-{}", org.simple()))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO users (id, name, role, org_id, pin_hash) VALUES ($1, $2, 'teller', $3, 'x')")
        .bind(teller)
        .bind(format!("Teller {label}"))
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO branches (id, org_id, name, code) VALUES ($1, $2, $3, $4)")
        .bind(branch)
        .bind(org)
        .bind(format!("Branch {label}"))
        .bind(org.simple().to_string()[..6].to_uppercase())
        .execute(pool)
        .await
        .unwrap();
    // No org_id named anywhere below: the fill triggers supply it.
    sqlx::query("INSERT INTO tills (id, branch_id, teller_id) VALUES ($1, $2, $3)")
        .bind(till)
        .bind(branch)
        .bind(teller)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO orders (id, branch_id, till_id, teller_id, order_number, payment_method, order_ref,
                             subtotal, total_amount)
         VALUES ($1, $2, $3, $4, 1, 'cash', $5, 300, 300)",
    )
    .bind(order)
    .bind(branch)
    .bind(till)
    .bind(teller)
    .bind(format!("REF-{}", &order.simple().to_string()[..8]))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO order_items (id, order_id, item_name, unit_price, quantity, line_total)
         VALUES ($1, $2, 'Latte', 100, 3, 300)",
    )
    .bind(item)
    .bind(order)
    .execute(pool)
    .await
    .unwrap();
    let addon = common::legacy_catalog::addon_item(pool, None, org, "Oat", "milk_type", 20).await;
    sqlx::query(
        "INSERT INTO order_item_addons (order_item_id, addon_item_id, addon_name, unit_price, quantity, line_total)
         VALUES ($1, $2, 'Oat', 20, 1, 20)",
    )
    .bind(item)
    .bind(addon)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO order_payments (order_id, method, amount, is_cash, till_id) VALUES ($1, 'cash', 300, true, $2)")
        .bind(order)
        .bind(till)
        .execute(pool)
        .await
        .unwrap();
    Seed {
        org,
        branch,
        till,
        order,
        item,
    }
}

async fn org_of(pool: &PgPool, table: &str, col: &str, id: Uuid) -> Option<Uuid> {
    sqlx::query_scalar(&format!("SELECT org_id FROM {table} WHERE {col} = $1 LIMIT 1"))
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn inserts_that_name_no_org_get_their_parents_org(pool: PgPool) {
    let s = seed(&pool, "A").await;
    assert_eq!(org_of(&pool, "orders", "id", s.order).await, Some(s.org));
    assert_eq!(org_of(&pool, "tills", "id", s.till).await, Some(s.org));
    assert_eq!(org_of(&pool, "order_items", "id", s.item).await, Some(s.org));
    assert_eq!(org_of(&pool, "order_item_addons", "order_item_id", s.item).await, Some(s.org));
    assert_eq!(org_of(&pool, "order_payments", "order_id", s.order).await, Some(s.org));
    // The changefeed rows the sync_emit triggers wrote carry it too.
    let feed: Vec<Option<Uuid>> =
        sqlx::query_scalar("SELECT org_id FROM sync_changes WHERE branch_id = $1")
            .bind(s.branch)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(!feed.is_empty());
    assert!(feed.iter().all(|o| *o == Some(s.org)), "{feed:?}");
}

#[sqlx::test]
async fn a_row_cannot_name_another_orgs_parent(pool: PgPool) {
    let a = seed(&pool, "A").await;
    let b = seed(&pool, "B").await;
    // An order line claiming org B for org A's order.
    let err = sqlx::query(
        "INSERT INTO order_items (order_id, org_id, item_name, unit_price, line_total)
         VALUES ($1, $2, 'x', 1, 1)",
    )
    .bind(a.order)
    .bind(b.org)
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        err.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("23503"),
        "{err}"
    );
    // And a line whose parent does not exist at all is refused with the
    // FK's own code, as before (the fill trigger raises it first).
    let err = sqlx::query(
        "INSERT INTO order_items (order_id, item_name, unit_price, line_total) VALUES ($1, 'x', 1, 1)",
    )
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(err.as_database_error().and_then(|e| e.code()).as_deref(), Some("23503"));
}

#[sqlx::test]
async fn every_phase1_constraint_is_validated(pool: PgPool) {
    let not_valid: Vec<String> = sqlx::query_scalar(
        "SELECT conrelid::regclass::text || '.' || conname FROM pg_constraint
          WHERE NOT convalidated
            AND (conname LIKE '%\\_org\\_id\\_present' OR conname LIKE '%\\_org\\_%\\_fk'
                 OR conname IN ('inventory_movements_source_type_check', 'sync_changes_type_check'))",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(not_valid.is_empty(), "{not_valid:?}");
    // Phase 2.2 turned the 22 `<t>_org_id_present` CHECKs into NOT NULL and
    // dropped them: none left, and every one of those columns is NOT NULL.
    let present: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint WHERE conname LIKE '%\\_org\\_id\\_present'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(present, 0);
    let nullable: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.columns
          WHERE table_schema = 'public' AND column_name = 'org_id' AND is_nullable = 'YES'
            AND table_name = ANY($1)",
    )
    .bind(
        [
            "orders", "tills", "inventory_movements", "branch_stock", "sync_changes",
            "order_items", "order_payments", "till_cash_movements", "open_ticket_rounds",
            "open_ticket_items", "kitchen_ticket_items", "menu_item_sizes", "modifier_options",
            "recipe_lines", "stocktake_items", "purchase_order_lines", "goods_receipt_lines",
            "stock_transfer_lines", "packaging_rule_lines", "recipe_base_lines",
            "order_item_addons", "order_item_optionals",
        ]
        .map(String::from)
        .to_vec(),
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(nullable.is_empty(), "{nullable:?}");
    let invalid_indexes: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_index WHERE NOT indisvalid")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(invalid_indexes, 0);
}

/// The CHECK on `sync_changes.type` is madar-sync's `ALL_TYPES`, and covers
/// every type a SQL emitter writes. A new sync type must extend the CHECK in
/// its own migration, or its writes would fail: this test says so first.
#[sqlx::test]
async fn sync_changes_type_is_madar_sync_all_types(pool: PgPool) {
    let def: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'sync_changes_type_check'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut in_check: Vec<String> = def
        .split('\'')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect();
    in_check.sort();
    in_check.dedup();
    let mut all: Vec<String> = madar_sync::ALL_TYPES.iter().map(|t| t.to_string()).collect();
    all.sort();
    assert_eq!(in_check, all, "sync_changes_type_check must list exactly madar_sync::ALL_TYPES");
    let emitted: Vec<String> = sqlx::query_scalar(
        r"SELECT DISTINCT m[1] FROM pg_proc,
              regexp_matches(prosrc, 'sync_emit(?:_org)?\(\s*[^,]+,\s*''([a-z_]+)''', 'g') m",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(!emitted.is_empty());
    for t in emitted {
        assert!(all.contains(&t), "a SQL emitter writes sync type `{t}`, which the CHECK refuses");
    }
}

#[sqlx::test]
async fn an_unknown_movement_source_type_is_refused(pool: PgPool) {
    let s = seed(&pool, "A").await;
    let ing: Uuid = sqlx::query_scalar(
        "INSERT INTO org_ingredients (org_id, name, unit) VALUES ($1, 'Milk', 'ml') RETURNING id",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    let insert = |source: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO inventory_movements (branch_id, org_ingredient_id, type, quantity, source_type)
                 VALUES ($1, $2, 'adjustment_add', 1, $3)",
            )
            .bind(s.branch)
            .bind(ing)
            .bind(source)
            .execute(&pool)
            .await
        }
    };
    insert("order").await.expect("a known source type");
    let err = insert("bogus").await.unwrap_err();
    assert_eq!(err.as_database_error().and_then(|e| e.code()).as_deref(), Some("23514"));
}

#[sqlx::test]
async fn sales_daily_recompute_is_idempotent_and_matches_the_ledger(pool: PgPool) {
    let s = seed(&pool, "A").await;
    let day: chrono::NaiveDate = sqlx::query_scalar(
        "SELECT (created_at AT TIME ZONE effective_timezone(branch_id))::date FROM orders WHERE id = $1",
    )
    .bind(s.order)
    .fetch_one(&pool)
    .await
    .unwrap();
    for _ in 0..2 {
        let n = madar_rust::rollups::recompute_till(&pool, s.till).await.unwrap();
        assert_eq!(n, 1);
    }
    let (orders, total): (i32, i64) = sqlx::query_as(
        "SELECT orders_count, total_amount FROM sales_daily_branch WHERE branch_id = $1 AND business_date = $2",
    )
    .bind(s.branch)
    .bind(day)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((orders, total), (1, 300));
    let (qty, line_total): (i64, i64) = sqlx::query_as(
        "SELECT quantity, line_total FROM sales_daily_item WHERE branch_id = $1 AND business_date = $2",
    )
    .bind(s.branch)
    .bind(day)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((qty, line_total), (3, 300));
    let addon: i64 = sqlx::query_scalar("SELECT line_total FROM sales_daily_modifier WHERE branch_id = $1")
        .bind(s.branch)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(addon, 20);
    let pay: (i32, i64) = sqlx::query_as(
        "SELECT payments_count, amount FROM sales_daily_payment WHERE branch_id = $1 AND is_cash",
    )
    .bind(s.branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pay, (1, 300));
    // A void moves the day out of the sums, not out of the count.
    sqlx::query("UPDATE orders SET status = 'voided', voided_at = now(), void_reason = 'wrong_order', voided_by = teller_id WHERE id = $1")
        .bind(s.order)
        .execute(&pool)
        .await
        .unwrap();
    madar_rust::rollups::recompute_till(&pool, s.till).await.unwrap();
    let (orders, voided, total): (i32, i32, i64) = sqlx::query_as(
        "SELECT orders_count, voided_count, total_amount FROM sales_daily_branch WHERE branch_id = $1",
    )
    .bind(s.branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((orders, voided, total), (0, 1, 0));
    let items: i64 = sqlx::query_scalar("SELECT count(*) FROM sales_daily_item WHERE branch_id = $1")
        .bind(s.branch)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(items, 0);
}

#[sqlx::test]
async fn compat_usage_counts_and_flushes(pool: PgPool) {
    let org = Uuid::new_v4();
    for _ in 0..3 {
        madar_rust::compat_usage::count("test_kind", "site-a", Some(org));
    }
    madar_rust::compat_usage::flush(&pool).await.unwrap();
    madar_rust::compat_usage::count("test_kind", "site-a", Some(org));
    madar_rust::compat_usage::flush(&pool).await.unwrap();
    let hits: i64 = sqlx::query_scalar(
        "SELECT hits FROM compat_usage WHERE kind = 'test_kind' AND site = 'site-a' AND org_id = $1",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(hits, 4);
}

#[test]
fn concurrent_index_names_are_read_from_no_transaction_files() {
    use madar_rust::migrate_guard::concurrent_index_name as name;
    assert_eq!(
        name("-- no-transaction\nCREATE INDEX CONCURRENTLY IF NOT EXISTS idx_a ON t (x);").as_deref(),
        Some("idx_a")
    );
    assert_eq!(
        name("-- no-transaction\nCREATE UNIQUE INDEX CONCURRENTLY uq_b ON t (x);").as_deref(),
        Some("uq_b")
    );
    // Phase 2.3 drops redundant indexes the same way, one per file.
    assert_eq!(
        name("-- no-transaction\nDROP INDEX CONCURRENTLY IF EXISTS idx_d;").as_deref(),
        Some("idx_d")
    );
    assert_eq!(name("CREATE INDEX CONCURRENTLY idx_c ON t (x);"), None);
    assert_eq!(name("-- no-transaction\nVACUUM t;"), None);
    // Every no-transaction migration in the tree names exactly one index.
    for m in madar_rust::MIGRATOR.iter().filter(|m| m.no_tx) {
        assert!(name(&m.sql).is_some(), "{} names no CONCURRENTLY index", m.version);
        let code: String = m
            .sql
            .lines()
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            code.matches(';').count(),
            1,
            "{}: a -- no-transaction file must hold exactly one statement",
            m.version
        );
    }
}

/// A CONCURRENTLY build that failed leaves an INVALID index; the boot guard
/// drops it when its migration is still pending, and leaves everything else.
#[sqlx::test]
async fn the_boot_guard_drops_the_invalid_index_of_a_pending_migration(pool: PgPool) {
    sqlx::raw_sql(
        "CREATE TABLE p1_dup (v int); INSERT INTO p1_dup VALUES (1), (1);
         CREATE INDEX p1_other ON p1_dup (v);",
    )
    .execute(&pool)
    .await
    .unwrap();
    // A unique build over duplicates fails and leaves `p1_dup_uq` INVALID.
    let failed = sqlx::raw_sql("CREATE UNIQUE INDEX CONCURRENTLY p1_dup_uq ON p1_dup (v)")
        .execute(&pool)
        .await;
    assert!(failed.is_err());
    let valid = |name: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, bool>(
                "SELECT i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = $1",
            )
            .bind(name)
            .fetch_optional(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(valid("p1_dup_uq").await, Some(false));

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("99990101000000_p1_dup_uq.sql"),
        "-- no-transaction\nCREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS p1_dup_uq ON p1_dup (v);\n",
    )
    .unwrap();
    let pending = sqlx::migrate::Migrator::new(dir.path()).await.unwrap();
    let dropped = madar_rust::migrate_guard::drop_invalid_pending_indexes(&pool, &pending)
        .await
        .unwrap();
    assert_eq!(dropped, vec!["p1_dup_uq".to_string()]);
    assert_eq!(valid("p1_dup_uq").await, None);
    assert_eq!(valid("p1_other").await, Some(true));
    // The real migration set has nothing pending on a migrated database.
    let none = madar_rust::migrate_guard::drop_invalid_pending_indexes(&pool, &madar_rust::MIGRATOR)
        .await
        .unwrap();
    assert!(none.is_empty());
}

#[sqlx::test]
async fn deadlocks_and_serialization_failures_are_transient(pool: PgPool) {
    for (code, transient) in [("40P01", true), ("40001", true), ("23505", false)] {
        let e: madar_rust::errors::AppError = sqlx::raw_sql(&format!(
            "DO $$ BEGIN RAISE EXCEPTION 'x' USING ERRCODE = '{code}'; END $$"
        ))
        .execute(&pool)
        .await
        .unwrap_err()
        .into();
        assert_eq!(madar_rust::db::is_transient(&e), transient, "{code}");
    }
    assert!(!madar_rust::db::is_transient(&madar_rust::errors::AppError::Internal));
}

// ── the minimum-client gate ─────────────────────────────────────────────────

const CLIENTS: &[Option<&str>] = &[
    None,
    Some(""),
    Some("pos"),
    Some("pos/garbage"),
    Some("pos/0.1.0"),
    Some("pos/0.6.1 (android)"),
    Some("pos/0.7.0 (ios)"),
    Some("pos/0.14.0 (android)"),
    Some("pos/99.0.0"),
    Some("kds/0.1.0 (linux)"),
    Some("dashboard"),
    Some("Mozilla/5.0"),
];

fn app_of(
    pool: PgPool,
) -> App<
    impl actix_web::dev::ServiceFactory<
        actix_web::dev::ServiceRequest,
        Config = (),
        Response = actix_web::dev::ServiceResponse<impl actix_web::body::MessageBody>,
        Error = actix_web::Error,
        InitError = (),
    >,
> {
    App::new()
        .app_data(web::Data::new(pool))
        .wrap(actix_web::middleware::from_fn(version_gate::gate))
        .route("/sync/pull", web::post().to(|| async { HttpResponse::Ok().body("pulled") }))
        .route("/sync/replay", web::post().to(|| async { HttpResponse::Ok().body("replayed") }))
        .route("/menu-items", web::get().to(|| async { HttpResponse::Ok().body("menu") }))
        .route("/orders", web::post().to(|| async { HttpResponse::Created().body("sold") }))
}

/// Owner, 2026-10-16: never force a till update. With no policy row the gate
/// changes nothing for ANY client — every version, a missing or unreadable
/// one, a browser — on every route, reads and writes alike.
#[sqlx::test]
async fn with_no_policy_row_every_client_is_served_exactly_as_before(pool: PgPool) {
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM client_version_policy")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "no policy row may be seeded");
    version_gate::forget_cache();
    let app = atest::init_service(app_of(pool.clone())).await;
    for client in CLIENTS {
        for (method, path, status, body) in [
            ("POST", "/sync/pull", 200, "pulled"),
            ("POST", "/sync/replay", 200, "replayed"),
            ("GET", "/menu-items", 200, "menu"),
            ("POST", "/orders", 201, "sold"),
        ] {
            let mut req = atest::TestRequest::default()
                .method(method.parse().unwrap())
                .uri(path);
            if let Some(c) = client {
                req = req.insert_header(("X-Madar-Client", *c));
            }
            let res = atest::call_service(&app, req.to_request()).await;
            assert_eq!(res.status().as_u16(), status, "{client:?} {path}");
            assert!(res.headers().get(version_gate::UPGRADE_HEADER).is_none(), "{client:?} {path}");
            let got = atest::read_body(res).await;
            assert_eq!(got, body.as_bytes(), "{client:?} {path}");
        }
    }
    // The pure decision agrees, for any org and either kind of route.
    for client in CLIENTS {
        let c = madar_rust::devices::ClientHeader::parse(*client);
        for gated in [true, false] {
            assert_eq!(version_gate::decide(&[], &c, Some(Uuid::new_v4()), gated), Verdict::Pass);
        }
    }
}

/// Once the owner inserts a row: a below-minimum till still flushes its
/// queued sales (replay and every write pass, nudged), and only the pull and
/// catalog reads answer 426.
#[sqlx::test]
async fn a_policy_row_refuses_only_reads_and_never_a_write(pool: PgPool) {
    sqlx::query(
        "INSERT INTO client_version_policy (app, min_supported, min_recommended, download_url)
         VALUES ('pos', '0.10.0', '0.12.0', 'https://example.test/pos')",
    )
    .execute(&pool)
    .await
    .unwrap();
    version_gate::forget_cache();
    let app = atest::init_service(app_of(pool.clone())).await;
    let call = |client: &'static str, method: &'static str, path: &'static str| {
        atest::TestRequest::default()
            .method(method.parse().unwrap())
            .uri(path)
            .insert_header(("X-Madar-Client", client))
            .to_request()
    };
    // Below min_supported: writes pass with `required`, reads are 426.
    for (method, path) in [("POST", "/sync/replay"), ("POST", "/orders")] {
        let res = atest::call_service(&app, call("pos/0.9.0 (android)", method, path)).await;
        assert!(res.status().is_success(), "{path}");
        assert_eq!(res.headers().get(version_gate::UPGRADE_HEADER).unwrap(), "required");
    }
    for (method, path) in [("POST", "/sync/pull"), ("GET", "/menu-items")] {
        let res = atest::call_service(&app, call("pos/0.9.0 (android)", method, path)).await;
        assert_eq!(res.status().as_u16(), 426, "{path}");
        let body: serde_json::Value = atest::read_body_json(res).await;
        assert_eq!(body["code"], "CLIENT_UPGRADE_REQUIRED");
        assert_eq!(body["min_supported"], "0.10.0");
    }
    // Between the two: served, nudged.
    let res = atest::call_service(&app, call("pos/0.11.0", "POST", "/sync/pull")).await;
    assert_eq!(res.status().as_u16(), 200);
    assert_eq!(res.headers().get(version_gate::UPGRADE_HEADER).unwrap(), "recommended");
    // Current, a KDS (no kds row), or no version: untouched.
    for client in ["pos/0.12.0", "kds/0.1.0", "pos"] {
        let res = atest::call_service(&app, call(client, "POST", "/sync/pull")).await;
        assert_eq!(res.status().as_u16(), 200, "{client}");
        assert!(res.headers().get(version_gate::UPGRADE_HEADER).is_none(), "{client}");
    }
    version_gate::forget_cache();
}

#[test]
fn the_most_specific_policy_row_wins() {
    let org = Uuid::new_v4();
    let global = Policy {
        app: "pos".into(),
        platform: None,
        org_id: None,
        min_supported: Some((0, 10, 0)),
        min_recommended: None,
        download_url: None,
    };
    let pilot = Policy {
        org_id: Some(org),
        min_supported: Some((0, 12, 0)),
        ..global.clone()
    };
    let c = madar_rust::devices::ClientHeader::parse(Some("pos/0.11.0 (android)"));
    let rows = [global, pilot];
    assert_eq!(version_gate::decide(&rows, &c, None, true), Verdict::Pass);
    assert!(matches!(
        version_gate::decide(&rows, &c, Some(org), true),
        Verdict::Refuse { .. }
    ));
    assert_eq!(version_gate::decide(&rows, &c, Some(org), false), Verdict::Nudge("required"));
}
