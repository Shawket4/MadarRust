//! The staff pool at the edges: the live route's gate, the replay path's
//! accept-and-flag, idempotency, and the daily reset.

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::{JwtSecret, create_token};
use crate::models::UserRole;
use crate::realtime::hub::BranchEventHub;

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn token(uid: Uuid, org: Uuid, role: UserRole) -> String {
    create_token(&secret(), uid, Some(org), role, None, 24).unwrap()
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(BranchEventHub::new()))
                .configure(crate::staff_pool::routes::configure)
                .configure(crate::sync::routes::configure),
        )
        .await
    };
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Pool Org', $2)")
        .bind(id)
        .bind(format!("pool-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

/// A branch in a fixed, whole-hour zone so the business day is never ambiguous.
async fn seed_branch(pool: &PgPool, org: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name, timezone) VALUES ($1, $2, $3, 'Africa/Cairo')")
        .bind(id)
        .bind(org)
        .bind(format!("Branch {id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, $3, $4, 'h', $5::user_role)",
    )
    .bind(id)
    .bind(org)
    .bind(format!("{role}-{id}"))
    .bind(format!("{id}@t.com"))
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn seed_item(pool: &PgPool, org: Uuid, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, name, base_price) VALUES ($1, $2, $3, 1000)")
        .bind(id)
        .bind(org)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    id
}

/// The pool, switched on for a scope with an allowance and a list.
async fn set_pool(pool: &PgPool, org: Uuid, branch: Option<Uuid>, allowance: i32, items: &[Uuid]) {
    sqlx::query(
        "INSERT INTO staff_pool_settings (org_id, branch_id, enabled, daily_allowance, eligible_item_ids) \
         VALUES ($1, $2, true, $3, $4) \
         ON CONFLICT (org_id, COALESCE(branch_id, '00000000-0000-0000-0000-000000000000'::uuid)) \
         DO UPDATE SET enabled = true, daily_allowance = EXCLUDED.daily_allowance, \
                       eligible_item_ids = EXCLUDED.eligible_item_ids",
    )
    .bind(org)
    .bind(branch)
    .bind(allowance)
    .bind(items)
    .execute(pool)
    .await
    .unwrap();
}

/// `orders.staff_drink.record` (223) for one person.
async fn allow_staff_drink(pool: &PgPool, org: Uuid, user: Uuid) {
    sqlx::query(
        "INSERT INTO user_overrides (org_id, user_id, capability_id, effect, reason) \
         VALUES ($1, $2, 223, 'allow', 'test')",
    )
    .bind(org)
    .bind(user)
    .execute(pool)
    .await
    .unwrap();
}

fn drink_body(id: Uuid, branch: Uuid, item: Uuid, note: &str, at: &str) -> Value {
    json!({
        "id": id, "branch_id": branch, "menu_item_id": item,
        "item_name": "Latte", "quantity": 1, "note": note, "recorded_at": at
    })
}

async fn flags(pool: &PgPool, author: Uuid) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT capability, reason FROM authz_replay_flags \
          WHERE author_id = $1 AND op = 'RecordStaffDrink' ORDER BY id",
    )
    .bind(author)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn rows_on(pool: &PgPool, branch: Uuid, day: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM staff_drinks WHERE branch_id = $1 AND business_date = $2::date",
    )
    .bind(branch)
    .bind(day)
    .fetch_one(pool)
    .await
    .unwrap()
}

// ── The live route ──────────────────────────────────────────────────────────

