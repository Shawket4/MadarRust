//! Dawam Phase B — requests, corrections and rules (audit 04).
//!
//! One business with branches A and B (UTC, so the arithmetic reads plainly).
//! `e` works at A (no Madar account); the owner, a manager of A (`mgr`) and a
//! second manager of A (`peer`) are employees too. Salary 3,000 EGP over 30
//! working days: one day is exactly 10,000 piastres.

use actix_web::{App, http::Method, test, web};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::models::UserRole;

mod common;
use common::employees::{authed, employee, secret, session, user_token};

const SALARY: i64 = 300_000;
const DAY: i64 = 10_000;

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

macro_rules! call {
    ($app:expr, $method:expr, $uri:expr, $token:expr) => {
        call!($app, $method, $uri, $token, Value::Null)
    };
    ($app:expr, $method:expr, $uri:expr, $token:expr, $body:expr) => {{
        let mut req = authed(
            test::TestRequest::default()
                .method(Method::from_bytes($method.as_bytes()).unwrap())
                .uri(&$uri),
            &$token,
        );
        let b: Value = $body;
        if !b.is_null() {
            req = req.set_json(&b);
        }
        test::call_service(&$app, req.to_request()).await
    }};
}

async fn body(resp: actix_web::dev::ServiceResponse) -> Value {
    serde_json::from_slice(&test::read_body(resp).await).unwrap_or(Value::Null)
}

/// `(status, body)` of a call, for assertions that print the body.
macro_rules! send {
    ($($t:tt)*) => {{
        let resp = call!($($t)*);
        let status = resp.status().as_u16();
        (status, body(resp).await)
    }};
}

struct F {
    org: Uuid,
    a: Uuid,
    b: Uuid,
    owner: Uuid,
    mgr: Uuid,
    peer: Uuid,
    /// Employees.
    e: Uuid,
    e_mgr: Uuid,
    e_peer: Uuid,
    e_owner: Uuid,
    /// At B.
    x: Uuid,
    /// 09:00–17:00 at A.
    day_shift: Uuid,
    /// 09:00–13:00 and 17:00–21:00 at A: a split day.
    morning: Uuid,
    evening: Uuid,
    /// 22:00–06:00 at A.
    night: Uuid,
}

impl F {
    fn owner_token(&self) -> String {
        user_token(self.owner, self.org, UserRole::OrgAdmin)
    }
    fn mgr_token(&self) -> String {
        user_token(self.mgr, self.org, UserRole::BranchManager)
    }
    fn peer_token(&self) -> String {
        user_token(self.peer, self.org, UserRole::BranchManager)
    }
}

async fn user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, $3, $4, 'hash', $3::user_role)",
    )
    .bind(id)
    .bind(org)
    .bind(role)
    .bind(format!("{id}@t.test"))
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn shift(pool: &PgPool, org: Uuid, branch: Uuid, name: &str, from: &str, to: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO work_shifts (org_id, branch_id, name, start_time, end_time, grace_minutes) \
         VALUES ($1, $2, $3, $4::time, $5::time, 0) RETURNING id",
    )
    .bind(org)
    .bind(branch)
    .bind(name)
    .bind(from)
    .bind(to)
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
        "INSERT INTO organizations (id, name, slug, modules) VALUES ($1, 'Rules', $2, '{pos,dawam}')",
    )
    .bind(org)
    .bind(format!("org-{org}"))
    .execute(pool)
    .await
    .unwrap();
    // The business's rules: 1–30 min late costs a quarter day, 31+ half a day;
    // an absence costs one day; permissions are unpaid unless said otherwise.
    sqlx::query(
        "INSERT INTO attendance_settings (org_id, rules_saved_at, late_deduction_tiers, \
             absence_deduction_days, excused_time_paid_default) \
         VALUES ($1, now() - INTERVAL '60 days', $2, 1, false)",
    )
    .bind(org)
    .bind(json!([
        { "from_minutes": 1, "to_minutes": 30, "kind": "day_fraction", "value": "0.25" },
        { "from_minutes": 31, "to_minutes": null, "kind": "day_fraction", "value": "0.5" }
    ]))
    .execute(pool)
    .await
    .unwrap();
    let mut br = Vec::new();
    for n in ["A", "B"] {
        br.push(
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO branches (org_id, name, timezone) VALUES ($1, $2, 'UTC'::timezone_name) RETURNING id",
            )
            .bind(org)
            .bind(n)
            .fetch_one(pool)
            .await
            .unwrap(),
        );
    }
    let (a, b) = (br[0], br[1]);
    let owner = user(pool, org, "org_admin").await;
    let mgr = user(pool, org, "branch_manager").await;
    let peer = user(pool, org, "branch_manager").await;
    for m in [mgr, peer] {
        sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
            .bind(m)
            .bind(a)
            .execute(pool)
            .await
            .unwrap();
    }
    let e = employee(
        pool,
        org,
        "Eman",
        None,
        Some("+201060000001"),
        true,
        &[a],
        SALARY,
    )
    .await;
    let e_mgr = employee(
        pool,
        org,
        "Mona",
        Some(mgr),
        Some("+201060000002"),
        true,
        &[a],
        SALARY,
    )
    .await;
    let e_peer = employee(
        pool,
        org,
        "Peter",
        Some(peer),
        Some("+201060000003"),
        true,
        &[a],
        SALARY,
    )
    .await;
    let e_owner = employee(
        pool,
        org,
        "Omar",
        Some(owner),
        Some("+201060000004"),
        true,
        &[a],
        SALARY,
    )
    .await;
    let x = employee(
        pool,
        org,
        "Xavier",
        None,
        Some("+201060000005"),
        true,
        &[b],
        SALARY,
    )
    .await;
    let day_shift = shift(pool, org, a, "Day", "09:00", "17:00").await;
    let morning = shift(pool, org, a, "Morning", "09:00", "13:00").await;
    let evening = shift(pool, org, a, "Evening", "17:00", "21:00").await;
    let night = shift(pool, org, a, "Night", "22:00", "06:00").await;
    F {
        org,
        a,
        b,
        owner,
        mgr,
        peer,
        e,
        e_mgr,
        e_peer,
        e_owner,
        x,
        day_shift,
        morning,
        evening,
        night,
    }
}

async fn roster(pool: &PgPool, f: &F, who: Uuid, shifts: &[Uuid]) {
    for s in shifts {
        sqlx::query(
            "INSERT INTO staff_schedules (org_id, employee_id, work_shift_id, effective_from) \
             VALUES ($1, $2, $3, '2026-01-01')",
        )
        .bind(f.org)
        .bind(who)
        .bind(s)
        .execute(pool)
        .await
        .unwrap();
    }
}

fn at(date: &str, hm: &str) -> DateTime<Utc> {
    let d = NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap();
    let (h, m) = hm.split_once(':').unwrap();
    Utc.from_utc_datetime(
        &d.and_hms_opt(h.parse().unwrap(), m.parse().unwrap(), 0)
            .unwrap(),
    )
}

/// An attendance record, as a punch or the sweep would leave it.
#[allow(clippy::too_many_arguments)]
async fn record(
    pool: &PgPool,
    f: &F,
    who: Uuid,
    shift: Uuid,
    date: &str,
    (start, end): (DateTime<Utc>, DateTime<Utc>),
    punches: Option<(DateTime<Utc>, DateTime<Utc>)>,
    status: &str,
) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO attendance_records (org_id, employee_id, branch_id, work_shift_id, business_date, \
             status, scheduled_start_at, scheduled_end_at, check_in_at, check_out_at, \
             check_in_method, check_out_method) \
         VALUES ($1, $2, $3, $4, $5::date, $6, $7, $8, $9, $10, \
                 CASE WHEN $9::timestamptz IS NULL THEN NULL ELSE 'mobile_gps' END, \
                 CASE WHEN $10::timestamptz IS NULL THEN NULL ELSE 'mobile_gps' END) RETURNING id",
    )
    .bind(f.org)
    .bind(who)
    .bind(f.a)
    .bind(shift)
    .bind(date)
    .bind(status)
    .bind(start)
    .bind(end)
    .bind(punches.map(|p| p.0))
    .bind(punches.map(|p| p.1))
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Re-derive a record the way a manager's correction does.
async fn rederive<S>(app: &S, f: &F, rec: Uuid)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let (st, b) = send!(
        *app,
        "PATCH",
        format!("/staff/attendance/{rec}"),
        f.owner_token(),
        json!({ "reason": "re-derive" })
    );
    assert_eq!(st, 200, "{b}");
}

async fn deduction(pool: &PgPool, rec: Uuid, source: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount_piastres), 0)::bigint FROM payroll_deductions \
          WHERE attendance_record_id = $1 AND source = $2",
    )
    .bind(rec)
    .bind(source)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn file(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    f: &F,
    who: Uuid,
    body: Value,
) -> Value {
    let mut b = body;
    b["employee_id"] = json!(who);
    let (st, row) = send!(
        *app,
        "POST",
        "/staff/requests".to_string(),
        f.owner_token(),
        b
    );
    assert_eq!(st, 201, "{row}");
    row
}

async fn decide(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    token: &str,
    id: &Value,
    body: Value,
) -> (u16, Value) {
    send!(
        *app,
        "PATCH",
        format!("/staff/requests/{}/decision", id.as_str().unwrap()),
        token.to_string(),
        body
    )
}

