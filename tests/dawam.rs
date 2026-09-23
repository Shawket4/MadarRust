//! Dawam: phone sign-in, presence flags, the published week, swaps and open
//! shifts, pay lines under a limit, paying a period, and the inbox.

use actix_web::{App, test, web};
use chrono::{Duration, NaiveDate, NaiveTime, TimeZone, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::JwtSecret;
use madar_rust::models::UserRole;
use madar_rust::staff::dawam::{signin, week_start};

mod common;
use common::employees::{authed, phone_token};

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
                .route(
                    "/auth/staff/otp/request",
                    web::post().to(signin::otp_request),
                )
                .route("/auth/staff/otp/verify", web::post().to(signin::otp_verify))
                .route("/auth/staff/refresh", web::post().to(signin::refresh))
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

/// `a` is a cashier who is also on payroll (a LINKED employee, `a_user` her
/// account); `b` has no Madar account at all and signs in to the app with his
/// number (an APP employee). Both work at `branch`.
struct F {
    org: Uuid,
    branch: Uuid,
    owner: Uuid,
    a: Uuid,
    a_user: Uuid,
    b: Uuid,
}

const LAT: f64 = 29.9792;
const LNG: f64 = 31.1342;

async fn user(pool: &PgPool, org: Uuid, name: &str, role: &str, phone: Option<&str>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, phone, password_hash, role) \
         VALUES ($1, $2, $3, $4, $5, 'hash', $6::user_role)",
    )
    .bind(id)
    .bind(org)
    .bind(name)
    .bind(format!("{id}@test.com"))
    .bind(phone)
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// The owner on payroll too, so the owner's app inbox can be read.
async fn owner_employee(pool: &PgPool, f: &F) -> Uuid {
    common::employees::employee(
        pool,
        f.org,
        "Owner",
        Some(f.owner),
        None,
        false,
        &[f.branch],
        0,
    )
    .await
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

    // The owner saved the rules at set-up (RU-1): people may clock in.
    sqlx::query("INSERT INTO attendance_settings (org_id, rules_saved_at) VALUES ($1, now())")
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
    let owner = user(pool, org, "Owner", "org_admin", None).await;
    let a_user = user(pool, org, "Amal", "teller", Some("+201012345678")).await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(a_user)
        .bind(branch)
        .execute(pool)
        .await
        .unwrap();
    let a = common::employees::employee(
        pool,
        org,
        "Amal",
        Some(a_user),
        Some("+201012345678"),
        true,
        &[branch],
        600_000,
    )
    .await;
    let b = common::employees::employee(
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
        a,
        a_user,
        b,
    }
}

async fn shift(pool: &PgPool, f: &F, name: &str, start: NaiveTime, end: NaiveTime) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO work_shifts (org_id, branch_id, name, start_time, end_time) \
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(f.org)
    .bind(f.branch)
    .bind(name)
    .bind(start)
    .bind(end)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn every_day(pool: &PgPool, f: &F, u: Uuid, shift: Uuid) {
    sqlx::query(
        "INSERT INTO staff_schedules (org_id, employee_id, work_shift_id, effective_from) \
         VALUES ($1, $2, $3, CURRENT_DATE - 60)",
    )
    .bind(f.org)
    .bind(u)
    .bind(shift)
    .execute(pool)
    .await
    .unwrap();
}

fn t(h: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, 0, 0).unwrap()
}

async fn publish_week(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    f: &F,
    day: NaiveDate,
) {
    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);
    let resp = call!(
        app,
        post,
        "/staff/roster/publish",
        owner,
        json!({ "branch_id": f.branch, "week_start": day })
    );
    assert_eq!(resp.status(), 204);
}

async fn keys_for(pool: &PgPool, u: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT key FROM staff_notifications WHERE employee_id = $1 ORDER BY created_at",
    )
    .bind(u)
    .fetch_all(pool)
    .await
    .unwrap()
}

// ── sign-in ────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn a_phone_code_signs_in_and_binds_one_phone(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner_e = owner_employee(&pool, &f).await;

    let unknown = test::TestRequest::post()
        .uri("/auth/staff/otp/request")
        .set_json(json!({ "phone": "01099999999" }))
        .to_request();
    assert_eq!(
        test::call_service(&app, unknown).await.status(),
        404,
        "no self-registration"
    );

    let sign_in = async |model: &str| -> Value {
        sqlx::query("DELETE FROM staff_otp")
            .execute(&pool)
            .await
            .unwrap();
        let req = test::TestRequest::post()
            .uri("/auth/staff/otp/request")
            .set_json(json!({ "phone": "01012345678" }))
            .to_request();
        assert_eq!(test::call_service(&app, req).await.status(), 200);
        let code: String = sqlx::query_scalar("SELECT code FROM staff_otp")
            .fetch_one(&pool)
            .await
            .unwrap();
        let wrong = if code == "000000" { "111111" } else { "000000" };
        let req = test::TestRequest::post()
            .uri("/auth/staff/otp/verify")
            .set_json(json!({ "phone": "01012345678", "code": wrong }))
            .to_request();
        assert_eq!(test::call_service(&app, req).await.status(), 400);
        let req = test::TestRequest::post()
            .uri("/auth/staff/otp/verify")
            .set_json(json!({ "phone": "01012345678", "code": code, "model": model }))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        json_of(resp).await
    };

    let first = sign_in("Pixel").await;
    assert_eq!(first["employee_id"], json!(f.a));
    let token = first["token"].as_str().unwrap().to_string();
    let device = first["device_token"].as_str().unwrap().to_string();

    let today = |dev: Option<String>| {
        let mut r = test::TestRequest::post()
            .uri("/staff/me/check-in")
            .set_json(json!({ "branch_id": f.branch, "latitude": LAT, "longitude": LNG }))
            .insert_header(("Authorization", format!("Bearer {token}")));
        if let Some(d) = dev {
            r = r.insert_header(("X-Staff-Device", d));
        }
        r.to_request()
    };
    assert_eq!(
        test::call_service(&app, today(None)).await.status(),
        401,
        "a bound account needs its phone"
    );
    assert_ne!(
        test::call_service(&app, today(Some(device.clone())))
            .await
            .status(),
        401
    );

    let second = sign_in("iPhone").await;
    assert_eq!(second["new_phone"], json!(true));
    assert_eq!(
        test::call_service(&app, today(Some(device))).await.status(),
        401,
        "the old phone is signed out"
    );
    let flagged: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM attendance_flags WHERE employee_id = $1 AND kind = 'new_phone'",
    )
    .bind(f.a)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(flagged, 1);
    assert!(
        keys_for(&pool, owner_e)
            .await
            .contains(&"staff.n_new_phone".to_string())
    );
}

