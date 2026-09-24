//! Staff module integration tests.
//!
//! The pure math is covered by unit tests in [`madar_rust::staff::rules`]; these
//! exercise the parts that only exist once a database and an HTTP layer are
//! involved — geofencing, shift resolution, the status/permission machines, and
//! the payroll generator's side effects on salary advances.
//!
//! Branches are pinned to `UTC` so a test can say "the shift started 40 minutes
//! ago" and mean it. The cross-midnight case uses a real timezone on purpose.

use actix_web::{App, test, web};
use chrono::{Duration, NaiveTime, Timelike, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::JwtSecret;
use madar_rust::models::UserRole;

mod common;
use common::employees::{Auth, authed, phone_token};

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn token_for(user_id: Uuid, org_id: Uuid, role: UserRole) -> String {
    madar_rust::auth::jwt::create_token(&get_secret(), user_id, Some(org_id), role, None, 24)
        .unwrap()
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(get_secret()))
                .configure(madar_rust::staff::routes::configure),
        )
        .await
    };
}

/// `$token` is a user's JWT, or a staff-app phone's `token|device` (see
/// `common::employees::phone_token`).
macro_rules! auth_get {
    ($app:expr, $uri:expr, $token:expr) => {{
        let req = authed(test::TestRequest::get().uri($uri), &$token).to_request();
        test::call_service(&$app, req).await
    }};
}

/// A body-carrying authenticated request. A macro rather than a helper closure
/// because a closure that borrows `app` across an `.await` can only be called
/// once, and every one of these tests fires the same request twice to prove an
/// operation is (or is not) repeatable.
macro_rules! auth_send {
    ($app:expr, $method:ident, $uri:expr, $token:expr, $body:expr) => {{
        let req = authed(test::TestRequest::$method().uri(&$uri), &$token)
            .set_json(&$body)
            .to_request();
        test::call_service(&$app, req).await
    }};
}

// ── Seeding ───────────────────────────────────────────────────

struct Fixture {
    org: Uuid,
    branch: Uuid,
    admin: Uuid,
    /// The employee (a Dawam employee id — never the user's).
    employee: Uuid,
    /// The Madar account the employee is linked to (a cashier).
    employee_user: Uuid,
}

/// Branch coordinates: the Giza pyramids, with a 200 m fence.
const BRANCH_LAT: f64 = 29.9792;
const BRANCH_LNG: f64 = 31.1342;

async fn seed(pool: &PgPool, timezone: &str) -> Fixture {
    // The role templates, before any user: an org's system roles take their
    // grants from them when the first account is made.
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    let org = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, modules) VALUES ($1, 'Test Org', $2, '{pos,dawam}')",
    )
        .bind(org)
        .bind(format!("org-{org}"))
        .execute(pool)
        .await
        .unwrap();

    // The owner saved the rules at set-up (RU-1): people may clock in.
    sqlx::query("INSERT INTO attendance_settings (org_id, rules_saved_at) VALUES ($1, now() - INTERVAL '60 days')")
        .bind(org)
        .execute(pool)
        .await
        .unwrap();

    let branch = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO branches (id, org_id, name, timezone, latitude, longitude, geo_radius_meters) \
         VALUES ($1, $2, 'Branch', $3::timezone_name, $4, $5, 200)",
    )
    .bind(branch)
    .bind(org)
    .bind(timezone)
    .bind(BRANCH_LAT)
    .bind(BRANCH_LNG)
    .execute(pool)
    .await
    .unwrap();

    let admin = seed_user(pool, org, "Admin", UserRole::OrgAdmin).await;
    let employee_user = seed_user(pool, org, "Employee", UserRole::Teller).await;
    grant_admin_everything(pool).await;
    // A cashier who is also on payroll: linked, at the branch, with the app.
    let employee = common::employees::employee(
        pool,
        org,
        "Employee",
        Some(employee_user),
        Some("+201012345678"),
        true,
        &[branch],
        0,
    )
    .await;

    Fixture {
        org,
        branch,
        admin,
        employee,
        employee_user,
    }
}

async fn seed_user(pool: &PgPool, org: Uuid, name: &str, role: UserRole) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, $3, $4, 'hash', $5::user_role)",
    )
    .bind(id)
    .bind(org)
    .bind(format!("{name} {id}"))
    .bind(format!("{}-{id}@test.com", name.to_lowercase()))
    .bind(match role {
        UserRole::OrgAdmin => "org_admin",
        UserRole::BranchManager => "branch_manager",
        _ => "teller",
    })
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn grant_admin_everything(pool: &PgPool) {
    for resource in ["staff", "work_shifts", "attendance", "leave", "payroll"] {
        for action in ["create", "read", "update", "delete"] {
            sqlx::query(
                "INSERT INTO role_permissions (role, resource, action, granted) \
                 VALUES ('org_admin'::user_role, $1::permission_resource, \
                         $2::permission_action, true) ON CONFLICT DO NOTHING",
            )
            .bind(resource)
            .bind(action)
            .execute(pool)
            .await
            .unwrap();
        }
    }
}

/// Put someone on payroll: an existing employee gets the salary; a Madar
/// user becomes a linked employee (a fresh id) at the org's branch. Returns
/// the employee.
async fn seed_profile(pool: &PgPool, org: Uuid, who: Uuid, salary_piastres: i64) -> Uuid {
    let updated = sqlx::query("UPDATE employees SET base_salary_piastres = $2 WHERE id = $1")
        .bind(who)
        .bind(salary_piastres)
        .execute(pool)
        .await
        .unwrap()
        .rows_affected();
    if updated > 0 {
        return who;
    }
    let branch: Uuid = sqlx::query_scalar("SELECT id FROM branches WHERE org_id = $1 LIMIT 1")
        .bind(org)
        .fetch_one(pool)
        .await
        .unwrap();
    common::employees::employee(
        pool,
        org,
        "Staff",
        Some(who),
        None,
        false,
        &[branch],
        salary_piastres,
    )
    .await
}

/// A work shift plus a roster row covering every day, so the employee is always
/// expected at `start`–`end`.
async fn seed_shift(
    pool: &PgPool,
    org: Uuid,
    branch: Uuid,
    name: &str,
    start: NaiveTime,
    end: NaiveTime,
    grace: i32,
) -> Uuid {
    let shift = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO work_shifts (id, org_id, branch_id, name, start_time, end_time, grace_minutes) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(shift)
    .bind(org)
    .bind(branch)
    .bind(name)
    .bind(start)
    .bind(end)
    .bind(grace)
    .execute(pool)
    .await
    .unwrap();
    shift
}

async fn roster(pool: &PgPool, org: Uuid, user: Uuid, shift: Uuid, day_of_week: Option<i16>) {
    sqlx::query(
        "INSERT INTO staff_schedules (org_id, employee_id, work_shift_id, day_of_week, effective_from) \
         VALUES ($1, $2, $3, $4, CURRENT_DATE - 30)",
    )
    .bind(org)
    .bind(user)
    .bind(shift)
    .bind(day_of_week)
    .execute(pool)
    .await
    .unwrap();
}

/// THE CLOCK-RELATIVE FIXTURES ARE DAY-AWARE.
///
/// These tests say "the shift started 40 minutes ago" by writing a bare
/// `NaiveTime` into `work_shifts.start_time`, and the server re-materialises it
/// as `business_date + start_time`. A bare time has no day, so a run near
/// midnight used to wrap: at 00:10 UTC, "40 minutes ago" is 23:30 — a time the
/// server places on the wrong side of midnight from the punch, and every dated
/// fixture beside it (an approved late arrival's `on_date`) then names a
/// different day from the attendance record's. Two tests failed that way in the
/// last verification, and only between 00:00 and 00:40 UTC.
///
/// The fix is to give the fixtures a day that cannot turn over while they run:
/// the branch is pinned to the whole-hour zone in which THIS RUN is local noon,
/// so an offset of ±8 hours still lands on the same local date, whatever the
/// wall clock in UTC says. Every offset below is a LOCAL time in that zone.
fn stable_zone_at(base: chrono::DateTime<Utc>) -> String {
    // POSIX sign convention: `Etc/GMT-2` is two hours AHEAD of UTC.
    let shift = 12 - base.hour() as i64;
    format!(
        "Etc/GMT{}{}",
        if shift > 0 { '-' } else { '+' },
        shift.abs()
    )
}

fn stable_zone() -> String {
    stable_zone_at(Utc::now())
}

/// The local date and time, in [`stable_zone_at`]'s zone, `minutes` from
/// `base` — derived from one full date-time, so the date always belongs to the
/// time beside it.
fn local_offset_at(base: chrono::DateTime<Utc>, minutes: i64) -> (chrono::NaiveDate, NaiveTime) {
    let shift = 12 - base.hour() as i64;
    let local = (base + Duration::hours(shift) + Duration::minutes(minutes)).naive_utc();
    (
        local.date(),
        NaiveTime::from_hms_opt(local.hour(), local.minute(), 0).unwrap(),
    )
}

/// Wall-clock time `minutes` from now in the fixtures' zone. Used to place a
/// shift's start relative to the moment the test runs.
fn utc_time_offset(minutes: i64) -> NaiveTime {
    local_offset_at(Utc::now(), minutes).1
}

/// The BUSINESS DATE that time falls on — what a dated fixture row (a leave or
/// late-arrival request) must name for the server to find it.
fn local_date_offset(minutes: i64) -> chrono::NaiveDate {
    local_offset_at(Utc::now(), minutes).0
}