#[sqlx::test]
async fn a_teller_without_the_grant_is_refused_before_the_body_is_read(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_item(&pool, org, "Latte").await;
    let teller = seed_user(&pool, org, "teller").await;
    set_pool(&pool, org, None, 5, &[item]).await;

    // A well-formed request from a teller: refused, because the capability is
    // off for tellers by default and nothing here grants it.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/staff-pool/drinks")
            .insert_header(("Authorization", format!("Bearer {}", token(teller, org, UserRole::Teller))))
            .set_json(&drink_body(Uuid::new_v4(), branch, item, "Sara", "2026-09-19T09:00:00Z"))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 403, "a teller holds no staff drink by default");

    // A body that is not even a staff drink is rejected by the extractor, one
    // step before the guard. That ordering is actix's, not a decision here —
    // what matters is that it tells the caller nothing and records nothing.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/staff-pool/drinks")
            .insert_header(("Authorization", format!("Bearer {}", token(teller, org, UserRole::Teller))))
            .set_json(&json!({"branch_id": branch}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400);
    assert_eq!(rows_on(&pool, branch, "2026-09-19").await, 0);
}

#[sqlx::test]
async fn a_granted_teller_records_one_and_the_note_is_kept(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_item(&pool, org, "Latte").await;
    let teller = seed_user(&pool, org, "teller").await;
    allow_staff_drink(&pool, org, teller).await;
    set_pool(&pool, org, None, 5, &[item]).await;

    let id = Uuid::new_v4();
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/staff-pool/drinks")
            .insert_header(("Authorization", format!("Bearer {}", token(teller, org, UserRole::Teller))))
            .set_json(&drink_body(id, branch, item, "  for Sara, closing shift  ", "2026-09-19T09:00:00Z"))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let body: Value = test::read_body_json(resp).await;
    assert_eq!(body["note"], "for Sara, closing shift", "the note is trimmed, not lost");
    assert_eq!(body["overspent"], false);
    assert_eq!(body["business_date"], "2026-09-19");
    // Nobody is named as the drinker: the note is the only record of that.
    assert!(body.get("staff_member_id").is_none());
}

#[sqlx::test]
async fn the_live_route_refuses_a_drink_with_no_note(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_item(&pool, org, "Latte").await;
    let admin = seed_user(&pool, org, "org_admin").await;
    set_pool(&pool, org, None, 5, &[item]).await;
    let bearer = token(admin, org, UserRole::OrgAdmin);

    for note in ["", "   ", "\t\n"] {
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/staff-pool/drinks")
                .insert_header(("Authorization", format!("Bearer {bearer}")))
                .set_json(&drink_body(Uuid::new_v4(), branch, item, note, "2026-09-19T09:00:00Z"))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 400, "whitespace is not a note");
    }
    assert_eq!(rows_on(&pool, branch, "2026-09-19").await, 0);
}

#[sqlx::test]
async fn an_item_off_the_list_and_an_empty_list_are_both_refused_live(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let latte = seed_item(&pool, org, "Latte").await;
    let cake = seed_item(&pool, org, "Cheesecake").await;
    let admin = seed_user(&pool, org, "org_admin").await;
    let bearer = token(admin, org, UserRole::OrgAdmin);
    set_pool(&pool, org, None, 5, &[latte]).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/staff-pool/drinks")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&drink_body(Uuid::new_v4(), branch, cake, "Sara", "2026-09-19T09:00:00Z"))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400, "cheesecake is not a staff drink");

    // An empty list is a pool that is off, whatever `enabled` says.
    set_pool(&pool, org, None, 5, &[]).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/staff-pool/drinks")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&drink_body(Uuid::new_v4(), branch, latte, "Sara", "2026-09-19T09:00:00Z"))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400);
    assert_eq!(rows_on(&pool, branch, "2026-09-19").await, 0);
}

#[sqlx::test]
async fn a_branch_override_replaces_the_org_allowance_wholesale(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_item(&pool, org, "Latte").await;
    let admin = seed_user(&pool, org, "org_admin").await;
    let bearer = token(admin, org, UserRole::OrgAdmin);
    set_pool(&pool, org, None, 5, &[item]).await;
    set_pool(&pool, org, Some(branch), 1, &[item]).await;

    let today = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/staff-pool/today?branch_id={branch}"))
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .to_request(),
    )
    .await;
    assert_eq!(today.status(), 200);
    let body: Value = test::read_body_json(today).await;
    assert_eq!(body["allowance"], 1, "the branch's own number, not the org's 5");
    assert_eq!(body["enabled"], true);
}

