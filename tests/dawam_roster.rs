//! Dawam roster (Phase B · schedules): the one roster function and everything
//! that reads it.
//!
//! - The resolver: a date's own set > the weekday row > the every-day row; a
//!   block only on its days, at that day's times; an assignment's own from/to;
//!   a shift crossing midnight belongs to the day it starts; Cairo in winter
//!   and summer (SC-6, SC-10, AT-1, AT-9).
//! - The absence sweep reads the same roster (SC-6, CV-6, RU-10).
//! - A date holds several shifts, and a swap, a claim or an accepted
//!   suggestion touches one block and keeps the rest (SC-5, SC-11).
//! - Every refusal code, and a branch-A manager refused on branch B for every
//!   roster write (RO-6, AT-11).
//! - Drafts stay hidden and published changes are marked and told (SC-3, SC-4).
//! - Punches and covers after midnight land on last night's shift (SC-10).
//! - Covers in the discipline report, the rejected-cover flag (CV-3, CV-7).
//! - Preferences overridden by a manager, logged (SC-12).
//! - The engine's guardrails: the crafted id, the learning freeze notice, the
//!   monthly fairness audit, the 30-second stale cache (SC-13).

use actix_web::{App, http::Method, test, web};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, Timelike, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::models::UserRole;
use madar_rust::staff::dawam::week_start;

mod common;
use common::employees::{authed, employee, phone_token, secret, user_token};

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
    ($app:expr, $method:expr, $uri:expr, $token:expr) => {
        call!($app, $method, $uri, $token, Value::Null)
    };
    ($app:expr, $method:expr, $uri:expr, $token:expr, $body:expr) => {{
        let body: Value = $body;
        let mut req = authed(
            test::TestRequest::default()
                .method(Method::from_bytes($method.as_bytes()).unwrap())
                .uri(&$uri),
            &$token,
        );
        if !body.is_null() {
            req = req.set_json(&body);
        }
        test::call_service(&$app, req.to_request()).await
    }};
}

/// Status, and the body as JSON (Null when there is none).
async fn done(resp: actix_web::dev::ServiceResponse) -> (u16, Value) {
    let status = resp.status().as_u16();
    let bytes = test::read_body(resp).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Assert a refusal's status and code.
macro_rules! refused {
    ($resp:expr, $status:expr, $code:expr) => {{
        let (status, body) = done($resp).await;
        assert_eq!(status, $status, "{body}");
        assert_eq!(body["code"], json!($code), "{body}");
    }};
}

const LAT: f64 = 29.9792;
const LNG: f64 = 31.1342;

/// One business, branches A and B (UTC unless a test says otherwise). The
/// manager runs A. `a` and `b` work at A, `x` at B; all have the app. The
/// owner is on payroll with no branch, so the owner's inbox can be read
/// without joining anyone's roster.
struct F {
    org: Uuid,
    br_a: Uuid,
    br_b: Uuid,
    owner: Uuid,
    owner_emp: Uuid,
    manager: Uuid,
    a: Uuid,
    a_user: Uuid,
    b: Uuid,
    x: Uuid,
}

impl F {
    fn owner(&self) -> String {
        user_token(self.owner, self.org, UserRole::OrgAdmin)
    }
    fn manager(&self) -> String {
        user_token(self.manager, self.org, UserRole::BranchManager)
    }
    fn teller(&self) -> String {
        user_token(self.a_user, self.org, UserRole::Teller)
    }
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
    .bind(format!("{id}@t.test"))
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn branch(pool: &PgPool, org: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO branches (org_id, name, timezone, latitude, longitude, geo_radius_meters) \
         VALUES ($1, $2, 'UTC'::timezone_name, $3, $4, 200) RETURNING id",
    )
    .bind(org)
    .bind(name)
    .bind(LAT)
    .bind(LNG)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn seed(pool: &PgPool) -> F {
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    let org = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, modules) VALUES ($1, 'Rue', $2, '{pos,dawam}')",
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
    let br_a = branch(pool, org, "A").await;
    let br_b = branch(pool, org, "B").await;
    let owner = user(pool, org, "Owner", "org_admin").await;
    let manager = user(pool, org, "Karim", "branch_manager").await;
    let a_user = user(pool, org, "Amal", "teller").await;
    for (u, b) in [(manager, br_a), (a_user, br_a)] {
        sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
            .bind(u)
            .bind(b)
            .execute(pool)
            .await
            .unwrap();
    }
    let a = employee(
        pool,
        org,
        "Amal",
        Some(a_user),
        Some("+201060000001"),
        true,
        &[br_a],
        600_000,
    )
    .await;
    let b = employee(
        pool,
        org,
        "Bassem",
        None,
        Some("+201060000002"),
        true,
        &[br_a],
        600_000,
    )
    .await;
    let x = employee(
        pool,
        org,
        "Xavier",
        None,
        Some("+201060000003"),
        true,
        &[br_b],
        600_000,
    )
    .await;
    let owner_emp = employee(pool, org, "Owner", Some(owner), None, false, &[], 0).await;
    F {
        org,
        br_a,
        br_b,
        owner,
        owner_emp,
        manager,
        a,
        a_user,
        b,
        x,
    }
}

fn t(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).unwrap()
}

fn today() -> NaiveDate {
    Utc::now().date_naive()
}

/// Postgres DOW (0 = Sunday).
fn dow(d: NaiveDate) -> i16 {
    d.weekday().num_days_from_sunday() as i16
}

/// The first date at least `ahead` days from today that falls on `dow`.
fn next_on(ahead: i64, dow_: i16) -> NaiveDate {
    let mut d = today() + Duration::days(ahead);
    while dow(d) != dow_ {
        d += Duration::days(1);
    }
    d
}

/// A block (work shift) at `branch` (None = the whole business).
async fn block(
    pool: &PgPool,
    f: &F,
    branch: Option<Uuid>,
    name: &str,
    start: NaiveTime,
    end: NaiveTime,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO work_shifts (org_id, branch_id, name, start_time, end_time) \
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(f.org)
    .bind(branch)
    .bind(name)
    .bind(start)
    .bind(end)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A standing pattern row from long ago: `dow` None = every day.
async fn pattern(pool: &PgPool, f: &F, who: Uuid, shift: Uuid, dow_: Option<i16>) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO staff_schedules (org_id, employee_id, work_shift_id, day_of_week, effective_from) \
         VALUES ($1, $2, $3, $4, DATE '2025-01-01') RETURNING id",
    )
    .bind(f.org)
    .bind(who)
    .bind(shift)
    .bind(dow_)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A date's own row, written straight to the table.
async fn override_row(pool: &PgPool, f: &F, who: Uuid, on: NaiveDate, shift: Option<Uuid>) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO staff_schedule_overrides (org_id, employee_id, on_date, work_shift_id) \
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(f.org)
    .bind(who)
    .bind(on)
    .bind(shift)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[derive(Debug, sqlx::FromRow)]
#[allow(dead_code)]
struct Rostered {
    on_date: NaiveDate,
    work_shift_id: Uuid,
    start_local: NaiveTime,
    end_local: NaiveTime,
    crosses_midnight: bool,
    times_edited: bool,
    from_override: bool,
    start_at: DateTime<Utc>,
    end_at: DateTime<Utc>,
}

/// The one roster function, straight from SQL.
async fn roster(pool: &PgPool, who: Uuid, from: NaiveDate, to: NaiveDate) -> Vec<Rostered> {
    sqlx::query_as(
        "SELECT on_date, work_shift_id, start_local, end_local, crosses_midnight, times_edited, \
                from_override, start_at, end_at \
           FROM dawam_roster(ARRAY[$1]::uuid[], $2, $3)",
    )
    .bind(who)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn shifts_on(pool: &PgPool, who: Uuid, on: NaiveDate) -> Vec<Uuid> {
    roster(pool, who, on, on)
        .await
        .into_iter()
        .map(|r| r.work_shift_id)
        .collect()
}

async fn publish(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    f: &F,
    branch: Uuid,
    day: NaiveDate,
) {
    let (s, b) = done(call!(
        app,
        "POST",
        "/staff/roster/publish",
        f.owner(),
        json!({ "branch_id": branch, "week_start": day })
    ))
    .await;
    assert_eq!(s, 204, "{b}");
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

async fn changed(pool: &PgPool, who: Uuid, on: NaiveDate) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM staff_roster_changes WHERE employee_id = $1 AND on_date = $2)",
    )
    .bind(who)
    .bind(on)
    .fetch_one(pool)
    .await
    .unwrap()
}

// ── the resolver ───────────────────────────────────────────────────────────

