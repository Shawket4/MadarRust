//! Dawam Phase A: who an employee is, and who a staff-app session is.
//!
//! - The three kinds of employee end to end — linked to a Madar user, an app
//!   user with no account, and a record with no sign-in at all: added, signed
//!   in, punched, paid.
//! - Making an existing owner or cashier an employee (and who may).
//! - The staff token: accepted only on `/staff/*`, short-lived and refreshed
//!   through the device, refused once the device is revoked (every route
//!   family), once the employee is inactive, the org suspended or Dawam off.
//! - Revocation on a new phone, a new number, a deactivated or deleted account.
//! - The nightly sweep skips suspended and Dawam-off businesses.
//! - Modules: read by any member, switched by a super admin only.

use actix_web::{App, http::Method, test, web};
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::models::UserRole;

mod common;
use common::employees::{Session, authed, employee, secret, session, set_modules, user_token};

const LAT: f64 = 29.9792;
const LNG: f64 = 31.1342;

/// The whole API, as `main.rs` mounts it: a staff token must be refused by
/// every route that is not `/staff/*`.
macro_rules! app {
    ($pool:expr) => {{
        unsafe {
            std::env::set_var("MADAR_DISABLE_RATE_LIMIT", "1");
            std::env::set_var("MADAR_DISABLE_AUTO_TRANSLATION", "1");
        }
        let read_pool = web::Data::new($pool.clone());
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(
                    madar_rust::menu::cache::MenuCache::from_env(),
                ))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(
                    madar_rust::auth::org_status::OrgStatusCache::new(),
                ))
                .app_data(web::Data::new(
                    madar_rust::realtime::hub::BranchEventHub::new(),
                ))
                .app_data(madar_rust::qr_card::routes::make_provider())
                .app_data(web::Data::new(
                    madar_rust::demo::config::DemoConfig::from_env(),
                ))
                .app_data(web::Data::new(madar_rust::ai::AiState::from_env()))
                .configure(|cfg| madar_rust::app_routes::configure_api(cfg, read_pool.clone())),
        )
        .await
    }};
}

/// `$token`: a user's JWT, or a phone's `token|device`.
macro_rules! call {
    ($app:expr, $method:expr, $uri:expr, $token:expr) => {{
        let req = authed(
            test::TestRequest::default()
                .method(Method::from_bytes($method.as_bytes()).unwrap())
                .uri(&$uri),
            &$token,
        )
        .to_request();
        test::call_service(&$app, req).await
    }};
    ($app:expr, $method:expr, $uri:expr, $token:expr, $body:expr) => {{
        let req = authed(
            test::TestRequest::default()
                .method(Method::from_bytes($method.as_bytes()).unwrap())
                .uri(&$uri),
            &$token,
        )
        .set_json(&$body)
        .to_request();
        test::call_service(&$app, req).await
    }};
}

async fn body(resp: actix_web::dev::ServiceResponse) -> Value {
    let bytes = test::read_body(resp).await;
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

fn phone(s: &Session) -> String {
    format!("{}|{}", s.token, s.device)
}

struct Org {
    org: Uuid,
    /// Branch A (the manager's) and branch B.
    a: Uuid,
    b: Uuid,
    owner: Uuid,
    manager: Uuid,
}

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

async fn assign(pool: &PgPool, user: Uuid, branch: Uuid) {
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(user)
        .bind(branch)
        .execute(pool)
        .await
        .unwrap();
}

async fn seed(pool: &PgPool) -> Org {
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    madar_rust::authz::sync_catalogue(pool).await.unwrap();
    let org = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, modules) VALUES ($1, 'Cafe', $2, '{pos,dawam}')",
    )
    .bind(org)
    .bind(format!("org-{org}"))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO attendance_settings (org_id, rules_saved_at) VALUES ($1, now())")
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    let mut branches = Vec::new();
    for name in ["A", "B"] {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO branches (id, org_id, name, timezone, latitude, longitude, geo_radius_meters) \
             VALUES ($1, $2, $3, 'UTC'::timezone_name, $4, $5, 200)",
        )
        .bind(id)
        .bind(org)
        .bind(name)
        .bind(LAT)
        .bind(LNG)
        .execute(pool)
        .await
        .unwrap();
        branches.push(id);
    }
    let owner = user(pool, org, "Owner", "org_admin", Some("+201000000001")).await;
    let manager = user(pool, org, "Manager", "branch_manager", None).await;
    assign(pool, manager, branches[0]).await;
    Org {
        org,
        a: branches[0],
        b: branches[1],
        owner,
        manager,
    }
}

fn owner_t(o: &Org) -> String {
    user_token(o.owner, o.org, UserRole::OrgAdmin)
}

fn manager_t(o: &Org) -> String {
    user_token(o.manager, o.org, UserRole::BranchManager)
}