#[actix_web::test]
async fn the_clock_fixtures_hold_on_both_sides_of_midnight() {
    let at = |h: u32, m: u32| {
        chrono::DateTime::from_naive_utc_and_offset(
            chrono::NaiveDate::from_ymd_opt(2026, 3, 14)
                .unwrap()
                .and_hms_opt(h, m, 0)
                .unwrap(),
            Utc,
        )
    };
    // Ten minutes either side of midnight UTC — the window that broke — plus a
    // midday run, and every hour of the day for good measure.
    for base in [at(23, 50), at(0, 10), at(12, 0)] {
        let (start_date, start) = local_offset_at(base, -40);
        let (end_date, end) = local_offset_at(base, 440);
        let (now_date, now) = local_offset_at(base, 0);
        assert_eq!(start_date, now_date, "the shift started on the punch's day");
        assert_eq!(end_date, now_date, "and ends on it");
        assert!(start < now && now < end, "{start} < {now} < {end}");
        assert_eq!(
            (now - start).num_minutes(),
            40,
            "forty minutes, not twenty-three hours and twenty"
        );
    }
    for hour in 0..24 {
        let base = at(hour, 30);
        let (d, t) = local_offset_at(base, 0);
        assert_eq!(d, local_offset_at(base, -40).0, "no wrap at {hour}:30 UTC");
        assert_eq!(d, local_offset_at(base, 480).0, "nor forward");
        assert_eq!(t.hour(), 12, "the run sits at local noon");
    }
}

async fn check_in(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    token: &str,
    branch: Uuid,
    lat: f64,
    lng: f64,
) -> actix_web::dev::ServiceResponse {
    let req = authed(test::TestRequest::post().uri("/staff/me/check-in"), token)
        .set_json(&json!({ "branch_id": branch, "latitude": lat, "longitude": lng }))
        .to_request();
    test::call_service(app, req).await
}

// ── Geofence ──────────────────────────────────────────────────

#[sqlx::test]
async fn check_in_inside_the_geofence_is_accepted(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &stable_zone()).await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let shift = seed_shift(
        &pool,
        f.org,
        f.branch,
        "Day",
        utc_time_offset(-10),
        utc_time_offset(470),
        15,
    )
    .await;
    roster(&pool, f.org, f.employee, shift, None).await;
    let token = phone_token(&pool, f.employee).await;

    // ~30 m north of the branch centre.
    let resp = check_in(&app, &token, f.branch, BRANCH_LAT + 0.0003, BRANCH_LNG).await;
    assert_eq!(
        resp.status(),
        201,
        "a punch inside the fence should be taken"
    );

    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["status"], "present");
    assert_eq!(body["check_in_method"], "mobile_gps");
    assert!(
        body["check_in_distance_meters"].as_f64().unwrap() < 200.0,
        "the measured distance should be stored for audit"
    );
}

#[sqlx::test]
async fn check_in_outside_the_geofence_is_refused(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &stable_zone()).await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let shift = seed_shift(
        &pool,
        f.org,
        f.branch,
        "Day",
        utc_time_offset(-10),
        utc_time_offset(470),
        15,
    )
    .await;
    roster(&pool, f.org, f.employee, shift, None).await;
    let token = phone_token(&pool, f.employee).await;

    // ~1.1 km away — well outside the 200 m fence.
    let resp = check_in(&app, &token, f.branch, BRANCH_LAT + 0.01, BRANCH_LNG).await;
    assert_eq!(
        resp.status(),
        403,
        "coordinates outside the fence must not clock anyone in"
    );

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attendance_records")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "a refused punch must not leave a record behind");
}

/// CL-2: the app's punch is always fenced. The org's old `require_geofence`
/// switch no longer reaches the phone (the spec has no off switch).
#[sqlx::test]
async fn the_fence_cannot_be_turned_off_for_the_app(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &stable_zone()).await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let shift = seed_shift(
        &pool,
        f.org,
        f.branch,
        "Day",
        utc_time_offset(-10),
        utc_time_offset(470),
        15,
    )
    .await;
    roster(&pool, f.org, f.employee, shift, None).await;
    sqlx::query(
        "INSERT INTO attendance_settings (org_id, require_geofence) VALUES ($1, FALSE)\
         ON CONFLICT (org_id, COALESCE(branch_id, '00000000-0000-0000-0000-000000000000'::uuid)) \
         DO UPDATE SET require_geofence = FALSE",
    )
    .bind(f.org)
    .execute(&pool)
    .await
    .unwrap();

    let token = phone_token(&pool, f.employee).await;
    let resp = check_in(&app, &token, f.branch, BRANCH_LAT + 0.01, BRANCH_LNG).await;
    assert_eq!(
        resp.status(),
        403,
        "the switch is gone: outside the radius is refused"
    );
    let resp = check_in(&app, &token, f.branch, BRANCH_LAT, BRANCH_LNG).await;
    assert_eq!(resp.status(), 201);
}

// ── Lateness ──────────────────────────────────────────────────

#[sqlx::test]
async fn arriving_within_grace_is_present_not_late(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &stable_zone()).await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    // Shift started 10 minutes ago with 15 minutes of grace.
    let shift = seed_shift(
        &pool,
        f.org,
        f.branch,
        "Day",
        utc_time_offset(-10),
        utc_time_offset(470),
        15,
    )
    .await;
    roster(&pool, f.org, f.employee, shift, None).await;
    let token = phone_token(&pool, f.employee).await;

    let resp = check_in(&app, &token, f.branch, BRANCH_LAT, BRANCH_LNG).await;
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["status"], "present");
    assert_eq!(body["late_minutes"], 0);
}

#[sqlx::test]
async fn arriving_past_grace_is_late_by_the_excess_only(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &stable_zone()).await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    // Started 40 minutes ago, 15 minutes of grace → 25 minutes late.
    let shift = seed_shift(
        &pool,
        f.org,
        f.branch,
        "Day",
        utc_time_offset(-40),
        utc_time_offset(440),
        15,
    )
    .await;
    roster(&pool, f.org, f.employee, shift, None).await;
    let token = phone_token(&pool, f.employee).await;

    let resp = check_in(&app, &token, f.branch, BRANCH_LAT, BRANCH_LNG).await;
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["status"], "late");
    let late = body["late_minutes"].as_i64().unwrap();
    assert!(
        (24..=25).contains(&late),
        "expected ~25 late minutes (40 elapsed − 15 grace), got {late}"
    );
}

#[sqlx::test]
async fn an_approved_late_arrival_forgives_the_lateness(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &stable_zone()).await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let shift = seed_shift(
        &pool,
        f.org,
        f.branch,
        "Day",
        utc_time_offset(-40),
        utc_time_offset(440),
        15,
    )
    .await;
    roster(&pool, f.org, f.employee, shift, None).await;

    // Approved arrival 5 minutes from now — comfortably after the punch.
    sqlx::query(
        "INSERT INTO staff_requests (org_id, employee_id, kind, on_date, to_time, status, decided_at) \
         VALUES ($1, $2, 'late_arrival', $4, $3, 'approved', now())",
    )
    .bind(f.org)
    .bind(f.employee)
    .bind(utc_time_offset(5))
    // The DAY the punch belongs to, derived beside the time (see
    // `stable_zone_at`) — not "today in UTC", which is a different day for a
    // run either side of midnight.
    .bind(local_date_offset(0))
    .execute(&pool)
    .await
    .unwrap();

    let token = phone_token(&pool, f.employee).await;
    let resp = check_in(&app, &token, f.branch, BRANCH_LAT, BRANCH_LNG).await;
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(
        body["status"], "present",
        "an approved late arrival should absorb the lateness"
    );
    assert_eq!(body["late_minutes"], 0);
}

// ── Duplicates and state ──────────────────────────────────────

#[sqlx::test]
async fn checking_in_twice_is_a_conflict_not_a_second_day(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &stable_zone()).await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let shift = seed_shift(
        &pool,
        f.org,
        f.branch,
        "Day",
        utc_time_offset(-10),
        utc_time_offset(470),
        15,
    )
    .await;
    roster(&pool, f.org, f.employee, shift, None).await;
    let token = phone_token(&pool, f.employee).await;

    assert_eq!(
        check_in(&app, &token, f.branch, BRANCH_LAT, BRANCH_LNG)
            .await
            .status(),
        201
    );
    assert_eq!(
        check_in(&app, &token, f.branch, BRANCH_LAT, BRANCH_LNG)
            .await
            .status(),
        409,
        "a second punch must not create a second paid day"
    );

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attendance_records")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test]
async fn clocking_straight_back_out_is_a_half_day_not_an_absence(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &stable_zone()).await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let shift = seed_shift(
        &pool,
        f.org,
        f.branch,
        "Day",
        utc_time_offset(-10),
        utc_time_offset(470),
        15,
    )
    .await;
    roster(&pool, f.org, f.employee, shift, None).await;
    let token = phone_token(&pool, f.employee).await;

    check_in(&app, &token, f.branch, BRANCH_LAT, BRANCH_LNG).await;
    let req = test::TestRequest::post()
        .uri("/staff/me/check-out")
        .auth(&token)
        .set_json(&json!({ "latitude": BRANCH_LAT, "longitude": BRANCH_LNG }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let body: serde_json::Value = test::read_body_json(resp).await;

    assert_eq!(
        body["status"], "half_day",
        "someone who physically clocked in must never be recorded absent — \
         absent is what payroll docks a whole day for"
    );
}

#[sqlx::test]
async fn checking_out_without_checking_in_is_a_404(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let token = phone_token(&pool, f.employee).await;

    let req = test::TestRequest::post()
        .uri("/staff/me/check-out")
        .auth(&token)
        .set_json(&json!({ "latitude": BRANCH_LAT, "longitude": BRANCH_LNG }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 404);
}

#[sqlx::test]
async fn today_tells_the_app_which_branch_to_clock_in_at(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &stable_zone()).await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let shift = seed_shift(
        &pool,
        f.org,
        f.branch,
        "Day",
        utc_time_offset(-10),
        utc_time_offset(470),
        15,
    )
    .await;
    roster(&pool, f.org, f.employee, shift, None).await;
    let token = phone_token(&pool, f.employee).await;

    let resp = auth_get!(app, "/staff/me/today", token);
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(
        body["branch_id"],
        f.branch.to_string(),
        "the server must name the branch — a picker would make the geofence \
         answerable to a dropdown"
    );
    assert_eq!(body["can_check_in"], true);
}

#[sqlx::test]
async fn an_employee_with_no_resolvable_branch_cannot_check_in(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    // No roster, no branch assignment: nothing says where they work.
    sqlx::query("DELETE FROM employee_branches WHERE employee_id = $1")
        .bind(f.employee)
        .execute(&pool)
        .await
        .unwrap();
    let token = phone_token(&pool, f.employee).await;

    let resp = auth_get!(app, "/staff/me/today", token);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert!(body["branch_id"].is_null());
    assert_eq!(
        body["can_check_in"], false,
        "guessing a branch would clock someone in somewhere they are not"
    );
    assert!(body["blocked_reason"].as_str().unwrap().contains("branch"));
}

#[sqlx::test]
async fn a_user_session_cannot_clock_in(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    // The cashier's till/dashboard session, not the phone: a punch comes only
    // from the employee's live phone (CL-1).
    let token = token_for(f.employee_user, f.org, UserRole::Teller);

    let resp = check_in(&app, &token, f.branch, BRANCH_LAT, BRANCH_LNG).await;
    assert_eq!(resp.status(), 403, "being a user is not a staff-app phone");
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["code"], "STAFF_APP_ONLY", "{body}");
}

