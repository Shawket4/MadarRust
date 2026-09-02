//! Staff module integration tests.
//!
//! The pure math is covered by unit tests in [`crate::staff::rules`]; these
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

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn token_for(user_id: Uuid, org_id: Uuid, role: UserRole) -> String {
    crate::auth::jwt::create_token(&get_secret(), user_id, Some(org_id), role, None, 24).unwrap()
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(get_secret()))
                .configure(crate::staff::routes::configure),
        )
        .await
    };
}

macro_rules! auth_get {
    ($app:expr, $uri:expr, $token:expr) => {{
        let req = test::TestRequest::get()
            .uri($uri)
            .insert_header(("Authorization", format!("Bearer {}", $token)))
            .to_request();
        test::call_service(&$app, req).await
    }};
}

/// A body-carrying authenticated request. A macro rather than a helper closure
/// because a closure that borrows `app` across an `.await` can only be called
/// once, and every one of these tests fires the same request twice to prove an
/// operation is (or is not) repeatable.
macro_rules! auth_send {
    ($app:expr, $method:ident, $uri:expr, $token:expr, $body:expr) => {{
        let req = test::TestRequest::$method()
            .uri(&$uri)
            .insert_header(("Authorization", format!("Bearer {}", $token)))
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
    employee: Uuid,
}

/// Branch coordinates: the Giza pyramids, with a 200 m fence.
const BRANCH_LAT: f64 = 29.9792;
const BRANCH_LNG: f64 = 31.1342;

async fn seed(pool: &PgPool, timezone: &str) -> Fixture {
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Test Org', $2)")
        .bind(org)
        .bind(format!("org-{org}"))
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
    let employee = seed_user(pool, org, "Employee", UserRole::Teller).await;
    grant_admin_everything(pool).await;

    Fixture {
        org,
        branch,
        admin,
        employee,
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

async fn seed_profile(pool: &PgPool, org: Uuid, user: Uuid, salary_piastres: i64) {
    sqlx::query(
        "INSERT INTO staff_profiles (user_id, org_id, base_salary_piastres, hire_date) \
         VALUES ($1, $2, $3, CURRENT_DATE - 365)",
    )
    .bind(user)
    .bind(org)
    .bind(salary_piastres)
    .execute(pool)
    .await
    .unwrap();
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
        "INSERT INTO staff_schedules (org_id, user_id, work_shift_id, day_of_week, effective_from) \
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

/// Wall-clock time `minutes_ago` before now, as a `NaiveTime` in UTC. Used to
/// place a shift's start relative to the moment the test runs.
fn utc_time_offset(minutes: i64) -> NaiveTime {
    let t = (Utc::now() + Duration::minutes(minutes)).time();
    NaiveTime::from_hms_opt(t.hour(), t.minute(), 0).unwrap()
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
    let req = test::TestRequest::post()
        .uri("/staff/me/check-in")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(&json!({ "branch_id": branch, "latitude": lat, "longitude": lng }))
        .to_request();
    test::call_service(app, req).await
}

// ── Geofence ──────────────────────────────────────────────────

#[sqlx::test]
async fn check_in_inside_the_geofence_is_accepted(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
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
    let token = token_for(f.employee, f.org, UserRole::Teller);

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
    let f = seed(&pool, "UTC").await;
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
    let token = token_for(f.employee, f.org, UserRole::Teller);

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

#[sqlx::test]
async fn geofencing_can_be_turned_off_per_org(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
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
    sqlx::query("INSERT INTO attendance_settings (org_id, require_geofence) VALUES ($1, FALSE)")
        .bind(f.org)
        .execute(&pool)
        .await
        .unwrap();

    let token = token_for(f.employee, f.org, UserRole::Teller);
    let resp = check_in(&app, &token, f.branch, BRANCH_LAT + 0.01, BRANCH_LNG).await;
    assert_eq!(
        resp.status(),
        201,
        "with the fence off, distance should not block the punch"
    );
}

// ── Lateness ──────────────────────────────────────────────────

#[sqlx::test]
async fn arriving_within_grace_is_present_not_late(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
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
    let token = token_for(f.employee, f.org, UserRole::Teller);

    let resp = check_in(&app, &token, f.branch, BRANCH_LAT, BRANCH_LNG).await;
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["status"], "present");
    assert_eq!(body["late_minutes"], 0);
}

#[sqlx::test]
async fn arriving_past_grace_is_late_by_the_excess_only(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
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
    let token = token_for(f.employee, f.org, UserRole::Teller);

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
    let f = seed(&pool, "UTC").await;
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
        "INSERT INTO staff_requests (org_id, user_id, kind, on_date, to_time, status, decided_at) \
         VALUES ($1, $2, 'late_arrival', (now() AT TIME ZONE 'UTC')::date, $3, 'approved', now())",
    )
    .bind(f.org)
    .bind(f.employee)
    .bind(utc_time_offset(5))
    .execute(&pool)
    .await
    .unwrap();

    let token = token_for(f.employee, f.org, UserRole::Teller);
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
    let f = seed(&pool, "UTC").await;
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
    let token = token_for(f.employee, f.org, UserRole::Teller);

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
    let f = seed(&pool, "UTC").await;
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
    let token = token_for(f.employee, f.org, UserRole::Teller);

    check_in(&app, &token, f.branch, BRANCH_LAT, BRANCH_LNG).await;
    let req = test::TestRequest::post()
        .uri("/staff/me/check-out")
        .insert_header(("Authorization", format!("Bearer {token}")))
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
    let token = token_for(f.employee, f.org, UserRole::Teller);

    let req = test::TestRequest::post()
        .uri("/staff/me/check-out")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(&json!({ "latitude": BRANCH_LAT, "longitude": BRANCH_LNG }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 404);
}

#[sqlx::test]
async fn today_tells_the_app_which_branch_to_clock_in_at(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
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
    let token = token_for(f.employee, f.org, UserRole::Teller);

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
    let token = token_for(f.employee, f.org, UserRole::Teller);

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
async fn a_user_without_a_profile_cannot_clock_in(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    // Deliberately no staff_profiles row.
    let token = token_for(f.employee, f.org, UserRole::Teller);

    let resp = check_in(&app, &token, f.branch, BRANCH_LAT, BRANCH_LNG).await;
    assert_eq!(
        resp.status(),
        403,
        "being a user is not the same as being an employee"
    );
}

#[sqlx::test]
async fn a_suspended_employee_cannot_clock_in(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    sqlx::query("UPDATE staff_profiles SET employment_status = 'suspended' WHERE user_id = $1")
        .bind(f.employee)
        .execute(&pool)
        .await
        .unwrap();
    let token = token_for(f.employee, f.org, UserRole::Teller);

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
    let f = seed(&pool, "UTC").await;
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
    let token = token_for(f.employee, f.org, UserRole::Teller);

    check_in(&app, &token, f.branch, BRANCH_LAT, BRANCH_LNG).await;
    // Backdate the check-in so the checkout has a measurable span.
    sqlx::query("UPDATE attendance_records SET check_in_at = now() - INTERVAL '6 hours'")
        .execute(&pool)
        .await
        .unwrap();

    let req = test::TestRequest::post()
        .uri("/staff/me/check-out")
        .insert_header(("Authorization", format!("Bearer {token}")))
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
        "INSERT INTO staff_schedule_overrides (org_id, user_id, on_date, work_shift_id) \
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
            "/staff/schedules/day?user_id={}&date={}&branch_id={}",
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
        "INSERT INTO staff_schedule_overrides (org_id, user_id, on_date, work_shift_id) \
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
            "/staff/schedules/day?user_id={}&date={}&branch_id={}",
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
            "/staff/schedules/day?user_id={}&date={}&branch_id={}",
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

    // …a branch manager with `staff:read` but no `payroll:read` does not.
    let manager = seed_user(&pool, f.org, "Manager", UserRole::BranchManager).await;
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ('branch_manager'::user_role, 'staff'::permission_resource, \
                 'read'::permission_action, true) ON CONFLICT DO NOTHING",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ('branch_manager'::user_role, 'payroll'::permission_resource, \
                 'read'::permission_action, false) \
         ON CONFLICT (role, resource, action) DO UPDATE SET granted = false",
    )
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
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(&json!({ "name": "Kitchen" }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 201);
    let dept: serde_json::Value = test::read_body_json(resp).await;
    let dept_id = dept["id"].as_str().unwrap().to_string();

    sqlx::query("UPDATE staff_profiles SET department_id = $1 WHERE user_id = $2")
        .bind(Uuid::parse_str(&dept_id).unwrap())
        .bind(f.employee)
        .execute(&pool)
        .await
        .unwrap();

    let req = test::TestRequest::delete()
        .uri(&format!("/staff/departments/{dept_id}"))
        .insert_header(("Authorization", format!("Bearer {token}")))
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

#[sqlx::test]
async fn approving_leave_spends_the_balance_and_cancelling_refunds_it(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let leave_type = seed_leave_type(&pool, f.org, true).await;
    let admin_token = token_for(f.admin, f.org, UserRole::OrgAdmin);
    let employee_token = token_for(f.employee, f.org, UserRole::Teller);

    let req = test::TestRequest::post()
        .uri("/staff/me/requests")
        .insert_header(("Authorization", format!("Bearer {employee_token}")))
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
        200
    );
    let used: Decimal = sqlx::query_scalar(
        "SELECT used_days FROM leave_balances WHERE user_id = $1 AND leave_type_id = $2",
    )
    .bind(f.employee)
    .bind(leave_type)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(used, dec!(3), "1–3 September is three days");

    assert_eq!(
        auth_send!(
            app,
            patch,
            decision_uri,
            admin_token,
            json!({ "status": "cancelled" })
        )
        .status(),
        200
    );
    let used: Decimal = sqlx::query_scalar(
        "SELECT used_days FROM leave_balances WHERE user_id = $1 AND leave_type_id = $2",
    )
    .bind(f.employee)
    .bind(leave_type)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        used,
        Decimal::ZERO,
        "cancelling approved leave must give the days back"
    );
}

#[sqlx::test]
async fn overlapping_leave_requests_are_refused(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let leave_type = seed_leave_type(&pool, f.org, true).await;
    let token = token_for(f.employee, f.org, UserRole::Teller);

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
        "two live requests over the same day would double-count the balance"
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
        "INSERT INTO staff_requests (org_id, user_id, kind, leave_type_id, on_date, end_date) \
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
        auth_send!(app, patch, uri, token, json!({ "status": "approved" })).status(),
        409,
        "re-deciding would double the balance arithmetic"
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
        .insert_header(("Authorization", format!("Bearer {token}")))
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
        .find(|s| s["user_id"] == f.employee.to_string())
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
             (org_id, user_id, branch_id, business_date, status, check_in_at, \
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

    let employee_token = token_for(f.employee, f.org, UserRole::Teller);
    let req = test::TestRequest::post()
        .uri("/staff/me/requests")
        .insert_header(("Authorization", format!("Bearer {employee_token}")))
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
        .insert_header(("Authorization", format!("Bearer {admin_token}")))
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
    seed_profile(&pool, f.org, f.admin, 300_000).await;

    let someone_elses: Uuid = sqlx::query_scalar(
        "INSERT INTO attendance_records \
             (org_id, user_id, branch_id, business_date, status, check_in_at) \
         VALUES ($1, $2, $3, '2026-09-08', 'present', '2026-09-08T09:00:00Z') RETURNING id",
    )
    .bind(f.org)
    .bind(f.admin)
    .bind(f.branch)
    .fetch_one(&pool)
    .await
    .unwrap();

    let employee_token = token_for(f.employee, f.org, UserRole::Teller);
    let req = test::TestRequest::post()
        .uri("/staff/me/requests")
        .insert_header(("Authorization", format!("Bearer {employee_token}")))
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
             (org_id, user_id, branch_id, business_date, status, check_in_at, \
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

    let employee_token = token_for(f.employee, f.org, UserRole::Teller);
    let req = test::TestRequest::post()
        .uri("/staff/me/requests")
        .insert_header(("Authorization", format!("Bearer {employee_token}")))
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
        .insert_header(("Authorization", format!("Bearer {admin_token}")))
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
        "INSERT INTO payroll_deductions (org_id, user_id, amount_piastres, reason, \
             effective_date, source, status) \
         VALUES ($1, $2, 25000, 'Late', '2026-09-08', 'late_penalty', 'approved')",
    )
    .bind(f.org)
    .bind(f.employee)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO salary_advances (org_id, user_id, amount_piastres, installments, \
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
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), 200);
    let preview: serde_json::Value = test::read_body_json(resp).await;
    let previewed = preview
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["user_id"] == f.employee.to_string())
        .expect("the employee should appear in the preview")
        .clone();

    // Previewing must not collect anything.
    let remaining: i64 =
        sqlx::query_scalar("SELECT remaining_piastres FROM salary_advances WHERE user_id = $1")
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
        .find(|s| s["user_id"] == f.employee.to_string())
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
        "INSERT INTO salary_advances (org_id, user_id, amount_piastres, installments, \
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
        sqlx::query_scalar("SELECT remaining_piastres FROM salary_advances WHERE user_id = $1")
            .bind(f.employee)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(remaining, 50_000, "one installment should have been taken");

    let net: i64 = sqlx::query_scalar("SELECT net_piastres FROM payslips WHERE user_id = $1")
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
        "INSERT INTO salary_advances (org_id, user_id, amount_piastres, installments, \
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
        sqlx::query_scalar("SELECT remaining_piastres FROM salary_advances WHERE user_id = $1")
            .bind(f.employee)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        remaining, 50_000,
        "three generations of one period must still collect exactly one installment"
    );

    let slips: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payslips WHERE user_id = $1")
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
    let req = test::TestRequest::patch()
        .uri(&format!("/staff/payroll/periods/{period}/status"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(&json!({ "status": "paid" }))
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
    let period = seed_period(&pool, f.org).await;
    let token = token_for(f.admin, f.org, UserRole::OrgAdmin);

    let uri = format!("/staff/payroll/periods/{period}/status");
    assert_eq!(
        auth_send!(app, patch, uri, token, json!({ "status": "generated" })).status(),
        200
    );
    assert_eq!(
        auth_send!(app, patch, uri, token, json!({ "status": "paid" })).status(),
        200
    );
    assert_eq!(
        auth_send!(app, patch, uri, token, json!({ "status": "draft" })).status(),
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
        "INSERT INTO staff_requests (org_id, user_id, kind, leave_type_id, on_date, end_date, \
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
            "INSERT INTO attendance_records (org_id, user_id, branch_id, business_date, status) \
             VALUES ($1, $2, $3, make_date(2026, 9, $4), 'on_leave') RETURNING id",
        )
        .bind(f.org)
        .bind(f.employee)
        .bind(f.branch)
        .bind(day)
        .fetch_one(&pool)
        .await
        .unwrap();

        let settings = crate::staff::attendance::load_settings(&pool, f.org, None)
            .await
            .unwrap();
        let mut conn = pool.acquire().await.unwrap();
        crate::staff::penalties::recompute_record(&mut conn, record, &settings)
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

    let net: i64 = sqlx::query_scalar("SELECT net_piastres FROM payslips WHERE user_id = $1")
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
        "INSERT INTO payroll_bonuses (org_id, user_id, percent_of_base, reason, effective_date) \
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

    let (bonus, net): (i64, i64) =
        sqlx::query_as("SELECT bonuses_piastres, net_piastres FROM payslips WHERE user_id = $1")
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
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let other = seed_user(&pool, f.org, "Other", UserRole::Teller).await;
    seed_profile(&pool, f.org, other, 900_000).await;

    let period = seed_period(&pool, f.org).await;
    let admin_token = token_for(f.admin, f.org, UserRole::OrgAdmin);
    generate(&app, &admin_token, period).await;

    let employee_token = token_for(f.employee, f.org, UserRole::Teller);
    let resp = auth_get!(app, "/staff/me/payslips", employee_token);
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;
    let slips = body.as_array().unwrap();
    assert_eq!(slips.len(), 1);
    assert_eq!(slips[0]["user_id"], f.employee.to_string());
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
                         {"from_minutes":31,"to_minutes":120,"kind":"day_fraction","value":0.5}]'::jsonb)"#,
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
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(&json!({ "latitude": BRANCH_LAT, "longitude": BRANCH_LNG }))
        .to_request();
    test::call_service(app, req).await
}

#[sqlx::test]
async fn a_late_arrival_is_priced_by_the_ladder_at_check_out(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_late_ladder(&pool, f.org).await;
    // 60 minutes past a 15-minute grace = 45 late → the half-day rung.
    seed_late_setup(&pool, &f, 60, 15).await;
    let token = token_for(f.employee, f.org, UserRole::Teller);

    check_in(&app, &token, f.branch, BRANCH_LAT, BRANCH_LNG).await;
    check_out(&app, &token).await;

    let (amount, source, original): (i64, String, Option<i64>) = sqlx::query_as(
        "SELECT amount_piastres, source, original_amount_piastres            FROM payroll_deductions WHERE user_id = $1",
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
    let f = seed(&pool, "UTC").await;
    seed_late_ladder(&pool, f.org).await;
    seed_late_setup(&pool, &f, 60, 15).await;

    // Permission to arrive 5 minutes from now — granted before the punch.
    sqlx::query(
        "INSERT INTO staff_requests (org_id, user_id, kind, on_date, to_time, status, decided_at)          VALUES ($1, $2, 'late_arrival', (now() AT TIME ZONE 'UTC')::date, $3, 'approved', now())",
    )
    .bind(f.org)
    .bind(f.employee)
    .bind(utc_time_offset(5))
    .execute(&pool)
    .await
    .unwrap();

    let token = token_for(f.employee, f.org, UserRole::Teller);
    check_in(&app, &token, f.branch, BRANCH_LAT, BRANCH_LNG).await;
    check_out(&app, &token).await;

    let penalties: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM payroll_deductions WHERE user_id = $1 AND source = 'late_penalty'",
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
    use crate::staff::attendance::{DayAdjustments, derive};
    use chrono::TimeZone;

    let at = |h: u32, m: u32| Utc.with_ymd_and_hms(2026, 8, 10, h, m, 0).unwrap();
    // Rostered 09:00–17:00; permission to leave at 13:00; actually left at 13:00.
    let approved = derive(
        Some(at(9, 0)),
        Some(at(13, 0)),
        Some(at(9, 0)),
        Some(at(17, 0)),
        None,
        &DayAdjustments {
            excused_from: Some(at(13, 0)),
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
    use crate::staff::attendance::{DayAdjustments, derive};
    use chrono::TimeZone;

    let at = |h: u32, m: u32| Utc.with_ymd_and_hms(2026, 8, 10, h, m, 0).unwrap();
    let base = DayAdjustments {
        excused_windows: vec![(at(12, 0), at(14, 0))],
        ..Default::default()
    };

    let paid = derive(
        Some(at(9, 0)),
        Some(at(17, 0)),
        Some(at(9, 0)),
        Some(at(17, 0)),
        None,
        &DayAdjustments {
            excused_time_paid: true,
            ..base.clone()
        },
    );
    let unpaid = derive(
        Some(at(9, 0)),
        Some(at(17, 0)),
        Some(at(9, 0)),
        Some(at(17, 0)),
        None,
        &DayAdjustments {
            excused_time_paid: false,
            ..base
        },
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
    let f = seed(&pool, "UTC").await;
    seed_late_ladder(&pool, f.org).await;
    seed_late_setup(&pool, &f, 60, 15).await;
    let employee_token = token_for(f.employee, f.org, UserRole::Teller);
    let admin_token = token_for(f.admin, f.org, UserRole::OrgAdmin);

    check_in(&app, &employee_token, f.branch, BRANCH_LAT, BRANCH_LNG).await;
    check_out(&app, &employee_token).await;

    let id: Uuid = sqlx::query_scalar("SELECT id FROM payroll_deductions WHERE user_id = $1")
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
    let settings = crate::staff::attendance::load_settings(&pool, f.org, None)
        .await
        .unwrap();
    let record: Uuid = sqlx::query_scalar("SELECT id FROM attendance_records WHERE user_id = $1")
        .bind(f.employee)
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    crate::staff::penalties::recompute_record(&mut conn, record, &settings)
        .await
        .unwrap();
    drop(conn);

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
        "INSERT INTO payroll_deductions (org_id, user_id, amount_piastres, reason,              effective_date, source, status)          VALUES ($1, $2, 40000, 'Manual', '2026-09-10', 'manual', 'approved') RETURNING id",
    )
    .bind(f.org)
    .bind(f.employee)
    .fetch_one(&pool)
    .await
    .unwrap();

    let period = seed_period(&pool, f.org).await;
    generate(&app, &admin_token, period).await;
    let before: i64 = sqlx::query_scalar("SELECT net_piastres FROM payslips WHERE user_id = $1")
        .bind(f.employee)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before, 260_000);

    let uri = format!("/staff/payroll/deductions/{id}/waive");
    auth_send!(
        app,
        patch,
        uri,
        admin_token,
        json!({ "reason": "Reversed" })
    );
    generate(&app, &admin_token, period).await;

    let after: i64 = sqlx::query_scalar("SELECT net_piastres FROM payslips WHERE user_id = $1")
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
        "INSERT INTO payroll_deductions (org_id, user_id, amount_piastres,              original_amount_piastres, reason, effective_date, source, status)          VALUES ($1, $2, 50000, 50000, 'Late', '2026-09-10', 'late_penalty', 'approved')          RETURNING id",
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
    let net: i64 = sqlx::query_scalar("SELECT net_piastres FROM payslips WHERE user_id = $1")
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
        "INSERT INTO payroll_deductions (org_id, user_id, amount_piastres, reason,              effective_date, source, status)          VALUES ($1, $2, 5000, 'Late', '2026-09-10', 'late_penalty', 'approved') RETURNING id",
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
    let token = token_for(f.employee, f.org, UserRole::Teller);
    let uri = "/staff/me/requests".to_string();

    // Each of these is the kind's REQUIRED field, missing.
    for body in [
        json!({ "kind": "late_arrival",    "on_date": "2026-09-01" }),
        json!({ "kind": "early_departure", "on_date": "2026-09-01" }),
        json!({ "kind": "excuse",          "on_date": "2026-09-01", "from_time": "12:00:00" }),
        json!({ "kind": "mission",         "on_date": "2026-09-01" }),
        json!({ "kind": "leave",           "on_date": "2026-09-01" }),
    ] {
        let kind = body["kind"].as_str().unwrap().to_string();
        assert_eq!(
            auth_send!(app, post, uri, token, body).status(),
            400,
            "a malformed {kind} must never reach the classifier"
        );
    }

    // And an excuse whose window ends before it starts.
    assert_eq!(
        auth_send!(
            app,
            post,
            uri,
            token,
            json!({ "kind": "excuse", "on_date": "2026-09-01",
                    "from_time": "14:00:00", "to_time": "12:00:00" })
        )
        .status(),
        400
    );
}

#[sqlx::test]
async fn one_live_request_per_kind_per_day(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;
    let token = token_for(f.employee, f.org, UserRole::Teller);
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
    let token = token_for(f.employee, f.org, UserRole::Teller);

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
    let token = token_for(f.employee, f.org, UserRole::Teller);

    // A teller has no `attendance` grant at all, yet must be able to see their
    // own day — that is the whole point of the /me surface.
    assert_eq!(auth_get!(app, "/staff/me/today", token).status(), 200);
}

#[sqlx::test]
async fn attendance_coordinates_are_purged_after_the_retention_window(pool: PgPool) {
    // Attendance TIMES are payroll evidence and must survive; the COORDINATES are
    // only there to prove the punch happened at the branch, and stop being
    // defensible to keep once the punch is settled. The geofence RESULT
    // (distance in metres) is kept — it is the auditable fact, and unlike a
    // latitude/longitude it does not record where the employee actually was.
    let f = seed(&pool, "UTC").await;
    seed_profile(&pool, f.org, f.employee, 300_000).await;

    let insert = |day: &'static str| {
        let pool = pool.clone();
        let (org, user, branch) = (f.org, f.employee, f.branch);
        async move {
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO attendance_records \
                     (org_id, user_id, branch_id, business_date, status, check_in_at, \
                      check_in_latitude, check_in_longitude, check_in_distance_meters, \
                      check_in_method, check_out_at, check_out_latitude, check_out_longitude) \
                 VALUES ($1, $2, $3, (CURRENT_DATE - $4::int), 'present', now(), \
                         30.0444, 31.2357, 12.5, 'mobile_gps', now(), 30.0445, 31.2358) \
                 RETURNING id",
            )
            .bind(org)
            .bind(user)
            .bind(branch)
            .bind(day.parse::<i32>().unwrap())
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };

    let old_record = insert("120").await; // well past the 90-day window
    let recent_record = insert("10").await; // comfortably inside it

    crate::staff::jobs::purge_stale_coordinates(&pool)
        .await
        .expect("the purge should succeed");

    // The old punch keeps everything payroll needs, minus the coordinates.
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
    .bind(old_record)
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

    // A recent punch is untouched: the window has not passed.
    let (lat, lng): (Option<f64>, Option<f64>) = sqlx::query_as(
        "SELECT check_in_latitude, check_in_longitude FROM attendance_records WHERE id = $1",
    )
    .bind(recent_record)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (lat, lng),
        (Some(30.0444), Some(31.2357)),
        "a punch inside the retention window must keep its coordinates"
    );

    // Idempotent: re-running finds nothing left to do and must not error.
    crate::staff::jobs::purge_stale_coordinates(&pool)
        .await
        .expect("a second pass should be a harmless no-op");
}