async fn set_rule(pool: &PgPool, org: Uuid, sql: &str) {
    sqlx::query(&format!(
        "UPDATE attendance_settings SET {sql} WHERE org_id = $1 AND branch_id IS NULL"
    ))
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
}

// ── RQ-2 / RQ-3 (audit B1, P0): unpaid leave is not free ────────────────────

#[sqlx::test]
async fn unpaid_leave_without_a_type_docks_a_day_and_paid_leave_does_not(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    roster(&pool, &f, f.e, &[f.day_shift]).await;
    let d = "2026-08-10";
    let rec = record(
        &pool,
        &f,
        f.e,
        f.day_shift,
        d,
        (at(d, "09:00"), at(d, "17:00")),
        None,
        "absent",
    )
    .await;
    rederive(&app, &f, rec).await;
    assert_eq!(
        deduction(&pool, rec, "absence").await,
        DAY,
        "absent: one day"
    );

    let leave = file(&app, &f, f.e, json!({ "kind": "leave", "on_date": d })).await;
    let (st, row) = decide(
        &app,
        &f.owner_token(),
        &leave["id"],
        json!({ "status": "approved", "is_paid": false }),
    )
    .await;
    assert_eq!(st, 200, "{row}");
    let status: String = sqlx::query_scalar("SELECT status FROM attendance_records WHERE id = $1")
        .bind(rec)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "on_leave");
    assert_eq!(
        deduction(&pool, rec, "absence").await,
        DAY,
        "unpaid leave with no type is priced like an absence (B1)"
    );

    // The absence cost is the rule: priced LIKE an absence (RQ-3).
    set_rule(&pool, f.org, "absence_deduction_days = 2").await;
    rederive(&app, &f, rec).await;
    assert_eq!(deduction(&pool, rec, "absence").await, 2 * DAY);

    let (st, _) = decide(
        &app,
        &f.owner_token(),
        &leave["id"],
        json!({ "status": "cancelled", "note": "wrong day" }),
    )
    .await;
    assert_eq!(st, 200);
    let paid = file(&app, &f, f.e, json!({ "kind": "leave", "on_date": d })).await;
    let (st, _) = decide(
        &app,
        &f.owner_token(),
        &paid["id"],
        json!({ "status": "approved", "is_paid": true }),
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(
        deduction(&pool, rec, "absence").await,
        0,
        "paid leave costs nothing"
    );
}

#[sqlx::test]
async fn leave_types_and_balances_are_gone_from_the_api(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    for (m, uri) in [
        ("GET", "/staff/leave/types"),
        ("POST", "/staff/leave/types"),
        ("GET", "/staff/leave/balances"),
        ("PUT", "/staff/leave/balances"),
    ] {
        let resp = call!(app, m, uri.to_string(), f.owner_token(), json!({}));
        assert_eq!(resp.status(), 404, "{m} {uri} is retired (RQ-2, RQ-3)");
    }
}

// ── RQ-4: an approved month is closed ───────────────────────────────────────

#[sqlx::test]
async fn an_approved_month_is_closed_to_requests_and_manual_edits(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let d = "2026-08-10";
    let rec = record(
        &pool,
        &f,
        f.e,
        f.day_shift,
        d,
        (at(d, "09:00"), at(d, "17:00")),
        Some((at(d, "09:00"), at(d, "17:00"))),
        "present",
    )
    .await;
    let pending = file(
        &app,
        &f,
        f.e,
        json!({ "kind": "leave", "on_date": "2026-08-12" }),
    )
    .await;
    let approved = file(
        &app,
        &f,
        f.e,
        json!({ "kind": "late_arrival", "on_date": "2026-08-13", "to_time": "10:00:00" }),
    )
    .await;
    let (st, _) = decide(
        &app,
        &f.owner_token(),
        &approved["id"],
        json!({ "status": "approved" }),
    )
    .await;
    assert_eq!(st, 200);
    // August's payroll is approved: `generated` (RQ-4).
    sqlx::query(
        "INSERT INTO payroll_periods (org_id, name, start_date, end_date, status) \
         VALUES ($1, 'August', '2026-08-01', '2026-08-31', 'generated')",
    )
    .bind(f.org)
    .execute(&pool)
    .await
    .unwrap();

    let (st, b) = send!(
        app,
        "POST",
        "/staff/requests".to_string(),
        f.owner_token(),
        json!({ "employee_id": f.e, "kind": "leave", "on_date": "2026-08-20" })
    );
    assert_eq!(
        (st, b["code"].as_str()),
        (409, Some("PERIOD_CLOSED")),
        "filing into an approved month"
    );
    // E2E suggestions: the refusal says whether the month is PAID (a paid
    // month can't be reopened), and a pending request in it reads
    // month_closed, so clients offer Reject, not Approve.
    assert_eq!(b["vars"]["paid"], json!(false), "{b}");
    assert_eq!(b["vars"]["date"], json!("2026-08-20"), "{b}");
    let (_, list) = send!(app, "GET", "/staff/requests".to_string(), f.owner_token());
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == pending["id"])
        .unwrap()
        .clone();
    assert_eq!(
        (row["can_decide"].clone(), row["month_closed"].clone()),
        (json!(true), json!(true)),
        "{row}"
    );
    // A leave reaching back into the closed month through its END is refused too.
    let (st, _) = send!(
        app,
        "POST",
        "/staff/requests".to_string(),
        f.owner_token(),
        json!({ "employee_id": f.e, "kind": "leave", "on_date": "2026-07-25", "end_date": "2026-08-02" })
    );
    assert_eq!(st, 409, "every day of the span is checked");
    let (st, _) = decide(
        &app,
        &f.owner_token(),
        &pending["id"],
        json!({ "status": "approved", "is_paid": true }),
    )
    .await;
    assert_eq!(st, 409, "approving into an approved month");
    let (st, _) = decide(
        &app,
        &f.owner_token(),
        &approved["id"],
        json!({ "status": "cancelled", "note": "n" }),
    )
    .await;
    assert_eq!(
        st, 409,
        "cancelling approved time after payroll approval (RQ-12)"
    );
    let (st, _) = decide(
        &app,
        &f.owner_token(),
        &pending["id"],
        json!({ "status": "rejected" }),
    )
    .await;
    assert_eq!(st, 200, "rejecting changes nothing that was paid");

    // The record says its month is closed (E2E), so clients don't offer
    // what the server would refuse.
    let (_, list) = send!(
        app,
        "GET",
        format!(
            "/staff/attendance?from=2026-08-10&to=2026-08-10&employee_id={}",
            f.e
        ),
        f.owner_token()
    );
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == json!(rec))
        .unwrap_or_else(|| panic!("{list}"))
        .clone();
    assert_eq!(row["month_closed"], json!(true), "{row}");
    // AT-7: manual attendance edits wait for next month too.
    let (st, _) = send!(
        app,
        "PATCH",
        format!("/staff/attendance/{rec}"),
        f.owner_token(),
        json!({ "reason": "fix", "status": "absent" })
    );
    assert_eq!(st, 409);
    let (st, _) = send!(
        app,
        "POST",
        "/staff/attendance".to_string(),
        f.owner_token(),
        json!({ "employee_id": f.e, "branch_id": f.a, "business_date": "2026-08-11", "status": "absent", "reason": "r" })
    );
    assert_eq!(st, 409);
    let (st, _) = send!(
        app,
        "DELETE",
        format!("/staff/attendance/{rec}"),
        f.owner_token()
    );
    assert_eq!(st, 409);
    // Once August is PAID the refusal says so.
    sqlx::query("UPDATE payroll_periods SET status = 'paid' WHERE org_id = $1 AND name = 'August'")
        .bind(f.org)
        .execute(&pool)
        .await
        .unwrap();
    let (st, b) = send!(
        app,
        "POST",
        "/staff/requests".to_string(),
        f.owner_token(),
        json!({ "employee_id": f.e, "kind": "leave", "on_date": "2026-08-21" })
    );
    assert_eq!((st, b["vars"]["paid"].clone()), (409, json!(true)), "{b}");
    // September is still open.
    let (st, _) = send!(
        app,
        "POST",
        "/staff/requests".to_string(),
        f.owner_token(),
        json!({ "employee_id": f.e, "kind": "leave", "on_date": "2026-09-02" })
    );
    assert_eq!(st, 201);
}

// ── RQ-5: routing and self-approval ─────────────────────────────────────────

#[sqlx::test]
async fn the_owners_own_request_is_approved_as_it_is_filed(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let s = session(&pool, f.e_owner).await;
    let phone = format!("{}|{}", s.token, s.device);
    let (st, row) = send!(
        app,
        "POST",
        "/staff/me/requests".to_string(),
        phone,
        json!({ "kind": "late_arrival", "on_date": "2026-09-10", "to_time": "10:00:00" })
    );
    assert_eq!(st, 201, "{row}");
    assert_eq!(
        row["status"], "approved",
        "hr.requests.self_approve: approved at filing (RQ-5, B12)"
    );
    assert_eq!(row["decided_by"], json!(f.owner));
}

