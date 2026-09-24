//! Dawam Phase B · clocking in, presence, location and privacy (audit 03,
//! AT-1/AT-4/AT-5). Every refusal the fixes add has a test here (AT-11):
//!
//! - the till PIN punch only from a real till (P0);
//! - offline time dated by the server's SIGNED anchor, forged stamps doubted;
//! - the punch's own fix checked for spoofing;
//! - left mid-shift: excuse hours, time away over the whole run, one notice;
//! - manager and till punches: window, night shift date, shift branch,
//!   queued time; approved corrections recorded as `correction`;
//! - sweeps in each branch's zone; coordinates wiped at payroll approval;
//!   privacy accepted per phone on the server; unrostered shifts close.
//!
//! Times: every test picks a branch zone that puts the branch's clock where
//! the test needs it (`tz_at`), so nothing depends on when the suite runs.

use actix_web::{App, test, web};
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Timelike, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::models::UserRole;
use madar_rust::staff::dawam::clock::sign_anchor;

mod common;
use common::employees::{
    Session, at_till, authed, employee, open_till, secret, session, session_unaccepted, user_token,
};

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .route(
                    "/auth/staff/refresh",
                    web::post().to(madar_rust::staff::dawam::signin::refresh),
                )
                .configure(madar_rust::staff::routes::configure),
        )
        .await
    };
}

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

fn phone(s: &Session) -> String {
    format!("{}|{}", s.token, s.device)
}

const LAT: f64 = 29.9792;
const LNG: f64 = 31.1342;
const AWAY: f64 = 30.02;

/// A fixed-offset zone whose clock reads `hour` o'clock (± the minutes past
/// the UTC hour) right now.
fn tz_at(hour: i64) -> String {
    let mut k = hour - i64::from(Utc::now().hour());
    if k < -12 {
        k += 24;
    }
    if k > 14 {
        k -= 24;
    }
    match k {
        0 => "UTC".into(),
        // POSIX sign: Etc/GMT-3 is UTC+3.
        k if k > 0 => format!("Etc/GMT-{k}"),
        k => format!("Etc/GMT+{}", -k),
    }
}