// ── presence ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn leaving_mid_shift_raises_a_flag_the_manager_can_deduct(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let now = Utc::now().time();
    let start = (Utc::now() - Duration::minutes(30)).time();
    let end = (Utc::now() + Duration::hours(4)).time();
    if end < now || start > now {
        return; // ponytail: skip the few minutes around midnight UTC
    }
    let s = shift(&pool, &f, "Day", start, end).await;
    every_day(&pool, &f, f.a, s).await;
    let tok = phone_token(&pool, f.a).await;
    let resp = call!(
        app,
        post,
        "/staff/me/check-in",
        tok,
        json!({ "branch_id": f.branch, "latitude": LAT, "longitude": LNG })
    );
    assert!(resp.status().is_success(), "{}", resp.status());

    let mut flags = Vec::new();
    for i in 0..2 {
        let resp = call!(
            app,
            post,
            "/staff/me/pings",
            tok,
            json!({ "latitude": LAT + 0.02 + f64::from(i) * 0.001, "longitude": LNG, "accuracy_meters": 10.0 + f64::from(i) })
        );
        assert_eq!(resp.status(), 200);
        let body = json_of(resp).await;
        assert_eq!(body["inside"], json!(false));
        flags.extend(body["flags"].as_array().unwrap().clone());
    }
    assert!(flags.contains(&json!("left_mid_shift")), "{flags:?}");

    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);
    let list = json_of(call!(
        app,
        get,
        format!("/staff/flags?branch_id={}", f.branch),
        owner
    ))
    .await;
    let flag = list
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["kind"] == "left_mid_shift")
        .unwrap()
        .clone();
    let resp = call!(
        app,
        patch,
        format!("/staff/flags/{}", flag["id"].as_str().unwrap()),
        owner,
        json!({ "action": "deduct", "amount_piastres": 5000 })
    );
    assert_eq!(resp.status(), 200);
    let deducted: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount_piastres), 0)::bigint FROM payroll_deductions WHERE employee_id = $1",
    )
    .bind(f.a)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(deducted, 5000);
    assert!(
        keys_for(&pool, f.a)
            .await
            .contains(&"staff.n_deduction_added".to_string())
    );
}

// ── the week ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn staff_see_only_published_weeks_and_hear_about_changes(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let s = shift(&pool, &f, "Morning", t(8), t(16)).await;
    every_day(&pool, &f, f.a, s).await;
    let ws = week_start(Utc::now().date_naive());
    let to = ws + Duration::days(6);
    let me = phone_token(&pool, f.a).await;
    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);
    let uri = format!("/staff/me/roster?from={ws}&to={to}");

    let body = json_of(call!(app, get, uri, me)).await;
    assert_eq!(
        body["shifts"].as_array().unwrap().len(),
        0,
        "a draft week is invisible"
    );
    assert_eq!(body["unpublished_weeks"], json!([ws]));

    let resp = call!(
        app,
        post,
        "/staff/roster/publish",
        owner,
        json!({ "branch_id": f.branch, "week_start": ws + Duration::days(3) })
    );
    assert_eq!(resp.status(), 204);
    let body = json_of(call!(app, get, uri, me)).await;
    assert_eq!(body["shifts"].as_array().unwrap().len(), 7);
    assert!(
        keys_for(&pool, f.a)
            .await
            .contains(&"staff.n_week_published".to_string())
    );

    let manager_view = json_of(call!(
        app,
        get,
        format!("/staff/roster?branch_id={}&from={ws}&to={to}", f.branch),
        owner
    ))
    .await;
    assert_eq!(manager_view["published_weeks"], json!([ws]));
    assert_eq!(manager_view["shifts"].as_array().unwrap().len(), 7);

    let day = ws + Duration::days(2);
    let resp = call!(
        app,
        put,
        "/staff/schedules/overrides",
        owner,
        json!({ "employee_id": f.a, "on_date": day, "work_shift_id": null, "reason": "Day off" })
    );
    assert_eq!(resp.status(), 200);
    let body = json_of(call!(app, get, uri, me)).await;
    assert_eq!(body["shifts"].as_array().unwrap().len(), 6);
    assert!(
        keys_for(&pool, f.a)
            .await
            .contains(&"staff.n_shift_changed".to_string())
    );
}