#[sqlx::test]
async fn a_suspended_employee_cannot_clock_in(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    sqlx::query("UPDATE employees SET employment_status = 'suspended' WHERE id = $1")
        .bind(f.employee)
        .execute(&pool)
        .await
        .unwrap();
    let token = phone_token(&pool, f.employee).await;

    assert_eq!(
        check_in(&app, &token, f.branch, BRANCH_LAT, BRANCH_LNG)
            .await
            .status(),
        403
    );
}

#[sqlx::test]
async fn check_out_closes_the_day_and_records_worked_minutes(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &stable_zone()).await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let shift = seed_shift(
        &pool,
        f.org,
        f.branch,
        "Day",
        utc_time_offset(-10),
        utc_time_offset(470),
        15,
    )
    .await;
    roster(&pool, f.org, f.employee, shift, None).await;
    let token = phone_token(&pool, f.employee).await;

    check_in(&app, &token, f.branch, BRANCH_LAT, BRANCH_LNG).await;
    // Backdate the check-in so the checkout has a measurable span.
    sqlx::query("UPDATE attendance_records SET check_in_at = now() - INTERVAL '6 hours'")
        .execute(&pool)
        .await
        .unwrap();

    let req = test::TestRequest::post()
        .uri("/staff/me/check-out")
        .auth(&token)
        .set_json(&json!({ "latitude": BRANCH_LAT, "longitude": BRANCH_LNG }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = test::read_body_json(resp).await;
    let worked = body["worked_minutes"].as_i64().unwrap();
    assert!(
        (359..=361).contains(&worked),
        "expected ~360 worked minutes, got {worked}"
    );
    assert!(body["check_out_at"].is_string());
}

// ── Shift resolution ──────────────────────────────────────────

#[sqlx::test]
async fn a_date_override_outranks_the_weekly_roster(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let morning = seed_shift(
        &pool,
        f.org,
        f.branch,
        "Morning",
        NaiveTime::from_hms_opt(8, 0, 0).unwrap(),
        NaiveTime::from_hms_opt(16, 0, 0).unwrap(),
        15,
    )
    .await;
    let evening = seed_shift(
        &pool,
        f.org,
        f.branch,
        "Evening",
        NaiveTime::from_hms_opt(16, 0, 0).unwrap(),
        NaiveTime::from_hms_opt(23, 0, 0).unwrap(),
        15,
    )
    .await;
    roster(&pool, f.org, f.employee, morning, None).await;
    sqlx::query(
        "INSERT INTO staff_schedule_overrides (org_id, employee_id, on_date, work_shift_id) \
         VALUES ($1, $2, CURRENT_DATE, $3)",
    )
    .bind(f.org)
    .bind(f.employee)
    .bind(evening)
    .execute(&pool)
    .await
    .unwrap();

    let token = token_for(f.admin, f.org, UserRole::OrgAdmin);
    let today = Utc::now().date_naive();
    let resp = auth_get!(
        app,
        &format!(
            "/staff/schedules/day?employee_id={}&date={}&branch_id={}",
            f.employee, today, f.branch
        ),
        token
    );
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(
        body[0]["name"], "Evening",
        "the override, not the weekly row, should win"
    );
}

#[sqlx::test]
async fn an_override_with_no_shift_is_a_day_off(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let morning = seed_shift(
        &pool,
        f.org,
        f.branch,
        "Morning",
        NaiveTime::from_hms_opt(8, 0, 0).unwrap(),
        NaiveTime::from_hms_opt(16, 0, 0).unwrap(),
        15,
    )
    .await;
    roster(&pool, f.org, f.employee, morning, None).await;
    sqlx::query(
        "INSERT INTO staff_schedule_overrides (org_id, employee_id, on_date, work_shift_id) \
         VALUES ($1, $2, CURRENT_DATE, NULL)",
    )
    .bind(f.org)
    .bind(f.employee)
    .execute(&pool)
    .await
    .unwrap();

    let token = token_for(f.admin, f.org, UserRole::OrgAdmin);
    let today = Utc::now().date_naive();
    let resp = auth_get!(
        app,
        &format!(
            "/staff/schedules/day?employee_id={}&date={}&branch_id={}",
            f.employee, today, f.branch
        ),
        token
    );
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert!(
        body.as_array().unwrap().is_empty(),
        "a NULL-shift override means no shift at all"
    );
}

#[sqlx::test]
async fn a_night_shift_ends_on_the_following_day(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "Africa/Cairo").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let night = seed_shift(
        &pool,
        f.org,
        f.branch,
        "Night",
        NaiveTime::from_hms_opt(22, 0, 0).unwrap(),
        NaiveTime::from_hms_opt(6, 0, 0).unwrap(),
        15,
    )
    .await;
    roster(&pool, f.org, f.employee, night, None).await;

    let crosses: bool =
        sqlx::query_scalar("SELECT crosses_midnight FROM work_shifts WHERE id = $1")
            .bind(night)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(crosses, "22:00→06:00 must be detected as crossing midnight");

    let token = token_for(f.admin, f.org, UserRole::OrgAdmin);
    let today = Utc::now().date_naive();
    let resp = auth_get!(
        app,
        &format!(
            "/staff/schedules/day?employee_id={}&date={}&branch_id={}",
            f.employee, today, f.branch
        ),
        token
    );
    let body: serde_json::Value = test::read_body_json(resp).await;
    let start = body[0]["scheduled_start_at"].as_str().unwrap().to_string();
    let end = body[0]["scheduled_end_at"].as_str().unwrap().to_string();
    assert!(
        end > start,
        "the window must not collapse to a negative span"
    );

    let span: i64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM ($2::timestamptz - $1::timestamptz))::bigint / 60",
    )
    .bind(&start)
    .bind(&end)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(span, 480, "22:00→06:00 is an eight-hour shift");
}

// ── Directory + salary visibility ─────────────────────────────

#[sqlx::test]
async fn salary_is_hidden_from_callers_without_payroll_access(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 750_000).await;

    // An org admin with full grants sees the figure…
    let admin_token = token_for(f.admin, f.org, UserRole::OrgAdmin);
    let resp = auth_get!(app, "/staff/employees", admin_token);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body[0]["base_salary_piastres"], 750_000);

    // …the branch's manager, who reads the staff (`hr.staff.read`) but not
    // payroll (`hr.payroll.read` is the owner's), does not.
    let manager = seed_user(&pool, f.org, "Manager", UserRole::BranchManager).await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(manager)
        .bind(f.branch)
        .execute(&pool)
        .await
        .unwrap();

    let manager_token = token_for(manager, f.org, UserRole::BranchManager);
    let resp = auth_get!(app, "/staff/employees", manager_token);
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert!(
        body[0]["base_salary_piastres"].is_null(),
        "salary must not leak to a caller without payroll access"
    );
}

#[sqlx::test]
async fn a_department_holding_employees_cannot_be_deleted(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let token = token_for(f.admin, f.org, UserRole::OrgAdmin);

    let req = test::TestRequest::post()
        .uri("/staff/departments")
        .auth(&token)
        .set_json(&json!({ "name": "Kitchen" }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);
    let dept: serde_json::Value = test::read_body_json(resp).await;
    let dept_id = dept["id"].as_str().unwrap().to_string();

    sqlx::query("UPDATE employees SET department_id = $1 WHERE id = $2")
        .bind(Uuid::parse_str(&dept_id).unwrap())
        .bind(f.employee)
        .execute(&pool)
        .await
        .unwrap();

    let req = test::TestRequest::delete()
        .uri(&format!("/staff/departments/{dept_id}"))
        .auth(&token)
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        400,
        "deleting an occupied department would silently unfile people"
    );
}

// ── Leave ─────────────────────────────────────────────────────

async fn seed_leave_type(pool: &PgPool, org: Uuid, paid: bool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO leave_types (id, org_id, name, is_paid, annual_quota_days) \
         VALUES ($1, $2, $3, $4, 21)",
    )
    .bind(id)
    .bind(org)
    .bind(if paid { "Annual" } else { "Unpaid" })
    .bind(paid)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// RQ-2, RQ-3: leave has no types and no balances. A type an older client