/// The wall clock at `at` in `tz` (Postgres decides, as the server does).
async fn local(pool: &PgPool, at: DateTime<Utc>, tz: &str) -> chrono::NaiveDateTime {
    sqlx::query_scalar("SELECT $1::timestamptz AT TIME ZONE $2")
        .bind(at)
        .bind(tz)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// `a` is a cashier on payroll (linked to `a_user`), `b` an app-only
/// employee, both at `branch`. The owner is on payroll too (`owner_e`), so
/// the owner's inbox shows what managers hear.
struct F {
    org: Uuid,
    branch: Uuid,
    tz: String,
    owner: Uuid,
    owner_e: Uuid,
    a: Uuid,
    a_user: Uuid,
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

async fn branch(pool: &PgPool, org: Uuid, name: &str, tz: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO branches (org_id, name, timezone, latitude, longitude, geo_radius_meters) \
         VALUES ($1, $2, $3::timezone_name, $4, $5, 200) RETURNING id",
    )
    .bind(org)
    .bind(name)
    .bind(tz)
    .bind(LAT)
    .bind(LNG)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn seed(pool: &PgPool, tz: &str) -> F {
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    let org: Uuid = sqlx::query_scalar(
        "INSERT INTO organizations (name, slug, modules) VALUES ('Cafe', $1, '{pos,dawam}') \
         RETURNING id",
    )
    .bind(format!("org-{}", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO attendance_settings (org_id, rules_saved_at) VALUES ($1, now())")
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    let branch = branch(pool, org, "Branch", tz).await;
    let owner = user(pool, org, "Owner", "org_admin").await;
    let a_user = user(pool, org, "Amal", "teller").await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(a_user)
        .bind(branch)
        .execute(pool)
        .await
        .unwrap();
    let a = employee(
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
    let owner_e = employee(pool, org, "Owner", Some(owner), None, false, &[branch], 0).await;
    F {
        org,
        branch,
        tz: tz.to_string(),
        owner,
        owner_e,
        a,
        a_user,
        b,
    }
}

fn owner_t(f: &F) -> String {
    user_token(f.owner, f.org, UserRole::OrgAdmin)
}

/// A shift template at the branch, `start`..`end` on the branch's clock.
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

async fn every_day(pool: &PgPool, f: &F, who: Uuid, shift: Uuid) {
    sqlx::query(
        "INSERT INTO staff_schedules (org_id, employee_id, work_shift_id, effective_from) \
         VALUES ($1, $2, $3, CURRENT_DATE - 60)",
    )
    .bind(f.org)
    .bind(who)
    .bind(shift)
    .execute(pool)
    .await
    .unwrap();
}

/// A day shift around now on the branch's clock: started `before` minutes
/// ago, ends `after` minutes from now.
async fn shift_around_now(pool: &PgPool, f: &F, who: Uuid, before: i64, after: i64) -> Uuid {
    let now = local(pool, Utc::now(), &f.tz).await;
    let s = shift(
        pool,
        f,
        &format!("Day {who}"),
        (now - Duration::minutes(before)).time(),
        (now + Duration::minutes(after)).time(),
    )
    .await;
    every_day(pool, f, who, s).await;
    s
}

async fn keys_for(pool: &PgPool, who: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT key FROM staff_notifications WHERE employee_id = $1 ORDER BY created_at",
    )
    .bind(who)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn flags_of(pool: &PgPool, who: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT kind FROM attendance_flags WHERE employee_id = $1 ORDER BY detected_at",
    )
    .bind(who)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// An offline stamp the server signed for this phone at `seen`, `elapsed`
/// later.
fn signed(s: &Session, seen: DateTime<Utc>, elapsed: Duration) -> Value {
    json!({
        "server_time": seen,
        "elapsed_ms": elapsed.num_milliseconds(),
        "rebooted": false,
        "anchor": sign_anchor(&secret(), s.device_id, seen),
    })
}

fn here() -> Value {
    json!({ "latitude": LAT, "longitude": LNG, "accuracy_meters": 12.0 })
}

fn with(mut v: Value, extra: Value) -> Value {
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

// ── the till (P0) ─────────────────────────────────────────────────────────

async fn give_pin(pool: &PgPool, user: Uuid, pin: &str) {
    sqlx::query("UPDATE users SET pin_hash = $2 WHERE id = $1")
        .bind(user)
        .bind(bcrypt::hash(pin, 4).unwrap())
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test]
async fn a_till_punch_needs_a_real_till_and_never_the_staff_app(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(12)).await;
    give_pin(&pool, f.a_user, "4321").await;
    let punch = json!({ "branch_id": f.branch, "pin": "4321" });
    let owner = owner_t(&f);

    // The Dawam app — even with a till's device id — is never the till.
    let device = open_till(&pool, f.org, f.branch, f.owner).await;
    let a_phone = session(&pool, f.a).await;
    let resp = call!(
        app,
        post,
        "/staff/attendance/till-punch",
        at_till(&phone(&a_phone), device),
        punch
    );
    assert_eq!(resp.status(), 403);
    assert_eq!(json_of(resp).await["code"], "TILL_ONLY");
    // A manager's phone session neither.
    common::employees::give_app(&pool, f.owner_e, "+201012345670").await;
    let owner_phone = session(&pool, f.owner_e).await;
    let resp = call!(
        app,
        post,
        "/staff/attendance/till-punch",
        at_till(&phone(&owner_phone), device),
        punch
    );
    assert_eq!(resp.status(), 403);

    // A POS session with no device, an unknown device, another branch's.
    let resp = call!(app, post, "/staff/attendance/till-punch", owner, punch);
    assert_eq!(resp.status(), 403);
    assert_eq!(json_of(resp).await["code"], "TILL_ONLY");
    let resp = call!(
        app,
        post,
        "/staff/attendance/till-punch",
        at_till(&owner, Uuid::new_v4()),
        punch
    );
    assert_eq!(resp.status(), 403);
    let other = branch(&pool, f.org, "Other", &f.tz).await;
    let elsewhere = open_till(&pool, f.org, other, f.owner).await;
    let resp = call!(
        app,
        post,
        "/staff/attendance/till-punch",
        at_till(&owner, elsewhere),
        punch
    );
    assert_eq!(resp.status(), 403, "a till of another branch");

    // The branch's device with no till open.
    sqlx::query("DELETE FROM tills WHERE device_id = $1")
        .bind(device)
        .execute(&pool)
        .await
        .unwrap();
    let resp = call!(
        app,
        post,
        "/staff/attendance/till-punch",
        at_till(&owner, device),
        punch
    );
    assert_eq!(resp.status(), 409);
    assert_eq!(json_of(resp).await["code"], "NO_TILL_SESSION");
    assert!(flags_of(&pool, f.a).await.is_empty());
    let none: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM attendance_records WHERE employee_id = $1")
            .bind(f.a)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(none, 0, "no refused path wrote a punch");

    // A retired device.
    let retired = open_till(&pool, f.org, f.branch, f.owner).await;
    sqlx::query("UPDATE devices SET retired_at = now() WHERE id = $1")
        .bind(retired)
        .execute(&pool)
        .await
        .unwrap();
    let resp = call!(
        app,
        post,
        "/staff/attendance/till-punch",
        at_till(&owner, retired),
        punch
    );
    assert_eq!(resp.status(), 403);

    // The real till: in, then out, and the branch's managers hear of it.
    let till = open_till(&pool, f.org, f.branch, f.owner).await;
    let body = json_of(call!(
        app,
        post,
        "/staff/attendance/till-punch",
        at_till(&owner, till),
        punch
    ))
    .await;
    assert_eq!(body["punched"], "in", "{body}");
    assert_eq!(body["record"]["check_in_method"], "till");
    let body = json_of(call!(
        app,
        post,
        "/staff/attendance/till-punch",
        at_till(&owner, till),
        punch
    ))
    .await;
    assert_eq!(body["punched"], "out");
    let heard = keys_for(&pool, f.owner_e).await;
    assert!(
        heard.contains(&"staff.n_till_punch_in".to_string()),
        "{heard:?}"
    );
    assert!(heard.contains(&"staff.n_till_punch_out".to_string()));
    // …in words a push can show with the app closed, in both languages.
    for key in ["staff.n_till_punch_in", "staff.n_till_punch_out"] {
        for ar in [false, true] {
            let said =
                madar_rust::push::render(key, &json!({ "name": "Amal" }), ar).unwrap_or_default();
            assert!(
                said.contains("Amal") && !said.contains("staff."),
                "{key} {ar}: {said}"
            );
        }
    }
}

#[sqlx::test]
async fn a_till_with_a_credential_must_prove_it(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(12)).await;
    give_pin(&pool, f.a_user, "4321").await;
    let till = open_till(&pool, f.org, f.branch, f.owner).await;
    let credential = "till-credential-0123456789abcdef";
    use sha2::Digest;
    let hash: String = sha2::Sha256::digest(credential.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    sqlx::query("UPDATE devices SET credential_hash = $2 WHERE id = $1")
        .bind(till)
        .bind(hash)
        .execute(&pool)
        .await
        .unwrap();
    let punch = json!({ "branch_id": f.branch, "pin": "4321" });
    let send = |token: Option<&str>| {
        let mut r = authed(
            test::TestRequest::post().uri("/staff/attendance/till-punch"),
            &at_till(&owner_t(&f), till),
        )
        .set_json(&punch);
        if let Some(t) = token {
            r = r.insert_header(("X-Madar-Device-Token", t.to_string()));
        }
        r.to_request()
    };
    assert_eq!(test::call_service(&app, send(None)).await.status(), 403);
    assert_eq!(
        test::call_service(&app, send(Some("wrong"))).await.status(),
        403
    );
    assert_eq!(
        test::call_service(&app, send(Some(credential)))
            .await
            .status(),
        200
    );
}

#[sqlx::test]
async fn guessing_a_colleagues_pin_at_the_till_slows_down(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(12)).await;
    give_pin(&pool, f.a_user, "4321").await;
    let till = at_till(
        &owner_t(&f),
        open_till(&pool, f.org, f.branch, f.owner).await,
    );
    // The free tries, then one more: the till now makes everyone wait.
    for guess in ["1111", "2222", "3333", "4444", "5555"] {
        let resp = call!(
            app,
            post,
            "/staff/attendance/till-punch",
            till,
            json!({ "branch_id": f.branch, "pin": guess })
        );
        assert_eq!(resp.status(), 401, "{guess}");
    }
    // Past the free tries: even the right PIN waits.
    let resp = call!(
        app,
        post,
        "/staff/attendance/till-punch",
        till,
        json!({ "branch_id": f.branch, "pin": "4321" })
    );
    assert_eq!(resp.status(), 429);
    let n: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM attendance_records WHERE employee_id = $1")
            .bind(f.a)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 0);
}

// ── offline time (CL-11) ───────────────────────────────────────────────────

#[sqlx::test]
async fn every_staff_answer_carries_a_signed_time_for_that_phone(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(12)).await;
    let s = session(&pool, f.a).await;
    let resp = call!(app, get, "/staff/me/today", phone(&s));
    assert_eq!(resp.status(), 200);
    let anchor = resp
        .headers()
        .get("x-dawam-time")
        .expect("the signed time")
        .to_str()
        .unwrap()
        .to_string();
    let at = madar_rust::staff::dawam::clock::verify_anchor(&secret(), s.device_id, &anchor)
        .expect("signed for this phone");
    assert!((Utc::now() - at).num_seconds().abs() < 5);
    // Not for another phone, and a dashboard session gets none.
    assert!(
        madar_rust::staff::dawam::clock::verify_anchor(&secret(), Uuid::new_v4(), &anchor)
            .is_none()
    );
    let resp = call!(app, get, "/staff/team/presence", owner_t(&f));
    assert!(resp.headers().get("x-dawam-time").is_none());
    // The refresh hands one over too.
    let req = test::TestRequest::post()
        .uri("/auth/staff/refresh")
        .insert_header(("X-Staff-Device", s.device.clone()))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let a = resp
        .headers()
        .get("x-dawam-time")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(madar_rust::staff::dawam::clock::verify_anchor(&secret(), s.device_id, a).is_some());
}

#[sqlx::test]
async fn a_signed_offline_punch_keeps_its_time_and_a_forged_one_is_doubted(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(12)).await;
    shift_around_now(&pool, &f, f.a, 120, 240).await;
    let s = session(&pool, f.a).await;

    // Queued 90 minutes ago against a time the server signed: exact, trusted.
    let seen = Utc::now() - Duration::minutes(100);
    let rec = json_of(call!(
        app,
        post,
        "/staff/me/check-in",
        phone(&s),
        with(
            here(),
            json!({ "branch_id": f.branch, "offline": signed(&s, seen, Duration::minutes(10)) })
        )
    ))
    .await;
    assert_eq!(rec["check_in_method"], "offline", "{rec}");
    let at: DateTime<Utc> = rec["check_in_at"].as_str().unwrap().parse().unwrap();
    assert!((at - (seen + Duration::minutes(10))).num_seconds().abs() <= 1);
    assert!(
        flags_of(&pool, f.a).await.is_empty(),
        "a signed time is not doubted"
    );

    // A ping claiming an earlier "server time" with no signature: dated as
    // claimed, marked unverified, and the manager is told once.
    let forged =
        json!({ "server_time": seen + Duration::minutes(20), "elapsed_ms": 0, "rebooted": false });
    let fix = |i: f64| json!({ "latitude": LAT + i * 0.00002, "longitude": LNG, "accuracy_meters": 8.0 + i });
    let resp = call!(
        app,
        post,
        "/staff/me/pings",
        phone(&s),
        with(fix(0.0), json!({ "offline": forged }))
    );
    assert_eq!(resp.status(), 200);
    let unverified: bool =
        sqlx::query_scalar("SELECT time_unverified FROM attendance_pings WHERE employee_id = $1")
            .bind(f.a)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(unverified);
    // Another phone's anchor, or a tampered one, is no better.
    let other = sign_anchor(&secret(), Uuid::new_v4(), seen + Duration::minutes(30));
    let mut tampered = sign_anchor(&secret(), s.device_id, seen + Duration::minutes(40));
    tampered.replace_range(tampered.len() - 4.., "beef");
    for (i, bad) in [other, tampered].into_iter().enumerate() {
        let stamp =
            json!({ "server_time": seen + Duration::minutes(30), "elapsed_ms": 0, "anchor": bad });
        let resp = call!(
            app,
            post,
            "/staff/me/pings",
            phone(&s),
            with(fix(1.0 + i as f64), json!({ "offline": stamp }))
        );
        assert_eq!(resp.status(), 200);
    }
    let doubted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM attendance_pings WHERE employee_id = $1 AND time_unverified",
    )
    .bind(f.a)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(doubted, 3);
    assert_eq!(flags_of(&pool, f.a).await, ["time_unverified"]);
    let told = keys_for(&pool, f.owner_e)
        .await
        .into_iter()
        .filter(|k| k == "staff.n_flag_time_unverified")
        .count();
    assert_eq!(told, 1, "told once, not per ping");
}