#[sqlx::test]
async fn a_swap_needs_the_colleague_then_the_manager(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let morning = shift(&pool, &f, "Morning", t(8), t(16)).await;
    let evening = shift(&pool, &f, "Evening", t(16), t(23)).await;
    every_day(&pool, &f, f.a, morning).await;
    every_day(&pool, &f, f.b, evening).await;
    let day = Utc::now().date_naive() + Duration::days(3);
    let (ta, tb) = (phone_token(&pool, f.a).await, phone_token(&pool, f.b).await);
    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);
    // Swaps are of the published roster only (SC-3, SC-8).
    publish_week(&app, &f, day).await;

    let resp = call!(
        app,
        post,
        "/staff/me/swaps",
        ta,
        json!({
        "my_date": day, "my_shift_id": morning, "peer_id": f.b, "peer_date": day, "peer_shift_id": evening })
    );
    assert_eq!(resp.status(), 201);
    let id = json_of(resp).await["id"].as_str().unwrap().to_string();

    let early = call!(
        app,
        patch,
        format!("/staff/swaps/{id}/decision"),
        owner,
        json!({ "approve": true })
    );
    assert_eq!(early.status(), 404, "the colleague agrees first");
    let resp = call!(
        app,
        patch,
        format!("/staff/me/swaps/{id}"),
        tb,
        json!({ "approve": true })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(json_of(resp).await["status"], "pending");
    let resp = call!(
        app,
        patch,
        format!("/staff/swaps/{id}/decision"),
        owner,
        json!({ "approve": true })
    );
    assert_eq!(resp.status(), 204);

    let view = json_of(call!(
        app,
        get,
        format!("/staff/roster?branch_id={}&from={day}&to={day}", f.branch),
        owner
    ))
    .await;
    let who = |u: Uuid| -> Vec<String> {
        view["shifts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["employee_id"] == json!(u))
            .map(|s| s["shift_name"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(who(f.a), vec!["Evening"]);
    assert_eq!(who(f.b), vec!["Morning"]);
}

#[sqlx::test]
async fn a_claimed_open_shift_is_the_claimers_once_approved(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let s = shift(&pool, &f, "Extra", t(10), t(18)).await;
    let day = Utc::now().date_naive() + Duration::days(2);
    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);
    let (ta, tb) = (phone_token(&pool, f.a).await, phone_token(&pool, f.b).await);
    // Staff claim only in a published week (SC-9).
    publish_week(&app, &f, day).await;
    let resp = call!(
        app,
        post,
        "/staff/open-shifts",
        owner,
        json!({ "branch_id": f.branch, "work_shift_id": s, "on_date": day })
    );
    assert_eq!(resp.status(), 201);
    let id = json_of(resp).await["id"].as_str().unwrap().to_string();

    assert_eq!(
        call!(
            app,
            post,
            format!("/staff/open-shifts/{id}/claim"),
            ta,
            json!({})
        )
        .status(),
        200
    );
    assert_eq!(
        call!(
            app,
            post,
            format!("/staff/open-shifts/{id}/claim"),
            tb,
            json!({})
        )
        .status(),
        409
    );
    // The manager's queue lists it with who claimed it; a teller can't read it.
    let list = json_of(call!(
        app,
        get,
        format!("/staff/open-shifts?from={day}&to={day}"),
        owner
    ))
    .await;
    assert_eq!(list[0]["status"], "claimed");
    assert_eq!(list[0]["claimed_by"], json!(f.a));
    assert_eq!(
        call!(
            app,
            get,
            format!("/staff/open-shifts?from={day}&to={day}"),
            ta
        )
        .status(),
        403
    );
    assert_eq!(
        call!(
            app,
            patch,
            format!("/staff/open-shifts/{id}/decision"),
            owner,
            json!({ "approve": true })
        )
        .status(),
        204
    );

    let view = json_of(call!(
        app,
        get,
        format!("/staff/roster?branch_id={}&from={day}&to={day}", f.branch),
        owner
    ))
    .await;
    let shifts = view["shifts"].as_array().unwrap();
    assert_eq!(shifts.len(), 1);
    assert_eq!(shifts[0]["employee_id"], json!(f.a));
}

// ── pay ───────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn a_managers_pay_line_over_the_limit_waits_for_the_owner(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner_e = owner_employee(&pool, &f).await;
    let mgr = user(&pool, f.org, "Manager", "branch_manager", None).await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(mgr)
        .bind(f.branch)
        .execute(&pool)
        .await
        .unwrap();
    let tm = token_for(mgr, f.org, UserRole::BranchManager);
    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);

    let small = call!(
        app,
        post,
        "/staff/adjustments",
        tm,
        json!({
        "employee_id": f.a, "kind": "bonus", "amount_piastres": 50_000, "reason": "Great week" })
    );
    assert_eq!(small.status(), 201);
    assert_eq!(json_of(small).await["status"], "approved");

    let big = call!(
        app,
        post,
        "/staff/adjustments",
        tm,
        json!({
        "employee_id": f.a, "kind": "deduction", "amount_piastres": 200_000, "reason": "Broken machine", "recurring": true })
    );
    assert_eq!(big.status(), 201);
    let big = json_of(big).await;
    assert_eq!(big["status"], "pending");
    assert!(
        keys_for(&pool, owner_e)
            .await
            .contains(&"staff.n_adjustment_pending".to_string())
    );

    let id = big["id"].as_str().unwrap();
    let resp = call!(
        app,
        patch,
        format!("/staff/adjustments/deduction/{id}/decision"),
        tm,
        json!({ "approve": true })
    );
    assert_eq!(resp.status(), 403, "the manager can't approve their own");
    let resp = call!(
        app,
        patch,
        format!("/staff/adjustments/deduction/{id}/decision"),
        owner,
        json!({ "approve": true })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(json_of(resp).await["status"], "approved");

    let resp = call!(
        app,
        post,
        format!("/staff/adjustments/deduction/{id}/stop"),
        owner,
        json!({})
    );
    assert_eq!(resp.status(), 200);
    assert!(json_of(resp).await["ends_on"].is_string());
}

#[sqlx::test]
async fn paying_everyone_pays_the_period_and_locks_reopening(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    for (res, act) in [
        ("payroll", "create"),
        ("payroll", "read"),
        ("payroll", "update"),
    ] {
        sqlx::query(
            "INSERT INTO role_permissions (role, resource, action, granted) \
             VALUES ('org_admin'::user_role, $1::permission_resource, $2::permission_action, true) \
             ON CONFLICT DO NOTHING",
        )
        .bind(res)
        .bind(act)
        .execute(&pool)
        .await
        .unwrap();
    }
    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);
    let current = json_of(call!(app, get, "/staff/payroll/current", owner)).await;
    assert_eq!(current["period"]["status"], "draft");
    assert_eq!(current["preview"].as_array().unwrap().len(), 2);
    let id = current["period"]["id"].as_str().unwrap().to_string();

    let me = phone_token(&pool, f.a).await;
    let est = json_of(call!(app, get, "/staff/me/pay/estimate", me)).await;
    assert_eq!(est["slip"]["employee_id"], json!(f.a));
    assert_eq!(
        est["advance_room_piastres"], 300_000,
        "half the salary by default"
    );

    assert!(
        call!(
            app,
            post,
            format!("/staff/payroll/periods/{id}/generate"),
            owner,
            json!({})
        )
        .status()
        .is_success()
    );
    let resp = call!(
        app,
        patch,
        format!("/staff/payroll/periods/{id}/payslips/{}/paid", f.a),
        owner,
        json!({ "method": "cash" })
    );
    assert_eq!(resp.status(), 200);
    let reopen = call!(
        app,
        patch,
        format!("/staff/payroll/periods/{id}/status"),
        owner,
        json!({ "status": "draft", "reason": "a line was missing" })
    );
    assert_eq!(reopen.status(), 409, "someone is already paid");

    let resp = call!(
        app,
        patch,
        format!("/staff/payroll/periods/{id}/payslips/{}/paid", f.b),
        owner,
        json!({ "method": "bank" })
    );
    assert_eq!(resp.status(), 200);
    let status: String =
        sqlx::query_scalar("SELECT status FROM payroll_periods WHERE id = $1::uuid")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "paid");

    let inbox = json_of(call!(app, get, "/staff/me/notifications", me)).await;
    assert_eq!(inbox[0]["key"], "staff.n_paid");
    assert_eq!(
        call!(app, post, "/staff/me/notifications/read", me, json!({})).status(),
        204
    );
    let inbox = json_of(call!(app, get, "/staff/me/notifications", me)).await;
    assert!(inbox[0]["read_at"].is_string());
}

#[sqlx::test]
async fn expense_advances_are_logged_never_deducted(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);
    let resp = call!(
        app,
        post,
        "/staff/expense-advances",
        owner,
        json!({
        "employee_id": f.a, "amount_piastres": 20_000, "purpose": "Milk", "via": "till" })
    );
    // A till pay-out is tagged on the POS, never typed here (AV-10).
    assert_eq!(resp.status(), 400);
    let resp = call!(
        app,
        post,
        "/staff/expense-advances",
        owner,
        json!({
        "employee_id": f.a, "amount_piastres": 20_000, "purpose": "Milk", "via": "safe" })
    );
    assert_eq!(resp.status(), 201);
    let mine = json_of(call!(
        app,
        get,
        "/staff/me/expense-advances",
        phone_token(&pool, f.a).await
    ))
    .await;
    assert_eq!(mine.as_array().unwrap().len(), 1);
    let deductions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM payroll_deductions WHERE employee_id = $1")
            .bind(f.a)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(deductions, 0);
    let _ = NaiveDate::MIN;
}

