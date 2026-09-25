//! Dawam money (Phase B, audit 05): a frozen month, the advance ledger, the
//! one shift-pricing function under branch rules, limits, caps, exports, the
//! audit log, and the punch → sweep → penalty → waive → approve → payslip
//! path (AT-12). Every figure is checked against a hand computation.
//!
//! Fixture: 26 working days a month, ladder 1–15 → 15 min, 16–30 → 60 min,
//! 31+ → half a day; overtime automatic at 1.35 / 1.70 (night 22–06).
//! Amal (branch A) earns 600,000 pt: a day is 23,076.92 pt, a minute of an
//! 8-hour day 48.0769 pt. Bassem (branch B) earns 500,000 pt.

use actix_web::{App, test, web};
use chrono::{Datelike, Duration, NaiveDate, NaiveTime, Timelike, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::JwtSecret;
use madar_rust::models::UserRole;

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

async fn text_of(resp: actix_web::dev::ServiceResponse) -> String {
    String::from_utf8(test::read_body(resp).await.to_vec()).unwrap()
}

const LAT: f64 = 29.9792;
const LNG: f64 = 31.1342;

struct F {
    org: Uuid,
    a: Uuid,
    b: Uuid,
    owner: Uuid,
    mgr: Uuid,
    amal: Uuid,
    bassem: Uuid,
    /// The current period (the org's start day is the 1st): first and last day.
    start: NaiveDate,
    end: NaiveDate,
    period: Uuid,
}

impl F {
    fn owner(&self) -> String {
        token_for(self.owner, self.org, UserRole::OrgAdmin)
    }
    fn mgr(&self) -> String {
        token_for(self.mgr, self.org, UserRole::BranchManager)
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
    .bind(format!("{id}@test.com"))
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

async fn branch(pool: &PgPool, org: Uuid, name: &str, tz: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO branches (id, org_id, name, timezone, latitude, longitude, geo_radius_meters) \
         VALUES ($1, $2, $3, $4::timezone_name, $5, $6, 200)",
    )
    .bind(id)
    .bind(org)
    .bind(name)
    .bind(tz)
    .bind(LAT)
    .bind(LNG)
    .execute(pool)
    .await
    .unwrap();
    id
}

const LADDER: &str = r#"[{"from_minutes":1,"to_minutes":15,"kind":"minutes","value":15},
    {"from_minutes":16,"to_minutes":30,"kind":"minutes","value":60},
    {"from_minutes":31,"to_minutes":null,"kind":"day_fraction","value":0.5}]"#;

async fn seed_with_zone(pool: &PgPool, tz: &str) -> F {
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    // The owner's legacy payroll cells, before the org's roles are made.
    for act in ["create", "read", "update", "delete"] {
        sqlx::query(
            "INSERT INTO role_permissions (role, resource, action, granted) \
             VALUES ('org_admin'::user_role, 'payroll'::permission_resource, $1::permission_action, true) \
             ON CONFLICT DO NOTHING",
        )
        .bind(act)
        .execute(pool)
        .await
        .unwrap();
    }
    let org = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, modules) VALUES ($1, 'Cafe', $2, '{pos,dawam}')",
    )
    .bind(org)
    .bind(format!("org-{org}"))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO attendance_settings (org_id, rules_saved_at, working_days_per_month, \
             overtime_mode, period_start_day, late_deduction_tiers, absence_deduction_days) \
         VALUES ($1, now() - INTERVAL '60 days', 26, 'automatic', 1, $2::jsonb, 1)",
    )
    .bind(org)
    .bind(LADDER)
    .execute(pool)
    .await
    .unwrap();
    let a = branch(pool, org, "A", tz).await;
    let b = branch(pool, org, "B", tz).await;
    let owner = user(pool, org, "Owner", "org_admin").await;
    let mgr = user(pool, org, "Manager", "branch_manager").await;
    assign(pool, mgr, a).await;
    let amal = common::employees::employee(
        pool,
        org,
        "Amal",
        None,
        Some("+201012345678"),
        true,
        &[a],
        600_000,
    )
    .await;
    let bassem = common::employees::employee(
        pool,
        org,
        "Bassem",
        None,
        Some("+201012345679"),
        true,
        &[b],
        500_000,
    )
    .await;
    // The period the org's clock is in (start day 1 = the calendar month).
    let today: NaiveDate = sqlx::query_scalar("SELECT (now() AT TIME ZONE $1)::date")
        .bind(tz)
        .fetch_one(pool)
        .await
        .unwrap();
    let (start, end) = madar_rust::staff::dawam::pay::period_window(today, 1);
    let period: Uuid = sqlx::query_scalar(
        "INSERT INTO payroll_periods (org_id, name, start_date, end_date) \
         VALUES ($1, 'This month', $2, $3) RETURNING id",
    )
    .bind(org)
    .bind(start)
    .bind(end)
    .fetch_one(pool)
    .await
    .unwrap();
    F {
        org,
        a,
        b,
        owner,
        mgr,
        amal,
        bassem,
        start,
        end,
        period,
    }
}

async fn seed(pool: &PgPool) -> F {
    seed_with_zone(pool, "UTC").await
}