/// Owner decision (QUESTIONS #19, 2026-09-24): a leave approved as it is filed
/// has no approver to choose paid or unpaid, so the filer must (RQ-2). Without
/// the choice it is refused BEFORE anything is stored; with it, approved as
/// chosen. A filer who doesn't self-approve may still leave it to the approver.
#[sqlx::test]
async fn a_self_approved_leave_must_say_paid_or_unpaid(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let s = session(&pool, f.e_owner).await;
    let phone = format!("{}|{}", s.token, s.device);
    let count = async || -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM staff_requests WHERE employee_id = $1 AND kind = 'leave'",
        )
        .bind(f.e_owner)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let before = count().await;
    let (st, body) = send!(
        app,
        "POST",
        "/staff/me/requests".to_string(),
        phone.clone(),
        json!({ "kind": "leave", "on_date": "2026-09-14" })
    );
    assert_eq!(
        (st, body["code"].as_str()),
        (400, Some("LEAVE_PAY_REQUIRED")),
        "{body}"
    );
    assert_eq!(count().await, before, "nothing stored");
    // The dashboard's "file" for the owner themselves: the same rule.
    let (st, body) = send!(
        app,
        "POST",
        "/staff/requests".to_string(),
        f.owner_token(),
        json!({ "employee_id": f.e_owner, "kind": "leave", "on_date": "2026-09-14" })
    );
    assert_eq!(
        (st, body["code"].as_str()),
        (400, Some("LEAVE_PAY_REQUIRED")),
        "{body}"
    );
    // With the choice: approved as filed, unpaid kept unpaid.
    let (st, row) = send!(
        app,
        "POST",
        "/staff/me/requests".to_string(),
        phone,
        json!({ "kind": "leave", "on_date": "2026-09-14", "is_paid": false })
    );
    assert_eq!(st, 201, "{row}");
    assert_eq!(
        (row["status"].clone(), row["is_paid"].clone()),
        (json!("approved"), json!(false)),
        "{row}"
    );
    // Someone who doesn't self-approve files without it: the approver decides.
    let e = session(&pool, f.e).await;
    let (st, row) = send!(
        app,
        "POST",
        "/staff/me/requests".to_string(),
        format!("{}|{}", e.token, e.device),
        json!({ "kind": "leave", "on_date": "2026-09-15" })
    );
    assert_eq!(
        (st, row["status"].clone()),
        (201, json!("pending")),
        "{row}"
    );
}

#[sqlx::test]
async fn a_managers_own_request_goes_to_the_owner_not_to_a_peer(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let s = session(&pool, f.e_mgr).await;
    let phone = format!("{}|{}", s.token, s.device);
    let (st, row) = send!(
        app,
        "POST",
        "/staff/me/requests".to_string(),
        phone.clone(),
        json!({ "kind": "early_departure", "on_date": "2026-09-10", "from_time": "16:00:00", "reason": "." })
    );
    assert_eq!(st, 201, "{row}");
    assert_eq!(
        row["status"], "pending",
        "a manager without self_approve waits"
    );
    assert_eq!(row["to_owner"], true, "flagged to the owner");
    assert!(
        row["reason"].is_null(),
        "a note of only punctuation is no note: {row}"
    );

    // The owner is told; the peer manager is not.
    let told = |who: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM staff_notifications WHERE employee_id = $1 AND key = 'staff.n_request'",
            )
            .bind(who)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(told(f.e_owner).await, 1);
    assert_eq!(told(f.e_peer).await, 0);

    // Nobody approves their own request by hand — dashboard or phone.
    let (st, _) = decide(
        &app,
        &f.mgr_token(),
        &row["id"],
        json!({ "status": "approved" }),
    )
    .await;
    assert_eq!(st, 403, "own request, dashboard");
    let (st, _) = decide(&app, &phone, &row["id"], json!({ "status": "approved" })).await;
    assert_eq!(st, 403, "own request, phone");
    // A peer manager of the same branch is not above them.
    let (st, b) = decide(
        &app,
        &f.peer_token(),
        &row["id"],
        json!({ "status": "approved" }),
    )
    .await;
    assert_eq!(st, 403, "a peer decides a peer's request: {b}");
    // The owner decides it.
    let (st, b) = decide(
        &app,
        &f.owner_token(),
        &row["id"],
        json!({ "status": "approved" }),
    )
    .await;
    assert_eq!(st, 200, "{b}");
    // The manager may still cancel their own, with a reason once approved.
    let (st, _) = decide(&app, &phone, &row["id"], json!({ "status": "cancelled" })).await;
    assert_eq!(st, 400, "an approved request is cancelled with a reason");
    let (st, _) = decide(
        &app,
        &phone,
        &row["id"],
        json!({ "status": "cancelled", "note": "plans changed" }),
    )
    .await;
    assert_eq!(st, 200);
}

#[sqlx::test]
async fn a_manager_decides_their_branchs_people_and_not_another_branchs(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let mine = file(
        &app,
        &f,
        f.e,
        json!({ "kind": "late_arrival", "on_date": "2026-09-10", "to_time": "10:00:00" }),
    )
    .await;
    let theirs = file(
        &app,
        &f,
        f.x,
        json!({ "kind": "late_arrival", "on_date": "2026-09-10", "to_time": "10:00:00" }),
    )
    .await;
    assert_eq!(mine["to_owner"], false);
    let (st, _) = decide(
        &app,
        &f.mgr_token(),
        &theirs["id"],
        json!({ "status": "approved" }),
    )
    .await;
    assert_eq!(st, 403, "B's request, A's manager");
    let (st, _) = decide(
        &app,
        &f.mgr_token(),
        &mine["id"],
        json!({ "status": "approved" }),
    )
    .await;
    assert_eq!(st, 200);
    // Cancelling someone else's request says why (AT-7).
    let other = file(&app, &f, f.e, json!({ "kind": "excuse", "on_date": "2026-09-11", "from_time": "12:00:00", "to_time": "13:00:00" })).await;
    let (st, _) = decide(
        &app,
        &f.mgr_token(),
        &other["id"],
        json!({ "status": "cancelled" }),
    )
    .await;
    assert_eq!(st, 400);
    let (st, _) = decide(
        &app,
        &f.mgr_token(),
        &other["id"],
        json!({ "status": "cancelled", "note": "asked by phone" }),
    )
    .await;
    assert_eq!(st, 200);
}

// ── RQ-7: the excuse's pay follows the rule, and unpaid time costs ──────────

#[sqlx::test]
async fn an_excuse_is_paid_by_the_rule_of_its_branch_and_unpaid_time_is_deducted(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    roster(&pool, &f, f.e, &[f.day_shift]).await;
    let d = "2026-08-10";
    let rec = record(
        &pool,
        &f,
        f.e,
        f.day_shift,
        d,
        (at(d, "09:00"), at(d, "17:00")),
        Some((at(d, "09:00"), at(d, "17:00"))),
        "present",
    )
    .await;

    let ex = file(
        &app,
        &f,
        f.e,
        json!({ "kind": "excuse", "on_date": d, "from_time": "12:00:00", "to_time": "14:00:00" }),
    )
    .await;
    assert_eq!(
        ex["paid_default"], false,
        "the business rule is the default (RQ-7)"
    );
    let (st, row) = decide(
        &app,
        &f.owner_token(),
        &ex["id"],
        json!({ "status": "approved" }),
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(row["is_paid"], false, "omitted = the rule");
    // Two unpaid hours of an eight-hour day: a quarter of 10,000.
    assert_eq!(deduction(&pool, rec, "excused_unpaid").await, 2_500);

    // A branch that pays permissions overrides the business (RU-2 → RQ-7).
    let (st, b) = send!(
        app,
        "PUT",
        "/staff/attendance/settings".to_string(),
        f.owner_token(),
        json!({ "branch_id": f.a, "excused_time_paid_default": true })
    );
    assert_eq!(st, 200, "{b}");
    let d2 = "2026-08-11";
    let rec2 = record(
        &pool,
        &f,
        f.e,
        f.day_shift,
        d2,
        (at(d2, "09:00"), at(d2, "17:00")),
        Some((at(d2, "09:00"), at(d2, "17:00"))),
        "present",
    )
    .await;
    let ex2 = file(
        &app,
        &f,
        f.e,
        json!({ "kind": "excuse", "on_date": d2, "from_time": "12:00:00", "to_time": "14:00:00" }),
    )
    .await;
    assert_eq!(ex2["paid_default"], true);
    let (_, row) = decide(
        &app,
        &f.owner_token(),
        &ex2["id"],
        json!({ "status": "approved" }),
    )
    .await;
    assert_eq!(row["is_paid"], true);
    assert_eq!(
        deduction(&pool, rec2, "excused_unpaid").await,
        0,
        "paid time costs nothing"
    );

    // An unpaid early departure: leaving at 15:00 of a 17:00 shift.
    let d3 = "2026-08-12";
    let rec3 = record(
        &pool,
        &f,
        f.e,
        f.day_shift,
        d3,
        (at(d3, "09:00"), at(d3, "17:00")),
        Some((at(d3, "09:00"), at(d3, "15:00"))),
        "present",
    )
    .await;
    let early = file(
        &app,
        &f,
        f.e,
        json!({ "kind": "early_departure", "on_date": d3, "from_time": "15:00:00" }),
    )
    .await;
    let (_, row) = decide(
        &app,
        &f.owner_token(),
        &early["id"],
        json!({ "status": "approved", "is_paid": false }),
    )
    .await;
    assert_eq!(row["is_paid"], false);
    assert_eq!(
        deduction(&pool, rec3, "excused_unpaid").await,
        2_500,
        "two unpaid hours"
    );
    let early_leave: i32 =
        sqlx::query_scalar("SELECT early_leave_minutes FROM attendance_records WHERE id = $1")
            .bind(rec3)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        early_leave, 0,
        "leaving at the agreed time is not leaving early"
    );
}

