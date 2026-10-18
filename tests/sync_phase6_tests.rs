//! DB audit Phase 6 (sync scale): the protocol version, batched replay, ledger
//! retention on the changefeed and the incremental sweep. Every old contract
//! is pinned next to its new sibling: a till that sends no `protocol` gets the
//! response it always got, and `/sync/replay` is untouched.

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::models::UserRole;
use madar_rust::realtime::hub::BranchEventHub;
use madar_rust::sync::pull::sweeper::{retention, sweep_once, sweep_once_at};
use madar_rust::sync::pull::{PullRequest, pull_core};

fn secret() -> JwtSecret {
    JwtSecret("secret".into())
}

struct Shop {
    org: Uuid,
    branch: Uuid,
    teller: Uuid,
}

async fn shop(pool: &PgPool) -> Shop {
    let org: Uuid =
        sqlx::query_scalar("INSERT INTO organizations (name, slug) VALUES ('P6', $1) RETURNING id")
            .bind(format!("p6-{}", Uuid::new_v4()))
            .fetch_one(pool)
            .await
            .unwrap();
    let branch: Uuid = sqlx::query_scalar(
        "INSERT INTO branches (org_id, name) VALUES ($1, 'P6') RETURNING id",
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .unwrap();
    let teller: Uuid = sqlx::query_scalar(
        "INSERT INTO users (org_id, name, email, password_hash, role, pin_hash)
         VALUES ($1, 'Teller', $2, 'h', 'teller', 'h') RETURNING id",
    )
    .bind(org)
    .bind(format!("{}@p6.test", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .unwrap();
    Shop {
        org,
        branch,
        teller,
    }
}

async fn open_till(pool: &PgPool, s: &Shop) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO tills (branch_id, teller_id, status, opening_cash) VALUES ($1, $2, 'open', 0) RETURNING id",
    )
    .bind(s.branch)
    .bind(s.teller)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn pull_req(branch: Uuid, protocol: Option<u16>) -> PullRequest {
    PullRequest {
        branch_id: branch,
        device_id: None,
        types: None,
        limit: None,
        ledger_page_size: None,
        snapshot_cursor: None,
        protocol,
    }
}

async fn head(pool: &PgPool, branch: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COALESCE(max(seq), 0) FROM sync_changes WHERE branch_id = $1")
        .bind(branch)
        .fetch_one(pool)
        .await
        .unwrap()
}

// ── protocol ────────────────────────────────────────────────────────────────

/// A till that names no protocol is answered exactly as before: none of the
/// v2 fields appear, on a full pull or an incremental one.
#[sqlx::test]
async fn a_pull_without_a_protocol_has_the_old_shape(pool: PgPool) {
    let s = shop(&pool).await;
    let full = pull_core(&pool, s.org, &pull_req(s.branch, None), None)
        .await
        .unwrap();
    let v = serde_json::to_value(&full).unwrap();
    assert!(v.get("protocol").is_none(), "{v}");
    assert!(v.get("ledger_retention_hours").is_none());
    let inc = pull_core(&pool, s.org, &pull_req(s.branch, None), full.next)
        .await
        .unwrap();
    let v = serde_json::to_value(&inc).unwrap();
    assert!(v.get("protocol").is_none());
    assert!(v.get("ledger_retention_hours").is_none());
}

#[sqlx::test]
async fn a_pull_with_a_protocol_is_answered_with_the_negotiated_one(pool: PgPool) {
    let s = shop(&pool).await;
    let r = pull_core(&pool, s.org, &pull_req(s.branch, Some(2)), None)
        .await
        .unwrap();
    assert_eq!(r.protocol, Some(2));
    assert_eq!(r.ledger_retention_hours, Some(retention().num_hours()));
    let r = pull_core(&pool, s.org, &pull_req(s.branch, Some(9)), None)
        .await
        .unwrap();
    assert_eq!(r.protocol, Some(madar_rust::sync::protocol::CURRENT));
    let r = pull_core(&pool, s.org, &pull_req(s.branch, Some(1)), None)
        .await
        .unwrap();
    assert_eq!(r.protocol, Some(1));
    assert_eq!(r.ledger_retention_hours, None, "a v2 field, not sent to v1");
}

// ── batched replay ──────────────────────────────────────────────────────────

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(BranchEventHub::new()))
                .configure(madar_rust::sync::routes::configure),
        )
        .await
    };
}