#[sqlx::test]
async fn a_backdated_check_in_with_a_forged_stamp_is_flagged_not_trusted(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(12)).await;
    let s = session(&pool, f.b).await;
    // Six days ago, "the server said" — nothing signed it.
    let claimed = Utc::now() - Duration::days(6);
    let rec = json_of(call!(
        app,
        post,
        "/staff/me/check-in",
        phone(&s),
        with(
            here(),
            json!({ "branch_id": f.branch,
            "offline": { "server_time": claimed, "elapsed_ms": 0, "rebooted": false } })
        )
    ))
    .await;
    assert_eq!(rec["check_in_method"], "offline", "{rec}");
    assert_eq!(flags_of(&pool, f.b).await, ["time_unverified"]);
    // A real week-old queue is refused outright, signed or not.
    let old = Utc::now() - Duration::days(8);
    let resp = call!(
        app,
        post,
        "/staff/me/check-out",
        phone(&s),
        with(
            here(),
            json!({ "offline": signed(&s, old, Duration::zero()) })
        )
    );
    assert_eq!(resp.status(), 400);
}

#[sqlx::test]
async fn a_reboot_is_unverified_and_satellite_time_cannot_predate_the_last_contact(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(12)).await;
    shift_around_now(&pool, &f, f.a, 120, 240).await;
    let s = session(&pool, f.a).await;
    let seen = Utc::now() - Duration::minutes(60);
    let mut stamp = signed(&s, seen, Duration::zero());
    stamp["rebooted"] = json!(true);
    // "Satellite" time before the last time the server saw the phone.
    stamp["gps_time"] = json!(seen - Duration::hours(3));
    let rec = json_of(call!(
        app,
        post,
        "/staff/me/check-in",
        phone(&s),
        with(here(), json!({ "branch_id": f.branch, "offline": stamp }))
    ))
    .await;
    let at: DateTime<Utc> = rec["check_in_at"].as_str().unwrap().parse().unwrap();
    assert!((at - seen).num_seconds().abs() <= 1, "{at} vs {seen}");
    assert_eq!(flags_of(&pool, f.a).await, ["time_unverified"]);
}

// ── privacy (AT-5) ─────────────────────────────────────────────────────────

#[sqlx::test]
async fn no_location_is_taken_before_this_phone_accepts_the_notice(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(12)).await;
    shift_around_now(&pool, &f, f.b, 30, 240).await;
    let s = session_unaccepted(&pool, f.b).await;

    let ctx = json_of(call!(app, get, "/staff/me/context", phone(&s))).await;
    assert!(ctx["privacy_accepted_at"].is_null(), "{ctx}");
    for (path, body) in [
        (
            "/staff/me/check-in",
            with(here(), json!({ "branch_id": f.branch })),
        ),
        ("/staff/me/pings", here()),
        ("/staff/me/check-out", here()),
    ] {
        let resp = call!(app, post, path, phone(&s), body);
        assert_eq!(resp.status(), 403, "{path}");
        assert_eq!(
            json_of(resp).await["code"],
            "PRIVACY_NOT_ACCEPTED",
            "{path}"
        );
    }
    // A dashboard session can't accept for anyone.
    let resp = call!(app, post, "/staff/me/privacy", owner_t(&f));
    assert_eq!(resp.status(), 403);

    let resp = call!(app, post, "/staff/me/privacy", phone(&s));
    assert_eq!(resp.status(), 200);
    let ctx = json_of(call!(app, get, "/staff/me/context", phone(&s))).await;
    assert!(ctx["privacy_accepted_at"].is_string());
    let resp = call!(
        app,
        post,
        "/staff/me/check-in",
        phone(&s),
        with(here(), json!({ "branch_id": f.branch }))
    );
    assert_eq!(resp.status(), 201);

    // A new phone asks again: acceptance belongs to the phone.
    let s2 = session_unaccepted(&pool, f.b).await;
    let ctx = json_of(call!(app, get, "/staff/me/context", phone(&s2))).await;
    assert!(ctx["privacy_accepted_at"].is_null());
    let resp = call!(app, post, "/staff/me/pings", phone(&s2), here());
    assert_eq!(resp.status(), 403);
}

// ── the punch's own fix (CL-8/9) ───────────────────────────────────────────

#[sqlx::test]
async fn a_mocked_or_perfect_fix_at_the_punch_is_suspicious(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(12)).await;
    shift_around_now(&pool, &f, f.a, 30, 240).await;
    shift_around_now(&pool, &f, f.b, 30, 240).await;
    // Tracking off, so no pings would ever check this.
    let a = session(&pool, f.a).await;
    let resp = call!(
        app,
        post,
        "/staff/me/check-in",
        phone(&a),
        json!({ "branch_id": f.branch, "latitude": LAT, "longitude": LNG,
                "accuracy_meters": 5.0, "is_mock": true, "tracking_off": true })
    );
    assert_eq!(resp.status(), 201);
    let mut kinds = flags_of(&pool, f.a).await;
    kinds.sort();
    assert_eq!(kinds, ["suspicious", "tracking_off"]);

    let b = session(&pool, f.b).await;
    let resp = call!(
        app,
        post,
        "/staff/me/check-in",
        phone(&b),
        json!({ "branch_id": f.branch, "latitude": LAT, "longitude": LNG, "accuracy_meters": 0.0 })
    );
    assert_eq!(resp.status(), 201);
    assert_eq!(flags_of(&pool, f.b).await, ["suspicious"]);
    // An honest fix at check-out adds nothing.
    let resp = call!(app, post, "/staff/me/check-out", phone(&b), here());
    assert_eq!(resp.status(), 200);
    assert_eq!(flags_of(&pool, f.b).await, ["suspicious"]);
}

// ── left mid-shift (CL-6/7) ────────────────────────────────────────────────

/// Check in at `start`, then ping at the given minutes after it, each at its
/// own (signed) time; `true` = at the branch.
macro_rules! walk {
    ($app:expr, $s:expr, $f:expr, $start:expr, $pings:expr) => {{
        let resp = call!(
            $app,
            post,
            "/staff/me/check-in",
            phone($s),
            with(here(), json!({ "branch_id": $f.branch, "offline": signed($s, $start, Duration::zero()) }))
        );
        assert_eq!(resp.status(), 201, "{:?}", test::read_body(resp).await);
        for (i, (min, inside)) in $pings.iter().enumerate() {
            let lat = if *inside { LAT + i as f64 * 0.00001 } else { AWAY + i as f64 * 0.0003 };
            let body = json!({ "latitude": lat, "longitude": LNG, "accuracy_meters": 11.0 + i as f64,
                               "offline": signed($s, $start, Duration::minutes(*min)) });
            let resp = call!($app, post, "/staff/me/pings", phone($s), body);
            assert_eq!(resp.status(), 200);
        }
    }};
}