// ── RQ-8: half-day leave, and no lateness on leave (B3) ─────────────────────

#[sqlx::test]
async fn a_first_half_leave_forgives_the_morning_and_prices_only_that_half(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    roster(&pool, &f, f.e, &[f.day_shift]).await;
    let d = "2026-08-10";
    // Arrived at 13:00, the middle of 09:00–17:00: on time for the worked half.
    let rec = record(
        &pool,
        &f,
        f.e,
        f.day_shift,
        d,
        (at(d, "09:00"), at(d, "17:00")),
        Some((at(d, "13:00"), at(d, "17:00"))),
        "late",
    )
    .await;
    rederive(&app, &f, rec).await;
    assert_eq!(
        deduction(&pool, rec, "late_penalty").await,
        DAY / 2,
        "without leave: 240 min late"
    );

    let half = file(
        &app,
        &f,
        f.e,
        json!({ "kind": "leave", "on_date": d, "is_half_day": true, "leave_half": "first" }),
    )
    .await;
    assert_eq!(half["leave_half"], "first");
    let (st, _) = decide(
        &app,
        &f.owner_token(),
        &half["id"],
        json!({ "status": "approved", "is_paid": false }),
    )
    .await;
    assert_eq!(st, 200);
    let (status, late): (String, i32) =
        sqlx::query_as("SELECT status, late_minutes FROM attendance_records WHERE id = $1")
            .bind(rec)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (status.as_str(), late),
        ("present", 0),
        "the other half is worked normally"
    );
    assert_eq!(deduction(&pool, rec, "late_penalty").await, 0);
    assert_eq!(
        deduction(&pool, rec, "absence").await,
        DAY / 2,
        "unpaid half day: half an absence"
    );
}

#[sqlx::test]
async fn a_second_half_leave_lets_them_go_at_the_middle(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    roster(&pool, &f, f.e, &[f.day_shift]).await;
    let d = "2026-08-10";
    let rec = record(
        &pool,
        &f,
        f.e,
        f.day_shift,
        d,
        (at(d, "09:00"), at(d, "17:00")),
        Some((at(d, "09:00"), at(d, "13:00"))),
        "present",
    )
    .await;
    let half = file(
        &app,
        &f,
        f.e,
        json!({ "kind": "leave", "on_date": d, "is_half_day": true, "leave_half": "second" }),
    )
    .await;
    let (st, _) = decide(
        &app,
        &f.owner_token(),
        &half["id"],
        json!({ "status": "approved", "is_paid": true }),
    )
    .await;
    assert_eq!(st, 200);
    let (status, early): (String, i32) =
        sqlx::query_as("SELECT status, early_leave_minutes FROM attendance_records WHERE id = $1")
            .bind(rec)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), early), ("present", 0));
    assert_eq!(
        deduction(&pool, rec, "absence").await,
        0,
        "paid half day costs nothing"
    );
    assert_eq!(deduction(&pool, rec, "excused_unpaid").await, 0);
}

#[sqlx::test]
async fn under_the_whole_day_rule_a_half_day_is_a_day_off(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    set_rule(&pool, f.org, "half_day_leave_counts = 'whole_day'").await;
    roster(&pool, &f, f.e, &[f.day_shift]).await;
    let d = "2026-08-10";
    let rec = record(
        &pool,
        &f,
        f.e,
        f.day_shift,
        d,
        (at(d, "09:00"), at(d, "17:00")),
        None,
        "absent",
    )
    .await;
    let half = file(
        &app,
        &f,
        f.e,
        json!({ "kind": "leave", "on_date": d, "is_half_day": true }),
    )
    .await;
    decide(
        &app,
        &f.owner_token(),
        &half["id"],
        json!({ "status": "approved", "is_paid": false }),
    )
    .await;
    let status: String = sqlx::query_scalar("SELECT status FROM attendance_records WHERE id = $1")
        .bind(rec)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "on_leave");
    assert_eq!(
        deduction(&pool, rec, "absence").await,
        DAY,
        "the whole day, unpaid"
    );
}

#[sqlx::test]
async fn nobody_is_late_on_a_day_of_approved_leave(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    roster(&pool, &f, f.e, &[f.day_shift]).await;
    let d = "2026-08-10";
    let rec = record(
        &pool,
        &f,
        f.e,
        f.day_shift,
        d,
        (at(d, "09:00"), at(d, "17:00")),
        Some((at(d, "11:00"), at(d, "12:00"))),
        "late",
    )
    .await;
    let leave = file(&app, &f, f.e, json!({ "kind": "leave", "on_date": d })).await;
    decide(
        &app,
        &f.owner_token(),
        &leave["id"],
        json!({ "status": "approved", "is_paid": true }),
    )
    .await;
    assert_eq!(
        deduction(&pool, rec, "late_penalty").await,
        0,
        "B3: no late penalty on leave"
    );
}

// ── RU-5 / RU-6: split days ─────────────────────────────────────────────────

#[sqlx::test]
async fn a_missed_shift_of_a_split_day_costs_its_share(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    roster(&pool, &f, f.e, &[f.morning, f.evening]).await;
    let d = "2026-08-10";
    let am = record(
        &pool,
        &f,
        f.e,
        f.morning,
        d,
        (at(d, "09:00"), at(d, "13:00")),
        None,
        "absent",
    )
    .await;
    rederive(&app, &f, am).await;
    assert_eq!(
        deduction(&pool, am, "absence").await,
        DAY / 2,
        "half the day's rostered time"
    );
    let pm = record(
        &pool,
        &f,
        f.e,
        f.evening,
        d,
        (at(d, "17:00"), at(d, "21:00")),
        None,
        "absent",
    )
    .await;
    rederive(&app, &f, pm).await;
    assert_eq!(
        deduction(&pool, am, "absence").await + deduction(&pool, pm, "absence").await,
        DAY,
        "missing both halves is ONE absence, not two (RU-5)"
    );
}

#[sqlx::test]
async fn a_late_arrival_for_the_evening_never_excuses_the_morning(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    roster(&pool, &f, f.e, &[f.morning, f.evening]).await;
    let d = "2026-08-10";
    // 40 minutes late for the morning.
    let am = record(
        &pool,
        &f,
        f.e,
        f.morning,
        d,
        (at(d, "09:00"), at(d, "13:00")),
        Some((at(d, "09:40"), at(d, "13:00"))),
        "late",
    )
    .await;
    let pm = record(
        &pool,
        &f,
        f.e,
        f.evening,
        d,
        (at(d, "17:00"), at(d, "21:00")),
        Some((at(d, "17:50"), at(d, "21:00"))),
        "late",
    )
    .await;
    let late = file(
        &app,
        &f,
        f.e,
        json!({ "kind": "late_arrival", "on_date": d, "to_time": "18:00:00" }),
    )
    .await;
    decide(
        &app,
        &f.owner_token(),
        &late["id"],
        json!({ "status": "approved" }),
    )
    .await;
    let lates: Vec<i32> = sqlx::query_scalar("SELECT late_minutes FROM attendance_records WHERE id = ANY($1) ORDER BY scheduled_start_at")
        .bind(vec![am, pm])
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        lates,
        vec![40, 0],
        "B4: the evening's permission stays with the evening"
    );
    // RU-6: the minute rate is the day's (480 min), so a half-day rung is half
    // of 10,000, not of a 4-hour shift's day.
    assert_eq!(deduction(&pool, am, "late_penalty").await, DAY / 2);

    // Tied to a shift by id, a morning time never reaches the evening.
    let d2 = "2026-08-11";
    let am2 = record(
        &pool,
        &f,
        f.e,
        f.morning,
        d2,
        (at(d2, "09:00"), at(d2, "13:00")),
        Some((at(d2, "09:40"), at(d2, "13:00"))),
        "late",
    )
    .await;
    let tied = file(&app, &f, f.e, json!({ "kind": "late_arrival", "on_date": d2, "to_time": "10:00:00", "work_shift_id": f.evening })).await;
    decide(
        &app,
        &f.owner_token(),
        &tied["id"],
        json!({ "status": "approved" }),
    )
    .await;
    let late2: i32 =
        sqlx::query_scalar("SELECT late_minutes FROM attendance_records WHERE id = $1")
            .bind(am2)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(late2, 40, "the request named the evening shift");
}

// ── B5: night shifts ────────────────────────────────────────────────────────

