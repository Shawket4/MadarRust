//! Dawam Phase B (platform): the staff app's sign-out and its pushes.
//!
//! - Signing out on the phone revokes the device (its token no longer
//!   refreshes or works) and every Dawam push of the employee, so a signed-out
//!   phone never shows the next person's names or amounts (APP-6, audit 06 B3).
//!   Only the phone's own staff session may sign out (refusals).
//! - A flag seen again on every ping tells each manager once, not once a ping
//!   (APP-6, audit 06 B7); a new flag after the first is resolved tells again.
//! - A flag's acts each need their own right too (PM-4, AT-11): confirming a
//!   cover is `hr.shift_cover.confirm`, signing a new phone out is
//!   `hr.staff.edit` — the same rights as the covers list and the employee.

use actix_web::{App, test, web};
use chrono::{Duration, NaiveTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::JwtSecret;
use madar_rust::models::UserRole;

mod common;
use common::employees::{authed, employee, phone_token, session};

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn token_for(user: Uuid, org: Uuid, role: UserRole) -> String {
    madar_rust::auth::jwt::create_token(&secret(), user, Some(org), role, None, 24).unwrap()
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .configure(madar_rust::staff::routes::configure),
        )
        .await
    };
}

/// `$token` is a user's JWT, or a staff-app phone's `token|device`.
macro_rules! call {
    ($app:expr, $method:ident, $uri:expr, $token:expr) => {{
        let req = authed(test::TestRequest::$method().uri(&$uri), &$token).to_request();
        test::call_service(&$app, req).await
    }};
    ($app:expr, $method:ident, $uri:expr, $token:expr, $body:expr) => {{
        let req = authed(test::TestRequest::$method().uri(&$uri), &$token)
            .set_json(&$body)
            .to_request();
        test::call_service(&$app, req).await
    }};
}

async fn json_of(resp: actix_web::dev::ServiceResponse) -> Value {
    test::read_body_json(resp).await
}

const LAT: f64 = 29.9792;
const LNG: f64 = 31.1342;

struct F {
    org: Uuid,
    branch: Uuid,
    owner: Uuid,
    /// The owner's own employee record: where the owner's app inbox lives.
    owner_e: Uuid,
    a: Uuid,
    b: Uuid,
}

async fn user(pool: &PgPool, org: Uuid, name: &str, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, $3, $4, 'hash', $5::user_role)",
    )
    .bind(id)
    .bind(org)
    .bind(name)
    .bind(format!("{id}@test.com"))
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn seed(pool: &PgPool) -> F {
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    let org = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, modules) VALUES ($1, 'Cafe', $2, '{pos,dawam}')",
    )
    .bind(org)
    .bind(format!("org-{org}"))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO attendance_settings (org_id, rules_saved_at) VALUES ($1, now() - INTERVAL '60 days')")
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    let branch = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO branches (id, org_id, name, timezone, latitude, longitude, geo_radius_meters) \
         VALUES ($1, $2, 'Branch', 'UTC'::timezone_name, $3, $4, 200)",
    )
    .bind(branch)
    .bind(org)
    .bind(LAT)
    .bind(LNG)
    .execute(pool)
    .await
    .unwrap();
    let owner = user(pool, org, "Owner", "org_admin").await;
    let owner_e = employee(pool, org, "Owner", Some(owner), None, false, &[branch], 0).await;
    let a = employee(
        pool,
        org,
        "Amal",
        None,
        Some("+201012345678"),
        true,
        &[branch],
        600_000,
    )
    .await;
    let b = employee(
        pool,
        org,
        "Bassem",
        None,
        Some("+201012345679"),
        true,
        &[branch],
        600_000,
    )
    .await;
    F {
        org,
        branch,
        owner,
        owner_e,
        a,
        b,
    }
}

async fn live_pushes(pool: &PgPool, employee: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM push_devices WHERE employee_id = $1 AND revoked_at IS NULL",
    )
    .bind(employee)
    .fetch_one(pool)
    .await
    .unwrap()
}

// ── sign-out ───────────────────────────────────────────────────────────────