#[sqlx::test]
async fn the_app_boots_from_one_context_call(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    shift(&pool, &f, "Morning", t(8), t(16)).await;
    let me = json_of(call!(
        app,
        get,
        "/staff/me/context",
        phone_token(&pool, f.a).await
    ))
    .await;
    assert_eq!(me["role"], "employee");
    assert_eq!(me["branches"].as_array().unwrap().len(), 1);
    assert_eq!(me["work_shifts"].as_array().unwrap().len(), 1);
    let people = me["people"].as_array().unwrap();
    let colleague = people
        .iter()
        .find(|p| p["employee_id"] == json!(f.b))
        .unwrap();
    assert!(
        colleague["base_salary_piastres"].is_null(),
        "no colleague's pay"
    );
    let myself = people
        .iter()
        .find(|p| p["employee_id"] == json!(f.a))
        .unwrap();
    assert_eq!(myself["base_salary_piastres"], 600_000);
    assert_eq!(me["settings"]["period_start_day"], 26);
}

/// The bug the simulator found: Sara works her own Morning shift, clocks
/// out, then covers Omar's Morning shift on the same date (CV-1).
#[sqlx::test]
async fn a_colleague_can_cover_the_shift_template_they_already_worked(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let now = Utc::now();
    let (start, end) = (
        (now - Duration::minutes(40)).time(),
        (now + Duration::hours(4)).time(),
    );
    if end < start {
        return; // ponytail: skip the hours around midnight UTC
    }
    let s = shift(&pool, &f, "Morning", start, end).await;
    every_day(&pool, &f, f.a, s).await;
    every_day(&pool, &f, f.b, s).await;
    let tok = phone_token(&pool, f.a).await;
    let here = json!({ "branch_id": f.branch, "latitude": LAT, "longitude": LNG });
    assert!(
        call!(app, post, "/staff/me/check-in", tok, here)
            .status()
            .is_success()
    );
    assert!(
        call!(
            app,
            post,
            "/staff/me/check-out",
            tok,
            json!({ "latitude": LAT, "longitude": LNG })
        )
        .status()
        .is_success()
    );

    let list = json_of(call!(app, get, "/staff/me/coverable", tok)).await;
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    let resp = call!(
        app,
        post,
        "/staff/me/cover",
        tok,
        json!({ "employee_id": f.b, "work_shift_id": s, "latitude": LAT, "longitude": LNG })
    );
    assert_eq!(resp.status(), 201, "{:?}", test::read_body(resp).await);
    let resp = call!(
        app,
        post,
        "/staff/me/cover",
        tok,
        json!({ "employee_id": f.b, "work_shift_id": s, "latitude": LAT, "longitude": LNG })
    );
    assert_eq!(resp.status(), 409, "one cover per shift covered");
}

/// A punch queued offline carries the last server time and time-since-boot;
/// the server rebuilds when it happened (CL-11) and a queued ping lands on the
/// record that was open at its time.
#[sqlx::test]
async fn an_offline_punch_is_dated_by_the_server_not_the_phone(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let now = Utc::now();
    let (start, end) = (
        (now - Duration::hours(1)).time(),
        (now + Duration::hours(4)).time(),
    );
    if end < start {
        return; // ponytail: skip the hours around midnight UTC
    }
    let s = shift(&pool, &f, "Morning", start, end).await;
    every_day(&pool, &f, f.a, s).await;
    let phone = common::employees::session(&pool, f.a).await;
    let tok = format!("{}|{}", phone.token, phone.device);
    let seen = now - Duration::minutes(40);
    // The last time the server spoke to this phone, signed for it (CL-11).
    let anchor = madar_rust::staff::dawam::clock::sign_anchor(&secret(), phone.device_id, seen);
    let stamp = |mins: i64, rebooted: bool| json!({ "server_time": seen, "elapsed_ms": mins * 60_000, "rebooted": rebooted, "anchor": anchor });
    let rec = json_of(call!(
        app,
        post,
        "/staff/me/check-in",
        tok,
        json!({
        "branch_id": f.branch, "latitude": LAT, "longitude": LNG, "offline": stamp(10, false) })
    ))
    .await;
    assert_eq!(rec["check_in_method"], "offline", "{rec}");
    let at: chrono::DateTime<Utc> = rec["check_in_at"].as_str().unwrap().parse().unwrap();
    assert!(
        (at - (seen + Duration::minutes(10))).num_seconds().abs() <= 1,
        "{at}"
    );

    let ping = call!(
        app,
        post,
        "/staff/me/pings",
        tok,
        json!({
        "latitude": LAT, "longitude": LNG, "accuracy_meters": 9.0, "offline": stamp(20, false) })
    );
    assert!(
        ping.status().is_success(),
        "{:?}",
        test::read_body(ping).await
    );

    // The phone rebooted before the check-out: dated, but doubted.
    let out = json_of(call!(
        app,
        post,
        "/staff/me/check-out",
        tok,
        json!({
        "latitude": LAT, "longitude": LNG, "offline": stamp(0, true) })
    ))
    .await;
    assert_eq!(out["check_out_method"], "offline");
    let flagged: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM attendance_flags WHERE employee_id = $1 AND kind = 'time_unverified')",
    )
    .bind(f.a)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(flagged);
}

/// RQ-11: the database refuses a second live request of the same kind over
/// the same time — windows, not just days, so two separate excuses on one
/// day are fine. RQ-9: one waiting correction per shift.
#[sqlx::test]
async fn overlapping_requests_are_refused_by_the_database(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let tok = phone_token(&pool, f.a).await;
    let day = (Utc::now() + Duration::days(3)).date_naive();
    let file = |body: serde_json::Value| body;
    let leave = file(
        json!({ "kind": "leave", "on_date": day, "end_date": day + Duration::days(2), "reason": "Wedding" }),
    );
    let resp = call!(app, post, "/staff/me/requests", tok, leave);
    assert_eq!(resp.status(), 201, "{:?}", test::read_body(resp).await);
    let inside = json!({ "kind": "leave", "on_date": day + Duration::days(1), "reason": "Again" });
    assert_eq!(
        call!(app, post, "/staff/me/requests", tok, inside).status(),
        409
    );

    let excuse = |from: &str, to: &str| json!({ "kind": "excuse", "on_date": day, "from_time": from, "to_time": to });
    assert_eq!(
        call!(
            app,
            post,
            "/staff/me/requests",
            tok,
            excuse("10:00", "11:00")
        )
        .status(),
        201
    );
    assert_eq!(
        call!(
            app,
            post,
            "/staff/me/requests",
            tok,
            excuse("15:00", "16:00")
        )
        .status(),
        201,
        "a separate window"
    );
    assert_eq!(
        call!(
            app,
            post,
            "/staff/me/requests",
            tok,
            excuse("10:30", "12:00")
        )
        .status(),
        409,
        "overlaps 10–11"
    );

    // Written straight to the table, the way two concurrent requests would
    // land: the constraint itself refuses the second.
    let raw = sqlx::query(
        "INSERT INTO staff_requests (org_id, employee_id, kind, on_date, status) VALUES ($1, $2, 'leave', $3, 'pending')",
    )
    .bind(f.org)
    .bind(f.a)
    .bind(day)
    .execute(&pool)
    .await;
    assert!(raw.is_err(), "the database itself refuses the overlap");
}