async fn left_flag(pool: &PgPool, who: Uuid) -> Option<i32> {
    sqlx::query_scalar(
        "SELECT minutes_away FROM attendance_flags WHERE employee_id = $1 AND kind = 'left_mid_shift'",
    )
    .bind(who)
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn time_away_counts_the_whole_run_and_managers_hear_once(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(14)).await;
    shift_around_now(&pool, &f, f.a, 240, 240).await;
    let s = session(&pool, f.a).await;
    let start = Utc::now() - Duration::minutes(200);
    // In at +15 and +30, out from +45 to +120 (six pings, 75 minutes).
    let pings = [
        (15, true),
        (30, true),
        (45, false),
        (60, false),
        (75, false),
        (90, false),
        (105, false),
        (120, false),
    ];
    walk!(app, &s, &f, start, pings);
    assert_eq!(
        left_flag(&pool, f.a).await,
        Some(75),
        "not capped at three pings"
    );
    let told = keys_for(&pool, f.owner_e)
        .await
        .into_iter()
        .filter(|k| k == "staff.n_flag_left_mid_shift")
        .count();
    assert_eq!(told, 1, "one notice per flag, not one per ping");
    // The suggestion is 75 minutes at her minute rate, to the nearest 5 EGP.
    let list = json_of(call!(
        app,
        get,
        format!("/staff/flags?branch_id={}", f.branch),
        owner_t(&f)
    ))
    .await;
    let flag = list
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["kind"] == "left_mid_shift")
        .unwrap();
    let suggested = flag["suggested_deduction_piastres"].as_i64().unwrap();
    assert!(suggested > 0 && suggested % 500 == 0, "{suggested}");
}

#[sqlx::test]
async fn an_excuse_forgives_only_its_own_hours(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(14)).await;
    shift_around_now(&pool, &f, f.a, 240, 240).await;
    let s = session(&pool, f.a).await;
    let start = Utc::now() - Duration::minutes(200);
    let from = local(&pool, start + Duration::minutes(40), &f.tz).await;
    let to = local(&pool, start + Duration::minutes(80), &f.tz).await;
    sqlx::query(
        "INSERT INTO staff_requests (org_id, employee_id, kind, on_date, from_time, to_time, \
             status, decided_at, is_paid) \
         VALUES ($1, $2, 'excuse', $3, $4, $5, 'approved', now(), true)",
    )
    .bind(f.org)
    .bind(f.a)
    .bind(from.date())
    .bind(from.time())
    .bind(to.time())
    .execute(&pool)
    .await
    .unwrap();
    // Out at +45, +60, +75: inside the excuse — nothing.
    walk!(
        app,
        &s,
        &f,
        start,
        [(15, true), (45, false), (60, false), (75, false)]
    );
    assert_eq!(left_flag(&pool, f.a).await, None, "excused");
    // Still out at +100 and +115: past the excuse — flagged, and the excused
    // 35 minutes (+45..+80) are not counted as away.
    for min in [100, 115] {
        let body = json!({ "latitude": AWAY, "longitude": LNG + min as f64 * 0.0001, "accuracy_meters": 9.0,
                           "offline": signed(&s, start, Duration::minutes(min)) });
        assert_eq!(
            call!(app, post, "/staff/me/pings", phone(&s), body).status(),
            200
        );
    }
    assert_eq!(left_flag(&pool, f.a).await, Some(115 - 45 - 35));

    // Any excuse that day used to forgive the whole day: one at another hour
    // does not.
    let b = session(&pool, f.b).await;
    shift_around_now(&pool, &f, f.b, 240, 240).await;
    let early = local(&pool, start - Duration::minutes(30), &f.tz).await;
    sqlx::query(
        "INSERT INTO staff_requests (org_id, employee_id, kind, on_date, from_time, to_time, \
             status, decided_at, is_paid) \
         VALUES ($1, $2, 'excuse', $3, $4, $5, 'approved', now(), true)",
    )
    .bind(f.org)
    .bind(f.b)
    .bind(early.date())
    .bind(early.time())
    .bind((early + Duration::minutes(10)).time())
    .execute(&pool)
    .await
    .unwrap();
    walk!(app, &b, &f, start, [(15, true), (30, false), (45, false)]);
    assert!(left_flag(&pool, f.b).await.is_some());
}

#[sqlx::test]
async fn an_unpaid_excuse_deducts_the_exact_minutes_not_the_suggestion(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(14)).await;
    shift_around_now(&pool, &f, f.a, 240, 240).await;
    let s = session(&pool, f.a).await;
    let start = Utc::now() - Duration::minutes(200);
    walk!(app, &s, &f, start, [(15, true), (30, false), (47, false)]);
    let list = json_of(call!(
        app,
        get,
        format!("/staff/flags?branch_id={}", f.branch),
        owner_t(&f)
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
        owner_t(&f),
        json!({ "action": "excuse_unpaid" })
    );
    assert_eq!(resp.status(), 200);
    let amount: i64 = sqlx::query_scalar(
        "SELECT amount_piastres FROM payroll_deductions WHERE employee_id = $1 AND source = 'excused_unpaid'",
    )
    .bind(f.a)
    .fetch_one(&pool)
    .await
    .unwrap();
    // 17 minutes of a 6,000 EGP salary over 26 days of 8 hours (the default
    // day when the record has its shift's length): exact piastres.
    assert!(amount > 0);
    assert_eq!(flag["minutes_away"], 17);
    assert_ne!(amount % 500, 0, "exact, not rounded to 5 EGP: {amount}");
}

