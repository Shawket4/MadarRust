//! RO-6, RO-9, AT-11: a branch manager sees and decides only for their own
//! branches, and the org-wide acts (payroll run, rules, holidays, roster
//! settings) need the capability for every branch.
//!
//! One business, branches A and B. The manager runs A. `y` works at A, `x` at
//! B, and B has a record of every kind. Every staff route family is called on
//! B's side: each is a 403 (never a 404 that confirms what exists); the same
//! acts on A's side go through, and the lists show only A.

use actix_web::{App, http::Method, test, web};
use chrono::{Duration, NaiveTime, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::models::UserRole;

mod common;
use common::employees::{authed, employee, secret, session, user_token};

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
    ($app:expr, $method:expr, $uri:expr, $token:expr, $body:expr) => {{
        let mut req = authed(
            test::TestRequest::default()
                .method(Method::from_bytes($method.as_bytes()).unwrap())
                .uri(&$uri),
            &$token,
        );
        if !$body.is_null() {
            req = req.set_json(&$body);
        }
        test::call_service(&$app, req.to_request()).await
    }};
}

async fn body(resp: actix_web::dev::ServiceResponse) -> Value {
    serde_json::from_slice(&test::read_body(resp).await).unwrap_or(Value::Null)
}

struct F {
    org: Uuid,
    a: Uuid,
    b: Uuid,
    owner: Uuid,
    manager: Uuid,
    /// Works at A (the manager's), at B.
    y: Uuid,
    x: Uuid,
    /// A second person at B, for a swap.
    x2: Uuid,
    // B's records.
    rec_x: Uuid,
    cover_x: Uuid,
    ot_x: Uuid,
    flag_x: Uuid,
    req_x: Uuid,
    adj_x: Uuid,
    adv_x: Uuid,
    doc_x: Uuid,
    sched_x: Uuid,
    override_x: Uuid,
    shift_b: Uuid,
    open_b: Uuid,
    swap_b: Uuid,
    period: Uuid,
    // A's, for the controls.
    rec_y: Uuid,
    flag_y: Uuid,
    req_y: Uuid,
}

async fn one<
    T: Send + Unpin + for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
>(
    pool: &PgPool,
    sql: &str,
    binds: &[Uuid],
) -> T {
    let mut q = sqlx::query_scalar::<_, T>(sql);
    for b in binds {
        q = q.bind(*b);
    }
    q.fetch_one(pool).await.unwrap()
}