/// sends is ignored, approving writes no balance, and the approver must say
/// paid or unpaid; cancelling approved leave needs a reason (AT-7).
#[sqlx::test]
async fn approving_leave_asks_paid_or_unpaid_and_writes_no_balance(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let leave_type = seed_leave_type(&pool, f.org, true).await;
    let admin_token = token_for(f.admin, f.org, UserRole::OrgAdmin);
    let employee_token = phone_token(&pool, f.employee).await;

    let req = test::TestRequest::post()
        .uri("/staff/me/requests")
        .auth(&employee_token)
        .set_json(&json!({
            "kind": "leave",
            "leave_type_id": leave_type,
            "on_date": "2026-09-01",
            "end_date": "2026-09-03",
            "reason": "Family"
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);
    let request: serde_json::Value = test::read_body_json(resp).await;
    assert!(
        request["leave_type_id"].is_null(),
        "a type is never stored: {request}"
    );
    let request_id = request["id"].as_str().unwrap().to_string();

    let decision_uri = format!("/staff/requests/{request_id}/decision");
    assert_eq!(
        auth_send!(
            app,
            patch,
            decision_uri,
            admin_token,
            json!({ "status": "approved" })
        )
        .status(),
        400,
        "leave is approved as paid or unpaid — the approver must say which"
    );
    let resp = auth_send!(
        app,
        patch,
        decision_uri,
        admin_token,
        json!({ "status": "approved", "is_paid": false })
    );
    assert_eq!(resp.status(), 200);
    let row: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(row["is_paid"], false);
    let balances: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM leave_balances WHERE employee_id = $1")
            .bind(f.employee)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(balances, 0, "no balance is spent (RQ-3)");

    assert_eq!(
        auth_send!(
            app,
            patch,
            decision_uri,
            admin_token,
            json!({ "status": "cancelled" })
        )
        .status(),
        400,
        "undoing an approval says why (AT-7)"
    );
    assert_eq!(
        auth_send!(
            app,
            patch,
            decision_uri,
            admin_token,
            json!({ "status": "cancelled", "note": "Filed the wrong week" })
        )
        .status(),
        200
    );
}

#[sqlx::test]
async fn overlapping_leave_requests_are_refused(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let leave_type = seed_leave_type(&pool, f.org, true).await;
    let token = phone_token(&pool, f.employee).await;

    let uri = "/staff/me/requests".to_string();
    assert_eq!(
        auth_send!(
            app,
            post,
            uri,
            token,
            json!({ "kind": "leave", "leave_type_id": leave_type,
                    "on_date": "2026-09-01", "end_date": "2026-09-05" })
        )
        .status(),
        201
    );
    assert_eq!(
        auth_send!(
            app,
            post,
            uri,
            token,
            json!({ "kind": "leave", "leave_type_id": leave_type,
                    "on_date": "2026-09-04", "end_date": "2026-09-08" })
        )
        .status(),
        409,
        "two live requests over the same day can't both land (RQ-11)"
    );
}

#[sqlx::test]
async fn a_decided_request_cannot_be_decided_again(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let leave_type = seed_leave_type(&pool, f.org, true).await;
    let token = token_for(f.admin, f.org, UserRole::OrgAdmin);

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO staff_requests (org_id, employee_id, kind, leave_type_id, on_date, end_date) \
         VALUES ($1, $2, 'leave', $3, '2026-09-01', '2026-09-01') RETURNING id",
    )
    .bind(f.org)
    .bind(f.employee)
    .bind(leave_type)
    .fetch_one(&pool)
    .await
    .unwrap();

    let uri = format!("/staff/requests/{id}/decision");
    assert_eq!(
        auth_send!(app, patch, uri, token, json!({ "status": "rejected" })).status(),
        200
    );
    assert_eq!(
        auth_send!(
            app,
            patch,
            uri,
            token,
            json!({ "status": "approved", "is_paid": true })
        )
        .status(),
        409,
        "a rejected request stays rejected"
    );
}

// ── Payroll ───────────────────────────────────────────────────

async fn seed_period(pool: &PgPool, org: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO payroll_periods (id, org_id, name, start_date, end_date) \
         VALUES ($1, $2, 'September', '2026-09-01', '2026-09-30')",
    )
    .bind(id)
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn generate(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    token: &str,
    period: Uuid,
) -> actix_web::dev::ServiceResponse {
    let req = test::TestRequest::post()
        .uri(&format!("/staff/payroll/periods/{period}/generate"))
        .auth(&token)
        .to_request();
    test::call_service(app, req).await
}

#[sqlx::test]
async fn a_clean_period_pays_base_salary(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let period = seed_period(&pool, f.org).await;
    let token = token_for(f.admin, f.org, UserRole::OrgAdmin);

    let resp = generate(&app, &token, period).await;
    assert_eq!(resp.status(), 200);
    let slips: serde_json::Value = test::read_body_json(resp).await;
    let mine = slips
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["employee_id"] == f.employee.to_string())
        .expect("the employee should have a payslip");
    assert_eq!(mine["net_piastres"], 300_000);
    assert_eq!(mine["base_salary_piastres"], 300_000);
}

#[sqlx::test]
async fn approving_a_correction_rewrites_the_punch_and_reprices(pool: PgPool) {
    // The whole point of a correction: the clock missed the check-out, the
    // employee proposes one, and approving it makes the record — and the money —
    // read as if the punch had been there all along.
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;

    let record: Uuid = sqlx::query_scalar(
        "INSERT INTO attendance_records \
             (org_id, employee_id, branch_id, business_date, status, check_in_at, \
              scheduled_start_at, scheduled_end_at) \
         VALUES ($1, $2, $3, '2026-09-08', 'present', '2026-09-08T09:00:00Z', \
                 '2026-09-08T09:00:00Z', '2026-09-08T17:00:00Z') RETURNING id",
    )
    .bind(f.org)
    .bind(f.employee)
    .bind(f.branch)
    .fetch_one(&pool)
    .await
    .unwrap();

    let employee_token = phone_token(&pool, f.employee).await;
    let req = test::TestRequest::post()
        .uri("/staff/me/requests")
        .auth(&employee_token)
        .set_json(serde_json::json!({
            "kind": "correction",
            "on_date": "2026-09-08",
            "to_time": "17:00:00",
            "attendance_record_id": record,
            "reason": "Forgot to clock out",
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201, "the employee should be able to file it");
    let filed: serde_json::Value = test::read_body_json(resp).await;

    // Still unapplied while it is pending — a request is not a fact.
    let out: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT check_out_at FROM attendance_records WHERE id = $1")
            .bind(record)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        out.is_none(),
        "a pending correction must not touch the record"
    );

    let admin_token = token_for(f.admin, f.org, UserRole::OrgAdmin);
    let req = test::TestRequest::patch()
        .uri(&format!(
            "/staff/requests/{}/decision",
            filed["id"].as_str().unwrap()
        ))
        .auth(&admin_token)
        .set_json(serde_json::json!({ "status": "approved" }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 200);

    let (out, worked): (Option<chrono::DateTime<chrono::Utc>>, i32) =
        sqlx::query_as("SELECT check_out_at, worked_minutes FROM attendance_records WHERE id = $1")
            .bind(record)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        out.map(|t| t.to_rfc3339()),
        Some("2026-09-08T17:00:00+00:00".to_string()),
        "approving should write the proposed punch"
    );
    assert_eq!(worked, 480, "and reprice the day from it");
}

#[sqlx::test]
async fn a_correction_against_someone_elses_record_is_refused(pool: PgPool) {
    // Without this check, filing a correction against a colleague's punch and
    // getting it waved through would rewrite THEIR pay.
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let admin_employee = seed_profile(&pool, f.org, f.admin, 300_000).await;

    let someone_elses: Uuid = sqlx::query_scalar(
        "INSERT INTO attendance_records \
             (org_id, employee_id, branch_id, business_date, status, check_in_at) \
         VALUES ($1, $2, $3, '2026-09-08', 'present', '2026-09-08T09:00:00Z') RETURNING id",
    )
    .bind(f.org)
    .bind(admin_employee)
    .bind(f.branch)
    .fetch_one(&pool)
    .await
    .unwrap();

    let employee_token = phone_token(&pool, f.employee).await;
    let req = test::TestRequest::post()
        .uri("/staff/me/requests")
        .auth(&employee_token)
        .set_json(serde_json::json!({
            "kind": "correction",
            "on_date": "2026-09-08",
            "to_time": "17:00:00",
            "attendance_record_id": someone_elses,
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        404,
        "another employee's record must not be correctable"
    );
}

#[sqlx::test]
async fn a_correction_never_forgives_the_day_it_corrects(pool: PgPool) {
    // Corrections are excluded from `day_adjustments` on purpose. If they were
    // not, an approved correction would read as an excused window and waive the
    // very lateness it just recorded.
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;

    let record: Uuid = sqlx::query_scalar(
        "INSERT INTO attendance_records \
             (org_id, employee_id, branch_id, business_date, status, check_in_at, \
              scheduled_start_at, scheduled_end_at) \
         VALUES ($1, $2, $3, '2026-09-08', 'late', '2026-09-08T10:00:00Z', \
                 '2026-09-08T09:00:00Z', '2026-09-08T17:00:00Z') RETURNING id",
    )
    .bind(f.org)
    .bind(f.employee)
    .bind(f.branch)
    .fetch_one(&pool)
    .await
    .unwrap();

    let employee_token = phone_token(&pool, f.employee).await;
    let req = test::TestRequest::post()
        .uri("/staff/me/requests")
        .auth(&employee_token)
        .set_json(serde_json::json!({
            "kind": "correction",
            "on_date": "2026-09-08",
            "to_time": "17:00:00",
            "attendance_record_id": record,
        }))
        .to_request();
    let filed: serde_json::Value = test::read_body_json(test::call_service(&app, req).await).await;

    let admin_token = token_for(f.admin, f.org, UserRole::OrgAdmin);
    let req = test::TestRequest::patch()
        .uri(&format!(
            "/staff/requests/{}/decision",
            filed["id"].as_str().unwrap()
        ))
        .auth(&admin_token)
        .set_json(serde_json::json!({ "status": "approved" }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 200);

    let late: i32 = sqlx::query_scalar("SELECT late_minutes FROM attendance_records WHERE id = $1")
        .bind(record)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        late > 0,
        "the hour of lateness must survive the correction, not be forgiven by it"
    );
}

#[sqlx::test]
async fn preview_matches_what_generating_produces(pool: PgPool) {
    // The preview's whole value is that it is TRUE. If it could drift from the
    // generator, a manager would be approving a number that never gets paid.
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    sqlx::query(
        "INSERT INTO payroll_deductions (org_id, employee_id, amount_piastres, reason, \
             effective_date, source, status) \
         VALUES ($1, $2, 25000, 'Late', '2026-09-08', 'late_penalty', 'approved')",
    )
    .bind(f.org)
    .bind(f.employee)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO salary_advances (org_id, employee_id, amount_piastres, installments, \
             monthly_installment_piastres, remaining_piastres, status) \
         VALUES ($1, $2, 100000, 2, 50000, 100000, 'approved')",
    )
    .bind(f.org)
    .bind(f.employee)
    .execute(&pool)
    .await
    .unwrap();

    let period = seed_period(&pool, f.org).await;
    let token = token_for(f.admin, f.org, UserRole::OrgAdmin);

    let req = test::TestRequest::get()
        .uri(&format!("/staff/payroll/periods/{period}/preview"))
        .auth(&token)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let preview: serde_json::Value = test::read_body_json(resp).await;
    let previewed = preview
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["employee_id"] == f.employee.to_string())
        .expect("the employee should appear in the preview")
        .clone();

    // Previewing must not collect anything.
    let remaining: i64 =
        sqlx::query_scalar("SELECT remaining_piastres FROM salary_advances WHERE employee_id = $1")
            .bind(f.employee)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(remaining, 100_000, "a preview must not touch the advances");
    let slip_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM payslips WHERE payroll_period_id = $1")
            .bind(period)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(slip_count, 0, "a preview must not write payslips");

    let resp = generate(&app, &token, period).await;
    assert_eq!(resp.status(), 200);
    let slips: serde_json::Value = test::read_body_json(resp).await;
    let actual = slips
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["employee_id"] == f.employee.to_string())
        .unwrap()
        .clone();

    for field in [
        "net_piastres",
        "overtime_piastres",
        "bonuses_piastres",
        "deductions_piastres",
        "advance_installment_piastres",
        "late_minutes",
        "overtime_minutes",
    ] {
        assert_eq!(
            previewed[field], actual[field],
            "preview and generate disagree on {field}"
        );
    }
    assert_eq!(previewed["net_piastres"], 225_000);
}