/// One attendance day, inserted as the clock would have left it: an 8-hour
/// shift from `start_hour` UTC, with `late` minutes late and `ot` minutes
/// past the end.
#[allow(clippy::too_many_arguments)]
async fn day(
    pool: &PgPool,
    f: &F,
    emp: Uuid,
    branch: Uuid,
    date: NaiveDate,
    status: &str,
    start_hour: u32,
    sched_minutes: i64,
    late: i32,
    ot: i32,
) -> Uuid {
    let start = date.and_hms_opt(start_hour, 0, 0).unwrap().and_utc();
    let end = start + Duration::minutes(sched_minutes);
    let (check_in, check_out) = if status == "absent" || status == "on_leave" {
        (None, None)
    } else {
        (
            Some(start + Duration::minutes(i64::from(late))),
            Some(end + Duration::minutes(i64::from(ot))),
        )
    };
    let worked = if let (Some(i), Some(o)) = (check_in, check_out) {
        (o - i).num_minutes() as i32
    } else {
        0
    };
    sqlx::query_scalar(
        "INSERT INTO attendance_records (org_id, employee_id, branch_id, business_date, status, \
             scheduled_start_at, scheduled_end_at, check_in_at, check_out_at, late_minutes, \
             worked_minutes, overtime_minutes, overtime_status) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, \
                 CASE WHEN $12 > 0 THEN 'pending' END) RETURNING id",
    )
    .bind(f.org)
    .bind(emp)
    .bind(branch)
    .bind(date)
    .bind(status)
    .bind(start)
    .bind(end)
    .bind(check_in)
    .bind(check_out)
    .bind(late)
    .bind(worked)
    .bind(ot)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn generate(app: &impl ServiceT, f: &F) -> actix_web::dev::ServiceResponse {
    call!(
        app,
        post,
        format!("/staff/payroll/periods/{}/generate", f.period),
        f.owner(),
        json!({})
    )
}

async fn reopen(app: &impl ServiceT, f: &F) -> actix_web::dev::ServiceResponse {
    call!(
        app,
        patch,
        format!("/staff/payroll/periods/{}/status", f.period),
        f.owner(),
        json!({ "status": "draft", "reason": "a line was missing" })
    )
}

trait ServiceT:
    actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >
{
}
impl<T> ServiceT for T where
    T: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >
{
}

async fn slip_of(app: &impl ServiceT, f: &F, emp: Uuid) -> Value {
    let cur = json_of(call!(app, get, "/staff/payroll/current", f.owner())).await;
    let list = if cur["payslips"].as_array().is_some_and(|a| !a.is_empty()) {
        &cur["payslips"]
    } else {
        &cur["preview"]
    };
    list.as_array()
        .unwrap()
        .iter()
        .find(|s| s["employee_id"] == json!(emp))
        .cloned()
        .unwrap_or(Value::Null)
}

async fn period_status(pool: &PgPool, id: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM payroll_periods WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn remaining(pool: &PgPool, advance: Uuid) -> (i64, String) {
    sqlx::query_as("SELECT remaining_piastres, status FROM salary_advances WHERE id = $1")
        .bind(advance)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn approved_advance(pool: &PgPool, f: &F, emp: Uuid, amount: i64, installments: i32) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO salary_advances (org_id, employee_id, amount_piastres, installments, \
             monthly_installment_piastres, remaining_piastres, status) \
         VALUES ($1, $2, $3, $4, $5, $3, 'approved') RETURNING id",
    )
    .bind(f.org)
    .bind(emp)
    .bind(amount)
    .bind(installments)
    .bind((amount as u64).div_ceil(installments as u64) as i64)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn audit_actions(pool: &PgPool, org: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT action FROM payroll_audit_log WHERE org_id = $1 ORDER BY created_at, action",
    )
    .bind(org)
    .fetch_all(pool)
    .await
    .unwrap()
}

// ── the frozen month (P0: PAY-5, PAY-6, AD-7, AD-10, B3, B7) ───────────────

#[sqlx::test]
async fn an_approved_month_is_frozen_until_reopened(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner();
    // One late day (24 min → 60 min of pay = 600000×60/12480 = 2884.6 → 2885)
    // priced by the sweep's function, and a manual bonus.
    let rec = day(&pool, &f, f.amal, f.a, f.start, "late", 9, 480, 24, 0).await;
    let settings = madar_rust::staff::attendance::load_settings(&pool, f.org, None)
        .await
        .unwrap();
    madar_rust::staff::penalties::recompute_record(&pool, rec, &settings)
        .await
        .unwrap();
    let (penalty_id, penalty): (Uuid, i64) = sqlx::query_as(
        "SELECT id, amount_piastres FROM payroll_deductions WHERE attendance_record_id = $1",
    )
    .bind(rec)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(penalty, 2_885);
    let bonus = json_of(call!(
        app,
        post,
        "/staff/adjustments",
        owner,
        json!({ "employee_id": f.amal, "kind": "bonus", "amount_piastres": 10_000,
                "reason": "Great week", "effective_date": f.start })
    ))
    .await;
    assert_eq!(bonus["status"], "approved");
    let bonus_id = bonus["id"].as_str().unwrap().to_string();

    let resp = generate(&app, &f).await;
    assert_eq!(resp.status(), 200);
    let slip = slip_of(&app, &f, f.amal).await;
    // 600000 + 10000 − 2885
    assert_eq!(slip["net_piastres"], 607_115);
    assert_eq!(period_status(&pool, f.period).await, "generated");

    // ── closed to everything dated inside it ──
    let resp = call!(
        app,
        post,
        "/staff/adjustments",
        owner,
        json!({ "employee_id": f.amal, "kind": "bonus", "amount_piastres": 5_000,
                "reason": "Late add", "effective_date": f.start })
    );
    assert_eq!(resp.status(), 409, "a pay line in an approved month");
    assert!(text_of(resp).await.contains("PERIOD_CLOSED"));
    let resp = call!(
        app,
        patch,
        format!("/staff/payroll/deductions/{penalty_id}/waive"),
        owner,
        json!({ "reason": "Traffic" })
    );
    assert_eq!(resp.status(), 409, "a waiver in an approved month");
    let resp = call!(
        app,
        patch,
        format!("/staff/payroll/deductions/{penalty_id}/override"),
        owner,
        json!({ "amount_piastres": 100, "reason": "Half" })
    );
    assert_eq!(resp.status(), 409, "an override in an approved month");
    let resp = call!(
        app,
        delete,
        format!("/staff/payroll/bonuses/{bonus_id}"),
        owner
    );
    assert_eq!(
        resp.status(),
        409,
        "deleting a manual line in an approved month"
    );
    assert_eq!(
        generate(&app, &f).await.status(),
        409,
        "no regenerate without a reopen"
    );
    let resp = call!(
        app,
        delete,
        format!("/staff/payroll/periods/{}", f.period),
        owner
    );
    assert_eq!(resp.status(), 409, "no delete of an approved month");
    // A correction's recompute leaves the frozen penalty alone.
    sqlx::query("UPDATE attendance_records SET late_minutes = 0 WHERE id = $1")
        .bind(rec)
        .execute(&pool)
        .await
        .unwrap();
    let touched = madar_rust::staff::penalties::recompute_record(&pool, rec, &settings)
        .await
        .unwrap();
    assert_eq!(touched, 0);
    let still: i64 =
        sqlx::query_scalar("SELECT amount_piastres FROM payroll_deductions WHERE id = $1")
            .bind(penalty_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(still, 2_885, "the approved month's penalty is a snapshot");
    // The status route never approves or pays by hand.
    for target in ["generated", "paid"] {
        let resp = call!(
            app,
            patch,
            format!("/staff/payroll/periods/{}/status", f.period),
            owner,
            json!({ "status": target })
        );
        assert_eq!(resp.status(), 409, "{target} by hand");
    }

    // ── reopen (with a reason): the month is live again ──
    let resp = call!(
        app,
        patch,
        format!("/staff/payroll/periods/{}/status", f.period),
        owner,
        json!({ "status": "draft" })
    );
    assert_eq!(resp.status(), 400, "reopening needs a reason");
    assert_eq!(reopen(&app, &f).await.status(), 200);
    assert_eq!(period_status(&pool, f.period).await, "draft");
    let slips: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM payslips WHERE payroll_period_id = $1")
            .bind(f.period)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(slips, 0, "a draft holds no frozen payslips");
    // Now the penalty recomputes (the record was corrected to 0 late).
    madar_rust::staff::penalties::recompute_record(&pool, rec, &settings)
        .await
        .unwrap();
    let gone: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payroll_deductions WHERE id = $1")
        .bind(penalty_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(gone, 0);
    let resp = call!(
        app,
        delete,
        format!("/staff/payroll/bonuses/{bonus_id}"),
        owner
    );
    assert_eq!(
        resp.status(),
        204,
        "manual lines can go while the month is open"
    );
    let slip = slip_of(&app, &f, f.amal).await;
    assert_eq!(slip["net_piastres"], 600_000);
    let actions = audit_actions(&pool, f.org).await;
    assert!(actions.contains(&"period.generate".into()));
    assert!(actions.contains(&"period.reopen".into()));
    assert!(actions.contains(&"adjustment.delete".into()));
}

#[sqlx::test]
async fn reopening_refunds_through_the_ledger_and_reapproving_collects_once(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    // 100,000 over 2 installments: 50,000 a month.
    let adv = approved_advance(&pool, &f, f.amal, 100_000, 2).await;
    let owner = f.owner();

    // The preview before, and what approving produces, agree (PAY-2).
    let before = slip_of(&app, &f, f.amal).await;
    assert_eq!(before["advance_installment_piastres"], 50_000);
    assert_eq!(before["net_piastres"], 550_000);
    assert_eq!(generate(&app, &f).await.status(), 200);
    assert_eq!(remaining(&pool, adv).await, (50_000, "approved".into()));
    let ledger: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM salary_advance_collections WHERE advance_id = $1")
            .bind(adv)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(ledger, 1);

    // Reopen: the collection goes with the payslip, the balance comes back,
    // and the preview shows the installment again (audit B7).
    assert_eq!(reopen(&app, &f).await.status(), 200);
    assert_eq!(remaining(&pool, adv).await, (100_000, "approved".into()));
    let again = slip_of(&app, &f, f.amal).await;
    assert_eq!(again["advance_installment_piastres"], 50_000);
    assert_eq!(
        again["net_piastres"], 550_000,
        "the preview is what re-approving produces"
    );

    // Approve, reopen, approve, reopen, approve: still exactly one installment.
    for _ in 0..2 {
        assert_eq!(generate(&app, &f).await.status(), 200);
        assert_eq!(reopen(&app, &f).await.status(), 200);
    }
    assert_eq!(generate(&app, &f).await.status(), 200);
    assert_eq!(remaining(&pool, adv).await, (50_000, "approved".into()));

    // The balance is rebuildable from the ledger alone (AV-6).
    sqlx::query("UPDATE salary_advances SET remaining_piastres = 7 WHERE id = $1")
        .bind(adv)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("SELECT salary_advance_sync_remaining($1)")
        .bind(adv)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(remaining(&pool, adv).await.0, 50_000);

    // A one-installment advance settles, and a reopen un-settles it.
    let one = approved_advance(&pool, &f, f.bassem, 30_000, 1).await;
    assert_eq!(reopen(&app, &f).await.status(), 200);
    assert_eq!(generate(&app, &f).await.status(), 200);
    assert_eq!(remaining(&pool, one).await, (0, "settled".into()));
    assert_eq!(reopen(&app, &f).await.status(), 200);
    assert_eq!(remaining(&pool, one).await, (30_000, "approved".into()));
    // Deleting the draft refunds too.
    let resp = call!(
        app,
        delete,
        format!("/staff/payroll/periods/{}", f.period),
        owner
    );
    assert_eq!(resp.status(), 204);
    assert_eq!(remaining(&pool, adv).await.0, 100_000);
}

#[sqlx::test]
async fn a_paid_person_locks_reopen_and_delete_and_the_month_pays_itself(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner();
    // Someone with nothing to pay: a 0-salary owner-employee on payroll.
    let nobody =
        common::employees::employee(&pool, f.org, "Zero", None, None, false, &[f.a], 0).await;
    assert_eq!(generate(&app, &f).await.status(), 200);
    let (method, by): (Option<String>, Option<Uuid>) =
        sqlx::query_as("SELECT paid_method, paid_by FROM payslips WHERE employee_id = $1")
            .bind(nobody)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        method.as_deref(),
        Some("none"),
        "a 0-net payslip is marked by the run"
    );
    assert_eq!(by, Some(f.owner));
    // That mark is not a payment: the month can still be reopened…
    assert_eq!(reopen(&app, &f).await.status(), 200);
    assert_eq!(generate(&app, &f).await.status(), 200);

    // …until a real person is paid.
    let resp = call!(
        app,
        patch,
        format!(
            "/staff/payroll/periods/{}/payslips/{}/paid",
            f.period, f.amal
        ),
        owner,
        json!({ "method": "cash" })
    );
    assert_eq!(resp.status(), 200);
    let paid = json_of(resp).await;
    assert_eq!(paid["paid_method"], "cash");
    assert_eq!(paid["paid_by"], json!(f.owner));
    assert_eq!(reopen(&app, &f).await.status(), 409);
    let resp = call!(
        app,
        delete,
        format!("/staff/payroll/periods/{}", f.period),
        owner
    );
    assert_eq!(resp.status(), 409);
    assert_eq!(generate(&app, &f).await.status(), 409);
    // 'none' is the run's mark, never a client's.
    let resp = call!(
        app,
        patch,
        format!(
            "/staff/payroll/periods/{}/payslips/{}/paid",
            f.period, f.bassem
        ),
        owner,
        json!({ "method": "none" })
    );
    assert_eq!(resp.status(), 400);
    assert_eq!(period_status(&pool, f.period).await, "generated");
    let resp = call!(
        app,
        patch,
        format!(
            "/staff/payroll/periods/{}/payslips/{}/paid",
            f.period, f.bassem
        ),
        owner,
        json!({ "method": "wallet" })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(
        period_status(&pool, f.period).await,
        "paid",
        "the 0-net payslip never blocks the month reaching Paid (PAY-7)"
    );
    // A paid month closes, and only closes.
    let resp = call!(
        app,
        patch,
        format!("/staff/payroll/periods/{}/status", f.period),
        owner,
        json!({ "status": "draft", "reason": "oops" })
    );
    assert_eq!(resp.status(), 409);
    let resp = call!(
        app,
        patch,
        format!("/staff/payroll/periods/{}/status", f.period),
        owner,
        json!({ "status": "closed" })
    );
    assert_eq!(resp.status(), 200);
    let cur = json_of(call!(app, get, "/staff/payroll/current", owner)).await;
    assert_eq!(cur["paid_count"], 3);
    assert_eq!(cur["totals"]["net_piastres"], 1_100_000);
    assert_eq!(cur["totals"]["people"], 3);
    let actions = audit_actions(&pool, f.org).await;
    assert_eq!(actions.iter().filter(|a| *a == "payslip.paid").count(), 2);
    assert!(actions.contains(&"period.close".into()));
}

// ── not on payroll (owner decision 2026-09-23) ─────────────────────────────

#[sqlx::test]
async fn people_not_on_payroll_are_skipped_but_keep_the_app(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner();
    let get_flag = || async {
        sqlx::query_scalar::<_, bool>("SELECT on_payroll FROM employees WHERE id = $1")
            .bind(f.amal)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    assert!(get_flag().await, "on payroll by default");
    let me = json_of(call!(app, get, "/staff/employees", owner)).await;
    let amal = me
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == json!(f.amal))
        .unwrap();
    assert_eq!(amal["on_payroll"], true);

    // A branch manager (no hr.payroll.edit everywhere) cannot flip it: the
    // field is ignored like the salary.
    let resp = call!(
        app,
        put,
        format!("/staff/employees/{}", f.amal),
        f.mgr(),
        json!({ "name": "Amal", "on_payroll": false })
    );
    assert_eq!(resp.status(), 200);
    assert!(get_flag().await, "a manager's flip is ignored");
    let resp = call!(
        app,
        put,
        format!("/staff/employees/{}", f.amal),
        owner,
        json!({ "name": "Amal", "on_payroll": false })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(json_of(resp).await["on_payroll"], false);
    assert!(!get_flag().await);

    // Skipped by the preview, the estimate and the payslips; still an
    // employee the roster and the clock know.
    let cur = json_of(call!(app, get, "/staff/payroll/current", owner)).await;
    let ids: Vec<Value> = cur["preview"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["employee_id"].clone())
        .collect();
    assert!(!ids.contains(&json!(f.amal)));
    assert!(ids.contains(&json!(f.bassem)));
    let est = json_of(call!(
        app,
        get,
        "/staff/me/pay/estimate",
        phone_token(&pool, f.amal).await
    ))
    .await;
    assert_eq!(est["on_payroll"], false);
    assert!(est["slip"].is_null());
    assert_eq!(generate(&app, &f).await.status(), 200);
    let slips: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payslips WHERE employee_id = $1")
        .bind(f.amal)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(slips, 0);
    // The month's totals count only the people on it, and it is Paid once
    // THEY are paid: nobody has to "pay" Amal 0 EGP (PAY-7).
    let cur = json_of(call!(app, get, "/staff/payroll/current", owner)).await;
    assert_eq!(cur["totals"]["people"], 1, "{cur}");
    assert_eq!(cur["paid_count"], 0);
    let resp = call!(
        app,
        patch,
        format!(
            "/staff/payroll/periods/{}/payslips/{}/paid",
            f.period, f.bassem
        ),
        owner,
        json!({ "method": "cash" })
    );
    assert_eq!(resp.status(), 200, "{}", text_of(resp).await);
    assert_eq!(period_status(&pool, f.period).await, "paid");
    let cur = json_of(call!(app, get, "/staff/payroll/current", owner)).await;
    assert_eq!(cur["paid_count"], 1);
    let ctx = json_of(call!(
        app,
        get,
        "/staff/me/context",
        phone_token(&pool, f.amal).await
    ))
    .await;
    assert_eq!(ctx["employee_id"], json!(f.amal), "the app still boots");
}

// ── advances (AV-2, AV-5, B10) ─────────────────────────────────────────────

#[sqlx::test]
async fn advances_over_the_cap_wait_for_the_owner_and_record_is_atomic(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner();
    let mgr = f.mgr();
    // Cap: 50% of 600,000 = 300,000 outstanding.
    macro_rules! ask {
        ($amount:expr) => {{
            let s = phone_token(&pool, f.amal).await;
            json_of(call!(app, post, "/staff/me/advances", s, json!({ "amount_piastres": $amount, "installments": 3 }))).await
        }};
    }
    let first = ask!(200_000);
    let resp = call!(
        app,
        patch,
        format!("/staff/advances/{}/review", first["id"].as_str().unwrap()),
        mgr,
        json!({ "approve": true })
    );
    assert_eq!(resp.status(), 200, "within the cap and within 50%");
    let row = json_of(resp).await;
    // The cap is half the salary: the manager sees only that it is within
    // it (D7); the owner sees the figure.
    assert!(row["cap_piastres"].is_null(), "{row}");
    assert_eq!(row["within_cap"], true);
    assert_eq!(row["outstanding_piastres"], 200_000);
    assert_eq!(
        row["monthly_installment_piastres"], 66_667,
        "200000/3 rounded up"
    );

    // 200,000 more would put 400,000 outstanding: over the cap for the manager.
    let second = ask!(200_000);
    let resp = call!(
        app,
        patch,
        format!("/staff/advances/{}/review", second["id"].as_str().unwrap()),
        mgr,
        json!({ "approve": true })
    );
    assert_eq!(resp.status(), 409);
    assert!(text_of(resp).await.contains("ADVANCE_OVER_CAP"));
    assert_eq!(
        remaining(
            &pool,
            Uuid::parse_str(second["id"].as_str().unwrap()).unwrap()
        )
        .await
        .1,
        "pending"
    );
    // The manager may approve less: 100,000 keeps it at the cap exactly.
    let resp = call!(
        app,
        patch,
        format!("/staff/advances/{}/review", second["id"].as_str().unwrap()),
        mgr,
        json!({ "approve": true, "amount_piastres": 100_000 })
    );
    assert_eq!(resp.status(), 200);
    // Over the cap needs the owner (AV-5).
    let third = ask!(50_000);
    let resp = call!(
        app,
        patch,
        format!("/staff/advances/{}/review", third["id"].as_str().unwrap()),
        mgr,
        json!({ "approve": true })
    );
    assert_eq!(resp.status(), 409);
    let resp = call!(
        app,
        patch,
        format!("/staff/advances/{}/review", third["id"].as_str().unwrap()),
        owner,
        json!({ "approve": true })
    );
    assert_eq!(resp.status(), 200, "the owner goes over the cap");

    // Recording directly is ONE call: refused, nothing is left behind.
    let before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM salary_advances WHERE employee_id = $1")
            .bind(f.bassem)
            .fetch_one(&pool)
            .await
            .unwrap();
    let resp = call!(
        app,
        post,
        "/staff/advances/record",
        owner,
        json!({ "employee_id": f.bassem, "amount_piastres": 100_000, "installments": 25 })
    );
    assert_eq!(resp.status(), 400, "installments are 1–24 everywhere");
    let resp = call!(
        app,
        post,
        "/staff/advances/record",
        mgr,
        json!({ "employee_id": f.bassem, "amount_piastres": 100_000, "installments": 2 })
    );
    assert_eq!(
        resp.status(),
        403,
        "Bassem is at branch B, not the manager's"
    );
    let after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM salary_advances WHERE employee_id = $1")
            .bind(f.bassem)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        before, after,
        "a refused record leaves no stray pending advance"
    );
    let resp = call!(
        app,
        post,
        "/staff/advances/record",
        owner,
        json!({ "employee_id": f.bassem, "amount_piastres": 100_000, "installments": 4, "reason": "Rent" })
    );
    assert_eq!(resp.status(), 201);
    let rec = json_of(resp).await;
    assert_eq!(rec["status"], "approved");
    assert_eq!(rec["monthly_installment_piastres"], 25_000);
    assert_eq!(rec["decided_by"], json!(f.owner));
    // The retired legacy decide route is gone.
    let resp = call!(
        app,
        patch,
        format!(
            "/staff/payroll/advances/{}/decision",
            rec["id"].as_str().unwrap()
        ),
        owner,
        json!({ "status": "approved" })
    );
    assert_eq!(resp.status(), 404);
    let actions = audit_actions(&pool, f.org).await;
    assert!(actions.contains(&"advance.record".into()));
    assert_eq!(actions.iter().filter(|a| *a == "advance.decide").count(), 3);
}

// ── limits (AD-5, B5) ──────────────────────────────────────────────────────

#[sqlx::test]
async fn bonus_and_deduction_limits_are_separate_and_percent_lines_are_valued(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner();
    let mgr = f.mgr();
    // The manager's deduction ceiling drops to 200 EGP; bonuses stay at 1,000.
    sqlx::query(
        "UPDATE org_role_grants g SET limits = '{\"max_amount\": 20000}'::jsonb \
           FROM org_roles r WHERE r.id = g.org_role_id AND r.org_id = $1 \
            AND r.kind::text = 'branch_manager' AND g.capability_id = 245",
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
    let ctx_mgr_employee = common::employees::employee(
        &pool,
        f.org,
        "Mgr",
        Some(f.mgr),
        Some("+201012345670"),
        true,
        &[f.a],
        0,
    )
    .await;
    let ctx = json_of(call!(
        app,
        get,
        "/staff/me/context",
        phone_token(&pool, ctx_mgr_employee).await
    ))
    .await;
    assert_eq!(ctx["adjustment_limit_piastres"], 100_000, "{ctx}");
    assert_eq!(ctx["deduction_limit_piastres"], 20_000, "{ctx}");

    let bonus = json_of(call!(
        app,
        post,
        "/staff/adjustments",
        mgr,
        json!({ "employee_id": f.amal, "kind": "bonus", "amount_piastres": 50_000, "reason": "Sales" })
    ))
    .await;
    assert_eq!(
        bonus["status"], "approved",
        "50 EGP... 500 EGP under the 1,000 bonus limit"
    );
    let ded = json_of(call!(
        app,
        post,
        "/staff/adjustments",
        mgr,
        json!({ "employee_id": f.amal, "kind": "deduction", "amount_piastres": 50_000, "reason": "Breakage" })
    ))
    .await;
    assert_eq!(
        ded["status"], "pending",
        "the same 500 EGP is over the 200 EGP deduction limit"
    );
    let resp = call!(
        app,
        post,
        "/staff/adjustments",
        mgr,
        json!({ "employee_id": f.amal, "kind": "deduction", "percent_of_base": 5, "reason": "x" })
    );
    assert_eq!(resp.status(), 400, "a deduction is an amount (AD-2)");

    // A 20% bonus on 600,000 is 120,000: over the manager's 1,000 EGP, so it
    // waits — and another manager with the same limit can't settle it (B5).
    let pct = json_of(call!(
        app,
        post,
        "/staff/adjustments",
        mgr,
        json!({ "employee_id": f.amal, "kind": "bonus", "percent_of_base": 20, "reason": "Target" })
    ))
    .await;
    assert_eq!(pct["status"], "pending");
    assert_eq!(
        pct["value_piastres"], 120_000,
        "the server values the percent line"
    );
    let mgr2 = user(&pool, f.org, "Manager 2", "branch_manager").await;
    assign(&pool, mgr2, f.a).await;
    let resp = call!(
        app,
        patch,
        format!(
            "/staff/adjustments/bonus/{}/decision",
            pct["id"].as_str().unwrap()
        ),
        token_for(mgr2, f.org, UserRole::BranchManager),
        json!({ "approve": true })
    );
    assert_eq!(
        resp.status(),
        403,
        "a percent line is judged at its value, not skipped"
    );
    let resp = call!(
        app,
        patch,
        format!(
            "/staff/adjustments/bonus/{}/decision",
            pct["id"].as_str().unwrap()
        ),
        owner,
        json!({ "approve": true })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(json_of(resp).await["status"], "approved");
    // 1.5% of 600,000 = 9,000; 0.75% of 600,001 = 4500.0075 → 4500.
    let small = json_of(call!(
        app,
        post,
        "/staff/adjustments",
        owner,
        json!({ "employee_id": f.amal, "kind": "bonus", "percent_of_base": 1.5, "reason": "Tip" })
    ))
    .await;
    assert_eq!(small["value_piastres"], 9_000);
    // The retired legacy creates are gone.
    for path in ["/staff/payroll/bonuses", "/staff/payroll/deductions"] {
        let resp = call!(
            app,
            post,
            path,
            owner,
            json!({ "employee_id": f.amal, "amount_piastres": 1, "reason": "x", "effective_date": f.start })
        );
        assert_eq!(resp.status(), 404, "{path}");
    }
}

/// E2E B-PAY-1: a bonus percent outside 1–100 is told the range, not that "a
/// deduction is an amount" (AD-1); a deduction with a percent still is (AD-2).
#[sqlx::test]
async fn a_bonus_percent_out_of_range_is_told_the_range(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    for p in [json!(0), json!(101), json!(-5)] {
        let resp = call!(
            app,
            post,
            "/staff/adjustments",
            f.owner(),
            json!({ "employee_id": f.amal, "kind": "bonus", "percent_of_base": p,
                    "reason": "E2E", "effective_date": f.start })
        );
        assert_eq!(resp.status(), 400, "{p}");
        let text = text_of(resp).await;
        assert!(text.contains("percentage (1–100)"), "{p}: {text}");
        assert!(!text.contains("deduction"), "{p}: {text}");
    }
    let resp = call!(
        app,
        post,
        "/staff/adjustments",
        f.owner(),
        json!({ "employee_id": f.amal, "kind": "deduction", "percent_of_base": 5,
                "reason": "E2E", "effective_date": f.start })
    );
    assert_eq!(resp.status(), 400);
    assert!(
        text_of(resp)
            .await
            .contains("A deduction is an amount, not a percentage")
    );
    let n: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM payroll_bonuses WHERE org_id = $1) \
              + (SELECT COUNT(*) FROM payroll_deductions WHERE org_id = $1)",
    )
    .bind(f.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 0, "nothing was written");
}

// ── the one pricing function (AT-9, RU-2, RU-6, RU-8, B2, PAY-13) ──────────

#[sqlx::test]
async fn overtime_is_valued_at_the_night_rate_under_the_branch_rules(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner();
    // Branch B overrides: 30 working days and a 2.0 day rate, night 1.7.
    sqlx::query(
        "INSERT INTO attendance_settings (org_id, branch_id, rules_saved_at, working_days_per_month, \
             overtime_mode, overtime_day_multiplier, overtime_night_multiplier, late_deduction_tiers) \
         VALUES ($1, $2, now(), 30, 'approval', 2.0, 1.7, $3::jsonb)",
    )
    .bind(f.org)
    .bind(f.b)
    .bind(LADDER)
    .execute(&pool)
    .await
    .unwrap();
    // Amal (A, business rules): a 14:00–22:00 shift, 30 min past the end,
    // all inside the night window → 600000×30×1.7/12480 = 2451.9 → 2452.
    let d1 = f.start + Duration::days(1);
    let rec_a = day(&pool, &f, f.amal, f.a, d1, "present", 14, 480, 0, 30).await;
    // Bassem (B): 09:00–17:00, 60 min of day overtime, approval mode →
    // nothing until approved, then 500000×60×2.0/(30×480) = 4166.67 → 4167.
    let rec_b = day(&pool, &f, f.bassem, f.b, d1, "present", 9, 480, 0, 60).await;

    let slip = slip_of(&app, &f, f.bassem).await;
    assert_eq!(
        slip["overtime_piastres"], 0,
        "approval mode: pending pays nothing"
    );
    let resp = call!(
        app,
        patch,
        format!("/staff/attendance/{rec_b}/overtime"),
        owner,
        json!({ "approve": true })
    );
    assert_eq!(resp.status(), 200);
    let slip = slip_of(&app, &f, f.bassem).await;
    assert_eq!(
        slip["overtime_piastres"], 4_167,
        "the BRANCH's days and rate (RU-2)"
    );
    assert_eq!(slip["overtime_minutes"], 60);
    let slip = slip_of(&app, &f, f.amal).await;
    assert_eq!(
        slip["overtime_piastres"], 2_452,
        "night minutes at the night rate (RU-8)"
    );
    assert_eq!(slip["breakdown"]["night_overtime_minutes"], 30);
    assert_eq!(slip["breakdown"]["overtime_shifts"][0]["piastres"], 2_452);
    // A shift template's own rate wins over the branch's (RU-8): 2.5 by day.
    let tpl: Uuid = sqlx::query_scalar(
        "INSERT INTO work_shifts (org_id, branch_id, name, start_time, end_time, ot_day_multiplier) \
         VALUES ($1, $2, 'Special', '09:00', '17:00', 2.5) RETURNING id",
    )
    .bind(f.org)
    .bind(f.a)
    .fetch_one(&pool)
    .await
    .unwrap();
    let d2 = f.start + Duration::days(2);
    let rec_c = day(&pool, &f, f.amal, f.a, d2, "present", 9, 480, 0, 60).await;
    sqlx::query("UPDATE attendance_records SET work_shift_id = $2 WHERE id = $1")
        .bind(rec_c)
        .bind(tpl)
        .execute(&pool)
        .await
        .unwrap();
    // 600000×60×2.5/12480 = 7211.5 → 7212, plus the 2452 night one.
    let slip = slip_of(&app, &f, f.amal).await;
    assert_eq!(slip["overtime_piastres"], 2_452 + 7_212);
    // The approval's own valuation used the night rate too: it went through
    // the manager's overtime limit as 2452 (well under 100000). A record in
    // an approved month can no longer be decided.
    let _ = rec_a;
    assert_eq!(generate(&app, &f).await.status(), 200);
    let rec_d = day(&pool, &f, f.bassem, f.b, f.start, "present", 9, 480, 0, 15).await;
    let resp = call!(
        app,
        patch,
        format!("/staff/attendance/{rec_d}/overtime"),
        owner,
        json!({ "approve": true })
    );
    assert_eq!(
        resp.status(),
        409,
        "overtime in an approved month is decided"
    );
}

#[sqlx::test]
async fn late_penalties_use_the_branch_rules_and_that_days_rostered_minutes(pool: PgPool) {
    let f = seed(&pool).await;
    // Branch B: 30 working days, and a ladder where 1–30 min costs 30 min.
    sqlx::query(
        "INSERT INTO attendance_settings (org_id, branch_id, rules_saved_at, working_days_per_month, \
             late_deduction_tiers) VALUES ($1, $2, now(), 30, \
             '[{\"from_minutes\":1,\"to_minutes\":30,\"kind\":\"minutes\",\"value\":30}]'::jsonb)",
    )
    .bind(f.org)
    .bind(f.b)
    .execute(&pool)
    .await
    .unwrap();
    // Bassem, 6-hour day, 10 min late → 30 min of pay at the 6-hour minute
    // rate: 500000×30/(30×360) = 1388.9 → 1389 (not the 8-hour 1042).
    let rec = day(&pool, &f, f.bassem, f.b, f.start, "late", 9, 360, 10, 0).await;
    // The caller passes the BUSINESS rules; the record's branch's apply.
    let org_settings = madar_rust::staff::attendance::load_settings(&pool, f.org, None)
        .await
        .unwrap();
    madar_rust::staff::penalties::recompute_record(&pool, rec, &org_settings)
        .await
        .unwrap();
    let amount: i64 = sqlx::query_scalar(
        "SELECT amount_piastres FROM payroll_deductions WHERE attendance_record_id = $1",
    )
    .bind(rec)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(amount, 1_389);
    // Amal, absent under the business rules: 600000/26 = 23076.9 → 23077.
    let rec = day(&pool, &f, f.amal, f.a, f.start, "absent", 9, 480, 0, 0).await;
    madar_rust::staff::penalties::recompute_record(&pool, rec, &org_settings)
        .await
        .unwrap();
    let (amount, source): (i64, String) = sqlx::query_as(
        "SELECT amount_piastres, source FROM payroll_deductions WHERE attendance_record_id = $1",
    )
    .bind(rec)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((amount, source.as_str()), (23_077, "absence"));
}

#[sqlx::test]
async fn a_joiner_mid_period_earns_overtime_at_the_full_rate(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    // Audit B2's worked example on this period: joined on day 16 of the
    // month; 600 minutes of day overtime.
    let window = (f.end - f.start).num_days() + 1;
    let hired = f.start + Duration::days(15);
    sqlx::query("UPDATE employees SET hire_date = $2 WHERE id = $1")
        .bind(f.amal)
        .bind(hired)
        .execute(&pool)
        .await
        .unwrap();
    // The history starts at the hire date too (the seed dated it a year back).
    sqlx::query("UPDATE employee_salary_history SET effective_from = $2 WHERE employee_id = $1")
        .bind(f.amal)
        .bind(hired)
        .execute(&pool)
        .await
        .unwrap();
    for i in 0..5 {
        day(
            &pool,
            &f,
            f.amal,
            f.a,
            hired + Duration::days(i),
            "present",
            9,
            480,
            0,
            120,
        )
        .await;
    }
    let slip = slip_of(&app, &f, f.amal).await;
    let paid_days = window - 15;
    // base = 600000 × paid_days / window, rounded once.
    let expected_base =
        ((600_000i128 * paid_days as i128 * 2 + window as i128) / (2 * window as i128)) as i64;
    assert_eq!(slip["base_piastres"], expected_base);
    assert_eq!(slip["breakdown"]["paid_days"], paid_days);
    // Each shift is priced once (AT-9): 120 min × 1.35 on the FULL salary =
    // 600000×120×1.35/12480 = 7788.46 → 7788, five times = 38940 (not the
    // prorated 20099 of audit B2, and not one rounding of the aggregate).
    assert_eq!(slip["overtime_piastres"], 5 * 7_788);
    assert_eq!(slip["net_piastres"], expected_base + 5 * 7_788);
}

#[sqlx::test]
async fn a_salary_change_mid_period_pays_each_day_at_its_rate(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner();
    // A raise to 660,000 from day 17 (day index 16), recorded by the
    // salary-history trigger when the salary changes…
    let raise_day = f.start + Duration::days(16);
    let resp = call!(
        app,
        put,
        format!("/staff/employees/{}", f.amal),
        owner,
        json!({ "name": "Amal", "base_salary_piastres": 660_000 })
    );
    assert_eq!(resp.status(), 200);
    // …dated today by the trigger; the test moves it to the raise day.
    sqlx::query(
        "UPDATE employee_salary_history SET effective_from = $2 \
          WHERE employee_id = $1 AND base_salary_piastres = 660000",
    )
    .bind(f.amal)
    .bind(raise_day)
    .execute(&pool)
    .await
    .unwrap();
    let window = (f.end - f.start).num_days() + 1;
    let slip = slip_of(&app, &f, f.amal).await;
    // (16 × 600000 + (window − 16) × 660000) / window, rounded once.
    let sum = 16i128 * 600_000 + (window as i128 - 16) * 660_000;
    let expected = ((sum * 2 + window as i128) / (2 * window as i128)) as i64;
    assert_eq!(slip["base_piastres"], expected);
    assert_eq!(
        slip["base_salary_piastres"], 660_000,
        "the salary in force at the end"
    );
    // A late day before the raise is priced at the OLD salary: 24 min → 2885.
    let rec = day(&pool, &f, f.amal, f.a, f.start, "late", 9, 480, 24, 0).await;
    let settings = madar_rust::staff::attendance::load_settings(&pool, f.org, None)
        .await
        .unwrap();
    madar_rust::staff::penalties::recompute_record(&pool, rec, &settings)
        .await
        .unwrap();
    let amount: i64 = sqlx::query_scalar(
        "SELECT amount_piastres FROM payroll_deductions WHERE attendance_record_id = $1",
    )
    .bind(rec)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(amount, 2_885);
    // And overtime after the raise at the NEW one: 60 min → 660000×60×1.35/12480 = 4283.65 → 4284.
    day(&pool, &f, f.amal, f.a, raise_day, "present", 9, 480, 0, 60).await;
    let slip = slip_of(&app, &f, f.amal).await;
    assert_eq!(slip["overtime_piastres"], 4_284);
}

// ── overrides and waivers (AD-7, AD-8, AT-7, B6) ───────────────────────────

#[sqlx::test]
async fn overrides_are_limited_zero_is_allowed_and_a_waiver_is_final(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner();
    let mgr = f.mgr();
    let rec = day(&pool, &f, f.amal, f.a, f.start, "absent", 9, 480, 0, 0).await;
    let settings = madar_rust::staff::attendance::load_settings(&pool, f.org, None)
        .await
        .unwrap();
    madar_rust::staff::penalties::recompute_record(&pool, rec, &settings)
        .await
        .unwrap();
    let id: Uuid =
        sqlx::query_scalar("SELECT id FROM payroll_deductions WHERE attendance_record_id = $1")
            .bind(rec)
            .fetch_one(&pool)
            .await
            .unwrap();
    // 23077 → lowering to 10000 is fine for the manager (a partial waiver).
    let resp = call!(
        app,
        patch,
        format!("/staff/payroll/deductions/{id}/override"),
        mgr,
        json!({ "amount_piastres": 10_000, "reason": "Half a day was covered" })
    );
    assert_eq!(resp.status(), 200);
    let row = json_of(resp).await;
    assert_eq!(row["original_amount_piastres"], 23_077);
    assert_eq!(row["amount_piastres"], 10_000);
    // Raising it by 150,000 is over the manager's 1,000 EGP deduction limit.
    let resp = call!(
        app,
        patch,
        format!("/staff/payroll/deductions/{id}/override"),
        mgr,
        json!({ "amount_piastres": 160_000, "reason": "Broke the machine" })
    );
    assert_eq!(resp.status(), 403);
    let resp = call!(
        app,
        patch,
        format!("/staff/payroll/deductions/{id}/override"),
        owner,
        json!({ "amount_piastres": 160_000, "reason": "Broke the machine" })
    );
    assert_eq!(resp.status(), 200, "the owner has no limit");
    // Zero is allowed (B6) and the original survives a second override.
    let resp = call!(
        app,
        patch,
        format!("/staff/payroll/deductions/{id}/override"),
        owner,
        json!({ "amount_piastres": 0, "reason": "Forgiven" })
    );
    assert_eq!(resp.status(), 200);
    let row = json_of(resp).await;
    assert_eq!(row["amount_piastres"], 0);
    assert_eq!(row["original_amount_piastres"], 23_077);
    // A recompute never brings it back (AD-8).
    madar_rust::staff::penalties::recompute_record(&pool, rec, &settings)
        .await
        .unwrap();
    let amount: i64 =
        sqlx::query_scalar("SELECT amount_piastres FROM payroll_deductions WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(amount, 0);
    // Waive, then no override; unwaive with a reason counts it again.
    let resp = call!(
        app,
        patch,
        format!("/staff/payroll/deductions/{id}/waive"),
        owner,
        json!({ "reason": "Sick" })
    );
    assert_eq!(resp.status(), 200);
    let resp = call!(
        app,
        patch,
        format!("/staff/payroll/deductions/{id}/override"),
        owner,
        json!({ "amount_piastres": 5, "reason": "x" })
    );
    assert_eq!(resp.status(), 409, "a waiver is final");
    let resp = call!(
        app,
        patch,
        format!("/staff/payroll/deductions/{id}/unwaive"),
        owner,
        json!({ "reason": "" })
    );
    assert_eq!(resp.status(), 400);
    let resp = call!(
        app,
        patch,
        format!("/staff/payroll/deductions/{id}/unwaive"),
        owner,
        json!({ "reason": "Wasn't sick after all" })
    );
    assert_eq!(resp.status(), 200);
    assert!(json_of(resp).await["waived_at"].is_null());
    // The employee's own list shows the override and waiver state (AD-6).
    let resp = call!(
        app,
        patch,
        format!("/staff/payroll/deductions/{id}/waive"),
        owner,
        json!({ "reason": "Sick" })
    );
    assert_eq!(resp.status(), 200);
    let mine = json_of(call!(
        app,
        get,
        "/staff/me/adjustments",
        phone_token(&pool, f.amal).await
    ))
    .await;
    let line = mine
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["id"] == json!(id))
        .unwrap();
    assert!(line["waived_at"].is_string());
    assert_eq!(line["original_amount_piastres"], 23_077);
    let actions = audit_actions(&pool, f.org).await;
    assert_eq!(
        actions
            .iter()
            .filter(|a| *a == "deduction.override")
            .count(),
        3
    );
    assert_eq!(
        actions.iter().filter(|a| *a == "deduction.waive").count(),
        2
    );
    assert_eq!(
        actions.iter().filter(|a| *a == "deduction.unwaive").count(),
        1
    );
    let log = json_of(call!(app, get, "/staff/payroll/audit", owner)).await;
    assert!(
        log.as_array()
            .unwrap()
            .iter()
            .any(|r| r["action"] == "deduction.unwaive" && r["reason"] == "Wasn't sick after all")
    );
    let resp = call!(app, get, "/staff/payroll/audit", mgr);
    assert_eq!(resp.status(), 403, "the log is the owner's");
}

// ── expense advances (AV-7, AV-10) ─────────────────────────────────────────

#[sqlx::test]
async fn expense_advances_carry_a_date_and_a_branch_and_never_a_till(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner();
    let resp = call!(
        app,
        post,
        "/staff/expense-advances",
        owner,
        json!({ "employee_id": f.amal, "amount_piastres": 20_000, "purpose": "Milk", "via": "till" })
    );
    assert_eq!(
        resp.status(),
        400,
        "a till pay-out is tagged on the till (AV-10)"
    );
    let given = f.start;
    let resp = call!(
        app,
        post,
        "/staff/expense-advances",
        owner,
        json!({ "employee_id": f.amal, "amount_piastres": 20_000, "purpose": "Milk", "via": "safe",
                "given_on": given, "branch_id": f.b })
    );
    assert_eq!(resp.status(), 201);
    let row = json_of(resp).await;
    assert_eq!(row["given_on"], json!(given));
    assert_eq!(row["branch_id"], json!(f.b));
    assert_eq!(row["handed_by"], json!(f.owner));
    // The manager of A cannot log at B.
    let resp = call!(
        app,
        post,
        "/staff/expense-advances",
        f.mgr(),
        json!({ "employee_id": f.amal, "amount_piastres": 1_000, "purpose": "Tea", "via": "bank", "branch_id": f.b })
    );
    assert_eq!(resp.status(), 403);
    let resp = call!(
        app,
        post,
        "/staff/expense-advances",
        owner,
        json!({ "employee_id": f.amal, "amount_piastres": 1_000, "purpose": "Tea", "via": "bank",
                "given_on": f.end + Duration::days(40) })
    );
    assert_eq!(resp.status(), 400, "not in the future");
    // Per branch in the report, and never on a payslip.
    let rep = json_of(call!(
        app,
        get,
        format!(
            "/staff/reports/advances?from={}&to={}&branch_id={}",
            f.start, f.end, f.b
        ),
        owner
    ))
    .await;
    assert_eq!(rep["expense_given_piastres"], 20_000);
    let slip = slip_of(&app, &f, f.amal).await;
    assert_eq!(slip["net_piastres"], 600_000);
}

// ── exports (PAY-8) ────────────────────────────────────────────────────────

#[sqlx::test]
async fn the_bank_file_and_wallet_list_come_from_the_server(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner();
    sqlx::query("UPDATE employees SET pay_method = 'bank', pay_account = 'EG12BANK' WHERE id = $1")
        .bind(f.amal)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE employees SET pay_method = 'wallet', pay_account = '01000000000' WHERE id = $1",
    )
    .bind(f.bassem)
    .execute(&pool)
    .await
    .unwrap();
    let resp = call!(
        app,
        get,
        format!("/staff/payroll/periods/{}/export.csv?method=bank", f.period),
        owner
    );
    assert_eq!(resp.status(), 409, "nothing to export before approval");
    assert_eq!(generate(&app, &f).await.status(), 200);
    let bank = text_of(call!(
        app,
        get,
        format!("/staff/payroll/periods/{}/export.csv?method=bank", f.period),
        owner
    ))
    .await;
    assert_eq!(
        bank,
        "employee,account,amount\n\"Amal\",\"EG12BANK\",6000.00\n"
    );
    let wallet = text_of(call!(
        app,
        get,
        format!(
            "/staff/payroll/periods/{}/export.csv?method=wallet",
            f.period
        ),
        owner
    ))
    .await;
    assert_eq!(
        wallet,
        "employee,wallet_number,amount\n\"Bassem\",\"01000000000\",5000.00\n"
    );
    let all = text_of(call!(
        app,
        get,
        format!("/staff/payroll/periods/{}/export.csv", f.period),
        owner
    ))
    .await;
    assert!(all.starts_with("employee,employee_id,pay_method,account,base,"));
    assert_eq!(all.lines().count(), 3);
    let resp = call!(
        app,
        get,
        format!(
            "/staff/payroll/periods/{}/export.csv?method=cheque",
            f.period
        ),
        owner
    );
    assert_eq!(resp.status(), 400);
    let slips = json_of(call!(
        app,
        get,
        format!("/staff/payroll/periods/{}/payslips", f.period),
        owner
    ))
    .await;
    assert_eq!(slips[0]["pay_method"], "bank");
    assert_eq!(slips[0]["pay_account"], "EG12BANK");
}

// ── the money path end to end (AT-12) ──────────────────────────────────────

fn stable_zone() -> String {
    // POSIX sign convention: `Etc/GMT-2` is two hours AHEAD of UTC. Pick the
    // zone where it is midday now, so a shift around now stays on one date.
    let shift = 12 - Utc::now().hour() as i64;
    format!(
        "Etc/GMT{}{}",
        if shift > 0 { '-' } else { '+' },
        shift.abs()
    )
}

fn local_time(minutes: i64) -> NaiveTime {
    let shift = 12 - Utc::now().hour() as i64;
    let local = (Utc::now() + Duration::hours(shift) + Duration::minutes(minutes)).naive_utc();
    NaiveTime::from_hms_opt(local.hour(), local.minute(), 0).unwrap()
}

#[sqlx::test]
async fn the_money_path_from_punch_to_payslip(pool: PgPool) {
    let app = app!(pool);
    let tz = stable_zone();
    let f = seed_with_zone(&pool, &tz).await;
    let owner = f.owner();
    // Amal's shift started 60 minutes ago with 15 minutes of grace: 45 late,
    // the half-day rung → 600000 × 0.5 / 26 = 11538.46 → 11538.
    let shift: Uuid = sqlx::query_scalar(
        "INSERT INTO work_shifts (org_id, branch_id, name, start_time, end_time, grace_minutes) \
         VALUES ($1, $2, 'Day', $3, $4, 15) RETURNING id",
    )
    .bind(f.org)
    .bind(f.a)
    .bind(local_time(-60))
    .bind(local_time(420))
    .fetch_one(&pool)
    .await
    .unwrap();
    // Rostered from TODAY (in the branch's zone): the sweep would otherwise
    // rightly mark yesterday's shift absent and price it.
    sqlx::query(
        "INSERT INTO staff_schedules (org_id, employee_id, work_shift_id, effective_from) \
         VALUES ($1, $2, $3, (now() AT TIME ZONE $4)::date)",
    )
    .bind(f.org)
    .bind(f.amal)
    .bind(shift)
    .bind(&tz)
    .execute(&pool)
    .await
    .unwrap();
    let phone = phone_token(&pool, f.amal).await;

    // 1. Punch in (late) and out.
    let resp = call!(
        app,
        post,
        "/staff/me/check-in",
        phone,
        json!({ "branch_id": f.a, "latitude": LAT, "longitude": LNG })
    );
    assert!(resp.status().is_success(), "{}", text_of(resp).await);
    let resp = call!(
        app,
        post,
        "/staff/me/check-out",
        phone,
        json!({ "latitude": LAT, "longitude": LNG })
    );
    assert_eq!(resp.status(), 200);

    // 2. The penalty row exists at the ladder's figure…
    let (id, amount, source): (Uuid, i64, String) = sqlx::query_as(
        "SELECT id, amount_piastres, source FROM payroll_deductions WHERE employee_id = $1 AND source <> 'absence'",
    )
    .bind(f.amal)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((amount, source.as_str()), (11_538, "late_penalty"));
    // …and the estimate already shows it.
    let est = json_of(call!(app, get, "/staff/me/pay/estimate", phone)).await;
    assert_eq!(est["slip"]["deductions_piastres"], 11_538);
    assert_eq!(est["slip"]["net_piastres"], 600_000 - 11_538);

    // 3. The nightly sweep changes nothing on this day (it may well mark
    //    yesterday's rostered shift absent — that is its job).
    madar_rust::staff::jobs::run_tick(&pool).await.unwrap();
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT source, amount_piastres FROM payroll_deductions \
          WHERE attendance_record_id = (SELECT attendance_record_id FROM payroll_deductions WHERE id = $1)",
    )
    .bind(id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows, vec![("late_penalty".to_string(), 11_538)]);

    // 4. The manager waives it; the sweep leaves the waiver alone.
    let resp = call!(
        app,
        patch,
        format!("/staff/payroll/deductions/{id}/waive"),
        f.mgr(),
        json!({ "reason": "Bus strike" })
    );
    assert_eq!(resp.status(), 200);
    madar_rust::staff::jobs::run_tick(&pool).await.unwrap();
    let waived: bool =
        sqlx::query_scalar("SELECT waived_at IS NOT NULL FROM payroll_deductions WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(waived);

    // 5. Approve: the payslip carries the struck-through line and full pay.
    let period = json_of(call!(app, get, "/staff/payroll/current", owner)).await["period"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let resp = call!(
        app,
        post,
        format!("/staff/payroll/periods/{period}/generate"),
        owner,
        json!({})
    );
    assert_eq!(resp.status(), 200);
    let mine = json_of(call!(app, get, "/staff/me/payslips", phone)).await;
    let slip = &mine[0];
    assert_eq!(slip["net_piastres"], 600_000);
    assert_eq!(slip["deductions_piastres"], 0);
    let lines = slip["breakdown"]["deductions"].as_array().unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["waived"], true);
    assert_eq!(lines[0]["piastres"], 11_538);
    let actions = audit_actions(&pool, f.org).await;
    assert!(actions.contains(&"deduction.waive".into()));
    assert!(actions.contains(&"period.generate".into()));
}

// ── periods (PAY-1, B8, AT-8) ──────────────────────────────────────────────

#[sqlx::test]
async fn overlapping_periods_are_refused_and_the_sweep_opens_the_month(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner();
    let resp = call!(
        app,
        post,
        "/staff/payroll/periods",
        owner,
        json!({ "name": "Overlap", "start_date": f.start + Duration::days(10), "end_date": f.end + Duration::days(10) })
    );
    assert_eq!(resp.status(), 409);
    // The database refuses it even without the handler.
    let raw = sqlx::query(
        "INSERT INTO payroll_periods (org_id, name, start_date, end_date) VALUES ($1, 'x', $2, $3)",
    )
    .bind(f.org)
    .bind(f.start - Duration::days(3))
    .bind(f.start)
    .execute(&pool)
    .await;
    assert!(raw.is_err());
    // A fresh org: the sweep opens its period on its start day (PAY-1), once.
    sqlx::query("DELETE FROM payroll_periods WHERE org_id = $1")
        .bind(f.org)
        .execute(&pool)
        .await
        .unwrap();
    madar_rust::staff::jobs::open_pay_periods(&pool)
        .await
        .unwrap();
    madar_rust::staff::jobs::open_pay_periods(&pool)
        .await
        .unwrap();
    let periods: Vec<(NaiveDate, NaiveDate)> =
        sqlx::query_as("SELECT start_date, end_date FROM payroll_periods WHERE org_id = $1")
            .bind(f.org)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(periods, vec![(f.start, f.end)]);
}

// ── recurring lines (AD-3, AD-9) ───────────────────────────────────────────

#[sqlx::test]
async fn stopping_a_recurring_line_records_who_and_why_and_ends_it_from_next_month(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner();
    // A meal allowance from a month AGO, and a transport one from THIS month.
    let last_month = f.start - Duration::days(1);
    let meal = json_of(call!(
        app,
        post,
        "/staff/adjustments",
        owner,
        json!({ "employee_id": f.amal, "kind": "bonus", "amount_piastres": 30_000, "reason": "Meals",
                "recurring": true, "effective_date": last_month })
    ))
    .await;
    let transport = json_of(call!(
        app,
        post,
        "/staff/adjustments",
        owner,
        json!({ "employee_id": f.amal, "kind": "bonus", "amount_piastres": 20_000, "reason": "Transport",
                "recurring": true, "effective_date": f.start })
    ))
    .await;
    let slip = slip_of(&app, &f, f.amal).await;
    assert_eq!(slip["bonuses_piastres"], 50_000);
    // A recurring line dated NEXT month is not in this one (AD-3 start month).
    let next = json_of(call!(
        app,
        post,
        "/staff/adjustments",
        owner,
        json!({ "employee_id": f.amal, "kind": "bonus", "amount_piastres": 1, "reason": "Future",
                "recurring": true, "effective_date": f.end + Duration::days(1) })
    ))
    .await;
    assert_eq!(next["status"], "approved");
    let slip = slip_of(&app, &f, f.amal).await;
    assert_eq!(slip["bonuses_piastres"], 50_000);

    // Owner decision D6 (24 Sep 2026): Stop = from next month. Stopping
    // while this month is open ends BOTH at the end of this month: the open
    // month keeps them (the screen says "Stopped from next month").
    for id in [
        meal["id"].as_str().unwrap(),
        transport["id"].as_str().unwrap(),
    ] {
        let resp = call!(
            app,
            post,
            format!("/staff/adjustments/bonus/{id}/stop"),
            owner,
            json!({ "reason": "Canteen opened" })
        );
        assert_eq!(resp.status(), 200);
        let row = json_of(resp).await;
        assert_eq!(row["ends_on"], json!(f.end));
        assert_eq!(row["stop_reason"], "Canteen opened");
        assert!(row["stopped_at"].is_string());
    }
    let slip = slip_of(&app, &f, f.amal).await;
    assert_eq!(slip["bonuses_piastres"], 50_000, "this month keeps them");
    // Next month doesn't: price the month after through the same engine.
    let next_start = f.end + Duration::days(1);
    let next_end = madar_rust::staff::dawam::pay::period_window(next_start, 1).1;
    let next_period: Uuid = sqlx::query_scalar(
        "INSERT INTO payroll_periods (org_id, name, start_date, end_date) \
         VALUES ($1, 'Next month', $2, $3) RETURNING id",
    )
    .bind(f.org)
    .bind(next_start)
    .bind(next_end)
    .fetch_one(&pool)
    .await
    .unwrap();
    let preview = json_of(call!(
        app,
        get,
        format!("/staff/payroll/periods/{next_period}/preview"),
        owner
    ))
    .await;
    let amal_next = preview
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["employee_id"] == json!(f.amal))
        .unwrap();
    assert_eq!(
        amal_next["bonuses_piastres"], 1,
        "only the future line runs on"
    );
    let details: Vec<Value> = sqlx::query_scalar(
        "SELECT details FROM payroll_audit_log WHERE org_id = $1 AND action = 'adjustment.stop'",
    )
    .bind(f.org)
    .fetch_all(&pool)
    .await
    .unwrap();
    for d in &details {
        assert_eq!(d["ends_on"], json!(f.end), "{d}");
        assert_eq!(d["rule"], "end_of_open_period", "{d}");
    }
    let (by, reason): (Option<Uuid>, Option<String>) =
        sqlx::query_as("SELECT stopped_by, stop_reason FROM payroll_bonuses WHERE id = $1::uuid")
            .bind(meal["id"].as_str().unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (by, reason.as_deref()),
        (Some(f.owner), Some("Canteen opened"))
    );
    assert_eq!(
        audit_actions(&pool, f.org)
            .await
            .iter()
            .filter(|a| *a == "adjustment.stop")
            .count(),
        2
    );
}

/// E2E B-PAY-2: stopping a monthly line records why (AD-9), like waive,
/// override, unwaive and reopen — no reason, nothing stops.
#[sqlx::test]
async fn stopping_a_monthly_line_needs_a_reason(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner();
    let line = json_of(call!(
        app,
        post,
        "/staff/adjustments",
        owner,
        json!({ "employee_id": f.amal, "kind": "bonus", "amount_piastres": 30_000, "reason": "Meals",
                "recurring": true, "effective_date": f.start })
    ))
    .await;
    let id = line["id"].as_str().unwrap();
    let uri = format!("/staff/adjustments/bonus/{id}/stop");
    for body in [
        json!({}),
        json!({ "reason": "   " }),
        json!({ "reason": null }),
    ] {
        let resp = call!(app, post, uri, owner, body);
        assert_eq!(resp.status(), 400, "{body}");
        assert!(
            text_of(resp)
                .await
                .contains("Stopping a monthly line needs a reason"),
            "{body}"
        );
    }
    let resp = test::call_service(
        &app,
        authed(test::TestRequest::post().uri(&uri), &owner).to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400, "no body at all");
    let (ends_on, stopped_at): (Option<NaiveDate>, Option<chrono::DateTime<Utc>>) =
        sqlx::query_as("SELECT ends_on, stopped_at FROM payroll_bonuses WHERE id = $1::uuid")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((ends_on, stopped_at), (None, None), "still running");
    assert!(
        !audit_actions(&pool, f.org)
            .await
            .contains(&"adjustment.stop".to_string())
    );

    let resp = call!(
        app,
        post,
        uri,
        owner,
        json!({ "reason": " Canteen opened " })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(json_of(resp).await["stop_reason"], "Canteen opened");
    let reason: Option<String> = sqlx::query_scalar(
        "SELECT reason FROM payroll_audit_log WHERE org_id = $1 AND action = 'adjustment.stop'",
    )
    .bind(f.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reason.as_deref(), Some("Canteen opened"));
}

// ── carry-over (PAY-12) ────────────────────────────────────────────────────

#[sqlx::test]
async fn deductions_past_earnings_carry_to_the_next_month_and_are_named(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner();
    // 700,000 of manual deductions against 600,000 of pay, plus a 50,000
    // advance installment that cannot be collected this month.
    let adv = approved_advance(&pool, &f, f.amal, 50_000, 1).await;
    for amount in [400_000, 300_000] {
        let resp = call!(
            app,
            post,
            "/staff/adjustments",
            owner,
            json!({ "employee_id": f.amal, "kind": "deduction", "amount_piastres": amount, "reason": "Damage",
                    "effective_date": f.start })
        );
        assert_eq!(resp.status(), 201);
    }
    let slip = slip_of(&app, &f, f.amal).await;
    assert_eq!(slip["deductions_piastres"], 600_000, "capped at earnings");
    assert_eq!(slip["net_piastres"], 0);
    assert_eq!(slip["carry_out_piastres"], 100_000);
    assert_eq!(
        slip["breakdown"]["capped_piastres"], 100_000,
        "the lines do not add up to the net; this says by how much"
    );
    assert_eq!(
        slip["advance_installment_piastres"], 0,
        "nothing left for the advance (AV-4)"
    );
    assert_eq!(generate(&app, &f).await.status(), 200);
    assert_eq!(
        remaining(&pool, adv).await.0,
        50_000,
        "the advance stays owed"
    );
    // Next month carries it in as its first deduction.
    let next_start = f.end + Duration::days(1);
    let next_end = (next_start + Duration::days(32)).with_day(1).unwrap() - Duration::days(1);
    let next: Uuid = sqlx::query_scalar(
        "INSERT INTO payroll_periods (org_id, name, start_date, end_date) VALUES ($1, 'Next', $2, $3) RETURNING id",
    )
    .bind(f.org)
    .bind(next_start)
    .bind(next_end)
    .fetch_one(&pool)
    .await
    .unwrap();
    let preview = json_of(call!(
        app,
        get,
        format!("/staff/payroll/periods/{next}/preview"),
        owner
    ))
    .await;
    let amal = preview
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["employee_id"] == json!(f.amal))
        .unwrap();
    let carry = amal["breakdown"]["deductions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["kind"] == "carry")
        .unwrap();
    assert_eq!(carry["piastres"], 100_000);
    // 600000 − 100000 carry − 50000 advance.
    assert_eq!(amal["net_piastres"], 450_000);
    assert_eq!(amal["advance_installment_piastres"], 50_000);
    // A reopened month leaks no carry: reopen this month, and next month's
    // preview reads none.
    assert_eq!(reopen(&app, &f).await.status(), 200);
    let preview = json_of(call!(
        app,
        get,
        format!("/staff/payroll/periods/{next}/preview"),
        owner
    ))
    .await;
    let amal = preview
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["employee_id"] == json!(f.amal))
        .unwrap();
    assert!(
        amal["breakdown"]["deductions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|l| l["kind"] != "carry")
    );
}

// ── refusals (AT-11) ───────────────────────────────────────────────────────

#[sqlx::test]
async fn every_new_money_act_refuses_the_wrong_person(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner();
    let mgr = f.mgr();
    let teller = user(&pool, f.org, "Teller", "teller").await;
    assign(&pool, teller, f.a).await;
    let teller = token_for(teller, f.org, UserRole::Teller);
    let rec = day(&pool, &f, f.bassem, f.b, f.start, "absent", 9, 480, 0, 0).await;
    let settings = madar_rust::staff::attendance::load_settings(&pool, f.org, None)
        .await
        .unwrap();
    madar_rust::staff::penalties::recompute_record(&pool, rec, &settings)
        .await
        .unwrap();
    let ded: Uuid =
        sqlx::query_scalar("SELECT id FROM payroll_deductions WHERE attendance_record_id = $1")
            .bind(rec)
            .fetch_one(&pool)
            .await
            .unwrap();
    let manual = json_of(call!(
        app,
        post,
        "/staff/adjustments",
        owner,
        json!({ "employee_id": f.bassem, "kind": "deduction", "amount_piastres": 1_000, "reason": "x" })
    ))
    .await;
    let manual_id = manual["id"].as_str().unwrap();

    // Rights before the body (AT-11): a bad `kind` from someone with no pay
    // rights is 403, not a 400 that names the fields; from the owner it is 400.
    for (m, path) in [
        ("post", "/staff/adjustments".to_string()),
        ("patch", format!("/staff/adjustments/x/{ded}/decision")),
        ("post", format!("/staff/adjustments/x/{ded}/stop")),
    ] {
        let body = json!({ "employee_id": f.amal, "kind": "x", "amount_piastres": 1, "reason": "r", "approve": true });
        let resp = match m {
            "post" => call!(app, post, path, teller, body),
            _ => call!(app, patch, path, teller, body),
        };
        assert_eq!(resp.status(), 403, "teller {m} {path}");
    }
    let resp = call!(
        app,
        post,
        "/staff/adjustments",
        owner,
        json!({ "employee_id": f.amal, "kind": "x", "amount_piastres": 1, "reason": "r" })
    );
    assert_eq!(resp.status(), 400);
    // A teller holds none of it (AT-11: one refusal per gate).
    for (m, path, body) in [
        (
            "patch",
            format!("/staff/payroll/deductions/{ded}/unwaive"),
            json!({ "reason": "r" }),
        ),
        (
            "patch",
            format!("/staff/payroll/deductions/{ded}/waive"),
            json!({ "reason": "r" }),
        ),
        (
            "patch",
            format!("/staff/payroll/deductions/{ded}/override"),
            json!({ "amount_piastres": 1, "reason": "r" }),
        ),
        (
            "post",
            "/staff/advances/record".to_string(),
            json!({ "employee_id": f.amal, "amount_piastres": 1 }),
        ),
        ("get", "/staff/payroll/audit".to_string(), Value::Null),
        (
            "delete",
            format!("/staff/payroll/deductions/{manual_id}"),
            Value::Null,
        ),
        (
            "patch",
            format!("/staff/payroll/periods/{}/status", f.period),
            json!({ "status": "draft", "reason": "r" }),
        ),
    ] {
        let resp = match m {
            "patch" => call!(app, patch, path, teller, body),
            "post" => call!(app, post, path, teller, body),
            "delete" => call!(app, delete, path, teller),
            _ => call!(app, get, path, teller),
        };
        assert_eq!(resp.status(), 403, "teller {m} {path}");
    }
    // The manager of A: not for B's people, and never the org-wide acts.
    let resp = call!(
        app,
        delete,
        format!("/staff/payroll/deductions/{manual_id}"),
        mgr
    );
    assert_eq!(resp.status(), 403, "Bassem is at B");
    for (path, body) in [
        (
            format!("/staff/payroll/deductions/{ded}/unwaive"),
            json!({ "reason": "r" }),
        ),
        (
            format!("/staff/payroll/deductions/{ded}/waive"),
            json!({ "reason": "r" }),
        ),
        (
            format!("/staff/payroll/deductions/{ded}/override"),
            json!({ "amount_piastres": 1, "reason": "r" }),
        ),
    ] {
        let resp = call!(app, patch, path, mgr, body);
        assert_eq!(resp.status(), 403, "A's manager on B's line: {path}");
    }
    let resp = call!(
        app,
        patch,
        format!("/staff/payroll/periods/{}/status", f.period),
        mgr,
        json!({ "status": "draft", "reason": "r" })
    );
    assert_eq!(resp.status(), 403, "reopening is the whole business's");
    let resp = call!(
        app,
        delete,
        format!("/staff/payroll/periods/{}", f.period),
        mgr
    );
    assert_eq!(resp.status(), 403);
    let resp = call!(
        app,
        get,
        format!("/staff/payroll/periods/{}/export.csv?method=bank", f.period),
        mgr
    );
    assert_eq!(resp.status(), 403);
    // A rule-made line is never deleted, by anyone.
    let resp = call!(
        app,
        delete,
        format!("/staff/payroll/deductions/{ded}"),
        owner
    );
    assert_eq!(resp.status(), 409);
    // Nobody adds a pay line for themselves.
    let mgr_emp =
        common::employees::employee(&pool, f.org, "Mgr", Some(f.mgr), None, false, &[f.a], 100)
            .await;
    let resp = call!(
        app,
        post,
        "/staff/adjustments",
        mgr,
        json!({ "employee_id": mgr_emp, "kind": "bonus", "amount_piastres": 1, "reason": "me" })
    );
    assert_eq!(resp.status(), 403);
}

// ── PAY-13: a first salary applies from the start, a raise from today ───────

#[sqlx::test]
async fn a_first_salary_counts_from_hire_and_a_raise_from_today(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner();
    // Added with no salary, given one mid-month: the whole month is paid at
    // it (nothing was ever paid at 0), not the days since it was typed.
    let dina =
        common::employees::employee(&pool, f.org, "Dina", None, None, false, &[f.a], 0).await;
    // PUT replaces the profile: keep the hire date the fixture gave her.
    let hired: NaiveDate = sqlx::query_scalar("SELECT hire_date FROM employees WHERE id = $1")
        .bind(dina)
        .fetch_one(&pool)
        .await
        .unwrap();
    let resp = call!(
        app,
        put,
        format!("/staff/employees/{dina}"),
        owner,
        json!({ "name": "Dina", "hire_date": hired, "base_salary_piastres": 400_000 })
    );
    assert_eq!(resp.status(), 200, "{}", text_of(resp).await);
    let slip = slip_of(&app, &f, dina).await;
    assert_eq!(slip["base_piastres"], 400_000, "{slip}");
    // A raise is dated today: earlier days keep the old figure.
    let resp = call!(
        app,
        put,
        format!("/staff/employees/{dina}"),
        owner,
        json!({ "name": "Dina", "hire_date": hired, "base_salary_piastres": 500_000 })
    );
    assert_eq!(resp.status(), 200);
    let rows: Vec<(NaiveDate, i64)> = sqlx::query_as(
        "SELECT effective_from, base_salary_piastres FROM employee_salary_history \
          WHERE employee_id = $1 ORDER BY effective_from",
    )
    .bind(dina)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].1, 400_000);
    assert_eq!(rows[1].1, 500_000);
    let slip = slip_of(&app, &f, dina).await;
    let base = slip["base_piastres"].as_i64().unwrap();
    assert!(base >= 400_000 && base <= 500_000, "{slip}");
}