async fn seed(pool: &PgPool) -> F {
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    let org = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, modules) VALUES ($1, 'Two', $2, '{pos,dawam}')",
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
    let mut br = Vec::new();
    for n in ["A", "B"] {
        br.push(
            one::<Uuid>(
                pool,
                &format!(
                    "INSERT INTO branches (org_id, name, timezone) VALUES ($1, '{n}', 'UTC'::timezone_name) RETURNING id"
                ),
                &[org],
            )
            .await,
        );
    }
    let (a, b) = (br[0], br[1]);
    let mk_user = async |role: &str| -> Uuid {
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
    };
    let owner = mk_user("org_admin").await;
    let manager = mk_user("branch_manager").await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(manager)
        .bind(a)
        .execute(pool)
        .await
        .unwrap();
    let y = employee(
        pool,
        org,
        "Yara",
        None,
        Some("+201050000001"),
        true,
        &[a],
        300_000,
    )
    .await;
    let x = employee(
        pool,
        org,
        "Xavier",
        None,
        Some("+201050000002"),
        true,
        &[b],
        300_000,
    )
    .await;
    let x2 = employee(
        pool,
        org,
        "Xena",
        None,
        Some("+201050000003"),
        true,
        &[b],
        300_000,
    )
    .await;

    let shift_b: Uuid = one(
        pool,
        "INSERT INTO work_shifts (org_id, branch_id, name, start_time, end_time) \
         VALUES ($1, $2, 'B day', '09:00', '17:00') RETURNING id",
        &[org, b],
    )
    .await;
    let rec = async |who: Uuid, branch: Uuid, extra: &str| -> Uuid {
        one::<Uuid>(
            pool,
            &format!(
                "INSERT INTO attendance_records (org_id, employee_id, branch_id, business_date, status, \
                    check_in_at, check_out_at, scheduled_start_at, scheduled_end_at, overtime_minutes{}) \
                 VALUES ($1, $2, $3, CURRENT_DATE - 2, 'present', now() - interval '50 hours', \
                    now() - interval '40 hours', now() - interval '50 hours', now() - interval '42 hours', 90{}) \
                 RETURNING id",
                if extra.is_empty() { "" } else { ", covered_employee_id, cover_status, work_shift_id" },
                if extra.is_empty() { "" } else { extra }
            ),
            &[org, who, branch],
        )
        .await
    };
    let rec_x = rec(x, b, "").await;
    let rec_y = rec(y, a, "").await;
    let ot_x = rec_x;
    sqlx::query("UPDATE attendance_records SET overtime_status = 'pending' WHERE id = $1")
        .bind(ot_x)
        .execute(pool)
        .await
        .unwrap();
    let cover_x: Uuid = one(
        pool,
        "INSERT INTO attendance_records (org_id, employee_id, branch_id, business_date, status, \
            check_in_at, covered_employee_id, cover_status, work_shift_id) \
         VALUES ($1, $2, $3, CURRENT_DATE - 3, 'present', now() - interval '70 hours', $4, 'pending', $5) \
         RETURNING id",
        &[org, x, b, x2, shift_b],
    )
    .await;
    let flag = async |who: Uuid, branch: Uuid, record: Uuid| -> Uuid {
        one::<Uuid>(
            pool,
            "INSERT INTO attendance_flags (org_id, employee_id, branch_id, attendance_record_id, kind, minutes_away) \
             VALUES ($1, $2, $3, $4, 'left_mid_shift', 30) RETURNING id",
            &[org, who, branch, record],
        )
        .await
    };
    let flag_x = flag(x, b, rec_x).await;
    let flag_y = flag(y, a, rec_y).await;
    let req = async |who: Uuid, days: i64| -> Uuid {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO staff_requests (org_id, employee_id, kind, on_date, end_date, reason) \
             VALUES ($1, $2, 'leave', CURRENT_DATE + $3::int, CURRENT_DATE + $3::int, 'x') RETURNING id",
        )
        .bind(org)
        .bind(who)
        .bind(days as i32)
        .fetch_one(pool)
        .await
        .unwrap();
        id
    };
    let req_x = req(x, 20).await;
    let req_y = req(y, 20).await;
    let adj_x: Uuid = one(
        pool,
        "INSERT INTO payroll_bonuses (org_id, employee_id, amount_piastres, reason, effective_date, \
            source, status, recurring, created_by) \
         VALUES ($1, $2, 900000, 'big', CURRENT_DATE, 'manual', 'pending', true, $3) RETURNING id",
        &[org, x, owner],
    )
    .await;
    let adv_x: Uuid = one(
        pool,
        "INSERT INTO salary_advances (org_id, employee_id, amount_piastres, installments, \
            monthly_installment_piastres, remaining_piastres) \
         VALUES ($1, $2, 10000, 1, 10000, 10000) RETURNING id",
        &[org, x],
    )
    .await;
    let doc_x: Uuid = one(
        pool,
        "INSERT INTO staff_documents (org_id, employee_id, title, file_url) \
         VALUES ($1, $2, 'ID', '/uploads/x') RETURNING id",
        &[org, x],
    )
    .await;
    let sched_x: Uuid = one(
        pool,
        "INSERT INTO staff_schedules (org_id, employee_id, work_shift_id, effective_from) \
         VALUES ($1, $2, $3, CURRENT_DATE - 30) RETURNING id",
        &[org, x, shift_b],
    )
    .await;
    sqlx::query(
        "INSERT INTO staff_schedules (org_id, employee_id, work_shift_id, effective_from) \
         VALUES ($1, $2, $3, CURRENT_DATE - 30)",
    )
    .bind(org)
    .bind(x2)
    .bind(shift_b)
    .execute(pool)
    .await
    .unwrap();
    let override_x: Uuid = one(
        pool,
        "INSERT INTO staff_schedule_overrides (org_id, employee_id, on_date, reason) \
         VALUES ($1, $2, CURRENT_DATE + 10, 'off') RETURNING id",
        &[org, x],
    )
    .await;
    let open_b: Uuid = one(
        pool,
        "INSERT INTO staff_open_shifts (org_id, branch_id, work_shift_id, on_date, status, claimed_by) \
         VALUES ($1, $2, $3, CURRENT_DATE + 4, 'claimed', $4) RETURNING id",
        &[org, b, shift_b, x],
    )
    .await;
    let swap_b: Uuid = one(
        pool,
        "INSERT INTO staff_swaps (org_id, requester_id, requester_date, requester_shift_id, \
            peer_id, peer_date, peer_shift_id, status) \
         VALUES ($1, $2, CURRENT_DATE + 5, $4, $3, CURRENT_DATE + 6, $4, 'pending') RETURNING id",
        &[org, x, x2, shift_b],
    )
    .await;
    let period: Uuid = one(
        pool,
        "INSERT INTO payroll_periods (org_id, name, start_date, end_date, status) \
         VALUES ($1, 'P', CURRENT_DATE - 60, CURRENT_DATE - 3, 'generated') RETURNING id",
        &[org],
    )
    .await;
    for who in [x, y] {
        sqlx::query(
            "INSERT INTO payslips (org_id, payroll_period_id, employee_id, base_salary_piastres, \
                worked_days, absent_days, leave_days, late_minutes, overtime_minutes, \
                overtime_piastres, bonuses_piastres, deductions_piastres, \
                advance_installment_piastres, net_piastres, breakdown) \
             VALUES ($1, $2, $3, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, '{}')",
        )
        .bind(org)
        .bind(period)
        .bind(who)
        .execute(pool)
        .await
        .unwrap();
    }
    F {
        org,
        a,
        b,
        owner,
        manager,
        y,
        x,
        x2,
        rec_x,
        cover_x,
        ot_x,
        flag_x,
        req_x,
        adj_x,
        adv_x,
        doc_x,
        sched_x,
        override_x,
        shift_b,
        open_b,
        swap_b,
        period,
        rec_y,
        flag_y,
        req_y,
    }
}