fn movement(s: &Shop, till: Uuid, client_ref: Uuid, amount: i32) -> Value {
    json!({
        "op": "cash_movement",
        "teller_id": s.teller,
        "till_id": till,
        "request": { "amount": amount, "note": "float", "client_ref": client_ref }
    })
}

async fn movements(pool: &PgPool, till: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM till_cash_movements WHERE till_id = $1")
        .bind(till)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn a_batch_replays_each_op_as_its_single_call_would(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let till = open_till(&pool, &s).await;
    let token = create_token(&secret(), s.teller, Some(s.org), UserRole::Teller, None, 24).unwrap();
    let post = |uri: &'static str, body: Value| {
        test::TestRequest::post()
            .uri(uri)
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(body)
            .to_request()
    };

    // The single-op route, unchanged, answers what a batch entry carries.
    let single = test::call_service(&app, post("/sync/replay", movement(&s, till, Uuid::new_v4(), 100))).await;
    let single_status = single.status().as_u16();
    assert!(single.status().is_success());
    let single_body: Value = test::read_body_json(single).await;

    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let batch = json!({ "protocol": 2, "ops": [movement(&s, till, a, 200), movement(&s, till, b, 300)] });
    let r = test::call_service(&app, post("/sync/replay/batch", batch.clone())).await;
    assert_eq!(r.status(), 200);
    let body: Value = test::read_body_json(r).await;
    assert_eq!(body["protocol"], 2);
    assert_eq!(body["applied"], 2);
    assert!(body.get("stopped_at").is_none());
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    for (i, r) in results.iter().enumerate() {
        assert_eq!(r["index"], i);
        assert_eq!(r["outcome"], "applied");
        assert_eq!(r["status"], single_status);
        let keys: std::collections::BTreeSet<&String> =
            r["body"].as_object().unwrap().keys().collect();
        let single_keys: std::collections::BTreeSet<&String> =
            single_body.as_object().unwrap().keys().collect();
        assert_eq!(keys, single_keys, "the same ack shape as the single call");
        assert!(r["sync_seq"].as_i64().is_some(), "each op is stamped like its single call");
    }
    assert_eq!(body["sync_seq"], results[1]["sync_seq"]);
    assert_eq!(movements(&pool, till).await, 3);

    // A till that lost the response and resends the whole batch: deduplicated
    // per op, exactly like a single retry.
    let r = test::call_service(&app, post("/sync/replay/batch", batch)).await;
    assert_eq!(r.status(), 200);
    assert_eq!(movements(&pool, till).await, 3);
}

#[sqlx::test]
async fn a_batch_stops_at_the_first_failure_unless_told_not_to(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let till = open_till(&pool, &s).await;
    let token = create_token(&secret(), s.teller, Some(s.org), UserRole::Teller, None, 24).unwrap();
    let post = |body: Value| {
        test::TestRequest::post()
            .uri("/sync/replay/batch")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(body)
            .to_request()
    };
    let bad = movement(&s, Uuid::new_v4(), Uuid::new_v4(), 100); // no such till
    let good = movement(&s, till, Uuid::new_v4(), 100);

    let r = test::call_service(&app, post(json!({ "ops": [bad.clone(), good.clone()] }))).await;
    assert_eq!(r.status(), 200);
    let body: Value = test::read_body_json(r).await;
    assert_eq!(body["protocol"], 2, "this route is v2 when the till names none");
    assert_eq!(body["applied"], 0);
    assert_eq!(body["stopped_at"], 0);
    assert_eq!(body["results"][0]["outcome"], "failed");
    assert!(body["results"][0]["status"].as_u64().unwrap() >= 400);
    assert!(body["results"][0]["body"]["error"].is_string(), "the single call's error body");
    assert_eq!(body["results"][1]["outcome"], "skipped");
    assert!(body["results"][1].get("status").is_none());
    assert_eq!(movements(&pool, till).await, 0, "a skipped op is not applied");

    let r = test::call_service(
        &app,
        post(json!({ "ops": [bad, good], "stop_on_error": false })),
    )
    .await;
    let body: Value = test::read_body_json(r).await;
    assert_eq!(body["applied"], 1);
    assert_eq!(body["results"][1]["outcome"], "applied");
    assert_eq!(movements(&pool, till).await, 1);

    // Bounds.
    let r = test::call_service(&app, post(json!({ "ops": [] }))).await;
    assert_eq!(r.status(), 400);
    let many: Vec<Value> = (0..=madar_rust::sync::batch::MAX_OPS)
        .map(|_| movement(&s, till, Uuid::new_v4(), 1))
        .collect();
    let r = test::call_service(&app, post(json!({ "ops": many }))).await;
    assert_eq!(r.status(), 400);
    assert_eq!(movements(&pool, till).await, 1, "an oversized batch applies nothing");
}