// ── AT-9: one day, one price, every path (orchestrator decision #3) ─────────

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

/// Like [`day`], for a shift ON THE ROSTER (`work_shift_id` set).
#[allow(clippy::too_many_arguments)]
async fn rostered_day(
    pool: &PgPool,
    f: &F,
    emp: Uuid,
    branch: Uuid,
    shift_id: Uuid,
    date: NaiveDate,
    status: &str,
    start_hour: u32,
    sched_minutes: i64,
    late: i32,
    ot: i32,
) -> Uuid {
    let id = day(
        pool,
        f,
        emp,
        branch,
        date,
        status,
        start_hour,
        sched_minutes,
        late,
        ot,
    )
    .await;
    sqlx::query("UPDATE attendance_records SET work_shift_id = $2 WHERE id = $1")
        .bind(id)
        .bind(shift_id)
        .execute(pool)
        .await
        .unwrap();
    id
}

/// The day's deduction lines: the sweep's rows keyed to the record, and a
/// manager's own lines for that person and day (a flag's deduction is not
/// keyed to the record — the flag keeps the link, so a second flag on the
/// same shift can still be deducted and the sweep never touches it).
async fn rows_of(pool: &PgPool, rec: Uuid) -> Vec<(String, i64)> {
    sqlx::query_as(
        "SELECT d.source, d.amount_piastres FROM payroll_deductions d \
           JOIN attendance_records r ON r.id = $1 \
          WHERE d.attendance_record_id = r.id \
             OR (d.attendance_record_id IS NULL AND d.employee_id = r.employee_id \
                 AND d.effective_date = r.business_date AND d.source <> 'manual') \
          ORDER BY d.source",
    )
    .bind(rec)
    .fetch_all(pool)
    .await
    .unwrap()
}