#[sqlx::test]
async fn generating_collects_an_advance_installment(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    sqlx::query(
        "INSERT INTO salary_advances (org_id, employee_id, amount_piastres, installments, \
             monthly_installment_piastres, remaining_piastres, status) \
         VALUES ($1, $2, 100000, 2, 50000, 100000, 'approved')",
    )
    .bind(f.org)
    .bind(f.employee)
    .execute(&pool)
    .await
    .unwrap();

    let period = seed_period(&pool, f.org).await;
    let token = token_for(f.admin, f.org, UserRole::OrgAdmin);
    generate(&app, &token, period).await;

    let remaining: i64 =
        sqlx::query_scalar("SELECT remaining_piastres FROM salary_advances WHERE employee_id = $1")
            .bind(f.employee)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(remaining, 50_000, "one installment should have been taken");

    let net: i64 = sqlx::query_scalar("SELECT net_piastres FROM payslips WHERE employee_id = $1")
        .bind(f.employee)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(net, 250_000);
}

#[sqlx::test]
async fn regenerating_refunds_before_recollecting(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    sqlx::query(
        "INSERT INTO salary_advances (org_id, employee_id, amount_piastres, installments, \
             monthly_installment_piastres, remaining_piastres, status) \
         VALUES ($1, $2, 100000, 2, 50000, 100000, 'approved')",
    )
    .bind(f.org)
    .bind(f.employee)
    .execute(&pool)
    .await
    .unwrap();

    let period = seed_period(&pool, f.org).await;
    let token = token_for(f.admin, f.org, UserRole::OrgAdmin);

    generate(&app, &token, period).await;
    generate(&app, &token, period).await;
    generate(&app, &token, period).await;

    let remaining: i64 =
        sqlx::query_scalar("SELECT remaining_piastres FROM salary_advances WHERE employee_id = $1")
            .bind(f.employee)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        remaining, 50_000,
        "three generations of one period must still collect exactly one installment"
    );

    let slips: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payslips WHERE employee_id = $1")
        .bind(f.employee)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(slips, 1, "regeneration replaces rather than appends");
}

#[sqlx::test]
async fn a_paid_period_cannot_be_regenerated(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let period = seed_period(&pool, f.org).await;
    let token = token_for(f.admin, f.org, UserRole::OrgAdmin);

    generate(&app, &token, period).await;
    // Paid means every payslip marked paid by hand (PAY-7); `status: paid`
    // by hand is refused.
    let req = test::TestRequest::patch()
        .uri(&format!("/staff/payroll/periods/{period}/status"))
        .auth(&token)
        .set_json(&json!({ "status": "paid" }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 409);
    let req = test::TestRequest::patch()
        .uri(&format!(
            "/staff/payroll/periods/{period}/payslips/{}/paid",
            f.employee
        ))
        .auth(&token)
        .set_json(&json!({ "method": "cash" }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 200);

    assert_eq!(
        generate(&app, &token, period).await.status(),
        409,
        "the payslips are what was paid — they are not recomputable"
    );
}

#[sqlx::test]
async fn a_paid_period_cannot_go_back_to_draft(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let period = seed_period(&pool, f.org).await;
    let token = token_for(f.admin, f.org, UserRole::OrgAdmin);

    // Approved = generated, paid = everyone marked paid: neither by hand.
    let uri = format!("/staff/payroll/periods/{period}/status");
    assert_eq!(
        auth_send!(app, patch, uri, token, json!({ "status": "generated" })).status(),
        409
    );
    assert_eq!(generate(&app, &token, period).await.status(), 200);
    assert_eq!(
        auth_send!(app, patch, uri, token, json!({ "status": "paid" })).status(),
        409
    );
    let paid = format!(
        "/staff/payroll/periods/{period}/payslips/{}/paid",
        f.employee
    );
    assert_eq!(
        auth_send!(app, patch, paid, token, json!({ "method": "cash" })).status(),
        200
    );
    assert_eq!(
        auth_send!(
            app,
            patch,
            uri,
            token,
            json!({ "status": "draft", "reason": "r" })
        )
        .status(),
        409
    );
}

#[sqlx::test]
async fn unpaid_leave_docks_pay_but_paid_leave_does_not(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let unpaid = seed_leave_type(&pool, f.org, false).await;
    // Five approved unpaid days inside the period, each with the `on_leave`
    // attendance row the nightly sweep would have written.
    sqlx::query(
        "INSERT INTO staff_requests (org_id, employee_id, kind, leave_type_id, on_date, end_date, \
             status, decided_at) \
         VALUES ($1, $2, 'leave', $3, '2026-09-01', '2026-09-05', 'approved', now())",
    )
    .bind(f.org)
    .bind(f.employee)
    .bind(unpaid)
    .execute(&pool)
    .await
    .unwrap();

    for day in 1..=5 {
        let record: Uuid = sqlx::query_scalar(
            "INSERT INTO attendance_records (org_id, employee_id, branch_id, business_date, status) \
             VALUES ($1, $2, $3, make_date(2026, 9, $4), 'on_leave') RETURNING id",
        )
        .bind(f.org)
        .bind(f.employee)
        .bind(f.branch)
        .bind(day)
        .fetch_one(&pool)
        .await
        .unwrap();

        let settings = madar_rust::staff::attendance::load_settings(&pool, f.org, None)
            .await
            .unwrap();
        madar_rust::staff::penalties::recompute_record(&pool, record, &settings)
            .await
            .unwrap();
    }

    let period = seed_period(&pool, f.org).await;
    let token = token_for(f.admin, f.org, UserRole::OrgAdmin);
    let resp = generate(&app, &token, period).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    assert_eq!(
        status,
        200,
        "generate failed: {}",
        String::from_utf8_lossy(&body)
    );

    let net: i64 = sqlx::query_scalar("SELECT net_piastres FROM payslips WHERE employee_id = $1")
        .bind(f.employee)
        .fetch_one(&pool)
        .await
        .unwrap();
    // 300,000 / 30 days = 10,000 per day; five unpaid days = 50,000 — and now it
    // arrives as five visible, waivable deduction rows rather than a hidden
    // subtraction inside the net calculation.
    assert_eq!(net, 250_000);
}

#[sqlx::test]
async fn a_percentage_bonus_resolves_against_base_salary(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    sqlx::query(
        "INSERT INTO payroll_bonuses (org_id, employee_id, percent_of_base, reason, effective_date) \
         VALUES ($1, $2, 10, 'Performance', '2026-09-15')",
    )
    .bind(f.org)
    .bind(f.employee)
    .execute(&pool)
    .await
    .unwrap();

    let period = seed_period(&pool, f.org).await;
    let token = token_for(f.admin, f.org, UserRole::OrgAdmin);
    generate(&app, &token, period).await;

    let (bonus, net): (i64, i64) = sqlx::query_as(
        "SELECT bonuses_piastres, net_piastres FROM payslips WHERE employee_id = $1",
    )
    .bind(f.employee)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(bonus, 30_000);
    assert_eq!(net, 330_000);
}

#[sqlx::test]
async fn an_employee_sees_only_their_own_payslips(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &stable_zone()).await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let other = seed_user(&pool, f.org, "Other", UserRole::Teller).await;
    seed_profile(&pool, f.org, other, 900_000).await;

    let period = seed_period(&pool, f.org).await;
    let admin_token = token_for(f.admin, f.org, UserRole::OrgAdmin);
    generate(&app, &admin_token, period).await;

    let employee_token = phone_token(&pool, f.employee).await;
    let resp = auth_get!(app, "/staff/me/payslips", employee_token);
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    let slips = body.as_array().unwrap();
    assert_eq!(slips.len(), 1);
    assert_eq!(slips[0]["employee_id"], f.employee.to_string());
}

// ── Requests suppress penalties ───────────────────────────────

/// A shift that started `minutes_ago`, rostered every day, with `grace` minutes
/// of tolerance — the setup every penalty test needs.
async fn seed_late_setup(pool: &PgPool, f: &Fixture, minutes_ago: i64, grace: i32) -> Uuid {
    seed_profile(pool, f.org, f.employee, 300_000).await;
    let shift = seed_shift(
        pool,
        f.org,
        f.branch,
        "Day",
        utc_time_offset(-minutes_ago),
        utc_time_offset(480 - minutes_ago),
        grace,
    )
    .await;
    roster(pool, f.org, f.employee, shift, None).await;
    shift
}

/// The user's rung: "31–120 minutes late costs half a day's pay."
async fn seed_late_ladder(pool: &PgPool, org: Uuid) {
    sqlx::query(
        r#"INSERT INTO attendance_settings (org_id, late_deduction_tiers)
           VALUES ($1, '[{"from_minutes":1,"to_minutes":30,"kind":"minutes","value":30},
                         {"from_minutes":31,"to_minutes":120,"kind":"day_fraction","value":0.5}]'::jsonb)
           ON CONFLICT (org_id, COALESCE(branch_id, '00000000-0000-0000-0000-000000000000'::uuid))
           DO UPDATE SET late_deduction_tiers = EXCLUDED.late_deduction_tiers"#,
    )
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
}

async fn check_out(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    token: &str,
) -> actix_web::dev::ServiceResponse {
    let req = test::TestRequest::post()
        .uri("/staff/me/check-out")
        .auth(&token)
        .set_json(&json!({ "latitude": BRANCH_LAT, "longitude": BRANCH_LNG }))
        .to_request();
    test::call_service(app, req).await
}

#[sqlx::test]
async fn a_late_arrival_is_priced_by_the_ladder_at_check_out(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &stable_zone()).await;
    seed_late_ladder(&pool, f.org).await;
    // 60 minutes past a 15-minute grace = 45 late → the half-day rung.
    seed_late_setup(&pool, &f, 60, 15).await;
    let token = phone_token(&pool, f.employee).await;

    check_in(&app, &token, f.branch, BRANCH_LAT, BRANCH_LNG).await;
    check_out(&app, &token).await;

    let (amount, source, original): (i64, String, Option<i64>) = sqlx::query_as(
        "SELECT amount_piastres, source, original_amount_piastres            FROM payroll_deductions WHERE employee_id = $1",
    )
    .bind(f.employee)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(source, "late_penalty");
    // 300,000 / 30 days = 10,000 a day; half of that.
    assert_eq!(amount, 5_000, "the 31–120 rung should dock half a day");
    assert_eq!(
        original,
        Some(5_000),
        "what the rule computed is recorded from the start"
    );
}