// ── ledger retention ────────────────────────────────────────────────────────

/// Make a till (and everything on it) look `days` old, without firing the
/// emitters that would refresh its feed rows.
async fn age(pool: &PgPool, till: Uuid, days: i32, close: bool) {
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("SET session_replication_role = replica")
        .execute(&mut *conn)
        .await
        .unwrap();
    let at = format!("{days} days");
    sqlx::query(
        "UPDATE tills SET opened_at = now() - $2::interval, created_at = now() - $2::interval,
                updated_at = now() - $2::interval,
                status = CASE WHEN $3 THEN 'closed'::till_status ELSE status END,
                closed_at = CASE WHEN $3 THEN now() - $2::interval ELSE closed_at END
          WHERE id = $1",
    )
    .bind(till)
    .bind(&at)
    .bind(close)
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query("UPDATE till_cash_movements SET created_at = now() - $2::interval WHERE till_id = $1")
        .bind(till)
        .bind(&at)
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE sync_changes c SET changed_at = now() - $2::interval
          WHERE (c.type = 'till' AND c.entity_id = $1)
             OR (c.type = 'cash_movement' AND c.entity_id IN (SELECT id FROM till_cash_movements WHERE till_id = $1))",
    )
    .bind(till)
    .bind(&at)
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query("SET session_replication_role = origin")
        .execute(&mut *conn)
        .await
        .unwrap();
}

async fn feed_rows(pool: &PgPool, till: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM sync_changes c
          WHERE (c.type = 'till' AND c.entity_id = $1)
             OR (c.type = 'cash_movement' AND c.entity_id IN (SELECT id FROM till_cash_movements WHERE till_id = $1))",
    )
    .bind(till)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn add_movement(pool: &PgPool, s: &Shop, till: Uuid) {
    sqlx::query(
        "INSERT INTO till_cash_movements (till_id, amount, note, moved_by, kind) VALUES ($1, 100, 'x', $2, 'pay_in')",
    )
    .bind(till)
    .bind(s.teller)
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test]
async fn ledger_rows_past_the_retention_leave_the_feed_and_old_cursors_resync(pool: PgPool) {
    let s = shop(&pool).await;
    let old_closed = open_till(&pool, &s).await;
    add_movement(&pool, &s, old_closed).await;
    let old_open = open_till(&pool, &s).await;
    add_movement(&pool, &s, old_open).await;
    let days = (retention().num_days() + 5) as i32;
    age(&pool, old_closed, days, true).await;
    age(&pool, old_open, days, false).await;
    let cursor_before = head(&pool, s.branch).await;
    let purged_seq: i64 = sqlx::query_scalar(
        "SELECT max(seq) FROM sync_changes WHERE type IN ('till', 'cash_movement') AND entity_id IN
           (SELECT $1 UNION SELECT id FROM till_cash_movements WHERE till_id = $1)",
    )
    .bind(old_closed)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(feed_rows(&pool, old_closed).await, 2);
    let ledger_before: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM tills WHERE branch_id = $1),
                (SELECT count(*) FROM till_cash_movements m JOIN tills t ON t.id = m.till_id WHERE t.branch_id = $1)",
    )
    .bind(s.branch)
    .fetch_one(&pool)
    .await
    .unwrap();

    let report = sweep_once(&pool).await.unwrap();
    assert_eq!(report.ledger_rows_purged, 2, "{report:?}");
    assert_eq!(feed_rows(&pool, old_closed).await, 0, "the old closed till left the feed");
    assert_eq!(feed_rows(&pool, old_open).await, 2, "an open till's history stays: a snapshot still needs it");

    // The ledger itself is untouched (no-data-loss: this is the changefeed).
    let ledger_after: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM tills WHERE branch_id = $1),
                (SELECT count(*) FROM till_cash_movements m JOIN tills t ON t.id = m.till_id WHERE t.branch_id = $1)",
    )
    .bind(s.branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(ledger_before, ledger_after);

    // A cursor from before the purge resyncs; a current one does not.
    let wm: i64 = sqlx::query_scalar("SELECT purged_through_seq FROM sync_feed_watermarks WHERE branch_id = $1")
        .bind(s.branch)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(wm >= purged_seq);
    let stale = pull_core(&pool, s.org, &pull_req(s.branch, None), Some(purged_seq - 1))
        .await
        .unwrap();
    assert!(stale.resync_required);
    let fresh = pull_core(&pool, s.org, &pull_req(s.branch, None), Some(cursor_before))
        .await
        .unwrap();
    assert!(!fresh.resync_required);

    // The gap pass does not put the purged rows back.
    let again = sweep_once(&pool).await.unwrap();
    assert_eq!(again.ledger_rows_purged, 0);
    assert_eq!(feed_rows(&pool, old_closed).await, 0);

    // Inside the retention nothing is purged.
    let recent = open_till(&pool, &s).await;
    add_movement(&pool, &s, recent).await;
    age(&pool, recent, 3, true).await;
    sweep_once(&pool).await.unwrap();
    assert_eq!(feed_rows(&pool, recent).await, 2);
}