fn deduction_lines(slip: &Value) -> Vec<(String, i64)> {
    let mut v: Vec<(String, i64)> = slip["breakdown"]["deductions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["source"].as_str().unwrap_or("?").to_string(),
                l["piastres"].as_i64().unwrap(),
            )
        })
        .collect();
    v.sort();
    v
}

/// A split day (09–13 late and with overtime, 17–21 missed) priced by hand,
/// by `price_shift` on the facts `load_facts` gathers, by the sweep's rows,
/// by the live estimate, by the flag's suggestion / unpaid excuse, by the
/// overtime approval and by the approved payslip: the same piastres in
/// every place.
#[sqlx::test]
async fn one_day_is_priced_identically_by_every_path(pool: PgPool) {
    use madar_rust::staff::pricing::{self, ShiftFacts};
    use madar_rust::staff::rules::AttendanceStatus;
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner();
    let morning = shift(&pool, f.org, f.a, "Morning", "09:00", "13:00").await;
    let evening = shift(&pool, f.org, f.a, "Evening", "17:00", "21:00").await;
    roster(&pool, &f, f.amal, &[morning, evening]).await;
    let d = f.start;
    // 24 minutes late for the morning, 30 minutes past its end; the evening missed.
    let am = rostered_day(&pool, &f, f.amal, f.a, morning, d, "late", 9, 240, 24, 30).await;
    let pm = rostered_day(&pool, &f, f.amal, f.a, evening, d, "absent", 17, 240, 0, 0).await;

    // By hand (600,000 pt, 26 days, the DAY is 480 minutes — RU-5/RU-6):
    //   late 24 min → rung 16–30 → 60 min of pay = 600000×60/12480 = 2884.6 → 2885
    //   overtime 30 min at 1.35 = 600000×30×1.35/12480 = 1947.1 → 1947
    //   the missed evening = half the day = 600000×(240/480)/26 = 11538.46 → 11538
    const LATE: i64 = 2_885;
    const OVERTIME: i64 = 1_947;
    const HALF_ABSENCE: i64 = 11_538;

    // 1. The pure function on hand-built facts.
    let settings = madar_rust::staff::attendance::load_settings(&pool, f.org, Some(f.a))
        .await
        .unwrap();
    let rules = pricing::ShiftRules::from_settings(&settings, None, None);
    let am_facts = ShiftFacts {
        base_salary_piastres: 600_000,
        scheduled_minutes: 240,
        day_minutes: 480,
        status: AttendanceStatus::Late,
        leave_minutes: 0,
        leave_paid: true,
        unpaid_excused_minutes: 0,
        late_minutes: 24,
        worked_minutes: 246,
        overtime_minutes: 30,
        night_overtime_minutes: 0,
        overtime_status: Some("pending".into()),
        is_confirmed_cover: false,
        is_other_cover: false,
        holiday: false,
    };
    let am_price = pricing::price_shift(&am_facts, &rules);
    assert_eq!(
        (
            am_price.late_penalty_piastres,
            am_price.overtime_piastres,
            am_price.absence_piastres
        ),
        (LATE, OVERTIME, 0)
    );
    let pm_price = pricing::price_shift(
        &ShiftFacts {
            status: AttendanceStatus::Absent,
            late_minutes: 0,
            worked_minutes: 0,
            overtime_minutes: 0,
            overtime_status: None,
            ..am_facts.clone()
        },
        &rules,
    );
    assert_eq!(pm_price.absence_piastres, HALF_ABSENCE);

    // 2. The facts the sweep gathers from the record and the roster are those.
    let loaded = madar_rust::staff::penalties::load_facts(&pool, am, &settings)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.facts.day_minutes, 480, "the roster's whole day");
    assert_eq!(loaded.facts.scheduled_minutes, 240);
    assert_eq!(loaded.facts.late_minutes, 24);
    assert_eq!(loaded.facts.overtime_minutes, 30);
    assert_eq!(loaded.facts.base_salary_piastres, 600_000);
    assert_eq!(loaded.price(), am_price);

    // 3. The sweep's rows.
    for rec in [am, pm] {
        madar_rust::staff::penalties::recompute_record(&pool, rec, &settings)
            .await
            .unwrap();
    }
    assert_eq!(
        rows_of(&pool, am).await,
        vec![("late_penalty".to_string(), LATE)]
    );
    assert_eq!(
        rows_of(&pool, pm).await,
        vec![("absence".to_string(), HALF_ABSENCE)]
    );

    // 4. The flag's suggestion for 30 minutes away is the same minute rate
    //    (600000×30/12480 = 1442.3 → nearest 5 EGP 1500), and resolving it as
    //    an unpaid excuse writes the exact figure under the ONE source name.
    let flag: Uuid = sqlx::query_scalar(
        "INSERT INTO attendance_flags (org_id, employee_id, branch_id, attendance_record_id, \
             kind, minutes_away) VALUES ($1, $2, $3, $4, 'left_mid_shift', 30) RETURNING id",
    )
    .bind(f.org)
    .bind(f.amal)
    .bind(f.a)
    .bind(am)
    .fetch_one(&pool)
    .await
    .unwrap();
    let flags = json_of(call!(
        app,
        get,
        format!("/staff/flags?branch_id={}", f.a),
        owner
    ))
    .await;
    let mine = flags
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == json!(flag))
        .cloned()
        .unwrap();
    assert_eq!(mine["suggested_deduction_piastres"], json!(1_500));
    let resp = call!(
        app,
        patch,
        format!("/staff/flags/{flag}"),
        owner,
        json!({ "action": "excuse_unpaid" })
    );
    assert_eq!(resp.status(), 200);
    const EXCUSED: i64 = 1_442;
    assert_eq!(
        rows_of(&pool, am).await,
        vec![
            ("excused_unpaid".to_string(), EXCUSED),
            ("late_penalty".to_string(), LATE)
        ],
        "decision #2: one source name for unpaid excused time"
    );
    // The sweep runs again (a check-out, a correction, the night): the
    // manager's line survives, the automatic ones are re-priced in place.
    madar_rust::staff::penalties::recompute_record(&pool, am, &settings)
        .await
        .unwrap();
    assert_eq!(
        rows_of(&pool, am).await,
        vec![
            ("excused_unpaid".to_string(), EXCUSED),
            ("late_penalty".to_string(), LATE)
        ]
    );

    // 5. The live estimate reads exactly those rows and prices the overtime
    //    with the same function.
    let phone = phone_token(&pool, f.amal).await;
    let est = json_of(call!(app, get, "/staff/me/pay/estimate", phone)).await;
    let slip = &est["slip"];
    assert_eq!(slip["overtime_piastres"], json!(OVERTIME), "{est}");
    assert_eq!(
        deduction_lines(slip),
        vec![
            ("absence".to_string(), HALF_ABSENCE),
            ("excused_unpaid".to_string(), EXCUSED),
            ("late_penalty".to_string(), LATE)
        ]
    );
    // E2E D-B2 (AT-13): the server's own wording comes with a stable code and
    // its figures, so each client says it in its language.
    let codes: Vec<(String, Value, Value)> = slip["breakdown"]["deductions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["source"].as_str().unwrap().to_string(),
                l["reason_code"].clone(),
                l["reason_vars"].clone(),
            )
        })
        .collect();
    let code_of = |src: &str| {
        codes
            .iter()
            .find(|(s, _, _)| s == src)
            .map(|(_, c, v)| (c.clone(), v.clone()))
            .unwrap()
    };
    assert_eq!(code_of("late_penalty").0, json!("late"));
    assert!(code_of("late_penalty").1["minutes"].as_i64().unwrap() > 0);
    assert_eq!(code_of("absence").0, json!("absent_no_punch"));
    assert_eq!(code_of("excused_unpaid").0, json!("unpaid_excuse"));
    let deductions = LATE + HALF_ABSENCE + EXCUSED;
    assert_eq!(slip["deductions_piastres"], json!(deductions));
    assert_eq!(slip["net_piastres"], json!(600_000 + OVERTIME - deductions));

    // 6. Under approval mode the overtime waits, and the approval prices it
    //    through the same facts; the estimate then shows the same figure.
    sqlx::query("UPDATE attendance_settings SET overtime_mode = 'approval' WHERE org_id = $1")
        .bind(f.org)
        .execute(&pool)
        .await
        .unwrap();
    let est = json_of(call!(app, get, "/staff/me/pay/estimate", phone)).await;
    assert_eq!(
        est["slip"]["overtime_piastres"],
        json!(0),
        "pending overtime is not paid"
    );
    let resp = call!(
        app,
        patch,
        format!("/staff/attendance/{am}/overtime"),
        owner,
        json!({ "approve": true })
    );
    assert_eq!(resp.status(), 200, "{}", text_of(resp).await);
    let est = json_of(call!(app, get, "/staff/me/pay/estimate", phone)).await;
    assert_eq!(est["slip"]["overtime_piastres"], json!(OVERTIME));

    // 7. The approved payslip is the estimate, frozen.
    let resp = generate(&app, &f).await;
    assert_eq!(resp.status(), 200, "{}", text_of(resp).await);
    let frozen = slip_of(&app, &f, f.amal).await;
    assert_eq!(frozen["overtime_piastres"], json!(OVERTIME));
    assert_eq!(frozen["deductions_piastres"], json!(deductions));
    assert_eq!(
        frozen["net_piastres"],
        json!(600_000 + OVERTIME - deductions)
    );
    assert_eq!(deduction_lines(&frozen), deduction_lines(&est["slip"]));
}