#[sqlx::test]
async fn a_night_shifts_times_after_midnight_land_on_the_next_morning(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    roster(&pool, &f, f.e, &[f.night]).await;
    let d = "2026-08-10";
    let next = "2026-08-11";
    // Arrived 00:30 (150 min late); the check-out was never recorded.
    let rec = record(
        &pool,
        &f,
        f.e,
        f.night,
        d,
        (at(d, "22:00"), at(next, "06:00")),
        Some((at(next, "00:30"), at(next, "06:00"))),
        "late",
    )
    .await;
    // A late arrival agreed for 00:30 means the next morning.
    let late = file(
        &app,
        &f,
        f.e,
        json!({ "kind": "late_arrival", "on_date": d, "to_time": "00:30:00" }),
    )
    .await;
    decide(
        &app,
        &f.owner_token(),
        &late["id"],
        json!({ "status": "approved" }),
    )
    .await;
    let lm: i32 = sqlx::query_scalar("SELECT late_minutes FROM attendance_records WHERE id = $1")
        .bind(rec)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(lm, 0, "00:30 is inside the night shift");

    // A correction whose check-out is 05:00: earlier on the clock than the
    // check-in, i.e. the next morning. It used to be refused.
    let s = session(&pool, f.e).await;
    let phone = format!("{}|{}", s.token, s.device);
    let (st, corr) = send!(
        app,
        "POST",
        "/staff/me/requests".to_string(),
        phone,
        json!({ "kind": "correction", "on_date": d, "attendance_record_id": rec, "to_time": "05:00:00" })
    );
    assert_eq!(st, 201, "{corr}");
    assert_eq!(
        corr["record_check_out_at"]
            .as_str()
            .map(|s| s.starts_with("2026-08-11T06:00")),
        Some(true),
        "the approver sees the current punch: {corr}"
    );
    let (st, b) = decide(
        &app,
        &f.owner_token(),
        &corr["id"],
        json!({ "status": "approved" }),
    )
    .await;
    assert_eq!(st, 200, "{b}");
    let out: DateTime<Utc> =
        sqlx::query_scalar("SELECT check_out_at FROM attendance_records WHERE id = $1")
            .bind(rec)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(out, at(next, "05:00"));
    // An approved correction can't be cancelled: the punch is rewritten.
    let (st, _) = decide(
        &app,
        &f.owner_token(),
        &corr["id"],
        json!({ "status": "cancelled", "note": "n" }),
    )
    .await;
    assert_eq!(st, 409);
}

// ── AT-7: automation never undoes a human decision ─────────────────────────

#[sqlx::test]
async fn a_request_decision_keeps_the_managers_status_and_audit(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    roster(&pool, &f, f.e, &[f.day_shift]).await;
    let d = "2026-08-10";
    let rec = record(
        &pool,
        &f,
        f.e,
        f.day_shift,
        d,
        (at(d, "09:00"), at(d, "17:00")),
        Some((at(d, "09:40"), at(d, "17:00"))),
        "late",
    )
    .await;
    let (st, _) = send!(
        app,
        "PATCH",
        format!("/staff/attendance/{rec}"),
        f.owner_token(),
        json!({ "reason": "Card reader broke", "status": "present" })
    );
    assert_eq!(st, 200);
    let ex = file(
        &app,
        &f,
        f.e,
        json!({ "kind": "excuse", "on_date": d, "from_time": "12:00:00", "to_time": "13:00:00" }),
    )
    .await;
    decide(
        &app,
        &f.owner_token(),
        &ex["id"],
        json!({ "status": "approved", "is_paid": true }),
    )
    .await;
    let (status, reason, overridden): (String, Option<String>, bool) = sqlx::query_as(
        "SELECT status, edit_reason, status_overridden FROM attendance_records WHERE id = $1",
    )
    .bind(rec)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        status, "present",
        "the manager's status survives the re-derive"
    );
    assert_eq!(
        reason.as_deref(),
        Some("Card reader broke"),
        "B10: the Legal report keeps the manager's reason"
    );
    assert!(overridden);
    // `derived` hands the day back to the rules.
    let (st, _) = send!(
        app,
        "PATCH",
        format!("/staff/attendance/{rec}"),
        f.owner_token(),
        json!({ "reason": "back to the rules", "status": "derived" })
    );
    assert_eq!(st, 200);
    let status: String = sqlx::query_scalar("SELECT status FROM attendance_records WHERE id = $1")
        .bind(rec)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "late");
}

#[sqlx::test]
async fn the_sweep_never_writes_back_a_day_a_manager_deleted(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let d = "2026-08-10";
    let rec = record(
        &pool,
        &f,
        f.e,
        f.day_shift,
        d,
        (at(d, "09:00"), at(d, "17:00")),
        None,
        "absent",
    )
    .await;
    let (st, _) = send!(
        app,
        "DELETE",
        format!("/staff/attendance/{rec}?reason=Was%20on%20a%20course"),
        f.owner_token()
    );
    assert_eq!(st, 204);
    // Exactly the row the absence sweep writes.
    let sweep = |status: &'static str| {
        let pool = pool.clone();
        let (org, e, a, s) = (f.org, f.e, f.a, f.day_shift);
        async move {
            sqlx::query(
                "INSERT INTO attendance_records (org_id, employee_id, branch_id, work_shift_id, business_date, \
                     status, scheduled_start_at, scheduled_end_at, is_manual, edit_reason) \
                 VALUES ($1, $2, $3, $4, '2026-08-10', $5, now(), now(), FALSE, 'Marked automatically: no check-in') \
                 ON CONFLICT (employee_id, business_date, \
                    COALESCE(work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid)) WHERE covered_employee_id IS NULL \
                 DO NOTHING",
            )
            .bind(org)
            .bind(e)
            .bind(a)
            .bind(s)
            .bind(status)
            .execute(&pool)
            .await
            .unwrap()
            .rows_affected()
        }
    };
    assert_eq!(sweep("absent").await, 0, "B11: the tombstone holds");
    let reason: Option<String> =
        sqlx::query_scalar("SELECT reason FROM attendance_tombstones WHERE employee_id = $1")
            .bind(f.e)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reason.as_deref(), Some("Was on a course"));
    // A manager can still record the day by hand.
    let (st, b) = send!(
        app,
        "POST",
        "/staff/attendance".to_string(),
        f.owner_token(),
        json!({ "employee_id": f.e, "branch_id": f.a, "business_date": d, "work_shift_id": f.day_shift, "status": "present", "reason": "course counts as work" })
    );
    assert_eq!(st, 201, "{b}");
}