/// Every act of every staff route family, aimed at branch B and its people.
fn at_b(f: &F) -> Vec<(&'static str, String, Value)> {
    let (x, b) = (f.x, f.b);
    let today = Utc::now().date_naive();
    let week = madar_rust::staff::dawam::week_start(today);
    let suggestion = format!("add|{}|{}|{}", today + Duration::days(9), f.shift_b, x);
    vec![
        // people
        ("GET", format!("/staff/employees/{x}"), Value::Null),
        (
            "PUT",
            format!("/staff/employees/{x}"),
            json!({ "job_title": "Barista" }),
        ),
        ("DELETE", format!("/staff/employees/{x}"), Value::Null),
        (
            "GET",
            format!("/staff/employees/{x}/documents"),
            Value::Null,
        ),
        (
            "POST",
            format!("/staff/employees/{x}/documents"),
            json!({ "title": "Contract", "file_url": "/uploads/c" }),
        ),
        (
            "DELETE",
            format!("/staff/documents/{}", f.doc_x),
            Value::Null,
        ),
        (
            "DELETE",
            format!("/staff/employees/{x}/device"),
            Value::Null,
        ),
        (
            "POST",
            "/staff/employees".into(),
            json!({ "name": "New at B", "branch_ids": [b] }),
        ),
        (
            "GET",
            format!("/staff/employees?branch_id={b}"),
            Value::Null,
        ),
        // attendance
        (
            "POST",
            "/staff/attendance".into(),
            json!({ "employee_id": x, "branch_id": b, "business_date": today - Duration::days(9),
                    "status": "absent", "reason": "r" }),
        ),
        (
            "PATCH",
            format!("/staff/attendance/{}", f.rec_x),
            json!({ "reason": "fix", "notes": "n" }),
        ),
        (
            "DELETE",
            format!("/staff/attendance/{}", f.rec_x),
            Value::Null,
        ),
        (
            "GET",
            format!("/staff/attendance?from={today}&to={today}&branch_id={b}"),
            Value::Null,
        ),
        (
            "GET",
            format!("/staff/attendance/summary?from={today}&to={today}&branch_id={b}"),
            Value::Null,
        ),
        (
            "GET",
            format!("/staff/team/presence?branch_id={b}"),
            Value::Null,
        ),
        (
            "GET",
            format!("/staff/discipline-report?from={today}&to={today}&branch_id={b}"),
            Value::Null,
        ),
        (
            "POST",
            "/staff/attendance/punch".into(),
            json!({ "employee_id": x, "reason": "dead phone" }),
        ),
        (
            "PATCH",
            format!("/staff/attendance/{}/cover", f.cover_x),
            json!({ "approve": true }),
        ),
        (
            "PATCH",
            format!("/staff/attendance/{}/overtime", f.ot_x),
            json!({ "approve": true }),
        ),
        (
            "PATCH",
            format!("/staff/attendance/{}/overtime", f.ot_x),
            json!({ "approve": false }),
        ),
        ("GET", format!("/staff/flags?branch_id={b}"), Value::Null),
        (
            "PATCH",
            format!("/staff/flags/{}", f.flag_x),
            json!({ "action": "ignore" }),
        ),
        (
            "GET",
            format!("/staff/attendance/settings?branch_id={b}"),
            Value::Null,
        ),
        // requests and leave
        (
            "PATCH",
            format!("/staff/requests/{}/decision", f.req_x),
            json!({ "status": "approved", "is_paid": true }),
        ),
        (
            "POST",
            "/staff/requests".into(),
            json!({ "employee_id": x, "kind": "leave", "on_date": today + Duration::days(30) }),
        ),
        (
            "PUT",
            "/staff/attendance/settings".into(),
            json!({ "branch_id": b, "absence_deduction_days": 2 }),
        ),
        // the roster
        (
            "POST",
            "/staff/schedules".into(),
            json!({ "employee_id": x, "work_shift_id": f.shift_b }),
        ),
        (
            "DELETE",
            format!("/staff/schedules/{}", f.sched_x),
            Value::Null,
        ),
        (
            "PUT",
            "/staff/schedules/overrides".into(),
            json!({ "employee_id": x, "on_date": today + Duration::days(11) }),
        ),
        (
            "DELETE",
            format!("/staff/schedules/overrides/{}", f.override_x),
            Value::Null,
        ),
        (
            "GET",
            format!("/staff/schedules/day?employee_id={x}&date={today}"),
            Value::Null,
        ),
        (
            "POST",
            "/staff/work-shifts".into(),
            json!({ "branch_id": b, "name": "B night", "start_time": "22:00:00", "end_time": "06:00:00" }),
        ),
        (
            "PATCH",
            format!("/staff/work-shifts/{}", f.shift_b),
            json!({ "branch_id": b, "name": "B day", "start_time": "08:00:00", "end_time": "16:00:00" }),
        ),
        (
            "GET",
            format!("/staff/roster?branch_id={b}&from={today}&to={today}"),
            Value::Null,
        ),
        (
            "POST",
            "/staff/roster/publish".into(),
            json!({ "branch_id": b, "week_start": week }),
        ),
        (
            "POST",
            "/staff/open-shifts".into(),
            json!({ "branch_id": b, "work_shift_id": f.shift_b, "on_date": today + Duration::days(8) }),
        ),
        (
            "PATCH",
            format!("/staff/open-shifts/{}/decision", f.open_b),
            json!({ "approve": true }),
        ),
        (
            "GET",
            format!("/staff/roster/coverage?branch_id={b}"),
            Value::Null,
        ),
        (
            "PUT",
            "/staff/roster/coverage".into(),
            json!({ "branch_id": b, "needs": [] }),
        ),
        (
            "GET",
            format!("/staff/roster/suggestions?branch_id={b}&week_start={week}"),
            Value::Null,
        ),
        (
            "POST",
            "/staff/roster/suggestions/decide".into(),
            json!({ "branch_id": b, "id": suggestion, "accept": false }),
        ),
        (
            "PATCH",
            format!("/staff/swaps/{}/decision", f.swap_b),
            json!({ "approve": true }),
        ),
        // pay
        (
            "POST",
            "/staff/adjustments".into(),
            json!({ "employee_id": x, "kind": "bonus", "amount_piastres": 100, "reason": "r" }),
        ),
        (
            "PATCH",
            format!("/staff/adjustments/bonus/{}/decision", f.adj_x),
            json!({ "approve": true }),
        ),
        (
            "POST",
            format!("/staff/adjustments/bonus/{}/stop", f.adj_x),
            Value::Null,
        ),
        (
            "PATCH",
            format!("/staff/advances/{}/review", f.adv_x),
            json!({ "approve": true }),
        ),
        (
            "PATCH",
            format!("/staff/advances/{}/review", f.adv_x),
            json!({ "approve": false }),
        ),
        (
            "POST",
            "/staff/expense-advances".into(),
            json!({ "employee_id": x, "amount_piastres": 100, "purpose": "Milk", "via": "safe" }),
        ),
        (
            "POST",
            "/staff/payroll/advances".into(),
            json!({ "employee_id": x, "amount_piastres": 100 }),
        ),
        (
            "GET",
            format!("/staff/reports/advances?from={today}&to={today}&branch_id={b}"),
            Value::Null,
        ),
        (
            "GET",
            format!("/staff/reports/labour-vs-sales?from={today}&to={today}&branch_id={b}"),
            Value::Null,
        ),
    ]
}

