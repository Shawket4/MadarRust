//! Pay (PAY-*, AD-*, AV-*): the running period, a live estimate for the
//! employee, marking payslips paid, bonuses and deductions under a manager's
//! limit, salary advances under the owner's cap, expense advances (a log only)
//! and the notification inbox.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::{notify, owners, user_name};
use crate::authz::{Cap, Decision, Request as AuthzRequest};
use crate::errors::{AppError, AppErrorResponse};
use crate::staff::access;
use crate::staff::attendance::{load_settings, today_in};
use crate::staff::payroll::{
    ComputedPayslip, PayrollPeriod, Payslip, SalaryAdvance, compute_payslips,
};
use crate::staff::principal::{Me, caller};

/// The pay window holding `day`: from `start_day` of one month to the day
/// before it in the next. `start_day` 1 is the calendar month.
pub fn period_window(day: NaiveDate, start_day: u32) -> (NaiveDate, NaiveDate) {
    let start_day = start_day.clamp(1, 28);
    let this = NaiveDate::from_ymd_opt(day.year(), day.month(), start_day).expect("day <= 28");
    let start = if day >= this {
        this
    } else {
        this.checked_sub_months(chrono::Months::new(1))
            .expect("in range")
    };
    let end = start
        .checked_add_months(chrono::Months::new(1))
        .expect("in range")
        - Duration::days(1);
    (start, end)
}

const PERIOD_COLS: &str = "id, org_id, name, start_date, end_date, status, employee_count, \
     total_net_piastres, generated_at, generated_by, paid_at, closed_at, created_at, updated_at";