// ── Replay ──────────────────────────────────────────────────────────────────

fn replay_envelope(teller: Uuid, request: Value) -> Value {
    json!({ "op": "record_staff_drink", "teller_id": teller, "request": request })
}

#[sqlx::test]
async fn an_overspend_lands_on_replay_and_is_flagged_never_refused(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_item(&pool, org, "Latte").await;
    let teller = seed_user(&pool, org, "teller").await;
    allow_staff_drink(&pool, org, teller).await;
    set_pool(&pool, org, None, 1, &[item]).await;
    let bearer = token(teller, org, UserRole::Teller);

    // Two drinks against an allowance of one. Both land; the second is marked.
    let mut marked = Vec::new();
    for _ in 0..2 {
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/sync/replay")
                .insert_header(("Authorization", format!("Bearer {bearer}")))
                .set_json(&replay_envelope(
                    teller,
                    drink_body(Uuid::new_v4(), branch, item, "the morning shift", "2026-09-19T09:00:00Z"),
                ))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 201, "a drink that was drunk always lands");
        let body: Value = test::read_body_json(resp).await;
        marked.push(body["overspent"].as_bool().unwrap());
    }
    assert_eq!(marked, vec![false, true], "the second drink is the overspend");
    assert_eq!(rows_on(&pool, branch, "2026-09-19").await, 2);

    let f = flags(&pool, teller).await;
    assert!(
        f.iter().any(|(cap, _)| cap.contains("overspent")),
        "the owner is told: {f:?}"
    );
}

#[sqlx::test]
async fn replaying_the_same_drink_twice_spends_one(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_item(&pool, org, "Latte").await;
    let teller = seed_user(&pool, org, "teller").await;
    allow_staff_drink(&pool, org, teller).await;
    set_pool(&pool, org, None, 5, &[item]).await;
    let bearer = token(teller, org, UserRole::Teller);
    let id = Uuid::new_v4();

    let mut codes = Vec::new();
    for _ in 0..3 {
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/sync/replay")
                .insert_header(("Authorization", format!("Bearer {bearer}")))
                .set_json(&replay_envelope(
                    teller,
                    drink_body(id, branch, item, "Sara", "2026-09-19T09:00:00Z"),
                ))
                .to_request(),
        )
        .await;
        codes.push(resp.status().as_u16());
    }
    assert_eq!(codes, vec![201, 200, 200], "a re-flush is the same drink");
    assert_eq!(rows_on(&pool, branch, "2026-09-19").await, 1);
}

#[sqlx::test]
async fn a_teller_with_no_grant_still_lands_the_drink_on_replay_and_is_flagged(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_item(&pool, org, "Latte").await;
    let teller = seed_user(&pool, org, "teller").await;
    // Deliberately NOT granted: the drink was still drunk.
    set_pool(&pool, org, None, 5, &[item]).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {}", token(teller, org, UserRole::Teller))))
            .set_json(&replay_envelope(
                teller,
                drink_body(Uuid::new_v4(), branch, item, "Sara", "2026-09-19T09:00:00Z"),
            ))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201, "accept, then flag — never drop");
    assert_eq!(rows_on(&pool, branch, "2026-09-19").await, 1);
    let f = flags(&pool, teller).await;
    assert!(!f.is_empty(), "a missing grant is reported to the owner");
}

#[sqlx::test]
async fn replay_refuses_only_the_one_thing_it_cannot_store_a_blank_note(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_item(&pool, org, "Latte").await;
    let teller = seed_user(&pool, org, "teller").await;
    allow_staff_drink(&pool, org, teller).await;
    set_pool(&pool, org, None, 5, &[item]).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {}", token(teller, org, UserRole::Teller))))
            .set_json(&replay_envelope(
                teller,
                drink_body(Uuid::new_v4(), branch, item, "   ", "2026-09-19T09:00:00Z"),
            ))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400);
    assert_eq!(rows_on(&pool, branch, "2026-09-19").await, 0);
}