/// The org-wide acts: the capability at every branch (RO-9), which the
/// manager of one branch never has.
fn org_wide(f: &F) -> Vec<(&'static str, String, Value)> {
    let today = Utc::now().date_naive();
    vec![
        (
            "PUT",
            "/staff/attendance/settings".into(),
            json!({ "advance_cap_percent": 80 }),
        ),
        (
            "PUT",
            format!(
                "/staff/holidays/{}",
                chrono::NaiveDate::from_ymd_opt(2026, 10, 6).unwrap()
            ),
            json!({ "decision": "holiday" }),
        ),
        (
            "GET",
            format!("/staff/roster/fairness?month={today}"),
            Value::Null,
        ),
        (
            "POST",
            "/staff/payroll/periods".into(),
            json!({ "name": "N", "start_date": today, "end_date": today }),
        ),
        (
            "POST",
            format!("/staff/payroll/periods/{}/generate", f.period),
            Value::Null,
        ),
        (
            "PATCH",
            format!("/staff/payroll/periods/{}/status", f.period),
            json!({ "status": "draft" }),
        ),
        (
            "PATCH",
            format!("/staff/payroll/periods/{}/payslips/{}/paid", f.period, f.y),
            json!({ "method": "cash" }),
        ),
        ("GET", "/staff/payroll/current".into(), Value::Null),
        (
            "GET",
            format!("/staff/payroll/periods/{}/preview", f.period),
            Value::Null,
        ),
        (
            "GET",
            format!("/staff/reports/payroll-history?from={today}&to={today}"),
            Value::Null,
        ),
        (
            "DELETE",
            format!("/staff/attendance/settings/branches/{}", f.a),
            Value::Null,
        ),
        (
            "POST",
            "/staff/departments".into(),
            json!({ "name": "Bar" }),
        ),
        (
            "POST",
            "/staff/work-shifts".into(),
            json!({ "name": "Everywhere", "start_time": "08:00:00", "end_time": "16:00:00" }),
        ),
        (
            "POST",
            "/staff/payroll/bonuses".into(),
            json!({ "employee_id": f.y, "amount_piastres": 100, "reason": "r", "effective_date": today }),
        ),
        (
            "PATCH",
            format!("/staff/payroll/advances/{}/decision", f.adv_x),
            json!({ "status": "approved" }),
        ),
    ]
}