#[sqlx::test]
async fn a_date_beats_the_weekday_row_which_beats_the_every_day_row(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let m = block(&pool, &f, Some(f.br_a), "Morning", t(8, 0), t(12, 0)).await;
    let e = block(&pool, &f, Some(f.br_a), "Evening", t(15, 0), t(19, 0)).await;
    let d = today() + Duration::days(3);
    pattern(&pool, &f, f.a, m, None).await;
    pattern(&pool, &f, f.a, e, Some(dow(d))).await;

    // The weekday row replaces the every-day row on its day: no stacking.
    assert_eq!(shifts_on(&pool, f.a, d).await, vec![e]);
    assert_eq!(shifts_on(&pool, f.a, d + Duration::days(1)).await, vec![m]);

    // Two weekday rows on one day are a split day from the pattern.
    pattern(&pool, &f, f.a, m, Some(dow(d))).await;
    assert_eq!(shifts_on(&pool, f.a, d).await, vec![m, e]);

    // A date's own set beats both: a day off, or exactly its blocks.
    let off = d + Duration::days(7);
    let (s, body) = done(call!(
        app,
        "PUT",
        "/staff/schedules/days",
        f.owner(),
        json!({ "employee_id": f.a, "on_date": off, "shifts": [] })
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["shifts"], json!([]));
    assert_eq!(body["follows_pattern"], json!(false));
    assert!(shifts_on(&pool, f.a, off).await.is_empty());
    // The grid knows it is a day off by date, so "back to the pattern" applies.
    let (_, view) = done(call!(
        app,
        "GET",
        format!("/staff/roster?branch_id={}&from={off}&to={off}", f.br_a),
        f.owner()
    ))
    .await;
    assert_eq!(
        view["date_sets"],
        json!([{ "employee_id": f.a, "date": off, "day_off": true }])
    );

    let one = d + Duration::days(8);
    let (s, body) = done(call!(
        app,
        "PUT",
        "/staff/schedules/days",
        f.owner(),
        json!({ "employee_id": f.a, "on_date": one, "shifts": [{ "work_shift_id": e }] })
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    let r = roster(&pool, f.a, one, one).await;
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].work_shift_id, e);
    assert!(r[0].from_override);

    // Back to the pattern: the date's set is dropped.
    let (s, body) = done(call!(
        app,
        "DELETE",
        format!("/staff/schedules/days?employee_id={}&on_date={one}", f.a),
        f.owner()
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["follows_pattern"], json!(true));
    assert_eq!(shifts_on(&pool, f.a, one).await, vec![m]);
    // The day view resolves the same way.
    let (s, body) = done(call!(
        app,
        "GET",
        format!("/staff/schedules/day?employee_id={}&date={one}", f.a),
        f.owner()
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body[0]["work_shift_id"], json!(m));
}

#[sqlx::test]
async fn a_block_is_offered_on_its_days_at_that_days_times(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    // The owner's example: Evening 16:00–00:00 Sat–Wed, 16:00–01:00 Thu–Fri,
    // as ONE block; Brunch is not a Friday shift.
    let (s, ev) = done(call!(
        app,
        "POST",
        "/staff/work-shifts",
        f.owner(),
        json!({
            "branch_id": f.br_a, "name": "Evening", "start_time": "16:00:00", "end_time": "00:00:00",
            "day_times": [
                { "day_of_week": 4, "start_time": "16:00:00", "end_time": "01:00:00" },
                { "day_of_week": 5, "start_time": "16:00:00", "end_time": "01:00:00" }
            ]
        })
    ))
    .await;
    assert_eq!(s, 201, "{ev}");
    assert_eq!(ev["valid_days"], json!([0, 1, 2, 3, 4, 5, 6]));
    assert_eq!(ev["day_times"].as_array().unwrap().len(), 2);
    assert_eq!(ev["crosses_midnight"], json!(true));
    let evening: Uuid = serde_json::from_value(ev["id"].clone()).unwrap();
    let (s, br) = done(call!(
        app,
        "POST",
        "/staff/work-shifts",
        f.owner(),
        json!({
            "branch_id": f.br_a, "name": "Brunch", "start_time": "10:00:00", "end_time": "14:00:00",
            "valid_days": [6, 0, 1, 2, 3, 4]
        })
    ))
    .await;
    assert_eq!(s, 201, "{br}");
    let brunch: Uuid = serde_json::from_value(br["id"].clone()).unwrap();
    pattern(&pool, &f, f.a, evening, None).await;
    pattern(&pool, &f, f.a, brunch, None).await;

    let wed = next_on(2, 3);
    let thu = wed + Duration::days(1);
    let fri = wed + Duration::days(2);
    let r = roster(&pool, f.a, wed, fri).await;
    let on = |d: NaiveDate, s: Uuid| r.iter().find(|x| x.on_date == d && x.work_shift_id == s);
    let w = on(wed, evening).unwrap();
    assert_eq!((w.start_local, w.end_local), (t(16, 0), t(0, 0)));
    assert!(w.crosses_midnight);
    // The shift belongs to the day it starts; it ends on the next date.
    assert_eq!(w.end_at, (thu).and_time(t(0, 0)).and_utc());
    let th = on(thu, evening).unwrap();
    assert_eq!((th.start_local, th.end_local), (t(16, 0), t(1, 0)));
    assert_eq!(th.end_at, fri.and_time(t(1, 0)).and_utc());
    assert!(on(wed, brunch).is_some());
    assert!(on(fri, brunch).is_none(), "Brunch isn't a Friday shift");

    // The manager's grid shows that day's times.
    let (s, body) = done(call!(
        app,
        "GET",
        format!("/staff/roster?branch_id={}&from={wed}&to={fri}", f.br_a),
        f.owner()
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    let thu_ev = body["shifts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["date"] == json!(thu) && s["work_shift_id"] == json!(evening))
        .unwrap();
    assert_eq!(thu_ev["end_time"], json!("01:00:00"));
    assert_eq!(thu_ev["crosses_midnight"], json!(true));
    let brief = body["work_shifts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == json!(brunch))
        .unwrap();
    assert_eq!(brief["valid_days"], json!([6, 0, 1, 2, 3, 4]));

    // Refused: the block on a day it isn't valid, by date, by weekday row and
    // as an open shift.
    refused!(
        call!(
            app,
            "PUT",
            "/staff/schedules/days",
            f.owner(),
            json!({ "employee_id": f.b, "on_date": fri, "shifts": [{ "work_shift_id": brunch }] })
        ),
        400,
        "SHIFT_NOT_ON_DAY"
    );
    refused!(
        call!(
            app,
            "PUT",
            "/staff/schedules/overrides",
            f.owner(),
            json!({ "employee_id": f.b, "on_date": fri, "work_shift_id": brunch })
        ),
        400,
        "SHIFT_NOT_ON_DAY"
    );
    refused!(
        call!(
            app,
            "POST",
            "/staff/schedules",
            f.owner(),
            json!({ "employee_id": f.b, "work_shift_id": brunch, "day_of_week": 5 })
        ),
        400,
        "SHIFT_NOT_ON_DAY"
    );
    refused!(
        call!(
            app,
            "POST",
            "/staff/open-shifts",
            f.owner(),
            json!({ "branch_id": f.br_a, "work_shift_id": brunch, "on_date": fri })
        ),
        400,
        "SHIFT_NOT_ON_DAY"
    );
    // A weekday's own times on a day the block isn't valid: refused.
    let (s, body) = done(call!(
        app,
        "PATCH",
        format!("/staff/work-shifts/{brunch}"),
        f.owner(),
        json!({
            "branch_id": f.br_a, "name": "Brunch", "start_time": "10:00:00", "end_time": "14:00:00",
            "day_times": [{ "day_of_week": 5, "start_time": "11:00:00", "end_time": "15:00:00" }]
        })
    ))
    .await;
    assert_eq!(s, 400, "{body}");
    // Taking Wednesday away while Bassem's own Wednesday row names it:
    // refused until he is moved.
    let row = pattern(&pool, &f, f.b, brunch, Some(3)).await;
    refused!(
        call!(
            app,
            "PATCH",
            format!("/staff/work-shifts/{brunch}"),
            f.owner(),
            json!({
                "branch_id": f.br_a, "name": "Brunch", "start_time": "10:00:00",
                "end_time": "14:00:00", "valid_days": [6, 0, 1, 2, 4]
            })
        ),
        409,
        "SHIFT_DAYS_IN_USE"
    );
    // A future date assignment on that weekday blocks it too.
    sqlx::query("DELETE FROM staff_schedules WHERE id = $1")
        .bind(row)
        .execute(&pool)
        .await
        .unwrap();
    override_row(&pool, &f, f.b, wed + Duration::days(7), Some(brunch)).await;
    refused!(
        call!(
            app,
            "PATCH",
            format!("/staff/work-shifts/{brunch}"),
            f.owner(),
            json!({
                "branch_id": f.br_a, "name": "Brunch", "start_time": "10:00:00",
                "end_time": "14:00:00", "valid_days": [6, 0, 1, 2, 4]
            })
        ),
        409,
        "SHIFT_DAYS_IN_USE"
    );
    sqlx::query("DELETE FROM staff_schedule_overrides WHERE employee_id = $1")
        .bind(f.b)
        .execute(&pool)
        .await
        .unwrap();
    // An every-day row follows the block's days: Amal's Wednesday brunch goes
    // with it (and, in a published week, she is told — SC-4).
    let (s, body) = done(call!(
        app,
        "PATCH",
        format!("/staff/work-shifts/{brunch}"),
        f.owner(),
        json!({
            "branch_id": f.br_a, "name": "Brunch", "start_time": "10:00:00",
            "end_time": "14:00:00", "valid_days": [6, 0, 1, 2, 4]
        })
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    assert!(!shifts_on(&pool, f.a, wed).await.contains(&brunch));
    assert!(shifts_on(&pool, f.a, thu).await.contains(&brunch));
    // Taking away a day nobody works it is fine.
    let (s, body) = done(call!(
        app,
        "PATCH",
        format!("/staff/work-shifts/{evening}"),
        f.owner(),
        json!({
            "branch_id": f.br_a, "name": "Evening", "start_time": "16:00:00",
            "end_time": "00:00:00", "valid_days": [0, 1, 2, 3, 4, 5, 6]
        })
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(
        body["day_times"].as_array().unwrap().len(),
        2,
        "omitted day_times are kept"
    );
}

#[sqlx::test]
async fn one_assignment_has_its_own_times_and_may_cross_midnight(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let e = block(&pool, &f, Some(f.br_a), "Evening", t(16, 0), t(23, 0)).await;
    pattern(&pool, &f, f.a, e, None).await;
    pattern(&pool, &f, f.b, e, None).await;
    let d = today() + Duration::days(4);

    let (s, body) = done(call!(
        app,
        "PUT",
        "/staff/schedules/days/times",
        f.owner(),
        json!({ "employee_id": f.a, "on_date": d, "work_shift_id": e,
                "start_time": "18:00:00", "end_time": "02:00:00" })
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    let sh = &body["shifts"][0];
    assert_eq!(sh["start_time"], json!("18:00:00"));
    assert_eq!(sh["end_time"], json!("02:00:00"));
    assert_eq!(sh["crosses_midnight"], json!(true));
    assert_eq!(sh["times_edited"], json!(true));
    assert_eq!(sh["on_date"], json!(d));
    let r = roster(&pool, f.a, d, d).await;
    assert_eq!(
        r[0].end_at,
        (d + Duration::days(1)).and_time(t(2, 0)).and_utc()
    );
    // The block is unchanged for everyone else.
    let rb = roster(&pool, f.b, d, d).await;
    assert_eq!((rb[0].start_local, rb[0].end_local), (t(16, 0), t(23, 0)));
    assert!(!rb[0].times_edited);

    // A night that long runs into the next morning's early shift: refused.
    let early = block(&pool, &f, Some(f.br_a), "Early", t(1, 0), t(7, 0)).await;
    refused!(
        call!(
            app,
            "PUT",
            "/staff/schedules/days",
            f.owner(),
            json!({ "employee_id": f.a, "on_date": d + Duration::days(1),
                    "shifts": [{ "work_shift_id": early }] })
        ),
        409,
        "SHIFTS_OVERLAP"
    );

    // Both, or neither; never an empty shift.
    let (s, _) = done(call!(
        app,
        "PUT",
        "/staff/schedules/days/times",
        f.owner(),
        json!({ "employee_id": f.a, "on_date": d, "work_shift_id": e, "start_time": "18:00:00" })
    ))
    .await;
    assert_eq!(s, 400);
    refused!(
        call!(
            app,
            "PUT",
            "/staff/schedules/days/times",
            f.owner(),
            json!({ "employee_id": f.a, "on_date": d, "work_shift_id": e,
                    "start_time": "18:00:00", "end_time": "18:00:00" })
        ),
        400,
        "SHIFT_EMPTY"
    );
    // Someone not on that block that day.
    refused!(
        call!(
            app,
            "PUT",
            "/staff/schedules/days/times",
            f.owner(),
            json!({ "employee_id": f.a, "on_date": d, "work_shift_id": early,
                    "start_time": "02:00:00", "end_time": "06:00:00" })
        ),
        409,
        "NOT_ROSTERED"
    );

    // Back to the block's own times.
    let (s, body) = done(call!(
        app,
        "PUT",
        "/staff/schedules/days/times",
        f.owner(),
        json!({ "employee_id": f.a, "on_date": d, "work_shift_id": e })
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["shifts"][0]["times_edited"], json!(false));
    assert_eq!(body["shifts"][0]["end_time"], json!("23:00:00"));
}

#[sqlx::test]
async fn cairo_shifts_follow_the_clock_in_winter_and_summer(pool: PgPool) {
    let f = seed(&pool).await;
    sqlx::query("UPDATE branches SET timezone = 'Africa/Cairo'::timezone_name WHERE id = $1")
        .bind(f.br_a)
        .execute(&pool)
        .await
        .unwrap();
    let day = block(&pool, &f, Some(f.br_a), "Day", t(9, 0), t(17, 0)).await;
    let night = block(&pool, &f, Some(f.br_a), "Night", t(22, 0), t(6, 0)).await;
    pattern(&pool, &f, f.a, day, None).await;
    pattern(&pool, &f, f.b, night, None).await;

    let winter = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
    let summer = NaiveDate::from_ymd_opt(2026, 7, 15).unwrap();
    let w = roster(&pool, f.a, winter, winter).await;
    assert_eq!(w[0].start_at, winter.and_time(t(7, 0)).and_utc(), "UTC+2");
    let s = roster(&pool, f.a, summer, summer).await;
    assert_eq!(s[0].start_at, summer.and_time(t(6, 0)).and_utc(), "UTC+3");

    // A night shift belongs to the day it starts, in local time.
    let n = roster(&pool, f.b, summer, summer).await;
    assert_eq!(n[0].on_date, summer);
    assert_eq!(n[0].start_at, summer.and_time(t(19, 0)).and_utc());
    assert_eq!(
        n[0].end_at,
        (summer + Duration::days(1)).and_time(t(3, 0)).and_utc()
    );
    assert_eq!((n[0].end_at - n[0].start_at).num_hours(), 8);

    // The zone database, not our arithmetic, handles the night the clocks go
    // forward (last Friday of April 2026): 22:00 → 06:00 is seven real hours.
    let spring = NaiveDate::from_ymd_opt(2026, 4, 23).unwrap();
    let n = roster(&pool, f.b, spring, spring).await;
    assert_eq!(n[0].start_at, spring.and_time(t(20, 0)).and_utc());
    assert_eq!((n[0].end_at - n[0].start_at).num_hours(), 7);
}

// ── the absence sweep ──────────────────────────────────────────────────────

async fn absences(pool: &PgPool, who: Uuid, on: NaiveDate) -> Vec<(Option<Uuid>, String)> {
    sqlx::query_as(
        "SELECT work_shift_id, status FROM attendance_records \
          WHERE employee_id = $1 AND business_date = $2 AND covered_employee_id IS NULL \
            AND status IN ('absent', 'on_leave') ORDER BY scheduled_start_at",
    )
    .bind(who)
    .bind(on)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn the_sweep_marks_absent_exactly_what_the_roster_says(pool: PgPool) {
    let f = seed(&pool).await;
    let y = today() - Duration::days(1);
    let m = block(&pool, &f, Some(f.br_a), "Morning", t(8, 0), t(12, 0)).await;
    let e = block(&pool, &f, Some(f.br_a), "Evening", t(15, 0), t(20, 0)).await;
    let person = async |name: &str| {
        employee(&pool, f.org, name, None, None, false, &[f.br_a], 300_000).await
    };

    // a: every day Morning, but Evening on yesterday's weekday → Evening only.
    pattern(&pool, &f, f.a, m, None).await;
    pattern(&pool, &f, f.a, e, Some(dow(y))).await;
    // b: nothing in the pattern; a date change gave him Morning (a day edit,
    // swap, claim or accepted suggestion) → missed like any other shift.
    override_row(&pool, &f, f.b, y, Some(m)).await;
    // c: every day Morning, a day off by date → nothing.
    let c = person("Cyrine").await;
    pattern(&pool, &f, c, m, None).await;
    override_row(&pool, &f, c, y, None).await;
    // d: a split day, worked the morning → only the evening is missed.
    let d = person("Dina").await;
    override_row(&pool, &f, d, y, Some(m)).await;
    override_row(&pool, &f, d, y, Some(e)).await;
    sqlx::query(
        "INSERT INTO attendance_records (org_id, employee_id, branch_id, work_shift_id, \
             business_date, status, check_in_at, check_out_at) \
         VALUES ($1, $2, $3, $4, $5, 'present', $6, $7)",
    )
    .bind(f.org)
    .bind(d)
    .bind(f.br_a)
    .bind(m)
    .bind(y)
    .bind(y.and_time(t(8, 0)).and_utc())
    .bind(y.and_time(t(12, 0)).and_utc())
    .execute(&pool)
    .await
    .unwrap();
    // g covered h's Morning: the absence stays h's (CV-6), and the cover row
    // is never h's own attendance.
    let g = person("Ghada").await;
    let h = person("Hani").await;
    pattern(&pool, &f, h, m, None).await;
    sqlx::query(
        "INSERT INTO attendance_records (org_id, employee_id, branch_id, work_shift_id, \
             business_date, status, check_in_at, check_out_at, check_in_method, \
             covered_employee_id, cover_status) \
         VALUES ($1, $2, $3, $4, $5, 'present', $6, $7, 'cover', $8, 'confirmed')",
    )
    .bind(f.org)
    .bind(g)
    .bind(f.br_a)
    .bind(m)
    .bind(y)
    .bind(y.and_time(t(8, 30)).and_utc())
    .bind(y.and_time(t(12, 0)).and_utc())
    .bind(h)
    .execute(&pool)
    .await
    .unwrap();
    // l: on approved leave → on_leave, not absent.
    let l = person("Laila").await;
    pattern(&pool, &f, l, m, None).await;
    sqlx::query(
        "INSERT INTO staff_requests (org_id, employee_id, kind, on_date, end_date, status, \
             decided_at) VALUES ($1, $2, 'leave', $3, $3, 'approved', now())",
    )
    .bind(f.org)
    .bind(l)
    .bind(y)
    .execute(&pool)
    .await
    .unwrap();

    madar_rust::staff::jobs::mark_absences(&pool).await.unwrap();

    assert_eq!(
        absences(&pool, f.a, y).await,
        vec![(Some(e), "absent".into())]
    );
    assert_eq!(
        absences(&pool, f.b, y).await,
        vec![(Some(m), "absent".into())]
    );
    assert!(absences(&pool, c, y).await.is_empty());
    assert_eq!(
        absences(&pool, d, y).await,
        vec![(Some(e), "absent".into())]
    );
    assert_eq!(
        absences(&pool, h, y).await,
        vec![(Some(m), "absent".into())]
    );
    assert!(absences(&pool, g, y).await.is_empty());
    assert_eq!(
        absences(&pool, l, y).await,
        vec![(Some(m), "on_leave".into())]
    );

    // Idempotent: a second pass adds nothing.
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attendance_records")
        .fetch_one(&pool)
        .await
        .unwrap();
    madar_rust::staff::jobs::mark_absences(&pool).await.unwrap();
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attendance_records")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before, after);
}

#[sqlx::test]
async fn the_sweep_works_in_the_branch_day_and_skips_a_confirmed_holiday(pool: PgPool) {
    let f = seed(&pool).await;
    let y = today() - Duration::days(1);
    let m = block(&pool, &f, Some(f.br_a), "Morning", t(8, 0), t(12, 0)).await;
    pattern(&pool, &f, f.a, m, None).await;
    // x's branch B is far ahead of UTC: its "yesterday" differs.
    let mb = block(&pool, &f, Some(f.br_b), "B morning", t(8, 0), t(9, 0)).await;
    pattern(&pool, &f, f.x, mb, None).await;
    sqlx::query("UPDATE branches SET timezone = 'Etc/GMT-14'::timezone_name WHERE id = $1")
        .bind(f.br_b)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO staff_holidays (org_id, on_date, name_en, name_ar, decision) \
         VALUES ($1, $2, 'Feast', 'عيد', 'holiday')",
    )
    .bind(f.org)
    .bind(y)
    .execute(&pool)
    .await
    .unwrap();
    madar_rust::staff::jobs::mark_absences(&pool).await.unwrap();
    assert!(absences(&pool, f.a, y).await.is_empty(), "a holiday");

    // At UTC+14 the local day is ahead: its yesterday is UTC's today (unless
    // the holiday), so the B-morning shift of the local yesterday is marked.
    let local_today: NaiveDate =
        sqlx::query_scalar("SELECT (now() AT TIME ZONE 'Etc/GMT-14')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    let local_y = local_today - Duration::days(1);
    if local_y != y {
        assert_eq!(
            absences(&pool, f.x, local_y).await,
            vec![(Some(mb), "absent".into())]
        );
    }
    // Never two local days back.
    assert!(
        absences(&pool, f.x, local_y - Duration::days(1))
            .await
            .is_empty()
    );
}

// ── split days through swaps, claims and suggestions ───────────────────────

#[sqlx::test]
async fn a_swap_moves_one_block_and_keeps_the_rest_of_both_days(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let m = block(&pool, &f, Some(f.br_a), "Morning", t(8, 0), t(12, 0)).await;
    let e = block(&pool, &f, Some(f.br_a), "Evening", t(17, 0), t(21, 0)).await;
    let l = block(&pool, &f, Some(f.br_a), "Lunch", t(13, 0), t(16, 0)).await;
    let d = today() + Duration::days(3);
    publish(&app, &f, f.br_a, d).await;
    let owner = f.owner();
    for (who, shifts) in [(f.a, vec![m, e]), (f.b, vec![l])] {
        let list: Vec<Value> = shifts
            .iter()
            .map(|s| json!({ "work_shift_id": s }))
            .collect();
        let (s, body) = done(call!(
            app,
            "PUT",
            "/staff/schedules/days",
            owner,
            json!({ "employee_id": who, "on_date": d, "shifts": list })
        ))
        .await;
        assert_eq!(s, 200, "{body}");
    }
    sqlx::query("DELETE FROM staff_notifications")
        .execute(&pool)
        .await
        .unwrap();
    let (ta, tb) = (phone_token(&pool, f.a).await, phone_token(&pool, f.b).await);
    // MY shift first, the colleague's second (06 B2).
    let (s, swap) = done(call!(
        app,
        "POST",
        "/staff/me/swaps",
        ta,
        json!({ "my_date": d, "my_shift_id": m, "peer_id": f.b, "peer_date": d, "peer_shift_id": l })
    ))
    .await;
    assert_eq!(s, 201, "{swap}");
    assert_eq!(swap["requester_shift_id"], json!(m));
    assert_eq!(swap["peer_shift_id"], json!(l));
    let id = swap["id"].as_str().unwrap().to_string();
    let (s, _) = done(call!(
        app,
        "PATCH",
        format!("/staff/me/swaps/{id}"),
        tb,
        json!({ "approve": true })
    ))
    .await;
    assert_eq!(s, 200);
    let (s, body) = done(call!(
        app,
        "PATCH",
        format!("/staff/swaps/{id}/decision"),
        owner,
        json!({ "approve": true })
    ))
    .await;
    assert_eq!(s, 204, "{body}");
    assert_eq!(
        shifts_on(&pool, f.a, d).await,
        vec![l, e],
        "Amal keeps her evening"
    );
    assert_eq!(shifts_on(&pool, f.b, d).await, vec![m]);
    // One decision only.
    let (s, _) = done(call!(
        app,
        "PATCH",
        format!("/staff/swaps/{id}/decision"),
        owner,
        json!({ "approve": true })
    ))
    .await;
    assert_eq!(s, 404);
    // Told once, by the approval; both days are marked changed.
    let keys = keys_for(&pool, f.a).await;
    assert!(
        keys.contains(&"staff.n_swap_approved".to_string()),
        "{keys:?}"
    );
    assert!(
        !keys.contains(&"staff.n_shift_changed".to_string()),
        "{keys:?}"
    );
    assert!(changed(&pool, f.a, d).await && changed(&pool, f.b, d).await);
}

#[sqlx::test]
async fn a_swap_is_refused_when_it_cant_happen(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let m = block(&pool, &f, Some(f.br_a), "Morning", t(8, 0), t(12, 0)).await;
    let e = block(&pool, &f, Some(f.br_a), "Evening", t(17, 0), t(21, 0)).await;
    let long = block(&pool, &f, Some(f.br_a), "Long", t(10, 0), t(18, 0)).await;
    let bm = block(&pool, &f, Some(f.br_b), "B morning", t(8, 0), t(12, 0)).await;
    pattern(&pool, &f, f.a, m, None).await;
    pattern(&pool, &f, f.b, long, None).await;
    pattern(&pool, &f, f.x, bm, None).await;
    let d = today() + Duration::days(3);
    let past = today() - Duration::days(1);
    publish(&app, &f, f.br_a, d).await;
    publish(&app, &f, f.br_a, past).await;
    publish(&app, &f, f.br_b, d).await;
    let (ta, tb) = (phone_token(&pool, f.a).await, phone_token(&pool, f.b).await);
    let ask = |my: NaiveDate, mine: Uuid, peer: Uuid, pd: NaiveDate, theirs: Uuid| {
        json!({ "my_date": my, "my_shift_id": mine, "peer_id": peer, "peer_date": pd,
                "peer_shift_id": theirs })
    };

    // Reversed arguments (the old app bug) name shifts nobody has: refused.
    let (s, _) = done(call!(
        app,
        "POST",
        "/staff/me/swaps",
        ta,
        ask(d, long, f.b, d, m)
    ))
    .await;
    assert_eq!(s, 409);
    refused!(
        call!(
            app,
            "POST",
            "/staff/me/swaps",
            ta,
            ask(past, m, f.b, past, long)
        ),
        409,
        "SWAP_STARTED"
    );
    refused!(
        call!(app, "POST", "/staff/me/swaps", ta, ask(d, m, f.x, d, bm)),
        409,
        "SWAP_OTHER_BRANCH"
    );
    let unpublished = d + Duration::days(21);
    refused!(
        call!(
            app,
            "POST",
            "/staff/me/swaps",
            ta,
            ask(unpublished, m, f.b, unpublished, long)
        ),
        409,
        "WEEK_NOT_PUBLISHED"
    );
    // Long (10–18) beside Amal's evening (17–21) would overlap.
    let (s, body) = done(call!(
        app,
        "PUT",
        "/staff/schedules/days",
        f.owner(),
        json!({ "employee_id": f.a, "on_date": d,
                "shifts": [{ "work_shift_id": m }, { "work_shift_id": e }] })
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    refused!(
        call!(app, "POST", "/staff/me/swaps", ta, ask(d, m, f.b, d, long)),
        409,
        "SHIFTS_OVERLAP"
    );

    // A stale swap: agreed, then the roster changed under it.
    let d2 = d + Duration::days(1);
    publish(&app, &f, f.br_a, d2).await;
    let (s, swap) = done(call!(
        app,
        "POST",
        "/staff/me/swaps",
        ta,
        ask(d2, m, f.b, d2, long)
    ))
    .await;
    assert_eq!(s, 201, "{swap}");
    let id = swap["id"].as_str().unwrap().to_string();
    // Only the requester cancels; the colleague answers.
    let (s, _) = done(call!(
        app,
        "POST",
        format!("/staff/me/swaps/{id}/cancel"),
        tb
    ))
    .await;
    assert_eq!(s, 404);
    let (s, _) = done(call!(
        app,
        "PATCH",
        format!("/staff/me/swaps/{id}"),
        tb,
        json!({ "approve": true })
    ))
    .await;
    assert_eq!(s, 200);
    let (s, body) = done(call!(
        app,
        "PUT",
        "/staff/schedules/days",
        f.owner(),
        json!({ "employee_id": f.a, "on_date": d2, "shifts": [] })
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    refused!(
        call!(
            app,
            "PATCH",
            format!("/staff/swaps/{id}/decision"),
            f.owner(),
            json!({ "approve": true })
        ),
        409,
        "SWAP_STALE"
    );
    let status: String = sqlx::query_scalar("SELECT status FROM staff_swaps WHERE id = $1::uuid")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "pending", "a refused approval changes nothing");
    assert!(shifts_on(&pool, f.b, d2).await == vec![long]);

    // The requester takes it back; the colleague hears; once only.
    let (s, body) = done(call!(
        app,
        "POST",
        format!("/staff/me/swaps/{id}/cancel"),
        ta
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["status"], json!("cancelled"));
    assert!(
        keys_for(&pool, f.b)
            .await
            .contains(&"staff.n_swap_cancelled".to_string())
    );
    let (s, _) = done(call!(
        app,
        "POST",
        format!("/staff/me/swaps/{id}/cancel"),
        ta
    ))
    .await;
    assert_eq!(s, 404);
}

#[sqlx::test]
async fn a_claimed_open_shift_joins_the_rest_of_the_day(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let m = block(&pool, &f, Some(f.br_a), "Morning", t(8, 0), t(12, 0)).await;
    let e = block(&pool, &f, Some(f.br_a), "Evening", t(17, 0), t(21, 0)).await;
    let l = block(&pool, &f, Some(f.br_a), "Lunch", t(13, 0), t(16, 0)).await;
    let wide = block(&pool, &f, Some(f.br_a), "Wide", t(11, 0), t(14, 0)).await;
    let d = today() + Duration::days(3);
    override_row(&pool, &f, f.a, d, Some(m)).await;
    override_row(&pool, &f, f.a, d, Some(e)).await;
    let owner = f.owner();
    let post = async |shift: Uuid, on: NaiveDate| -> String {
        let (s, body) = done(call!(
            app,
            "POST",
            "/staff/open-shifts",
            owner,
            json!({ "branch_id": f.br_a, "work_shift_id": shift, "on_date": on })
        ))
        .await;
        assert_eq!(s, 201, "{body}");
        body["id"].as_str().unwrap().to_string()
    };
    let ta = phone_token(&pool, f.a).await;
    let lunch = post(l, d).await;
    refused!(
        call!(app, "POST", format!("/staff/open-shifts/{lunch}/claim"), ta),
        409,
        "WEEK_NOT_PUBLISHED"
    );
    publish(&app, &f, f.br_a, d).await;
    let again = post(m, d).await;
    refused!(
        call!(app, "POST", format!("/staff/open-shifts/{again}/claim"), ta),
        409,
        "ALREADY_ROSTERED"
    );
    let clash = post(wide, d).await;
    refused!(
        call!(app, "POST", format!("/staff/open-shifts/{clash}/claim"), ta),
        409,
        "SHIFTS_OVERLAP"
    );
    let (s, body) = done(call!(
        app,
        "POST",
        format!("/staff/open-shifts/{lunch}/claim"),
        ta
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    let (s, _) = done(call!(
        app,
        "PATCH",
        format!("/staff/open-shifts/{lunch}/decision"),
        owner,
        json!({ "approve": true })
    ))
    .await;
    assert_eq!(s, 204);
    assert_eq!(shifts_on(&pool, f.a, d).await, vec![m, l, e]);
    let keys = keys_for(&pool, f.a).await;
    assert!(
        keys.contains(&"staff.n_claim_approved".to_string()),
        "{keys:?}"
    );
    assert!(
        !keys.contains(&"staff.n_shift_changed".to_string()),
        "told once: {keys:?}"
    );
    assert!(changed(&pool, f.a, d).await);
    let (s, _) = done(call!(
        app,
        "PATCH",
        format!("/staff/open-shifts/{lunch}/decision"),
        owner,
        json!({ "approve": true })
    ))
    .await;
    assert_eq!(s, 404, "one decision only");
    // A filled shift can't be cancelled.
    let (s, _) = done(call!(
        app,
        "POST",
        format!("/staff/open-shifts/{lunch}/cancel"),
        owner
    ))
    .await;
    assert_eq!(s, 409);
}

#[sqlx::test]
async fn an_open_shift_can_be_taken_back_and_its_claimer_hears(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let l = block(&pool, &f, Some(f.br_a), "Lunch", t(13, 0), t(16, 0)).await;
    let lb = block(&pool, &f, Some(f.br_b), "B lunch", t(13, 0), t(16, 0)).await;
    let d = today() + Duration::days(3);
    publish(&app, &f, f.br_a, d).await;
    let (s, body) = done(call!(
        app,
        "POST",
        "/staff/open-shifts",
        f.owner(),
        json!({ "branch_id": f.br_a, "work_shift_id": l, "on_date": d })
    ))
    .await;
    assert_eq!(s, 201, "{body}");
    let id = body["id"].as_str().unwrap().to_string();
    let tb = phone_token(&pool, f.b).await;
    let (s, _) = done(call!(
        app,
        "POST",
        format!("/staff/open-shifts/{id}/claim"),
        tb
    ))
    .await;
    assert_eq!(s, 200);
    // The teller holds no roster right.
    let (s, _) = done(call!(
        app,
        "POST",
        format!("/staff/open-shifts/{id}/cancel"),
        f.teller()
    ))
    .await;
    assert_eq!(s, 403);
    let (s, _) = done(call!(
        app,
        "POST",
        format!("/staff/open-shifts/{id}/cancel"),
        f.manager()
    ))
    .await;
    assert_eq!(s, 204);
    assert!(
        keys_for(&pool, f.b)
            .await
            .contains(&"staff.n_open_shift_cancelled".to_string())
    );
    let (s, _) = done(call!(
        app,
        "POST",
        format!("/staff/open-shifts/{id}/cancel"),
        f.owner()
    ))
    .await;
    assert_eq!(s, 409, "already cancelled");
    // Nobody can claim a cancelled shift.
    let (s, _) = done(call!(
        app,
        "POST",
        format!("/staff/open-shifts/{id}/claim"),
        tb
    ))
    .await;
    assert_eq!(s, 409);
    // Branch B's open shift is not the A manager's to cancel.
    let (s, body) = done(call!(
        app,
        "POST",
        "/staff/open-shifts",
        f.owner(),
        json!({ "branch_id": f.br_b, "work_shift_id": lb, "on_date": d })
    ))
    .await;
    assert_eq!(s, 201, "{body}");
    let bid = body["id"].as_str().unwrap().to_string();
    let (s, _) = done(call!(
        app,
        "POST",
        format!("/staff/open-shifts/{bid}/cancel"),
        f.manager()
    ))
    .await;
    assert_eq!(s, 403);
}

/// Next week's Saturday: a week the suggestion tests can fill.
fn next_week() -> NaiveDate {
    week_start(today()) + Duration::days(7)
}

#[sqlx::test]
async fn an_accepted_suggestion_adds_a_block_beside_the_rest_of_the_day(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let ws = next_week();
    let d = ws + Duration::days(2);
    let m = block(&pool, &f, Some(f.br_a), "Morning", t(9, 0), t(12, 0)).await;
    let e = block(&pool, &f, Some(f.br_a), "Evening", t(15, 0), t(18, 0)).await;
    // The pattern puts Bassem on the evening; that date he's off and can't
    // work, so the evening has a gap. Amal works that morning only.
    pattern(&pool, &f, f.b, e, None).await;
    pattern(&pool, &f, f.a, m, Some(dow(d))).await;
    override_row(&pool, &f, f.b, d, None).await;
    sqlx::query("UPDATE employees SET cant_work_days = $2 WHERE id = $1")
        .bind(f.b)
        .bind(vec![dow(d)])
        .execute(&pool)
        .await
        .unwrap();

    let uri = format!(
        "/staff/roster/suggestions?branch_id={}&week_start={ws}",
        f.br_a
    );
    let (s, list) = done(call!(app, "GET", uri, f.manager())).await;
    assert_eq!(s, 200, "{list}");
    let sug = list
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["date"] == json!(d) && x["work_shift_id"] == json!(e))
        .unwrap_or_else(|| panic!("no evening suggestion: {list}"))
        .clone();
    assert_eq!(sug["employee_id"], json!(f.a));
    assert_eq!(sug["start_time"], json!("15:00:00"));

    // A crafted id — someone of another business moved onto the shift — is
    // not one the engine made: refused, nothing written.
    let stranger = Uuid::new_v4();
    let crafted = format!("move|{d}|{e}|{stranger}|{}", f.a);
    refused!(
        call!(
            app,
            "POST",
            "/staff/roster/suggestions/decide",
            f.manager(),
            json!({ "branch_id": f.br_a, "id": crafted, "accept": true })
        ),
        409,
        "SUGGESTION_STALE"
    );
    let crafted = format!("add|{d}|{e}|{}", f.b);
    refused!(
        call!(
            app,
            "POST",
            "/staff/roster/suggestions/decide",
            f.manager(),
            json!({ "branch_id": f.br_a, "id": crafted, "accept": true })
        ),
        409,
        "SUGGESTION_STALE"
    );
    let (s, _) = done(call!(
        app,
        "POST",
        "/staff/roster/suggestions/decide",
        f.manager(),
        json!({ "branch_id": f.br_a, "id": "garbage", "accept": true })
    ))
    .await;
    assert_eq!(s, 400);
    // Branch B's suggestions are not the A manager's.
    let (s, _) = done(call!(
        app,
        "POST",
        "/staff/roster/suggestions/decide",
        f.manager(),
        json!({ "branch_id": f.br_b, "id": sug["id"], "accept": true })
    ))
    .await;
    assert_eq!(s, 403);
    let n: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM staff_schedule_overrides WHERE employee_id <> $1")
            .bind(f.b)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 0, "no refused decision wrote a roster row");

    let (s, body) = done(call!(
        app,
        "POST",
        "/staff/roster/suggestions/decide",
        f.manager(),
        json!({ "branch_id": f.br_a, "id": sug["id"], "accept": true })
    ))
    .await;
    assert_eq!(s, 204, "{body}");
    assert_eq!(
        shifts_on(&pool, f.a, d).await,
        vec![m, e],
        "Amal keeps her morning"
    );
    // Decided once: it is not offered again.
    let (_, list) = done(call!(app, "GET", uri, f.manager())).await;
    assert!(
        !list
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"] == sug["id"])
    );
}

#[sqlx::test]
async fn a_stale_week_is_recomputed_at_most_every_thirty_seconds(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let ws = next_week();
    let m = block(&pool, &f, Some(f.br_a), "Morning", t(9, 0), t(12, 0)).await;
    pattern(&pool, &f, f.a, m, None).await;
    let uri = format!(
        "/staff/roster/suggestions?branch_id={}&week_start={ws}",
        f.br_a
    );
    let cache = async || -> (bool, DateTime<Utc>) {
        sqlx::query_as(
            "SELECT stale, computed_at FROM staff_suggestion_cache \
              WHERE branch_id = $1 AND week_start = $2",
        )
        .bind(f.br_a)
        .bind(ws)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let (s, _) = done(call!(app, "GET", uri, f.manager())).await;
    assert_eq!(s, 200);
    let (stale, first) = cache().await;
    assert!(!stale);

    // A roster edit marks the kept week stale…
    let (s, _) = done(call!(
        app,
        "PUT",
        "/staff/schedules/days",
        f.owner(),
        json!({ "employee_id": f.b, "on_date": ws + Duration::days(1),
                "shifts": [{ "work_shift_id": m }] })
    ))
    .await;
    assert_eq!(s, 200);
    assert!(cache().await.0);
    // …which is still served inside the 30 s window,
    let (s, _) = done(call!(app, "GET", uri, f.manager())).await;
    assert_eq!(s, 200);
    assert_eq!(cache().await, (true, first));
    // and recomputed once the window has passed.
    sqlx::query(
        "UPDATE staff_suggestion_cache SET computed_at = now() - INTERVAL '31 seconds' \
          WHERE branch_id = $1",
    )
    .bind(f.br_a)
    .execute(&pool)
    .await
    .unwrap();
    let (s, _) = done(call!(app, "GET", uri, f.manager())).await;
    assert_eq!(s, 200);
    let (stale, again) = cache().await;
    assert!(!stale);
    assert!(again > first);
    // A change to the engine's inputs (a block) drops the week outright.
    sqlx::query("UPDATE work_shifts SET name = 'Early' WHERE id = $1")
        .bind(m)
        .execute(&pool)
        .await
        .unwrap();
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM staff_suggestion_cache")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[sqlx::test]
async fn the_owner_hears_when_learning_freezes_and_resumes(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let events = async |accepted: bool, n: usize| {
        for i in 0..n {
            sqlx::query(
                "INSERT INTO staff_suggestion_events (org_id, branch_id, suggestion, employee_id, \
                     on_date, accepted, source) VALUES ($1, $2, $3, $4, CURRENT_DATE, $5, 'suggestion')",
            )
            .bind(f.org)
            .bind(f.br_a)
            .bind(format!("add|x|{accepted}|{i}"))
            .bind(f.a)
            .bind(accepted)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    // One of six accepted in four weeks: under 40%.
    events(true, 1).await;
    events(false, 5).await;
    let month = today();
    let (s, body) = done(call!(
        app,
        "GET",
        format!("/staff/roster/fairness?month={month}"),
        f.owner()
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(body["learning_frozen"], json!(true));
    let a = body["branches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["branch_id"] == json!(f.br_a))
        .unwrap();
    assert_eq!(a["learning_frozen"], json!(true));
    assert_eq!(
        keys_for(&pool, f.owner_emp).await,
        vec!["staff.n_learning_frozen".to_string()],
        "told once"
    );
    // Asking again is not news.
    let _ = call!(
        app,
        "GET",
        format!("/staff/roster/fairness?month={month}"),
        f.owner()
    );
    assert_eq!(keys_for(&pool, f.owner_emp).await.len(), 1);
    // Managers accept again: learning resumes, and the owner hears.
    events(true, 10).await;
    let (s, body) = done(call!(
        app,
        "GET",
        format!("/staff/roster/fairness?month={month}"),
        f.owner()
    ))
    .await;
    assert_eq!(s, 200);
    assert_eq!(body["learning_frozen"], json!(false));
    assert!(
        keys_for(&pool, f.owner_emp)
            .await
            .contains(&"staff.n_learning_resumed".to_string())
    );
    // The fairness view is the owner's alone.
    let (s, _) = done(call!(
        app,
        "GET",
        format!("/staff/roster/fairness?month={month}"),
        f.manager()
    ))
    .await;
    assert_eq!(s, 403);
}

#[sqlx::test]
async fn the_monthly_fairness_audit_runs_once_per_branch_and_tells_the_owner(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    // Last month: every night went to the men, while only a woman said she
    // wants evenings — a gap far over 20 points at branch A.
    let night = block(&pool, &f, Some(f.br_a), "Night", t(22, 0), t(6, 0)).await;
    for (who, g) in [(f.a, "f"), (f.b, "m")] {
        sqlx::query("UPDATE employees SET gender = $2 WHERE id = $1")
            .bind(who)
            .bind(g)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE employees SET pref_time = 'evening' WHERE id = $1")
        .bind(f.a)
        .execute(&pool)
        .await
        .unwrap();
    let last_month: NaiveDate = sqlx::query_scalar(
        "SELECT (date_trunc('month', now() AT TIME ZONE 'UTC') - INTERVAL '1 month')::date",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    for i in 0..4 {
        let day = last_month + Duration::days(i);
        sqlx::query(
            "INSERT INTO attendance_records (org_id, employee_id, branch_id, work_shift_id, \
                 business_date, status, scheduled_start_at, scheduled_end_at) \
             VALUES ($1, $2, $3, $4, $5, 'present', $6, $7)",
        )
        .bind(f.org)
        .bind(f.b)
        .bind(f.br_a)
        .bind(night)
        .bind(day)
        .bind(day.and_time(t(22, 0)).and_utc())
        .bind((day + Duration::days(1)).and_time(t(6, 0)).and_utc())
        .execute(&pool)
        .await
        .unwrap();
    }
    madar_rust::staff::dawam::suggest::monthly_fairness(&pool)
        .await
        .unwrap();
    let audits: Vec<(Uuid, NaiveDate, bool)> = sqlx::query_as(
        "SELECT branch_id, month, flagged FROM staff_fairness_audits WHERE org_id = $1 ORDER BY branch_id",
    )
    .bind(f.org)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(audits.len(), 2, "each branch with people: {audits:?}");
    let a = audits.iter().find(|x| x.0 == f.br_a).unwrap();
    assert_eq!(a.1, last_month);
    assert!(a.2, "flagged");
    let keys = keys_for(&pool, f.owner_emp).await;
    assert!(
        keys.contains(&"staff.n_fairness_flagged".to_string()),
        "{keys:?}"
    );
    assert!(
        keys.contains(&"staff.n_fairness_ready".to_string()),
        "{keys:?}"
    );
    // Once a month.
    madar_rust::staff::dawam::suggest::monthly_fairness(&pool)
        .await
        .unwrap();
    assert_eq!(keys_for(&pool, f.owner_emp).await.len(), keys.len());

    let (s, list) = done(call!(
        app,
        "GET",
        "/staff/roster/fairness/audits",
        f.owner()
    ))
    .await;
    assert_eq!(s, 200, "{list}");
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert!(list[0]["report"]["gap_points"].as_i64().is_some());
    let (s, _) = done(call!(
        app,
        "GET",
        "/staff/roster/fairness/audits",
        f.manager()
    ))
    .await;
    assert_eq!(s, 403);
}

// ── refusals and branch scope ──────────────────────────────────────────────

#[sqlx::test]
async fn day_edits_refuse_blocks_that_cant_be_rostered_there(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let m = block(&pool, &f, Some(f.br_a), "Morning", t(8, 0), t(12, 0)).await;
    let m2 = block(&pool, &f, Some(f.br_a), "Morning 2", t(10, 0), t(14, 0)).await;
    let bm = block(&pool, &f, Some(f.br_b), "B morning", t(8, 0), t(12, 0)).await;
    let off = block(&pool, &f, Some(f.br_a), "Old", t(8, 0), t(12, 0)).await;
    sqlx::query("UPDATE work_shifts SET is_active = false WHERE id = $1")
        .bind(off)
        .execute(&pool)
        .await
        .unwrap();
    let d = today() + Duration::days(3);
    let put = |shifts: Value| json!({ "employee_id": f.a, "on_date": d, "shifts": shifts });
    let owner = f.owner();
    refused!(
        call!(
            app,
            "PUT",
            "/staff/schedules/days",
            owner,
            put(json!([{ "work_shift_id": bm }]))
        ),
        400,
        "SHIFT_OTHER_BRANCH"
    );
    refused!(
        call!(
            app,
            "PUT",
            "/staff/schedules/days",
            owner,
            put(json!([{ "work_shift_id": off }]))
        ),
        400,
        "SHIFT_INACTIVE"
    );
    refused!(
        call!(
            app,
            "PUT",
            "/staff/schedules/days",
            owner,
            put(json!([{ "work_shift_id": m, "start_time": "09:00:00", "end_time": "09:00:00" }]))
        ),
        400,
        "SHIFT_EMPTY"
    );
    refused!(
        call!(
            app,
            "PUT",
            "/staff/schedules/days",
            owner,
            put(json!([{ "work_shift_id": m }, { "work_shift_id": m2 }]))
        ),
        409,
        "SHIFTS_OVERLAP"
    );
    let (s, _) = done(call!(
        app,
        "PUT",
        "/staff/schedules/days",
        owner,
        put(json!([{ "work_shift_id": m }, { "work_shift_id": m }]))
    ))
    .await;
    assert_eq!(s, 400, "a block once a day");
    let (s, _) = done(call!(
        app,
        "PUT",
        "/staff/schedules/overrides",
        owner,
        json!({ "employee_id": f.a, "on_date": d, "start_time": "09:00:00", "end_time": "10:00:00" })
    ))
    .await;
    assert_eq!(s, 400, "a day off has no times");
    refused!(
        call!(
            app,
            "POST",
            "/staff/schedules",
            owner,
            json!({ "employee_id": f.a, "work_shift_id": bm })
        ),
        400,
        "SHIFT_OTHER_BRANCH"
    );
    // A pattern row that would overlap an existing one.
    pattern(&pool, &f, f.a, m, None).await;
    refused!(
        call!(
            app,
            "POST",
            "/staff/schedules",
            owner,
            json!({ "employee_id": f.a, "work_shift_id": m2 })
        ),
        409,
        "SHIFTS_OVERLAP"
    );
    // A refused edit leaves nothing behind.
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM staff_schedule_overrides")
            .fetch_one(&pool)
            .await
            .unwrap()
            == 0
    );
}

#[sqlx::test]
async fn a_shift_moves_to_a_colleague_and_both_keep_the_rest(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let m = block(&pool, &f, Some(f.br_a), "Morning", t(8, 0), t(12, 0)).await;
    let e = block(&pool, &f, Some(f.br_a), "Evening", t(17, 0), t(21, 0)).await;
    let l = block(&pool, &f, Some(f.br_a), "Lunch", t(13, 0), t(16, 0)).await;
    pattern(&pool, &f, f.a, m, None).await;
    pattern(&pool, &f, f.a, e, None).await;
    pattern(&pool, &f, f.b, l, None).await;
    let d = today() + Duration::days(3);
    // Amal's morning carries its own times; they move with it.
    let (s, _) = done(call!(
        app,
        "PUT",
        "/staff/schedules/days/times",
        f.manager(),
        json!({ "employee_id": f.a, "on_date": d, "work_shift_id": m,
                "start_time": "07:30:00", "end_time": "11:30:00" })
    ))
    .await;
    assert_eq!(s, 200);
    let (s, body) = done(call!(
        app,
        "POST",
        "/staff/schedules/days/move",
        f.manager(),
        json!({ "employee_id": f.a, "to_employee_id": f.b, "on_date": d, "work_shift_id": m })
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(shifts_on(&pool, f.a, d).await, vec![e]);
    assert_eq!(shifts_on(&pool, f.b, d).await, vec![m, l]);
    assert_eq!(body["to"]["shifts"][0]["start_time"], json!("07:30:00"));
    assert_eq!(body["to"]["shifts"][0]["times_edited"], json!(true));
    // Not hers any more.
    refused!(
        call!(
            app,
            "POST",
            "/staff/schedules/days/move",
            f.manager(),
            json!({ "employee_id": f.a, "to_employee_id": f.b, "on_date": d, "work_shift_id": m })
        ),
        409,
        "NOT_ROSTERED"
    );
    let (s, _) = done(call!(
        app,
        "POST",
        "/staff/schedules/days/move",
        f.manager(),
        json!({ "employee_id": f.a, "to_employee_id": f.a, "on_date": d, "work_shift_id": e })
    ))
    .await;
    assert_eq!(s, 400);
}

#[sqlx::test]
async fn a_branch_a_manager_is_refused_every_roster_write_on_branch_b(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let am = block(&pool, &f, Some(f.br_a), "A morning", t(8, 0), t(12, 0)).await;
    let bm = block(&pool, &f, Some(f.br_b), "B morning", t(8, 0), t(12, 0)).await;
    let x_row = pattern(&pool, &f, f.x, bm, None).await;
    let d = today() + Duration::days(3);
    let x_ov = override_row(&pool, &f, f.x, d + Duration::days(1), None).await;
    let mgr = f.manager();
    let day = |who: Uuid, shift: Uuid| json!({ "employee_id": who, "on_date": d, "shifts": [{ "work_shift_id": shift }] });
    let refusals: Vec<(&str, String, Value)> = vec![
        ("PUT", "/staff/schedules/days".into(), day(f.x, bm)),
        (
            "DELETE",
            format!("/staff/schedules/days?employee_id={}&on_date={d}", f.x),
            Value::Null,
        ),
        (
            "PUT",
            "/staff/schedules/days/times".into(),
            json!({ "employee_id": f.x, "on_date": d, "work_shift_id": bm,
                    "start_time": "09:00:00", "end_time": "13:00:00" }),
        ),
        (
            "POST",
            "/staff/schedules/days/move".into(),
            json!({ "employee_id": f.x, "to_employee_id": f.a, "on_date": d, "work_shift_id": bm }),
        ),
        (
            "POST",
            "/staff/schedules/days/move".into(),
            json!({ "employee_id": f.a, "to_employee_id": f.x, "on_date": d, "work_shift_id": am }),
        ),
        (
            "PUT",
            "/staff/schedules/overrides".into(),
            json!({ "employee_id": f.x, "on_date": d, "work_shift_id": bm }),
        ),
        (
            "DELETE",
            format!("/staff/schedules/overrides/{x_ov}"),
            Value::Null,
        ),
        (
            "POST",
            "/staff/schedules".into(),
            json!({ "employee_id": f.x, "work_shift_id": bm }),
        ),
        ("DELETE", format!("/staff/schedules/{x_row}"), Value::Null),
        // A's person on B's block.
        ("PUT", "/staff/schedules/days".into(), day(f.a, bm)),
        (
            "POST",
            "/staff/work-shifts".into(),
            json!({ "branch_id": f.br_b, "name": "B late", "start_time": "16:00:00",
                    "end_time": "22:00:00" }),
        ),
        (
            "POST",
            "/staff/work-shifts".into(),
            json!({ "name": "Everywhere", "start_time": "16:00:00", "end_time": "22:00:00" }),
        ),
        (
            "PATCH",
            format!("/staff/work-shifts/{bm}"),
            json!({ "branch_id": f.br_b, "name": "B morning", "start_time": "07:00:00",
                    "end_time": "12:00:00" }),
        ),
        (
            "POST",
            "/staff/roster/publish".into(),
            json!({ "branch_id": f.br_b, "week_start": d }),
        ),
        (
            "POST",
            "/staff/open-shifts".into(),
            json!({ "branch_id": f.br_b, "work_shift_id": bm, "on_date": d }),
        ),
        (
            "GET",
            format!("/staff/roster?branch_id={}&from={d}&to={d}", f.br_b),
            Value::Null,
        ),
        (
            "GET",
            format!(
                "/staff/roster/suggestions?branch_id={}&week_start={d}",
                f.br_b
            ),
            Value::Null,
        ),
        (
            "PUT",
            format!("/staff/employees/{}/preferences", f.x),
            json!({ "pref_time": "morning", "cant_work_days": [] }),
        ),
        (
            "GET",
            format!("/staff/employees/{}/preferences/log", f.x),
            Value::Null,
        ),
    ];
    for (method, uri, body) in refusals {
        let (s, b) = done(call!(app, method, uri, mgr, body)).await;
        assert_eq!(s, 403, "{method} {uri}: {b}");
    }
    // Nothing of B's changed.
    assert!(shifts_on(&pool, f.x, d).await == vec![bm]);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM staff_schedule_overrides")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);

    // The same acts on A's side go through.
    let (s, b) = done(call!(
        app,
        "PUT",
        "/staff/schedules/days",
        mgr,
        day(f.a, am)
    ))
    .await;
    assert_eq!(s, 200, "{b}");
    let (s, b) = done(call!(
        app,
        "PUT",
        "/staff/schedules/days/times",
        mgr,
        json!({ "employee_id": f.a, "on_date": d, "work_shift_id": am,
                "start_time": "09:00:00", "end_time": "13:00:00" })
    ))
    .await;
    assert_eq!(s, 200, "{b}");
    let (s, b) = done(call!(
        app,
        "PATCH",
        format!("/staff/work-shifts/{am}"),
        mgr,
        json!({ "branch_id": f.br_a, "name": "A morning", "start_time": "07:00:00",
                "end_time": "12:00:00" })
    ))
    .await;
    assert_eq!(s, 200, "{b}");
    // A teller holds no roster right at all.
    let (s, _) = done(call!(
        app,
        "PUT",
        "/staff/schedules/days",
        f.teller(),
        day(f.b, am)
    ))
    .await;
    assert_eq!(s, 403);
}

// ── drafts and changes (SC-3, SC-4) ────────────────────────────────────────

#[sqlx::test]
async fn my_schedule_hides_draft_weeks(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let m = block(&pool, &f, Some(f.br_a), "Morning", t(8, 0), t(12, 0)).await;
    pattern(&pool, &f, f.a, m, None).await;
    let ws = next_week();
    let to = ws + Duration::days(6);
    let me = phone_token(&pool, f.a).await;
    let uri = format!("/staff/me/schedule?from={ws}&to={to}");
    let (s, body) = done(call!(app, "GET", uri, me)).await;
    assert_eq!(s, 200, "{body}");
    let days = body.as_array().unwrap();
    assert_eq!(days.len(), 7);
    assert!(
        days.iter()
            .all(|d| d["shifts"] == json!([]) && d["published"] == json!(false))
    );
    publish(&app, &f, f.br_a, ws).await;
    let (_, body) = done(call!(app, "GET", uri, me)).await;
    let days = body.as_array().unwrap();
    assert!(days.iter().all(|d| d["published"] == json!(true)));
    assert_eq!(days[0]["shifts"][0]["work_shift_id"], json!(m));
}

#[sqlx::test]
async fn published_weeks_mark_and_tell_whatever_changed_them(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let m = block(&pool, &f, Some(f.br_a), "Morning", t(8, 0), t(12, 0)).await;
    let e = block(&pool, &f, Some(f.br_a), "Evening", t(15, 0), t(19, 0)).await;
    pattern(&pool, &f, f.a, m, None).await;
    let ws = week_start(today());
    publish(&app, &f, f.br_a, ws).await;
    publish(&app, &f, f.br_a, ws + Duration::days(7)).await;
    let d = today() + Duration::days(3);
    let owner = f.owner();
    let reset = async || {
        sqlx::query("DELETE FROM staff_roster_changes")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM staff_notifications")
            .execute(&pool)
            .await
            .unwrap();
    };
    reset().await;

    // Deleting a date change (back to the pattern) is a change.
    let ov = override_row(&pool, &f, f.a, d, None).await;
    let (s, _) = done(call!(
        app,
        "DELETE",
        format!("/staff/schedules/overrides/{ov}"),
        owner
    ))
    .await;
    assert_eq!(s, 204);
    assert!(changed(&pool, f.a, d).await);
    assert_eq!(
        keys_for(&pool, f.a).await,
        vec!["staff.n_shift_changed".to_string()]
    );
    reset().await;

    // A new weekday row marks the dates it changes, and tells the person once.
    let (s, body) = done(call!(
        app,
        "POST",
        "/staff/schedules",
        owner,
        json!({ "employee_id": f.b, "work_shift_id": e, "day_of_week": dow(d) })
    ))
    .await;
    assert_eq!(s, 201, "{body}");
    let row = body["id"].as_str().unwrap().to_string();
    assert!(changed(&pool, f.b, d).await);
    assert!(
        !changed(&pool, f.b, d + Duration::days(1)).await,
        "only its weekday"
    );
    assert_eq!(keys_for(&pool, f.b).await.len(), 1);
    reset().await;
    // Deleting it again, too.
    let (s, _) = done(call!(
        app,
        "DELETE",
        format!("/staff/schedules/{row}"),
        owner
    ))
    .await;
    assert_eq!(s, 204);
    assert!(changed(&pool, f.b, d).await);
    reset().await;

    // New times on a block reach everyone on it.
    let (s, body) = done(call!(
        app,
        "PATCH",
        format!("/staff/work-shifts/{m}"),
        owner,
        json!({ "branch_id": f.br_a, "name": "Morning", "start_time": "07:00:00", "end_time": "12:00:00" })
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    assert!(changed(&pool, f.a, d).await);
    assert!(!changed(&pool, f.b, d).await, "Bassem isn't on it");
    assert_eq!(keys_for(&pool, f.a).await.len(), 1);
    reset().await;

    // A renamed block changes nobody's hours: silent.
    let (s, _) = done(call!(
        app,
        "PATCH",
        format!("/staff/work-shifts/{m}"),
        owner,
        json!({ "branch_id": f.br_a, "name": "Early", "start_time": "07:00:00", "end_time": "12:00:00" })
    ))
    .await;
    assert_eq!(s, 200);
    assert!(keys_for(&pool, f.a).await.is_empty());

    // A draft week is silent.
    let far = ws + Duration::days(28);
    let (s, _) = done(call!(
        app,
        "PUT",
        "/staff/schedules/days",
        owner,
        json!({ "employee_id": f.a, "on_date": far, "shifts": [] })
    ))
    .await;
    assert_eq!(s, 200);
    assert!(!changed(&pool, f.a, far).await);
    assert!(keys_for(&pool, f.a).await.is_empty());
    // The manager's grid shows the change marker.
    let (s, _) = done(call!(
        app,
        "PUT",
        "/staff/schedules/days",
        owner,
        json!({ "employee_id": f.a, "on_date": d, "shifts": [{ "work_shift_id": e }] })
    ))
    .await;
    assert_eq!(s, 200);
    let (_, view) = done(call!(
        app,
        "GET",
        format!("/staff/roster?branch_id={}&from={d}&to={d}", f.br_a),
        owner
    ))
    .await;
    let mine = view["shifts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["employee_id"] == json!(f.a))
        .unwrap();
    assert_eq!(mine["changed"], json!(true));
    assert_eq!(mine["from_override"], json!(true));
}

// ── after midnight (SC-10) ─────────────────────────────────────────────────

/// A fixed-offset zone where it is now between 01:00 and 02:00, so last
/// night's 20:00–04:00 shift is running and today's has not started.
fn zone_at_one_am() -> String {
    let h = Utc::now().hour() as i32;
    let mut off = 1 - h;
    if off < -12 {
        off += 24;
    }
    if off > 14 {
        off -= 24;
    }
    match off {
        0 => "Etc/UTC".into(),
        o if o > 0 => format!("Etc/GMT-{o}"),
        o => format!("Etc/GMT+{}", -o),
    }
}

#[sqlx::test]
async fn a_punch_and_a_cover_after_midnight_belong_to_last_nights_shift(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let zone = zone_at_one_am();
    sqlx::query("UPDATE branches SET timezone = $2::timezone_name WHERE id = $1")
        .bind(f.br_a)
        .bind(&zone)
        .execute(&pool)
        .await
        .unwrap();
    let local_today: NaiveDate = sqlx::query_scalar("SELECT (now() AT TIME ZONE $1)::date")
        .bind(&zone)
        .fetch_one(&pool)
        .await
        .unwrap();
    let last_night = local_today - Duration::days(1);
    let night = block(&pool, &f, Some(f.br_a), "Night", t(20, 0), t(4, 0)).await;
    pattern(&pool, &f, f.a, night, None).await;
    pattern(&pool, &f, f.b, night, None).await;
    let c = employee(
        &pool,
        f.org,
        "Cyrine",
        None,
        Some("+201060000009"),
        true,
        &[f.br_a],
        300_000,
    )
    .await;
    pattern(&pool, &f, c, night, None).await;

    // The manager punches Amal in at 01:xx: last night's shift, not today's.
    let (s, rec) = done(call!(
        app,
        "POST",
        "/staff/attendance/punch",
        f.owner(),
        json!({ "employee_id": f.a, "reason": "Phone died" })
    ))
    .await;
    assert_eq!(s, 200, "{rec}");
    assert_eq!(rec["business_date"], json!(last_night), "{zone}");
    assert_eq!(rec["work_shift_id"], json!(night));

    // Bassem never came: Cyrine covers his night after midnight.
    let tc = phone_token(&pool, c).await;
    let (s, list) = done(call!(app, "GET", "/staff/me/coverable", tc)).await;
    assert_eq!(s, 200, "{list}");
    let his = list
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["employee_id"] == json!(f.b))
        .unwrap_or_else(|| panic!("{list}"));
    assert_eq!(his["business_date"], json!(last_night));
    // Amal is punched in: her night is not coverable.
    assert!(
        !list
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["employee_id"] == json!(f.a))
    );
    let (s, cover) = done(call!(
        app,
        "POST",
        "/staff/me/cover",
        tc,
        json!({ "employee_id": f.b, "work_shift_id": night, "latitude": LAT, "longitude": LNG })
    ))
    .await;
    assert_eq!(s, 201, "{cover}");
    assert_eq!(cover["business_date"], json!(last_night));
    let id = cover["id"].as_str().unwrap().to_string();

    // Rejected: the flag says rejected (CV-3), and once only.
    let (s, body) = done(call!(
        app,
        "PATCH",
        format!("/staff/attendance/{id}/cover"),
        f.owner(),
        json!({ "approve": false })
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    let resolution: Option<String> = sqlx::query_scalar(
        "SELECT resolution FROM attendance_flags WHERE attendance_record_id = $1::uuid AND kind = 'cover'",
    )
    .bind(&id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(resolution.as_deref(), Some("rejected"));
    let (s, _) = done(call!(
        app,
        "PATCH",
        format!("/staff/attendance/{id}/cover"),
        f.owner(),
        json!({ "approve": true })
    ))
    .await;
    assert_eq!(s, 404);
}

#[sqlx::test]
async fn the_discipline_report_shows_covers_for_both_people(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let m = block(&pool, &f, Some(f.br_a), "Morning", t(8, 0), t(12, 0)).await;
    let base = today() - Duration::days(10);
    let rec =
        async |who: Uuid, day: i64, status: &str, covered: Option<Uuid>, cover: Option<&str>| {
            let on = base + Duration::days(day);
            sqlx::query(
                "INSERT INTO attendance_records (org_id, employee_id, branch_id, work_shift_id, \
                 business_date, status, check_in_method, covered_employee_id, cover_status) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(f.org)
            .bind(who)
            .bind(f.br_a)
            .bind(m)
            .bind(on)
            .bind(status)
            .bind(covered.map(|_| "cover"))
            .bind(covered)
            .bind(cover)
            .execute(&pool)
            .await
            .unwrap();
        };
    rec(f.a, 0, "present", None, None).await;
    rec(f.a, 1, "present", Some(f.b), Some("confirmed")).await;
    rec(f.a, 2, "present", Some(f.b), Some("pending")).await;
    rec(f.a, 3, "present", Some(f.b), Some("rejected")).await;
    rec(f.b, 1, "absent", None, None).await;
    rec(f.b, 2, "absent", None, None).await;
    rec(f.b, 3, "absent", None, None).await;
    let (s, body) = done(call!(
        app,
        "GET",
        format!(
            "/staff/discipline-report?from={base}&to={}",
            base + Duration::days(5)
        ),
        f.owner()
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    let row = |who: Uuid| {
        body["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["employee_id"] == json!(who))
            .unwrap()
            .clone()
    };
    let a = row(f.a);
    assert_eq!(a["present_days"], json!(1), "covers are not her own days");
    assert_eq!(a["covers_given"], json!(1), "confirmed only");
    assert_eq!(a["covers_pending"], json!(1));
    let b = row(f.b);
    assert_eq!(b["absent_days"], json!(3), "the absence stays his (CV-6)");
    assert_eq!(
        b["covered_by_others"],
        json!(2),
        "a rejected cover doesn't count"
    );
}

// ── preferences (SC-12) ────────────────────────────────────────────────────

#[sqlx::test]
async fn a_manager_overrides_preferences_and_it_is_logged(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let me = phone_token(&pool, f.a).await;
    let (s, _) = done(call!(
        app,
        "PUT",
        "/staff/me/preferences",
        me,
        json!({ "pref_time": "evening", "cant_work_days": [5, 5] })
    ))
    .await;
    assert_eq!(s, 204);
    let (s, _) = done(call!(
        app,
        "PUT",
        format!("/staff/employees/{}/preferences", f.a),
        f.manager(),
        json!({ "pref_time": "morning", "cant_work_days": [], "note": "Opening shift cover" })
    ))
    .await;
    assert_eq!(s, 204);
    let (pref, set_by): (Option<String>, String) =
        sqlx::query_as("SELECT pref_time, prefs_set_by FROM employees WHERE id = $1")
            .bind(f.a)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (pref.as_deref(), set_by.as_str()),
        (Some("morning"), "manager")
    );
    assert!(
        keys_for(&pool, f.a)
            .await
            .contains(&"staff.n_prefs_changed".to_string())
    );
    let (s, log) = done(call!(
        app,
        "GET",
        format!("/staff/employees/{}/preferences/log", f.a),
        f.manager()
    ))
    .await;
    assert_eq!(s, 200, "{log}");
    assert_eq!(log[0]["source"], json!("manager"));
    assert_eq!(log[0]["changed_by_name"], json!("Karim"));
    assert_eq!(log[0]["note"], json!("Opening shift cover"));
    assert_eq!(log[1]["source"], json!("employee"));
    assert_eq!(log[1]["cant_work_days"], json!([5]), "deduplicated");
    // The app shows who set them.
    let from = today();
    let (_, mine) = done(call!(
        app,
        "GET",
        format!("/staff/me/roster?from={from}&to={from}"),
        me
    ))
    .await;
    assert_eq!(mine["prefs_set_by"], json!("manager"));

    // Invalid, and not the teller's to change.
    let (s, _) = done(call!(
        app,
        "PUT",
        format!("/staff/employees/{}/preferences", f.b),
        f.manager(),
        json!({ "pref_time": "night", "cant_work_days": [] })
    ))
    .await;
    assert_eq!(s, 400);
    let (s, _) = done(call!(
        app,
        "PUT",
        format!("/staff/employees/{}/preferences", f.b),
        f.teller(),
        json!({ "pref_time": "morning", "cant_work_days": [] })
    ))
    .await;
    assert_eq!(s, 403);
}

// ── blocks ─────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn a_block_carries_its_own_overtime_rates_and_warns_past_the_presence_cap(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner();
    let (s, w) = done(call!(
        app,
        "POST",
        "/staff/work-shifts",
        owner,
        json!({ "branch_id": f.br_a, "name": "Double", "start_time": "08:00:00",
                "end_time": "23:00:00", "ot_day_multiplier": 1.75, "ot_night_multiplier": 2.25 })
    ))
    .await;
    assert_eq!(s, 201, "{w}");
    assert_eq!(w["ot_day_multiplier"], json!(1.75));
    assert_eq!(w["ot_night_multiplier"], json!(2.25));
    assert_eq!(
        w["over_presence_cap"],
        json!(true),
        "15 hours: a warning, not a refusal"
    );
    let id = w["id"].as_str().unwrap().to_string();
    // Omitted: kept. Null: back to the branch's rules.
    let (s, w) = done(call!(
        app,
        "PATCH",
        format!("/staff/work-shifts/{id}"),
        owner,
        json!({ "branch_id": f.br_a, "name": "Day", "start_time": "08:00:00", "end_time": "16:00:00",
                "ot_night_multiplier": null })
    ))
    .await;
    assert_eq!(s, 200, "{w}");
    assert_eq!(w["ot_day_multiplier"], json!(1.75));
    assert_eq!(w["ot_night_multiplier"], Value::Null);
    assert_eq!(w["over_presence_cap"], json!(false));
    let (s, _) = done(call!(
        app,
        "PATCH",
        format!("/staff/work-shifts/{id}"),
        owner,
        json!({ "branch_id": f.br_a, "name": "Day", "start_time": "08:00:00", "end_time": "16:00:00",
                "ot_day_multiplier": 0 })
    ))
    .await;
    assert_eq!(s, 400);
    let (s, list) = done(call!(app, "GET", "/staff/work-shifts", f.manager())).await;
    assert_eq!(s, 200, "{list}");
    assert_eq!(list[0]["ot_day_multiplier"], json!(1.75));
}

// ── consumers of the effective times ───────────────────────────────────────

#[sqlx::test]
async fn clock_in_is_judged_against_the_assignments_own_times(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let now = Utc::now();
    let (start, end) = (now - Duration::minutes(40), now + Duration::hours(3));
    if start.date_naive() != now.date_naive() || end.date_naive() != now.date_naive() {
        return; // the hours around midnight UTC: the day-shift case doesn't apply
    }
    // The block itself starts much later; only Amal's own times say "now".
    let later = (now + Duration::hours(5)).time();
    let block_end = (now + Duration::hours(7)).time();
    let s = block(&pool, &f, Some(f.br_a), "Day", later, block_end).await;
    pattern(&pool, &f, f.a, s, None).await;
    let fmt = |x: DateTime<Utc>| x.time().format("%H:%M:00").to_string();
    let (st, body) = done(call!(
        app,
        "PUT",
        "/staff/schedules/days/times",
        f.owner(),
        json!({ "employee_id": f.a, "on_date": today(), "work_shift_id": s,
                "start_time": fmt(start), "end_time": fmt(end) })
    ))
    .await;
    assert_eq!(st, 200, "{body}");
    let tok = phone_token(&pool, f.a).await;
    let (st, rec) = done(call!(
        app,
        "POST",
        "/staff/me/check-in",
        tok,
        json!({ "branch_id": f.br_a, "latitude": LAT, "longitude": LNG })
    ))
    .await;
    assert_eq!(st, 201, "{rec}");
    let due: DateTime<Utc> = serde_json::from_value(rec["scheduled_start_at"].clone()).unwrap();
    assert_eq!(
        due.time().format("%H:%M").to_string(),
        start.time().format("%H:%M").to_string()
    );
    assert!(rec["late_minutes"].as_i64().unwrap() > 0, "{rec}");
}

#[sqlx::test]
async fn team_presence_reads_the_one_roster(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    // Due since midnight, so "rostered and nothing recorded" is an absence.
    let all_day = block(&pool, &f, Some(f.br_a), "All day", t(0, 0), t(23, 59)).await;
    pattern(&pool, &f, f.a, all_day, None).await;
    // Bassem's pattern says all day, but today is his day off by date.
    pattern(&pool, &f, f.b, all_day, None).await;
    override_row(&pool, &f, f.b, today(), None).await;
    // Cyrine has no pattern; a date change gave her today.
    let c = employee(
        &pool,
        f.org,
        "Cyrine",
        None,
        None,
        false,
        &[f.br_a],
        300_000,
    )
    .await;
    override_row(&pool, &f, c, today(), Some(all_day)).await;
    let (s, body) = done(call!(
        app,
        "GET",
        format!("/staff/team/presence?branch_id={}", f.br_a),
        f.owner()
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    let state = |who: Uuid| {
        body["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["employee_id"] == json!(who))
            .map(|r| r["state"].as_str().unwrap().to_string())
    };
    assert_eq!(state(f.a).as_deref(), Some("absent"));
    assert_eq!(
        state(f.b).as_deref(),
        Some("off"),
        "a day off is not an absence"
    );
    assert_eq!(
        state(c).as_deref(),
        Some("absent"),
        "a date change rosters her"
    );
}

#[sqlx::test]
async fn swaps_and_claims_never_suggest_a_new_pattern(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let m = block(&pool, &f, Some(f.br_a), "Morning", t(8, 0), t(12, 0)).await;
    let e = block(&pool, &f, Some(f.br_a), "Evening", t(15, 0), t(19, 0)).await;
    pattern(&pool, &f, f.a, m, None).await;
    pattern(&pool, &f, f.b, m, None).await;
    let ws = next_week();
    let monday = ws + Duration::days(2);
    // Amal swapped her last four Mondays; Bassem's manager moved his.
    for k in 1..=4 {
        for (who, reason) in [(f.a, "Swap"), (f.b, "Evenings for now")] {
            sqlx::query(
                "INSERT INTO staff_schedule_overrides \
                     (org_id, employee_id, on_date, work_shift_id, reason) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(f.org)
            .bind(who)
            .bind(monday - Duration::days(7 * k))
            .bind(e)
            .bind(reason)
            .execute(&pool)
            .await
            .unwrap();
        }
    }
    let (s, list) = done(call!(
        app,
        "GET",
        format!(
            "/staff/roster/suggestions?branch_id={}&week_start={ws}",
            f.br_a
        ),
        f.owner()
    ))
    .await;
    assert_eq!(s, 200, "{list}");
    let patterns: Vec<&Value> = list
        .as_array()
        .unwrap()
        .iter()
        .filter(|x| x["reason_key"] == "staff.sg_pattern")
        .collect();
    assert_eq!(patterns.len(), 1, "{list}");
    assert_eq!(patterns[0]["employee_id"], json!(f.b));
}

/// E2E D-B1: "Confirm the cover" on the cover's own flag (the Team board)
/// confirms the COVER — it is paid (CV-5) and leaves Approvals — not only the
/// flag; and the covers list's decision resolves the flag (CV-3). One decision
/// settles both, whichever screen it came from.
#[sqlx::test]
async fn confirming_a_cover_flag_confirms_the_cover_and_deciding_a_cover_resolves_its_flag(
    pool: PgPool,
) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let m = block(&pool, &f, Some(f.br_a), "Morning", t(8, 0), t(12, 0)).await;
    let cover = async |day: i64| -> (Uuid, Uuid) {
        let rec: Uuid = sqlx::query_scalar(
            "INSERT INTO attendance_records (org_id, employee_id, branch_id, work_shift_id, \
             business_date, status, check_in_method, covered_employee_id, cover_status) \
             VALUES ($1, $2, $3, $4, $5, 'present', 'cover', $6, 'pending') RETURNING id",
        )
        .bind(f.org)
        .bind(f.a)
        .bind(f.br_a)
        .bind(m)
        .bind(today() - Duration::days(day))
        .bind(f.b)
        .fetch_one(&pool)
        .await
        .unwrap();
        let flag: Uuid = sqlx::query_scalar(
            "INSERT INTO attendance_flags (org_id, employee_id, branch_id, attendance_record_id, kind) \
             VALUES ($1, $2, $3, $4, 'cover') RETURNING id",
        )
        .bind(f.org)
        .bind(f.a)
        .bind(f.br_a)
        .bind(rec)
        .fetch_one(&pool)
        .await
        .unwrap();
        (rec, flag)
    };
    let status_of = async |rec: Uuid| -> (Option<String>, Option<String>) {
        sqlx::query_as(
            "SELECT a.cover_status, f.resolution FROM attendance_records a \
               JOIN attendance_flags f ON f.attendance_record_id = a.id AND f.kind = 'cover' \
              WHERE a.id = $1",
        )
        .bind(rec)
        .fetch_one(&pool)
        .await
        .unwrap()
    };

    // From the Team board: the flag's "Confirm the cover".
    let (rec, flag) = cover(1).await;
    let (s, body) = done(call!(
        app,
        "PATCH",
        format!("/staff/flags/{flag}"),
        f.owner(),
        json!({ "action": "confirm" })
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(
        status_of(rec).await,
        (Some("confirmed".into()), Some("confirmed".into()))
    );
    // Decided once: the covers list finds nothing pending any more.
    let (s, _) = done(call!(
        app,
        "PATCH",
        format!("/staff/attendance/{rec}/cover"),
        f.owner(),
        json!({ "approve": true })
    ))
    .await;
    assert_eq!(s, 404);

    // From the covers list (Approvals): the flag follows.
    let (rec2, _) = cover(2).await;
    let (s, body) = done(call!(
        app,
        "PATCH",
        format!("/staff/attendance/{rec2}/cover"),
        f.owner(),
        json!({ "approve": false })
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(
        status_of(rec2).await,
        (Some("rejected".into()), Some("rejected".into()))
    );

    // A cover dated in a PAID month: confirming would pay into it (AD-10) —
    // refused from the flag and from the list; rejecting pays nothing, so it
    // still goes through.
    sqlx::query(
        "INSERT INTO payroll_periods (org_id, name, start_date, end_date, status) \
         VALUES ($1, 'Closed', $2, $3, 'paid')",
    )
    .bind(f.org)
    .bind(today() - Duration::days(9))
    .bind(today() - Duration::days(6))
    .execute(&pool)
    .await
    .unwrap();
    let (rec4, flag4) = cover(7).await;
    let (s, body) = done(call!(
        app,
        "PATCH",
        format!("/staff/flags/{flag4}"),
        f.owner(),
        json!({ "action": "confirm" })
    ))
    .await;
    assert_eq!(
        (s, body["code"].clone()),
        (409, json!("PERIOD_CLOSED")),
        "{body}"
    );
    let (s, _) = done(call!(
        app,
        "PATCH",
        format!("/staff/attendance/{rec4}/cover"),
        f.owner(),
        json!({ "approve": true })
    ))
    .await;
    assert_eq!(s, 409);
    assert_eq!(status_of(rec4).await.0.as_deref(), Some("pending"));
    let (s, _) = done(call!(
        app,
        "PATCH",
        format!("/staff/attendance/{rec4}/cover"),
        f.owner(),
        json!({ "approve": false })
    ))
    .await;
    assert_eq!(s, 200);
    assert_eq!(status_of(rec4).await.0.as_deref(), Some("rejected"));

    // "Ignore" on a cover flag decides nothing: the cover still waits.
    let (rec3, flag3) = cover(3).await;
    let (s, _) = done(call!(
        app,
        "PATCH",
        format!("/staff/flags/{flag3}"),
        f.owner(),
        json!({ "action": "ignore" })
    ))
    .await;
    assert_eq!(s, 200);
    assert_eq!(status_of(rec3).await.0.as_deref(), Some("pending"));
}

/// Mac E2E R-B1 (SC-8): the manager's approval re-checks what the ask
/// checked. A swap agreed while both shifts were ahead is refused once its
/// week is no longer published (WEEK_NOT_PUBLISHED) or a shift has started
/// (SWAP_STARTED); it stays pending and neither roster moves.
#[sqlx::test]
async fn a_swap_is_not_approved_once_its_shift_started_or_its_week_is_unpublished(pool: PgPool) {
    use madar_rust::staff::dawam::week_start;
    let app = app!(pool);
    let f = seed(&pool).await;
    let m = block(&pool, &f, Some(f.br_a), "Morning", t(0, 0), t(12, 0)).await;
    let l = block(&pool, &f, Some(f.br_a), "Lunch", t(13, 0), t(16, 0)).await;
    let d = today() + Duration::days(3);
    publish(&app, &f, f.br_a, d).await;
    let owner = f.owner();
    for (who, s) in [(f.a, m), (f.b, l)] {
        let (st, body) = done(call!(
            app,
            "PUT",
            "/staff/schedules/days",
            owner,
            json!({ "employee_id": who, "on_date": d, "shifts": [{ "work_shift_id": s }] })
        ))
        .await;
        assert_eq!(st, 200, "{body}");
    }
    let (ta, tb) = (phone_token(&pool, f.a).await, phone_token(&pool, f.b).await);
    let (st, swap) = done(call!(
        app,
        "POST",
        "/staff/me/swaps",
        ta,
        json!({ "my_date": d, "my_shift_id": m, "peer_id": f.b, "peer_date": d, "peer_shift_id": l })
    ))
    .await;
    assert_eq!(st, 201, "{swap}");
    let id = swap["id"].as_str().unwrap().to_string();
    let (st, _) = done(call!(
        app,
        "PATCH",
        format!("/staff/me/swaps/{id}"),
        tb,
        json!({ "approve": true })
    ))
    .await;
    assert_eq!(st, 200);
    let status = || {
        let pool = pool.clone();
        let id = id.clone();
        async move {
            sqlx::query_scalar::<_, String>("SELECT status FROM staff_swaps WHERE id = $1::uuid")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };

    // 1. The week is no longer published.
    sqlx::query("DELETE FROM staff_week_publications WHERE branch_id = $1")
        .bind(f.br_a)
        .execute(&pool)
        .await
        .unwrap();
    let (st, body) = done(call!(
        app,
        "PATCH",
        format!("/staff/swaps/{id}/decision"),
        owner,
        json!({ "approve": true })
    ))
    .await;
    assert_eq!(st, 409, "{body}");
    assert_eq!(body["code"], "WEEK_NOT_PUBLISHED", "{body}");
    assert_eq!(status().await, "pending");

    // 2. Both shifts moved to today, and the morning one has begun.
    let today = today();
    sqlx::query(
        "INSERT INTO staff_week_publications (org_id, branch_id, week_start) VALUES ($1, $2, $3)",
    )
    .bind(f.org)
    .bind(f.br_a)
    .bind(week_start(today))
    .execute(&pool)
    .await
    .unwrap();
    for (who, s) in [(f.a, m), (f.b, l)] {
        override_row(&pool, &f, who, today, Some(s)).await;
    }
    sqlx::query("UPDATE staff_swaps SET requester_date = $2, peer_date = $2 WHERE id = $1::uuid")
        .bind(&id)
        .bind(today)
        .execute(&pool)
        .await
        .unwrap();
    let (st, body) = done(call!(
        app,
        "PATCH",
        format!("/staff/swaps/{id}/decision"),
        owner,
        json!({ "approve": true })
    ))
    .await;
    assert_eq!(st, 409, "{body}");
    assert_eq!(body["code"], "SWAP_STARTED", "{body}");
    assert_eq!(status().await, "pending");
    assert_eq!(shifts_on(&pool, f.a, today).await, vec![m], "nothing moved");
    assert_eq!(shifts_on(&pool, f.b, today).await, vec![l]);
    // Rejecting still works: nothing changes hands.
    let (st, _) = done(call!(
        app,
        "PATCH",
        format!("/staff/swaps/{id}/decision"),
        owner,
        json!({ "approve": false })
    ))
    .await;
    assert_eq!(st, 204);
    assert_eq!(status().await, "rejected");
}

/// E2E B-ROTA-1 (SC-5c): moving a block to someone already on that block
/// that day is refused (409 ALREADY_ROSTERED) and nothing changes. It used to
/// answer 200, give the first person a day off and leave the second with one
/// Morning: the day's headcount silently dropped by one.
#[sqlx::test]
async fn moving_a_block_to_someone_already_on_it_is_refused(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let m = block(&pool, &f, Some(f.br_a), "Morning", t(8, 0), t(12, 0)).await;
    pattern(&pool, &f, f.a, m, None).await;
    pattern(&pool, &f, f.b, m, None).await;
    let d = today() + Duration::days(3);
    refused!(
        call!(
            app,
            "POST",
            "/staff/schedules/days/move",
            f.manager(),
            json!({ "employee_id": f.a, "to_employee_id": f.b, "on_date": d, "work_shift_id": m })
        ),
        409,
        "ALREADY_ROSTERED"
    );
    assert_eq!(shifts_on(&pool, f.a, d).await, vec![m], "Amal keeps it");
    assert_eq!(shifts_on(&pool, f.b, d).await, vec![m]);
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM staff_schedule_overrides WHERE on_date = $1")
            .bind(d)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 0, "nothing written");
}

/// Mac E2E (roster): a claim that loses to a colleague's is 409
/// ALREADY_CLAIMED — a code the phone words — not an uncoded "Conflict:".
#[sqlx::test]
async fn claiming_a_taken_open_shift_is_already_claimed(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let l = block(&pool, &f, Some(f.br_a), "Lunch", t(13, 0), t(16, 0)).await;
    let d = today() + Duration::days(3);
    publish(&app, &f, f.br_a, d).await;
    let (s, body) = done(call!(
        app,
        "POST",
        "/staff/open-shifts",
        f.owner(),
        json!({ "branch_id": f.br_a, "work_shift_id": l, "on_date": d })
    ))
    .await;
    assert_eq!(s, 201, "{body}");
    let id = body["id"].as_str().unwrap().to_string();
    let (s, body) = done(call!(
        app,
        "POST",
        format!("/staff/open-shifts/{id}/claim"),
        phone_token(&pool, f.a).await
    ))
    .await;
    assert_eq!(s, 200, "{body}");
    refused!(
        call!(
            app,
            "POST",
            format!("/staff/open-shifts/{id}/claim"),
            phone_token(&pool, f.b).await
        ),
        409,
        "ALREADY_CLAIMED"
    );
}