#[sqlx::test]
async fn a_waiver_can_be_taken_back_with_a_reason(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    roster(&pool, &f, f.e, &[f.day_shift]).await;
    let d = "2026-08-10";
    let rec = record(
        &pool,
        &f,
        f.e,
        f.day_shift,
        d,
        (at(d, "09:00"), at(d, "17:00")),
        None,
        "absent",
    )
    .await;
    rederive(&app, &f, rec).await;
    let id: Uuid =
        sqlx::query_scalar("SELECT id FROM payroll_deductions WHERE attendance_record_id = $1")
            .bind(rec)
            .fetch_one(&pool)
            .await
            .unwrap();
    let (st, _) = send!(
        app,
        "PATCH",
        format!("/staff/payroll/deductions/{id}/waive"),
        f.owner_token(),
        json!({ "reason": "sick" })
    );
    assert_eq!(st, 200);
    let uri = format!("/staff/payroll/deductions/{id}/unwaive");
    // Refusals first (AT-11): no reason; a manager of another branch; an employee.
    let (st, _) = send!(
        app,
        "PATCH",
        uri.clone(),
        f.owner_token(),
        json!({ "reason": " " })
    );
    assert_eq!(st, 400);
    let other = user(&pool, f.org, "branch_manager").await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(other)
        .bind(f.b)
        .execute(&pool)
        .await
        .unwrap();
    let (st, _) = send!(
        app,
        "PATCH",
        uri.clone(),
        user_token(other, f.org, UserRole::BranchManager),
        json!({ "reason": "r" })
    );
    assert_eq!(st, 403, "a manager of B");
    let s = session(&pool, f.e).await;
    let (st, _) = send!(
        app,
        "PATCH",
        uri.clone(),
        format!("{}|{}", s.token, s.device),
        json!({ "reason": "r" })
    );
    assert_eq!(st, 403, "the employee's phone");
    let (st, b) = send!(
        app,
        "PATCH",
        uri.clone(),
        f.owner_token(),
        json!({ "reason": "No sick note came" })
    );
    assert_eq!(st, 200, "{b}");
    assert!(b["waived_at"].is_null(), "the row comes back live: {b}");
    let (waived, by, why): (Option<DateTime<Utc>>, Option<Uuid>, Option<String>) = sqlx::query_as(
        "SELECT waived_at, unwaived_by, unwaive_reason FROM payroll_deductions WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(waived.is_none());
    assert_eq!(
        (by, why.as_deref()),
        (Some(f.owner), Some("No sick note came"))
    );
    let (st, _) = send!(
        app,
        "PATCH",
        uri,
        f.owner_token(),
        json!({ "reason": "again" })
    );
    assert_eq!(st, 409, "it isn't waived any more");
}

// ── RU-2: a branch overrides the business field by field ────────────────────

#[sqlx::test]
async fn a_branch_override_inherits_every_rule_it_does_not_set(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner_token();
    let (st, b) = send!(
        app,
        "PUT",
        "/staff/attendance/settings".to_string(),
        owner.clone(),
        json!({ "branch_id": f.a, "overtime_mode": "automatic" })
    );
    assert_eq!(st, 200, "{b}");
    assert_eq!(b["overtime_mode"], "automatic");
    assert_eq!(
        b["late_deduction_tiers"].as_array().map(Vec::len),
        Some(2),
        "B2: the business ladder, not an empty one"
    );
    assert_eq!(b["overridden"], json!(["overtime_mode"]));

    // A later business edit reaches the branch.
    let (st, _) = send!(
        app,
        "PUT",
        "/staff/attendance/settings".to_string(),
        owner.clone(),
        json!({ "absence_deduction_days": 2 })
    );
    assert_eq!(st, 200);
    let a = body(call!(
        app,
        "GET",
        format!("/staff/attendance/settings?branch_id={}", f.a),
        owner.clone()
    ))
    .await;
    assert_eq!(a["absence_deduction_days"].as_f64(), Some(2.0));
    assert_eq!(a["overtime_mode"], "automatic");
    let biz = body(call!(
        app,
        "GET",
        "/staff/attendance/settings".to_string(),
        owner.clone()
    ))
    .await;
    assert_eq!(biz["overtime_mode"], "off", "the business keeps its own");

    // A branch never overrides the business's own settings.
    let (st, _) = send!(
        app,
        "PUT",
        "/staff/attendance/settings".to_string(),
        owner.clone(),
        json!({ "branch_id": f.a, "period_start_day": 1 })
    );
    assert_eq!(st, 400);
    let (st, _) = send!(
        app,
        "PUT",
        "/staff/attendance/settings".to_string(),
        owner.clone(),
        json!({ "inherit": ["overtime_mode"] })
    );
    assert_eq!(st, 400, "only a branch inherits");

    // Back to the business's value, field by field, and then altogether.
    let (st, b) = send!(
        app,
        "PUT",
        "/staff/attendance/settings".to_string(),
        owner.clone(),
        json!({ "branch_id": f.a, "inherit": ["overtime_mode"], "working_days_per_month": 26 })
    );
    assert_eq!(st, 200, "{b}");
    assert_eq!(b["overtime_mode"], "off");
    assert_eq!(b["overridden"], json!(["working_days_per_month"]));
    let list = body(call!(
        app,
        "GET",
        "/staff/attendance/settings/branches".to_string(),
        owner.clone()
    ))
    .await;
    assert_eq!(
        list.as_array().map(Vec::len),
        Some(2),
        "both branches for the owner: {list}"
    );
    let (st, _) = send!(
        app,
        "DELETE",
        format!("/staff/attendance/settings/branches/{}", f.a),
        owner.clone()
    );
    assert_eq!(st, 204);
    let a = body(call!(
        app,
        "GET",
        format!("/staff/attendance/settings?branch_id={}", f.a),
        owner
    ))
    .await;
    assert_eq!(a["overridden"], json!([]));
    assert_eq!(a["working_days_per_month"].as_f64(), Some(30.0));
}

// ── Owner decision 2026-09-23: managers VIEW the rules, read-only ───────────

#[sqlx::test]
async fn a_manager_sees_the_rules_of_their_branches_and_changes_none(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let mgr = f.mgr_token();
    let (st, b) = send!(
        app,
        "GET",
        "/staff/attendance/settings".to_string(),
        mgr.clone()
    );
    assert_eq!(st, 200, "the business's rules: {b}");
    let (st, _) = send!(
        app,
        "GET",
        format!("/staff/attendance/settings?branch_id={}", f.a),
        mgr.clone()
    );
    assert_eq!(st, 200, "their own branch");
    let (st, _) = send!(
        app,
        "GET",
        format!("/staff/attendance/settings?branch_id={}", f.b),
        mgr.clone()
    );
    assert_eq!(st, 403, "another branch's rules");
    let list = body(call!(
        app,
        "GET",
        "/staff/attendance/settings/branches".to_string(),
        mgr.clone()
    ))
    .await;
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["branch_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![f.a.to_string()], "only their branches' overrides");
    // Every write is refused.
    for (m, uri, b) in [
        (
            "PUT",
            "/staff/attendance/settings".to_string(),
            json!({ "absence_deduction_days": 3 }),
        ),
        (
            "PUT",
            "/staff/attendance/settings".to_string(),
            json!({ "branch_id": f.a, "absence_deduction_days": 3 }),
        ),
        (
            "PUT",
            "/staff/attendance/settings".to_string(),
            json!({ "branch_id": f.a, "inherit": ["absence_deduction_days"] }),
        ),
        (
            "DELETE",
            format!("/staff/attendance/settings/branches/{}", f.a),
            Value::Null,
        ),
    ] {
        let (st, body) = send!(app, m, uri.clone(), mgr.clone(), b);
        assert_eq!(st, 403, "{m} {uri}: {body}");
    }
    // An employee with no Madar account sees nothing.
    let s = session(&pool, f.e).await;
    let (st, _) = send!(
        app,
        "GET",
        "/staff/attendance/settings".to_string(),
        format!("{}|{}", s.token, s.device)
    );
    assert_eq!(st, 403);
    // The rules are unchanged.
    let absence: rust_decimal::Decimal = sqlx::query_scalar("SELECT absence_deduction_days FROM attendance_settings WHERE org_id = $1 AND branch_id IS NULL").bind(f.org).fetch_one(&pool).await.unwrap();
    assert_eq!(absence, rust_decimal::Decimal::ONE);
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM attendance_settings WHERE org_id = $1")
            .bind(f.org)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 1);
}

#[sqlx::test]
async fn managers_hold_the_rules_view_capability_by_default(pool: PgPool) {
    let _ = seed(&pool).await;
    let (key, defaults): (String, String) =
        sqlx::query_as("SELECT key, defaults FROM capabilities WHERE id = 241")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((key.as_str(), defaults.as_str()), ("hr.rules.view", "om"));
    assert_eq!(madar_rust::authz::Cap::HrRulesView.key(), "hr.rules.view");
}

// ── Wire contract (§3) ──────────────────────────────────────────────────────

#[sqlx::test]
async fn a_mission_needs_a_title_or_a_note(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let (st, _) = send!(
        app,
        "POST",
        "/staff/requests".to_string(),
        f.owner_token(),
        json!({ "employee_id": f.e, "kind": "mission", "on_date": "2026-09-10", "reason": "  " })
    );
    assert_eq!(st, 400);
    let (st, row) = send!(
        app,
        "POST",
        "/staff/requests".to_string(),
        f.owner_token(),
        json!({ "employee_id": f.e, "kind": "mission", "on_date": "2026-09-10", "reason": "Bank" })
    );
    assert_eq!((st, row["title"].as_str()), (201, Some("Bank")));
    // A half day is the first or the second half, nothing else.
    let (st, _) = send!(
        app,
        "POST",
        "/staff/requests".to_string(),
        f.owner_token(),
        json!({ "employee_id": f.e, "kind": "leave", "on_date": "2026-09-12", "is_half_day": true, "leave_half": "middle" })
    );
    assert_eq!(st, 400);
    let (st, _) = send!(
        app,
        "POST",
        "/staff/requests".to_string(),
        f.owner_token(),
        json!({ "employee_id": f.e, "kind": "leave", "on_date": "2026-09-12", "leave_half": "first" })
    );
    assert_eq!(st, 400, "only a half day says which half");
    let (st, _) = send!(
        app,
        "POST",
        "/staff/requests".to_string(),
        f.owner_token(),
        json!({ "employee_id": f.e, "kind": "leave", "on_date": "2026-09-12", "work_shift_id": f.day_shift })
    );
    assert_eq!(st, 400, "a leave names no shift");
    let (st, _) = send!(
        app,
        "POST",
        "/staff/requests".to_string(),
        f.owner_token(),
        json!({ "employee_id": f.e, "kind": "late_arrival", "on_date": "2026-09-12", "to_time": "10:00:00", "work_shift_id": Uuid::new_v4() })
    );
    assert_eq!(st, 404, "a shift of this business");
}

#[::core::prelude::v1::test]
fn the_half_of_a_split_day_is_its_first_shift() {
    use madar_rust::staff::requests::half_day_window;
    let d = "2026-08-10";
    let split = [
        (at(d, "09:00"), at(d, "13:00")),
        (at(d, "17:00"), at(d, "21:00")),
    ];
    assert_eq!(
        half_day_window(&split, "first"),
        Some((at(d, "09:00"), at(d, "13:00")))
    );
    assert_eq!(
        half_day_window(&split, "second"),
        Some((at(d, "13:00"), at(d, "21:00")))
    );
    let one = [(at(d, "09:00"), at(d, "17:00"))];
    assert_eq!(
        half_day_window(&one, "first"),
        Some((at(d, "09:00"), at(d, "13:00")))
    );
    assert_eq!(half_day_window(&[], "first"), None);
}

// ── RU-10 / audit B15: holidays are suggested every year, read without writes,
// and a decision re-prices the day ────────────────────────────────────────

#[sqlx::test]
async fn reading_the_roster_suggests_holidays_without_writing_them(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    // 2028: past the old hard-coded table, the arithmetic calendar answers.
    let uri = format!(
        "/staff/roster?branch_id={}&from=2028-02-20&to=2028-03-05",
        f.a
    );
    let (st, b) = send!(app, "GET", uri, f.owner_token());
    assert_eq!(st, 200, "{b}");
    let names: Vec<&str> = b["holidays"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["name_en"].as_str())
        .collect();
    assert!(
        names.contains(&"Eid al-Fitr"),
        "2028 still has its Eid: {b}"
    );
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM staff_holidays WHERE org_id = $1")
        .bind(f.org)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 0, "a GET never writes (B15)");
}