#[sqlx::test]
async fn a_manager_of_branch_a_is_refused_on_every_family_at_branch_b(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let mgr = user_token(f.manager, f.org, UserRole::BranchManager);
    let mut refused = 0;
    for (method, uri, b) in at_b(&f) {
        let resp = call!(app, method, uri, mgr, b);
        assert_eq!(
            resp.status(),
            403,
            "{method} {uri}: a manager of A acting at B — {}",
            body(resp).await
        );
        refused += 1;
    }
    for (method, uri, b) in org_wide(&f) {
        let resp = call!(app, method, uri, mgr, b);
        assert_eq!(resp.status(), 403, "{method} {uri}: an org-wide act");
        refused += 1;
    }
    assert!(refused > 60, "{refused}");

    // Nothing at B changed.
    let (flag, req, status): (Option<String>, String, String) = sqlx::query_as(
        "SELECT (SELECT resolution FROM attendance_flags WHERE id = $1), \
                (SELECT status FROM staff_requests WHERE id = $2), \
                (SELECT employment_status FROM employees WHERE id = $3)",
    )
    .bind(f.flag_x)
    .bind(f.req_x)
    .bind(f.x)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (flag, req.as_str(), status.as_str()),
        (None, "pending", "active")
    );
}

/// The same refusals through the manager's phone (a linked employee's staff
/// token acts through their account, with the same scope).
#[sqlx::test]
async fn the_managers_phone_has_the_same_scope(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let me = employee(
        &pool,
        f.org,
        "Manager",
        Some(f.manager),
        Some("+201050000009"),
        true,
        &[f.a],
        1,
    )
    .await;
    let s = session(&pool, me).await;
    let phone = format!("{}|{}", s.token, s.device);
    for (method, uri, b) in at_b(&f).into_iter().chain(org_wide(&f)) {
        let resp = call!(app, method, uri, phone, b);
        assert_eq!(resp.status(), 403, "{method} {uri} from the phone");
    }
    let resp = call!(
        app,
        "PATCH",
        format!("/staff/flags/{}", f.flag_y),
        phone,
        json!({ "action": "ignore" })
    );
    assert_eq!(resp.status(), 200, "their own branch, from the phone");
}