/// E2E B-ROTA-6 (SC-11): a split day with one block missed is ONE worked day
/// on the payslip, not a worked day and an absent day; missing the block
/// still costs its share. A date whose every block is missed is one absent
/// day, and a date with two worked blocks is one worked day.
#[sqlx::test]
async fn a_split_day_with_one_block_missed_counts_one_worked_day(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let morning = shift(&pool, f.org, f.a, "Morning", "09:00", "13:00").await;
    let evening = shift(&pool, f.org, f.a, "Evening", "17:00", "21:00").await;
    roster(&pool, &f, f.amal, &[morning, evening]).await;
    let d1 = f.start;
    let d2 = f.start + Duration::days(1);
    let d3 = f.start + Duration::days(2);
    // Day 1: the morning worked, the evening missed.
    rostered_day(&pool, &f, f.amal, f.a, morning, d1, "present", 9, 240, 0, 0).await;
    let pm = rostered_day(&pool, &f, f.amal, f.a, evening, d1, "absent", 17, 240, 0, 0).await;
    // Day 2: both blocks worked (one late).
    rostered_day(&pool, &f, f.amal, f.a, morning, d2, "late", 9, 240, 5, 0).await;
    rostered_day(
        &pool, &f, f.amal, f.a, evening, d2, "present", 17, 240, 0, 0,
    )
    .await;
    // Day 3: both blocks missed.
    rostered_day(&pool, &f, f.amal, f.a, morning, d3, "absent", 9, 240, 0, 0).await;
    rostered_day(&pool, &f, f.amal, f.a, evening, d3, "absent", 17, 240, 0, 0).await;
    // The sweep's absence line for the missed evening: half the day.
    sqlx::query(
        "INSERT INTO payroll_deductions (org_id, employee_id, amount_piastres, reason, \
             effective_date, source, status, attendance_record_id) \
         VALUES ($1, $2, 11538, 'Absent', $3, 'absence', 'approved', $4)",
    )
    .bind(f.org)
    .bind(f.amal)
    .bind(d1)
    .bind(pm)
    .execute(&pool)
    .await
    .unwrap();

    let slip = slip_of(&app, &f, f.amal).await;
    assert_eq!(slip["worked_days"].as_f64(), Some(2.0), "{slip}");
    assert_eq!(slip["absent_days"].as_f64(), Some(1.0), "{slip}");
    assert_eq!(
        slip["late_minutes"], 5,
        "late minutes still add up per block"
    );
    assert!(
        deduction_lines(&slip).contains(&("absence".to_string(), 11_538)),
        "the missed block still costs its share: {slip}"
    );
}