/// A second time away on the same shift (the first flag already handled)
/// can be deducted too, and neither line is undone when the day is re-priced
/// (a correction runs the rules again). The two used to collide with the one
/// automatic line per record and source: 409, or deleted by the recompute.
#[sqlx::test]
async fn a_second_flag_on_the_same_shift_deducts_and_survives_a_reprice(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(14)).await;
    shift_around_now(&pool, &f, f.a, 240, 240).await;
    let s = session(&pool, f.a).await;
    let start = Utc::now() - Duration::minutes(200);
    let open_flag = || {
        let pool = pool.clone();
        let who = f.a;
        async move {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM attendance_flags WHERE employee_id = $1 AND kind = 'left_mid_shift' \
                    AND resolution IS NULL",
            )
            .bind(who)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    walk!(app, &s, &f, start, [(15, true), (30, false), (45, false)]);
    let first = open_flag().await;
    let resp = call!(
        app,
        patch,
        format!("/staff/flags/{first}"),
        owner_t(&f),
        json!({ "action": "deduct", "amount_piastres": 5000 })
    );
    assert_eq!(resp.status(), 200, "{:?}", test::read_body(resp).await);
    // Back at +60, away again from +75.
    for (min, inside) in [(60, true), (75, false), (90, false)] {
        let lat = if inside {
            LAT
        } else {
            AWAY + min as f64 * 0.0001
        };
        let body = json!({ "latitude": lat, "longitude": LNG, "accuracy_meters": 9.0 + min as f64 / 10.0,
                           "offline": signed(&s, start, Duration::minutes(min)) });
        assert_eq!(
            call!(app, post, "/staff/me/pings", phone(&s), body).status(),
            200
        );
    }
    let second = open_flag().await;
    assert_ne!(first, second, "a new flag for the new time away");
    let resp = call!(
        app,
        patch,
        format!("/staff/flags/{second}"),
        owner_t(&f),
        json!({ "action": "excuse_unpaid" })
    );
    assert_eq!(resp.status(), 200, "{:?}", test::read_body(resp).await);
    let lines = || {
        let pool = pool.clone();
        let who = f.a;
        async move {
            sqlx::query_as::<_, (String, i64)>(
                "SELECT source, amount_piastres FROM payroll_deductions \
                  WHERE employee_id = $1 AND source IN ('left_mid_shift', 'excused_unpaid', 'unpaid_excuse') \
                  ORDER BY source",
            )
            .bind(who)
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };
    let before = lines().await;
    assert_eq!(
        before.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
        ["excused_unpaid", "left_mid_shift"],
        "both, under the one name for unpaid excused time"
    );
    // The manager corrects the day: the rules price it again.
    let rec: Uuid = sqlx::query_scalar("SELECT id FROM attendance_records WHERE employee_id = $1")
        .bind(f.a)
        .fetch_one(&pool)
        .await
        .unwrap();
    let resp = call!(
        app,
        patch,
        format!("/staff/attendance/{rec}"),
        owner_t(&f),
        json!({ "check_in_at": start - Duration::minutes(5), "reason": "Came in earlier" })
    );
    assert_eq!(resp.status(), 200, "{:?}", test::read_body(resp).await);
    assert_eq!(
        lines().await,
        before,
        "a manager's decisions outlive a re-price"
    );
}

// ── manager and till punches ───────────────────────────────────────────────

#[sqlx::test]
async fn a_managers_punch_is_marked_manager_and_respects_the_window(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(10)).await;
    // B's shift starts in four hours; its window opens two hours before.
    let now = local(&pool, Utc::now(), &f.tz).await;
    let later = shift(
        &pool,
        &f,
        "Late",
        (now + Duration::hours(4)).time(),
        (now + Duration::hours(8)).time(),
    )
    .await;
    every_day(&pool, &f, f.b, later).await;
    let resp = call!(
        app,
        post,
        "/staff/attendance/punch",
        owner_t(&f),
        json!({ "employee_id": f.b, "reason": "Dead phone" })
    );
    assert_eq!(resp.status(), 400, "too early for his shift");

    // A's shift is on now.
    shift_around_now(&pool, &f, f.a, 20, 240).await;
    let rec = json_of(call!(
        app,
        post,
        "/staff/attendance/punch",
        owner_t(&f),
        json!({ "employee_id": f.a, "reason": "Dead phone" })
    ))
    .await;
    assert_eq!(rec["check_in_method"], "manager", "{rec}");
    assert_eq!(rec["is_manual"], true);
    assert_eq!(rec["punch_reason"], "Dead phone");
    let rec = json_of(call!(
        app,
        post,
        "/staff/attendance/punch",
        owner_t(&f),
        json!({ "employee_id": f.a, "reason": "Dead phone" })
    ))
    .await;
    assert_eq!(rec["check_out_method"], "manager");
}

#[sqlx::test]
async fn a_managers_punch_after_midnight_lands_on_the_night_shifts_day(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(1)).await;
    let now = local(&pool, Utc::now(), &f.tz).await;
    let night = shift(
        &pool,
        &f,
        "Night",
        NaiveTime::from_hms_opt(22, 0, 0).unwrap(),
        NaiveTime::from_hms_opt(6, 0, 0).unwrap(),
    )
    .await;
    every_day(&pool, &f, f.b, night).await;
    let rec = json_of(call!(
        app,
        post,
        "/staff/attendance/punch",
        owner_t(&f),
        json!({ "employee_id": f.b, "reason": "Forgot phone" })
    ))
    .await;
    let day: NaiveDate = rec["business_date"].as_str().unwrap().parse().unwrap();
    assert_eq!(day, now.date() - Duration::days(1), "{rec}");
    assert_eq!(rec["work_shift_id"], json!(night));

    // The till, too.
    give_pin(&pool, f.a_user, "4321").await;
    every_day(&pool, &f, f.a, night).await;
    let till = at_till(
        &owner_t(&f),
        open_till(&pool, f.org, f.branch, f.owner).await,
    );
    let body = json_of(call!(
        app,
        post,
        "/staff/attendance/till-punch",
        till,
        json!({ "branch_id": f.branch, "pin": "4321" })
    ))
    .await;
    let day: NaiveDate = body["record"]["business_date"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(day, now.date() - Duration::days(1), "{body}");
}

#[sqlx::test]
async fn a_queued_managers_punch_keeps_its_own_time(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(12)).await;
    shift_around_now(&pool, &f, f.a, 120, 240).await;
    shift_around_now(&pool, &f, f.b, 120, 240).await;
    // From the owner's own phone, against a time the server signed for it.
    common::employees::give_app(&pool, f.owner_e, "+201012345670").await;
    let owner_phone = session(&pool, f.owner_e).await;
    let seen = Utc::now() - Duration::minutes(50);
    let rec = json_of(call!(
        app,
        post,
        "/staff/attendance/punch",
        phone(&owner_phone),
        json!({ "employee_id": f.a, "reason": "Dead phone", "offline": signed(&owner_phone, seen, Duration::minutes(5)) })
    ))
    .await;
    let at: DateTime<Utc> = rec["check_in_at"].as_str().unwrap().parse().unwrap();
    assert!(
        (at - (seen + Duration::minutes(5))).num_seconds().abs() <= 1,
        "{rec}"
    );
    assert!(flags_of(&pool, f.a).await.is_empty());
    // From the dashboard nothing vouches for a queued time: dated, doubted.
    let rec = json_of(call!(
        app,
        post,
        "/staff/attendance/punch",
        owner_t(&f),
        json!({ "employee_id": f.b, "reason": "Dead phone",
                "offline": { "server_time": seen, "elapsed_ms": 0 } })
    ))
    .await;
    assert_eq!(rec["check_in_method"], "manager");
    assert_eq!(flags_of(&pool, f.b).await, ["time_unverified"]);
}

#[sqlx::test]
async fn a_managers_punch_goes_to_the_rostered_shifts_branch(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(12)).await;
    let second = branch(&pool, f.org, "Second", &f.tz).await;
    sqlx::query(
        "INSERT INTO employee_branches (employee_id, branch_id, org_id) VALUES ($1, $2, $3)",
    )
    .bind(f.b)
    .bind(second)
    .bind(f.org)
    .execute(&pool)
    .await
    .unwrap();
    let now = local(&pool, Utc::now(), &f.tz).await;
    let s: Uuid = sqlx::query_scalar(
        "INSERT INTO work_shifts (org_id, branch_id, name, start_time, end_time) \
         VALUES ($1, $2, 'There', $3, $4) RETURNING id",
    )
    .bind(f.org)
    .bind(second)
    .bind((now - Duration::minutes(30)).time())
    .bind((now + Duration::hours(3)).time())
    .fetch_one(&pool)
    .await
    .unwrap();
    every_day(&pool, &f, f.b, s).await;
    let rec = json_of(call!(
        app,
        post,
        "/staff/attendance/punch",
        owner_t(&f),
        json!({ "employee_id": f.b, "reason": "No phone" })
    ))
    .await;
    assert_eq!(rec["branch_id"], json!(second), "{rec}");
}

// ── covers (CV, bug 6) ─────────────────────────────────────────────────────

#[sqlx::test]
async fn a_cover_queued_offline_starts_at_its_own_time(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(12)).await;
    // Bassem's shift began an hour ago and he never came.
    let shift_id = shift_around_now(&pool, &f, f.b, 60, 240).await;
    let s = session(&pool, f.a).await;
    let seen = Utc::now() - Duration::minutes(25);
    let rec = json_of(call!(
        app,
        post,
        "/staff/me/cover",
        phone(&s),
        with(
            here(),
            json!({ "employee_id": f.b, "work_shift_id": shift_id,
                             "offline": signed(&s, seen, Duration::minutes(5)) })
        )
    ))
    .await;
    assert_eq!(rec["check_in_method"], "cover", "{rec}");
    let at: DateTime<Utc> = rec["check_in_at"].as_str().unwrap().parse().unwrap();
    assert!((at - (seen + Duration::minutes(5))).num_seconds().abs() <= 1);
    // An honest cover raises only the cover flag for the manager.
    let kinds = flags_of(&pool, f.a).await;
    assert_eq!(kinds, ["cover"]);
}