/// RU-1: nobody clocks in until the business has saved its rules; saving
/// them is what opens the door.
#[sqlx::test]
async fn nobody_clocks_in_before_the_rules_are_saved(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    sqlx::query("DELETE FROM attendance_settings WHERE org_id = $1")
        .bind(f.org)
        .execute(&pool)
        .await
        .unwrap();
    let now = Utc::now();
    let s = shift(
        &pool,
        &f,
        "Morning",
        (now - Duration::minutes(10)).time(),
        (now + Duration::hours(4)).time(),
    )
    .await;
    every_day(&pool, &f, f.a, s).await;
    let tok = phone_token(&pool, f.a).await;
    let here = json!({ "branch_id": f.branch, "latitude": LAT, "longitude": LNG });
    let resp = call!(app, post, "/staff/me/check-in", tok, here.clone());
    assert_eq!(resp.status(), 409);
    assert!(String::from_utf8_lossy(&test::read_body(resp).await).contains("RULES_NOT_SET"));

    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);
    // A one-field save (a limit, the gender mode) is not the rules step: the
    // door stays shut until the ladder and the absence cost are saved (RU-1).
    let partial = json_of(call!(
        app,
        put,
        "/staff/attendance/settings",
        owner,
        json!({ "absence_deduction_days": 1 })
    ))
    .await;
    assert!(partial["rules_saved_at"].is_null(), "{partial}");
    assert!(
        partial["suggested_tiers"]
            .as_array()
            .is_some_and(|t| !t.is_empty()),
        "the set-up step gets a suggested ladder to start from: {partial}"
    );
    assert_eq!(
        partial["late_deduction_tiers"],
        json!([]),
        "a suggestion is never saved by itself"
    );
    let resp = call!(app, post, "/staff/me/check-in", tok, here.clone());
    assert_eq!(resp.status(), 409);
    let saved = json_of(call!(
        app,
        put,
        "/staff/attendance/settings",
        owner,
        json!({ "absence_deduction_days": 1, "late_deduction_tiers": partial["suggested_tiers"] })
    ))
    .await;
    assert!(saved["rules_saved_at"].is_string(), "{saved}");
    assert_eq!(saved["night_start"], "22:00:00");
    assert_eq!(saved["gender_mode"], "soft");
    if (now + Duration::hours(4)).time() > (now - Duration::minutes(10)).time() {
        assert_eq!(
            call!(app, post, "/staff/me/check-in", tok, here).status(),
            201
        );
    }
}

/// RU-8, RU-9: overtime is night-rated only for the minutes inside the night
/// window, whatever shift it follows.
#[sqlx::test]
async fn night_overtime_is_the_overtime_inside_the_night_window(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    sqlx::query("UPDATE attendance_settings SET overtime_mode = 'automatic' WHERE org_id = $1")
        .bind(f.org)
        .execute(&pool)
        .await
        .unwrap();
    let evening = shift(&pool, &f, "Evening", t(14), t(22)).await;
    let morning = shift(&pool, &f, "Morning", t(8), t(16)).await;
    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);
    let period = json_of(call!(app, get, "/staff/payroll/current", owner)).await["period"].clone();
    let start: NaiveDate = period["start_date"].as_str().unwrap().parse().unwrap();
    for (day, ws, end_h) in [
        (start, evening, 22),
        (start + Duration::days(1), morning, 16),
    ] {
        let at = |h: u32, m: u32| Utc.from_utc_datetime(&day.and_hms_opt(h, m, 0).unwrap());
        sqlx::query(
            "INSERT INTO attendance_records (org_id, employee_id, branch_id, work_shift_id, business_date, status, \
                scheduled_start_at, scheduled_end_at, check_in_at, check_out_at, check_in_method, check_out_method, \
                overtime_minutes, worked_minutes) \
             VALUES ($1, $2, $3, $4, $5, 'present', $6, $7, $6, $8, 'mobile_gps', 'mobile_gps', 60, 540)",
        )
        .bind(f.org)
        .bind(f.a)
        .bind(f.branch)
        .bind(ws)
        .bind(day)
        .bind(at(end_h - 8, 0))
        .bind(at(end_h, 0))
        .bind(at(end_h + 1, 0))
        .execute(&pool)
        .await
        .unwrap();
    }
    let cur = json_of(call!(app, get, "/staff/payroll/current", owner)).await;
    let sara = cur["preview"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["employee_id"] == json!(f.a))
        .unwrap();
    assert_eq!(sara["overtime_minutes"], 120);
    // Each shift is priced on its own (AT-9): the evening's hour is all
    // night, the morning's all day; the breakdown says so per shift.
    assert_eq!(sara["breakdown"]["night_overtime_minutes"], 60, "{sara}");
    let shifts = sara["breakdown"]["overtime_shifts"].as_array().unwrap();
    let night_minutes: Vec<i64> = shifts
        .iter()
        .map(|s| s["night_minutes"].as_i64().unwrap())
        .collect();
    assert_eq!(night_minutes, vec![60, 0], "{sara}");
    let rate = |v: &Value| {
        v.as_str()
            .map_or_else(|| v.as_f64().unwrap(), |x| x.parse::<f64>().unwrap())
    };
    assert!((rate(&shifts[0]["night_multiplier"]) - 1.70).abs() < 1e-9);
    assert!((rate(&shifts[0]["day_multiplier"]) - 1.35).abs() < 1e-9);
}

// ── the roster engine (SC-12, SC-13, RU-13, PS-2) ────────────────────────────

async fn set_gender(pool: &PgPool, u: Uuid, g: &str) {
    sqlx::query("UPDATE employees SET gender = $2 WHERE id = $1")
        .bind(u)
        .bind(g)
        .execute(pool)
        .await
        .unwrap();
}