#[sqlx::test]
async fn deciding_a_holiday_takes_back_the_sweeps_absence_and_dismissing_restores_it(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    roster(&pool, &f, f.e, &[f.day_shift]).await;
    let d = "2026-05-01"; // Labour Day, a suggestion.
    let rec = record(
        &pool,
        &f,
        f.e,
        f.day_shift,
        d,
        (at(d, "09:00"), at(d, "17:00")),
        None,
        "absent",
    )
    .await;
    rederive(&app, &f, rec).await;
    assert_eq!(deduction(&pool, rec, "absence").await, DAY);
    // A day a manager punched stays whatever is decided.
    let worked = record(
        &pool,
        &f,
        f.x,
        f.day_shift,
        d,
        (at(d, "09:00"), at(d, "17:00")),
        Some((at(d, "09:00"), at(d, "17:00"))),
        "present",
    )
    .await;

    // Refusals first (AT-11): a manager can't decide the business's holiday;
    // an unknown date is not a holiday; a bad decision is refused.
    let (st, _) = send!(
        app,
        "PUT",
        format!("/staff/holidays/{d}"),
        f.mgr_token(),
        json!({ "decision": "holiday" })
    );
    assert_eq!(st, 403, "a branch manager");
    let (st, _) = send!(
        app,
        "PUT",
        "/staff/holidays/2026-05-02".to_string(),
        f.owner_token(),
        json!({ "decision": "holiday" })
    );
    assert_eq!(st, 404, "not a public holiday");
    let (st, _) = send!(
        app,
        "PUT",
        format!("/staff/holidays/{d}"),
        f.owner_token(),
        json!({ "decision": "maybe" })
    );
    assert_eq!(st, 400);

    let (st, b) = send!(
        app,
        "PUT",
        format!("/staff/holidays/{d}"),
        f.owner_token(),
        json!({ "decision": "holiday" })
    );
    assert_eq!(st, 200, "{b}");
    assert_eq!(b["decision"], "holiday");
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attendance_records WHERE id = $1")
        .bind(rec)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0, "nobody is absent on a holiday (RU-10)");
    let docked: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM payroll_deductions WHERE employee_id = $1")
            .bind(f.e)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(docked, 0, "the absence's deduction went with it");
    let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attendance_records WHERE id = $1")
        .bind(worked)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kept, 1, "a worked day stays");

    // Dismissed: a normal day again, the no-show is absent again.
    let (st, b) = send!(
        app,
        "PUT",
        format!("/staff/holidays/{d}"),
        f.owner_token(),
        json!({ "decision": "dismissed" })
    );
    assert_eq!(st, 200, "{b}");
    let back: Uuid = sqlx::query_scalar(
        "SELECT id FROM attendance_records WHERE employee_id = $1 AND business_date = $2::date AND status = 'absent'",
    )
    .bind(f.e)
    .bind(d)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(deduction(&pool, back, "absence").await, DAY);
}

#[sqlx::test]
async fn a_holiday_in_an_approved_month_cant_be_decided(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    sqlx::query(
        "INSERT INTO payroll_periods (org_id, name, start_date, end_date, status) \
         VALUES ($1, 'May', '2026-04-26', '2026-05-25', 'generated')",
    )
    .bind(f.org)
    .execute(&pool)
    .await
    .unwrap();
    let (st, b) = send!(
        app,
        "PUT",
        "/staff/holidays/2026-05-01".to_string(),
        f.owner_token(),
        json!({ "decision": "holiday" })
    );
    assert_eq!(
        (st, b["code"].as_str()),
        (409, Some("PERIOD_CLOSED")),
        "{b}"
    );
}

// ── RQ-11: overlaps are judged on the shift, not on `on_date + time` ────────

#[sqlx::test]
async fn a_split_day_takes_one_late_arrival_per_shift_and_a_night_keeps_one(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    roster(&pool, &f, f.e, &[f.morning, f.evening]).await;
    let d = "2026-08-10";
    // Both used to start at 00:00 of the date, so the second was refused.
    file(
        &app,
        &f,
        f.e,
        json!({ "kind": "late_arrival", "on_date": d, "to_time": "09:30:00" }),
    )
    .await;
    file(
        &app,
        &f,
        f.e,
        json!({ "kind": "late_arrival", "on_date": d, "to_time": "17:30:00" }),
    )
    .await;
    // A second one for the same (morning) shift still overlaps.
    let (st, b) = send!(
        app,
        "POST",
        "/staff/requests".to_string(),
        f.owner_token(),
        json!({ "employee_id": f.e, "kind": "late_arrival", "on_date": d, "to_time": "09:45:00" })
    );
    assert_eq!(st, 409, "{b}");

    // A night shift's two excuses after midnight are on the same morning:
    // they overlap, whatever the calendar date says.
    roster(&pool, &f, f.x, &[f.night]).await;
    file(
        &app,
        &f,
        f.x,
        json!({ "kind": "excuse", "on_date": d, "from_time": "01:00:00", "to_time": "02:00:00" }),
    )
    .await;
    let (st, b) = send!(
        app,
        "POST",
        "/staff/requests".to_string(),
        f.owner_token(),
        json!({ "employee_id": f.x, "kind": "excuse", "on_date": d, "from_time": "01:30:00", "to_time": "03:00:00" })
    );
    assert_eq!(st, 409, "{b}");
    let (from, to): (Option<chrono::NaiveDateTime>, Option<chrono::NaiveDateTime>) = sqlx::query_as(
        "SELECT window_from, window_to FROM staff_requests WHERE employee_id = $1 AND kind = 'excuse'",
    )
    .bind(f.x)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (from.map(|t| t.to_string()), to.map(|t| t.to_string())),
        (
            Some("2026-08-11 01:00:00".into()),
            Some("2026-08-11 02:00:00".into())
        ),
        "the night's 01:00 is the next morning"
    );
}

// ── RQ-9: a shift nobody clocked can still be corrected ─────────────────────

#[sqlx::test]
async fn a_correction_can_fix_a_rostered_shift_with_no_record(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    roster(&pool, &f, f.e, &[f.day_shift]).await;
    let d = "2026-08-10";
    let s = session(&pool, f.e).await;
    let phone = format!("{}|{}", s.token, s.device);

    // Refusals: a shift not on their roster, a shift that hasn't started.
    let (st, _) = send!(
        app,
        "POST",
        "/staff/me/requests".to_string(),
        phone.clone(),
        json!({ "kind": "correction", "on_date": d, "work_shift_id": f.night, "from_time": "09:00:00", "to_time": "17:00:00" })
    );
    assert_eq!(st, 400, "not their shift");
    let (st, _) = send!(
        app,
        "POST",
        "/staff/me/requests".to_string(),
        phone.clone(),
        json!({ "kind": "correction", "on_date": "2099-01-01", "work_shift_id": f.day_shift, "from_time": "09:00:00" })
    );
    assert_eq!(st, 400, "not started yet");
    let (st, _) = send!(
        app,
        "POST",
        "/staff/me/requests".to_string(),
        phone.clone(),
        json!({ "kind": "correction", "on_date": d, "from_time": "09:00:00" })
    );
    assert_eq!(st, 400, "neither a record nor a shift");

    let (st, corr) = send!(
        app,
        "POST",
        "/staff/me/requests".to_string(),
        phone.clone(),
        json!({ "kind": "correction", "on_date": d, "work_shift_id": f.day_shift, "from_time": "09:05:00", "to_time": "17:00:00", "reason": "My phone died" })
    );
    assert_eq!(st, 201, "{corr}");
    assert!(corr["attendance_record_id"].is_null());
    let (st, _) = send!(
        app,
        "POST",
        "/staff/me/requests".to_string(),
        phone.clone(),
        json!({ "kind": "correction", "on_date": d, "work_shift_id": f.day_shift, "from_time": "09:00:00" })
    );
    assert_eq!(st, 409, "one correction waits per shift");

    let (st, b) = decide(
        &app,
        &f.owner_token(),
        &corr["id"],
        json!({ "status": "approved" }),
    )
    .await;
    assert_eq!(st, 200, "{b}");
    let rec: Uuid = b["attendance_record_id"].as_str().unwrap().parse().unwrap();
    let (cin, cout, late, method): (Option<DateTime<Utc>>, Option<DateTime<Utc>>, i32, Option<String>) = sqlx::query_as(
        "SELECT check_in_at, check_out_at, late_minutes, check_in_method FROM attendance_records WHERE id = $1",
    )
    .bind(rec)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((cin, cout), (Some(at(d, "09:05")), Some(at(d, "17:00"))));
    assert_eq!(late, 5, "lateness is kept (RQ-10)");
    assert_eq!(method.as_deref(), Some("correction"));
    assert_eq!(
        deduction(&pool, rec, "absence").await,
        0,
        "no absence for a worked shift"
    );
}

// ── RQ-5: the server says whose request it is and who may decide it ────────