#[sqlx::test]
async fn an_item_that_left_the_list_while_the_till_was_offline_lands_and_is_flagged(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let latte = seed_item(&pool, org, "Latte").await;
    let tea = seed_item(&pool, org, "Tea").await;
    let teller = seed_user(&pool, org, "teller").await;
    allow_staff_drink(&pool, org, teller).await;
    // The till rang a tea; by the time it drained, tea was off the list.
    set_pool(&pool, org, None, 5, &[latte]).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {}", token(teller, org, UserRole::Teller))))
            .set_json(&replay_envelope(
                teller,
                drink_body(Uuid::new_v4(), branch, tea, "Omar", "2026-09-19T09:00:00Z"),
            ))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201, "the tea was drunk");
    let body: Value = test::read_body_json(resp).await;
    assert_eq!(body["overspent"], true, "outside the rules reads as over, for the owner");
    let f = flags(&pool, teller).await;
    assert!(f.iter().any(|(cap, _)| cap.contains("item_not_eligible")), "{f:?}");
}

// ── The reset ───────────────────────────────────────────────────────────────

#[sqlx::test]
async fn the_pool_resets_on_the_branch_business_day_not_midnight_utc(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await; // Africa/Cairo, UTC+3
    let item = seed_item(&pool, org, "Latte").await;
    let teller = seed_user(&pool, org, "teller").await;
    allow_staff_drink(&pool, org, teller).await;
    set_pool(&pool, org, None, 2, &[item]).await;
    let bearer = token(teller, org, UserRole::Teller);

    let ring = |at: &'static str| {
        let b = bearer.clone();
        async move {
            test::TestRequest::post()
                .uri("/sync/replay")
                .insert_header(("Authorization", format!("Bearer {b}")))
                .set_json(&replay_envelope(
                    teller,
                    drink_body(Uuid::new_v4(), branch, item, "shift", at),
                ))
                .to_request()
        }
    };

    // 20:00 and 20:59 UTC are both the 19th in Cairo (23:00, 23:59 local).
    for at in ["2026-09-19T20:00:00Z", "2026-09-19T20:59:00Z"] {
        let resp = test::call_service(&app, ring(at).await).await;
        assert_eq!(resp.status(), 201);
        let body: Value = test::read_body_json(resp).await;
        assert_eq!(body["business_date"], "2026-09-19");
        assert_eq!(body["overspent"], false, "both fit the allowance of 2");
    }

    // 21:00 UTC is local midnight: a NEW business day, and the pool is fresh —
    // even though UTC is still on the 19th.
    let resp = test::call_service(&app, ring("2026-09-19T21:00:00Z").await).await;
    assert_eq!(resp.status(), 201);
    let body: Value = test::read_body_json(resp).await;
    assert_eq!(body["business_date"], "2026-09-20", "the branch turned the page, not UTC");
    assert_eq!(
        body["overspent"], false,
        "the third drink of the UTC day is the first of the branch's day"
    );

    assert_eq!(rows_on(&pool, branch, "2026-09-19").await, 2);
    assert_eq!(rows_on(&pool, branch, "2026-09-20").await, 1);
}

#[sqlx::test]
async fn the_report_lists_the_drinks_with_their_notes_newest_first(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_item(&pool, org, "Latte").await;
    let admin = seed_user(&pool, org, "org_admin").await;
    let bearer = token(admin, org, UserRole::OrgAdmin);
    set_pool(&pool, org, None, 1, &[item]).await;

    for (note, at) in [("for Sara", "2026-09-19T09:00:00Z"), ("for Omar", "2026-09-19T10:00:00Z")] {
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/staff-pool/drinks")
                .insert_header(("Authorization", format!("Bearer {bearer}")))
                .set_json(&drink_body(Uuid::new_v4(), branch, item, note, at))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 201);
    }

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/staff-pool/drinks?branch_id={branch}&from=2026-09-19&to=2026-09-19"))
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: Value = test::read_body_json(resp).await;
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    // Newest first, and the note — the only record of who drank it — is there.
    assert_eq!(rows[0]["note"], "for Omar");
    assert_eq!(rows[1]["note"], "for Sara");
    assert_eq!(rows[0]["overspent"], true, "the second drink went past an allowance of 1");
    assert_eq!(rows[1]["overspent"], false);

    // The owner's "what went over" view.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/staff-pool/drinks?branch_id={branch}&overspent_only=true"))
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .to_request(),
    )
    .await;
    let body: Value = test::read_body_json(resp).await;
    assert_eq!(body.as_array().unwrap().len(), 1);
}