#[sqlx::test]
async fn an_approved_late_arrival_means_there_is_no_penalty_to_waive(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &stable_zone()).await;
    seed_late_ladder(&pool, f.org).await;
    seed_late_setup(&pool, &f, 60, 15).await;

    // Permission to arrive 5 minutes from now — granted before the punch.
    sqlx::query(
        "INSERT INTO staff_requests (org_id, employee_id, kind, on_date, to_time, status, decided_at)          VALUES ($1, $2, 'late_arrival', $4, $3, 'approved', now())",
    )
    .bind(f.org)
    .bind(f.employee)
    .bind(utc_time_offset(5))
    // The DAY the punch belongs to, derived beside the time (see
    // `stable_zone_at`) — not "today in UTC", which is a different day for a
    // run either side of midnight.
    .bind(local_date_offset(0))
    .execute(&pool)
    .await
    .unwrap();

    let token = phone_token(&pool, f.employee).await;
    check_in(&app, &token, f.branch, BRANCH_LAT, BRANCH_LNG).await;
    check_out(&app, &token).await;

    let penalties: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM payroll_deductions WHERE employee_id = $1 AND source = 'late_penalty'",
    )
    .bind(f.employee)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        penalties, 0,
        "an approved request removes the penalty at its source — there should be          nothing to argue about afterwards"
    );
}

#[sqlx::test]
async fn an_approved_early_departure_shortens_the_day_that_was_owed(pool: PgPool) {
    // Controlled timestamps, because the point is what a SHORTENED-but-worked day
    // classifies as — not what an instant in-and-out does.
    use chrono::TimeZone;
    use madar_rust::staff::attendance::{DayAdjustments, TimedRequest, derive};

    let at = |h: u32, m: u32| Utc.with_ymd_and_hms(2026, 8, 10, h, m, 0).unwrap();
    // Rostered 09:00–17:00; permission to leave at 13:00; actually left at 13:00.
    let approved = derive(
        Some(at(9, 0)),
        Some(at(13, 0)),
        Some(at(9, 0)),
        Some(at(17, 0)),
        None,
        &DayAdjustments {
            early_departures: vec![TimedRequest {
                candidates: vec![at(13, 0)],
                work_shift_id: None,
                paid: true,
            }],
            ..Default::default()
        },
    );
    assert_eq!(
        approved.early_leave_minutes, 0,
        "leaving at the agreed time is not leaving early"
    );

    // The same day WITHOUT permission: four hours short of an eight-hour shift.
    let unapproved = derive(
        Some(at(9, 0)),
        Some(at(13, 0)),
        Some(at(9, 0)),
        Some(at(17, 0)),
        None,
        &DayAdjustments::default(),
    );
    assert_eq!(
        unapproved.early_leave_minutes, 240,
        "without permission the four missing hours are still early"
    );
    let _ = pool;
}

#[sqlx::test]
async fn a_paid_excuse_credits_the_time_and_an_unpaid_one_does_not(pool: PgPool) {
    // The pure shape of the rule, without the clock: an excused window inside the
    // attendance span is credited when paid and ignored when not.
    use chrono::TimeZone;
    use madar_rust::staff::attendance::{DayAdjustments, WindowRequest, derive};

    let at = |h: u32, m: u32| Utc.with_ymd_and_hms(2026, 8, 10, h, m, 0).unwrap();
    let excuse = |paid: bool| DayAdjustments {
        excuses: vec![WindowRequest {
            candidates: vec![(at(12, 0), at(14, 0))],
            work_shift_id: None,
            paid,
        }],
        ..Default::default()
    };

    let paid = derive(
        Some(at(9, 0)),
        Some(at(17, 0)),
        Some(at(9, 0)),
        Some(at(17, 0)),
        None,
        &excuse(true),
    );
    let unpaid = derive(
        Some(at(9, 0)),
        Some(at(17, 0)),
        Some(at(9, 0)),
        Some(at(17, 0)),
        None,
        &excuse(false),
    );

    assert_eq!(unpaid.worked_minutes, 480, "the clocked span, unchanged");
    assert_eq!(
        paid.worked_minutes, 600,
        "a paid excuse credits the two hours back"
    );
    let _ = pool;
}

// ── Overriding an automatic deduction ─────────────────────────

#[sqlx::test]
async fn a_waived_penalty_survives_the_nightly_sweep(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, &stable_zone()).await;
    seed_late_ladder(&pool, f.org).await;
    seed_late_setup(&pool, &f, 60, 15).await;
    let employee_token = phone_token(&pool, f.employee).await;
    let admin_token = token_for(f.admin, f.org, UserRole::OrgAdmin);

    check_in(&app, &employee_token, f.branch, BRANCH_LAT, BRANCH_LNG).await;
    check_out(&app, &employee_token).await;

    let id: Uuid = sqlx::query_scalar("SELECT id FROM payroll_deductions WHERE employee_id = $1")
        .bind(f.employee)
        .fetch_one(&pool)
        .await
        .unwrap();

    let uri = format!("/staff/payroll/deductions/{id}/waive");
    assert_eq!(
        auth_send!(
            app,
            patch,
            uri,
            admin_token,
            json!({ "reason": "Traffic — agreed" })
        )
        .status(),
        200
    );

    // Recomputing is exactly what the sweep does. The waiver must hold.
    let settings = madar_rust::staff::attendance::load_settings(&pool, f.org, None)
        .await
        .unwrap();
    let record: Uuid =
        sqlx::query_scalar("SELECT id FROM attendance_records WHERE employee_id = $1")
            .bind(f.employee)
            .fetch_one(&pool)
            .await
            .unwrap();
    madar_rust::staff::penalties::recompute_record(&pool, record, &settings)
        .await
        .unwrap();

    let (waived, reason): (Option<chrono::DateTime<Utc>>, Option<String>) =
        sqlx::query_as("SELECT waived_at, waive_reason FROM payroll_deductions WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        waived.is_some(),
        "the sweep must never undo a manager's waiver — they would believe it held"
    );
    assert_eq!(reason.as_deref(), Some("Traffic — agreed"));
}