#[sqlx::test]
async fn every_request_says_whether_the_caller_may_decide_it(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let e_req = file(
        &app,
        &f,
        f.e,
        json!({ "kind": "leave", "on_date": "2026-08-20" }),
    )
    .await;
    let mgr_req = file(
        &app,
        &f,
        f.e_mgr,
        json!({ "kind": "leave", "on_date": "2026-08-21" }),
    )
    .await;
    let x_req = file(
        &app,
        &f,
        f.x,
        json!({ "kind": "leave", "on_date": "2026-08-22" }),
    )
    .await;
    let flags = |list: &Value, id: &Value| {
        let r = list
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == *id)
            .cloned();
        r.map(|r| {
            (
                r["is_own"].as_bool().unwrap(),
                r["can_decide"].as_bool().unwrap(),
            )
        })
    };

    // The manager of A: decides Eman, not their own, never sees B.
    let (st, list) = send!(app, "GET", "/staff/requests".to_string(), f.mgr_token());
    assert_eq!(st, 200, "{list}");
    assert_eq!(flags(&list, &e_req["id"]), Some((false, true)));
    assert_eq!(
        flags(&list, &mgr_req["id"]),
        Some((true, false)),
        "their own"
    );
    assert_eq!(flags(&list, &x_req["id"]), None, "another branch");
    // A peer manager can't decide a manager's request; the owner can.
    let (_, list) = send!(app, "GET", "/staff/requests".to_string(), f.peer_token());
    assert_eq!(
        flags(&list, &mgr_req["id"]),
        Some((false, false)),
        "a peer is not above them"
    );
    let (_, list) = send!(app, "GET", "/staff/requests".to_string(), f.owner_token());
    assert_eq!(flags(&list, &mgr_req["id"]), Some((false, true)));
    assert_eq!(flags(&list, &x_req["id"]), Some((false, true)));

    // Decided: nobody can decide it again.
    let (st, row) = decide(
        &app,
        &f.owner_token(),
        &e_req["id"],
        json!({ "status": "approved", "is_paid": true }),
    )
    .await;
    assert_eq!(st, 200, "{row}");
    assert_eq!(
        (row["is_own"].as_bool(), row["can_decide"].as_bool()),
        (Some(false), Some(false))
    );

    // The employee's own list: all theirs, none to decide.
    let s = session(&pool, f.e).await;
    let (st, mine) = send!(
        app,
        "GET",
        "/staff/me/requests".to_string(),
        format!("{}|{}", s.token, s.device)
    );
    assert_eq!(st, 200);
    assert!(
        mine.as_array()
            .unwrap()
            .iter()
            .all(|r| r["is_own"] == true && r["can_decide"] == false),
        "{mine}"
    );
}

/// E2E B-TEAM-3 / RQ-F6 (AT-7, AT-10): cancelling an APPROVED request keeps
/// who approved it and why, records who cancelled it and why beside that,
/// and tells the person. Their own pending request cancelled by themselves
/// needs no notice.
#[sqlx::test]
async fn cancelling_an_approved_request_keeps_the_approver_and_tells_the_person(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let row = file(
        &app,
        &f,
        f.e,
        json!({ "kind": "late_arrival", "on_date": "2026-09-10", "to_time": "10:00:00" }),
    )
    .await;
    let (st, b) = decide(
        &app,
        &f.owner_token(),
        &row["id"],
        json!({ "status": "approved", "note": "Doctor's note seen" }),
    )
    .await;
    assert_eq!(st, 200, "{b}");
    let (st, b) = decide(
        &app,
        &f.mgr_token(),
        &row["id"],
        json!({ "status": "cancelled", "note": "Shift was moved" }),
    )
    .await;
    assert_eq!(st, 200, "{b}");
    assert_eq!(b["status"], "cancelled");
    assert_eq!(b["decided_by"], json!(f.owner), "the approver stays: {b}");
    assert_eq!(b["decision_note"], "Doctor's note seen");
    assert!(b["decided_at"].is_string());
    assert_eq!(b["cancelled_by"], json!(f.mgr), "{b}");
    assert_eq!(b["cancel_note"], "Shift was moved");
    assert!(b["cancelled_at"].is_string());
    let told: Vec<Value> = sqlx::query_scalar(
        "SELECT args FROM staff_notifications WHERE employee_id = $1 AND key = 'staff.n_request_cancelled'",
    )
    .bind(f.e)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(told.len(), 1, "the person is told once");
    assert_eq!(told[0]["note"], "Shift was moved", "{:?}", told[0]);

    // Their own pending request, cancelled by themselves: no notice.
    let mine = file(
        &app,
        &f,
        f.e_mgr,
        json!({ "kind": "late_arrival", "on_date": "2026-09-11", "to_time": "10:00:00" }),
    )
    .await;
    let (st, b) = decide(
        &app,
        &f.mgr_token(),
        &mine["id"],
        json!({ "status": "cancelled" }),
    )
    .await;
    assert_eq!(st, 200, "{b}");
    assert!(b["decided_by"].is_null(), "nobody decided it: {b}");
    assert_eq!(b["cancelled_by"], json!(f.mgr));
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM staff_notifications WHERE employee_id = $1 AND key = 'staff.n_request_cancelled'",
    )
    .bind(f.e_mgr)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 0);
}

/// E2E B-TEAM-2 (AT-13): request refusals carry stable codes.
#[sqlx::test]
async fn request_refusals_carry_codes(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let mine = file(
        &app,
        &f,
        f.e_mgr,
        json!({ "kind": "late_arrival", "on_date": "2026-09-10", "to_time": "10:00:00" }),
    )
    .await;
    let (st, b) = decide(
        &app,
        &f.mgr_token(),
        &mine["id"],
        json!({ "status": "approved" }),
    )
    .await;
    assert_eq!((st, b["code"].clone()), (403, json!("OWN_REQUEST")), "{b}");
    let (st, b) = decide(
        &app,
        &f.peer_token(),
        &mine["id"],
        json!({ "status": "approved" }),
    )
    .await;
    assert_eq!(
        (st, b["code"].clone()),
        (403, json!("MANAGER_REQUEST_ABOVE")),
        "{b}"
    );
    // The same excuse twice.
    let body = json!({ "employee_id": f.e, "kind": "excuse", "on_date": "2026-09-11",
                       "from_time": "12:00:00", "to_time": "13:00:00" });
    file(&app, &f, f.e, body.clone()).await;
    let (st, b) = send!(
        app,
        "POST",
        "/staff/requests".to_string(),
        f.owner_token(),
        body
    );
    assert_eq!(
        (st, b["code"].clone()),
        (409, json!("OVERLAPPING_REQUEST")),
        "{b}"
    );
}

/// RQ-F6 follow-up: a request names who decided and who cancelled it, so the
/// employee's phone (which can't look up the owner's account) shows the
/// name, not "a manager". The linked employee's name, else the account's.
#[sqlx::test]
async fn a_request_names_who_decided_and_who_cancelled_it(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let row = file(
        &app,
        &f,
        f.e,
        json!({ "kind": "late_arrival", "on_date": "2026-09-10", "to_time": "10:00:00" }),
    )
    .await;
    let (st, _) = decide(
        &app,
        &f.mgr_token(),
        &row["id"],
        json!({ "status": "approved", "note": "ok" }),
    )
    .await;
    assert_eq!(st, 200);
    let (st, _) = decide(
        &app,
        &f.owner_token(),
        &row["id"],
        json!({ "status": "cancelled", "note": "moved" }),
    )
    .await;
    assert_eq!(st, 200);
    let s = session(&pool, f.e).await;
    let phone = format!("{}|{}", s.token, s.device);
    let mine = || {
        let phone = phone.clone();
        let id = row["id"].clone();
        let app = &app;
        async move {
            let (st, list) = send!(*app, "GET", "/staff/me/requests".to_string(), phone);
            assert_eq!(st, 200, "{list}");
            list.as_array()
                .unwrap()
                .iter()
                .find(|r| r["id"] == id)
                .cloned()
                .unwrap()
        }
    };
    let r = mine().await;
    assert_eq!(r["decided_by_name"], "Mona", "{r}");
    assert_eq!(r["cancelled_by_name"], "Omar", "{r}");
    // The dashboard's list names them too.
    let (st, list) = send!(app, "GET", "/staff/requests".to_string(), f.owner_token());
    assert_eq!(st, 200, "{list}");
    let r = list
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == row["id"])
        .cloned()
        .unwrap();
    assert_eq!(
        (r["decided_by_name"].clone(), r["cancelled_by_name"].clone()),
        (json!("Mona"), json!("Omar"))
    );
    // No linked employee: the account's own name.
    sqlx::query("UPDATE employees SET user_id = NULL WHERE id = $1")
        .bind(f.e_owner)
        .execute(&pool)
        .await
        .unwrap();
    let owner_name: String = sqlx::query_scalar("SELECT name FROM users WHERE id = $1")
        .bind(f.owner)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(mine().await["cancelled_by_name"], json!(owner_name));
}

/// Mac E2E (RQ follow-up): an approved late arrival shortens the time the
/// person OWES, as an early departure does, so a day worked inside both
/// agreed times is present — not a half day. 14:00–18:00, arrive by 16:40
/// and leave from 17:30 approved; in 16:15, out 17:40.
#[sqlx::test]
async fn an_approved_late_arrival_is_not_owed_time(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let aft = shift(&pool, f.org, f.a, "Afternoon", "14:00", "18:00").await;
    roster(&pool, &f, f.e, &[aft]).await;
    let d = "2026-09-10";
    for body in [
        json!({ "kind": "late_arrival", "on_date": d, "to_time": "16:40:00" }),
        json!({ "kind": "early_departure", "on_date": d, "from_time": "17:30:00" }),
    ] {
        let row = file(&app, &f, f.e, body).await;
        let (st, b) = decide(
            &app,
            &f.owner_token(),
            &row["id"],
            json!({ "status": "approved", "is_paid": true }),
        )
        .await;
        assert_eq!(st, 200, "{b}");
    }
    let rec = record(
        &pool,
        &f,
        f.e,
        aft,
        d,
        (at(d, "14:00"), at(d, "18:00")),
        Some((at(d, "16:15"), at(d, "17:40"))),
        "present",
    )
    .await;
    let (st, b) = send!(
        app,
        "PATCH",
        format!("/staff/attendance/{rec}"),
        f.owner_token(),
        json!({ "check_out_at": at(d, "17:40"), "reason": "re-derive" })
    );
    assert_eq!(st, 200, "{b}");
    assert_eq!(b["status"], "present", "{b}");
    assert_eq!(b["late_minutes"], 0, "{b}");
}