#[sqlx::test]
async fn a_teller_without_the_grant_cannot_read_the_notes(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/staff-pool/drinks?branch_id={branch}"))
            .insert_header(("Authorization", format!("Bearer {}", token(teller, org, UserRole::Teller))))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 403);
}

// ── The feed ────────────────────────────────────────────────────────────────

/// The till reads the allowance out of the `branch_settings` projection, and
/// counts the day out of `staff_drink` rows. Neither existed until the
/// projection carried them — the tables and triggers alone left the POS
/// reading the pool as OFF, with the action never appearing.
#[sqlx::test]
async fn the_feed_carries_the_settings_and_the_drinks_to_the_till(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_item(&pool, org, "Latte").await;
    let admin = seed_user(&pool, org, "org_admin").await;
    let bearer = token(admin, org, UserRole::OrgAdmin);
    set_pool(&pool, org, None, 4, &[item]).await;

    let drink_id = Uuid::new_v4();
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/staff-pool/drinks")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&drink_body(drink_id, branch, item, "for Sara", "2026-09-19T09:00:00Z"))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);

    let mut conn = pool.acquire().await.unwrap();

    // The allowance rides the projection the tablet already reads.
    let settings = crate::sync::pull::projection::project(
        &mut conn, org, branch, "branch_settings", &[branch],
    )
    .await
    .unwrap();
    let sp = settings[&branch]["staff_pool"].clone();
    assert_eq!(sp["enabled"], true, "the till must not read the pool as off: {settings:?}");
    assert_eq!(sp["daily_allowance"], 4);
    assert_eq!(sp["eligible_item_ids"][0], item.to_string());

    // And the drink itself reaches every device of the branch.
    let drinks = crate::sync::pull::projection::project(
        &mut conn, org, branch, "staff_drink", &[drink_id],
    )
    .await
    .unwrap();
    let d = &drinks[&drink_id];
    assert_eq!(d["branch_id"], branch.to_string());
    assert_eq!(d["quantity"], 1);
    assert_eq!(d["note"], "for Sara");
    assert_eq!(d["overspent"], false);
    // The business day is resolved SERVER-side and carried, so a till never
    // re-derives it from an instant and a timezone it might not have.
    assert_eq!(d["business_date"], "2026-09-19");
}

/// A branch row overrides the org one wholesale in the projection too, or a
/// branch would run on numbers its own settings screen does not show.
#[sqlx::test]
async fn the_projection_prefers_the_branch_override(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_item(&pool, org, "Latte").await;
    set_pool(&pool, org, None, 9, &[item]).await;
    set_pool(&pool, org, Some(branch), 2, &[item]).await;

    let mut conn = pool.acquire().await.unwrap();
    let settings = crate::sync::pull::projection::project(
        &mut conn, org, branch, "branch_settings", &[branch],
    )
    .await
    .unwrap();
    assert_eq!(settings[&branch]["staff_pool"]["daily_allowance"], 2);
}

/// `staff_drink` is a wire type the POS may ask for, and a LEDGER one — the
/// rows are dated and grow forever, so a full snapshot windows them.
#[test]
fn staff_drink_is_a_windowed_wire_type() {
    assert!(crate::sync::pull::ALL_TYPES.contains(&"staff_drink"));
    assert!(
        crate::sync::pull::is_ledger("staff_drink"),
        "an ever-growing dated table must not be checksummed in full"
    );
}