/// E2E B-SETUP-4 (AV-5, PM-1): an advance limit is a `max_percent` in basis
/// points like every other percent limit (the dashboard stores 30% as 3000).
/// A manager's override of 30% holds a 39% advance for someone higher and
/// lets a 25% one through; the role default is 50% (5000); the app is told
/// the limit in whole percent.
#[sqlx::test]
async fn an_advance_percent_limit_is_in_basis_points(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let mgr = f.mgr();
    // Out of the org cap's way: only the manager's own limit is judged here.
    sqlx::query("UPDATE attendance_settings SET advance_cap_percent = 100 WHERE org_id = $1")
        .bind(f.org)
        .execute(&pool)
        .await
        .unwrap();
    let role_default: Value = sqlx::query_scalar(
        "SELECT g.limits FROM org_role_grants g JOIN org_roles r ON r.id = g.org_role_id \
          WHERE r.org_id = $1 AND r.kind::text = 'branch_manager' AND g.capability_id = 228",
    )
    .bind(f.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(role_default, json!({ "max_percent": 5000 }), "50%, in bp");

    macro_rules! ask {
        ($who:expr, $amount:expr) => {{
            let s = phone_token(&pool, $who).await;
            let r = json_of(call!(app, post, "/staff/me/advances", s,
                json!({ "amount_piastres": $amount, "installments": 3 }))).await;
            r["id"].as_str().unwrap().to_string()
        }};
    }
    macro_rules! review {
        ($id:expr) => {
            call!(
                app,
                patch,
                format!("/staff/advances/{}/review", $id),
                mgr,
                json!({ "approve": true })
            )
        };
    }
    // The role default (50%) lets 45% through.
    let a45 = ask!(f.amal, 270_000);
    assert_eq!(review!(a45).status(), 200, "45% is within 50%");

    // The owner lowers this manager to 30% (the dashboard sends 3000).
    sqlx::query(
        "INSERT INTO user_overrides (org_id, user_id, capability_id, effect, limits, reason) \
         VALUES ($1, $2, 228, 'allow', '{\"max_percent\": 3000}'::jsonb, 'test')",
    )
    .bind(f.org)
    .bind(f.mgr)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("SELECT authz_bump_epoch($1)")
        .bind(f.org)
        .execute(&pool)
        .await
        .unwrap();
    let mgr_e = common::employees::employee(
        &pool,
        f.org,
        "Mgr",
        Some(f.mgr),
        Some("+201012345670"),
        true,
        &[f.a],
        0,
    )
    .await;
    let ctx = json_of(call!(
        app,
        get,
        "/staff/me/context",
        phone_token(&pool, mgr_e).await
    ))
    .await;
    assert_eq!(
        ctx["advance_limit_percent"], 30,
        "the app reads whole percent"
    );

    // Bassem earns 500,000: 195,000 is 39%, 125,000 is 25%.
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(f.mgr)
        .bind(f.b)
        .execute(&pool)
        .await
        .unwrap();
    let b39 = ask!(f.bassem, 195_000);
    let resp = review!(b39);
    assert_eq!(resp.status(), 403, "39% is over 30%");
    assert_eq!(
        remaining(&pool, Uuid::parse_str(&b39).unwrap()).await.1,
        "pending"
    );
    sqlx::query("DELETE FROM salary_advances WHERE id = $1::uuid")
        .bind(&b39)
        .execute(&pool)
        .await
        .unwrap();
    let b25 = ask!(f.bassem, 125_000);
    assert_eq!(review!(b25).status(), 200, "25% is within 30%");
}

/// E2E B-TEAM-6 (RU-7): overtime from a hand-entered record, a correction or
/// a manager's punch goes to approval in approval mode like a phone's
/// check-out — it used to stay with no status, never listed and never paid.
#[sqlx::test]
async fn overtime_from_every_path_waits_for_approval(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner();
    sqlx::query("UPDATE attendance_settings SET overtime_mode = 'approval' WHERE org_id = $1")
        .bind(f.org)
        .execute(&pool)
        .await
        .unwrap();
    let day_shift = shift(&pool, f.org, f.a, "Day", "09:00", "17:00").await;
    roster(&pool, &f, f.amal, &[day_shift]).await;
    let d = f.start;
    let at = |h: u32, m: u32| d.and_hms_opt(h, m, 0).unwrap().and_utc();
    let status = |id: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (i32, Option<String>)>(
                "SELECT overtime_minutes, overtime_status FROM attendance_records WHERE id = $1::uuid",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    // 1. Entered by hand, 45 minutes past the end.
    let resp = call!(
        app,
        post,
        "/staff/attendance",
        owner,
        json!({ "employee_id": f.amal, "branch_id": f.a, "business_date": d,
                "work_shift_id": day_shift, "check_in_at": at(9, 0),
                "check_out_at": at(17, 45), "reason": "Phone was dead" })
    );
    assert_eq!(resp.status(), 201);
    let manual = json_of(resp).await["id"].as_str().unwrap().to_string();
    let (ot, st) = status(manual.clone()).await;
    assert!(ot > 0, "{ot}");
    assert_eq!(st.as_deref(), Some("pending"), "a hand-entered record");
    let resp = call!(
        app,
        patch,
        format!("/staff/attendance/{manual}/overtime"),
        owner,
        json!({ "approve": true })
    );
    assert_eq!(resp.status(), 200, "it can be approved");

    // 2. A correction that adds overtime to a day that had none.
    let d2 = f.start + Duration::days(1);
    let resp = call!(
        app,
        post,
        "/staff/attendance",
        owner,
        json!({ "employee_id": f.amal, "branch_id": f.a, "business_date": d2,
                "work_shift_id": day_shift,
                "check_in_at": d2.and_hms_opt(9, 0, 0).unwrap().and_utc(),
                "check_out_at": d2.and_hms_opt(17, 0, 0).unwrap().and_utc(),
                "reason": "Phone was dead" })
    );
    assert_eq!(resp.status(), 201);
    let fixed = json_of(resp).await["id"].as_str().unwrap().to_string();
    assert_eq!(status(fixed.clone()).await, (0, None));
    let resp = call!(
        app,
        patch,
        format!("/staff/attendance/{fixed}"),
        owner,
        json!({ "check_out_at": d2.and_hms_opt(18, 0, 0).unwrap().and_utc(),
                "reason": "Stayed for the delivery" })
    );
    assert_eq!(resp.status(), 200);
    let (ot, st) = status(fixed).await;
    assert!(ot > 0, "{ot}");
    assert_eq!(st.as_deref(), Some("pending"), "a correction");
}

/// E2E B-PAY-3 (AV-9): expense advances are listed per branch — the
/// expense's own branch — and asking about a branch the caller can't read is
/// refused, not answered with everything.
#[sqlx::test]
async fn expense_advances_are_listed_per_branch(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let owner = f.owner();
    for (who, branch, purpose) in [
        (f.amal, f.a, "Milk"),
        (f.amal, f.b, "Cups"),
        (f.bassem, f.b, "Ice"),
    ] {
        let resp = call!(
            app,
            post,
            "/staff/expense-advances",
            owner,
            json!({ "employee_id": who, "amount_piastres": 10_000, "purpose": purpose, "via": "safe",
                    "given_on": f.start, "branch_id": branch })
        );
        assert_eq!(resp.status(), 201);
    }
    let purposes = |v: Value| {
        let mut p: Vec<String> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["purpose"].as_str().unwrap().to_string())
            .collect();
        p.sort();
        p
    };
    let all = json_of(call!(app, get, "/staff/expense-advances", owner)).await;
    assert_eq!(
        purposes(all),
        ["Cups", "Ice", "Milk"],
        "no param: unchanged"
    );
    let at_b = json_of(call!(
        app,
        get,
        format!("/staff/expense-advances?branch_id={}", f.b),
        owner
    ))
    .await;
    assert_eq!(
        purposes(at_b),
        ["Cups", "Ice"],
        "by the expense's own branch"
    );
    // A's manager: A's is answered, B's is refused.
    let resp = call!(
        app,
        get,
        format!("/staff/expense-advances?branch_id={}", f.b),
        f.mgr()
    );
    assert_eq!(resp.status(), 403);
    let resp = call!(
        app,
        get,
        format!("/staff/expense-advances?branch_id={}", f.a),
        f.mgr()
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(purposes(json_of(resp).await), ["Milk"]);
}

/// E2E B-TEAM-2 (AT-13): every money refusal carries a stable code (and its
/// figures), so the Arabic dashboard can word it instead of showing the
/// server's English with a "Forbidden:" / "Conflict:" prefix.
#[sqlx::test]
async fn money_refusals_carry_codes(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    async fn coded(resp: actix_web::dev::ServiceResponse, status: u16, code: &str) -> Value {
        assert_eq!(resp.status(), status, "{code}");
        let body: Value = test::read_body_json(resp).await;
        assert_eq!(body["code"], code, "{body}");
        let text = body["error"].as_str().unwrap();
        assert!(
            !text.starts_with("Forbidden:") && !text.starts_with("Conflict:"),
            "{body}"
        );
        body
    }
    let mgr_e = common::employees::employee(
        &pool,
        f.org,
        "Mgr",
        Some(f.mgr),
        Some("+201012345670"),
        true,
        &[f.a],
        600_000,
    )
    .await;
    // Your own pay line.
    coded(
        call!(
            app,
            post,
            "/staff/adjustments",
            f.mgr(),
            json!({ "employee_id": mgr_e, "kind": "bonus", "amount_piastres": 100, "reason": "x" })
        ),
        403,
        "OWN_PAY_LINE",
    )
    .await;
    // Over the advance cap (50% of 600,000 = 300,000).
    let s = phone_token(&pool, f.amal).await;
    let adv = json_of(call!(
        app,
        post,
        "/staff/me/advances",
        s,
        json!({ "amount_piastres": 400_000, "installments": 4 })
    ))
    .await;
    let body = coded(
        call!(
            app,
            patch,
            format!("/staff/advances/{}/review", adv["id"].as_str().unwrap()),
            f.mgr(),
            json!({ "approve": true })
        ),
        409,
        "ADVANCE_OVER_CAP",
    )
    .await;
    // A manager never learns the room left (D7).
    assert_eq!(body["vars"], json!({ "over_cap": true }), "{body}");
    // Your own advance.
    let mine = json_of(call!(
        app,
        post,
        "/staff/me/advances",
        phone_token(&pool, mgr_e).await,
        json!({ "amount_piastres": 10_000, "installments": 1 })
    ))
    .await;
    coded(
        call!(
            app,
            patch,
            format!("/staff/advances/{}/review", mine["id"].as_str().unwrap()),
            f.mgr(),
            json!({ "approve": true })
        ),
        403,
        "OWN_ADVANCE",
    )
    .await;
    // Above your limit too: a pending line another manager with the same
    // limit can't settle.
    let big = json_of(call!(app, post, "/staff/adjustments", f.mgr(),
        json!({ "employee_id": f.amal, "kind": "bonus", "amount_piastres": 150_000, "reason": "Big" }))).await;
    assert_eq!(big["status"], "pending", "{big}");
    let mgr2 = user(&pool, f.org, "Manager 2", "branch_manager").await;
    assign(&pool, mgr2, f.a).await;
    coded(
        call!(
            app,
            patch,
            format!(
                "/staff/adjustments/bonus/{}/decision",
                big["id"].as_str().unwrap()
            ),
            token_for(mgr2, f.org, UserRole::BranchManager),
            json!({ "approve": true })
        ),
        403,
        "ABOVE_LIMIT",
    )
    .await;
}

/// E2E B-PAY-4 (AT-13): the Bonuses & deductions list carries a rule-made
/// line's reason code and figures, as the payslip breakdown does, so it is
/// worded in Arabic too; a bonus or a manual line has none.
#[sqlx::test]
async fn adjustments_carry_the_rule_lines_reason_code(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let d = f.start;
    // An absence and a 24-minute late arrival, priced by the sweep's function.
    let absent = day(&pool, &f, f.amal, f.a, d, "absent", 9, 480, 0, 0).await;
    let late = day(
        &pool,
        &f,
        f.amal,
        f.a,
        d + Duration::days(1),
        "late",
        9,
        480,
        24,
        0,
    )
    .await;
    let settings = madar_rust::staff::attendance::load_settings(&pool, f.org, Some(f.a))
        .await
        .unwrap();
    for r in [absent, late] {
        madar_rust::staff::penalties::recompute_record(&pool, r, &settings)
            .await
            .unwrap();
    }
    let bonus = call!(
        app,
        post,
        "/staff/adjustments",
        f.owner(),
        json!({ "employee_id": f.amal, "kind": "bonus", "amount_piastres": 1_000, "reason": "Tips" })
    );
    assert_eq!(bonus.status(), 201);
    let rows = json_of(call!(
        app,
        get,
        format!("/staff/adjustments?employee_id={}", f.amal),
        f.owner()
    ))
    .await;
    let by = |source: &str| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|r| r["source"] == source)
            .cloned()
            .unwrap_or_else(|| panic!("no {source} in {rows}"))
    };
    let a = by("absence");
    assert_eq!(a["reason_code"], "absent_no_punch", "{a}");
    let l = by("late_penalty");
    assert_eq!(l["reason_code"], "late", "{l}");
    assert_eq!(l["reason_vars"]["minutes"], 24, "{l}");
    let b = by("manual");
    assert!(
        b["reason_code"].is_null() && b["reason_vars"].is_null(),
        "{b}"
    );
}