/// The WhatsApp sign-in, for real: request a code, read it, verify it.
async fn sign_in<S>(app: &S, pool: &PgPool, number: &str) -> Value
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    sqlx::query("DELETE FROM staff_otp")
        .execute(pool)
        .await
        .unwrap();
    let req = test::TestRequest::post()
        .uri("/auth/staff/otp/request")
        .set_json(json!({ "phone": number }))
        .to_request();
    let resp = test::call_service(app, req).await;
    assert_eq!(resp.status(), 200, "{number} asks for a code");
    let code: String = sqlx::query_scalar("SELECT code FROM staff_otp")
        .fetch_one(pool)
        .await
        .unwrap();
    let req = test::TestRequest::post()
        .uri("/auth/staff/otp/verify")
        .set_json(json!({ "phone": number, "code": code, "model": "Test" }))
        .to_request();
    let resp = test::call_service(app, req).await;
    assert_eq!(resp.status(), 200);
    body(resp).await
}

fn session_of(v: &Value) -> String {
    format!(
        "{}|{}",
        v["token"].as_str().unwrap(),
        v["device_token"].as_str().unwrap()
    )
}

async fn otp_status<S>(app: &S, number: &str) -> u16
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let req = test::TestRequest::post()
        .uri("/auth/staff/otp/request")
        .set_json(json!({ "phone": number }))
        .to_request();
    test::call_service(app, req).await.status().as_u16()
}

/// A pay period over the last month, approved (generated) by the owner.
async fn run_payroll<S>(app: &S, o: &Org) -> String
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let today = Utc::now().date_naive();
    let resp = call!(
        app,
        "POST",
        "/staff/payroll/periods",
        owner_t(o),
        json!({ "name": "This month", "start_date": today - Duration::days(20), "end_date": today + Duration::days(5) })
    );
    assert_eq!(resp.status(), 201);
    let id = body(resp).await["id"].as_str().unwrap().to_string();
    let resp = call!(
        app,
        "POST",
        format!("/staff/payroll/periods/{id}/generate"),
        owner_t(o)
    );
    assert_eq!(resp.status(), 200);
    id
}

// ── the three kinds, end to end ────────────────────────────────────────────

/// Kind (b): payroll and attendance only — no Madar account, no phone sign-in.
#[sqlx::test]
async fn a_manual_employee_is_on_payroll_without_any_sign_in(pool: PgPool) {
    let app = app!(pool);
    let o = seed(&pool).await;
    let users_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&pool)
        .await
        .unwrap();
    let resp = call!(
        app,
        "POST",
        "/staff/employees",
        owner_t(&o),
        json!({ "name": "Hoda", "phone": "01055555555", "app_access": false,
                "branch_ids": [o.a], "base_salary_piastres": 450_000 })
    );
    assert_eq!(resp.status(), 201);
    let e = body(resp).await;
    assert_eq!(e["kind"], "manual");
    assert_eq!(e["app_access"], false);
    assert!(e["user_id"].is_null());
    assert_eq!(e["branch_ids"], json!([o.a]));
    let id = e["id"].as_str().unwrap().to_string();
    let users_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(users_before, users_after, "no login, no teller");

    // No app: her number is not registered for a code.
    assert_eq!(otp_status(&app, "01055555555").await, 404);

    // Her manager punches for her (a forgotten phone and no app alike).
    let resp = call!(
        app,
        "POST",
        "/staff/attendance/punch",
        manager_t(&o),
        json!({ "employee_id": id, "reason": "No phone" })
    );
    assert_eq!(resp.status(), 200);
    let rec = body(resp).await;
    assert_eq!(rec["employee_id"], json!(id));
    assert_eq!(rec["check_in_method"], "manual");

    let period = run_payroll(&app, &o).await;
    let slips = body(call!(
        app,
        "GET",
        format!("/staff/payroll/periods/{period}/payslips"),
        owner_t(&o)
    ))
    .await;
    let mine = slips
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["employee_id"] == json!(id))
        .unwrap_or_else(|| panic!("{slips}"));
    assert_eq!(mine["employee_name"], "Hoda");
    assert!(mine["net_piastres"].as_i64().unwrap() > 0, "{mine}");
}