// ── corrections (CL-16) ────────────────────────────────────────────────────

#[sqlx::test]
async fn a_corrected_punch_is_recorded_as_a_correction(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(12)).await;
    shift_around_now(&pool, &f, f.a, 60, 240).await;
    let s = session(&pool, f.a).await;
    let rec = json_of(call!(
        app,
        post,
        "/staff/me/check-in",
        phone(&s),
        with(here(), json!({ "branch_id": f.branch }))
    ))
    .await;
    let id = rec["id"].as_str().unwrap().to_string();
    let rec = json_of(call!(app, post, "/staff/me/check-out", phone(&s), here())).await;
    assert_eq!(rec["check_out_method"], "mobile_gps");

    // The manager moves the check-in: that punch becomes a correction, the
    // check-out keeps how it was made.
    let earlier = Utc::now() - Duration::minutes(50);
    let rec = json_of(call!(
        app,
        patch,
        format!("/staff/attendance/{id}"),
        owner_t(&f),
        json!({ "check_in_at": earlier, "reason": "Came in earlier" })
    ))
    .await;
    assert_eq!(rec["check_in_method"], "correction", "{rec}");
    assert_eq!(rec["check_out_method"], "mobile_gps");

    // An approved correction request moves the check-out.
    let day = rec["business_date"].as_str().unwrap().to_string();
    let out = local(&pool, Utc::now() - Duration::minutes(5), &f.tz)
        .await
        .time();
    let req = json_of(call!(
        app,
        post,
        "/staff/me/requests",
        phone(&s),
        json!({ "kind": "correction", "on_date": day, "attendance_record_id": id,
                "to_time": out.format("%H:%M").to_string(), "reason": "Left later" })
    ))
    .await;
    let resp = call!(
        app,
        patch,
        format!("/staff/requests/{}/decision", req["id"].as_str().unwrap()),
        owner_t(&f),
        json!({ "status": "approved" })
    );
    assert_eq!(resp.status(), 200, "{:?}", test::read_body(resp).await);
    let (inm, outm): (String, String) = sqlx::query_as(
        "SELECT check_in_method, check_out_method FROM attendance_records WHERE id = $1::uuid",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((inm.as_str(), outm.as_str()), ("correction", "correction"));
}

// ── auto-close (CL-15, CL-17) ──────────────────────────────────────────────

#[sqlx::test]
async fn an_unrostered_shift_closes_itself_and_stops_tracking(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(12)).await;
    let s = session(&pool, f.b).await;
    // No roster today: the punch opens a record with no scheduled end.
    let rec = json_of(call!(
        app,
        post,
        "/staff/me/check-in",
        phone(&s),
        with(here(), json!({ "branch_id": f.branch }))
    ))
    .await;
    let id: Uuid = rec["id"].as_str().unwrap().parse().unwrap();
    assert!(rec["scheduled_end_at"].is_null());
    madar_rust::staff::jobs::close_unrostered(&pool)
        .await
        .unwrap();
    let open: bool =
        sqlx::query_scalar("SELECT check_out_at IS NULL FROM attendance_records WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(open, "not before the presence limit");

    // Thirteen hours later (the default 10-hour day plus the 2-hour buffer).
    sqlx::query(
        "UPDATE attendance_records SET check_in_at = now() - INTERVAL '13 hours' WHERE id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    madar_rust::staff::jobs::close_unrostered(&pool)
        .await
        .unwrap();
    let (method, worked, span): (String, i32, f64) = sqlx::query_as(
        "SELECT check_out_method, worked_minutes, EXTRACT(EPOCH FROM check_out_at - check_in_at)::float8 \
           FROM attendance_records WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((method.as_str(), worked), ("auto", 600));
    assert_eq!(span, 36_000.0);
    // Closed: pings are refused (CL-17).
    let resp = call!(app, post, "/staff/me/pings", phone(&s), here());
    assert_eq!(resp.status(), 409);
}

#[sqlx::test]
async fn a_queued_check_out_amends_the_automatic_one(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(12)).await;
    shift_around_now(&pool, &f, f.a, 300, 30).await;
    let s = session(&pool, f.a).await;
    let start = Utc::now() - Duration::minutes(280);
    let resp = call!(
        app,
        post,
        "/staff/me/check-in",
        phone(&s),
        with(
            here(),
            json!({ "branch_id": f.branch, "offline": signed(&s, start, Duration::zero()) })
        )
    );
    assert_eq!(resp.status(), 201);
    // The sweep closed it at the scheduled end…
    sqlx::query(
        "UPDATE attendance_records SET check_out_at = scheduled_end_at, check_out_method = 'auto' \
          WHERE employee_id = $1",
    )
    .bind(f.a)
    .execute(&pool)
    .await
    .unwrap();
    // …while her phone held the real check-out, two hours in.
    let rec = json_of(call!(
        app,
        post,
        "/staff/me/check-out",
        phone(&s),
        with(
            here(),
            json!({ "offline": signed(&s, start, Duration::minutes(120)) })
        )
    ))
    .await;
    assert_eq!(rec["check_out_method"], "offline", "{rec}");
    let out: DateTime<Utc> = rec["check_out_at"].as_str().unwrap().parse().unwrap();
    assert!((out - (start + Duration::minutes(120))).num_seconds().abs() <= 1);
    // Without a queued stamp there is nothing to amend.
    let resp = call!(app, post, "/staff/me/check-out", phone(&s), here());
    assert_eq!(resp.status(), 404);
}

#[sqlx::test]
async fn a_shift_that_goes_quiet_without_a_low_battery_reads_tracking_off(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(12)).await;
    shift_around_now(&pool, &f, f.a, 120, 240).await;
    shift_around_now(&pool, &f, f.b, 120, 240).await;
    let a = session(&pool, f.a).await;
    let b = session(&pool, f.b).await;
    for s in [&a, &b] {
        let resp = call!(
            app,
            post,
            "/staff/me/check-in",
            phone(s),
            with(here(), json!({ "branch_id": f.branch }))
        );
        assert_eq!(resp.status(), 201);
    }
    // A pings at 60%, B at 10%; then both go silent for 50 minutes.
    for (s, battery) in [(&a, 60), (&b, 10)] {
        let resp = call!(
            app,
            post,
            "/staff/me/pings",
            phone(s),
            with(here(), json!({ "battery_percent": battery }))
        );
        assert_eq!(resp.status(), 200);
    }
    sqlx::query("UPDATE attendance_pings SET at = at - INTERVAL '50 minutes'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE attendance_records SET check_in_at = check_in_at - INTERVAL '50 minutes'")
        .execute(&pool)
        .await
        .unwrap();
    madar_rust::staff::jobs::tracking_went_quiet(&pool)
        .await
        .unwrap();
    madar_rust::staff::jobs::tracking_went_quiet(&pool)
        .await
        .unwrap();
    assert_eq!(flags_of(&pool, f.a).await, ["tracking_off"], "once");
    assert!(
        flags_of(&pool, f.b).await.is_empty(),
        "a low battery is the phone dying, not tracking off"
    );
}

// ── AT-1: each branch's day ────────────────────────────────────────────────

