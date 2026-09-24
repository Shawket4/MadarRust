//! Pay (PAY-*, AD-*, AV-*): the running period, a live estimate for the
//! employee, marking payslips paid, bonuses and deductions under a manager's
//! limit, salary advances under the owner's cap, expense advances (a log only)
//! and the notification inbox.
//!
//! Every act dated inside an approved month is refused (`period_lock`,
//! AD-10): fixes go into the next open month as new lines.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use rust_decimal::Decimal;
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
    ComputedPayslip, PAYSLIP_SELECT, PERIOD_COLS, PayrollPeriod, PayrollTotals, Payslip,
    SalaryAdvance, audit, compute_payslips, create_cap, installment_of, load_advance,
    settle_period_if_all_paid,
};
use crate::staff::period_lock;
use crate::staff::pricing;
use crate::staff::principal::{Me, caller};

/// The pay window holding `day`: from `start_day` of one month to the day
/// before it in the next. `start_day` 1 is the calendar month.
///
/// The rule is madar-shared's (`madar_dawam::pay::period_window`), the staff
/// app's too.
pub fn period_window(day: NaiveDate, start_day: u32) -> (NaiveDate, NaiveDate) {
    madar_dawam::pay::period_window(day, i64::from(start_day))
}

/// The org's "today": its first branch's zone (the business's own clock).
pub(crate) async fn today_for_org(pool: &PgPool, org_id: Uuid) -> Result<NaiveDate, AppError> {
    let tz: Option<String> = sqlx::query_scalar(
        "SELECT timezone::text FROM branches WHERE org_id = $1 AND deleted_at IS NULL \
          ORDER BY created_at LIMIT 1",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    today_in(pool, tz.as_deref().unwrap_or("Africa/Cairo")).await
}

/// "Today" where a money act happens (AT-1): that branch's own day, else
/// the org's. Never the database server's date.
pub(crate) async fn today_at(
    pool: &PgPool,
    org_id: Uuid,
    branch: Option<Uuid>,
) -> Result<NaiveDate, AppError> {
    if let Some(b) = branch {
        let tz: Option<String> =
            sqlx::query_scalar("SELECT timezone::text FROM branches WHERE id = $1 AND org_id = $2")
                .bind(b)
                .bind(org_id)
                .fetch_optional(pool)
                .await?;
        if let Some(tz) = tz {
            return today_in(pool, &tz).await;
        }
    }
    today_for_org(pool, org_id).await
}

/// An employee's own day (AT-1): their first branch's zone.
pub(crate) async fn today_for_employee(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
) -> Result<NaiveDate, AppError> {
    let branch = crate::staff::access::branches_of(pool, employee_id)
        .await?
        .into_iter()
        .next();
    today_at(pool, org_id, branch).await
}

/// The period covering today, created when it doesn't exist yet (PAY-1). Two
/// callers racing to open the same month both get the one row (AT-8).
pub(crate) async fn ensure_current_period(
    pool: &PgPool,
    org_id: Uuid,
) -> Result<PayrollPeriod, AppError> {
    let settings = load_settings(pool, org_id, None).await?;
    let today = today_for_org(pool, org_id).await?;
    ensure_period_for(pool, org_id, today, settings.period_start_day.max(1) as u32).await
}

pub(crate) async fn ensure_period_for(
    pool: &PgPool,
    org_id: Uuid,
    day: NaiveDate,
    start_day: u32,
) -> Result<PayrollPeriod, AppError> {
    let (start, end) = period_window(day, start_day);
    let existing = || async {
        sqlx::query_as::<_, PayrollPeriod>(&format!(
            "SELECT {PERIOD_COLS} FROM payroll_periods \
              WHERE org_id = $1 AND start_date <= $2 AND end_date >= $2 ORDER BY start_date DESC LIMIT 1"
        ))
        .bind(org_id)
        .bind(day)
        .fetch_optional(pool)
        .await
    };
    if let Some(p) = existing().await? {
        return Ok(p);
    }
    // ON CONFLICT with no target covers the unique span AND the no-overlap
    // exclusion: a concurrent opener wins, and we read theirs.
    let inserted = sqlx::query_as::<_, PayrollPeriod>(&format!(
        "INSERT INTO payroll_periods (org_id, name, start_date, end_date) \
         VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING RETURNING {PERIOD_COLS}"
    ))
    .bind(org_id)
    .bind(format!(
        "{} – {}",
        start.format("%d %b"),
        end.format("%d %b %Y")
    ))
    .bind(start)
    .bind(end)
    .fetch_optional(pool)
    .await?;
    match inserted {
        Some(p) => Ok(p),
        None => existing()
            .await?
            .ok_or_else(|| AppError::Conflict("The period could not be opened".into())),
    }
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
    /// The run added up by the server (AT-3).
    pub totals: PayrollTotals,
    /// How many payslips are marked paid (a 'none' mark counts).
    pub paid_count: i64,
}

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
            None,
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
    let totals = if payslips.is_empty() {
        PayrollTotals::of_computed(&preview)
    } else {
        PayrollTotals::of_payslips(&payslips)
    };
    let paid_count = payslips.iter().filter(|s| s.paid_at.is_some()).count() as i64;
    Ok(HttpResponse::Ok().json(CurrentPayroll {
        period,
        preview,
        payslips,
        history,
        totals,
        paid_count,
    }))
}