async fn today_for_org(pool: &PgPool, org_id: Uuid) -> Result<NaiveDate, AppError> {
    let tz: Option<String> = sqlx::query_scalar(
        "SELECT timezone::text FROM branches WHERE org_id = $1 AND deleted_at IS NULL \
          ORDER BY created_at LIMIT 1",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    today_in(pool, tz.as_deref().unwrap_or("Africa/Cairo")).await
}

/// The period covering today, created when it doesn't exist yet (PAY-1).
pub(crate) async fn ensure_current_period(
    pool: &PgPool,
    org_id: Uuid,
) -> Result<PayrollPeriod, AppError> {
    let settings = load_settings(pool, org_id, None).await?;
    let today = today_for_org(pool, org_id).await?;
    let (start, end) = period_window(today, settings.period_start_day.max(1) as u32);
    if let Some(p) = sqlx::query_as::<_, PayrollPeriod>(&format!(
        "SELECT {PERIOD_COLS} FROM payroll_periods \
          WHERE org_id = $1 AND start_date <= $2 AND end_date >= $2 ORDER BY start_date DESC LIMIT 1"
    ))
    .bind(org_id)
    .bind(today)
    .fetch_optional(pool)
    .await?
    {
        return Ok(p);
    }
    Ok(sqlx::query_as::<_, PayrollPeriod>(&format!(
        "INSERT INTO payroll_periods (org_id, name, start_date, end_date) \
         VALUES ($1, $2, $3, $4) RETURNING {PERIOD_COLS}"
    ))
    .bind(org_id)
    .bind(format!(
        "{} – {}",
        start.format("%d %b"),
        end.format("%d %b %Y")
    ))
    .bind(start)
    .bind(end)
    .fetch_one(pool)
    .await?)
}

#[derive(Serialize, ToSchema)]
pub struct CurrentPayroll {
    pub period: PayrollPeriod,
    /// A live computation while the period is still a draft.
    pub preview: Vec<ComputedPayslip>,
    /// The frozen payslips once it has been generated.
    pub payslips: Vec<Payslip>,
    /// Earlier periods, newest first.
    pub history: Vec<PayrollPeriod>,
}

const PAYSLIP_SELECT: &str = r#"
    SELECT s.id, s.org_id, s.payroll_period_id, s.employee_id, e.name AS employee_name,
           s.base_salary_piastres, s.worked_days, s.absent_days, s.leave_days,
           s.late_minutes, s.overtime_minutes, s.overtime_piastres, s.bonuses_piastres,
           s.deductions_piastres, s.advance_installment_piastres, s.net_piastres,
           s.breakdown, s.generated_at, s.paid_method, s.paid_at, s.carry_out_piastres,
           pp.name AS period_name, pp.start_date AS period_start, pp.end_date AS period_end
      FROM payslips s
      JOIN employees e ON e.id = s.employee_id
      JOIN payroll_periods pp ON pp.id = s.payroll_period_id
"#;

/// The running period with everyone's pay (PAY-1..PAY-5).
#[utoipa::path(
    get, path = "/staff/payroll/current", tag = "staff",
    responses((status = 200, body = CurrentPayroll), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn current(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    // Everyone's pay for the whole business: payroll read for every branch.
    access::require_everywhere(pool.get_ref(), &claims, org_id, Cap::HrPayrollRead).await?;
    let period = ensure_current_period(pool.get_ref(), org_id).await?;
    let (preview, payslips) = if period.status == "draft" {
        let settings = load_settings(pool.get_ref(), org_id, None).await?;
        let mut conn = pool.acquire().await?;
        let p = compute_payslips(
            &mut conn,
            org_id,
            period.start_date,
            period.end_date,
            &settings,
        )
        .await?;
        (p, Vec::new())
    } else {
        let s = sqlx::query_as::<_, Payslip>(&format!(
            "{PAYSLIP_SELECT} WHERE s.payroll_period_id = $1 ORDER BY lower(e.name)"
        ))
        .bind(period.id)
        .fetch_all(pool.get_ref())
        .await?;
        (Vec::new(), s)
    };
    let history = sqlx::query_as::<_, PayrollPeriod>(&format!(
        "SELECT {PERIOD_COLS} FROM payroll_periods WHERE org_id = $1 AND start_date < $2 \
          ORDER BY start_date DESC LIMIT 24"
    ))
    .bind(org_id)
    .bind(period.start_date)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(CurrentPayroll {
        period,
        preview,
        payslips,
        history,
    }))
}

#[derive(Serialize, ToSchema)]
pub struct PayEstimate {
    pub period_start: NaiveDate,
    pub period_end: NaiveDate,
    /// So far this period, from the same engine payroll uses (PAY-11).
    pub slip: Option<ComputedPayslip>,
    /// How much more can be asked for as an advance (AV-5).
    pub advance_room_piastres: i64,
}

async fn advance_room(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
) -> Result<(i64, i64), AppError> {
    let settings = load_settings(pool, org_id, None).await?;
    let (salary, outstanding): (i64, i64) = sqlx::query_as(
        "SELECT p.base_salary_piastres, \
                COALESCE((SELECT SUM(remaining_piastres) FROM salary_advances a \
                           WHERE a.employee_id = p.id AND a.status IN ('pending', 'approved')), 0)::bigint \
           FROM employees p WHERE p.id = $1",
    )
    .bind(employee_id)
    .fetch_one(pool)
    .await?;
    let cap = (Decimal::from(salary) * settings.advance_cap_percent / Decimal::from(100))
        .floor()
        .to_i64()
        .unwrap_or(0);
    Ok(((cap - outstanding).max(0), salary))
}

/// What I've earned so far this period.
#[utoipa::path(
    get, path = "/staff/me/pay/estimate", tag = "staff",
    responses((status = 200, body = PayEstimate), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_estimate(me: Me, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let employee_id = me.employee_id;
    let org_id = me.org_id;
    let settings = load_settings(pool.get_ref(), org_id, None).await?;
    let today = today_for_org(pool.get_ref(), org_id).await?;
    let (start, end) = period_window(today, settings.period_start_day.max(1) as u32);
    let mut conn = pool.acquire().await?;
    let slip = compute_payslips(&mut conn, org_id, start, end, &settings)
        .await?
        .into_iter()
        .find(|s| s.employee_id == employee_id);
    drop(conn);
    let (room, _) = advance_room(pool.get_ref(), org_id, employee_id).await?;
    Ok(HttpResponse::Ok().json(PayEstimate {
        period_start: start,
        period_end: end,
        slip,
        advance_room_piastres: room,
    }))
}

#[derive(Deserialize, ToSchema)]
pub struct MarkPaid {
    /// `cash` · `bank` · `wallet`
    pub method: String,
}

/// Mark one payslip paid; the period is paid once everyone is (PAY-7).
#[utoipa::path(
    patch, path = "/staff/payroll/periods/{id}/payslips/{employee_id}/paid", tag = "staff",
    request_body = MarkPaid,
    params(("id" = Uuid, Path), ("employee_id" = Uuid, Path)),
    responses((status = 200, body = Payslip), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn mark_paid(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(Uuid, Uuid)>,
    body: web::Json<MarkPaid>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let (period_id, employee_id) = path.into_inner();
    // Paying is part of the payroll run, held for every branch (RO-9).
    access::require_everywhere(pool.get_ref(), &claims, org_id, Cap::HrPayrollRun).await?;
    if !matches!(body.method.as_str(), "cash" | "bank" | "wallet") {
        return Err(AppError::BadRequest(
            "method is cash, bank or wallet".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    let status: String = sqlx::query_scalar(
        "SELECT status FROM payroll_periods WHERE id = $1 AND org_id = $2 FOR UPDATE",
    )
    .bind(period_id)
    .bind(org_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound("Payroll period not found".into()))?;
    if status != "generated" && status != "paid" {
        return Err(AppError::Conflict(
            "Approve the payroll before paying it.".into(),
        ));
    }
    let n = sqlx::query(
        "UPDATE payslips SET paid_method = $3, paid_at = now() \
          WHERE payroll_period_id = $1 AND employee_id = $2 AND paid_at IS NULL",
    )
    .bind(period_id)
    .bind(employee_id)
    .bind(&body.method)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(AppError::Conflict(
            "That payslip is already paid or doesn't exist.".into(),
        ));
    }
    sqlx::query(
        "UPDATE payroll_periods SET status = 'paid', paid_at = now(), updated_at = now() \
          WHERE id = $1 AND status = 'generated' \
            AND NOT EXISTS (SELECT 1 FROM payslips WHERE payroll_period_id = $1 AND paid_at IS NULL)",
    )
    .bind(period_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    notify(
        pool.get_ref(),
        org_id,
        employee_id,
        "staff.n_paid",
        json!({ "method": body.method }),
    )
    .await;
    let row = sqlx::query_as::<_, Payslip>(&format!(
        "{PAYSLIP_SELECT} WHERE s.payroll_period_id = $1 AND s.employee_id = $2"
    ))
    .bind(period_id)
    .bind(employee_id)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(row))
}

// ── bonuses and deductions under a limit (AD-*) ────────────────────────────

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct Adjustment {
    pub id: Uuid,
    /// `bonus` · `deduction`
    pub kind: String,
    pub employee_id: Uuid,
    pub employee_name: String,
    pub amount_piastres: Option<i64>,
    pub percent_of_base: Option<Decimal>,
    pub reason: String,
    pub effective_date: NaiveDate,
    pub source: String,
    /// `pending` (waits for the owner) · `approved` · `rejected`
    pub status: String,
    pub recurring: bool,
    pub ends_on: Option<NaiveDate>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

const ADJ_SELECT: &str = "SELECT * FROM ( \
    SELECT a.id, 'bonus' AS kind, a.org_id, a.employee_id, e.name AS employee_name, a.amount_piastres, \
           a.percent_of_base, a.reason, a.effective_date, a.source, a.status, a.recurring, \
           a.ends_on, a.created_by, a.created_at \
      FROM payroll_bonuses a JOIN employees e ON e.id = a.employee_id \
    UNION ALL \
    SELECT a.id, 'deduction', a.org_id, a.employee_id, e.name, a.amount_piastres, a.percent_of_base, \
           a.reason, a.effective_date, a.source, a.status, a.recurring, a.ends_on, a.created_by, \
           a.created_at \
      FROM payroll_deductions a JOIN employees e ON e.id = a.employee_id \
    ) x";

async fn load_adjustment(pool: &PgPool, id: Uuid) -> Result<Adjustment, AppError> {
    sqlx::query_as::<_, Adjustment>(&format!("{ADJ_SELECT} WHERE x.id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::NotFound("Adjustment not found".into()))
}

fn table_of(kind: &str) -> Result<&'static str, AppError> {
    match kind {
        "bonus" => Ok("payroll_bonuses"),
        "deduction" => Ok("payroll_deductions"),
        _ => Err(AppError::BadRequest("kind is bonus or deduction".into())),
    }
}

#[derive(Deserialize, ToSchema)]
pub struct NewAdjustment {
    pub employee_id: Uuid,
    /// `bonus` · `deduction`
    pub kind: String,
    #[serde(default)]
    pub amount_piastres: Option<i64>,
    /// A bonus may be a % of salary.
    #[serde(default)]
    pub percent_of_base: Option<Decimal>,
    pub reason: String,
    #[serde(default)]
    pub effective_date: Option<NaiveDate>,
    /// Every month until stopped (AD-3).
    #[serde(default)]
    pub recurring: bool,
}

/// Add a bonus or deduction. Over the caller's limit it is created pending
/// and waits for the owner (AD-5).
#[utoipa::path(
    post, path = "/staff/adjustments", tag = "staff", request_body = NewAdjustment,
    responses((status = 201, body = Adjustment), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_adjustment(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<NewAdjustment>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let by = claims.user_id_safe()?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrAdjustmentsCreate).await?;
    let table = table_of(&body.kind)?;
    let subject = access::subject(pool, org_id, body.employee_id).await?;
    if subject.is(&claims) {
        return Err(AppError::Forbidden(
            "You can't add pay lines for yourself.".into(),
        ));
    }
    // A manager adds pay lines for the people of their branches (RO-6).
    access::require_for(pool, &claims, Cap::HrAdjustmentsCreate, &subject).await?;
    let reason = body.reason.trim();
    if reason.is_empty() {
        return Err(AppError::BadRequest("A reason is required".into()));
    }
    let salary: i64 =
        sqlx::query_scalar("SELECT base_salary_piastres FROM employees WHERE id = $1")
            .bind(body.employee_id)
            .fetch_optional(pool)
            .await?
            .unwrap_or(0);
    let amount = match (body.amount_piastres, body.percent_of_base) {
        (Some(a), None) if a > 0 => a,
        (None, Some(p)) if p > Decimal::ZERO && p <= Decimal::from(100) => {
            (Decimal::from(salary) * p / Decimal::from(100))
                .round()
                .to_i64()
                .unwrap_or(0)
        }
        _ => {
            return Err(AppError::BadRequest(
                "Give a positive amount or a percentage (1–100)".into(),
            ));
        }
    };
    let branch = access::decision_branch(pool, &claims, Cap::HrAdjustmentsCreate, &subject).await?;
    let mut ask = AuthzRequest::of(Cap::HrAdjustmentsCreate);
    ask.amount = Some(amount);
    let status = match crate::authz::require::decide_for(pool, by, &ask, branch).await? {
        Decision::Allow => "approved",
        Decision::NeedsApproval(_) => "pending",
        Decision::Deny(_) => return Err(crate::authz::require::denied(Cap::HrAdjustmentsCreate)),
    };
    let today = today_for_org(pool, org_id).await?;
    let id: Uuid = sqlx::query_scalar(&format!(
        "INSERT INTO {table} (org_id, employee_id, amount_piastres, percent_of_base, reason, \
            effective_date, source, status, created_by, recurring) \
         VALUES ($1, $2, $3, $4, $5, $6, 'manual', $7, $8, $9) RETURNING id"
    ))
    .bind(org_id)
    .bind(body.employee_id)
    .bind(body.percent_of_base.is_none().then_some(amount))
    .bind(body.percent_of_base)
    .bind(reason)
    .bind(body.effective_date.unwrap_or(today))
    .bind(status)
    .bind(by)
    .bind(body.recurring)
    .fetch_one(pool)
    .await?;
    let who = subject.name.clone();
    if status == "pending" {
        let by = user_name(pool, by).await;
        for o in owners(pool, org_id).await? {
            notify(
                pool,
                org_id,
                o,
                "staff.n_adjustment_pending",
                json!({ "name": who, "by": by, "amount": amount }),
            )
            .await;
        }
    } else {
        notify(
            pool,
            org_id,
            body.employee_id,
            if body.kind == "bonus" {
                "staff.n_bonus_added"
            } else {
                "staff.n_deduction_added"
            },
            json!({ "amount": amount, "reason": reason }),
        )
        .await;
    }
    Ok(HttpResponse::Created().json(load_adjustment(pool, id).await?))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AdjustmentList {
    #[serde(default)]
    pub employee_id: Option<Uuid>,
    #[serde(default)]
    pub status: Option<String>,
}

#[utoipa::path(
    get, path = "/staff/adjustments", tag = "staff", params(AdjustmentList),
    responses((status = 200, body = Vec<Adjustment>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_adjustments(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<AdjustmentList>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    // Payroll readers, and managers for the lines of their branches (B15).
    let scope = crate::staff::payroll::money_scope(
        pool.get_ref(),
        &claims,
        org_id,
        Cap::HrAdjustmentsCreate,
    )
    .await?;
    let rows = sqlx::query_as::<_, Adjustment>(&format!(
        "{ADJ_SELECT} WHERE x.org_id = $1 AND ($2::uuid IS NULL OR x.employee_id = $2) \
            AND ($3::text IS NULL OR x.status = $3) AND {} \
          ORDER BY x.created_at DESC LIMIT 300",
        access::in_scope("x.employee_id", 4)
    ))
    .bind(org_id)
    .bind(query.employee_id)
    .bind(query.status.as_deref())
    .bind(scope.as_deref())
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    get, path = "/staff/me/adjustments", tag = "staff",
    responses((status = 200, body = Vec<Adjustment>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_adjustments(me: Me, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let rows = sqlx::query_as::<_, Adjustment>(&format!(
        "{ADJ_SELECT} WHERE x.employee_id = $1 AND x.status = 'approved' \
          ORDER BY x.created_at DESC LIMIT 200"
    ))
    .bind(me.employee_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[derive(Deserialize, ToSchema)]
pub struct DecidePay {
    pub approve: bool,
}

/// The owner (or anyone whose limit covers it) decides a pending line.
#[utoipa::path(
    patch, path = "/staff/adjustments/{kind}/{id}/decision", tag = "staff", request_body = DecidePay,
    params(("kind" = String, Path), ("id" = Uuid, Path)),
    responses((status = 200, body = Adjustment), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn decide_adjustment(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(String, Uuid)>,
    body: web::Json<DecidePay>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let by = claims.user_id_safe()?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrAdjustmentsCreate).await?;
    let (kind, id) = path.into_inner();
    let table = table_of(&kind)?;
    let a = load_adjustment(pool, id).await?;
    if a.status != "pending" || a.kind != kind {
        return Err(AppError::Conflict(
            "This line has already been decided".into(),
        ));
    }
    let subject = access::subject(pool, org_id, a.employee_id).await?;
    access::require_for(pool, &claims, Cap::HrAdjustmentsCreate, &subject).await?;
    let branch = access::decision_branch(pool, &claims, Cap::HrAdjustmentsCreate, &subject).await?;
    let mut ask = AuthzRequest::of(Cap::HrAdjustmentsCreate);
    ask.amount = a.amount_piastres;
    let pending = crate::authz::Pending {
        request: ask,
        subject_id: subject.authz_key(),
        requested_by: a
            .created_by
            .map_or_else(|| subject.authz_key(), |u| u.to_string()),
        why: crate::authz::Why::NotHeld,
    };
    crate::authz::require::settle(pool, by, &pending, branch).await?;
    sqlx::query(&format!(
        "UPDATE {table} SET status = $3, decided_by = $4, decided_at = now(), updated_at = now() \
          WHERE id = $1 AND org_id = $2 AND status = 'pending'"
    ))
    .bind(id)
    .bind(org_id)
    .bind(if body.approve { "approved" } else { "rejected" })
    .bind(by)
    .execute(pool)
    .await?;
    if body.approve {
        notify(
            pool,
            org_id,
            a.employee_id,
            if kind == "bonus" {
                "staff.n_bonus_added"
            } else {
                "staff.n_deduction_added"
            },
            json!({ "amount": a.amount_piastres, "reason": a.reason }),
        )
        .await;
    }
    // The manager who added it hears back, in the app, through their employee.
    if let Some(creator) = a.created_by {
        let theirs: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM employees WHERE user_id = $1")
                .bind(creator)
                .fetch_optional(pool)
                .await?;
        if let Some(e) = theirs {
            notify(
                pool,
                org_id,
                e,
                if body.approve {
                    "staff.n_adjustment_approved"
                } else {
                    "staff.n_adjustment_rejected"
                },
                json!({ "name": a.employee_name }),
            )
            .await;
        }
    }
    Ok(HttpResponse::Ok().json(load_adjustment(pool, id).await?))
}

/// Stop a monthly line from the next period on; past payslips keep it (AD-3).
#[utoipa::path(
    post, path = "/staff/adjustments/{kind}/{id}/stop", tag = "staff",
    params(("kind" = String, Path), ("id" = Uuid, Path)),
    responses((status = 200, body = Adjustment), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn stop_adjustment(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(String, Uuid)>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrAdjustmentsCreate).await?;
    let (kind, id) = path.into_inner();
    let table = table_of(&kind)?;
    // For a line of someone the caller manages — never "anywhere" (audit B-3).
    let line = load_adjustment(pool, id).await?;
    if line.kind != kind {
        return Err(AppError::NotFound("Adjustment not found".into()));
    }
    let subject = access::subject(pool, org_id, line.employee_id).await?;
    access::require_for(pool, &claims, Cap::HrAdjustmentsCreate, &subject).await?;
    let period = ensure_current_period(pool, org_id).await?;
    let n = sqlx::query(&format!(
        "UPDATE {table} SET ends_on = $3, updated_at = now() \
          WHERE id = $1 AND org_id = $2 AND recurring AND ends_on IS NULL"
    ))
    .bind(id)
    .bind(org_id)
    .bind(period.start_date - Duration::days(1))
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(AppError::Conflict(
            "That isn't a running monthly line.".into(),
        ));
    }
    Ok(HttpResponse::Ok().json(load_adjustment(pool, id).await?))
}

// ── salary advances under the owner's cap (AV-*) ───────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct ReviewAdvance {
    pub approve: bool,
    /// Approve a different amount than asked.
    #[serde(default)]
    pub amount_piastres: Option<i64>,
    #[serde(default)]
    pub installments: Option<i32>,
    #[serde(default)]
    pub note: Option<String>,
}

/// Decide an advance within the cap: the approver's limit is a % of the
/// employee's salary (AV-4, AV-5).
#[utoipa::path(
    patch, path = "/staff/advances/{id}/review", tag = "staff", request_body = ReviewAdvance,
    params(("id" = Uuid, Path)),
    responses((status = 200, body = SalaryAdvance), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn review_advance(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<ReviewAdvance>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let by = claims.user_id_safe()?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrAdvancesDecide).await?;
    let (employee_id, asked, inst): (Uuid, i64, i32) = sqlx::query_as(
        "SELECT employee_id, amount_piastres, installments FROM salary_advances \
          WHERE id = $1 AND org_id = $2 AND status = 'pending'",
    )
    .bind(*id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::Conflict("This advance has already been decided".into()))?;
    let subject = access::subject(pool, org_id, employee_id).await?;
    if subject.is(&claims) {
        return Err(AppError::Forbidden(
            "Someone else has to decide your advance.".into(),
        ));
    }
    // Approve or reject: at one of the person's branches (audit B-3).
    access::require_for(pool, &claims, Cap::HrAdvancesDecide, &subject).await?;
    let amount = body.amount_piastres.unwrap_or(asked);
    let installments = body.installments.unwrap_or(inst);
    if amount <= 0 || !(1..=12).contains(&installments) {
        return Err(AppError::BadRequest(
            "Invalid amount or installments".into(),
        ));
    }
    if body.approve {
        let (room, salary) = advance_room(pool, org_id, employee_id).await?;
        // The one being decided is itself counted as outstanding.
        if amount > room + asked {
            return Err(AppError::Conflict(format!(
                "That's over the advance cap — at most {} EGP more.",
                (room + asked) / 100
            )));
        }
        let branch =
            access::decision_branch(pool, &claims, Cap::HrAdvancesDecide, &subject).await?;
        let mut ask = AuthzRequest::of(Cap::HrAdvancesDecide);
        ask.percent = Some(if salary > 0 {
            (amount * 100).div_euclid(salary)
        } else {
            100
        });
        let pending = crate::authz::Pending {
            request: ask,
            subject_id: subject.authz_key(),
            requested_by: subject.authz_key(),
            why: crate::authz::Why::NotHeld,
        };
        crate::authz::require::settle(pool, by, &pending, branch).await?;
    }
    let monthly = (amount as u64).div_ceil(installments as u64) as i64;
    sqlx::query(
        "UPDATE salary_advances SET status = $3, amount_piastres = $4, remaining_piastres = $4, \
            installments = $5, monthly_installment_piastres = $6, decided_by = $7, \
            decided_at = now(), decision_note = $8, updated_at = now() \
          WHERE id = $1 AND org_id = $2 AND status = 'pending'",
    )
    .bind(*id)
    .bind(org_id)
    .bind(if body.approve { "approved" } else { "rejected" })
    .bind(amount)
    .bind(installments)
    .bind(monthly)
    .bind(by)
    .bind(
        body.note
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty()),
    )
    .execute(pool)
    .await?;
    notify(
        pool,
        org_id,
        employee_id,
        if body.approve {
            "staff.n_advance_approved"
        } else {
            "staff.n_advance_rejected"
        },
        json!({ "amount": amount }),
    )
    .await;
    let row = sqlx::query_as::<_, SalaryAdvance>(
        "SELECT a.id, a.org_id, a.employee_id, e.name AS employee_name, a.amount_piastres, \
                a.installments, a.monthly_installment_piastres, a.remaining_piastres, \
                a.reason, a.status, a.decided_by, a.decided_at, a.decision_note, \
                a.created_at, a.updated_at \
           FROM salary_advances a JOIN employees e ON e.id = a.employee_id WHERE a.id = $1",
    )
    .bind(*id)
    .fetch_one(pool)
    .await?;
    Ok(HttpResponse::Ok().json(row))
}

// ── expense advances: a log, never deducted (AV-7, AV-8) ────────────────────

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct ExpenseAdvance {
    pub id: Uuid,
    pub employee_id: Uuid,
    pub employee_name: String,
    pub branch_id: Option<Uuid>,
    pub amount_piastres: i64,
    pub purpose: String,
    /// `safe` · `bank` · `till`
    pub via: String,
    pub handed_by: Option<Uuid>,
    pub handed_by_name: Option<String>,
    pub given_on: NaiveDate,
    pub created_at: DateTime<Utc>,
}

const EXP_SELECT: &str = "SELECT e.id, e.employee_id, p.name AS employee_name, e.branch_id, \
    e.amount_piastres, e.purpose, e.via, e.handed_by, h.name AS handed_by_name, e.given_on, \
    e.created_at FROM expense_advances e JOIN employees p ON p.id = e.employee_id \
    LEFT JOIN users h ON h.id = e.handed_by";

#[derive(Deserialize, ToSchema)]
pub struct NewExpenseAdvance {
    pub employee_id: Uuid,
    pub amount_piastres: i64,
    pub purpose: String,
    /// `safe` · `bank` · `till`
    pub via: String,
}

#[utoipa::path(
    post, path = "/staff/expense-advances", tag = "staff", request_body = NewExpenseAdvance,
    responses((status = 201, body = ExpenseAdvance), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn log_expense_advance(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<NewExpenseAdvance>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrExpenseAdvancesLog).await?;
    let subject = access::subject(pool, org_id, body.employee_id).await?;
    access::require_for(pool, &claims, Cap::HrExpenseAdvancesLog, &subject).await?;
    let branch =
        access::decision_branch(pool, &claims, Cap::HrExpenseAdvancesLog, &subject).await?;
    if !matches!(body.via.as_str(), "safe" | "bank" | "till") {
        return Err(AppError::BadRequest("via is safe, bank or till".into()));
    }
    let today = today_for_org(pool, org_id).await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO expense_advances (org_id, employee_id, branch_id, amount_piastres, purpose, via, \
            handed_by, given_on) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
    )
    .bind(org_id)
    .bind(body.employee_id)
    .bind(branch)
    .bind(body.amount_piastres)
    .bind(body.purpose.trim())
    .bind(&body.via)
    .bind(claims.user_id_safe().ok())
    .bind(today)
    .fetch_one(pool)
    .await?;
    let row = sqlx::query_as::<_, ExpenseAdvance>(&format!("{EXP_SELECT} WHERE e.id = $1"))
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(HttpResponse::Created().json(row))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ExpenseQuery {
    #[serde(default)]
    pub employee_id: Option<Uuid>,
}

#[utoipa::path(
    get, path = "/staff/expense-advances", tag = "staff", params(ExpenseQuery),
    responses((status = 200, body = Vec<ExpenseAdvance>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_expense_advances(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<ExpenseQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    // Payroll readers, and whoever logs them, for their branches (B15).
    let scope = crate::staff::payroll::money_scope(
        pool.get_ref(),
        &claims,
        org_id,
        Cap::HrExpenseAdvancesLog,
    )
    .await?;
    let rows = sqlx::query_as::<_, ExpenseAdvance>(&format!(
        "{EXP_SELECT} WHERE e.org_id = $1 AND ($2::uuid IS NULL OR e.employee_id = $2) AND {} \
          ORDER BY e.given_on DESC, e.created_at DESC LIMIT 300",
        access::in_scope("e.employee_id", 3)
    ))
    .bind(org_id)
    .bind(query.employee_id)
    .bind(scope.as_deref())
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    get, path = "/staff/me/expense-advances", tag = "staff",
    responses((status = 200, body = Vec<ExpenseAdvance>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_expense_advances(me: Me, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let rows = sqlx::query_as::<_, ExpenseAdvance>(&format!(
        "{EXP_SELECT} WHERE e.employee_id = $1 ORDER BY e.given_on DESC LIMIT 200"
    ))
    .bind(me.employee_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

// ── the inbox ───────────────────────────────────────────────────────────────

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct StaffNotification {
    pub id: Uuid,
    /// A core i18n key (`staff.n_*`); the app renders it with `args`.
    pub key: String,
    pub args: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub read_at: Option<DateTime<Utc>>,
}

#[utoipa::path(
    get, path = "/staff/me/notifications", tag = "staff",
    responses((status = 200, body = Vec<StaffNotification>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_notifications(me: Me, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let rows = sqlx::query_as::<_, StaffNotification>(
        "SELECT id, key, args, created_at, read_at FROM staff_notifications \
          WHERE employee_id = $1 ORDER BY created_at DESC LIMIT 100",
    )
    .bind(me.employee_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[derive(Deserialize, ToSchema)]
pub struct ReadNotifications {
    /// Empty = all of them.
    #[serde(default)]
    pub ids: Vec<Uuid>,
}

#[utoipa::path(
    post, path = "/staff/me/notifications/read", tag = "staff", request_body = ReadNotifications,
    responses((status = 204), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn read_notifications(
    me: Me,
    pool: crate::db::Db,
    body: web::Json<ReadNotifications>,
) -> Result<HttpResponse, AppError> {
    sqlx::query(
        "UPDATE staff_notifications SET read_at = now() \
          WHERE employee_id = $1 AND read_at IS NULL AND (cardinality($2::uuid[]) = 0 OR id = ANY($2))",
    )
    .bind(me.employee_id)
    .bind(&body.ids)
    .execute(pool.get_ref())
    .await?;
    Ok(HttpResponse::NoContent().finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_follow_the_start_day() {
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        assert_eq!(
            period_window(d(2026, 9, 22), 26),
            (d(2026, 8, 26), d(2026, 9, 25))
        );
        assert_eq!(
            period_window(d(2026, 9, 26), 26),
            (d(2026, 9, 26), d(2026, 10, 25))
        );
        assert_eq!(
            period_window(d(2026, 1, 3), 26),
            (d(2025, 12, 26), d(2026, 1, 25))
        );
        assert_eq!(
            period_window(d(2026, 2, 14), 1),
            (d(2026, 2, 1), d(2026, 2, 28))
        );
    }
}