/// Kind (c): signs in with a WhatsApp code, with no Madar account at all.
#[sqlx::test]
async fn an_app_employee_signs_in_punches_and_reads_their_payslip(pool: PgPool) {
    let app = app!(pool);
    let o = seed(&pool).await;
    let resp = call!(
        app,
        "POST",
        "/staff/employees",
        manager_t(&o),
        json!({ "name": "Mona", "phone": "01066666666", "branch_ids": [o.a] })
    );
    assert_eq!(
        resp.status(),
        201,
        "a branch manager adds people at their branch"
    );
    let e = body(resp).await;
    assert_eq!(e["kind"], "app");
    assert_eq!(e["phone"], "+201066666666");
    let id = e["id"].as_str().unwrap().to_string();
    sqlx::query("UPDATE employees SET base_salary_piastres = 300000 WHERE id = $1::uuid")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();

    let s = sign_in(&app, &pool, "01066666666").await;
    assert_eq!(s["employee_id"], json!(id));
    assert!(s["user_id"].is_null() && s["role"].is_null(), "{s}");
    assert!(s["token_expires_at"].is_string());
    let me = session_of(&s);

    let ctx = body(call!(app, "GET", "/staff/me/context", me)).await;
    assert_eq!(ctx["employee_id"], json!(id));
    assert_eq!(ctx["role"], "employee");
    assert_eq!(ctx["caps"], json!([]));

    let resp = call!(
        app,
        "POST",
        "/staff/me/check-in",
        me,
        json!({ "branch_id": o.a, "latitude": LAT, "longitude": LNG })
    );
    assert_eq!(resp.status(), 201);
    let resp = call!(
        app,
        "POST",
        "/staff/me/check-out",
        me,
        json!({ "latitude": LAT, "longitude": LNG })
    );
    assert_eq!(resp.status(), 200);
    // At a branch she does not work at: refused.
    let resp = call!(
        app,
        "POST",
        "/staff/me/check-in",
        me,
        json!({ "branch_id": o.b, "latitude": LAT, "longitude": LNG })
    );
    assert_eq!(resp.status(), 403);

    // No Madar account: no manager act, and a 403 — never a 401 that would
    // sign the app out.
    let resp = call!(app, "GET", "/staff/employees", me);
    assert_eq!(resp.status(), 403);
    assert_eq!(body(resp).await["code"], "MANAGER_ACCOUNT_NEEDED");

    run_payroll(&app, &o).await;
    let slips = body(call!(app, "GET", "/staff/me/payslips", me)).await;
    assert_eq!(slips.as_array().unwrap().len(), 1, "{slips}");
    assert_eq!(slips[0]["employee_id"], json!(id));
}

/// Kind (a): an existing cashier is made an employee; their till account is
/// untouched and gains the app.
#[sqlx::test]
async fn a_cashier_made_an_employee_keeps_the_till_and_gains_the_app(pool: PgPool) {
    let app = app!(pool);
    let o = seed(&pool).await;
    let teller = user(&pool, o.org, "Tarek", "teller", Some("01077777777")).await;
    assign(&pool, teller, o.a).await;
    let pin = bcrypt::hash("2468", 4).unwrap();
    sqlx::query("UPDATE users SET pin_hash = $2 WHERE id = $1")
        .bind(teller)
        .bind(&pin)
        .execute(&pool)
        .await
        .unwrap();

    // He is a user, not yet an employee: he is offered, and not on payroll.
    let linkable = body(call!(app, "GET", "/staff/employees/linkable", owner_t(&o))).await;
    assert!(
        linkable
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["user_id"] == json!(teller) && u["role"] == "teller"),
        "{linkable}"
    );
    let resp = call!(
        app,
        "POST",
        "/staff/employees",
        owner_t(&o),
        json!({ "user_id": teller, "branch_ids": [o.a], "base_salary_piastres": 500_000 })
    );
    assert_eq!(resp.status(), 201);
    let e = body(resp).await;
    assert_eq!(e["kind"], "linked");
    assert_eq!(e["user_id"], json!(teller));
    assert_eq!(e["role"], "teller");
    assert_eq!(e["name"], "Tarek", "his account's name");
    assert_eq!(e["phone"], "+201077777777", "and number");
    let id = e["id"].as_str().unwrap().to_string();
    assert_ne!(id, teller.to_string(), "the employee is its own record");
    let role: String = sqlx::query_scalar("SELECT role::text FROM users WHERE id = $1")
        .bind(teller)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(role, "teller", "his POS role is untouched");
    // Once only.
    let resp = call!(
        app,
        "POST",
        "/staff/employees",
        owner_t(&o),
        json!({ "user_id": teller, "branch_ids": [o.a] })
    );
    assert_eq!(resp.status(), 409);

    // The app: his linked account is a cashier, so no manager tabs.
    let s = sign_in(&app, &pool, "01077777777").await;
    assert_eq!(s["employee_id"], json!(id));
    assert_eq!(s["user_id"], json!(teller));
    let ctx = body(call!(app, "GET", "/staff/me/context", session_of(&s))).await;
    assert_eq!(ctx["role"], "employee");

    // The till: his PIN punches the employee he is.
    let till = user_token(o.owner, o.org, UserRole::OrgAdmin);
    let resp = call!(
        app,
        "POST",
        "/staff/attendance/till-punch",
        till,
        json!({ "branch_id": o.a, "pin": "2468" })
    );
    assert_eq!(resp.status(), 200);
    let r = body(resp).await;
    assert_eq!(r["employee_id"], json!(id));
    assert_eq!(r["record"]["check_in_method"], "till");

    run_payroll(&app, &o).await;
    let slips = body(call!(app, "GET", "/staff/me/payslips", session_of(&s))).await;
    assert_eq!(slips[0]["employee_id"], json!(id));
}