/// Mac E2E BB2: recording an advance over the cap is the same coded refusal
/// as approving one — ADVANCE_OVER_CAP, no "Conflict:" or code in the text.
/// A manager who may not read the salary hears only {over_cap: true}, never
/// the room left (owner decision D7); the text names no amount. (Amal:
/// 600,000, cap 50% = 300,000, 160,000 outstanding: 140,000 more at most.)
#[sqlx::test]
async fn recording_an_advance_over_the_cap_is_coded(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    approved_advance(&pool, &f, f.amal, 160_000, 4).await;
    let resp = call!(
        app,
        post,
        "/staff/advances/record",
        f.mgr(),
        json!({ "employee_id": f.amal, "amount_piastres": 200_000, "installments": 2 })
    );
    assert_eq!(resp.status(), 409);
    let body = json_of(resp).await;
    assert_eq!(body["code"], "ADVANCE_OVER_CAP", "{body}");
    assert_eq!(body["vars"], json!({ "over_cap": true }), "{body}");
    let text = body["error"].as_str().unwrap();
    assert!(
        !text.starts_with("Conflict:") && !text.contains("ADVANCE_OVER_CAP"),
        "{body}"
    );
    assert!(
        !text.contains("EGP") && !text.chars().any(|c| c.is_ascii_digit()),
        "no amount in the text: {text}"
    );
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM salary_advances WHERE employee_id = $1")
        .bind(f.amal)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "nothing recorded");
}

/// BB2 follow-up: a closed-month refusal's text is a sentence, not the code
/// again (the code is in `code`).
#[sqlx::test]
async fn a_closed_month_refusal_does_not_repeat_its_code_in_the_text(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    sqlx::query("UPDATE payroll_periods SET status = 'paid' WHERE id = $1")
        .bind(f.period)
        .execute(&pool)
        .await
        .unwrap();
    let resp = call!(
        app,
        post,
        "/staff/adjustments",
        f.owner(),
        json!({ "employee_id": f.amal, "kind": "bonus", "amount_piastres": 100,
                "reason": "x", "effective_date": f.start })
    );
    assert_eq!(resp.status(), 409);
    let body = json_of(resp).await;
    assert_eq!(body["code"], "PERIOD_CLOSED");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .starts_with("That month is paid"),
        "{body}"
    );
}