#[sqlx::test]
async fn the_same_acts_at_the_managers_own_branch_go_through(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let mgr = user_token(f.manager, f.org, UserRole::BranchManager);
    let y = f.y;
    for (method, uri, b, want) in [
        ("GET", format!("/staff/employees/{y}"), Value::Null, 200),
        (
            "PUT",
            format!("/staff/employees/{y}"),
            json!({ "job_title": "Lead" }),
            200,
        ),
        (
            "PATCH",
            format!("/staff/flags/{}", f.flag_y),
            json!({ "action": "ignore" }),
            200,
        ),
        (
            "PATCH",
            format!("/staff/requests/{}/decision", f.req_y),
            json!({ "status": "approved", "is_paid": true }),
            200,
        ),
        (
            "PATCH",
            format!("/staff/attendance/{}", f.rec_y),
            json!({ "reason": "fix" }),
            200,
        ),
        (
            "POST",
            "/staff/attendance/punch".into(),
            json!({ "employee_id": y, "reason": "dead phone" }),
            200,
        ),
        (
            "POST",
            "/staff/adjustments".into(),
            json!({ "employee_id": y, "kind": "bonus", "amount_piastres": 100, "reason": "r" }),
            201,
        ),
        (
            "POST",
            "/staff/expense-advances".into(),
            json!({ "employee_id": y, "amount_piastres": 100, "purpose": "Milk", "via": "safe" }),
            201,
        ),
        (
            "POST",
            "/staff/employees".into(),
            json!({ "name": "New at A", "branch_ids": [f.a] }),
            201,
        ),
    ] {
        let resp = call!(app, method, uri, mgr, b);
        assert_eq!(
            resp.status(),
            want,
            "{method} {uri} at A: {}",
            body(resp).await
        );
    }
    // The owner does all of it everywhere.
    let owner = user_token(f.owner, f.org, UserRole::OrgAdmin);
    let resp = call!(
        app,
        "PATCH",
        format!("/staff/flags/{}", f.flag_x),
        owner,
        json!({ "action": "ignore" })
    );
    assert_eq!(resp.status(), 200);
    let resp = call!(
        app,
        "PUT",
        "/staff/attendance/settings",
        owner,
        json!({ "advance_cap_percent": 45 })
    );
    assert_eq!(resp.status(), 200);
}

