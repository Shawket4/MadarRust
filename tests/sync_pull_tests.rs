//! `/sync/pull` (TILLS_CONTRACT §10.6, B2).
use actix_web::{App, http::header, middleware::Compress, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::sync::pull::{ALL_TYPES, PullRequest, checksum::checksum_of, pull_core};
use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::models::UserRole;

struct Shop {
    org: Uuid,
    branch: Uuid,
    admin: Uuid,
}

async fn shop(pool: &PgPool) -> Shop {
    let org: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug) VALUES ('Pull Org', $1) RETURNING id",
    )
    .bind(format!("pull-{}", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .unwrap();
    let branch: Uuid = sqlx::query_scalar(
        "INSERT INTO branches (org_id, name, code) VALUES ($1, 'Pull', 'PULL') RETURNING id",
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .unwrap();
    let admin: Uuid = sqlx::query_scalar(
        "INSERT INTO users (org_id, name, email, password_hash, role) VALUES ($1, 'Admin', $2, 'x', 'org_admin') RETURNING id",
    )
    .bind(org)
    .bind(format!("{}@pull.test", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .unwrap();
    Shop { org, branch, admin }
}

async fn category(pool: &PgPool, org: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar("INSERT INTO categories (org_id, name) VALUES ($1, $2) RETURNING id")
        .bind(org)
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn req(branch: Uuid) -> PullRequest {
    PullRequest {
        branch_id: branch,
        device_id: None,
        types: None,
        limit: None,
        ledger_page_size: None,
        snapshot_cursor: None,
    }
}

async fn head(pool: &PgPool, branch: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COALESCE(max(seq), 0) FROM sync_changes WHERE branch_id = $1")
        .bind(branch)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn pull_full_snapshot_includes_all_types_and_next(pool: PgPool) {
    let s = shop(&pool).await;
    let cat = category(&pool, s.org, "Drinks").await;
    let resp = pull_core(&pool, s.org, &req(s.branch), None).await.unwrap();
    assert!(resp.full);
    assert_eq!(
        resp.types,
        ALL_TYPES.iter().map(|t| t.to_string()).collect::<Vec<_>>()
    );
    assert_eq!(resp.next, Some(head(&pool, s.branch).await));
    let cats = &resp.data["category"];
    let row = cats
        .iter()
        .find(|r| r["id"] == json!(cat))
        .expect("category in snapshot");
    assert_eq!(row["name"], "Drinks");
    assert!(row["seq"].as_i64().unwrap() > 0);
    assert!(row.get("org_id").is_none(), "lean: no org_id");
    let settings = &resp.data["branch_settings"];
    assert!(settings.iter().any(|r| r["id"] == json!(s.branch)));
    assert!(resp.ledger_window.is_some());
    assert!(
        resp.asset_bundle.is_some(),
        "full carries asset_bundle (null when none built)"
    );
    for ty in madar_rust::sync::pull::LEDGER_TYPES {
        assert!(
            !resp.checksums.contains_key(*ty),
            "ledger types are not checksummed"
        );
    }
}

#[sqlx::test]
async fn pull_incremental_returns_changes_after_since(pool: PgPool) {
    let s = shop(&pool).await;
    category(&pool, s.org, "Old").await;
    let since = head(&pool, s.branch).await;
    let cat = category(&pool, s.org, "New").await;
    sqlx::query("UPDATE categories SET deleted_at = now() WHERE org_id = $1 AND name = 'Old'")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let resp = pull_core(&pool, s.org, &req(s.branch), Some(since))
        .await
        .unwrap();
    assert!(!resp.full && !resp.has_more);
    assert!(resp.changes.iter().all(|c| c.seq > since));
    let new = resp
        .changes
        .iter()
        .find(|c| c.id == cat)
        .expect("new category change");
    assert_eq!(new.op, "upsert");
    assert_eq!(new.data.as_ref().unwrap()["name"], "New");
    assert!(
        resp.changes
            .iter()
            .any(|c| c.ty == "category" && c.op == "delete"),
        "soft delete leaves the live set"
    );
    assert_eq!(resp.next, Some(head(&pool, s.branch).await));
    assert!(
        resp.checksums.contains_key("category"),
        "final page carries checksums"
    );
}

#[sqlx::test]
async fn pull_horizon_never_skips_inflight_commit(pool: PgPool) {
    let s = shop(&pool).await;
    let since = head(&pool, s.branch).await;
    // T1 takes a seq and stays open.
    let mut t1 = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO categories (org_id, name) VALUES ($1, 'Inflight')")
        .bind(s.org)
        .execute(&mut *t1)
        .await
        .unwrap();
    // T2 commits a higher seq.
    category(&pool, s.org, "Committed").await;
    let resp = pull_core(&pool, s.org, &req(s.branch), Some(since))
        .await
        .unwrap();
    assert_eq!(
        resp.next,
        Some(since),
        "cannot see past an in-flight emitter"
    );
    assert!(resp.changes.is_empty());
    t1.commit().await.unwrap();
    let resp = pull_core(&pool, s.org, &req(s.branch), Some(since))
        .await
        .unwrap();
    let names: Vec<&str> = resp
        .changes
        .iter()
        .filter_map(|c| c.data.as_ref().and_then(|d| d["name"].as_str()))
        .collect();
    assert!(
        names.contains(&"Inflight") && names.contains(&"Committed"),
        "{names:?}"
    );
}

#[sqlx::test]
async fn pull_pages_with_has_more(pool: PgPool) {
    let s = shop(&pool).await;
    let since = head(&pool, s.branch).await;
    for i in 0..5 {
        category(&pool, s.org, &format!("C{i}")).await;
    }
    let mut r = req(s.branch);
    r.limit = Some(2);
    let p1 = pull_core(&pool, s.org, &r, Some(since)).await.unwrap();
    assert!(p1.has_more);
    assert_eq!(p1.changes.len(), 2);
    assert!(p1.checksums.is_empty(), "checksums only on the final page");
    assert_eq!(p1.next, Some(p1.changes[1].seq));
    let mut cursor = p1.next.unwrap();
    let mut seen = 2;
    loop {
        let p = pull_core(&pool, s.org, &r, Some(cursor)).await.unwrap();
        seen += p.changes.len();
        cursor = p.next.unwrap();
        if !p.has_more {
            break;
        }
    }
    assert_eq!(seen, 5);
    assert_eq!(cursor, head(&pool, s.branch).await);
}

#[sqlx::test]
async fn pull_resync_required_after_purge(pool: PgPool) {
    let s = shop(&pool).await;
    category(&pool, s.org, "A").await;
    let h = head(&pool, s.branch).await;
    sqlx::query(
        "INSERT INTO sync_feed_watermarks (branch_id, purged_through_seq) VALUES ($1, $2) \
         ON CONFLICT (branch_id) DO UPDATE SET purged_through_seq = EXCLUDED.purged_through_seq",
    )
    .bind(s.branch)
    .bind(h)
    .execute(&pool)
    .await
    .unwrap();
    let resp = pull_core(&pool, s.org, &req(s.branch), Some(h - 1))
        .await
        .unwrap();
    assert!(resp.resync_required);
    let ahead = pull_core(&pool, s.org, &req(s.branch), Some(h + 1000))
        .await
        .unwrap();
    assert!(
        ahead.resync_required,
        "a cursor ahead of the head also resyncs"
    );
}

#[sqlx::test]
async fn pull_types_subset_requires_no_since(pool: PgPool) {
    let s = shop(&pool).await;
    let mut r = req(s.branch);
    r.types = Some(vec!["category".into()]);
    let err = pull_core(&pool, s.org, &r, Some(0)).await.unwrap_err();
    assert!(matches!(
        err,
        madar_rust::errors::AppError::Coded {
            code: "TYPES_REQUIRE_FULL",
            ..
        }
    ));
    let resp = pull_core(&pool, s.org, &r, None).await.unwrap();
    assert_eq!(resp.types, vec!["category".to_string()]);
    assert_eq!(resp.data.len(), 1);
    r.types = Some(vec!["nope".into()]);
    let err = pull_core(&pool, s.org, &r, None).await.unwrap_err();
    assert!(matches!(
        err,
        madar_rust::errors::AppError::Coded {
            code: "UNKNOWN_SYNC_TYPE",
            ..
        }
    ));
}

#[sqlx::test]
async fn pull_checksums_match_reference_formula(pool: PgPool) {
    let s = shop(&pool).await;
    for n in ["X", "Y", "Z"] {
        category(&pool, s.org, n).await;
    }
    let resp = pull_core(&pool, s.org, &req(s.branch), None).await.unwrap();
    for (ty, rows) in &resp.data {
        if madar_rust::sync::pull::is_ledger(ty) {
            continue;
        }
        let pairs: Vec<(String, i64)> = rows
            .iter()
            .map(|r| {
                (
                    r["id"].as_str().unwrap().to_string(),
                    r["seq"].as_i64().unwrap(),
                )
            })
            .collect();
        let c = &resp.checksums[ty];
        assert_eq!(c.count, pairs.len() as i64, "{ty}");
        assert_eq!(c.checksum, checksum_of(&pairs), "{ty}");
    }
}

#[sqlx::test]
async fn pull_ledger_window_includes_open_till_history(pool: PgPool) {
    let s = shop(&pool).await;
    let old_closed: Uuid = sqlx::query_scalar(
        "INSERT INTO tills (branch_id, teller_id, status, opening_cash, opened_at, closed_at) \
         VALUES ($1, $2, 'closed', 0, now() - interval '5 days', now() - interval '5 days') RETURNING id",
    )
    .bind(s.branch)
    .bind(s.admin)
    .fetch_one(&pool)
    .await
    .unwrap();
    let old_open: Uuid = sqlx::query_scalar(
        "INSERT INTO tills (branch_id, teller_id, status, opening_cash, opened_at) \
         VALUES ($1, $2, 'open', 0, now() - interval '4 days') RETURNING id",
    )
    .bind(s.branch)
    .bind(s.admin)
    .fetch_one(&pool)
    .await
    .unwrap();
    // Both rows changed long ago as far as the feed knows.
    sqlx::query("UPDATE sync_changes SET changed_at = now() - interval '4 days' WHERE branch_id = $1 AND type = 'till'")
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();
    let resp = pull_core(&pool, s.org, &req(s.branch), None).await.unwrap();
    let tills: Vec<&Value> = resp.data["till"].iter().collect();
    assert!(
        tills.iter().any(|t| t["id"] == json!(old_open)),
        "open till's history is always in the window"
    );
    assert!(
        !tills.iter().any(|t| t["id"] == json!(old_closed)),
        "old closed till is outside the window"
    );
}

#[sqlx::test]
async fn pull_gzip_negotiated(pool: PgPool) {
    let s = shop(&pool).await;
    for i in 0..40 {
        category(
            &pool,
            s.org,
            &format!("Category number {i} with a long enough name"),
        )
        .await;
    }
    let app = test::init_service(
        App::new()
            .wrap(Compress::default())
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(JwtSecret("test_secret".into())))
            .configure(madar_rust::sync::routes::configure),
    )
    .await;
    let token = create_token(
        &JwtSecret("test_secret".into()),
        s.admin,
        Some(s.org),
        UserRole::OrgAdmin,
        None,
        24,
    )
    .unwrap();
    for (accept, want) in [("br", "br"), ("gzip", "gzip")] {
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/sync/pull")
                .insert_header(("Authorization", format!("Bearer {token}")))
                .insert_header((header::ACCEPT_ENCODING, accept))
                .set_json(json!({ "branch_id": s.branch }))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get(header::CONTENT_ENCODING).unwrap(), want);
    }
}

#[sqlx::test]
async fn legacy_catalog_sync_unchanged(pool: PgPool) {
    // The pull must not change what /catalog/sync (old clients) returns: same
    // shape, recipes still on options.
    let s = shop(&pool).await;
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(JwtSecret("test_secret".into())))
            .configure(madar_rust::menu::routes::configure),
    )
    .await;
    let token = create_token(
        &JwtSecret("test_secret".into()),
        s.admin,
        Some(s.org),
        UserRole::OrgAdmin,
        None,
        24,
    )
    .unwrap();
    madar_rust::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/catalog/sync?branch_id={}", s.branch))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: Value = test::read_body_json(resp).await;
    for k in ["catalog_revision", "changed", "items", "ingredients"] {
        assert!(body.get(k).is_some(), "{k}");
    }
}

#[sqlx::test]
async fn sweeper_emits_time_based_deletes_and_raises_watermark(pool: PgPool) {
    let s = shop(&pool).await;
    let booking: Uuid = sqlx::query_scalar(
        "INSERT INTO bookings (org_id, branch_id, status, party_size, starts_at, ends_at, guest_name, guest_phone) \
         VALUES ($1, $2, 'confirmed', 2, now() + interval '1 hour', now() + interval '2 hours', 'G', '0100') RETURNING id",
    )
    .bind(s.org)
    .bind(s.branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    let op = |pool: PgPool| async move {
        sqlx::query_scalar::<_, String>("SELECT op FROM sync_changes WHERE branch_id = $1 AND type = 'booking' AND entity_id = $2")
            .bind(s.branch)
            .bind(booking)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    assert_eq!(op(pool.clone()).await, "upsert");
    // Time passes: the slot ended days ago, with no write to fire a trigger.
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("SET session_replication_role = replica")
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query("UPDATE bookings SET starts_at = now() - interval '3 days 1 hour', ends_at = now() - interval '3 days' WHERE id = $1")
        .bind(booking)
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query("SET session_replication_role = origin")
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);

    // An old tombstone to purge.
    let ghost = Uuid::new_v4();
    sqlx::query("SELECT sync_emit($1, 'category', $2, 'delete')")
        .bind(s.branch)
        .bind(ghost)
        .execute(&pool)
        .await
        .unwrap();
    let ghost_seq: i64 = sqlx::query_scalar(
        "UPDATE sync_changes SET changed_at = now() - interval '31 days' WHERE branch_id = $1 AND entity_id = $2 RETURNING seq",
    )
    .bind(s.branch)
    .bind(ghost)
    .fetch_one(&pool)
    .await
    .unwrap();

    let report = madar_rust::sync::pull::sweeper::sweep_once(&pool).await.unwrap();
    assert!(report.deletes_emitted >= 1, "{report:?}");
    assert!(report.tombstones_purged >= 1, "{report:?}");
    assert_eq!(op(pool.clone()).await, "delete");
    let wm: i64 = sqlx::query_scalar(
        "SELECT purged_through_seq FROM sync_feed_watermarks WHERE branch_id = $1",
    )
    .bind(s.branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(wm >= ghost_seq);
    let resp = pull_core(&pool, s.org, &req(s.branch), Some(ghost_seq - 1))
        .await
        .unwrap();
    assert!(
        resp.resync_required,
        "a cursor before the purge must resync"
    );
}

#[::core::prelude::v1::test]
fn sync_changed_debounced_per_branch() {
    use madar_rust::sync::pull::listener::{DEBOUNCE, Debouncer};
    use std::time::Instant;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let t0 = Instant::now();
    let mut d = Debouncer::default();
    assert!(d.on_notify(a, t0), "first change publishes immediately");
    assert!(d.on_notify(b, t0), "other branches are independent");
    assert!(!d.on_notify(a, t0 + DEBOUNCE / 4));
    assert!(
        !d.on_notify(a, t0 + DEBOUNCE / 2),
        "a burst folds into one trailing event"
    );
    assert!(d.due(t0 + DEBOUNCE / 2).is_empty());
    assert_eq!(d.due(t0 + DEBOUNCE), vec![a]);
    assert!(
        d.due(t0 + DEBOUNCE * 2).is_empty(),
        "exactly one trailing publish"
    );
}

#[sqlx::test]
async fn sync_changed_realtime_published_debounced(pool: PgPool) {
    let s = shop(&pool).await;
    let hub = madar_rust::realtime::hub::BranchEventHub::new();
    let mut rx = hub.subscribe(s.branch);
    madar_rust::sync::pull::listener::spawn(pool.clone(), hub.clone());
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    for i in 0..5 {
        category(&pool, s.org, &format!("Burst {i}")).await;
    }
    let mut events = 0;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(2500);
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        assert_eq!(ev.event_type, "sync.changed");
        assert_eq!(ev.data["branch_id"], json!(s.branch));
        events += 1;
    }
    assert!(
        (1..=2).contains(&events),
        "five changes in a burst → one immediate + at most one trailing event, got {events}"
    );
}

/// One live entity of every one of the 22 types at `s.branch`, plus rows whose
/// feed entry still says `upsert` but which no longer project: the time-based
/// live rules (kitchen ticket closed > 12 h ago, delivery finished > 48 h ago,
/// booking ended > 1 day ago) age out with no write to fire a trigger, and the
/// sweeper has not run yet. Returns the ids of those stale rows.
async fn seed_every_type(pool: &PgPool, s: &Shop) -> Vec<Uuid> {
    let sql = format!(
        r#"
DO $$
DECLARE
    org uuid := '{org}';
    br uuid := '{branch}';
    adm uuid := '{admin}';
    cat uuid; item uuid; item2 uuid; bun uuid; icat uuid; pm uuid; dev uuid; sec uuid; tbl uuid; tbl2 uuid;
    ot uuid; ot2 uuid; til uuid; ord uuid; ord2 uuid;
BEGIN
    INSERT INTO categories (org_id, name) VALUES (org, 'Hot') RETURNING id INTO cat;
    INSERT INTO menu_items (org_id, name, category_id) VALUES (org, 'Latte', cat) RETURNING id INTO item;
    INSERT INTO menu_items (org_id, name, category_id) VALUES (org, 'Mocha', cat) RETURNING id INTO item2;
    INSERT INTO menu_item_sizes (menu_item_id, label, price) VALUES (item, 'M', 1000), (item2, 'M', 1200);
    INSERT INTO bundles (org_id, name, price, status) VALUES (org, 'Duo', 2000, 'active') RETURNING id INTO bun;
    INSERT INTO bundle_components (bundle_id, item_id) VALUES (bun, item), (bun, item2);
    INSERT INTO ingredient_categories (org_id, slug, name) VALUES (org, 'dairy', 'Dairy') RETURNING id INTO icat;
    INSERT INTO org_ingredients (org_id, name, unit, category_id) VALUES (org, 'Milk', 'ml', icat);
    INSERT INTO org_payment_methods (org_id, name, color, icon, is_cash) VALUES (org, 'Cash', '#000', 'cash', true) RETURNING id INTO pm;
    INSERT INTO branch_payment_methods (branch_id, payment_method_id, org_id) VALUES (br, pm, org);
    INSERT INTO user_payment_methods (user_id, payment_method_id, org_id) VALUES (adm, pm, org);
    INSERT INTO discounts (org_id, name, type, value) VALUES (org, 'Staff', 'percentage', 0.1);
    INSERT INTO addon_items (org_id, name, type, default_price) VALUES (org, 'Oat milk', 'milk', 1500);
    INSERT INTO customers (org_id, name, phone, phone_key) VALUES (org, 'Mona', '0100 123 4567', '01001234567');
    INSERT INTO staff_drinks (id, org_id, branch_id, menu_item_id, item_name, note, business_date, recorded_at)
      VALUES (gen_random_uuid(), org, br, NULL, 'Latte', 'for Sara, closing shift', current_date, now());
    INSERT INTO devices (id, org_id, branch_id, code) VALUES (gen_random_uuid(), org, br, 'A') RETURNING id INTO dev;
    INSERT INTO device_payment_methods (device_id, payment_method_id, org_id) VALUES (dev, pm, org);
    INSERT INTO floor_sections (org_id, branch_id, name) VALUES (org, br, 'Main') RETURNING id INTO sec;
    INSERT INTO branch_tables (org_id, branch_id, label, section_id) VALUES (org, br, 'T1', sec) RETURNING id INTO tbl;
    INSERT INTO branch_tables (org_id, branch_id, label, section_id) VALUES (org, br, 'T2', sec) RETURNING id INTO tbl2;
    INSERT INTO open_tickets (org_id, branch_id, opened_by, table_id) VALUES (org, br, adm, tbl) RETURNING id INTO ot;
    INSERT INTO open_tickets (org_id, branch_id, opened_by) VALUES (org, br, adm) RETURNING id INTO ot2;
    INSERT INTO table_occupancies (org_id, branch_id, table_id, held_by, open_ticket_id, started_by)
         VALUES (org, br, tbl, 'ticket', ot, adm);
    INSERT INTO table_transfer_requests (id, org_id, branch_id, occupant_kind, occupant_id, target_section_id)
         VALUES (gen_random_uuid(), org, br, 'open_ticket', ot2, sec);
    INSERT INTO bookings (org_id, branch_id, status, party_size, starts_at, ends_at, guest_name, guest_phone)
         VALUES (org, br, 'confirmed', 2, now() + interval '1 hour', now() + interval '2 hours', 'G', '0100');
    INSERT INTO tills (branch_id, teller_id, status, opening_cash) VALUES (br, adm, 'open', 0) RETURNING id INTO til;
    INSERT INTO till_cash_movements (till_id, amount, note, moved_by, kind) VALUES (til, 500, 'float', adm, 'pay_in');
    INSERT INTO orders (branch_id, till_id, teller_id, order_number, payment_method, order_ref, subtotal, total_amount)
         VALUES (br, til, adm, 1, 'Cash', 'PULL-1', 1000, 1000) RETURNING id INTO ord;
    INSERT INTO orders (branch_id, till_id, teller_id, order_number, payment_method, order_ref, subtotal, total_amount)
         VALUES (br, til, adm, 2, 'Cash', 'PULL-2', 1000, 1000) RETURNING id INTO ord2;
    INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES (ord, 'Cash', 1000, true);
    INSERT INTO order_refunds (org_id, branch_id, order_id, till_id, amount, method, is_cash, reason, issued_by)
         VALUES (org, br, ord, til, 100, 'Cash', true, 'goodwill', adm);
    INSERT INTO kitchen_tickets (org_id, branch_id, order_id) VALUES (org, br, ord);
    INSERT INTO kitchen_ticket_items (kitchen_ticket_id, line)
         SELECT id, '{{"name":"Latte"}}'::jsonb FROM kitchen_tickets WHERE order_id = ord;
    INSERT INTO delivery_orders (org_id, branch_id, channel, customer_name, customer_phone, cart, tax_amount, tax_rate_applied, tax_inclusive)
         VALUES (org, br, 'pickup', 'C', '0101', '[]', 0, 0, false);
    -- Rows that will age out of their live sets.
    INSERT INTO kitchen_tickets (org_id, branch_id, order_id) VALUES (org, br, ord2);
    INSERT INTO delivery_orders (org_id, branch_id, channel, customer_name, customer_phone, cart, tax_amount, tax_rate_applied, tax_inclusive, status)
         VALUES (org, br, 'pickup', 'Old', '0102', '[]', 0, 0, false, 'cancelled');
    INSERT INTO bookings (org_id, branch_id, status, party_size, starts_at, ends_at, guest_name, guest_phone)
         VALUES (org, br, 'confirmed', 2, now() + interval '3 hours', now() + interval '4 hours', 'Gone', '0103');
END $$;
"#,
        org = s.org,
        branch = s.branch,
        admin = s.admin
    );
    sqlx::raw_sql(&sql).execute(pool).await.unwrap();

    // Time passes with no write (triggers off): the feed still says `upsert`.
    let mut conn = pool.acquire().await.unwrap();
    sqlx::raw_sql(&format!(
        "SET session_replication_role = replica;
         UPDATE kitchen_tickets SET status = 'ready', closed_at = now() - interval '13 hours', close_reason = 'bumped'
          WHERE branch_id = '{b}' AND order_id = (SELECT id FROM orders WHERE order_ref = 'PULL-2');
         UPDATE delivery_orders SET updated_at = now() - interval '49 hours' WHERE branch_id = '{b}' AND customer_name = 'Old';
         UPDATE bookings SET starts_at = now() - interval '3 days 1 hour', ends_at = now() - interval '3 days'
          WHERE branch_id = '{b}' AND guest_name = 'Gone';
         SET session_replication_role = origin;",
        b = s.branch
    ))
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    let stale: Vec<Uuid> = sqlx::query_scalar(
        "SELECT kt.id FROM kitchen_tickets kt JOIN orders o ON o.id = kt.order_id WHERE o.order_ref = 'PULL-2' \
         UNION ALL SELECT id FROM delivery_orders WHERE customer_name = 'Old' AND branch_id = $1 \
         UNION ALL SELECT id FROM bookings WHERE guest_name = 'Gone' AND branch_id = $1",
    )
    .bind(s.branch)
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(stale.len(), 3);
    for id in &stale {
        let op: String = sqlx::query_scalar(
            "SELECT op FROM sync_changes WHERE branch_id = $1 AND entity_id = $2",
        )
        .bind(s.branch)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(op, "upsert", "precondition: the feed row is stale");
    }
    stale
}

/// A device's local store: type → id → seq, exactly what the POS core keeps.
type Store = std::collections::BTreeMap<String, std::collections::BTreeMap<String, i64>>;

fn store_from_full(resp: &madar_rust::sync::pull::PullResponse) -> Store {
    let mut store = Store::new();
    for ty in ALL_TYPES {
        let rows = store.entry(ty.to_string()).or_default();
        for r in resp.data.get(*ty).into_iter().flatten() {
            rows.insert(
                r["id"].as_str().unwrap().to_string(),
                r["seq"].as_i64().unwrap(),
            );
        }
    }
    store
}

fn apply_changes(store: &mut Store, resp: &madar_rust::sync::pull::PullResponse) {
    for c in &resp.changes {
        let rows = store.entry(c.ty.clone()).or_default();
        if c.op == "upsert" {
            rows.insert(c.id.to_string(), c.seq);
        } else {
            rows.remove(&c.id.to_string());
        }
    }
}

/// Every type the response checksums must match the store; ledger types carry
/// none. Returns the mismatching types (empty = the POS would not self-heal).
fn mismatches(store: &Store, resp: &madar_rust::sync::pull::PullResponse) -> Vec<String> {
    let mut bad = Vec::new();
    for ty in ALL_TYPES {
        if madar_rust::sync::pull::is_ledger(ty) {
            if resp.checksums.contains_key(*ty) {
                bad.push(format!("{ty}: ledger type checksummed"));
            }
            continue;
        }
        let Some(c) = resp.checksums.get(*ty) else {
            bad.push(format!("{ty}: no checksum"));
            continue;
        };
        let pairs: Vec<(String, i64)> = store[*ty]
            .iter()
            .map(|(id, seq)| (id.clone(), *seq))
            .collect();
        if c.count != pairs.len() as i64 || c.checksum != checksum_of(&pairs) {
            bad.push(format!(
                "{ty}: server count {} vs store {}",
                c.count,
                pairs.len()
            ));
        }
    }
    bad
}

#[sqlx::test]
async fn pull_checksums_equal_projected_sets_for_every_type(pool: PgPool) {
    let s = shop(&pool).await;
    let stale = seed_every_type(&pool, &s).await;

    let full = pull_core(&pool, s.org, &req(s.branch), None).await.unwrap();
    for ty in ALL_TYPES {
        assert!(
            !full.data.get(*ty).is_none_or(|v| v.is_empty()),
            "fixture seeds a live `{ty}`"
        );
    }
    let mut store = store_from_full(&full);
    for id in &stale {
        assert!(
            store
                .values()
                .all(|rows| !rows.contains_key(&id.to_string())),
            "stale {id} does not project"
        );
    }
    assert_eq!(
        mismatches(&store, &full),
        Vec::<String>::new(),
        "full snapshot checksums = its own data"
    );

    // A clean incremental pull right after the full one: nothing changed, and
    // no type reports a mismatch (so no type is re-fetched).
    let inc = pull_core(&pool, s.org, &req(s.branch), full.next)
        .await
        .unwrap();
    assert!(!inc.has_more && inc.changes.is_empty(), "{:?}", inc.changes);
    apply_changes(&mut store, &inc);
    assert_eq!(mismatches(&store, &inc), Vec::<String>::new());

    // Changes after that, including one that leaves a live set, still match.
    category(&pool, s.org, "Cold").await;
    sqlx::query("UPDATE discounts SET is_active = false WHERE org_id = $1")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let inc2 = pull_core(&pool, s.org, &req(s.branch), inc.next)
        .await
        .unwrap();
    assert!(
        inc2.changes
            .iter()
            .any(|c| c.ty == "discount" && c.op == "delete")
    );
    apply_changes(&mut store, &inc2);
    assert_eq!(mismatches(&store, &inc2), Vec::<String>::new());

    // An incremental that SEES a stale row (its feed row moved) sends a delete.
    sqlx::query("SELECT sync_emit($1, 'booking', $2, 'upsert')")
        .bind(s.branch)
        .bind(stale[2])
        .execute(&pool)
        .await
        .unwrap();
    let inc3 = pull_core(&pool, s.org, &req(s.branch), inc2.next)
        .await
        .unwrap();
    let ch = inc3
        .changes
        .iter()
        .find(|c| c.id == stale[2])
        .expect("the re-emitted booking");
    assert_eq!(
        ch.op, "delete",
        "a booking that no longer projects is a delete"
    );
    apply_changes(&mut store, &inc3);
    assert_eq!(mismatches(&store, &inc3), Vec::<String>::new());
}

/// The CRITICAL deadlock: a pull used to hold a transaction and then take a
/// second pooled connection for the projection, so pool_size concurrent pulls
/// each held one and waited forever for another. Now a pull holds at most one
/// connection at a time: many more pulls than connections all complete.
#[sqlx::test]
async fn pull_concurrent_pulls_on_small_pool_all_complete(pool: PgPool) {
    let s = shop(&pool).await;
    seed_every_type(&pool, &s).await;
    let since = pull_core(&pool, s.org, &req(s.branch), None)
        .await
        .unwrap()
        .next;
    category(&pool, s.org, "After").await;

    let small = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for i in 0..16 {
        let small = small.clone();
        let (org, branch) = (s.org, s.branch);
        tasks.push(tokio::spawn(async move {
            let since = if i % 2 == 0 { None } else { since };
            pull_core(&small, org, &req(branch), since).await
        }));
    }
    let all = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        futures::future::join_all(tasks),
    )
    .await
    .expect("16 pulls on a pool of 2 finish (no deadlock)");
    for r in all {
        let resp = r.unwrap().expect("pull ok");
        assert!(resp.full || resp.changes.iter().any(|c| c.ty == "category"));
    }
}

// ════════════════════════════════════════════════════════════════════════════
// A branch born after the org's data
// ════════════════════════════════════════════════════════════════════════════

/// The owner's bug: payment methods do not show on a NEW device at a NEW till,
/// with the org's methods present and no override restricting them.
///
/// `sync_emit_org` fans an org write out to the branches that exist AT THAT
/// MOMENT, and a device's full snapshot is built from feed rows rather than
/// from the live tables — so a branch opened after the methods were created
/// has no feed row for any of them and its first device sees none. Nothing
/// above notices: the checksum is computed from the same feed, so device and
/// server agree perfectly on nothing.
#[sqlx::test]
async fn a_branch_created_after_the_orgs_data_still_gets_all_of_it(pool: PgPool) {
    let s = shop(&pool).await;

    // The org is set up first — methods, menu, a discount, staff.
    let method: Uuid = sqlx::query_scalar(
        "INSERT INTO org_payment_methods (org_id, name, color, icon) \
         VALUES ($1, 'Instapay', '#000', 'card') RETURNING id",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    let cat = category(&pool, s.org, "Hot drinks").await;

    // THEN a second branch opens.
    let later: Uuid = sqlx::query_scalar(
        "INSERT INTO branches (org_id, name, code) VALUES ($1, 'Later', 'LATE') RETURNING id",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();

    // A brand-new device there takes its first snapshot.
    let resp = madar_rust::sync::pull::pull_core(&pool, s.org, &req(later), None)
        .await
        .unwrap();
    let methods = resp.data.get("payment_method").cloned().unwrap_or_default();
    assert!(
        methods.iter().any(|v| v["id"] == method.to_string()),
        "a till opened after the org's payment methods must still see them: {methods:?}"
    );
    let cats = resp.data.get("category").cloned().unwrap_or_default();
    assert!(
        cats.iter().any(|v| v["id"] == cat.to_string()),
        "…and the menu it is supposed to sell: {cats:?}"
    );
    assert!(
        resp.data
            .get("teller")
            .map(|t| t.iter().any(|v| v["id"] == s.admin.to_string()))
            .unwrap_or(false),
        "…and the people who sign in on it — a new branch with no staff is the \
         same bug wearing a different hat"
    );

    // The older branch is untouched and still complete.
    let first = madar_rust::sync::pull::pull_core(&pool, s.org, &req(s.branch), None)
        .await
        .unwrap();
    assert!(
        first
            .data
            .get("payment_method")
            .map(|m| m.iter().any(|v| v["id"] == method.to_string()))
            .unwrap_or(false),
        "the branch that was there all along keeps its rows"
    );
}

/// The feed must equal the live set for EVERY branch, not only ones created
/// before their org's data. This is the invariant the branch backfill restores
/// and the sweep keeps: without it the hole above exists for ten of the feed's
/// types, not just payment methods.
#[sqlx::test]
async fn the_feed_matches_the_live_set_even_for_a_late_branch(pool: PgPool) {
    let s = shop(&pool).await;
    seed_every_type(&pool, &s).await;

    let later: Uuid = sqlx::query_scalar(
        "INSERT INTO branches (org_id, name, code) VALUES ($1, 'Later', 'LATE') RETURNING id",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();

    let missing: Vec<(String, i64)> = sqlx::query_as(
        "SELECT l.type, count(*) FROM sync_live_rows() l \
          WHERE l.branch_id = $1 \
            AND NOT EXISTS (SELECT 1 FROM sync_changes c \
                             WHERE c.branch_id = l.branch_id AND c.type = l.type \
                               AND c.entity_id = l.entity_id) \
          GROUP BY l.type ORDER BY l.type",
    )
    .bind(later)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        missing.is_empty(),
        "the late branch is short of feed rows it should have: {missing:?}"
    );
}

/// Belt and braces: even if a feed row goes missing some other way — a path
/// that forgets to emit, a restore, a hand-written INSERT — the sweep closes
/// the hole without anyone touching the device.
#[sqlx::test]
async fn the_sweep_re_emits_live_rows_the_feed_never_heard_of(pool: PgPool) {
    let s = shop(&pool).await;
    let method: Uuid = sqlx::query_scalar(
        "INSERT INTO org_payment_methods (org_id, name, color, icon) \
         VALUES ($1, 'Instapay', '#000', 'card') RETURNING id",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();

    // Simulate the hole exactly as the org fan-out used to leave it.
    sqlx::query("DELETE FROM sync_changes WHERE branch_id = $1 AND type = 'payment_method'")
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();
    let before = madar_rust::sync::pull::pull_core(&pool, s.org, &req(s.branch), None)
        .await
        .unwrap();
    assert!(
        before
            .data
            .get("payment_method")
            .map(|m| m.is_empty())
            .unwrap_or(true),
        "the hole is real: the snapshot shows no methods"
    );

    let report = madar_rust::sync::pull::sweeper::sweep_once(&pool).await.unwrap();
    assert!(
        report.upserts_emitted >= 1,
        "the sweep noticed the missing row: {report:?}"
    );

    let after = madar_rust::sync::pull::pull_core(&pool, s.org, &req(s.branch), None)
        .await
        .unwrap();
    assert!(
        after
            .data
            .get("payment_method")
            .map(|m| m.iter().any(|v| v["id"] == method.to_string()))
            .unwrap_or(false),
        "…and the device's next pull brings the method back"
    );
}

/// A device that ALREADY took the empty snapshot heals by itself: the backfill
/// emits with fresh seqs, so the rows arrive on its next ORDINARY incremental
/// pull. No full re-sync, no checksum repair, nobody reinstalling a till.
#[sqlx::test]
async fn a_device_that_already_synced_the_empty_snapshot_heals_on_its_next_pull(pool: PgPool) {
    let s = shop(&pool).await;
    let method: Uuid = sqlx::query_scalar(
        "INSERT INTO org_payment_methods (org_id, name, color, icon) \
         VALUES ($1, 'Instapay', '#000', 'card') RETURNING id",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();

    // Put the branch in the broken state the old fan-out left behind.
    sqlx::query("DELETE FROM sync_changes WHERE branch_id = $1 AND type = 'payment_method'")
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();

    // The device bootstraps and gets nothing, then parks on its cursor.
    let first = madar_rust::sync::pull::pull_core(&pool, s.org, &req(s.branch), None)
        .await
        .unwrap();
    assert!(
        first
            .data
            .get("payment_method")
            .map(|m| m.is_empty())
            .unwrap_or(true),
        "the device starts out short, as the field does"
    );
    let cursor = first.next.unwrap_or_else(|| 0);

    // The fix lands (the migration runs this for every branch).
    let healed: i32 = sqlx::query_scalar("SELECT sync_backfill_branch($1)")
        .bind(s.branch)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(healed >= 1, "the backfill emitted the missing rows");

    // The device does nothing special — its ordinary incremental pull.
    let next = madar_rust::sync::pull::pull_core(&pool, s.org, &req(s.branch), Some(cursor))
        .await
        .unwrap();
    assert!(
        !next.resync_required,
        "no full re-sync is demanded of it: {next:?}"
    );
    // An incremental pull answers with `changes`, the ordinary per-row feed.
    assert!(
        next.changes
            .iter()
            .any(|c| c.ty == "payment_method" && c.id == method && c.op == "upsert"),
        "and the method it was missing simply arrives, as an ordinary change: {:?}",
        next.changes
    );
}

/// The affected set is not "payment methods". A late branch was missing its
/// STAFF and its ADD-ONS too — a fresh device on it could show no one to sign
/// in as and no add-ons to sell. Pinned so a future emit path that reintroduces
/// the hole for any of these fails here.
#[sqlx::test]
async fn a_late_branch_has_its_staff_and_addons_too(pool: PgPool) {
    let s = shop(&pool).await;
    seed_every_type(&pool, &s).await;
    let later: Uuid = sqlx::query_scalar(
        "INSERT INTO branches (org_id, name, code) VALUES ($1, 'Later', 'LATE') RETURNING id",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();

    let resp = madar_rust::sync::pull::pull_core(&pool, s.org, &req(later), None)
        .await
        .unwrap();
    for ty in [
        "teller",
        "addon_item",
        "payment_method",
        "menu_item",
        "category",
        "discount",
        "ingredient",
        "bundle",
        "customer",
    ] {
        assert!(
            resp.data.get(ty).map(|v| !v.is_empty()).unwrap_or(false),
            "a branch opened after its org's data must still see `{ty}`"
        );
    }
}

/// It is not ONLY a new-branch problem. `sync_emit_org` also skips a
/// SOFT-DELETED branch, so a branch that is deleted and later restored is
/// short of everything its org wrote while it was gone — the same ten types,
/// on a branch that has been live for months. This is why the reconcile lives
/// in the 10-minute sweep and not only in the creation trigger.
#[sqlx::test]
async fn a_restored_branch_is_short_until_the_sweep_catches_it(pool: PgPool) {
    let s = shop(&pool).await;

    sqlx::query("UPDATE branches SET deleted_at = now() WHERE id = $1")
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();
    // The org carries on working while the branch is away.
    let method: Uuid = sqlx::query_scalar(
        "INSERT INTO org_payment_methods (org_id, name, color, icon) \
         VALUES ($1, 'Instapay', '#000', 'card') RETURNING id",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE branches SET deleted_at = NULL WHERE id = $1")
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();

    // Restored, live, and silently short: no INSERT fired, so the creation
    // trigger never ran.
    let short: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sync_live_rows() l \
          WHERE l.branch_id = $1 \
            AND NOT EXISTS (SELECT 1 FROM sync_changes c \
                             WHERE c.branch_id=l.branch_id AND c.type=l.type \
                               AND c.entity_id=l.entity_id)",
    )
    .bind(s.branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(short >= 1, "the restored branch really is short");

    // The sweep closes it, with nobody touching the device.
    let report = madar_rust::sync::pull::sweeper::sweep_once(&pool).await.unwrap();
    assert!(report.upserts_emitted >= short, "{report:?}");
    let resp = madar_rust::sync::pull::pull_core(&pool, s.org, &req(s.branch), None)
        .await
        .unwrap();
    assert!(
        resp.data
            .get("payment_method")
            .map(|m| m.iter().any(|v| v["id"] == method.to_string()))
            .unwrap_or(false),
        "and the branch has what its org wrote while it was away"
    );
}