/// A till user who is not an employee punches nothing.
#[sqlx::test]
async fn a_till_pin_of_someone_who_is_not_an_employee_is_refused(pool: PgPool) {
    let app = app!(pool);
    let o = seed(&pool).await;
    let teller = user(&pool, o.org, "Nour", "teller", None).await;
    assign(&pool, teller, o.a).await;
    sqlx::query("UPDATE users SET pin_hash = $2 WHERE id = $1")
        .bind(teller)
        .bind(bcrypt::hash("1357", 4).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let resp = call!(
        app,
        "POST",
        "/staff/attendance/till-punch",
        owner_t(&o),
        json!({ "branch_id": o.a, "pin": "1357" })
    );
    assert_eq!(resp.status(), 403);
    assert_eq!(body(resp).await["code"], "NOT_AN_EMPLOYEE");
}

/// The owner made an employee manages from the app — within Dawam only: the
/// WhatsApp code never becomes a dashboard or till session.
#[sqlx::test]
async fn the_owner_made_an_employee_manages_from_the_app_and_nowhere_else(pool: PgPool) {
    let app = app!(pool);
    let o = seed(&pool).await;
    let resp = call!(
        app,
        "POST",
        "/staff/employees",
        owner_t(&o),
        json!({ "user_id": o.owner, "branch_ids": [o.a, o.b] })
    );
    assert_eq!(
        resp.status(),
        201,
        "the owner may make themselves an employee"
    );
    let s = sign_in(&app, &pool, "01000000001").await;
    let me = session_of(&s);
    let ctx = body(call!(app, "GET", "/staff/me/context", me)).await;
    assert_eq!(ctx["role"], "owner");
    assert!(
        ctx["caps"]
            .as_array()
            .unwrap()
            .contains(&json!("hr.payroll.run")),
        "{ctx}"
    );
    assert_eq!(ctx["branches"].as_array().unwrap().len(), 2);
    // Manager acts on /staff go through the linked account.
    assert_eq!(call!(app, "GET", "/staff/employees", me).status(), 200);
    assert_eq!(
        call!(app, "GET", "/staff/payroll/current", me).status(),
        200
    );

    // Everywhere else the staff token is not a session at all.
    let token = s["token"].as_str().unwrap().to_string();
    for (method, uri) in [
        ("GET", "/auth/me".to_string()),
        ("GET", "/auth/permissions".to_string()),
        ("GET", "/users".to_string()),
        ("GET", format!("/orgs/{}", o.org)),
        ("GET", format!("/orgs/{}/modules", o.org)),
        ("GET", format!("/orgs/{}/offline-auth-bundle", o.org)),
        ("GET", "/authz/me".to_string()),
        ("GET", format!("/branches/{}", o.a)),
        ("POST", format!("/tills/branches/{}/open", o.a)),
        ("GET", "/menu-items".to_string()),
        ("PUT", "/push/token".to_string()),
    ] {
        let resp = call!(app, method, uri, token, json!({}));
        assert_eq!(
            resp.status(),
            401,
            "a staff token on {method} {uri} must not be a session"
        );
        // With the device header too.
        let resp = call!(app, method, uri, me, json!({}));
        assert_eq!(resp.status(), 401, "{method} {uri} with the device");
    }
}

/// Anti-escalation: a manager can't hand the owner's sign-in to a number, nor
/// add people at a branch that isn't theirs.
#[sqlx::test]
async fn a_branch_manager_links_a_cashier_but_never_the_owner(pool: PgPool) {
    let app = app!(pool);
    let o = seed(&pool).await;
    let resp = call!(
        app,
        "POST",
        "/staff/employees",
        manager_t(&o),
        json!({ "user_id": o.owner, "phone": "01099990000", "branch_ids": [o.a] })
    );
    assert_eq!(resp.status(), 403, "the owner is not the manager's to link");
    let teller = user(&pool, o.org, "Sami", "teller", None).await;
    assign(&pool, teller, o.a).await;
    let resp = call!(
        app,
        "POST",
        "/staff/employees",
        manager_t(&o),
        json!({ "user_id": teller, "branch_ids": [o.a] })
    );
    assert_eq!(resp.status(), 201);
    let resp = call!(
        app,
        "POST",
        "/staff/employees",
        manager_t(&o),
        json!({ "name": "Elsewhere", "branch_ids": [o.b] })
    );
    assert_eq!(resp.status(), 403, "branch B is not theirs");
}

#[sqlx::test]
async fn two_people_may_share_a_name_but_not_an_app_number(pool: PgPool) {
    let app = app!(pool);
    let o = seed(&pool).await;
    for n in ["01011112222", "01011113333"] {
        let resp = call!(
            app,
            "POST",
            "/staff/employees",
            owner_t(&o),
            json!({ "name": "Mohamed Ahmed", "phone": n, "branch_ids": [o.a] })
        );
        assert_eq!(resp.status(), 201, "a common name is not a conflict");
    }
    let resp = call!(
        app,
        "POST",
        "/staff/employees",
        owner_t(&o),
        json!({ "name": "Other", "phone": "+201011112222", "branch_ids": [o.a] })
    );
    assert_eq!(
        resp.status(),
        409,
        "one app number per person in a business"
    );
    // Without the app, a shared number is only a contact.
    let resp = call!(
        app,
        "POST",
        "/staff/employees",
        owner_t(&o),
        json!({ "name": "Other", "phone": "01011112222", "app_access": false, "branch_ids": [o.a] })
    );
    assert_eq!(resp.status(), 201);
}

// ── the session ────────────────────────────────────────────────────────────

/// The staff routes a phone uses, one per family.
fn families(employee: &str, branch: Uuid) -> Vec<(&'static str, String, Value)> {
    let today = Utc::now().date_naive();
    vec![
        ("GET", "/staff/me/context".into(), Value::Null),
        ("GET", "/staff/me/today".into(), Value::Null),
        (
            "POST",
            "/staff/me/check-in".into(),
            json!({ "branch_id": branch, "latitude": LAT, "longitude": LNG }),
        ),
        (
            "POST",
            "/staff/me/check-out".into(),
            json!({ "latitude": LAT, "longitude": LNG }),
        ),
        (
            "POST",
            "/staff/me/pings".into(),
            json!({ "latitude": LAT, "longitude": LNG }),
        ),
        ("GET", "/staff/me/coverable".into(), Value::Null),
        (
            "GET",
            format!("/staff/me/attendance?from={today}&to={today}"),
            Value::Null,
        ),
        (
            "GET",
            format!("/staff/me/schedule?from={today}&to={today}"),
            Value::Null,
        ),
        (
            "GET",
            format!("/staff/me/roster?from={today}&to={today}"),
            Value::Null,
        ),
        ("GET", "/staff/me/requests".into(), Value::Null),
        (
            "POST",
            "/staff/me/requests".into(),
            json!({ "kind": "leave", "on_date": today + Duration::days(9) }),
        ),
        ("GET", "/staff/me/leave-balances".into(), Value::Null),
        ("GET", "/staff/me/advances".into(), Value::Null),
        (
            "POST",
            "/staff/me/advances".into(),
            json!({ "amount_piastres": 1000 }),
        ),
        ("GET", "/staff/me/payslips".into(), Value::Null),
        ("GET", "/staff/me/pay/estimate".into(), Value::Null),
        ("GET", "/staff/me/adjustments".into(), Value::Null),
        ("GET", "/staff/me/expense-advances".into(), Value::Null),
        ("GET", "/staff/me/notifications".into(), Value::Null),
        ("POST", "/staff/me/notifications/read".into(), json!({})),
        (
            "PUT",
            "/staff/me/preferences".into(),
            json!({ "pref_time": "morning" }),
        ),
        (
            "PUT",
            "/staff/me/push-token".into(),
            json!({ "token": "fcm" }),
        ),
        (
            "POST",
            "/staff/me/swaps".into(),
            json!({ "my_date": today, "my_shift_id": Uuid::nil(), "peer_id": Uuid::nil(),
                    "peer_date": today, "peer_shift_id": Uuid::nil() }),
        ),
        (
            "POST",
            format!("/staff/open-shifts/{}/claim", Uuid::nil()),
            json!({}),
        ),
        // Management families, through a linked manager's account.
        ("GET", "/staff/employees".into(), Value::Null),
        ("GET", format!("/staff/employees/{employee}"), Value::Null),
        ("GET", "/staff/flags".into(), Value::Null),
        ("GET", "/staff/requests".into(), Value::Null),
        (
            "GET",
            format!("/staff/attendance?from={today}&to={today}"),
            Value::Null,
        ),
        ("GET", "/staff/team/presence".into(), Value::Null),
        (
            "GET",
            format!("/staff/roster?branch_id={branch}&from={today}&to={today}"),
            Value::Null,
        ),
        ("GET", "/staff/adjustments".into(), Value::Null),
        ("GET", "/staff/payroll/advances".into(), Value::Null),
        ("GET", "/staff/expense-advances".into(), Value::Null),
        ("GET", "/staff/swaps".into(), Value::Null),
        ("GET", "/staff/work-shifts".into(), Value::Null),
        ("GET", "/staff/attendance/settings".into(), Value::Null),
        ("GET", "/staff/payroll/current".into(), Value::Null),
    ]
}

/// RO-4: a revoked phone is refused on every staff route family, and cannot
/// refresh its session.
#[sqlx::test]
async fn a_revoked_phone_is_refused_on_every_staff_route_family(pool: PgPool) {
    let app = app!(pool);
    let o = seed(&pool).await;
    // A manager on payroll: management routes are reachable from the phone.
    let m = employee(
        &pool,
        o.org,
        "Manager",
        Some(o.manager),
        Some("+201012121212"),
        true,
        &[o.a],
        100,
    )
    .await;
    let s = session(&pool, m).await;
    let me = phone(&s);
    // Live: nothing is a 401 (whatever each route answers on its own).
    for (method, uri, b) in families(&m.to_string(), o.a) {
        let resp = if b.is_null() {
            call!(app, method, uri, me)
        } else {
            call!(app, method, uri, me, b)
        };
        assert_ne!(resp.status(), 401, "{method} {uri} while live");
    }
    let resp = call!(
        app,
        "DELETE",
        format!("/staff/employees/{m}/device"),
        owner_t(&o)
    );
    assert_eq!(resp.status(), 204);
    for (method, uri, b) in families(&m.to_string(), o.a) {
        let resp = if b.is_null() {
            call!(app, method, uri, me)
        } else {
            call!(app, method, uri, me, b)
        };
        assert_eq!(resp.status(), 401, "{method} {uri} after the revoke");
        assert_eq!(body(resp).await["code"], "DEVICE_REVOKED", "{method} {uri}");
    }
    let req = test::TestRequest::post()
        .uri("/auth/staff/refresh")
        .insert_header(("X-Staff-Device", s.device.clone()))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 401);
    // And the token without its device is no session either.
    let resp = call!(app, "GET", "/staff/me/context", s.token.clone());
    assert_eq!(resp.status(), 401);
}