#[sqlx::test]
async fn a_waived_deduction_does_not_reach_the_payslip(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let admin_token = token_for(f.admin, f.org, UserRole::OrgAdmin);

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO payroll_deductions (org_id, employee_id, amount_piastres, reason,              effective_date, source, status)          VALUES ($1, $2, 40000, 'Manual', '2026-09-10', 'manual', 'approved') RETURNING id",
    )
    .bind(f.org)
    .bind(f.employee)
    .fetch_one(&pool)
    .await
    .unwrap();

    let period = seed_period(&pool, f.org).await;
    generate(&app, &admin_token, period).await;
    let before: i64 =
        sqlx::query_scalar("SELECT net_piastres FROM payslips WHERE employee_id = $1")
            .bind(f.employee)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, 260_000);

    let uri = format!("/staff/payroll/deductions/{id}/waive");
    // The month is approved: reopen it first (a reason is kept), the waiver
    // lands, then approve again (PAY-6).
    let resp = auth_send!(
        app,
        patch,
        format!("/staff/payroll/periods/{period}/status"),
        admin_token,
        json!({ "status": "draft", "reason": "a waiver" })
    );
    assert_eq!(resp.status(), 200);
    let resp = auth_send!(
        app,
        patch,
        uri,
        admin_token,
        json!({ "reason": "Reversed" })
    );
    assert_eq!(resp.status(), 200);
    generate(&app, &admin_token, period).await;

    let after: i64 = sqlx::query_scalar("SELECT net_piastres FROM payslips WHERE employee_id = $1")
        .bind(f.employee)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        after, 300_000,
        "waiving must move the payslip by exactly the waived amount"
    );
}

#[sqlx::test]
async fn overriding_keeps_the_original_and_payroll_uses_the_new_amount(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let admin_token = token_for(f.admin, f.org, UserRole::OrgAdmin);

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO payroll_deductions (org_id, employee_id, amount_piastres,              original_amount_piastres, reason, effective_date, source, status)          VALUES ($1, $2, 50000, 50000, 'Late', '2026-09-10', 'late_penalty', 'approved')          RETURNING id",
    )
    .bind(f.org)
    .bind(f.employee)
    .fetch_one(&pool)
    .await
    .unwrap();

    let uri = format!("/staff/payroll/deductions/{id}/override");
    assert_eq!(
        auth_send!(
            app,
            patch,
            uri,
            admin_token,
            json!({ "amount_piastres": 10000, "reason": "First offence" })
        )
        .status(),
        200
    );

    let (amount, original): (i64, Option<i64>) = sqlx::query_as(
        "SELECT amount_piastres, original_amount_piastres FROM payroll_deductions WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(amount, 10_000);
    assert_eq!(
        original,
        Some(50_000),
        "the figure the rule computed must survive the override"
    );

    let period = seed_period(&pool, f.org).await;
    generate(&app, &admin_token, period).await;
    let net: i64 = sqlx::query_scalar("SELECT net_piastres FROM payslips WHERE employee_id = $1")
        .bind(f.employee)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(net, 290_000, "payroll charges the overridden amount");
}

#[sqlx::test]
async fn an_override_needs_a_reason(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let admin_token = token_for(f.admin, f.org, UserRole::OrgAdmin);

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO payroll_deductions (org_id, employee_id, amount_piastres, reason,              effective_date, source, status)          VALUES ($1, $2, 5000, 'Late', '2026-09-10', 'late_penalty', 'approved') RETURNING id",
    )
    .bind(f.org)
    .bind(f.employee)
    .fetch_one(&pool)
    .await
    .unwrap();

    let uri = format!("/staff/payroll/deductions/{id}/override");
    assert_eq!(
        auth_send!(
            app,
            patch,
            uri,
            admin_token,
            json!({ "amount_piastres": 0, "reason": "  " })
        )
        .status(),
        400,
        "an unexplained override is indistinguishable from a mistake later"
    );
}

// ── Request shapes ────────────────────────────────────────────

#[sqlx::test]
async fn each_request_kind_rejects_a_malformed_shape(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let token = phone_token(&pool, f.employee).await;
    let uri = "/staff/me/requests".to_string();

    // Each of these is the kind's REQUIRED field, missing.
    for body in [
        json!({ "kind": "late_arrival",    "on_date": "2026-09-01" }),
        json!({ "kind": "early_departure", "on_date": "2026-09-01" }),
        json!({ "kind": "excuse",          "on_date": "2026-09-01", "from_time": "12:00:00" }),
        json!({ "kind": "mission",         "on_date": "2026-09-01" }),
        // Leave needs nothing more: it has no types (Dawam RQ-2).
    ] {
        let kind = body["kind"].as_str().unwrap().to_string();
        assert_eq!(
            auth_send!(app, post, uri, token, body).status(),
            400,
            "a malformed {kind} must never reach the classifier"
        );
    }

    // And an excuse whose window is empty.
    assert_eq!(
        auth_send!(
            app,
            post,
            uri,
            token,
            json!({ "kind": "excuse", "on_date": "2026-09-01",
                    "from_time": "14:00:00", "to_time": "14:00:00" })
        )
        .status(),
        400
    );
    // One that ends earlier on the clock runs past midnight (a night shift,
    // B5): its end is on the next day.
    let resp = auth_send!(
        app,
        post,
        uri,
        token,
        json!({ "kind": "excuse", "on_date": "2026-09-01",
                "from_time": "23:00:00", "to_time": "01:00:00" })
    );
    assert_eq!(resp.status(), 201);
    let row: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(row["end_date"], "2026-09-02");
    // A mission takes its note as the title when it has none (§3).
    let resp = auth_send!(
        app,
        post,
        uri,
        token,
        json!({ "kind": "mission", "on_date": "2026-09-03", "reason": "Supplier visit" })
    );
    assert_eq!(resp.status(), 201);
    let row: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(row["title"], "Supplier visit");
}

#[sqlx::test]
async fn one_live_request_per_kind_per_day(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let token = phone_token(&pool, f.employee).await;
    let uri = "/staff/me/requests".to_string();
    let body = json!({ "kind": "late_arrival", "on_date": "2026-09-01", "to_time": "10:00:00" });

    assert_eq!(auth_send!(app, post, uri, token, body).status(), 201);
    assert_eq!(
        auth_send!(app, post, uri, token, body).status(),
        409,
        "two approved arrival times for one morning would be ambiguous"
    );
}

// ── Permissions ───────────────────────────────────────────────

#[sqlx::test]
async fn a_teller_cannot_read_the_employee_directory(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let token = phone_token(&pool, f.employee).await;

    assert_eq!(
        auth_get!(app, "/staff/employees", token).status(),
        403,
        "self-service must not imply access to everyone else's records"
    );
}

#[sqlx::test]
async fn self_service_needs_no_permission_grant(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let token = phone_token(&pool, f.employee).await;

    // A teller has no `attendance` grant at all, yet must be able to see their
    // own day — that is the whole point of the /me surface.
    assert_eq!(auth_get!(app, "/staff/me/today", token).status(), 200);
}

#[sqlx::test]
async fn attendance_coordinates_are_wiped_once_their_month_is_approved(pool: PgPool) {
    // AT-4: exact coordinates go when the payroll month they belong to is
    // approved — not by age. Attendance TIMES are payroll evidence and must
    // survive, and so must the geofence RESULT (distance in metres): it is
    // the auditable fact, and unlike a latitude/longitude it does not record
    // where the employee actually was. A month nobody approved yet keeps its
    // coordinates however old it is.
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;

    let insert = |day: &'static str| {
        let pool = pool.clone();
        let (org, user, branch) = (f.org, f.employee, f.branch);
        async move {
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO attendance_records \
                     (org_id, employee_id, branch_id, business_date, status, check_in_at, \
                      check_in_latitude, check_in_longitude, check_in_distance_meters, \
                      check_in_method, check_out_at, check_out_latitude, check_out_longitude) \
                 VALUES ($1, $2, $3, $4::date, 'present', now(), \
                         30.0444, 31.2357, 12.5, 'mobile_gps', now(), 30.0445, 31.2358) \
                 RETURNING id",
            )
            .bind(org)
            .bind(user)
            .bind(branch)
            .bind(day)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };

    let approved_record = insert("2026-06-10").await;
    let open_record = insert("2026-05-10").await; // older, but never approved
    sqlx::query(
        "INSERT INTO payroll_periods (org_id, name, start_date, end_date, status) \
         VALUES ($1, 'June', '2026-06-01', '2026-06-30', 'generated'), \
                ($1, 'May', '2026-05-01', '2026-05-31', 'draft')",
    )
    .bind(f.org)
    .execute(&pool)
    .await
    .unwrap();

    madar_rust::staff::jobs::purge_stale_coordinates(&pool)
        .await
        .expect("the wipe should succeed");

    // The approved month keeps everything payroll needs, minus the coordinates.
    let (lat, lng, out_lat, out_lng, checked_in, distance, method): (
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<f64>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT check_in_latitude, check_in_longitude, check_out_latitude, \
                check_out_longitude, check_in_at, check_in_distance_meters, check_in_method \
           FROM attendance_records WHERE id = $1",
    )
    .bind(approved_record)
    .fetch_one(&pool)
    .await
    .unwrap();

    assert!(
        lat.is_none() && lng.is_none(),
        "check-in coordinates should be gone"
    );
    assert!(
        out_lat.is_none() && out_lng.is_none(),
        "check-out coordinates should be gone too"
    );
    assert!(
        checked_in.is_some(),
        "the punch TIME is payroll evidence and must survive"
    );
    assert_eq!(
        distance,
        Some(12.5),
        "the geofence result must survive — it is the auditable fact"
    );
    assert_eq!(
        method.as_deref(),
        Some("mobile_gps"),
        "how the punch was made must survive"
    );

    // The month nobody approved keeps its coordinates.
    let (lat, lng): (Option<f64>, Option<f64>) = sqlx::query_as(
        "SELECT check_in_latitude, check_in_longitude FROM attendance_records WHERE id = $1",
    )
    .bind(open_record)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (lat, lng),
        (Some(30.0444), Some(31.2357)),
        "a month not yet approved must keep its coordinates"
    );

    // Idempotent: re-running finds nothing left to do and must not error.
    madar_rust::staff::jobs::purge_stale_coordinates(&pool)
        .await
        .expect("a second pass should be a harmless no-op");
}