/// A confirmed cover of `minutes` at branch `branch` for `emp`, on `date`.
async fn confirmed_cover(
    pool: &PgPool,
    f: &F,
    emp: Uuid,
    covered: Uuid,
    branch: Uuid,
    date: NaiveDate,
    minutes: i64,
) -> Uuid {
    let start = date.and_hms_opt(9, 0, 0).unwrap().and_utc();
    let end = start + Duration::minutes(minutes);
    sqlx::query_scalar(
        "INSERT INTO attendance_records (org_id, employee_id, branch_id, business_date, status, \
             scheduled_start_at, scheduled_end_at, check_in_at, check_out_at, worked_minutes, \
             check_in_method, covered_employee_id, cover_status) \
         VALUES ($1, $2, $3, $4, 'present', $5, $6, $5, $6, $7, 'cover', $8, 'confirmed') \
         RETURNING id",
    )
    .bind(f.org)
    .bind(emp)
    .bind(branch)
    .bind(date)
    .bind(start)
    .bind(end)
    .bind(minutes as i32)
    .bind(covered)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Owner decision D5 (24 Sep 2026): how a cover is paid is a rule, set by
/// the business with a per-branch override. `minute_rate` (the default) is
/// the coverer's day rate over an 8-hour day × the minutes covered (CV-4);
/// `full_block` pays the covered block as a full day. The preview, the
/// approved payslip and its cover line all use the effective mode.
#[sqlx::test]
async fn a_cover_is_paid_by_the_cover_pay_mode_of_its_branch(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    // Amal covers 2.5 hours at branch A.
    confirmed_cover(&pool, &f, f.amal, f.bassem, f.a, f.start, 150).await;
    let cover_of = |slip: &Value| -> (i64, String) {
        let line = slip["breakdown"]["bonuses"]
            .as_array()
            .and_then(|l| l.iter().find(|b| b["kind"] == "cover"))
            .unwrap_or_else(|| panic!("{slip}"));
        (
            line["piastres"].as_i64().unwrap(),
            line["covers"][0]["mode"].as_str().unwrap().to_string(),
        )
    };
    // 600,000 × 150 ÷ (26 × 480) = 7,211.54 → 7,212 (a plain 2.5 hours).
    const MINUTE_RATE: i64 = 7_212;
    // 600,000 ÷ 26 = 23,076.92 → 23,077 (the block as a full day).
    const FULL_BLOCK: i64 = 23_077;

    let rules = json_of(call!(app, get, "/staff/attendance/settings", f.owner())).await;
    assert_eq!(
        rules["cover_pay_mode"], "minute_rate",
        "the default: {rules}"
    );
    let slip = slip_of(&app, &f, f.amal).await;
    assert_eq!(cover_of(&slip), (MINUTE_RATE, "minute_rate".into()));
    assert_eq!(slip["bonuses_piastres"], MINUTE_RATE);

    // The business pays covers as a full block.
    macro_rules! put {
        ($body:expr) => {
            call!(app, put, "/staff/attendance/settings", f.owner(), $body)
        };
    }
    let resp = put!(json!({ "cover_pay_mode": "full_block" }));
    assert_eq!(resp.status(), 200);
    assert_eq!(
        cover_of(&slip_of(&app, &f, f.amal).await),
        (FULL_BLOCK, "full_block".into())
    );

    // Branch A overrides it back to the minute rate.
    let resp = put!(json!({ "branch_id": f.a, "cover_pay_mode": "minute_rate" }));
    assert_eq!(resp.status(), 200);
    let a_rules = json_of(call!(
        app,
        get,
        format!("/staff/attendance/settings?branch_id={}", f.a),
        f.owner()
    ))
    .await;
    assert_eq!(a_rules["cover_pay_mode"], "minute_rate");
    assert!(
        a_rules["overridden"]
            .as_array()
            .unwrap()
            .contains(&json!("cover_pay_mode")),
        "{a_rules}"
    );
    let b_rules = json_of(call!(
        app,
        get,
        format!("/staff/attendance/settings?branch_id={}", f.b),
        f.owner()
    ))
    .await;
    assert_eq!(
        b_rules["cover_pay_mode"], "full_block",
        "B follows the business"
    );
    assert_eq!(
        cover_of(&slip_of(&app, &f, f.amal).await),
        (MINUTE_RATE, "minute_rate".into())
    );

    // The approved payslip keeps what the preview said.
    let resp = generate(&app, &f).await;
    assert_eq!(resp.status(), 200, "{}", text_of(resp).await);
    let slip = slip_of(&app, &f, f.amal).await;
    assert_eq!(cover_of(&slip), (MINUTE_RATE, "minute_rate".into()));
    let resp = reopen(&app, &f).await;
    assert_eq!(resp.status(), 200);

    // Back to the business's rule.
    let resp = put!(json!({ "branch_id": f.a, "inherit": ["cover_pay_mode"] }));
    assert_eq!(resp.status(), 200);
    assert_eq!(
        cover_of(&slip_of(&app, &f, f.amal).await),
        (FULL_BLOCK, "full_block".into())
    );

    // Only the two modes, and only with the rules right.
    let resp = put!(json!({ "cover_pay_mode": "per_hour" }));
    assert_eq!(resp.status(), 400);
    let body = json_of(resp).await;
    assert_eq!(body["code"], "SETTING_OUT_OF_RANGE");
    assert_eq!(body["vars"]["field"], "cover_pay_mode");
    let resp = call!(
        app,
        put,
        "/staff/attendance/settings",
        f.mgr(),
        json!({ "branch_id": f.a, "cover_pay_mode": "full_block" })
    );
    assert_eq!(resp.status(), 403);
}

/// Owner decision D7 (24 Sep 2026): the advance cap is half the salary, so a
/// manager who may not read salaries sees only "within cap" or "over cap"
/// on every advance summary, never the figure; the owner and the person
/// themselves see it.
#[sqlx::test]
async fn a_manager_sees_within_cap_never_the_cap(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    // Cap: 50% of 600,000 = 300,000; 200,000 owed.
    approved_advance(&pool, &f, f.amal, 200_000, 4).await;
    let list = |token: String| {
        let app = &app;
        async move {
            json_of(call!(
                app,
                get,
                format!("/staff/payroll/advances?employee_id={}", f.amal),
                token
            ))
            .await
        }
    };
    let rows = list(f.mgr()).await;
    let row = &rows[0];
    assert!(row["cap_piastres"].is_null(), "{row}");
    assert_eq!(row["within_cap"], true);
    assert_eq!(row["outstanding_piastres"], 200_000);
    let rows = list(f.owner()).await;
    assert_eq!(rows[0]["cap_piastres"], 300_000);
    assert_eq!(rows[0]["within_cap"], true);
    let mine = json_of(call!(
        app,
        get,
        "/staff/me/advances",
        phone_token(&pool, f.amal).await
    ))
    .await;
    assert_eq!(mine[0]["cap_piastres"], 300_000, "her own cap");
    // The profile: the cap hidden with the salary, within_cap shown.
    let emp = json_of(call!(
        app,
        get,
        format!("/staff/employees/{}", f.amal),
        f.mgr()
    ))
    .await;
    assert!(emp["advance_cap_piastres"].is_null(), "{emp}");
    assert_eq!(emp["advance_within_cap"], true, "{emp}");

    // Over the cap once the owner approves more: every summary says so.
    approved_advance(&pool, &f, f.amal, 150_000, 1).await;
    let rows = list(f.mgr()).await;
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .all(|r| r["within_cap"] == false && r["cap_piastres"].is_null()),
        "{rows}"
    );
    let emp = json_of(call!(
        app,
        get,
        format!("/staff/employees/{}", f.amal),
        f.mgr()
    ))
    .await;
    assert_eq!(emp["advance_within_cap"], false, "{emp}");
    // A new ask, reviewed by the manager: refused with no amount.
    let ask = json_of(call!(
        app,
        post,
        "/staff/me/advances",
        phone_token(&pool, f.amal).await,
        json!({ "amount_piastres": 10_000, "installments": 1 })
    ))
    .await;
    assert!(
        ask["cap_piastres"].is_number(),
        "the asker sees her cap: {ask}"
    );
    assert_eq!(ask["within_cap"], false);
    let resp = call!(
        app,
        patch,
        format!("/staff/advances/{}/review", ask["id"].as_str().unwrap()),
        f.mgr(),
        json!({ "approve": true })
    );
    assert_eq!(resp.status(), 409);
    let body = json_of(resp).await;
    assert_eq!(body["code"], "ADVANCE_OVER_CAP");
    assert_eq!(body["vars"], json!({ "over_cap": true }), "{body}");
    assert_eq!(
        body["error"],
        "That's over the advance cap. Only the owner can approve it."
    );
}

/// Hunt H2-B2: a second decision on a pay line, an advance or a shift's
/// overtime is 409 ALREADY_DECIDED with the status it already has, and the
/// person hears once — also when two decisions land at the same moment
/// (each UPDATE was guarded on `pending` but its row count was ignored, and
/// overtime's had no guard at all).
#[sqlx::test]
async fn a_second_decision_is_already_decided_and_told_once(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let (owner, mgr) = (f.owner(), f.mgr());
    let told = async |key: &str| -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM staff_notifications WHERE employee_id = $1 AND key = $2",
        )
        .bind(f.amal)
        .bind(key)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let decided = async |resp: actix_web::dev::ServiceResponse, status: &str| {
        assert_eq!(resp.status(), 409);
        let body = json_of(resp).await;
        assert_eq!(body["code"], "ALREADY_DECIDED", "{body}");
        assert_eq!(body["vars"]["status"], status, "{body}");
    };
    let decide = |uri: &str, approve: bool| {
        let req = authed(test::TestRequest::patch().uri(uri), &owner)
            .set_json(json!({ "approve": approve }))
            .to_request();
        test::call_service(&app, req)
    };

    // A 2,000 EGP bonus is over the manager's 1,000: it waits for the owner.
    let bonus = async || -> String {
        let row = json_of(call!(
            app,
            post,
            "/staff/adjustments",
            mgr,
            json!({ "employee_id": f.amal, "kind": "bonus", "amount_piastres": 200_000, "reason": "Target" })
        ))
        .await;
        assert_eq!(row["status"], "pending", "{row}");
        format!(
            "/staff/adjustments/bonus/{}/decision",
            row["id"].as_str().unwrap()
        )
    };
    let uri = bonus().await;
    assert_eq!(decide(&uri, true).await.status(), 200);
    decided(decide(&uri, false).await, "approved").await;
    assert_eq!(told("staff.n_bonus_added").await, 1);
    let uri = bonus().await;
    let (one, two) = futures::join!(decide(&uri, true), decide(&uri, true));
    let mut codes = [one.status().as_u16(), two.status().as_u16()];
    codes.sort();
    assert_eq!(codes, [200, 409], "one wins at once");
    assert_eq!(told("staff.n_bonus_added").await, 2, "told once for it");

    // An advance.
    let advance = async || -> String {
        let row = json_of(call!(
            app,
            post,
            "/staff/me/advances",
            phone_token(&pool, f.amal).await,
            json!({ "amount_piastres": 50_000, "installments": 2 })
        ))
        .await;
        assert_eq!(row["status"], "pending", "{row}");
        format!("/staff/advances/{}/review", row["id"].as_str().unwrap())
    };
    let uri = advance().await;
    assert_eq!(decide(&uri, false).await.status(), 200);
    decided(decide(&uri, true).await, "rejected").await;
    assert_eq!(told("staff.n_advance_rejected").await, 1);
    assert_eq!(told("staff.n_advance_approved").await, 0);
    let uri = advance().await;
    let (one, two) = futures::join!(decide(&uri, false), decide(&uri, false));
    let mut codes = [one.status().as_u16(), two.status().as_u16()];
    codes.sort();
    assert_eq!(codes, [200, 409], "one wins at once");
    assert_eq!(
        told("staff.n_advance_rejected").await,
        2,
        "told once for it"
    );

    // A shift's overtime.
    let overtime = async |on: NaiveDate| -> String {
        let rec = day(&pool, &f, f.amal, f.a, on, "present", 8, 480, 0, 60).await;
        format!("/staff/attendance/{rec}/overtime")
    };
    let uri = overtime(f.start).await;
    assert_eq!(decide(&uri, true).await.status(), 200);
    decided(decide(&uri, false).await, "approved").await;
    assert_eq!(told("staff.n_overtime_approved").await, 1);
    assert_eq!(told("staff.n_overtime_rejected").await, 0);
    let uri = overtime(f.start + Duration::days(1)).await;
    let (one, two) = futures::join!(decide(&uri, true), decide(&uri, true));
    let mut codes = [one.status().as_u16(), two.status().as_u16()];
    codes.sort();
    assert_eq!(codes, [200, 409], "one wins at once");
    assert_eq!(
        told("staff.n_overtime_approved").await,
        2,
        "told once for it"
    );
    // A day with no overtime has nothing to decide.
    let none = day(
        &pool,
        &f,
        f.amal,
        f.a,
        f.start + Duration::days(2),
        "present",
        8,
        480,
        0,
        0,
    )
    .await;
    let resp = decide(&format!("/staff/attendance/{none}/overtime"), true).await;
    assert_eq!(resp.status(), 404);
}

/// Hunt H2-B4 (AV-4): asking for an advance tells the people who may decide
/// it at the person's branch — as a leave request tells its deciders — and
/// never the asker. A manager's own ask reaches the owner.
#[sqlx::test]
async fn an_advance_ask_tells_its_deciders(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let emp = async |name: &str, user: Uuid, phone: &str, branches: &[Uuid]| -> Uuid {
        common::employees::employee(
            &pool,
            f.org,
            name,
            Some(user),
            Some(phone),
            true,
            branches,
            400_000,
        )
        .await
    };
    let mgr_emp = emp("Manager", f.mgr, "+201012345601", &[f.a]).await;
    let owner_emp = emp("Owner", f.owner, "+201012345602", &[]).await;
    let mgr_b = user(&pool, f.org, "Manager B", "branch_manager").await;
    assign(&pool, mgr_b, f.b).await;
    let mgr_b_emp = emp("Manager B", mgr_b, "+201012345603", &[f.b]).await;
    let told = async || -> Vec<(Uuid, Value)> {
        let rows: Vec<(Uuid, Value)> = sqlx::query_as(
            "SELECT employee_id, args FROM staff_notifications WHERE key = 'staff.n_request' \
              ORDER BY employee_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM staff_notifications")
            .execute(&pool)
            .await
            .unwrap();
        rows
    };
    let ask = async |who: Uuid| {
        let resp = call!(
            app,
            post,
            "/staff/me/advances",
            phone_token(&pool, who).await,
            json!({ "amount_piastres": 50_000, "installments": 2 })
        );
        assert_eq!(resp.status(), 201);
    };
    let today: NaiveDate = sqlx::query_scalar("SELECT (now() AT TIME ZONE 'UTC')::date")
        .fetch_one(&pool)
        .await
        .unwrap();

    ask(f.amal).await;
    let rows = told().await;
    let mut who: Vec<Uuid> = rows.iter().map(|r| r.0).collect();
    let mut want = vec![mgr_emp, owner_emp];
    who.sort();
    want.sort();
    assert_eq!(who, want, "A's deciders, not B's manager nor Amal");
    assert!(!who.contains(&mgr_b_emp) && !who.contains(&f.amal));
    assert_eq!(
        rows[0].1,
        json!({ "name": "Amal", "kind": "salary_advance", "date": today })
    );
    assert_eq!(
        madar_rust::push::render("staff.n_request", &rows[0].1, false).unwrap(),
        format!("Amal: new Salary advance request for {today}")
    );

    // The manager's own ask: someone else decides it.
    ask(mgr_emp).await;
    let who: Vec<Uuid> = told().await.into_iter().map(|r| r.0).collect();
    assert_eq!(who, vec![owner_emp]);
}

/// Hunt H2-B5: Approvals asks for every pending cover and overtime, however
/// old — `GET /staff/attendance` needed a date range, so the dashboard's
/// 35-day window lost older ones. `?cover_status=pending` and
/// `?overtime_status=pending` need no range; the manager's branches still
/// bound what they see; any other listing still needs its range.
#[sqlx::test]
async fn pending_covers_and_overtime_are_listed_without_a_range(pool: PgPool) {
    let app = app!(pool);
    let f = seed(&pool).await;
    let long_ago = f.start - Duration::days(70);
    let old_ot = day(&pool, &f, f.amal, f.a, long_ago, "present", 8, 480, 0, 45).await;
    let b_ot = day(&pool, &f, f.bassem, f.b, long_ago, "present", 8, 480, 0, 30).await;
    let done_ot = day(
        &pool,
        &f,
        f.amal,
        f.a,
        long_ago + Duration::days(1),
        "present",
        8,
        480,
        0,
        20,
    )
    .await;
    sqlx::query("UPDATE attendance_records SET overtime_status = 'approved' WHERE id = $1")
        .bind(done_ot)
        .execute(&pool)
        .await
        .unwrap();
    let cover = day(
        &pool,
        &f,
        f.amal,
        f.a,
        long_ago + Duration::days(2),
        "present",
        8,
        480,
        0,
        0,
    )
    .await;
    sqlx::query(
        "UPDATE attendance_records SET covered_employee_id = $2, cover_status = 'pending', \
                check_in_method = 'cover' WHERE id = $1",
    )
    .bind(cover)
    .bind(f.bassem)
    .execute(&pool)
    .await
    .unwrap();
    let ids = async |uri: &str, token: &str| -> Vec<String> {
        let resp = call!(app, get, uri, token);
        assert_eq!(resp.status(), 200, "{uri}");
        let mut ids: Vec<String> = json_of(resp)
            .await
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect();
        ids.sort();
        ids
    };
    let sorted = |mut v: Vec<String>| {
        v.sort();
        v
    };
    let (owner, mgr) = (f.owner(), f.mgr());
    assert_eq!(
        ids("/staff/attendance?overtime_status=pending", &owner).await,
        sorted(vec![old_ot.to_string(), b_ot.to_string()])
    );
    assert_eq!(
        ids("/staff/attendance?overtime_status=pending", &mgr).await,
        vec![old_ot.to_string()],
        "the manager's branch only"
    );
    assert_eq!(
        ids("/staff/attendance?cover_status=pending", &owner).await,
        vec![cover.to_string()]
    );
    // With a range, the filter narrows it.
    let uri = format!(
        "/staff/attendance?from={long_ago}&to={}&overtime_status=pending",
        long_ago + Duration::days(5)
    );
    assert_eq!(
        ids(&uri, &owner).await,
        sorted(vec![old_ot.to_string(), b_ot.to_string()])
    );
    // Anything else still needs its range.
    for uri in [
        "/staff/attendance",
        "/staff/attendance?overtime_status=approved",
        "/staff/attendance?cover_status=rejected",
    ] {
        let resp = call!(app, get, uri, owner);
        assert_eq!(resp.status(), 400, "{uri}");
        let body = json_of(resp).await;
        assert_eq!(body["code"], "RANGE_REQUIRED", "{uri} {body}");
    }
    let resp = call!(app, get, "/staff/attendance?overtime_status=soon", owner);
    assert_eq!(resp.status(), 400);
}