/// Lists show the manager their own branch's people and records only.
#[sqlx::test]
async fn the_managers_lists_hold_only_their_branch(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let mgr = user_token(f.manager, f.org, UserRole::BranchManager);
    let today = Utc::now().date_naive();
    let ids = |v: &Value, key: &str| -> Vec<Uuid> {
        v.as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|r| serde_json::from_value(r[key].clone()).ok())
            .collect()
    };
    let employees = body(call!(app, "GET", "/staff/employees", mgr, Value::Null)).await;
    assert_eq!(ids(&employees, "id"), vec![f.y], "{employees}");
    let requests = body(call!(app, "GET", "/staff/requests", mgr, Value::Null)).await;
    assert_eq!(ids(&requests, "employee_id"), vec![f.y]);
    let from = today - Duration::days(10);
    let records = body(call!(
        app,
        "GET",
        format!("/staff/attendance?from={from}&to={today}"),
        mgr,
        Value::Null
    ))
    .await;
    assert!(
        ids(&records, "employee_id").iter().all(|e| *e == f.y),
        "{records}"
    );
    assert!(!ids(&records, "employee_id").is_empty());
    let flags = body(call!(app, "GET", "/staff/flags", mgr, Value::Null)).await;
    assert_eq!(ids(&flags, "employee_id"), vec![f.y]);
    let swaps = body(call!(app, "GET", "/staff/swaps", mgr, Value::Null)).await;
    assert!(swaps.as_array().unwrap().is_empty(), "{swaps}");
    let lines = body(call!(app, "GET", "/staff/adjustments", mgr, Value::Null)).await;
    assert!(
        lines.as_array().unwrap().is_empty(),
        "B's pay lines: {lines}"
    );
    let advances = body(call!(
        app,
        "GET",
        "/staff/payroll/advances",
        mgr,
        Value::Null
    ))
    .await;
    assert!(advances.as_array().unwrap().is_empty(), "{advances}");
    let open = body(call!(
        app,
        "GET",
        format!(
            "/staff/open-shifts?from={today}&to={}",
            today + Duration::days(9)
        ),
        mgr,
        Value::Null
    ))
    .await;
    assert!(open.as_array().unwrap().is_empty(), "{open}");
    let presence = body(call!(app, "GET", "/staff/team/presence", mgr, Value::Null)).await;
    assert_eq!(ids(&presence["rows"], "employee_id"), vec![f.y]);
    let people = body(call!(
        app,
        "GET",
        format!("/staff/branches/{}/people", f.a),
        mgr,
        Value::Null
    ))
    .await;
    assert_eq!(ids(&people, "employee_id"), vec![f.y]);
    let resp = call!(
        app,
        "GET",
        format!("/staff/branches/{}/people", f.b),
        mgr,
        Value::Null
    );
    assert_eq!(resp.status(), 403, "B's till picker is B's");
    // The owner's are everyone's.
    let owner = user_token(f.owner, f.org, UserRole::OrgAdmin);
    let employees = body(call!(app, "GET", "/staff/employees", owner, Value::Null)).await;
    assert_eq!(employees.as_array().unwrap().len(), 3);
    let _ = (f.x2, NaiveTime::MIN);
}