// ── incremental sweep ───────────────────────────────────────────────────────

#[sqlx::test]
async fn the_gap_pass_skips_a_branch_whose_feed_did_not_move(pool: PgPool) {
    let s = shop(&pool).await;
    let first = sweep_once(&pool).await.unwrap();
    assert!(first.branches_checked >= 1, "{first:?}");
    let second = sweep_once(&pool).await.unwrap();
    assert_eq!(second.branches_checked, 0, "nothing moved: {second:?}");
    assert!(second.branches_skipped >= 1);

    // A write moves the head: the branch is checked again.
    sqlx::query("INSERT INTO categories (org_id, name) VALUES ($1, 'Tea')")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let third = sweep_once(&pool).await.unwrap();
    assert_eq!(third.branches_checked, 1, "{third:?}");

    // A hole that moves no seq (a lost feed row) on an idle branch closes at
    // the idle recheck.
    sqlx::query("DELETE FROM sync_changes WHERE branch_id = $1 AND type = 'category'")
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();
    let idle = sweep_once(&pool).await.unwrap();
    assert_eq!(idle.upserts_emitted, 0, "not re-checked yet: {idle:?}");
    let later = sweep_once_at(&pool, chrono::Utc::now() + chrono::Duration::hours(7))
        .await
        .unwrap();
    assert!(later.upserts_emitted >= 1, "{later:?}");
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sync_changes WHERE branch_id = $1 AND type = 'category'",
    )
    .bind(s.branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 1);
}

/// The time-keyed live set equals the per-branch one when the window is all
/// of time, so the two functions cannot drift apart silently.
#[sqlx::test]
async fn sync_live_rows_since_matches_for_branch(pool: PgPool) {
    let s = shop(&pool).await;
    let till = open_till(&pool, &s).await;
    add_movement(&pool, &s, till).await;
    let old = open_till(&pool, &s).await;
    add_movement(&pool, &s, old).await;
    age(&pool, old, 400, true).await;
    sqlx::query("INSERT INTO categories (org_id, name) VALUES ($1, 'Coffee')")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let diff: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (
            (SELECT * FROM sync_live_rows_since($1, '-infinity') EXCEPT ALL SELECT * FROM sync_live_rows_for_branch($1))
            UNION ALL
            (SELECT * FROM sync_live_rows_for_branch($1) EXCEPT ALL SELECT * FROM sync_live_rows_since($1, '-infinity'))) d",
    )
    .bind(s.branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(diff, 0);
    // And a real window drops the old closed till, keeps the open one.
    let kept: Vec<Uuid> = sqlx::query_scalar(
        "SELECT entity_id FROM sync_live_rows_since($1, now() - interval '30 days') WHERE type = 'till'",
    )
    .bind(s.branch)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(kept, vec![till]);
    let _ = head(&pool, s.branch).await;
}

/// `SYNC_SWEEP_ENABLED` reads falsy values like every other background switch
/// (it used to honour only the literal `false`, so `=0` left it running), and
/// unset keeps the default: on.
#[::core::prelude::v1::test]
fn sync_sweep_switch_accepts_every_falsy_spelling() {
    use madar_rust::sync::pull::sweeper::disabled_by;
    for v in ["0", "false", "no", "off"] {
        assert!(disabled_by(Some(v)), "{v} switches the sweep off");
    }
    for v in ["1", "true", "yes", "on", ""] {
        assert!(!disabled_by(Some(v)), "{v:?} leaves it on");
    }
    assert!(!disabled_by(None), "unset: on by default");
}