async fn suggestions_for(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    f: &F,
    ws: NaiveDate,
) -> Vec<Value> {
    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);
    let resp = call!(
        app,
        get,
        format!(
            "/staff/roster/suggestions?branch_id={}&week_start={ws}",
            f.branch
        ),
        owner
    );
    assert_eq!(resp.status(), 200);
    json_of(resp).await.as_array().unwrap().clone()
}

#[sqlx::test]
async fn suggestions_fill_the_coverage_grid_by_the_gender_mode(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    set_gender(&pool, f.a, "f").await;
    set_gender(&pool, f.b, "m").await;
    let night = shift(&pool, &f, "Evening", t(18), t(23)).await;
    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);
    let grid: Vec<Value> = (0..7)
        .map(|d| json!({ "day_of_week": d, "band_start": "18:00:00", "band_end": "23:00:00", "staff": 1 }))
        .collect();
    let resp = call!(
        app,
        put,
        "/staff/roster/coverage",
        owner,
        json!({ "branch_id": f.branch, "needs": grid })
    );
    assert_eq!(resp.status(), 204);
    let cov = json_of(call!(
        app,
        get,
        format!("/staff/roster/coverage?branch_id={}", f.branch),
        owner
    ))
    .await;
    assert_eq!(cov["source"], "grid");
    assert_eq!(cov["needs"].as_array().unwrap().len(), 7);

    let ws = week_start(Utc::now().date_naive()) + Duration::days(7);
    // Soft: the first late shift goes to the man, by the default, and says so;
    // fair spread then hands some to the woman.
    let soft = suggestions_for(&app, &f, ws).await;
    assert_eq!(soft.len(), 7, "one per evening: {soft:?}");
    assert!(soft.iter().all(|s| s["reason_key"] == "staff.sg_coverage"));
    assert_eq!(soft[0]["employee_id"], json!(f.b));
    assert_eq!(soft[0]["by_default"], true);
    assert!(soft.iter().any(|s| s["employee_id"] == json!(f.a)));
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM staff_suggestion_cache WHERE branch_id = $1)"
        )
        .bind(f.branch)
        .fetch_one(&pool)
        .await
        .unwrap(),
        "the week is kept"
    );

    // Hard: late shifts only to women who asked for them. Bassem takes six;
    // the seventh would cost his weekly rest, so it stays open.
    sqlx::query("UPDATE attendance_settings SET gender_mode = 'hard' WHERE org_id = $1")
        .bind(f.org)
        .execute(&pool)
        .await
        .unwrap();
    let hard = suggestions_for(&app, &f, ws).await;
    assert_eq!(hard.len(), 6, "{hard:?}");
    assert!(hard.iter().all(|s| s["employee_id"] == json!(f.b)));

    // She says she prefers evenings: she is back, on her own preference.
    sqlx::query("UPDATE employees SET pref_time = 'evening' WHERE id = $1")
        .bind(f.a)
        .execute(&pool)
        .await
        .unwrap();
    let pref = suggestions_for(&app, &f, ws).await;
    assert_eq!(pref[0]["employee_id"], json!(f.a));
    assert_eq!(pref[0]["by_default"], false);

    // Off: no gender weight at all.
    sqlx::query("UPDATE attendance_settings SET gender_mode = 'off' WHERE org_id = $1")
        .bind(f.org)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE employees SET pref_time = NULL WHERE id = $1")
        .bind(f.a)
        .execute(&pool)
        .await
        .unwrap();
    let off = suggestions_for(&app, &f, ws).await;
    assert!(off.iter().all(|s| s["by_default"] == false));

    // A rejection is remembered and the next best is offered instead.
    let first = off[0]["id"].as_str().unwrap().to_string();
    let resp = call!(
        app,
        post,
        "/staff/roster/suggestions/decide",
        owner,
        json!({ "branch_id": f.branch, "id": first, "accept": false })
    );
    assert_eq!(resp.status(), 204);
    let again = suggestions_for(&app, &f, ws).await;
    assert!(again.iter().all(|s| s["id"] != json!(first)));
    assert!(
        again.iter().any(|s| s["date"] == off[0]["date"]),
        "the next best is offered for that evening"
    );
    let shift_logged: Option<Uuid> = sqlx::query_scalar(
        "SELECT work_shift_id FROM staff_suggestion_events WHERE suggestion = $1",
    )
    .bind(&first)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(shift_logged, Some(night), "learning knows which shift");

    // The Wednesday job's query runs (it only picks branches when due).
    madar_rust::staff::jobs::precompute_suggestions(&pool)
        .await
        .unwrap();
}

#[sqlx::test]
async fn the_roster_warns_past_labour_limits(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let long = shift(&pool, &f, "Long", t(8), t(20)).await;
    every_day(&pool, &f, f.a, long).await;
    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);
    let ws = week_start(Utc::now().date_naive()) + Duration::days(7);
    let view = json_of(call!(
        app,
        get,
        format!(
            "/staff/roster?branch_id={}&from={ws}&to={}",
            f.branch,
            ws + Duration::days(6)
        ),
        owner
    ))
    .await;
    assert_eq!(view["limits_unconfirmed"], true);
    let kinds: std::collections::HashSet<String> = view["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| w["employee_id"] == json!(f.a))
        .map(|w| w["kind"].as_str().unwrap().to_string())
        .collect();
    for k in ["day_hours", "presence", "week_hours", "weekly_rest"] {
        assert!(kinds.contains(k), "{k} missing from {kinds:?}");
    }
    assert!(
        !view["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["employee_id"] == json!(f.b))
    );
}

#[sqlx::test]
async fn four_identical_weeks_suggest_changing_the_pattern(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let morning = shift(&pool, &f, "Morning", t(8), t(16)).await;
    let late = shift(&pool, &f, "Late", t(14), t(22)).await;
    every_day(&pool, &f, f.a, morning).await;
    let ws = week_start(Utc::now().date_naive()) + Duration::days(7);
    // The last four Mondays were all moved to Late.
    let monday = ws + Duration::days(2);
    for k in 1..=4 {
        sqlx::query(
            "INSERT INTO staff_schedule_overrides (org_id, employee_id, on_date, work_shift_id, reason) \
             VALUES ($1, $2, $3, $4, 'swap')",
        )
        .bind(f.org)
        .bind(f.a)
        .bind(monday - Duration::days(7 * k))
        .bind(late)
        .execute(&pool)
        .await
        .unwrap();
    }
    let all = suggestions_for(&app, &f, ws).await;
    let p = all
        .iter()
        .find(|s| s["reason_key"] == "staff.sg_pattern")
        .unwrap_or_else(|| panic!("no pattern suggestion in {all:?}"));
    assert_eq!(p["date"], json!(monday));
    assert_eq!(p["work_shift_id"], json!(late));
    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);
    let resp = call!(
        app,
        post,
        "/staff/roster/suggestions/decide",
        owner,
        json!({ "branch_id": f.branch, "id": p["id"], "accept": true })
    );
    assert_eq!(resp.status(), 204);
    let view = json_of(call!(
        app,
        get,
        format!(
            "/staff/roster?branch_id={}&from={ws}&to={}",
            f.branch,
            ws + Duration::days(13)
        ),
        owner
    ))
    .await;
    let shifts = view["shifts"].as_array().unwrap();
    assert_eq!(shifts.len(), 14, "one a day, no stacking: {shifts:?}");
    for s in shifts {
        let date: NaiveDate = serde_json::from_value(s["date"].clone()).unwrap();
        let want = if (date - monday).num_days() % 7 == 0 {
            late
        } else {
            morning
        };
        assert_eq!(s["work_shift_id"], json!(want), "{date}");
    }
    // The week before is untouched: its own edit (the Monday) still stands.
    let before = json_of(call!(
        app,
        get,
        format!(
            "/staff/roster?branch_id={}&from={}&to={}",
            f.branch,
            ws - Duration::days(7),
            ws - Duration::days(1)
        ),
        owner
    ))
    .await;
    assert!(before["shifts"].as_array().unwrap().iter().all(|s| {
        let edited = s["date"] == json!(monday - Duration::days(7));
        s["work_shift_id"] == json!(if edited { late } else { morning })
    }));
}