#[sqlx::test]
async fn signing_out_forgets_the_phone_and_its_pushes(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let a = session(&pool, f.a).await;
    let a_tok = format!("{}|{}", a.token, a.device);
    let b_tok = phone_token(&pool, f.b).await;
    for tok in [&a_tok, &b_tok] {
        let token = format!("fcm-{}", Uuid::new_v4());
        let resp = call!(
            app,
            put,
            "/staff/me/push-token",
            tok,
            json!({ "token": token, "locale": "en" })
        );
        assert_eq!(resp.status(), 204);
    }
    assert_eq!(live_pushes(&pool, f.a).await, 1);

    let resp = call!(app, post, "/staff/me/sign-out", a_tok);
    assert_eq!(resp.status(), 204);
    assert_eq!(
        live_pushes(&pool, f.a).await,
        0,
        "no more pushes to that phone"
    );
    let revoked: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM staff_devices WHERE id = $1")
            .bind(a.device_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(revoked, "the device can't refresh a token again");
    let resp = call!(app, get, "/staff/me/context", a_tok);
    assert_eq!(resp.status(), 401, "the signed-out phone is refused");
    assert_eq!(json_of(resp).await["code"], "DEVICE_REVOKED");

    // Signing out twice is harmless (a retry after a lost answer)… but only
    // with a live session: the revoked phone can't reach the route at all.
    let resp = call!(app, post, "/staff/me/sign-out", a_tok);
    assert_eq!(resp.status(), 401);

    // Another person's phone is untouched.
    assert_eq!(live_pushes(&pool, f.b).await, 1);
    let resp = call!(app, get, "/staff/me/context", b_tok);
    assert_eq!(resp.status(), 200);
}

#[sqlx::test]
async fn only_the_phone_itself_can_sign_out(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let b_tok = phone_token(&pool, f.b).await;
    let resp = call!(
        app,
        put,
        "/staff/me/push-token",
        b_tok,
        json!({ "token": "fcm-b", "locale": "ar" })
    );
    assert_eq!(resp.status(), 204);

    // A dashboard session (even the owner's) is not a phone.
    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);
    let resp = call!(app, post, "/staff/me/sign-out", owner);
    assert_eq!(resp.status(), 403);
    assert_eq!(json_of(resp).await["code"], "STAFF_APP_ONLY");
    // No session at all.
    let req = test::TestRequest::post()
        .uri("/staff/me/sign-out")
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 401);
    // A staff token without its device header.
    let (t, _) = b_tok.split_once('|').unwrap();
    let resp = call!(app, post, "/staff/me/sign-out", t);
    assert_eq!(resp.status(), 401);

    assert_eq!(live_pushes(&pool, f.b).await, 1, "nothing was revoked");
    let resp = call!(app, get, "/staff/me/context", b_tok);
    assert_eq!(resp.status(), 200);
}

// ── one push per flag ──────────────────────────────────────────────────────

async fn flag_notices(pool: &PgPool, manager: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM staff_notifications \
          WHERE employee_id = $1 AND key = 'staff.n_flag_left_mid_shift'",
    )
    .bind(manager)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn a_flag_seen_on_every_ping_tells_the_manager_once(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let now = Utc::now().time();
    let start = (Utc::now() - Duration::minutes(30)).time();
    let end = (Utc::now() + Duration::hours(4)).time();
    if end < now || start > now {
        return; // the few minutes around midnight UTC
    }
    let shift: Uuid = sqlx::query_scalar(
        "INSERT INTO work_shifts (org_id, branch_id, name, start_time, end_time) \
         VALUES ($1, $2, 'Day', $3, $4) RETURNING id",
    )
    .bind(f.org)
    .bind(f.branch)
    .bind(start)
    .bind(end)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO staff_schedules (org_id, employee_id, work_shift_id, effective_from) \
         VALUES ($1, $2, $3, CURRENT_DATE - 60)",
    )
    .bind(f.org)
    .bind(f.a)
    .bind(shift)
    .execute(&pool)
    .await
    .unwrap();
    let tok = phone_token(&pool, f.a).await;
    let resp = call!(
        app,
        post,
        "/staff/me/check-in",
        tok,
        json!({ "branch_id": f.branch, "latitude": LAT, "longitude": LNG })
    );
    assert!(resp.status().is_success(), "{}", resp.status());

    let away = |i: u32| json!({ "latitude": LAT + 0.02 + f64::from(i) * 0.001, "longitude": LNG, "accuracy_meters": 10.0 });
    for i in 0..5 {
        let resp = call!(app, post, "/staff/me/pings", tok, away(i));
        assert_eq!(resp.status(), 200);
    }
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM attendance_flags \
          WHERE employee_id = $1 AND kind = 'left_mid_shift' AND resolution IS NULL",
    )
    .bind(f.a)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(open, 1, "one open flag, updated by each ping");
    assert_eq!(
        flag_notices(&pool, f.owner_e).await,
        1,
        "the manager heard once, not once a ping"
    );

    // The manager lets it go; she walks off again later: a NEW flag, told again.
    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);
    let flags = json_of(call!(
        app,
        get,
        format!("/staff/flags?branch_id={}", f.branch),
        owner
    ))
    .await;
    let id = flags
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["kind"] == "left_mid_shift")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = call!(
        app,
        patch,
        format!("/staff/flags/{id}"),
        owner,
        json!({ "action": "ignore" })
    );
    assert_eq!(resp.status(), 200);
    for i in 5..9 {
        let resp = call!(app, post, "/staff/me/pings", tok, away(i));
        assert_eq!(resp.status(), 200);
    }
    assert_eq!(flag_notices(&pool, f.owner_e).await, 2);
    // The person who left never hears about their own flag.
    assert_eq!(flag_notices(&pool, f.a).await, 0);
    let _ = f.b;
}