#[sqlx::test]
async fn the_absence_sweep_uses_the_branchs_day(pool: PgPool) {
    let db_today: NaiveDate = sqlx::query_scalar("SELECT CURRENT_DATE")
        .fetch_one(&pool)
        .await
        .unwrap();
    // A zone whose date is not the database server's, at least 01:00 there.
    let tz = [14i64, -12]
        .iter()
        .map(|k| match *k {
            k if k > 0 => format!("Etc/GMT-{k}"),
            k => format!("Etc/GMT+{}", -k),
        })
        .find(|tz| {
            let l = Utc::now()
                .with_timezone(&tz.parse::<chrono_tz::Tz>().unwrap())
                .naive_local();
            l.date() != db_today && l.hour() >= 1
        })
        .expect("one of the two always works");
    let f = seed(&pool, &tz).await;
    let now = local(&pool, Utc::now(), &f.tz).await;
    // A 10-minute shift just after midnight there, already over.
    let early = shift(
        &pool,
        &f,
        "Dawn",
        NaiveTime::from_hms_opt(0, 10, 0).unwrap(),
        NaiveTime::from_hms_opt(0, 20, 0).unwrap(),
    )
    .await;
    every_day(&pool, &f, f.b, early).await;
    madar_rust::staff::jobs::run_tick(&pool).await.unwrap();
    let days: Vec<NaiveDate> = sqlx::query_scalar(
        "SELECT business_date FROM attendance_records WHERE employee_id = $1 AND status = 'absent' ORDER BY 1",
    )
    .bind(f.b)
    .fetch_all(&pool)
    .await
    .unwrap();
    // The branch's yesterday and today — never a day outside its own two.
    assert_eq!(days, [now.date() - Duration::days(1), now.date()], "{tz}");
}

#[sqlx::test]
async fn team_presence_reads_each_branchs_day(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(12)).await;
    // A second branch where it is already tomorrow's small hours.
    let far_tz = tz_at(i64::from(Utc::now().hour()) + 13);
    let far = branch(&pool, f.org, "Far", &far_tz).await;
    let c = employee(&pool, f.org, "Chaker", None, None, false, &[far], 500_000).await;
    let far_now = local(&pool, Utc::now(), &far_tz).await;
    // Checked in on the far branch's today.
    sqlx::query(
        "INSERT INTO attendance_records (org_id, employee_id, branch_id, business_date, status, check_in_at, check_in_method) \
         VALUES ($1, $2, $3, $4, 'present', now(), 'manager')",
    )
    .bind(f.org)
    .bind(c)
    .bind(far)
    .bind(far_now.date())
    .execute(&pool)
    .await
    .unwrap();
    let body = json_of(call!(
        app,
        get,
        format!("/staff/team/presence?branch_id={far}"),
        owner_t(&f)
    ))
    .await;
    let row = body["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["employee_id"] == json!(c))
        .unwrap();
    assert_eq!(row["state"], "in", "{body}");
    assert_eq!(body["business_date"], json!(far_now.date()));
    assert_eq!(body["timezone"], json!(far_tz));
    // And across every branch, each person on their own branch's day.
    let body = json_of(call!(app, get, "/staff/team/presence", owner_t(&f))).await;
    let row = body["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["employee_id"] == json!(c))
        .unwrap();
    assert_eq!(row["state"], "in");
}

// ── AT-4: coordinates wiped at approval ────────────────────────────────────

#[sqlx::test]
async fn approving_the_month_wipes_its_coordinates_and_keeps_the_facts(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(12)).await;
    let day = |d: u32| NaiveDate::from_ymd_opt(2026, 8, d).unwrap();
    let rec = |d: NaiveDate| {
        let pool = pool.clone();
        let (org, who, branch) = (f.org, f.a, f.branch);
        async move {
            let id: Uuid = sqlx::query_scalar(
                "INSERT INTO attendance_records (org_id, employee_id, branch_id, business_date, status, \
                     check_in_at, check_in_latitude, check_in_longitude, check_in_distance_meters, \
                     check_in_method, check_out_at, check_out_latitude, check_out_longitude) \
                 VALUES ($1, $2, $3, $4, 'present', $4::date + TIME '09:00', 30.1, 31.2, 12.5, \
                         'mobile_gps', $4::date + TIME '17:00', 30.1, 31.2) RETURNING id",
            )
            .bind(org)
            .bind(who)
            .bind(branch)
            .bind(d)
            .fetch_one(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO attendance_pings (org_id, employee_id, attendance_record_id, at, latitude, \
                     longitude, distance_meters, inside) \
                 VALUES ($1, $2, $3, $4::date + TIME '12:00', 30.1, 31.2, 12.5, true)",
            )
            .bind(org)
            .bind(who)
            .bind(id)
            .bind(d)
            .execute(&pool)
            .await
            .unwrap();
            id
        }
    };
    let inside = rec(day(10)).await;
    let outside = rec(NaiveDate::from_ymd_opt(2026, 7, 10).unwrap()).await;
    let period: Uuid = sqlx::query_scalar(
        "INSERT INTO payroll_periods (org_id, name, start_date, end_date) VALUES ($1, 'Aug', $2, $3) RETURNING id",
    )
    .bind(f.org)
    .bind(day(1))
    .bind(day(31))
    .fetch_one(&pool)
    .await
    .unwrap();
    let resp = call!(
        app,
        post,
        format!("/staff/payroll/periods/{period}/generate"),
        owner_t(&f)
    );
    assert_eq!(resp.status(), 200, "{:?}", test::read_body(resp).await);

    let coords = |id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<f64>)>(
                "SELECT a.check_in_latitude, a.check_out_longitude, p.latitude, p.distance_meters, \
                        a.check_in_distance_meters \
                   FROM attendance_records a JOIN attendance_pings p ON p.attendance_record_id = a.id \
                  WHERE a.id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(
        coords(inside).await,
        (None, None, None, Some(12.5), Some(12.5)),
        "gone, distances kept"
    );
    assert_eq!(
        coords(outside).await.0,
        Some(30.1),
        "another month is untouched"
    );
    let wiped: bool = sqlx::query_scalar(
        "SELECT coordinates_wiped_at IS NOT NULL FROM payroll_periods WHERE id = $1",
    )
    .bind(period)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(wiped);

    // A ping that reaches an approved month later is caught by the sweep.
    sqlx::query(
        "INSERT INTO attendance_pings (org_id, employee_id, attendance_record_id, at, latitude, longitude, inside) \
         VALUES ($1, $2, $3, now(), 30.2, 31.3, true)",
    )
    .bind(f.org)
    .bind(f.a)
    .bind(inside)
    .execute(&pool)
    .await
    .unwrap();
    madar_rust::staff::jobs::purge_stale_coordinates(&pool)
        .await
        .unwrap();
    let left: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM attendance_pings WHERE attendance_record_id = $1 AND latitude IS NOT NULL",
    )
    .bind(inside)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, 0);
}

// ── a closed month (AT-7, orchestrator decision 1: one check, PERIOD_CLOSED) ──

/// Once a month's payroll is approved, no attendance edit reaches back into
/// it: the manual record, the correction and the delete all answer 409
/// `PERIOD_CLOSED` (the one `period_lock` check), and the record is left as
/// it was. A day in an open month still changes.
#[sqlx::test]
async fn an_approved_month_refuses_every_attendance_edit(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &tz_at(12)).await;
    let closed_day = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
    let open_day = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
    let rec = |d: NaiveDate| {
        let pool = pool.clone();
        let (org, who, branch) = (f.org, f.a, f.branch);
        async move {
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO attendance_records (org_id, employee_id, branch_id, business_date, status, \
                     check_in_at, check_in_method, check_out_at, check_out_method) \
                 VALUES ($1, $2, $3, $4, 'present', $4::date + TIME '09:00', 'mobile_gps', \
                         $4::date + TIME '17:00', 'mobile_gps') RETURNING id",
            )
            .bind(org)
            .bind(who)
            .bind(branch)
            .bind(d)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let closed_rec = rec(closed_day).await;
    let open_rec = rec(open_day).await;
    sqlx::query(
        "INSERT INTO payroll_periods (org_id, name, start_date, end_date, status) \
         VALUES ($1, 'Aug', '2026-08-01', '2026-08-31', 'generated')",
    )
    .bind(f.org)
    .execute(&pool)
    .await
    .unwrap();

    let closed = |resp: actix_web::dev::ServiceResponse| async move {
        let st = resp.status().as_u16();
        let body = String::from_utf8(test::read_body(resp).await.to_vec()).unwrap();
        assert_eq!(st, 409, "{body}");
        assert!(body.contains("PERIOD_CLOSED"), "{body}");
        assert!(
            !body.contains("MONTH_CLOSED"),
            "one check, one code: {body}"
        );
    };
    let later = |d: NaiveDate| d.and_hms_opt(8, 0, 0).unwrap().and_utc();
    closed(call!(
        app,
        patch,
        format!("/staff/attendance/{closed_rec}"),
        owner_t(&f),
        json!({ "check_in_at": later(closed_day), "reason": "Came in earlier" })
    ))
    .await;
    closed(call!(
        app,
        post,
        "/staff/attendance",
        owner_t(&f),
        json!({ "employee_id": f.b, "branch_id": f.branch, "business_date": closed_day,
                "check_in_at": later(closed_day), "reason": "Forgot the phone" })
    ))
    .await;
    closed(call!(
        app,
        delete,
        format!("/staff/attendance/{closed_rec}?reason=wrong"),
        owner_t(&f)
    ))
    .await;
    let method: Option<String> =
        sqlx::query_scalar("SELECT check_in_method FROM attendance_records WHERE id = $1")
            .bind(closed_rec)
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(method.as_deref(), Some("mobile_gps"), "kept, and untouched");

    // The open month still takes a correction.
    let resp = call!(
        app,
        patch,
        format!("/staff/attendance/{open_rec}"),
        owner_t(&f),
        json!({ "check_in_at": later(open_day), "reason": "Came in earlier" })
    );
    assert_eq!(resp.status(), 200, "{:?}", test::read_body(resp).await);
}