#[sqlx::test]
async fn switching_dawam_off_hides_it_and_keeps_the_records(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let me = phone_token(&pool, f.a).await;
    let ctx = json_of(call!(app, get, "/staff/me/context", me)).await;
    assert_eq!(ctx["modules"], json!(["pos", "dawam"]));

    sqlx::query("UPDATE organizations SET modules = '{pos}' WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await
        .unwrap();
    let resp = call!(app, get, "/staff/me/context", me);
    assert_eq!(resp.status(), 403);
    assert_eq!(json_of(resp).await["code"], "DAWAM_OFF");
    let req = test::TestRequest::post()
        .uri("/auth/staff/otp/request")
        .set_json(json!({ "phone": "+201012345678" }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 404);

    // Nothing was deleted: switching it back on restores the app.
    sqlx::query("UPDATE organizations SET modules = '{pos,dawam}' WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(call!(app, get, "/staff/me/context", me).status(), 200);

    // Never an empty set.
    assert!(
        sqlx::query("UPDATE organizations SET modules = '{}' WHERE id = $1")
            .bind(f.org)
            .execute(&pool)
            .await
            .is_err()
    );
}

#[sqlx::test]
async fn the_fairness_audit_is_the_owners(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    set_gender(&pool, f.a, "f").await;
    set_gender(&pool, f.b, "m").await;
    let today = Utc::now().date_naive();
    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);
    let body = json_of(call!(
        app,
        get,
        format!("/staff/roster/fairness?month={today}"),
        owner
    ))
    .await;
    assert_eq!(body["rows"].as_array().unwrap().len(), 2, "{body}");
    assert_eq!(body["learning_frozen"], false);
    let me = phone_token(&pool, f.a).await;
    assert_eq!(
        call!(
            app,
            get,
            format!("/staff/roster/fairness?month={today}"),
            me
        )
        .status(),
        403
    );
}

#[sqlx::test]
async fn a_low_battery_warns_once_and_silence_reads_phone_died(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner_e = owner_employee(&pool, &f).await;
    let now = Utc::now().time();
    let start = (Utc::now() - Duration::minutes(30)).time();
    let end = (Utc::now() + Duration::hours(4)).time();
    if end < now || start > now {
        return; // ponytail: skip the few minutes around midnight UTC
    }
    let s = shift(&pool, &f, "Day", start, end).await;
    every_day(&pool, &f, f.a, s).await;
    let tok = phone_token(&pool, f.a).await;
    let resp = call!(
        app,
        post,
        "/staff/me/check-in",
        tok,
        json!({ "branch_id": f.branch, "latitude": LAT, "longitude": LNG })
    );
    assert!(resp.status().is_success());
    for (i, battery) in [40, 14, 12].into_iter().enumerate() {
        let body = json_of(call!(
            app,
            post,
            "/staff/me/pings",
            tok,
            json!({ "latitude": LAT + 0.0001 * i as f64, "longitude": LNG,
                    "accuracy_meters": 8.0 + i as f64, "battery_percent": battery })
        ))
        .await;
        assert_eq!(body["charge_phone"], json!(battery <= 15));
    }
    let warned = keys_for(&pool, f.a)
        .await
        .into_iter()
        .filter(|k| k == "staff.n_charge_phone")
        .count();
    assert_eq!(warned, 1, "told once per shift");

    // Then nothing for 50 minutes.
    sqlx::query(
        "UPDATE attendance_pings SET at = at - INTERVAL '50 minutes' WHERE employee_id = $1",
    )
    .bind(f.a)
    .execute(&pool)
    .await
    .unwrap();
    madar_rust::staff::jobs::phones_that_died(&pool)
        .await
        .unwrap();
    madar_rust::staff::jobs::phones_that_died(&pool)
        .await
        .unwrap();
    let kinds: Vec<String> =
        sqlx::query_scalar("SELECT kind FROM attendance_flags WHERE employee_id = $1")
            .bind(f.a)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(kinds, ["phone_died"], "never left_mid_shift, and only once");
    assert_eq!(
        keys_for(&pool, owner_e)
            .await
            .iter()
            .filter(|k| *k == "staff.n_flag_phone_died")
            .count(),
        1
    );
}

#[sqlx::test]
async fn a_forgotten_phone_punches_on_the_till_with_a_pin(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let hash = bcrypt::hash("4321", 4).unwrap();
    // The PIN is her till account's; the punch is for the employee linked to it.
    sqlx::query("UPDATE users SET pin_hash = $2 WHERE id = $1")
        .bind(f.a_user)
        .bind(&hash)
        .execute(&pool)
        .await
        .unwrap();
    // The till is signed in as whoever works it; here the owner, on the
    // branch's POS device with its till open.
    let device = common::employees::open_till(&pool, f.org, f.branch, f.owner).await;
    let till = common::employees::at_till(&token_for(f.owner, f.org, UserRole::OrgAdmin), device);
    let punch = |pin: &str| json!({ "branch_id": f.branch, "pin": pin });

    let resp = call!(
        app,
        post,
        "/staff/attendance/till-punch",
        till,
        punch("0000")
    );
    assert_eq!(resp.status(), 401);
    let body = json_of(call!(
        app,
        post,
        "/staff/attendance/till-punch",
        till,
        punch("4321")
    ))
    .await;
    assert_eq!(body["employee_id"], json!(f.a));
    assert_eq!(body["punched"], "in");
    assert_eq!(body["record"]["check_in_method"], "till");
    let body = json_of(call!(
        app,
        post,
        "/staff/attendance/till-punch",
        till,
        punch("4321")
    ))
    .await;
    assert_eq!(body["punched"], "out");
    assert_eq!(body["record"]["check_out_method"], "till");

    // Dawam-only orgs have no till.
    sqlx::query("UPDATE organizations SET modules = '{dawam}' WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await
        .unwrap();
    let resp = call!(
        app,
        post,
        "/staff/attendance/till-punch",
        till,
        punch("4321")
    );
    assert_eq!(resp.status(), 403);
}

#[sqlx::test]
async fn a_till_pay_out_can_be_an_expense_advance(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .configure(madar_rust::tills::routes::configure)
            .configure(madar_rust::staff::routes::configure),
    )
    .await;
    let f = seed(&pool).await;
    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);
    let resp = call!(
        app,
        post,
        format!("/tills/branches/{}/open", f.branch),
        owner,
        json!({ "id": Uuid::new_v4(), "opening_cash": 100000 })
    );
    assert!(resp.status().is_success(), "{}", resp.status());
    let till = json_of(resp).await["id"].as_str().unwrap().to_string();

    let resp = call!(
        app,
        post,
        format!("/tills/{till}/cash-movements"),
        owner,
        json!({ "amount": 5000, "kind": "pay_in", "note": "float", "expense_advance_to": f.a })
    );
    assert_eq!(resp.status(), 400, "only a pay-out");
    let resp = call!(
        app,
        post,
        format!("/tills/{till}/cash-movements"),
        owner,
        json!({ "amount": -20000, "kind": "pay_out", "note": "Milk and cups", "expense_advance_to": f.a })
    );
    assert_eq!(resp.status(), 201);
    let movement = json_of(resp).await["id"].as_str().unwrap().to_string();
    let logged: (i64, String, String) = sqlx::query_as(
        "SELECT amount_piastres, via, purpose FROM expense_advances WHERE employee_id = $1",
    )
    .bind(f.a)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(logged, (20000, "till".into(), "Milk and cups".into()));

    let resp = call!(
        app,
        post,
        format!("/tills/{till}/cash-movements"),
        owner,
        json!({ "amount": 20000, "kind": "correction", "corrects_id": movement, "note": "wrong till" })
    );
    assert_eq!(resp.status(), 201);
    let left: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM expense_advances WHERE employee_id = $1")
            .bind(f.a)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(left, 0, "undoing the pay-out undoes the advance");

    // The till's picker: names of the branch's staff, for anyone who works there.
    let teller = token_for(f.a_user, f.org, UserRole::Teller);
    let people = json_of(call!(
        app,
        get,
        format!("/staff/branches/{}/people", f.branch),
        teller
    ))
    .await;
    let names: Vec<&str> = people
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Amal", "Bassem"]);
}

#[sqlx::test]
async fn an_employee_is_added_with_a_whatsapp_number_and_no_pin(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);
    let body = json!({ "name": "Sara", "phone": "01098765432", "branch_id": f.branch,
                       "base_salary_piastres": 700000, "gender": "f" });
    let resp = call!(app, post, "/staff/employees", owner, body);
    assert_eq!(resp.status(), 201);
    let e = json_of(resp).await;
    assert_eq!(e["base_salary_piastres"], 700000);
    assert_eq!(e["kind"], "app");
    assert_eq!(e["phone"], "+201098765432");
    assert!(e["user_id"].is_null(), "no Madar account is made: {e}");
    // No login, no till PIN, no POS cashier: an employee is not a user.
    let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE name = 'Sara'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(users, 0);
    // Same number again: refused.
    let resp = call!(app, post, "/staff/employees", owner, body);
    assert_eq!(resp.status(), 409);
    // She can ask for a sign-in code at once.
    let req = test::TestRequest::post()
        .uri("/auth/staff/otp/request")
        .set_json(json!({ "phone": "+201098765432" }))
        .to_request();
    assert!(test::call_service(&app, req).await.status().is_success());
    // A teller can't add people.
    let me = phone_token(&pool, f.a).await;
    let resp = call!(
        app,
        post,
        "/staff/employees",
        me,
        json!({ "name": "X", "phone": "01011111111", "branch_id": f.branch })
    );
    assert_eq!(resp.status(), 403);
}