/// Discipline ranks by absences, then lates, then late minutes — within a
/// department, ties sharing a rank.
#[sqlx::test]
async fn discipline_report_ranks_absences_before_lates(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    let punctual_but_late =
        common::employees::employee(&pool, f.org, "Late", None, None, false, &[f.branch], 0).await;
    for (user, day, status, late) in [
        (f.employee, 1, "absent", 0),
        (punctual_but_late, 1, "late", 10),
        (punctual_but_late, 2, "late", 5),
        (punctual_but_late, 3, "present", 0),
    ] {
        sqlx::query(
            "INSERT INTO attendance_records (org_id, employee_id, branch_id, business_date, status, late_minutes) \
             VALUES ($1, $2, $3, make_date(2026, 9, $4), $5, $6)",
        )
        .bind(f.org)
        .bind(user)
        .bind(f.branch)
        .bind(day)
        .bind(status)
        .bind(late)
        .execute(&pool)
        .await
        .unwrap();
    }
    let token = token_for(f.admin, f.org, UserRole::OrgAdmin);
    let resp = auth_get!(
        app,
        "/staff/discipline-report?from=2026-09-01&to=2026-09-30",
        token
    );
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    let rows = body["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["employee_id"], serde_json::json!(punctual_but_late));
    assert_eq!(rows[0]["rank_in_department"], 1);
    assert_eq!(rows[0]["late_days"], 2);
    assert_eq!(rows[0]["present_days"], 1);
    assert_eq!(rows[0]["total_late_minutes"], 15);
    assert_eq!(rows[1]["absent_days"], 1);
    assert_eq!(rows[1]["rank_in_department"], 2);
}

/// Architecture E: the discipline report is `hr.attendance.read`, and a branch
/// manager ranks only the branches they work at, never the whole org. A person
/// without the capability is refused, and so is a manager naming a branch they
/// do not work at.
#[sqlx::test]
async fn discipline_report_is_scoped_to_the_callers_branches(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    // Every role's default cells, as a real org has them.
    madar_rust::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();
    let other = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO branches (id, org_id, name, timezone) VALUES ($1, $2, 'Other', 'UTC'::timezone_name)",
    )
    .bind(other)
    .bind(f.org)
    .execute(&pool)
    .await
    .unwrap();
    let manager = seed_user(&pool, f.org, "Manager", UserRole::BranchManager).await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(manager)
        .bind(f.branch)
        .execute(&pool)
        .await
        .unwrap();
    let elsewhere =
        common::employees::employee(&pool, f.org, "Elsewhere", None, None, false, &[other], 0)
            .await;
    for (user, branch) in [(f.employee, f.branch), (elsewhere, other)] {
        sqlx::query(
            "INSERT INTO attendance_records (org_id, employee_id, branch_id, business_date, status, late_minutes) \
             VALUES ($1, $2, $3, make_date(2026, 9, 1), 'late', 5)",
        )
        .bind(f.org)
        .bind(user)
        .bind(branch)
        .execute(&pool)
        .await
        .unwrap();
    }
    let uri = "/staff/discipline-report?from=2026-09-01&to=2026-09-30";

    let owner = token_for(f.admin, f.org, UserRole::OrgAdmin);
    let resp = auth_get!(app, uri, owner);
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(
        body["rows"].as_array().unwrap().len(),
        2,
        "the owner sees every branch"
    );

    let mgr = token_for(manager, f.org, UserRole::BranchManager);
    let resp = auth_get!(app, uri, mgr);
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    let rows = body["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "a manager sees only their branch: {rows:?}");
    assert_eq!(rows[0]["employee_id"], serde_json::json!(f.employee));

    let resp = auth_get!(app, &format!("{uri}&branch_id={other}"), mgr);
    assert_eq!(resp.status(), 403, "a branch the manager does not work at");

    let teller = phone_token(&pool, f.employee).await;
    let resp = auth_get!(app, uri, teller);
    assert_eq!(resp.status(), 403, "a teller holds no hr.attendance.read");
}

/// E2E B-SETUP-1 (RU-8, AV-5, RU-13, AT-11): impossible rates, caps and limits
/// are refused at the door with a plain message and a code — never stored,
/// and never a raw "Database error" — and the row is unchanged.
#[sqlx::test]
async fn settings_refuse_impossible_rates_and_caps(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    let admin = token_for(f.admin, f.org, UserRole::OrgAdmin);
    let snapshot = || {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, serde_json::Value>(
                "SELECT to_jsonb(s) - 'updated_at' FROM attendance_settings s \
                  WHERE org_id = $1 AND branch_id IS NULL",
            )
            .bind(f.org)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let before = snapshot().await;
    for body in [
        json!({ "overtime_day_multiplier": 0 }),
        json!({ "overtime_day_multiplier": -1 }),
        json!({ "overtime_day_multiplier": 100 }),
        json!({ "overtime_night_multiplier": 0.5 }),
        json!({ "holiday_multiplier": 0 }),
        json!({ "default_overtime_multiplier": 100 }),
        json!({ "advance_cap_percent": 101 }),
        json!({ "advance_cap_percent": -1 }),
        json!({ "absence_deduction_days": -1 }),
        json!({ "absence_deduction_days": 32 }),
        json!({ "working_days_per_month": 40 }),
        json!({ "limit_day_hours": 200 }),
        json!({ "limit_day_hours": 0 }),
        json!({ "limit_week_hours": 169 }),
        json!({ "limit_presence_hours": -2 }),
        json!({ "limit_rest_hours": -1 }),
        json!({ "limit_overtime_day_hours": 169 }),
        json!({ "orders_per_staff": -3 }),
        json!({ "orders_per_staff": 0 }),
        json!({ "overtime_mode": "bogus" }),
        json!({ "half_day_leave_counts": "x" }),
        json!({ "period_start_day": 29 }),
    ] {
        let resp = auth_send!(app, put, "/staff/attendance/settings", admin, body);
        assert_eq!(resp.status(), 400, "{body}");
        let err: serde_json::Value = test::read_body_json(resp).await;
        let text = err["error"].as_str().unwrap_or_default();
        assert!(!text.contains("Database error"), "{body}: {err}");
        assert_eq!(err["code"], "SETTING_OUT_OF_RANGE", "{body}: {err}");
        let field = body.as_object().unwrap().keys().next().unwrap();
        assert_eq!(err["vars"]["field"], json!(field), "{body}: {err}");
    }
    assert_eq!(snapshot().await, before, "nothing was stored");
    // The edges themselves are fine.
    let resp = auth_send!(
        app,
        put,
        "/staff/attendance/settings",
        admin,
        json!({ "overtime_day_multiplier": 1, "holiday_multiplier": 99.99,
                "advance_cap_percent": 0, "limit_day_hours": 168, "limit_rest_hours": 0,
                "orders_per_staff": 1, "overtime_mode": "approval",
                "half_day_leave_counts": "whole_day" })
    );
    assert_eq!(resp.status(), 200);
}

/// E2E B-SETUP-2 (D-046, D-047, PAY-7): switching an employee to cash drops
/// the old bank account or wallet, `pay_account: null` clears it, and
/// `gender: null` is "Not set". An omitted field still keeps what is there.
#[sqlx::test]
async fn cash_clears_the_pay_account_and_gender_can_be_unset(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    let admin = token_for(f.admin, f.org, UserRole::OrgAdmin);
    let uri = format!("/staff/employees/{}", f.employee);
    let iban = "EG380019000500000000263180002";
    macro_rules! put {
        ($body:expr) => {{
            let resp = auth_send!(app, put, uri, admin, $body);
            assert_eq!(resp.status(), 200, "{}", $body);
        }};
    }
    let facts = || {
        let pool = pool.clone();
        let id = f.employee;
        async move {
            sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
                "SELECT pay_method, pay_account, gender FROM employees WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    put!(json!({ "pay_method": "bank", "pay_account": iban, "gender": "m" }));
    assert_eq!(
        facts().await,
        ("bank".into(), Some(iban.into()), Some("m".into()))
    );
    // Omitted: kept.
    put!(json!({ "job_title": "Barista" }));
    assert_eq!(
        facts().await,
        ("bank".into(), Some(iban.into()), Some("m".into()))
    );
    // Cash, as the dashboard sends it.
    put!(json!({ "pay_method": "cash", "pay_account": null }));
    assert_eq!(facts().await.1, None, "cash drops the account");
    // Cash with the account omitted, or even sent: still none.
    put!(json!({ "pay_method": "wallet", "pay_account": "01012345678" }));
    put!(json!({ "pay_method": "cash" }));
    assert_eq!(facts().await.1, None);
    put!(json!({ "pay_method": "cash", "pay_account": "stale" }));
    assert_eq!(facts().await.1, None);
    // An explicit null or empty account clears it on bank too.
    put!(json!({ "pay_method": "bank", "pay_account": iban }));
    put!(json!({ "pay_account": "" }));
    assert_eq!(facts().await.1, None);
    // Gender "Not set".
    put!(json!({ "gender": null }));
    assert_eq!(facts().await.2, None, "gender can be unset");
    put!(json!({ "gender": "f" }));
    put!(json!({ "notes": "x" }));
    assert_eq!(facts().await.2, Some("f".into()), "omitted keeps it");
    put!(json!({ "gender": "" }));
    assert_eq!(facts().await.2, None);
}