// ── money dates (AT-1, audit 08) ───────────────────────────────────────────

/// A pay line and an expense advance with no date land on the day where
/// the person works: their branch's day, not the database server's and not
/// the org's first branch's. UTC−11 and UTC+14 are always on different
/// dates, so the test holds whenever it runs. Before, the far branch's own
/// today was refused as "in the future".
#[sqlx::test]
async fn money_acts_are_dated_on_the_branchs_day(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "Pacific/Pago_Pago").await;
    let far = branch(&pool, f.org, "Kiritimati", "Pacific/Kiritimati").await;
    let nour = employee(
        &pool,
        f.org,
        "Nour",
        None,
        Some("+201012345677"),
        true,
        &[far],
        600_000,
    )
    .await;
    let there = local(&pool, Utc::now(), "Pacific/Kiritimati").await.date();
    let first = local(&pool, Utc::now(), "Pacific/Pago_Pago").await.date();
    assert_ne!(there, first);

    let adj = json_of(call!(
        app,
        post,
        "/staff/adjustments",
        owner_t(&f),
        json!({ "employee_id": nour, "kind": "bonus", "amount_piastres": 10_000, "reason": "Great week" })
    ))
    .await;
    assert_eq!(adj["effective_date"], json!(there), "{adj}");

    let resp = call!(
        app,
        post,
        "/staff/expense-advances",
        owner_t(&f),
        json!({ "employee_id": nour, "amount_piastres": 5_000, "purpose": "Milk", "via": "safe",
                "branch_id": far, "given_on": there })
    );
    assert_eq!(resp.status(), 201, "{:?}", test::read_body(resp).await);
    let resp = call!(
        app,
        post,
        "/staff/expense-advances",
        owner_t(&f),
        json!({ "employee_id": nour, "amount_piastres": 5_000, "purpose": "Sugar", "via": "safe",
                "branch_id": far })
    );
    assert_eq!(resp.status(), 201);
    assert_eq!(json_of(resp).await["given_on"], json!(there));
    // The day after the branch's today is still the future.
    let resp = call!(
        app,
        post,
        "/staff/expense-advances",
        owner_t(&f),
        json!({ "employee_id": nour, "amount_piastres": 5_000, "purpose": "Cups", "via": "safe",
                "branch_id": far, "given_on": there + Duration::days(1) })
    );
    assert_eq!(resp.status(), 400);
    // No writer can leave the day to the database any more.
    let left: Result<u64, _> = sqlx::query(
        "INSERT INTO expense_advances (org_id, employee_id, amount_piastres, purpose, via) \
         VALUES ($1, $2, 100, 'x', 'safe')",
    )
    .bind(f.org)
    .bind(nour)
    .execute(&pool)
    .await
    .map(|r| r.rows_affected());
    assert!(left.is_err(), "given_on has no server-date default");
}

/// E2E B-TEAM-4: one open record checked in AFTER its shift's end (a
/// hand-entered record) used to fail the auto-close on the order check and
/// abort the whole tick, for every org, every tick. It now closes at its own
/// check-in; and any row or org that still fails is logged and skipped, so
/// the rest of the tick runs — another org's absences are still marked.
#[sqlx::test]
async fn one_bad_record_or_org_never_stops_the_sweep(pool: PgPool) {
    let f = seed(&pool, &tz_at(12)).await;
    let g = seed(&pool, &tz_at(12)).await;
    let now = Utc::now();
    let today = local(&pool, now, &f.tz).await.date();
    let open = |who: Uuid, check_in: DateTime<Utc>| {
        let pool = pool.clone();
        let f_org = f.org;
        let f_branch = f.branch;
        async move {
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO attendance_records (org_id, employee_id, branch_id, business_date, \
                     status, scheduled_start_at, scheduled_end_at, check_in_at, check_in_method, \
                     is_manual) \
                 VALUES ($1, $2, $3, $4, 'present', $5, $6, $7, 'manager', true) RETURNING id",
            )
            .bind(f_org)
            .bind(who)
            .bind(f_branch)
            .bind(today)
            .bind(now - Duration::hours(5))
            .bind(now - Duration::hours(4))
            .bind(check_in)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    // Checked in an hour after the shift ended; never checked out.
    let late_in = now - Duration::hours(3);
    let bad = open(f.a, late_in).await;
    // A second open record that fails for a reason the sweep can't know.
    let doomed = open(f.b, now - Duration::hours(5)).await;
    sqlx::query(&format!(
        "CREATE FUNCTION e2e_boom() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
           IF TG_TABLE_NAME = 'attendance_records' AND NEW.id = '{doomed}'::uuid \
              OR TG_TABLE_NAME = 'payroll_periods' AND NEW.org_id = '{}'::uuid THEN \
             RAISE EXCEPTION 'boom'; \
           END IF; RETURN NEW; END $$",
        f.org
    ))
    .execute(&pool)
    .await
    .unwrap();
    for t in ["attendance_records", "payroll_periods"] {
        sqlx::query(&format!(
            "CREATE TRIGGER e2e_boom BEFORE INSERT OR UPDATE ON {t} \
             FOR EACH ROW EXECUTE FUNCTION e2e_boom()"
        ))
        .execute(&pool)
        .await
        .unwrap();
    }
    // Another org's person missed a shift that is over.
    let dawn = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO work_shifts (org_id, branch_id, name, start_time, end_time) \
         VALUES ($1, $2, 'Dawn', '01:00', '02:00') RETURNING id",
    )
    .bind(g.org)
    .bind(g.branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    every_day(&pool, &g, g.b, dawn).await;

    madar_rust::staff::jobs::run_tick(&pool).await.unwrap();

    let (out, method, worked): (Option<DateTime<Utc>>, Option<String>, i32) = sqlx::query_as(
        "SELECT check_out_at, check_out_method, worked_minutes FROM attendance_records WHERE id = $1",
    )
    .bind(bad)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (out.map(|t| t.timestamp()), method.as_deref(), worked),
        (Some(late_in.timestamp()), Some("auto"), 0),
        "closed at its own check-in, never before it"
    );
    let still_open: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT check_out_at FROM attendance_records WHERE id = $1")
            .bind(doomed)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(still_open.is_none(), "the failing row is skipped");
    let absent: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM attendance_records \
          WHERE employee_id = $1 AND business_date = $2 AND status = 'absent'",
    )
    .bind(g.b)
    .bind(today)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        absent, 1,
        "the other org's absence is marked in the same tick"
    );
    let g_periods: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM payroll_periods WHERE org_id = $1")
            .bind(g.org)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        g_periods, 1,
        "an org that fails to open its month skips only itself"
    );
}