#[sqlx::test]
async fn the_reports_read_the_clock_the_till_and_the_payslips(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = token_for(f.owner, f.org, UserRole::OrgAdmin);
    let today = Utc::now().date_naive();
    // Amal worked 8 hours today at 6000 EGP a month over 30 days: 200 EGP.
    sqlx::query(
        "INSERT INTO attendance_records (org_id, employee_id, branch_id, business_date, status, \
            scheduled_start_at, scheduled_end_at, check_in_at, check_out_at, worked_minutes) \
         VALUES ($1, $2, $3, $4, 'present', now() - INTERVAL '8 hours', now(), \
                 now() - INTERVAL '8 hours', now(), 480)",
    )
    .bind(f.org)
    .bind(f.a)
    .bind(f.branch)
    .bind(today)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO expense_advances (org_id, employee_id, amount_piastres, purpose, given_on) \
         VALUES ($1, $2, 15000, 'Cups', $3)",
    )
    .bind(f.org)
    .bind(f.a)
    .bind(today)
    .execute(&pool)
    .await
    .unwrap();
    let q = format!("from={}&to={today}", today - Duration::days(7));
    let labour = json_of(call!(
        app,
        get,
        format!("/staff/reports/labour-vs-sales?{q}"),
        owner
    ))
    .await;
    let row = labour
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["date"] == json!(today))
        .unwrap_or_else(|| panic!("{labour}"));
    assert_eq!(row["labour_piastres"], 20000);
    assert_eq!(row["sales_piastres"], 0);
    assert_eq!(row["labour_share_bp"], Value::Null);

    let adv = json_of(call!(
        app,
        get,
        format!("/staff/reports/advances?{q}"),
        owner
    ))
    .await;
    assert_eq!(adv["expense_given_piastres"], 15000);
    let hist = call!(
        app,
        get,
        format!("/staff/reports/payroll-history?{q}"),
        owner
    );
    assert_eq!(hist.status(), 200);

    // Dawam-only: no sales to compare with (DSH-4).
    sqlx::query("UPDATE organizations SET modules = '{dawam}' WHERE id = $1")
        .bind(f.org)
        .execute(&pool)
        .await
        .unwrap();
    let resp = call!(
        app,
        get,
        format!("/staff/reports/labour-vs-sales?{q}"),
        owner
    );
    assert_eq!(resp.status(), 403);
    let me = phone_token(&pool, f.a).await;
    assert_eq!(
        call!(app, get, format!("/staff/reports/advances?{q}"), me).status(),
        403
    );
}