async fn live<S>(app: &S, me: &str) -> u16
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    call!(app, "GET", "/staff/me/context", me.to_string())
        .status()
        .as_u16()
}

/// RO-4, RO-10: a new phone, a new number, the app switched off, the
/// employee suspended, the linked account deactivated or deleted — each signs
/// the phone out at once.
#[sqlx::test]
async fn every_revocation_signs_the_old_phone_out(pool: PgPool) {
    let app = app!(pool);
    let o = seed(&pool).await;
    let owner = owner_t(&o);
    let new_emp = async |name: &str, number: &str, user: Option<Uuid>| -> Uuid {
        employee(&pool, o.org, name, user, Some(number), true, &[o.a], 100).await
    };

    // A new phone: signing in again elsewhere.
    new_emp("Phone", "01020000001", None).await;
    let first = session_of(&sign_in(&app, &pool, "01020000001").await);
    assert_eq!(live(&app, &first).await, 200);
    let second = session_of(&sign_in(&app, &pool, "01020000001").await);
    assert_eq!(live(&app, &first).await, 401, "the old phone is signed out");
    assert_eq!(live(&app, &second).await, 200);

    // A new number.
    let e = new_emp("Number", "01020000002", None).await;
    let s = phone(&session(&pool, e).await);
    let resp = call!(
        app,
        "PUT",
        format!("/staff/employees/{e}"),
        owner,
        json!({ "phone": "01020000099" })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(live(&app, &s).await, 401);

    // The app switched off.
    let e = new_emp("NoApp", "01020000003", None).await;
    let s = phone(&session(&pool, e).await);
    let resp = call!(
        app,
        "PUT",
        format!("/staff/employees/{e}"),
        owner,
        json!({ "app_access": false })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(live(&app, &s).await, 401);

    // Suspended.
    let e = new_emp("Suspended", "01020000004", None).await;
    let s = phone(&session(&pool, e).await);
    let resp = call!(
        app,
        "PUT",
        format!("/staff/employees/{e}"),
        owner,
        json!({ "employment_status": "suspended" })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(live(&app, &s).await, 401);

    // Removed (terminated; the records stay).
    let e = new_emp("Removed", "01020000005", None).await;
    let s = phone(&session(&pool, e).await);
    assert_eq!(
        call!(app, "DELETE", format!("/staff/employees/{e}"), owner).status(),
        204
    );
    assert_eq!(live(&app, &s).await, 401);
    let status: String =
        sqlx::query_scalar("SELECT employment_status FROM employees WHERE id = $1")
            .bind(e)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "terminated");

    // The linked account deactivated.
    let u = user(&pool, o.org, "Linked", "teller", None).await;
    assign(&pool, u, o.a).await;
    let e = new_emp("Linked", "01020000006", Some(u)).await;
    let s = phone(&session(&pool, e).await);
    assert_eq!(live(&app, &s).await, 200);
    let resp = call!(
        app,
        "PATCH",
        format!("/users/{u}"),
        owner,
        json!({ "is_active": false })
    );
    assert!(resp.status().is_success(), "{}", resp.status());
    assert_eq!(live(&app, &s).await, 401);

    // The linked account deleted.
    let u = user(&pool, o.org, "Deleted", "teller", None).await;
    assign(&pool, u, o.a).await;
    let e = new_emp("Deleted", "01020000007", Some(u)).await;
    let s = phone(&session(&pool, e).await);
    assert_eq!(
        call!(app, "DELETE", format!("/users/{u}"), owner).status(),
        204
    );
    assert_eq!(live(&app, &s).await, 401);
}

/// `/staff/me/*` refuses an inactive employee even while the phone is live.
#[sqlx::test]
async fn an_inactive_employee_is_refused_with_a_live_phone(pool: PgPool) {
    let app = app!(pool);
    let o = seed(&pool).await;
    let e = employee(
        &pool,
        o.org,
        "Idle",
        None,
        Some("+201030000001"),
        true,
        &[o.a],
        100,
    )
    .await;
    let me = phone(&session(&pool, e).await);
    // Straight to the table: no revocation ran.
    sqlx::query("UPDATE employees SET employment_status = 'suspended' WHERE id = $1")
        .bind(e)
        .execute(&pool)
        .await
        .unwrap();
    for (method, uri, b) in [
        ("GET", "/staff/me/context", Value::Null),
        (
            "POST",
            "/staff/me/check-in",
            json!({ "branch_id": o.a, "latitude": LAT, "longitude": LNG }),
        ),
        ("GET", "/staff/me/payslips", Value::Null),
    ] {
        let resp = if b.is_null() {
            call!(app, method, uri, me)
        } else {
            call!(app, method, uri, me, b)
        };
        assert_eq!(resp.status(), 403, "{uri}");
        assert_eq!(body(resp).await["code"], "EMPLOYEE_INACTIVE");
    }
}

/// A user session is not a phone: `/staff/me/*` is the staff app's.
#[sqlx::test]
async fn a_dashboard_or_till_session_cannot_use_the_phone_routes(pool: PgPool) {
    let app = app!(pool);
    let o = seed(&pool).await;
    let u = user(&pool, o.org, "Cash", "teller", None).await;
    assign(&pool, u, o.a).await;
    employee(&pool, o.org, "Cash", Some(u), None, false, &[o.a], 100).await;
    for uri in ["/staff/me/context", "/staff/me/today", "/staff/me/payslips"] {
        let resp = call!(app, "GET", uri, user_token(u, o.org, UserRole::Teller));
        assert_eq!(resp.status(), 403, "{uri}");
        assert_eq!(body(resp).await["code"], "STAFF_APP_ONLY");
    }
}

/// RO-3: the staff token lives an hour; the phone refreshes it with its
/// device token.
#[sqlx::test]
async fn an_expired_session_refreshes_through_the_device(pool: PgPool) {
    let app = app!(pool);
    let o = seed(&pool).await;
    let e = employee(
        &pool,
        o.org,
        "Tok",
        None,
        Some("+201030000002"),
        true,
        &[o.a],
        100,
    )
    .await;
    let s = session(&pool, e).await;
    let now = Utc::now().timestamp() as usize;
    let stale = madar_rust::staff::principal::StaffClaims {
        sub: e.to_string(),
        org: o.org.to_string(),
        uid: None,
        dev: s.device_id.to_string(),
        typ: "dawam_staff".into(),
        aud: madar_rust::staff::principal::STAFF_AUDIENCE.into(),
        iat: now - 7200,
        exp: now - 3600,
    };
    let old = jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &stale,
        &jsonwebtoken::EncodingKey::from_secret(secret().0.as_bytes()),
    )
    .unwrap();
    let resp = call!(
        app,
        "GET",
        "/staff/me/context",
        format!("{old}|{}", s.device)
    );
    assert_eq!(resp.status(), 401);
    assert_eq!(body(resp).await["code"], "TOKEN_EXPIRED");

    let req = test::TestRequest::post()
        .uri("/auth/staff/refresh")
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        401,
        "the device is the credential"
    );
    let req = test::TestRequest::post()
        .uri("/auth/staff/refresh")
        .insert_header(("X-Staff-Device", s.device.clone()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let fresh = body(resp).await;
    assert_eq!(fresh["employee_id"], json!(e));
    let exp: chrono::DateTime<Utc> = fresh["expires_at"].as_str().unwrap().parse().unwrap();
    assert!(exp <= Utc::now() + Duration::minutes(61), "short-lived");
    let token = fresh["token"].as_str().unwrap();
    assert_eq!(
        call!(
            app,
            "GET",
            "/staff/me/context",
            format!("{token}|{}", s.device)
        )
        .status(),
        200
    );
}

// ── the business switches (PS-7, SA-3) ─────────────────────────────────────

/// Suspended, or Dawam off: every staff route refuses (phone or dashboard),
/// the code is not sent, and the nightly sweep leaves the records alone.
#[sqlx::test]
async fn a_suspended_or_dawam_off_business_is_refused_and_skipped_by_the_sweep(pool: PgPool) {
    let app = app!(pool);
    let live_org = seed(&pool).await;
    let off = seed(&pool).await;
    let suspended = seed(&pool).await;
    set_modules(&pool, off.org, &["pos"]).await;
    sqlx::query("UPDATE organizations SET is_active = false WHERE id = $1")
        .bind(suspended.org)
        .execute(&pool)
        .await
        .unwrap();

    // One employee each, rostered yesterday on a shift long over, never came.
    let yesterday = Utc::now().date_naive() - Duration::days(1);
    let mut people = Vec::new();
    for (o, number) in [
        (&live_org, "01040000001"),
        (&off, "01040000002"),
        (&suspended, "01040000003"),
    ] {
        let e = employee(
            &pool,
            o.org,
            "Rostered",
            None,
            Some(number),
            true,
            &[o.a],
            300_000,
        )
        .await;
        let s: Uuid = sqlx::query_scalar(
            "INSERT INTO work_shifts (org_id, branch_id, name, start_time, end_time) \
             VALUES ($1, $2, 'Early', '01:00', '02:00') RETURNING id",
        )
        .bind(o.org)
        .bind(o.a)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO staff_schedules (org_id, employee_id, work_shift_id, effective_from) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(o.org)
        .bind(e)
        .bind(s)
        .bind(yesterday - Duration::days(1))
        .execute(&pool)
        .await
        .unwrap();
        people.push((o, e, phone(&session(&pool, e).await)));
    }

    madar_rust::staff::jobs::run_tick(&pool).await.unwrap();
    for (o, e, me) in &people {
        let absences: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM attendance_records WHERE employee_id = $1 AND status = 'absent'",
        )
        .bind(e)
        .fetch_one(&pool)
        .await
        .unwrap();
        let live = o.org == live_org.org;
        assert_eq!(
            absences > 0,
            live,
            "the sweep runs only for a live Dawam org"
        );
        let resp = call!(app, "GET", "/staff/me/context", me.clone());
        let resp_mgr = call!(app, "GET", "/staff/employees", owner_t(o));
        if live {
            assert_eq!(resp.status(), 200);
            assert_eq!(resp_mgr.status(), 200);
        } else if o.org == off.org {
            assert_eq!(resp.status(), 403);
            assert_eq!(body(resp).await["code"], "DAWAM_OFF");
            assert_eq!(resp_mgr.status(), 403);
            assert_eq!(body(resp_mgr).await["code"], "DAWAM_OFF");
        } else {
            assert_eq!(resp.status(), 403);
            assert_eq!(body(resp).await["code"], "ORG_SUSPENDED");
            assert_eq!(resp_mgr.status(), 403);
        }
    }
    assert_eq!(
        otp_status(&app, "01040000002").await,
        404,
        "Dawam off: no code"
    );
}

// ── modules (PS-2, SA-1) ───────────────────────────────────────────────────

#[sqlx::test]
async fn modules_are_read_by_members_and_switched_by_a_super_admin_only(pool: PgPool) {
    let app = app!(pool);
    let o = seed(&pool).await;
    // A new business starts with POS only.
    let fresh = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'New', $2)")
        .bind(fresh)
        .bind(format!("new-{fresh}"))
        .execute(&pool)
        .await
        .unwrap();
    let modules: Vec<String> =
        sqlx::query_scalar("SELECT modules FROM organizations WHERE id = $1")
            .bind(fresh)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(modules, ["pos"]);

    // A branch manager reads them (the dashboard routes by the answer).
    let resp = call!(
        app,
        "GET",
        format!("/orgs/{}/modules", o.org),
        manager_t(&o)
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(body(resp).await["modules"], json!(["pos", "dawam"]));
    // Not another org's.
    let resp = call!(app, "GET", format!("/orgs/{fresh}/modules"), manager_t(&o));
    assert_eq!(resp.status(), 403);

    // The owner can't switch them; a super admin can.
    let resp = call!(
        app,
        "PATCH",
        format!("/orgs/{}", o.org),
        owner_t(&o),
        json!({ "modules": ["dawam"] })
    );
    assert_eq!(resp.status(), 403);
    let admin = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, NULL, 'Platform', $2, 'hash', 'super_admin')",
    )
    .bind(admin)
    .bind(format!("{admin}@test.com"))
    .execute(&pool)
    .await
    .unwrap();
    let sa =
        madar_rust::auth::jwt::create_token(&secret(), admin, None, UserRole::SuperAdmin, None, 1)
            .unwrap();
    let resp = call!(
        app,
        "PATCH",
        format!("/orgs/{}", o.org),
        sa,
        json!({ "modules": ["dawam"] })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(body(resp).await["modules"], json!(["dawam"]));
    let resp = call!(
        app,
        "PATCH",
        format!("/orgs/{}", o.org),
        sa,
        json!({ "modules": [] })
    );
    assert!(resp.status().is_client_error(), "never an empty set");
}

/// RO-9, audit B2: the business-wide rules are the owner's.
#[sqlx::test]
async fn the_rules_are_changed_for_every_branch_by_the_owner_only(pool: PgPool) {
    let app = app!(pool);
    let o = seed(&pool).await;
    let resp = call!(
        app,
        "PUT",
        "/staff/attendance/settings",
        manager_t(&o),
        json!({ "advance_cap_percent": 90 })
    );
    assert_eq!(resp.status(), 403);
    let resp = call!(
        app,
        "PUT",
        "/staff/attendance/settings",
        manager_t(&o),
        json!({ "branch_id": o.a, "working_days_per_month": 20 })
    );
    assert_eq!(resp.status(), 403, "a branch override is the rules too");
    let resp = call!(
        app,
        "PUT",
        "/staff/attendance/settings",
        owner_t(&o),
        json!({ "advance_cap_percent": 40 })
    );
    assert_eq!(resp.status(), 200);
}