// ── a flag's acts on their own capabilities ────────────────────────────────

/// A branch manager at `branch`, with `caps` taken away from managers.
async fn manager_without(pool: &PgPool, f: &F, caps: &[&str]) -> String {
    let m = user(pool, f.org, "Manager", "branch_manager").await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(m)
        .bind(f.branch)
        .execute(pool)
        .await
        .unwrap();
    for cap in caps {
        sqlx::query(
            "DELETE FROM org_role_grants g USING org_roles r \
              WHERE r.id = g.org_role_id AND r.org_id = $1 AND r.kind::text = 'branch_manager' \
                AND g.capability_id = (SELECT id FROM capabilities WHERE key = $2)",
        )
        .bind(f.org)
        .bind(*cap)
        .execute(pool)
        .await
        .unwrap();
    }
    sqlx::query("SELECT authz_bump_epoch($1)")
        .bind(f.org)
        .execute(pool)
        .await
        .unwrap();
    token_for(m, f.org, UserRole::BranchManager)
}

async fn open_flag(pool: &PgPool, f: &F, who: Uuid, kind: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO attendance_flags (org_id, employee_id, branch_id, kind) \
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(f.org)
    .bind(who)
    .bind(f.branch)
    .bind(kind)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn resolution(pool: &PgPool, flag: Uuid) -> Option<String> {
    sqlx::query_scalar("SELECT resolution FROM attendance_flags WHERE id = $1")
        .bind(flag)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn confirming_a_cover_flag_needs_the_cover_right(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let flag = open_flag(&pool, &f, f.a, "cover").await;
    let m = manager_without(&pool, &f, &["hr.shift_cover.confirm"]).await;
    let uri = format!("/staff/flags/{flag}");

    let resp = call!(app, patch, uri, m, json!({ "action": "confirm" }));
    assert_eq!(
        resp.status(),
        403,
        "attendance.edit alone does not confirm a cover"
    );
    assert_eq!(resolution(&pool, flag).await, None, "nothing was written");
    // What attendance.edit does allow still works for him.
    let resp = call!(app, patch, uri, m, json!({ "action": "ignore" }));
    assert_eq!(resp.status(), 200);
    assert_eq!(resolution(&pool, flag).await.as_deref(), Some("ignored"));

    // A manager who holds it (the default) confirms.
    let flag = open_flag(&pool, &f, f.b, "cover").await;
    let m2 = {
        let id = user(&pool, f.org, "Manager 2", "branch_manager").await;
        sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
            .bind(id)
            .bind(f.branch)
            .execute(&pool)
            .await
            .unwrap();
        id
    };
    // Give it back to managers, then confirm.
    sqlx::query(
        "INSERT INTO org_role_grants (org_role_id, org_id, capability_id, source) \
         SELECT r.id, r.org_id, c.id, 'custom' FROM org_roles r, capabilities c \
          WHERE r.org_id = $1 AND r.kind::text = 'branch_manager' AND c.key = 'hr.shift_cover.confirm'",
    )
    .bind(f.org)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("SELECT authz_bump_epoch($1)")
        .bind(f.org)
        .execute(&pool)
        .await
        .unwrap();
    let m2 = token_for(m2, f.org, UserRole::BranchManager);
    let resp = call!(
        app,
        patch,
        format!("/staff/flags/{flag}"),
        m2,
        json!({ "action": "confirm" })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(resolution(&pool, flag).await.as_deref(), Some("confirmed"));
}

#[sqlx::test]
async fn revoking_a_new_phone_from_a_flag_needs_the_staff_edit_right(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let flag = open_flag(&pool, &f, f.a, "new_phone").await;
    let tok = phone_token(&pool, f.a).await;
    let m = manager_without(&pool, &f, &["hr.staff.edit"]).await;
    let uri = format!("/staff/flags/{flag}");

    let resp = call!(app, patch, uri, m, json!({ "action": "revoke" }));
    assert_eq!(resp.status(), 403);
    assert_eq!(resolution(&pool, flag).await, None);
    let resp = call!(app, get, "/staff/me/context", tok);
    assert_eq!(resp.status(), 200, "the phone still works");

    // The owner (who holds it) signs the phone out.
    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);
    let resp = call!(app, patch, uri, owner, json!({ "action": "revoke" }));
    assert_eq!(resp.status(), 200);
    assert_eq!(resolution(&pool, flag).await.as_deref(), Some("revoked"));
    let resp = call!(app, get, "/staff/me/context", tok);
    assert_eq!(resp.status(), 401, "the revoked phone is refused");
}