#[derive(Serialize, ToSchema)]
pub struct PayEstimate {
    pub period_start: NaiveDate,
    pub period_end: NaiveDate,
    /// So far this period, from the same engine payroll uses (PAY-9). Null
    /// for someone not on payroll.
    pub slip: Option<ComputedPayslip>,
    /// How much more can be asked for as an advance (AV-5).
    pub advance_room_piastres: i64,
    /// The owner's cap on what this person may owe (AV-5), server-computed.
    pub advance_cap_piastres: i64,
    pub advance_outstanding_piastres: i64,
    pub on_payroll: bool,
}

/// (room, cap, outstanding, salary) for one person's advances (AV-5).
async fn advance_room(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
) -> Result<(i64, i64, i64, i64), AppError> {
    let (salary, outstanding, cap): (i64, i64, i64) = sqlx::query_as(
        "SELECT p.base_salary_piastres, \
                COALESCE((SELECT SUM(remaining_piastres) FROM salary_advances a \
                           WHERE a.employee_id = p.id AND a.status IN ('pending', 'approved')), 0)::bigint, \
                dawam_advance_cap(p.org_id, p.base_salary_piastres) \
           FROM employees p WHERE p.id = $1 AND p.org_id = $2",
    )
    .bind(employee_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Employee not found".into()))?;
    Ok(((cap - outstanding).max(0), cap, outstanding, salary))
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
    let today = today_for_employee(pool.get_ref(), org_id, employee_id).await?;
    let (start, end) = period_window(today, settings.period_start_day.max(1) as u32);
    let on_payroll: bool =
        sqlx::query_scalar("SELECT on_payroll FROM employees WHERE id = $1 AND org_id = $2")
            .bind(employee_id)
            .bind(org_id)
            .fetch_optional(pool.get_ref())
            .await?
            .unwrap_or(false);
    let slip = if on_payroll {
        let mut conn = pool.acquire().await?;
        compute_payslips(&mut conn, org_id, start, end, &settings, Some(employee_id))
            .await?
            .into_iter()
            .find(|s| s.employee_id == employee_id)
    } else {
        None
    };
    let (room, cap, outstanding, _) = advance_room(pool.get_ref(), org_id, employee_id).await?;
    Ok(HttpResponse::Ok().json(PayEstimate {
        period_start: start,
        period_end: end,
        slip,
        advance_room_piastres: room,
        advance_cap_piastres: cap,
        advance_outstanding_piastres: outstanding,
        on_payroll,
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
    let by = claims.user_id_safe().ok();
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
    let paid: Option<(Uuid, i64)> = sqlx::query_as(
        "UPDATE payslips SET paid_method = $3, paid_at = now(), paid_by = $4 \
          WHERE payroll_period_id = $1 AND employee_id = $2 AND paid_at IS NULL \
          RETURNING id, net_piastres",
    )
    .bind(period_id)
    .bind(employee_id)
    .bind(&body.method)
    .bind(by)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((payslip_id, net)) = paid else {
        return Err(AppError::Conflict(
            "That payslip is already paid or doesn't exist.".into(),
        ));
    };
    settle_period_if_all_paid(&mut tx, period_id).await?;
    audit(
        &mut *tx,
        org_id,
        by,
        "payslip.paid",
        "payslips",
        Some(payslip_id),
        Some(employee_id),
        Some(period_id),
        None,
        json!({ "method": body.method, "net_piastres": net }),
    )
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
    /// A percent line valued against the salary, in piastres — the server's
    /// figure (AT-3); equals `amount_piastres` for a flat line.
    pub value_piastres: i64,
    pub reason: String,
    /// The month it lands in (the first day of a recurring line, AD-1/AD-3).
    pub effective_date: NaiveDate,
    pub source: String,
    /// `pending` (waits for the owner) · `approved` · `rejected`
    pub status: String,
    pub recurring: bool,
    pub ends_on: Option<NaiveDate>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    /// A rule-made deduction the manager forgave: shown, counted for nothing (AD-6/AD-8).
    #[sqlx(default)]
    pub waived_at: Option<DateTime<Utc>>,
    #[sqlx(default)]
    pub overridden_at: Option<DateTime<Utc>>,
    #[sqlx(default)]
    pub original_amount_piastres: Option<i64>,
    #[sqlx(default)]
    pub stopped_at: Option<DateTime<Utc>>,
    #[sqlx(default)]
    pub stop_reason: Option<String>,
}

const ADJ_SELECT: &str = "SELECT * FROM ( \
    SELECT a.id, 'bonus' AS kind, a.org_id, a.employee_id, e.name AS employee_name, a.amount_piastres, \
           a.percent_of_base, \
           COALESCE(a.amount_piastres, round(e.base_salary_piastres::numeric * COALESCE(a.percent_of_base, 0) / 100))::bigint AS value_piastres, \
           a.reason, a.effective_date, a.source, a.status, a.recurring, \
           a.ends_on, a.created_by, a.created_at, \
           NULL::timestamptz AS waived_at, NULL::timestamptz AS overridden_at, \
           NULL::bigint AS original_amount_piastres, a.stopped_at, a.stop_reason \
      FROM payroll_bonuses a JOIN employees e ON e.id = a.employee_id \
    UNION ALL \
    SELECT a.id, 'deduction', a.org_id, a.employee_id, e.name, a.amount_piastres, a.percent_of_base, \
           COALESCE(a.amount_piastres, round(e.base_salary_piastres::numeric * COALESCE(a.percent_of_base, 0) / 100))::bigint, \
           a.reason, a.effective_date, a.source, a.status, a.recurring, a.ends_on, a.created_by, \
           a.created_at, a.waived_at, a.overridden_at, a.original_amount_piastres, a.stopped_at, a.stop_reason \
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

/// The table and capability for a pay line of `kind`, with the rights checked
/// BEFORE the body is judged (AT-11): someone who may add neither bonuses nor
/// deductions anywhere gets 403, never a 400 that tells them the field names.
async fn gate_kind(
    pool: &PgPool,
    claims: &crate::auth::jwt::Claims,
    org_id: Uuid,
    kind: &str,
) -> Result<(&'static str, Cap), AppError> {
    match table_of(kind) {
        Ok(table) => {
            let cap = create_cap(table);
            access::gate(pool, claims, org_id, cap).await?;
            Ok((table, cap))
        }
        Err(bad_kind) => {
            if access::gate(pool, claims, org_id, Cap::HrAdjustmentsCreate)
                .await
                .is_err()
            {
                access::gate(pool, claims, org_id, Cap::HrDeductionsCreate).await?;
            }
            Err(bad_kind)
        }
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
    /// The month it lands in (AD-1): any day of that month; the first month
    /// of a recurring line (AD-3). Defaults to today. Must be an open month.
    #[serde(default)]
    pub effective_date: Option<NaiveDate>,
    /// Every month until stopped (AD-3).
    #[serde(default)]
    pub recurring: bool,
}

/// Add a bonus or deduction. Over the caller's limit it is created pending
/// and waits for the owner (AD-5). Bonuses and deductions have separate limits.
#[utoipa::path(
    post, path = "/staff/adjustments", tag = "staff", request_body = NewAdjustment,
    responses(
        (status = 201, body = Adjustment),
        (status = 409, description = "Dated in an approved month (AD-10)"),
        AppErrorResponse,
    ),
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
    let (table, cap) = gate_kind(pool, &claims, org_id, &body.kind).await?;
    let subject = access::subject(pool, org_id, body.employee_id).await?;
    if subject.is(&claims) {
        return Err(AppError::Forbidden(
            "You can't add pay lines for yourself.".into(),
        ));
    }
    // A manager adds pay lines for the people of their branches (RO-6).
    access::require_for(pool, &claims, cap, &subject).await?;
    let reason = body.reason.trim();
    if reason.is_empty() {
        return Err(AppError::BadRequest("A reason is required".into()));
    }
    let salary: i64 = sqlx::query_scalar(
        "SELECT base_salary_piastres FROM employees WHERE id = $1 AND org_id = $2",
    )
    .bind(body.employee_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?
    .unwrap_or(0);
    // A deduction is always an amount (AD-2); only a bonus may be a % of salary.
    let amount = match (body.amount_piastres, body.percent_of_base) {
        (Some(a), None) if a > 0 => a,
        (None, Some(p)) if body.kind == "bonus" && p > Decimal::ZERO && p <= Decimal::from(100) => {
            pricing::percent_of_salary(salary, p)
        }
        // Only a deduction hears this; a bonus percent outside 1–100 falls
        // through to the range (E2E B-PAY-1).
        (None, Some(_)) if body.kind == "deduction" => {
            return Err(AppError::BadRequest(
                "A deduction is an amount, not a percentage".into(),
            ));
        }
        _ => {
            return Err(AppError::BadRequest(
                "Give a positive amount or a percentage (1–100)".into(),
            ));
        }
    };
    // The person's own day, not the server's or the first branch's (AT-1).
    let today = today_for_employee(pool, org_id, body.employee_id).await?;
    let effective_date = body.effective_date.unwrap_or(today);
    // Once a month is approved, fixes go into the next month (AD-10).
    period_lock::assert_open(pool, org_id, effective_date, "a pay line").await?;

    let branch = access::decision_branch(pool, &claims, cap, &subject).await?;
    let mut ask = AuthzRequest::of(cap);
    ask.amount = Some(amount);
    let status = match crate::authz::require::decide_for(pool, by, &ask, branch).await? {
        Decision::Allow => "approved",
        Decision::NeedsApproval(_) => "pending",
        Decision::Deny(_) => return Err(crate::authz::require::denied(cap)),
    };
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
    .bind(effective_date)
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
        &[Cap::HrAdjustmentsCreate, Cap::HrDeductionsCreate],
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

/// The owner (or anyone whose limit covers it) decides a pending line. A
/// percent line is judged at its value in piastres (audit B5).
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
    let (kind, id) = path.into_inner();
    let (table, cap) = gate_kind(pool, &claims, org_id, &kind).await?;
    let a = load_adjustment(pool, id).await?;
    if a.status != "pending" || a.kind != kind {
        return Err(AppError::Conflict(
            "This line has already been decided".into(),
        ));
    }
    let subject = access::subject(pool, org_id, a.employee_id).await?;
    access::require_for(pool, &claims, cap, &subject).await?;
    period_lock::assert_open(pool, org_id, a.effective_date, "this pay line").await?;
    let branch = access::decision_branch(pool, &claims, cap, &subject).await?;
    let mut ask = AuthzRequest::of(cap);
    ask.amount = Some(a.value_piastres);
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
            json!({ "amount": a.value_piastres, "reason": a.reason }),
        )
        .await;
    }
    // The manager who added it hears back, in the app, through their employee.
    if let Some(creator) = a.created_by {
        let theirs: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM employees WHERE user_id = $1 AND org_id = $2")
                .bind(creator)
                .bind(org_id)
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

#[derive(Deserialize, ToSchema, Default)]
pub struct StopAdjustment {
    /// Why it stops (AD-9). Required: blank or missing is a 400.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Stop a monthly line from the next period on; past payslips keep it (AD-3).
#[utoipa::path(
    post, path = "/staff/adjustments/{kind}/{id}/stop", tag = "staff",
    params(("kind" = String, Path), ("id" = Uuid, Path)),
    request_body = StopAdjustment,
    responses((status = 200, body = Adjustment), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn stop_adjustment(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(String, Uuid)>,
    body: Option<web::Json<StopAdjustment>>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let by = claims.user_id_safe()?;
    let pool = pool.get_ref();
    let (kind, id) = path.into_inner();
    let (table, cap) = gate_kind(pool, &claims, org_id, &kind).await?;
    // For a line of someone the caller manages — never "anywhere" (audit B-3).
    let line = load_adjustment(pool, id).await?;
    if line.kind != kind {
        return Err(AppError::NotFound("Adjustment not found".into()));
    }
    let subject = access::subject(pool, org_id, line.employee_id).await?;
    access::require_for(pool, &claims, cap, &subject).await?;
    // Who, when and WHY (AD-9), like a waiver or a reopen (E2E B-PAY-2).
    // Checked after the rights, so a stranger still hears 403 (AT-11).
    let reason = body
        .as_ref()
        .and_then(|b| b.reason.as_deref())
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .ok_or_else(|| AppError::BadRequest("Stopping a monthly line needs a reason".into()))?;
    let period = ensure_current_period(pool, org_id).await?;
    // The current month is approved already? Then it keeps the line and the
    // stop takes effect after it.
    let ends_on = if crate::staff::payroll::is_closed_status(&period.status) {
        period.end_date
    } else {
        period.start_date - Duration::days(1)
    };
    let mut tx = pool.begin().await?;
    let n = sqlx::query(&format!(
        "UPDATE {table} SET ends_on = $3, stopped_by = $4, stopped_at = now(), stop_reason = $5, \
             updated_at = now() \
          WHERE id = $1 AND org_id = $2 AND recurring AND ends_on IS NULL"
    ))
    .bind(id)
    .bind(org_id)
    .bind(ends_on)
    .bind(by)
    .bind(reason)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(AppError::Conflict(
            "That isn't a running monthly line.".into(),
        ));
    }
    audit(
        &mut *tx,
        org_id,
        Some(by),
        "adjustment.stop",
        table,
        Some(id),
        Some(line.employee_id),
        None,
        Some(reason),
        json!({ "ends_on": ends_on }),
    )
    .await?;
    tx.commit().await?;
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

/// The cap and limit rules for approving `amount` for `employee_id`
/// (AV-5): the org's cap on what is OUTSTANDING (the one being decided
/// counted) may only be passed by someone holding the payroll run for every
/// branch — the owner; and the approver's own percent limit is judged on
/// outstanding + new, not on the single advance.
async fn approve_advance_checks(
    pool: &PgPool,
    claims: &crate::auth::jwt::Claims,
    org_id: Uuid,
    subject: &access::Subject,
    amount: i64,
    already_counted: i64,
) -> Result<(), AppError> {
    let by = claims.user_id_safe()?;
    let (_, cap, outstanding, salary) = advance_room(pool, org_id, subject.id).await?;
    let after = outstanding - already_counted + amount;
    if after > cap {
        let owner = access::can_everywhere(pool, claims, org_id, Cap::HrPayrollRun).await?;
        if !owner {
            return Err(AppError::Conflict(format!(
                "ADVANCE_OVER_CAP: that's over the advance cap — at most {} EGP more; the owner can approve it.",
                (cap - outstanding + already_counted).max(0) / 100
            )));
        }
    }
    let branch = access::decision_branch(pool, claims, Cap::HrAdvancesDecide, subject).await?;
    let mut ask = AuthzRequest::of(Cap::HrAdvancesDecide);
    ask.percent = Some(if salary > 0 {
        // Share of salary owed after this one, rounded UP so a limit of 50%
        // is never passed by a fraction of a percent.
        (after * 100 + salary - 1).div_euclid(salary)
    } else {
        100
    });
    let pending = crate::authz::Pending {
        request: ask,
        subject_id: subject.authz_key(),
        requested_by: subject.authz_key(),
        why: crate::authz::Why::NotHeld,
    };
    crate::authz::require::settle(pool, by, &pending, branch).await
}

/// Decide an advance within the cap: the approver's limit is a % of the
/// employee's salary owed after this one (AV-4, AV-5).
#[utoipa::path(
    patch, path = "/staff/advances/{id}/review", tag = "staff", request_body = ReviewAdvance,
    params(("id" = Uuid, Path)),
    responses(
        (status = 200, body = SalaryAdvance),
        (status = 409, description = "Over the cap for anyone but the owner"),
        AppErrorResponse,
    ),
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
    let monthly = installment_of(amount, installments)?;
    if body.approve {
        // The pending one is already counted as outstanding at its asked amount.
        approve_advance_checks(pool, &claims, org_id, &subject, amount, asked).await?;
    }
    let note = body
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty());
    let mut tx = pool.begin().await?;
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
    .bind(note)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut *tx,
        org_id,
        Some(by),
        "advance.decide",
        "salary_advances",
        Some(*id),
        Some(employee_id),
        None,
        note,
        json!({ "approve": body.approve, "amount_piastres": amount, "installments": installments, "asked_piastres": asked }),
    )
    .await?;
    tx.commit().await?;
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
    Ok(HttpResponse::Ok().json(load_advance(pool, *id).await?))
}

#[derive(Deserialize, ToSchema)]
pub struct RecordAdvance {
    pub employee_id: Uuid,
    pub amount_piastres: i64,
    /// 1 = in full from the next payslip (AV-3).
    #[serde(default)]
    pub installments: Option<i32>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// A manager hands an advance over directly (AV-2): recorded and approved in
/// ONE call under the same cap and limit as a review, so a refusal never
/// leaves a stray pending advance behind (audit B10).
#[utoipa::path(
    post, path = "/staff/advances/record", tag = "staff", request_body = RecordAdvance,
    responses(
        (status = 201, body = SalaryAdvance),
        (status = 409, description = "Over the cap for anyone but the owner"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn record_advance(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<RecordAdvance>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let by = claims.user_id_safe()?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrAdvancesDecide).await?;
    let subject = access::subject(pool, org_id, body.employee_id).await?;
    if subject.is(&claims) {
        return Err(AppError::Forbidden(
            "Someone else has to record your advance.".into(),
        ));
    }
    access::require_for(pool, &claims, Cap::HrAdvancesDecide, &subject).await?;
    let installments = body.installments.unwrap_or(1);
    let monthly = installment_of(body.amount_piastres, installments)?;
    approve_advance_checks(pool, &claims, org_id, &subject, body.amount_piastres, 0).await?;
    let reason = body
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty());
    let mut tx = pool.begin().await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO salary_advances (org_id, employee_id, amount_piastres, installments, \
             monthly_installment_piastres, remaining_piastres, reason, status, decided_by, decided_at) \
         VALUES ($1, $2, $3, $4, $5, $3, $6, 'approved', $7, now()) RETURNING id",
    )
    .bind(org_id)
    .bind(body.employee_id)
    .bind(body.amount_piastres)
    .bind(installments)
    .bind(monthly)
    .bind(reason)
    .bind(by)
    .fetch_one(&mut *tx)
    .await?;
    audit(
        &mut *tx,
        org_id,
        Some(by),
        "advance.record",
        "salary_advances",
        Some(id),
        Some(body.employee_id),
        None,
        reason,
        json!({ "amount_piastres": body.amount_piastres, "installments": installments }),
    )
    .await?;
    tx.commit().await?;
    notify(
        pool,
        org_id,
        body.employee_id,
        "staff.n_advance_approved",
        json!({ "amount": body.amount_piastres }),
    )
    .await;
    Ok(HttpResponse::Created().json(load_advance(pool, id).await?))
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
    /// `safe` · `bank`. A till pay-out is tagged on the POS (AV-8), never
    /// logged here by hand (AV-10).
    pub via: String,
    /// When the cash changed hands; defaults to today (AV-7).
    #[serde(default)]
    pub given_on: Option<NaiveDate>,
    /// Where it was handed over; defaults to the person's first branch. Must
    /// be a branch the caller may log at.
    #[serde(default)]
    pub branch_id: Option<Uuid>,
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
    let branch = match body.branch_id {
        Some(b) => {
            access::require_at(pool, &claims, org_id, Cap::HrExpenseAdvancesLog, b).await?;
            Some(b)
        }
        None => access::decision_branch(pool, &claims, Cap::HrExpenseAdvancesLog, &subject).await?,
    };
    if !matches!(body.via.as_str(), "safe" | "bank") {
        return Err(AppError::BadRequest(
            "via is safe or bank — a till pay-out is tagged on the till itself".into(),
        ));
    }
    if body.amount_piastres <= 0 {
        return Err(AppError::BadRequest("Amount must be positive".into()));
    }
    let purpose = body.purpose.trim();
    if purpose.is_empty() {
        return Err(AppError::BadRequest("Say what it's for".into()));
    }
    // The day where the cash changed hands (AT-1).
    let today = match branch {
        Some(_) => today_at(pool, org_id, branch).await?,
        None => today_for_employee(pool, org_id, body.employee_id).await?,
    };
    let given_on = body.given_on.unwrap_or(today);
    if given_on > today {
        return Err(AppError::BadRequest(
            "The date can't be in the future".into(),
        ));
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO expense_advances (org_id, employee_id, branch_id, amount_piastres, purpose, via, \
            handed_by, given_on) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
    )
    .bind(org_id)
    .bind(body.employee_id)
    .bind(branch)
    .bind(body.amount_piastres)
    .bind(purpose)
    .bind(&body.via)
    .bind(claims.user_id_safe().ok())
    .bind(given_on)
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
        &[Cap::HrExpenseAdvancesLog],
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

    #[test]
    fn the_installment_cap_is_shared_with_the_clients() {
        assert_eq!(crate::staff::payroll::MAX_INSTALLMENTS, 24);
    }
}
