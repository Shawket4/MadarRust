//! `/sync/pull` (TILLS_CONTRACT §10.6, B2).
use actix_web::{App, http::header, middleware::Compress, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::{ALL_TYPES, PullRequest, checksum::checksum_of, pull_core};
use crate::auth::jwt::{JwtSecret, create_token};
use crate::models::UserRole;

struct Shop {
    org: Uuid,
    branch: Uuid,
    admin: Uuid,
}

async fn shop(pool: &PgPool) -> Shop {
    let org: Uuid = sqlx::query_scalar("INSERT INTO organizations (name, slug) VALUES ('Pull Org', $1) RETURNING id")
        .bind(format!("pull-{}", Uuid::new_v4()))
        .fetch_one(pool)
        .await
        .unwrap();
    let branch: Uuid = sqlx::query_scalar("INSERT INTO branches (org_id, name, code) VALUES ($1, 'Pull', 'PULL') RETURNING id")
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
    PullRequest { branch_id: branch, device_id: None, types: None, limit: None }
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
    assert_eq!(resp.types, ALL_TYPES.iter().map(|t| t.to_string()).collect::<Vec<_>>());
    assert_eq!(resp.next, Some(head(&pool, s.branch).await));
    let cats = &resp.data["category"];
    let row = cats.iter().find(|r| r["id"] == json!(cat)).expect("category in snapshot");
    assert_eq!(row["name"], "Drinks");
    assert!(row["seq"].as_i64().unwrap() > 0);
    assert!(row.get("org_id").is_none(), "lean: no org_id");
    let settings = &resp.data["branch_settings"];
    assert!(settings.iter().any(|r| r["id"] == json!(s.branch)));
    assert!(resp.ledger_window.is_some());
    assert!(resp.asset_bundle.is_some(), "full carries asset_bundle (null when none built)");
    for ty in super::LEDGER_TYPES {
        assert!(!resp.checksums.contains_key(*ty), "ledger types are not checksummed");
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
    let resp = pull_core(&pool, s.org, &req(s.branch), Some(since)).await.unwrap();
    assert!(!resp.full && !resp.has_more);
    assert!(resp.changes.iter().all(|c| c.seq > since));
    let new = resp.changes.iter().find(|c| c.id == cat).expect("new category change");
    assert_eq!(new.op, "upsert");
    assert_eq!(new.data.as_ref().unwrap()["name"], "New");
    assert!(resp.changes.iter().any(|c| c.ty == "category" && c.op == "delete"), "soft delete leaves the live set");
    assert_eq!(resp.next, Some(head(&pool, s.branch).await));
    assert!(resp.checksums.contains_key("category"), "final page carries checksums");
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
    let resp = pull_core(&pool, s.org, &req(s.branch), Some(since)).await.unwrap();
    assert_eq!(resp.next, Some(since), "cannot see past an in-flight emitter");
    assert!(resp.changes.is_empty());
    t1.commit().await.unwrap();
    let resp = pull_core(&pool, s.org, &req(s.branch), Some(since)).await.unwrap();
    let names: Vec<&str> = resp
        .changes
        .iter()
        .filter_map(|c| c.data.as_ref().and_then(|d| d["name"].as_str()))
        .collect();
    assert!(names.contains(&"Inflight") && names.contains(&"Committed"), "{names:?}");
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
    let resp = pull_core(&pool, s.org, &req(s.branch), Some(h - 1)).await.unwrap();
    assert!(resp.resync_required);
    let ahead = pull_core(&pool, s.org, &req(s.branch), Some(h + 1000)).await.unwrap();
    assert!(ahead.resync_required, "a cursor ahead of the head also resyncs");
}

#[sqlx::test]
async fn pull_types_subset_requires_no_since(pool: PgPool) {
    let s = shop(&pool).await;
    let mut r = req(s.branch);
    r.types = Some(vec!["category".into()]);
    let err = pull_core(&pool, s.org, &r, Some(0)).await.unwrap_err();
    assert!(matches!(err, crate::errors::AppError::Coded { code: "TYPES_REQUIRE_FULL", .. }));
    let resp = pull_core(&pool, s.org, &r, None).await.unwrap();
    assert_eq!(resp.types, vec!["category".to_string()]);
    assert_eq!(resp.data.len(), 1);
    r.types = Some(vec!["nope".into()]);
    let err = pull_core(&pool, s.org, &r, None).await.unwrap_err();
    assert!(matches!(err, crate::errors::AppError::Coded { code: "UNKNOWN_SYNC_TYPE", .. }));
}

#[sqlx::test]
async fn pull_checksums_match_reference_formula(pool: PgPool) {
    let s = shop(&pool).await;
    for n in ["X", "Y", "Z"] {
        category(&pool, s.org, n).await;
    }
    let resp = pull_core(&pool, s.org, &req(s.branch), None).await.unwrap();
    for (ty, rows) in &resp.data {
        if super::is_ledger(ty) {
            continue;
        }
        let pairs: Vec<(String, i64)> = rows
            .iter()
            .map(|r| (r["id"].as_str().unwrap().to_string(), r["seq"].as_i64().unwrap()))
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
    assert!(tills.iter().any(|t| t["id"] == json!(old_open)), "open till's history is always in the window");
    assert!(!tills.iter().any(|t| t["id"] == json!(old_closed)), "old closed till is outside the window");
}

#[sqlx::test]
async fn pull_gzip_negotiated(pool: PgPool) {
    let s = shop(&pool).await;
    for i in 0..40 {
        category(&pool, s.org, &format!("Category number {i} with a long enough name")).await;
    }
    let app = test::init_service(
        App::new()
            .wrap(Compress::default())
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(JwtSecret("test_secret".into())))
            .configure(crate::sync::routes::configure),
    )
    .await;
    let token = create_token(&JwtSecret("test_secret".into()), s.admin, Some(s.org), UserRole::OrgAdmin, None, 24).unwrap();
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
            .configure(crate::menu::routes::configure),
    )
    .await;
    let token = create_token(&JwtSecret("test_secret".into()), s.admin, Some(s.org), UserRole::OrgAdmin, None, 24).unwrap();
    crate::permissions::seeder::seed_role_permissions(&pool).await.unwrap();
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
    sqlx::query("SET session_replication_role = replica").execute(&mut *conn).await.unwrap();
    sqlx::query("UPDATE bookings SET starts_at = now() - interval '3 days', ends_at = now() - interval '3 days' WHERE id = $1")
        .bind(booking)
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query("SET session_replication_role = origin").execute(&mut *conn).await.unwrap();
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

    let report = super::sweeper::sweep_once(&pool).await.unwrap();
    assert!(report.deletes_emitted >= 1, "{report:?}");
    assert!(report.tombstones_purged >= 1, "{report:?}");
    assert_eq!(op(pool.clone()).await, "delete");
    let wm: i64 = sqlx::query_scalar("SELECT purged_through_seq FROM sync_feed_watermarks WHERE branch_id = $1")
        .bind(s.branch)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(wm >= ghost_seq);
    let resp = pull_core(&pool, s.org, &req(s.branch), Some(ghost_seq - 1)).await.unwrap();
    assert!(resp.resync_required, "a cursor before the purge must resync");
}

#[::core::prelude::v1::test]
fn sync_changed_debounced_per_branch() {
    use super::listener::{DEBOUNCE, Debouncer};
    use std::time::Instant;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let t0 = Instant::now();
    let mut d = Debouncer::default();
    assert!(d.on_notify(a, t0), "first change publishes immediately");
    assert!(d.on_notify(b, t0), "other branches are independent");
    assert!(!d.on_notify(a, t0 + DEBOUNCE / 4));
    assert!(!d.on_notify(a, t0 + DEBOUNCE / 2), "a burst folds into one trailing event");
    assert!(d.due(t0 + DEBOUNCE / 2).is_empty());
    assert_eq!(d.due(t0 + DEBOUNCE), vec![a]);
    assert!(d.due(t0 + DEBOUNCE * 2).is_empty(), "exactly one trailing publish");
}

#[sqlx::test]
async fn sync_changed_realtime_published_debounced(pool: PgPool) {
    let s = shop(&pool).await;
    let hub = crate::realtime::hub::BranchEventHub::new();
    let mut rx = hub.subscribe(s.branch);
    super::listener::spawn(pool.clone(), hub.clone());
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
    assert!((1..=2).contains(&events), "five changes in a burst → one immediate + at most one trailing event, got {events}");
}
