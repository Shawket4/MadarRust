//! Payroll: adjustments, advances, periods, and payslips.
//!
//!   Net = base + overtime + bonuses − deductions − advance installment
//!
//! A PAYSLIP IS A SNAPSHOT. Approving (generating) a period freezes every
//! figure, including the individual bonus/deduction/advance rows that fed it,
//! into `payslips.breakdown`. From that moment the month is CLOSED
//! (`period_lock`): nothing dated inside it moves money any more, and the
//! only way back is a reopen — before anyone is marked paid — which drops the
//! payslips and makes the month a live preview again.
//!
//! ADVANCES ARE A LEDGER. Approving writes one `salary_advance_collections`
//! row per installment taken; `salary_advances.remaining_piastres` is derived
//! from that ledger by a database trigger. Dropping a payslip (reopen, delete)
//! drops its collections and the money comes back by itself — so nothing is
//! ever collected twice, and any balance can be rebuilt from the rows (AV-6).
//!
//! EVERY SHIFT IS PRICED ONCE, in `pricing::price_shift`, under the branch's
//! rules (AT-9, RU-2): payroll sums what it says for each attendance record,
//! and reads the penalty rows `penalties` wrote with the same function.

use std::collections::{BTreeMap, HashMap};

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    authz::{Cap, Decision, Request as AuthzRequest},
    errors::{AppError, AppErrorResponse},
    staff::{
        access,
        attendance::{AttendanceSettings, load_settings},
        period_lock,
        pricing::{self, ShiftFacts, ShiftRules},
        principal::{Me, caller},
        rules::resolve_adjustment_piastres,
        scope_org,
    },
};

use crate::staff::pricing::DEFAULT_SHIFT_MINUTES;

/// Statuses that mean the payslips are frozen.
pub(crate) fn is_closed_status(status: &str) -> bool {
    matches!(status, "generated" | "paid" | "closed")
}

// ── Models ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct PayrollAdjustment {
    pub id: Uuid,
    pub org_id: Uuid,
    pub employee_id: Uuid,
    #[sqlx(default)]
    pub employee_name: Option<String>,
    pub amount_piastres: Option<i64>,
    pub percent_of_base: Option<Decimal>,
    pub reason: String,
    pub effective_date: NaiveDate,
    pub source: String,
    pub status: String,
    /// What the RULE computed, before any human touched it. `None` on a
    /// hand-entered row — nothing was overridden, so there is no "original".
    #[sqlx(default)]
    pub original_amount_piastres: Option<i64>,
    #[sqlx(default)]
    pub overridden_at: Option<DateTime<Utc>>,
    #[sqlx(default)]
    pub override_reason: Option<String>,
    /// A waived deduction keeps its amount and stays visible; payroll skips it.
    #[sqlx(default)]
    pub waived_at: Option<DateTime<Utc>>,
    #[sqlx(default)]
    pub waive_reason: Option<String>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct SalaryAdvance {
    pub id: Uuid,
    pub org_id: Uuid,
    pub employee_id: Uuid,
    #[sqlx(default)]
    pub employee_name: Option<String>,
    pub amount_piastres: i64,
    pub installments: i32,
    pub monthly_installment_piastres: i64,
    /// Derived from the collection ledger (AV-6).
    pub remaining_piastres: i64,
    pub reason: Option<String>,
    pub status: String,
    pub decided_by: Option<Uuid>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decision_note: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// The owner's cap on what this person may owe in advances, in piastres
    /// (AV-5) — the server's figure, so no client recomputes it.
    #[sqlx(default)]
    pub cap_piastres: i64,
    /// What the person owes across their live advances (pending ones count).
    #[sqlx(default)]
    pub outstanding_piastres: i64,
}

const ADVANCE_SELECT: &str = r#"
    SELECT a.id, a.org_id, a.employee_id, e.name AS employee_name, a.amount_piastres,
           a.installments, a.monthly_installment_piastres, a.remaining_piastres,
           a.reason, a.status, a.decided_by, a.decided_at, a.decision_note,
           a.created_at, a.updated_at,
           dawam_advance_cap(a.org_id, e.base_salary_piastres) AS cap_piastres,
           COALESCE((SELECT SUM(o.remaining_piastres) FROM salary_advances o
                      WHERE o.employee_id = a.employee_id AND o.status IN ('pending', 'approved')), 0)::bigint
               AS outstanding_piastres
      FROM salary_advances a
      JOIN employees e ON e.id = a.employee_id
"#;

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct PayrollPeriod {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub status: String,
    pub employee_count: i32,
    pub total_net_piastres: i64,
    pub generated_at: Option<DateTime<Utc>>,
    pub generated_by: Option<Uuid>,
    pub paid_at: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub(crate) const PERIOD_COLS: &str = "id, org_id, name, start_date, end_date, status, employee_count, \
     total_net_piastres, generated_at, generated_by, paid_at, closed_at, created_at, updated_at";

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct Payslip {
    pub id: Uuid,
    pub org_id: Uuid,
    pub payroll_period_id: Uuid,
    pub employee_id: Uuid,
    #[sqlx(default)]
    pub employee_name: Option<String>,
    pub base_salary_piastres: i64,
    pub worked_days: Decimal,
    pub absent_days: Decimal,
    pub leave_days: Decimal,
    pub late_minutes: i32,
    pub overtime_minutes: i32,
    pub overtime_piastres: i64,
    pub bonuses_piastres: i64,
    pub deductions_piastres: i64,
    pub advance_installment_piastres: i64,
    pub net_piastres: i64,
    pub breakdown: serde_json::Value,
    pub generated_at: DateTime<Utc>,
    /// Paid by `cash` · `bank` · `wallet` (PAY-7), or `none` for a payslip
    /// with nothing to pay, marked by the run itself; null until marked paid.
    #[sqlx(default)]
    pub paid_method: Option<String>,
    #[sqlx(default)]
    pub paid_at: Option<DateTime<Utc>>,
    /// Who marked it paid (AT-10).
    #[sqlx(default)]
    pub paid_by: Option<Uuid>,
    /// What deductions exceeded pay by; carried into the next payslip (PAY-12).
    #[sqlx(default)]
    pub carry_out_piastres: i64,
    /// The period this covers, denormalised. A payslip identified only by its
    /// generation timestamp is unreadable — two months run on the same day would
    /// be indistinguishable to the employee looking at them.
    #[sqlx(default)]
    pub period_name: Option<String>,
    #[sqlx(default)]
    pub period_start: Option<NaiveDate>,
    #[sqlx(default)]
    pub period_end: Option<NaiveDate>,
    /// The person's pay method and account at the time of reading, for the
    /// bank and wallet lists (PAY-8).
    #[sqlx(default)]
    pub pay_method: Option<String>,
    #[sqlx(default)]
    pub pay_account: Option<String>,
}

pub(crate) const PAYSLIP_SELECT: &str = r#"
    SELECT s.id, s.org_id, s.payroll_period_id, s.employee_id, e.name AS employee_name,
           s.base_salary_piastres, s.worked_days, s.absent_days, s.leave_days,
           s.late_minutes, s.overtime_minutes, s.overtime_piastres, s.bonuses_piastres,
           s.deductions_piastres, s.advance_installment_piastres, s.net_piastres,
           s.breakdown, s.generated_at, s.paid_method, s.paid_at, s.paid_by, s.carry_out_piastres,
           pp.name AS period_name, pp.start_date AS period_start,
           pp.end_date AS period_end, e.pay_method, e.pay_account
      FROM payslips s
      JOIN employees e ON e.id = s.employee_id
      JOIN payroll_periods pp ON pp.id = s.payroll_period_id
"#;

// ── Requests ──────────────────────────────────────────────────

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateAdvanceRequest {
    /// Admin-only; omitted on `/staff/me/*`.
    #[serde(default)]
    pub employee_id: Option<Uuid>,
    pub amount_piastres: i64,
    /// Defaults to 1 — repaid in full from the next payslip.
    #[serde(default)]
    pub installments: Option<i32>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreatePeriodRequest {
    pub name: String,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct PeriodStatusRequest {
    /// `draft` (reopen an approved month, before anyone is paid) or `closed`
    /// (archive a paid month). Approving is `POST …/generate`; Paid is
    /// reached by marking everyone paid (PAY-7), never by hand.
    pub status: String,
    /// Why (AD-9). Required to reopen.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct AdjustmentQuery {
    #[serde(default)]
    pub employee_id: Option<Uuid>,
    #[serde(default)]
    pub from: Option<NaiveDate>,
    #[serde(default)]
    pub to: Option<NaiveDate>,
}

/// The most installments an advance is spread over (AV-3); the dashboard and
/// the app offer the same range.
pub const MAX_INSTALLMENTS: i32 = 24;

// ── Audit log (AD-9, AT-10) ───────────────────────────────────

/// One line in the money audit log: who did what to which row, and why.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn audit<'e, E>(
    conn: E,
    org_id: Uuid,
    actor: Option<Uuid>,
    action: &str,
    entity: &str,
    entity_id: Option<Uuid>,
    employee_id: Option<Uuid>,
    period_id: Option<Uuid>,
    reason: Option<&str>,
    details: serde_json::Value,
) -> Result<(), AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query(
        "INSERT INTO payroll_audit_log (org_id, actor_id, action, entity, entity_id, employee_id, \
             period_id, reason, details) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(org_id)
    .bind(actor)
    .bind(action)
    .bind(entity)
    .bind(entity_id)
    .bind(employee_id)
    .bind(period_id)
    .bind(reason)
    .bind(details)
    .execute(conn)
    .await?;
    Ok(())
}

#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct AuditRow {
    pub id: Uuid,
    pub actor_id: Option<Uuid>,
    pub actor_name: Option<String>,
    pub action: String,
    pub entity: String,
    pub entity_id: Option<Uuid>,
    pub employee_id: Option<Uuid>,
    pub employee_name: Option<String>,
    pub period_id: Option<Uuid>,
    pub reason: Option<String>,
    pub details: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct AuditQuery {
    #[serde(default)]
    pub employee_id: Option<Uuid>,
    #[serde(default)]
    pub period_id: Option<Uuid>,
}

/// The money audit log: every delete, stop, waive, override, reopen and
/// payment, with who and why (AD-9, AT-10).
#[utoipa::path(
    get, path = "/staff/payroll/audit", tag = "staff", params(AuditQuery),
    responses((status = 200, body = Vec<AuditRow>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_audit(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<AuditQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::require_everywhere(pool.get_ref(), &claims, org_id, Cap::HrPayrollRead).await?;
    let rows = sqlx::query_as::<_, AuditRow>(
        "SELECT l.id, l.actor_id, u.name AS actor_name, l.action, l.entity, l.entity_id, \
                l.employee_id, e.name AS employee_name, l.period_id, l.reason, l.details, l.created_at \
           FROM payroll_audit_log l \
           LEFT JOIN users u ON u.id = l.actor_id \
           LEFT JOIN employees e ON e.id = l.employee_id \
          WHERE l.org_id = $1 AND ($2::uuid IS NULL OR l.employee_id = $2) \
            AND ($3::uuid IS NULL OR l.period_id = $3) \
          ORDER BY l.created_at DESC LIMIT 500",
    )
    .bind(org_id)
    .bind(query.employee_id)
    .bind(query.period_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

// ── Adjustments (deductions + bonuses) ────────────────────────

/// Deductions and bonuses are the same shape with opposite signs, so both tables
/// are served by one pair of handlers parameterised by table name. The name is
/// never caller-supplied — see the two call sites.
fn adjustment_select(table: &str) -> String {
    // Bonuses have no override/waive columns — only deductions are ever machine
    // generated, so only deductions need vetoing. NULL literals keep one row
    // shape for both tables.
    let overrides = if table == "payroll_deductions" {
        "a.original_amount_piastres, a.overridden_at, a.override_reason, \
         a.waived_at, a.waive_reason"
    } else {
        "NULL::bigint AS original_amount_piastres, \
         NULL::timestamptz AS overridden_at, NULL::text AS override_reason, \
         NULL::timestamptz AS waived_at, NULL::text AS waive_reason"
    };
    format!(
        "SELECT a.id, a.org_id, a.employee_id, e.name AS employee_name, a.amount_piastres, \
                a.percent_of_base, a.reason, a.effective_date, a.source, a.status, \
                {overrides}, \
                a.created_by, a.created_at, a.updated_at \
           FROM {table} a JOIN employees e ON e.id = a.employee_id"
    )
}

/// The capability that adds (and so may delete) a line of `table` (AD-5:
/// bonuses and deductions have separate limits).
pub(crate) fn create_cap(table: &str) -> Cap {
    if table == "payroll_deductions" {
        Cap::HrDeductionsCreate
    } else {
        Cap::HrAdjustmentsCreate
    }
}

async fn list_adjustments(
    req: &HttpRequest,
    pool: &crate::db::Db,
    query: &AdjustmentQuery,
    table: &str,
) -> Result<HttpResponse, AppError> {
    let claims = caller(req)?;
    let org_id = scope_org(req, &claims)?;
    let scope = money_scope(pool.get_ref(), &claims, org_id, &[create_cap(table)]).await?;

    let rows = sqlx::query_as::<_, PayrollAdjustment>(&format!(
        "{} WHERE a.org_id = $1 \
             AND ($2::uuid IS NULL OR a.employee_id = $2) \
             AND ($3::date IS NULL OR a.effective_date >= $3) \
             AND ($4::date IS NULL OR a.effective_date <= $4) \
             AND {} \
           ORDER BY a.effective_date DESC, lower(e.name)",
        adjustment_select(table),
        access::in_scope("a.employee_id", 5)
    ))
    .bind(org_id)
    .bind(query.employee_id)
    .bind(query.from)
    .bind(query.to)
    .bind(scope.as_deref())
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

/// Delete a MANUAL line while its month is still open (AD-7, AD-10). Rule-made
/// lines are never deleted: they are waived or overridden, with a reason.
async fn delete_adjustment(
    req: &HttpRequest,
    pool: &crate::db::Db,
    id: Uuid,
    table: &str,
) -> Result<HttpResponse, AppError> {
    let claims = caller(req)?;
    let org_id = scope_org(req, &claims)?;
    let cap = create_cap(table);
    access::gate(pool.get_ref(), &claims, org_id, cap).await?;

    let row: Option<(Uuid, String, NaiveDate, String, Option<i64>)> = sqlx::query_as(&format!(
        "SELECT employee_id, source, effective_date, reason, amount_piastres \
           FROM {table} WHERE id = $1 AND org_id = $2"
    ))
    .bind(id)
    .bind(org_id)
    .fetch_optional(pool.get_ref())
    .await?;
    let Some((employee_id, source, effective_date, reason, amount)) = row else {
        return Err(AppError::NotFound("Adjustment not found".into()));
    };
    let subject = access::subject(pool.get_ref(), org_id, employee_id).await?;
    access::require_for(pool.get_ref(), &claims, cap, &subject).await?;
    if source != "manual" {
        return Err(AppError::Conflict(
            "A rule-made line is never deleted: waive or override it, with a reason.".into(),
        ));
    }
    period_lock::assert_open(pool.get_ref(), org_id, effective_date, "a pay line").await?;

    let mut tx = pool.begin().await?;
    let deleted = sqlx::query(&format!(
        "DELETE FROM {table} WHERE id = $1 AND org_id = $2 AND source = 'manual'"
    ))
    .bind(id)
    .bind(org_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if deleted == 0 {
        return Err(AppError::NotFound("Adjustment not found".into()));
    }
    audit(
        &mut *tx,
        org_id,
        claims.user_id_safe().ok(),
        "adjustment.delete",
        table,
        Some(id),
        Some(employee_id),
        None,
        None,
        json!({ "reason": reason, "amount_piastres": amount, "effective_date": effective_date }),
    )
    .await?;
    tx.commit().await?;
    Ok(HttpResponse::NoContent().finish())
}

#[utoipa::path(
    get, path = "/staff/payroll/deductions", tag = "staff", params(AdjustmentQuery),
    responses((status = 200, description = "Deductions", body = Vec<PayrollAdjustment>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_deductions(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<AdjustmentQuery>,
) -> Result<HttpResponse, AppError> {
    list_adjustments(&req, &pool, &query, "payroll_deductions").await
}

#[utoipa::path(
    delete, path = "/staff/payroll/deductions/{id}", tag = "staff",
    params(("id" = Uuid, Path, description = "Deduction ID")),
    responses(
        (status = 204, description = "Deduction deleted"),
        (status = 409, description = "Rule-made, or its month is approved"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn delete_deduction(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    delete_adjustment(&req, &pool, *id, "payroll_deductions").await
}

#[utoipa::path(
    get, path = "/staff/payroll/bonuses", tag = "staff", params(AdjustmentQuery),
    responses((status = 200, description = "Bonuses", body = Vec<PayrollAdjustment>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_bonuses(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<AdjustmentQuery>,
) -> Result<HttpResponse, AppError> {
    list_adjustments(&req, &pool, &query, "payroll_bonuses").await
}

#[utoipa::path(
    delete, path = "/staff/payroll/bonuses/{id}", tag = "staff",
    params(("id" = Uuid, Path, description = "Bonus ID")),
    responses(
        (status = 204, description = "Bonus deleted"),
        (status = 409, description = "Its month is approved"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn delete_bonus(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    delete_adjustment(&req, &pool, *id, "payroll_bonuses").await
}

// ── Overriding an automatic deduction ─────────────────────────

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct OverrideDeductionRequest {
    /// The figure to charge instead of the computed one. Zero is allowed — it
    /// means "charge nothing" while keeping the row and its history.
    pub amount_piastres: i64,
    /// Required. An override with no stated reason is indistinguishable from a
    /// mistake six months later.
    pub reason: String,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct WaiveDeductionRequest {
    pub reason: String,
}

/// The deduction a waive/override/unwaive acts on, once the caller may edit
/// that person's pay and its month is still open.
async fn deduction_for_edit(
    pool: &PgPool,
    claims: &Claims,
    org_id: Uuid,
    id: Uuid,
    what: &str,
) -> Result<(Uuid, NaiveDate, i64, bool), AppError> {
    let row: Option<(Uuid, NaiveDate, i64, bool)> = sqlx::query_as(
        "SELECT employee_id, effective_date, amount_piastres, waived_at IS NOT NULL \
           FROM payroll_deductions WHERE id = $1 AND org_id = $2",
    )
    .bind(id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Err(AppError::NotFound("Deduction not found".into()));
    };
    let subject = access::subject(pool, org_id, row.0).await?;
    // A waive, override or unwaive is a deduction decision (AD-7, AD-8): the
    // manager's deduction capability at one of the person's branches; the
    // limit on raising is judged by the caller.
    access::require_for(pool, claims, Cap::HrDeductionsCreate, &subject).await?;
    period_lock::assert_open(pool, org_id, row.1, what).await?;
    Ok(row)
}

#[utoipa::path(
    patch, path = "/staff/payroll/deductions/{id}/override", tag = "staff",
    params(("id" = Uuid, Path, description = "Deduction ID")),
    request_body = OverrideDeductionRequest,
    responses(
        (status = 200, description = "Deduction overridden", body = PayrollAdjustment),
        (status = 409, description = "Waived (final), or its month is approved"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn override_deduction(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<OverrideDeductionRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::gate(pool.get_ref(), &claims, org_id, Cap::HrDeductionsCreate).await?;
    let (employee_id, _, current, waived) =
        deduction_for_edit(pool.get_ref(), &claims, org_id, *id, "an override").await?;

    let reason = body.reason.trim();
    if reason.is_empty() {
        return Err(AppError::BadRequest("An override needs a reason".into()));
    }
    if body.amount_piastres < 0 {
        return Err(AppError::BadRequest("Amount cannot be negative".into()));
    }
    // A waived line is final (AD-8): no override brings it back or changes it.
    if waived {
        return Err(AppError::Conflict(
            "This deduction was waived — a waiver is final.".into(),
        ));
    }
    // Raising a deduction is adding one: the increase is judged against the
    // caller's deduction limit (AD-5). Lowering it is a partial waiver.
    if body.amount_piastres > current {
        let subject = access::subject(pool.get_ref(), org_id, employee_id).await?;
        let branch =
            access::decision_branch(pool.get_ref(), &claims, Cap::HrDeductionsCreate, &subject)
                .await?;
        let mut ask = AuthzRequest::of(Cap::HrDeductionsCreate);
        ask.amount = Some(body.amount_piastres - current);
        match crate::authz::require::decide_for(
            pool.get_ref(),
            claims.user_id_safe()?,
            &ask,
            branch,
        )
        .await?
        {
            Decision::Allow => {}
            _ => {
                return Err(AppError::Coded {
                    status: 403,
                    code: "ABOVE_LIMIT",
                    reason:
                        "Raising this deduction is above your limit — the owner can override it."
                            .into(),
                });
            }
        }
    }

    let mut tx = pool.begin().await?;
    // `original_amount_piastres` is only ever set from the CURRENT amount when it
    // is still NULL, so overriding twice does not lose what the rule first said.
    let updated = sqlx::query(
        "UPDATE payroll_deductions SET \
             original_amount_piastres = COALESCE(original_amount_piastres, amount_piastres), \
             amount_piastres = $3, overridden_at = now(), overridden_by = $4, \
             override_reason = $5, updated_at = now() \
          WHERE id = $1 AND org_id = $2 AND waived_at IS NULL",
    )
    .bind(*id)
    .bind(org_id)
    .bind(body.amount_piastres)
    .bind(claims.user_id_safe().ok())
    .bind(reason)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(AppError::NotFound("Deduction not found".into()));
    }
    audit(
        &mut *tx,
        org_id,
        claims.user_id_safe().ok(),
        "deduction.override",
        "payroll_deductions",
        Some(*id),
        Some(employee_id),
        None,
        Some(reason),
        json!({ "from_piastres": current, "to_piastres": body.amount_piastres }),
    )
    .await?;
    tx.commit().await?;

    let row = sqlx::query_as::<_, PayrollAdjustment>(&format!(
        "{} WHERE a.id = $1",
        adjustment_select("payroll_deductions")
    ))
    .bind(*id)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    patch, path = "/staff/payroll/deductions/{id}/waive", tag = "staff",
    params(("id" = Uuid, Path, description = "Deduction ID")),
    request_body = WaiveDeductionRequest,
    responses(
        (status = 200, description = "Deduction waived", body = PayrollAdjustment),
        (status = 409, description = "Its month is approved"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn waive_deduction(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<WaiveDeductionRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::gate(pool.get_ref(), &claims, org_id, Cap::HrDeductionsCreate).await?;
    let (employee_id, _, amount, _) =
        deduction_for_edit(pool.get_ref(), &claims, org_id, *id, "a waiver").await?;

    let reason = body.reason.trim();
    if reason.is_empty() {
        return Err(AppError::BadRequest("A waiver needs a reason".into()));
    }

    let mut tx = pool.begin().await?;
    // Deliberately NOT a delete. The row stays so the decision is on the record,
    // and so the nightly sweep sees it and leaves it alone instead of recreating
    // the penalty the manager just forgave.
    let updated = sqlx::query(
        "UPDATE payroll_deductions SET \
             original_amount_piastres = COALESCE(original_amount_piastres, amount_piastres), \
             waived_at = now(), waived_by = $3, waive_reason = $4, updated_at = now() \
          WHERE id = $1 AND org_id = $2 AND waived_at IS NULL",
    )
    .bind(*id)
    .bind(org_id)
    .bind(claims.user_id_safe().ok())
    .bind(reason)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(AppError::NotFound(
            "Deduction not found, or already waived".into(),
        ));
    }
    audit(
        &mut *tx,
        org_id,
        claims.user_id_safe().ok(),
        "deduction.waive",
        "payroll_deductions",
        Some(*id),
        Some(employee_id),
        None,
        Some(reason),
        json!({ "amount_piastres": amount }),
    )
    .await?;
    tx.commit().await?;

    let row = sqlx::query_as::<_, PayrollAdjustment>(&format!(
        "{} WHERE a.id = $1",
        adjustment_select("payroll_deductions")
    ))
    .bind(*id)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(row))
}

/// Undo a waiver, with a reason (AT-7): the line counts again at the amount
/// it had. Only while the month is open.
#[utoipa::path(
    patch, path = "/staff/payroll/deductions/{id}/unwaive", tag = "staff",
    params(("id" = Uuid, Path, description = "Deduction ID")),
    request_body = WaiveDeductionRequest,
    responses(
        (status = 200, description = "Waiver undone", body = PayrollAdjustment),
        (status = 409, description = "Its month is approved"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn unwaive_deduction(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<WaiveDeductionRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::gate(pool.get_ref(), &claims, org_id, Cap::HrDeductionsCreate).await?;
    let (employee_id, _, amount, waived) =
        deduction_for_edit(pool.get_ref(), &claims, org_id, *id, "undoing a waiver").await?;
    let reason = body.reason.trim();
    if reason.is_empty() {
        return Err(AppError::BadRequest(
            "Undoing a waiver needs a reason".into(),
        ));
    }
    if !waived {
        return Err(AppError::Conflict("This deduction is not waived.".into()));
    }
    let mut tx = pool.begin().await?;
    // Who took the waiver back, when and why stay on the row (AT-7, AT-10).
    sqlx::query(
        "UPDATE payroll_deductions SET waived_at = NULL, waived_by = NULL, waive_reason = NULL, \
             unwaived_at = now(), unwaived_by = $3, unwaive_reason = $4, \
             updated_at = now() WHERE id = $1 AND org_id = $2",
    )
    .bind(*id)
    .bind(org_id)
    .bind(claims.user_id_safe().ok())
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut *tx,
        org_id,
        claims.user_id_safe().ok(),
        "deduction.unwaive",
        "payroll_deductions",
        Some(*id),
        Some(employee_id),
        None,
        Some(reason),
        json!({ "amount_piastres": amount }),
    )
    .await?;
    tx.commit().await?;
    // An automatic deduction is priced again by today's rule for its day.
    let record: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT a.id, a.branch_id FROM payroll_deductions d \
           JOIN attendance_records a ON a.id = d.attendance_record_id \
          WHERE d.id = $1 AND d.org_id = $2",
    )
    .bind(*id)
    .bind(org_id)
    .fetch_optional(pool.get_ref())
    .await?;
    if let Some((record_id, branch_id)) = record {
        let settings =
            crate::staff::attendance::load_settings(pool.get_ref(), org_id, Some(branch_id))
                .await?;
        crate::staff::penalties::recompute_record(pool.get_ref(), record_id, &settings).await?;
    }
    let row = sqlx::query_as::<_, PayrollAdjustment>(&format!(
        "{} WHERE a.id = $1",
        adjustment_select("payroll_deductions")
    ))
    .bind(*id)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(row))
}

// ── Salary advances ───────────────────────────────────────────

#[utoipa::path(
    get, path = "/staff/payroll/advances", tag = "staff", params(AdjustmentQuery),
    responses((status = 200, description = "Salary advances", body = Vec<SalaryAdvance>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_advances(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<AdjustmentQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    // Payroll readers, and whoever decides advances, for their branches (B15).
    let scope = money_scope(pool.get_ref(), &claims, org_id, &[Cap::HrAdvancesDecide]).await?;

    let rows = sqlx::query_as::<_, SalaryAdvance>(&format!(
        "{ADVANCE_SELECT} WHERE a.org_id = $1 AND ($2::uuid IS NULL OR a.employee_id = $2) \
            AND {} ORDER BY a.created_at DESC",
        access::in_scope("a.employee_id", 3)
    ))
    .bind(org_id)
    .bind(query.employee_id)
    .bind(scope.as_deref())
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

/// Validate an advance's shape; returns the monthly installment.
pub(crate) fn installment_of(amount: i64, installments: i32) -> Result<i64, AppError> {
    if amount <= 0 {
        return Err(AppError::BadRequest("Amount must be positive".into()));
    }
    if !(1..=MAX_INSTALLMENTS).contains(&installments) {
        return Err(AppError::BadRequest(format!(
            "Installments must be between 1 and {MAX_INSTALLMENTS}"
        )));
    }
    // Round the installment UP so the final one is the small remainder rather
    // than leaving a few piastres outstanding forever.
    Ok((amount as u64).div_ceil(installments as u64) as i64)
}

pub(crate) async fn insert_advance(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
    body: &CreateAdvanceRequest,
) -> Result<SalaryAdvance, AppError> {
    let installments = body.installments.unwrap_or(1);
    let monthly = installment_of(body.amount_piastres, installments)?;

    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO salary_advances (org_id, employee_id, amount_piastres, installments, \
                                      monthly_installment_piastres, remaining_piastres, reason) \
         VALUES ($1, $2, $3, $4, $5, $3, $6) RETURNING id",
    )
    .bind(org_id)
    .bind(employee_id)
    .bind(body.amount_piastres)
    .bind(installments)
    .bind(monthly)
    .bind(
        body.reason
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty()),
    )
    .fetch_one(pool)
    .await?;

    load_advance(pool, id).await
}

pub(crate) async fn load_advance(pool: &PgPool, id: Uuid) -> Result<SalaryAdvance, AppError> {
    Ok(
        sqlx::query_as::<_, SalaryAdvance>(&format!("{ADVANCE_SELECT} WHERE a.id = $1"))
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

/// Record an ask on someone's behalf: it still waits for a decision
/// (`PATCH /staff/advances/{id}/review`). To hand one over at once, use
/// `POST /staff/advances/record`.
#[utoipa::path(
    post, path = "/staff/payroll/advances", tag = "staff", request_body = CreateAdvanceRequest,
    responses((status = 201, description = "Advance created (pending)", body = SalaryAdvance), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_advance_admin(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateAdvanceRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::gate(pool.get_ref(), &claims, org_id, Cap::HrAdvancesDecide).await?;
    let employee_id = body
        .employee_id
        .ok_or_else(|| AppError::BadRequest("employee_id is required".into()))?;
    let subject = access::subject(pool.get_ref(), org_id, employee_id).await?;
    access::require_for(pool.get_ref(), &claims, Cap::HrAdvancesDecide, &subject).await?;

    let row = insert_advance(pool.get_ref(), org_id, employee_id, &body).await?;
    Ok(HttpResponse::Created().json(row))
}

#[utoipa::path(
    post, path = "/staff/me/advances", tag = "staff", request_body = CreateAdvanceRequest,
    responses((status = 201, description = "Advance requested", body = SalaryAdvance), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_my_advance(
    me: Me,
    pool: crate::db::Db,
    body: web::Json<CreateAdvanceRequest>,
) -> Result<HttpResponse, AppError> {
    let row = insert_advance(pool.get_ref(), me.org_id, me.employee_id, &body).await?;
    Ok(HttpResponse::Created().json(row))
}

#[utoipa::path(
    get, path = "/staff/me/advances", tag = "staff",
    responses((status = 200, description = "The employee's own advances", body = Vec<SalaryAdvance>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_advances(me: Me, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let rows = sqlx::query_as::<_, SalaryAdvance>(&format!(
        "{ADVANCE_SELECT} WHERE a.employee_id = $1 ORDER BY a.created_at DESC"
    ))
    .bind(me.employee_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

// ── Periods ───────────────────────────────────────────────────

#[utoipa::path(
    get, path = "/staff/payroll/periods", tag = "staff",
    responses((status = 200, description = "Payroll periods", body = Vec<PayrollPeriod>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_periods(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::scope(pool.get_ref(), &claims, org_id, Cap::HrPayrollRead).await?;

    let rows = sqlx::query_as::<_, PayrollPeriod>(&format!(
        "SELECT {PERIOD_COLS} FROM payroll_periods WHERE org_id = $1 ORDER BY start_date DESC"
    ))
    .bind(org_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    post, path = "/staff/payroll/periods", tag = "staff", request_body = CreatePeriodRequest,
    responses(
        (status = 201, description = "Period created", body = PayrollPeriod),
        (status = 409, description = "Overlaps an existing period"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn create_period(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreatePeriodRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    // Payroll runs for the whole business (RO-9).
    access::require_everywhere(pool.get_ref(), &claims, org_id, Cap::HrPayrollRun).await?;

    let name = body.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("Period name is required".into()));
    }
    if body.end_date < body.start_date {
        return Err(AppError::BadRequest("End date is before start date".into()));
    }
    // Two periods that overlap would each pay the base and each collect an
    // installment (B8). The database refuses it too; this is the readable answer.
    let overlaps: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM payroll_periods \
          WHERE org_id = $1 AND start_date <= $3 AND end_date >= $2)",
    )
    .bind(org_id)
    .bind(body.start_date)
    .bind(body.end_date)
    .fetch_one(pool.get_ref())
    .await?;
    if overlaps {
        return Err(AppError::Conflict(
            "That span overlaps a period that already exists.".into(),
        ));
    }

    let row = sqlx::query_as::<_, PayrollPeriod>(&format!(
        "INSERT INTO payroll_periods (org_id, name, start_date, end_date) \
         VALUES ($1, $2, $3, $4) RETURNING {PERIOD_COLS}"
    ))
    .bind(org_id)
    .bind(name)
    .bind(body.start_date)
    .bind(body.end_date)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Created().json(row))
}

/// Reopen an approved month (before anyone is paid) or close a paid one.
///
/// Reopening DROPS the frozen payslips: their advance collections go with
/// them (the ledger trigger refunds), so the live preview reads exactly what
/// re-approving will collect (PAY-2, PAY-6, audit B7).
#[utoipa::path(
    patch, path = "/staff/payroll/periods/{id}/status", tag = "staff",
    params(("id" = Uuid, Path, description = "Period ID")),
    request_body = PeriodStatusRequest,
    responses(
        (status = 200, description = "Status changed", body = PayrollPeriod),
        (status = 409, description = "Not a move this period can make"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn set_period_status(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<PeriodStatusRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    // Approving, reopening, paying and closing are the payroll run (RO-9).
    access::require_everywhere(pool.get_ref(), &claims, org_id, Cap::HrPayrollRun).await?;

    let mut tx = pool.begin().await?;
    let current: String = sqlx::query_scalar(
        "SELECT status FROM payroll_periods WHERE id = $1 AND org_id = $2 FOR UPDATE",
    )
    .bind(*id)
    .bind(org_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound("Payroll period not found".into()))?;
    let reason = body
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty());

    match (current.as_str(), body.status.as_str()) {
        ("generated", "draft") => {
            let Some(reason) = reason else {
                return Err(AppError::BadRequest("Reopening needs a reason".into()));
            };
            // PAY-6: only while nobody has been paid — a 'none' mark by the run
            // itself (nothing to pay) is not a payment.
            let any_paid: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM payslips WHERE payroll_period_id = $1 \
                    AND paid_at IS NOT NULL AND paid_method <> 'none')",
            )
            .bind(*id)
            .fetch_one(&mut *tx)
            .await?;
            if any_paid {
                return Err(AppError::Conflict(
                    "Someone has already been paid — this payroll can't be reopened.".into(),
                ));
            }
            let dropped = sqlx::query("DELETE FROM payslips WHERE payroll_period_id = $1")
                .bind(*id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            sqlx::query(
                "UPDATE payroll_periods SET status = 'draft', employee_count = 0, \
                     total_net_piastres = 0, generated_at = NULL, generated_by = NULL, \
                     updated_at = now() WHERE id = $1",
            )
            .bind(*id)
            .execute(&mut *tx)
            .await?;
            audit(
                &mut *tx,
                org_id,
                claims.user_id_safe().ok(),
                "period.reopen",
                "payroll_periods",
                Some(*id),
                None,
                Some(*id),
                Some(reason),
                json!({ "payslips_dropped": dropped }),
            )
            .await?;
        }
        ("paid", "closed") => {
            sqlx::query(
                "UPDATE payroll_periods SET status = 'closed', closed_at = now(), updated_at = now() \
                  WHERE id = $1",
            )
            .bind(*id)
            .execute(&mut *tx)
            .await?;
            audit(
                &mut *tx,
                org_id,
                claims.user_id_safe().ok(),
                "period.close",
                "payroll_periods",
                Some(*id),
                None,
                Some(*id),
                reason,
                json!({}),
            )
            .await?;
        }
        (_, "generated") => {
            return Err(AppError::Conflict(
                "Approve a month with POST …/generate; it freezes the payslips.".into(),
            ));
        }
        (_, "paid") => {
            return Err(AppError::Conflict(
                "A month is Paid when every payslip is marked paid — never by hand (PAY-7).".into(),
            ));
        }
        (cur, target) => {
            return Err(AppError::Conflict(format!(
                "A {cur} period cannot move to {target}"
            )));
        }
    }
    let row = sqlx::query_as::<_, PayrollPeriod>(&format!(
        "SELECT {PERIOD_COLS} FROM payroll_periods WHERE id = $1"
    ))
    .bind(*id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(HttpResponse::Ok().json(row))
}

/// Delete a DRAFT period. An approved month is reopened first (which is
/// refused once anyone is paid), so a paid payslip can never be wiped (B3).
#[utoipa::path(
    delete, path = "/staff/payroll/periods/{id}", tag = "staff",
    params(("id" = Uuid, Path, description = "Period ID")),
    responses(
        (status = 204, description = "Period deleted"),
        (status = 409, description = "Only a draft period can be deleted"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn delete_period(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::require_everywhere(pool.get_ref(), &claims, org_id, Cap::HrPayrollRun).await?;

    let mut tx = pool.begin().await?;
    let (status, name): (String, String) = sqlx::query_as(
        "SELECT status, name FROM payroll_periods WHERE id = $1 AND org_id = $2 FOR UPDATE",
    )
    .bind(*id)
    .bind(org_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound("Payroll period not found".into()))?;
    if status != "draft" {
        return Err(AppError::Conflict(format!(
            "A {status} period cannot be deleted — reopen it first (only before anyone is paid)"
        )));
    }
    // Any payslips a draft still holds cascade away, and their collections'
    // trigger gives the advances back.
    audit(
        &mut *tx,
        org_id,
        claims.user_id_safe().ok(),
        "period.delete",
        "payroll_periods",
        Some(*id),
        None,
        None,
        None,
        json!({ "name": name }),
    )
    .await?;
    sqlx::query("DELETE FROM payroll_periods WHERE id = $1 AND org_id = $2")
        .bind(*id)
        .bind(org_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(HttpResponse::NoContent().finish())
}

// ── Generation ────────────────────────────────────────────────

/// One employee's pay for a period, computed but not yet written.
///
/// PREVIEW AND GENERATE SHARE THIS. The preview endpoint exists so a manager can
/// see what payroll is about to do — a figure that would be worthless if it came
/// from a second implementation that could drift from the real one. So the
/// generator computes these first and then persists them, and the preview
/// computes exactly the same values and persists nothing.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ComputedPayslip {
    pub employee_id: Uuid,
    pub name: String,
    /// The monthly salary in force at the end of the window.
    pub base_salary_piastres: i64,
    pub worked_days: Decimal,
    pub absent_days: Decimal,
    pub leave_days: Decimal,
    pub late_minutes: i64,
    pub overtime_minutes: i64,
    /// After the calendar-day proration — what the days employed earn, at
    /// each day's salary (PAY-13).
    pub base_piastres: i64,
    pub overtime_piastres: i64,
    pub bonuses_piastres: i64,
    pub deductions_piastres: i64,
    /// What the advances WANT versus what the payslip can afford differ when net
    /// pay would go negative; this is the affordable figure, the one collected.
    pub advance_installment_piastres: i64,
    pub net_piastres: i64,
    /// Deductions beyond what was earned: the payslip stops at zero and this
    /// carries into the next one as a debt (PAY-12).
    pub carry_out_piastres: i64,
    /// Line-by-line, so a preview can name each deduction rather than showing a
    /// lump sum nobody can argue with.
    pub breakdown: serde_json::Value,
    /// How much of each advance this payslip would collect. Applied by the
    /// generator; ignored by the preview.
    #[serde(skip)]
    #[schema(ignore)]
    pub advance_applications: Vec<(Uuid, i64)>,
}

/// The whole run's figures, added up by the server (AT-3).
#[derive(Debug, Serialize, Deserialize, Clone, Default, ToSchema)]
pub struct PayrollTotals {
    pub people: i64,
    pub base_piastres: i64,
    pub overtime_piastres: i64,
    pub bonuses_piastres: i64,
    pub deductions_piastres: i64,
    pub advances_piastres: i64,
    pub net_piastres: i64,
    pub carry_out_piastres: i64,
}

impl PayrollTotals {
    pub fn of_computed(slips: &[ComputedPayslip]) -> Self {
        let mut t = Self::default();
        for s in slips {
            t.people += 1;
            t.base_piastres += s.base_piastres;
            t.overtime_piastres += s.overtime_piastres;
            t.bonuses_piastres += s.bonuses_piastres;
            t.deductions_piastres += s.deductions_piastres;
            t.advances_piastres += s.advance_installment_piastres;
            t.net_piastres += s.net_piastres;
            t.carry_out_piastres += s.carry_out_piastres;
        }
        t
    }

    pub fn of_payslips(slips: &[Payslip]) -> Self {
        let mut t = Self::default();
        for s in slips {
            t.people += 1;
            t.base_piastres += s.base_salary_piastres;
            t.overtime_piastres += s.overtime_piastres;
            t.bonuses_piastres += s.bonuses_piastres;
            t.deductions_piastres += s.deductions_piastres;
            t.advances_piastres += s.advance_installment_piastres;
            t.net_piastres += s.net_piastres;
            t.carry_out_piastres += s.carry_out_piastres;
        }
        t
    }
}

/// The rules of every branch of the org, loaded once per run (RU-2).
async fn rules_by_branch(
    conn: &mut sqlx::PgConnection,
    org_id: Uuid,
) -> Result<HashMap<Option<Uuid>, AttendanceSettings>, AppError> {
    let branches: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM branches WHERE org_id = $1 AND deleted_at IS NULL")
            .bind(org_id)
            .fetch_all(&mut *conn)
            .await?;
    let mut out = HashMap::new();
    out.insert(None, load_settings(&mut *conn, org_id, None).await?);
    for b in branches {
        let s = load_settings(&mut *conn, org_id, Some(b)).await?;
        out.insert(Some(b), s);
    }
    Ok(out)
}

/// Compute every payslip for a window without writing anything. With
/// `only`, just that person's (the app's estimate, PAY-9).
pub(crate) async fn compute_payslips(
    conn: &mut sqlx::PgConnection,
    org_id: Uuid,
    start_date: NaiveDate,
    end_date: NaiveDate,
    settings: &AttendanceSettings,
    only: Option<Uuid>,
) -> Result<Vec<ComputedPayslip>, AppError> {
    // ── Everyone on payroll who was employed during the window ──
    #[derive(sqlx::FromRow)]
    struct Staff {
        employee_id: Uuid,
        name: String,
        base_salary_piastres: i64,
        hire_date: Option<NaiveDate>,
        termination_date: Option<NaiveDate>,
    }
    let staff: Vec<Staff> = sqlx::query_as(
        "SELECT p.id AS employee_id, p.name, p.base_salary_piastres, p.hire_date, \
                p.termination_date \
           FROM employees p \
          WHERE p.org_id = $1 \
            AND p.on_payroll \
            AND p.employment_status <> 'suspended' \
            AND (p.hire_date        IS NULL OR p.hire_date        <= $3) \
            AND (p.termination_date IS NULL OR p.termination_date >= $2) \
            AND ($4::uuid IS NULL OR p.id = $4) \
          ORDER BY lower(p.name)",
    )
    .bind(org_id)
    .bind(start_date)
    .bind(end_date)
    .bind(only)
    .fetch_all(&mut *conn)
    .await?;
    if staff.is_empty() {
        return Ok(Vec::new());
    }

    // ── Salary history (PAY-13) ──────────────────────────────────
    let history_rows: Vec<(Uuid, NaiveDate, i64)> = sqlx::query_as(
        "SELECT employee_id, effective_from, base_salary_piastres FROM employee_salary_history \
          WHERE org_id = $1 AND ($2::uuid IS NULL OR employee_id = $2) \
          ORDER BY employee_id, effective_from",
    )
    .bind(org_id)
    .bind(only)
    .fetch_all(&mut *conn)
    .await?;
    let mut history: HashMap<Uuid, Vec<(NaiveDate, i64)>> = HashMap::new();
    for (e, from, s) in history_rows {
        history.entry(e).or_default().push((from, s));
    }

    // ── Every attendance record in the window, priced one by one ─
    #[derive(sqlx::FromRow)]
    struct Rec {
        employee_id: Uuid,
        branch_id: Uuid,
        work_shift_id: Option<Uuid>,
        business_date: NaiveDate,
        status: String,
        late_minutes: i32,
        worked_minutes: i32,
        overtime_minutes: i32,
        overtime_status: Option<String>,
        night_overtime_minutes: i64,
        scheduled_minutes: Option<i32>,
        cover_status: Option<String>,
        is_cover: bool,
        holiday: bool,
        unpaid_leave: bool,
        shift_ot_day: Option<Decimal>,
        shift_ot_night: Option<Decimal>,
    }
    let recs: Vec<Rec> = sqlx::query_as(
        r#"
        SELECT a.employee_id, a.branch_id, a.work_shift_id, a.business_date, a.status, a.late_minutes,
               COALESCE(a.worked_minutes, 0) AS worked_minutes,
               COALESCE(a.overtime_minutes, 0) AS overtime_minutes, a.overtime_status,
               COALESCE(dawam_night_minutes(a.scheduled_end_at, a.check_out_at, br.timezone::text, $5, $6), 0)::bigint
                   AS night_overtime_minutes,
               (EXTRACT(EPOCH FROM (a.scheduled_end_at - a.scheduled_start_at)) / 60)::int AS scheduled_minutes,
               a.cover_status,
               a.covered_employee_id IS NOT NULL AS is_cover,
               h.on_date IS NOT NULL AS holiday,
               EXISTS (
                   SELECT 1 FROM staff_requests r
                     LEFT JOIN leave_types lt ON lt.id = r.leave_type_id
                    WHERE r.employee_id = a.employee_id AND r.kind = 'leave'
                      AND r.status = 'approved' AND NOT COALESCE(r.is_paid, lt.is_paid, true)
                      AND r.on_date <= a.business_date
                      AND COALESCE(r.end_date, r.on_date) >= a.business_date
               ) AS unpaid_leave,
               ws.ot_day_multiplier AS shift_ot_day, ws.ot_night_multiplier AS shift_ot_night
          FROM attendance_records a
          JOIN branches br ON br.id = a.branch_id
          LEFT JOIN work_shifts ws ON ws.id = a.work_shift_id
          LEFT JOIN staff_holidays h ON h.org_id = a.org_id AND h.on_date = a.business_date
                                    AND h.decision = 'holiday'
         WHERE a.org_id = $1 AND a.business_date BETWEEN $2 AND $3
           AND ($4::uuid IS NULL OR a.employee_id = $4)
         ORDER BY a.employee_id, a.business_date
        "#,
    )
    .bind(org_id)
    .bind(start_date)
    .bind(end_date)
    .bind(only)
    .bind(settings.night_start)
    .bind(settings.night_end)
    .fetch_all(&mut *conn)
    .await?;

    let rules = rules_by_branch(&mut *conn, org_id).await?;
    // Every rostered minute of each person's day, from THE roster function,
    // is the minute rate's divisor (RU-5, RU-6) — exactly as the sweep and the
    // overtime approval build it (AT-9).
    let ids: Vec<Uuid> = staff.iter().map(|p| p.employee_id).collect();
    let rostered =
        crate::staff::penalties::rostered_by_day(&mut *conn, &ids, start_date, end_date, None)
            .await?;

    #[derive(Default)]
    struct Totals {
        worked_days: Decimal,
        absent_days: Decimal,
        leave_days: Decimal,
        late_minutes: i64,
        overtime_minutes: i64,
        night_overtime_minutes: i64,
        overtime_piastres: i64,
        cover_piastres: i64,
        holiday_piastres: i64,
        /// Per-shift overtime lines, for the breakdown.
        shifts: Vec<serde_json::Value>,
    }
    let mut totals: HashMap<Uuid, Totals> = HashMap::new();
    // A date counts once, by its best block (SC-11, E2E B-ROTA-6): a split day
    // with one block worked is one worked day, not a worked day AND an absent
    // one; missing the other block costs its share through its own deduction
    // line. Rank: worked (present/late) > half day > leave > absent.
    let mut day_rank: HashMap<(Uuid, NaiveDate), u8> = HashMap::new();
    for r in &recs {
        let t = totals.entry(r.employee_id).or_default();
        let hist = history
            .get(&r.employee_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let fallback = staff
            .iter()
            .find(|p| p.employee_id == r.employee_id)
            .map_or(0, |p| p.base_salary_piastres);
        let salary = pricing::salary_on(hist, r.business_date, fallback);
        let branch_rules = rules
            .get(&Some(r.branch_id))
            .or_else(|| rules.get(&None))
            .unwrap_or(settings);
        let shift_rules = ShiftRules::from_settings(branch_rules, r.shift_ot_day, r.shift_ot_night);
        let status = crate::staff::rules::AttendanceStatus::parse(&r.status)?;
        let scheduled_minutes = r
            .scheduled_minutes
            .map(i64::from)
            .filter(|m| *m > 0)
            .unwrap_or(DEFAULT_SHIFT_MINUTES);
        let day_minutes = if r.is_cover {
            scheduled_minutes
        } else {
            pricing::day_minutes_of(
                rostered
                    .get(&(r.employee_id, r.business_date))
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
                r.work_shift_id,
                scheduled_minutes,
            )
        };
        let facts = ShiftFacts {
            base_salary_piastres: salary,
            scheduled_minutes,
            day_minutes,
            status,
            // The deduction side (absence, unpaid leave, late, excused time) is
            // read from the rows the sweep wrote with this same function; the
            // leave facts here only keep the day's earnings honest.
            leave_minutes: if status == crate::staff::rules::AttendanceStatus::OnLeave {
                scheduled_minutes
            } else {
                0
            },
            leave_paid: !r.unpaid_leave,
            unpaid_excused_minutes: 0,
            late_minutes: i64::from(r.late_minutes),
            worked_minutes: i64::from(r.worked_minutes),
            overtime_minutes: i64::from(r.overtime_minutes),
            night_overtime_minutes: r.night_overtime_minutes,
            overtime_status: r.overtime_status.clone(),
            is_confirmed_cover: r.is_cover && r.cover_status.as_deref() == Some("confirmed"),
            is_other_cover: r.is_cover && r.cover_status.as_deref() != Some("confirmed"),
            holiday: r.holiday,
        };
        let price = pricing::price_shift(&facts, &shift_rules);
        if !r.is_cover {
            use crate::staff::rules::AttendanceStatus as S;
            let rank = match status {
                S::Present | S::Late => 3,
                S::HalfDay => 2,
                S::OnLeave => 1,
                S::Absent => 0,
            };
            day_rank
                .entry((r.employee_id, r.business_date))
                .and_modify(|best| *best = (*best).max(rank))
                .or_insert(rank);
            t.late_minutes += i64::from(r.late_minutes);
        }
        t.overtime_minutes += price.overtime_minutes;
        t.night_overtime_minutes += price.night_overtime_minutes;
        t.overtime_piastres += price.overtime_piastres;
        t.cover_piastres += price.cover_piastres;
        t.holiday_piastres += price.holiday_piastres;
        if price.overtime_piastres > 0 {
            t.shifts.push(json!({
                "date": r.business_date, "branch_id": r.branch_id,
                "minutes": price.overtime_minutes, "night_minutes": price.night_overtime_minutes,
                "piastres": price.overtime_piastres,
                "day_multiplier": shift_rules.overtime_day_multiplier,
                "night_multiplier": shift_rules.overtime_night_multiplier,
                "scheduled_minutes": facts.scheduled_minutes,
                "salary_piastres": salary,
            }));
        }
    }
    for ((employee_id, _), rank) in day_rank {
        let t = totals.entry(employee_id).or_default();
        match rank {
            3 => t.worked_days += Decimal::ONE,
            2 => t.worked_days += Decimal::new(5, 1),
            1 => t.leave_days += Decimal::ONE,
            _ => t.absent_days += Decimal::ONE,
        }
    }

    // ── Approved adjustments in the window ───────────────────────
    #[derive(sqlx::FromRow)]
    struct AdjRow {
        id: Uuid,
        employee_id: Uuid,
        amount_piastres: Option<i64>,
        percent_of_base: Option<Decimal>,
        reason: String,
        source: String,
        effective_date: NaiveDate,
        recurring: bool,
        waived: bool,
        reason_code: Option<String>,
        reason_vars: Option<serde_json::Value>,
    }
    async fn load_adjustments(
        conn: &mut sqlx::PgConnection,
        table: &str,
        org_id: Uuid,
        from: NaiveDate,
        to: NaiveDate,
        only: Option<Uuid>,
    ) -> Result<HashMap<Uuid, Vec<AdjRow>>, AppError> {
        // A waived deduction stays on the payslip, struck through and counted
        // for nothing (AD-8): the decision is final and visible. The bonuses
        // table has no waive columns, hence the per-table column.
        let waived = if table == "payroll_deductions" {
            "waived_at IS NOT NULL"
        } else {
            "false"
        };
        // Only deductions carry the server's reason codes (AT-13).
        let codes = if table == "payroll_deductions" {
            "reason_code, reason_vars"
        } else {
            "NULL::text AS reason_code, NULL::jsonb AS reason_vars"
        };
        let rows: Vec<AdjRow> = sqlx::query_as(&format!(
            // A recurring allowance or deduction counts in every period from
            // its start month until stopped (AD-3): `ends_on` applies whether
            // the line started this period or earlier.
            "SELECT id, employee_id, amount_piastres, percent_of_base, reason, source, \
                    effective_date, recurring, {waived} AS waived, {codes} FROM {table} \
              WHERE org_id = $1 AND status = 'approved' \
                AND ($4::uuid IS NULL OR employee_id = $4) \
                AND effective_date <= $3 \
                AND (effective_date >= $2 OR recurring) \
                AND (NOT recurring OR ends_on IS NULL OR ends_on >= $2)"
        ))
        .bind(org_id)
        .bind(from)
        .bind(to)
        .bind(only)
        .fetch_all(&mut *conn)
        .await?;
        let mut map: HashMap<Uuid, Vec<AdjRow>> = HashMap::new();
        for row in rows {
            map.entry(row.employee_id).or_default().push(row);
        }
        Ok(map)
    }
    let bonus_rows = load_adjustments(
        &mut *conn,
        "payroll_bonuses",
        org_id,
        start_date,
        end_date,
        only,
    )
    .await?;
    let deduction_rows = load_adjustments(
        &mut *conn,
        "payroll_deductions",
        org_id,
        start_date,
        end_date,
        only,
    )
    .await?;

    let window_days = (end_date - start_date).num_days() + 1;
    let mut out = Vec::with_capacity(staff.len());
    for person in &staff {
        let attendance = totals.remove(&person.employee_id).unwrap_or_default();
        let hist = history
            .get(&person.employee_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        // The salary in force at the window's end prices percent lines.
        let salary_now = pricing::salary_on(hist, end_date, person.base_salary_piastres);

        let resolve = |rows: Option<&Vec<AdjRow>>| -> (i64, Vec<serde_json::Value>) {
            let mut total = 0i64;
            let mut lines = Vec::new();
            for row in rows.map(Vec::as_slice).unwrap_or(&[]) {
                let amount = resolve_adjustment_piastres(
                    row.amount_piastres,
                    row.percent_of_base,
                    salary_now,
                );
                let mut line = json!({
                    "id": row.id, "reason": row.reason, "piastres": amount,
                    "source": row.source, "effective_date": row.effective_date,
                    "recurring": row.recurring,
                    // A stable code + figures for the server's own wording, so
                    // each client says it in its language (AT-13); null for a
                    // person's own words.
                    "reason_code": row.reason_code, "reason_vars": row.reason_vars,
                });
                if row.waived {
                    line["waived"] = json!(true);
                    lines.push(line);
                    continue;
                }
                total = total.saturating_add(amount);
                lines.push(line);
            }
            (total, lines)
        };
        let (mut bonus_total, mut bonus_lines) = resolve(bonus_rows.get(&person.employee_id));
        let (mut deduction_total, mut deduction_lines) =
            resolve(deduction_rows.get(&person.employee_id));

        // Joined, left or changed salary mid-period: paid by calendar days at
        // each day's rate (PAY-13).
        let from = person.hire_date.map_or(start_date, |h| h.max(start_date));
        let to = person
            .termination_date
            .map_or(end_date, |t| t.min(end_date));
        let paid_days = ((to - from).num_days() + 1).clamp(0, window_days);
        let base_salary = pricing::prorated_base(
            hist,
            person.base_salary_piastres,
            start_date,
            end_date,
            from,
            to,
        );

        if attendance.cover_piastres > 0 {
            bonus_total = bonus_total.saturating_add(attendance.cover_piastres);
            bonus_lines.push(json!({
                "id": null, "kind": "cover", "reason": "cover", "piastres": attendance.cover_piastres,
            }));
        }
        if attendance.holiday_piastres > 0 {
            bonus_total = bonus_total.saturating_add(attendance.holiday_piastres);
            bonus_lines.push(json!({
                "id": null, "kind": "holiday", "reason": "holiday", "piastres": attendance.holiday_piastres,
            }));
        }
        // Last APPROVED payslip's shortfall is this one's first deduction
        // (PAY-12). A reopened month has no payslips, so it cannot leak a
        // stale carry.
        let carry_in: i64 = sqlx::query_scalar(&format!(
            "SELECT s.carry_out_piastres FROM payslips s \
               JOIN payroll_periods pp ON pp.id = s.payroll_period_id \
              WHERE s.employee_id = $1 AND pp.org_id = $2 AND pp.end_date < $3 \
                AND pp.status IN ({}) \
              ORDER BY pp.end_date DESC LIMIT 1",
            period_lock::CLOSED
        ))
        .bind(person.employee_id)
        .bind(org_id)
        .bind(start_date)
        .fetch_optional(&mut *conn)
        .await?
        .unwrap_or(0);
        if carry_in > 0 {
            deduction_total = deduction_total.saturating_add(carry_in);
            deduction_lines.push(
                json!({ "id": null, "kind": "carry", "reason": "carry", "piastres": carry_in }),
            );
        }

        // Live advances, oldest first — the earliest debt is repaid first.
        #[derive(sqlx::FromRow)]
        struct Advance {
            id: Uuid,
            monthly_installment_piastres: i64,
            remaining_piastres: i64,
        }
        let advances: Vec<Advance> = sqlx::query_as(
            "SELECT id, monthly_installment_piastres, remaining_piastres \
               FROM salary_advances \
              WHERE employee_id = $1 AND org_id = $2 AND status = 'approved' \
                AND remaining_piastres > 0 \
              ORDER BY created_at",
        )
        .bind(person.employee_id)
        .bind(org_id)
        .fetch_all(&mut *conn)
        .await?;
        let wanted: i64 = advances
            .iter()
            .map(|a| a.monthly_installment_piastres.min(a.remaining_piastres))
            .sum();

        // Every shift is already priced; the payslip only adds up, with the two
        // guards that keep it payable (deductions never past earnings, the
        // advance only out of what is left).
        let settled = pricing::settle_net(
            base_salary,
            attendance.overtime_piastres,
            bonus_total,
            deduction_total,
            wanted,
        );
        let deductions = settled.deductions_piastres;
        let advance = settled.advance_piastres;
        let net = settled.net_piastres;
        let capped = settled.capped_piastres;

        // Distribute whatever the payslip could actually afford across the
        // advances in order, so a partial collection settles the oldest debt
        // first and the rest stays owed.
        let mut left = advance;
        let mut advance_applications = Vec::new();
        let mut advance_lines = Vec::new();
        for a in &advances {
            if left <= 0 {
                break;
            }
            let take = a
                .monthly_installment_piastres
                .min(a.remaining_piastres)
                .min(left);
            if take <= 0 {
                continue;
            }
            left -= take;
            advance_applications.push((a.id, take));
            advance_lines.push(json!({ "id": a.id, "applied_piastres": take }));
        }

        out.push(ComputedPayslip {
            employee_id: person.employee_id,
            name: person.name.clone(),
            base_salary_piastres: salary_now,
            worked_days: attendance.worked_days,
            absent_days: attendance.absent_days,
            leave_days: attendance.leave_days,
            late_minutes: attendance.late_minutes,
            overtime_minutes: attendance.overtime_minutes,
            base_piastres: base_salary,
            overtime_piastres: attendance.overtime_piastres,
            bonuses_piastres: settled.bonuses_piastres,
            deductions_piastres: deductions,
            advance_installment_piastres: advance,
            net_piastres: net,
            carry_out_piastres: capped,
            breakdown: json!({
                "bonuses": bonus_lines,
                "deductions": deduction_lines,
                "advances": advance_lines,
                "overtime_shifts": attendance.shifts,
                "night_overtime_minutes": attendance.night_overtime_minutes,
                "paid_days": paid_days,
                "window_days": window_days,
                "salary_piastres": salary_now,
                "working_days_per_month": settings.working_days_per_month,
                // Deductions past what was earned: the lines above add up to
                // more than the net; this is the part that carried (PAY-12).
                "capped_piastres": capped,
            }),
            advance_applications,
        });
    }
    Ok(out)
}

/// Approve a DRAFT month: freeze every payslip and collect the advance
/// installments in the ledger. An approved month is not regenerated — it is
/// reopened (before anyone is paid) and approved again.
#[utoipa::path(
    post, path = "/staff/payroll/periods/{id}/generate", tag = "staff",
    params(("id" = Uuid, Path, description = "Period ID")),
    responses(
        (status = 200, description = "Payslips generated", body = Vec<Payslip>),
        (status = 409, description = "Only a draft period is approved; reopen first"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn generate_period(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    // Approving payroll is the payroll run, held for every branch (RO-9).
    access::require_everywhere(pool.get_ref(), &claims, org_id, Cap::HrPayrollRun).await?;
    let actor = claims.user_id_safe().ok();

    #[derive(sqlx::FromRow)]
    struct Period {
        start_date: NaiveDate,
        end_date: NaiveDate,
        status: String,
    }

    let mut tx = pool.begin().await?;
    let period: Period = sqlx::query_as(
        "SELECT start_date, end_date, status FROM payroll_periods \
          WHERE id = $1 AND org_id = $2 FOR UPDATE",
    )
    .bind(*id)
    .bind(org_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound("Payroll period not found".into()))?;

    if period.status != "draft" {
        return Err(AppError::Conflict(format!(
            "A {} period is frozen — reopen it (before anyone is paid) to approve it again",
            period.status
        )));
    }
    // A draft never holds payslips after a reopen; clear any older leftovers
    // (their collections cascade and the ledger refunds them).
    sqlx::query("DELETE FROM payslips WHERE payroll_period_id = $1")
        .bind(*id)
        .execute(&mut *tx)
        .await?;

    let settings = load_settings(&mut *tx, org_id, None).await?;
    let computed = compute_payslips(
        &mut tx,
        org_id,
        period.start_date,
        period.end_date,
        &settings,
        None,
    )
    .await?;

    let mut employee_count = 0i32;
    let mut grand_total = 0i64;

    for slip in &computed {
        // A payslip with nothing on it — no pay, no lines — is marked paid by
        // the run itself, so it never blocks the month reaching Paid (PAY-7).
        let empty = slip.net_piastres == 0
            && slip.base_piastres == 0
            && slip.overtime_piastres == 0
            && slip.bonuses_piastres == 0
            && slip.deductions_piastres == 0
            && slip.advance_installment_piastres == 0;
        let payslip_id: Uuid = sqlx::query_scalar(
            "INSERT INTO payslips (
                 org_id, payroll_period_id, employee_id, base_salary_piastres, worked_days,
                 absent_days, leave_days, late_minutes, overtime_minutes, overtime_piastres,
                 bonuses_piastres, deductions_piastres, advance_installment_piastres,
                 net_piastres, breakdown, carry_out_piastres, paid_method, paid_at, paid_by
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                       CASE WHEN $17 THEN 'none' END, CASE WHEN $17 THEN now() END,
                       CASE WHEN $17 THEN $18 END)
             RETURNING id",
        )
        .bind(org_id)
        .bind(*id)
        .bind(slip.employee_id)
        .bind(slip.base_piastres)
        .bind(slip.worked_days)
        .bind(slip.absent_days)
        .bind(slip.leave_days)
        .bind(slip.late_minutes as i32)
        .bind(slip.overtime_minutes as i32)
        .bind(slip.overtime_piastres)
        .bind(slip.bonuses_piastres)
        .bind(slip.deductions_piastres)
        .bind(slip.advance_installment_piastres)
        .bind(slip.net_piastres)
        .bind(&slip.breakdown)
        .bind(slip.carry_out_piastres)
        .bind(empty)
        .bind(actor)
        .fetch_one(&mut *tx)
        .await?;

        // Collect the installments this payslip affords, in the ledger. The
        // trigger derives `remaining_piastres`; deleting the payslip later
        // deletes these rows and the money comes back (AV-6).
        for (advance_id, take) in &slip.advance_applications {
            sqlx::query(
                "INSERT INTO salary_advance_collections \
                     (org_id, advance_id, payslip_id, period_id, employee_id, amount_piastres) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(org_id)
            .bind(advance_id)
            .bind(payslip_id)
            .bind(*id)
            .bind(slip.employee_id)
            .bind(take)
            .execute(&mut *tx)
            .await?;
        }

        employee_count += 1;
        grand_total = grand_total.saturating_add(slip.net_piastres);
    }

    sqlx::query(
        "UPDATE payroll_periods SET status = 'generated', employee_count = $2, \
             total_net_piastres = $3, generated_at = now(), generated_by = $4, updated_at = now() \
          WHERE id = $1",
    )
    .bind(*id)
    .bind(employee_count)
    .bind(grand_total)
    .bind(actor)
    .execute(&mut *tx)
    .await?;
    // Nothing to pay anyone? Then everyone is "paid" and the month is Paid.
    settle_period_if_all_paid(&mut tx, *id).await?;
    audit(
        &mut *tx,
        org_id,
        actor,
        "period.generate",
        "payroll_periods",
        Some(*id),
        None,
        Some(*id),
        None,
        json!({ "people": employee_count, "total_net_piastres": grand_total }),
    )
    .await?;
    tx.commit().await?;
    // AT-4: an approved month's exact coordinates go now (clocking).
    crate::staff::dawam::privacy::wipe_period_coordinates(
        pool.get_ref(),
        org_id,
        period.start_date,
        period.end_date,
    )
    .await?;

    let slips = sqlx::query_as::<_, Payslip>(&format!(
        "{PAYSLIP_SELECT} WHERE s.payroll_period_id = $1 ORDER BY lower(e.name)"
    ))
    .bind(*id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(slips))
}

/// The month is Paid when every payslip is (PAY-7).
pub(crate) async fn settle_period_if_all_paid(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    period_id: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE payroll_periods SET status = 'paid', paid_at = now(), updated_at = now() \
          WHERE id = $1 AND status = 'generated' \
            AND EXISTS (SELECT 1 FROM payslips WHERE payroll_period_id = $1) \
            AND NOT EXISTS (SELECT 1 FROM payslips WHERE payroll_period_id = $1 AND paid_at IS NULL)",
    )
    .bind(period_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[utoipa::path(
    get, path = "/staff/payroll/periods/{id}/preview", tag = "staff",
    params(("id" = Uuid, Path, description = "Period ID")),
    responses(
        (status = 200, description = "What generating would produce", body = Vec<ComputedPayslip>),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn preview_period(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    // READ, not run: seeing what payroll would do is not running it. It shows
    // everyone's pay, so it needs payroll read for every branch.
    access::require_everywhere(pool.get_ref(), &claims, org_id, Cap::HrPayrollRead).await?;

    let period: (NaiveDate, NaiveDate) = sqlx::query_as(
        "SELECT start_date, end_date FROM payroll_periods WHERE id = $1 AND org_id = $2",
    )
    .bind(*id)
    .bind(org_id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Payroll period not found".into()))?;

    let settings = load_settings(pool.get_ref(), org_id, None).await?;
    let mut conn = pool.acquire().await?;
    let computed = compute_payslips(&mut conn, org_id, period.0, period.1, &settings, None).await?;
    Ok(HttpResponse::Ok().json(computed))
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct ExportQuery {
    /// `bank` (a transfer file: name, account, amount) · `wallet` (numbers
    /// and amounts) · `cash`; omitted = everyone, every figure (PAY-8).
    #[serde(default)]
    pub method: Option<String>,
}

/// The generated period as a CSV: the bank file, the wallet list, or the
/// whole run.
///
/// Deliberately serves the PAYSLIPS, not a fresh computation: the file handed to
/// a bank must be exactly what was approved, even if a deduction has been edited
/// since. A period that has not been generated has nothing to export.
#[utoipa::path(
    get, path = "/staff/payroll/periods/{id}/export.csv", tag = "staff",
    params(("id" = Uuid, Path, description = "Period ID"), ExportQuery),
    responses(
        (status = 200, description = "CSV of the period's payslips", content_type = "text/csv"),
        (status = 409, description = "The period has not been generated yet"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn export_period_csv(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    query: web::Query<ExportQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::require_everywhere(pool.get_ref(), &claims, org_id, Cap::HrPayrollRead).await?;
    let method = query
        .method
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    if let Some(m) = method
        && !matches!(m, "bank" | "wallet" | "cash")
    {
        return Err(AppError::BadRequest(
            "method is bank, wallet or cash".into(),
        ));
    }

    let period: (String, String) =
        sqlx::query_as("SELECT name, status FROM payroll_periods WHERE id = $1 AND org_id = $2")
            .bind(*id)
            .bind(org_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound("Payroll period not found".into()))?;
    if period.1 == "draft" {
        return Err(AppError::Conflict(
            "Generate the period before exporting it".into(),
        ));
    }

    let slips = sqlx::query_as::<_, Payslip>(&format!(
        "{PAYSLIP_SELECT} WHERE s.payroll_period_id = $1 \
            AND ($2::text IS NULL OR e.pay_method = $2) \
          ORDER BY lower(e.name)"
    ))
    .bind(*id)
    .bind(method)
    .fetch_all(pool.get_ref())
    .await?;

    // Amounts are written in MAJOR units with two decimals — piastres are an
    // internal representation, and a bank importing "480000" would pay a
    // hundredfold. Every field is quoted and inner quotes doubled, so a name
    // containing a comma cannot shift a column.
    let esc = |value: &str| format!("\"{}\"", value.replace('"', "\"\""));
    let money = |piastres: i64| format!("{}.{:02}", piastres / 100, (piastres % 100).abs());

    let mut csv = String::new();
    match method {
        // The bank file: who, where, how much. Nothing a bank does not need.
        Some("bank") => {
            csv.push_str("employee,account,amount\n");
            for slip in slips.iter().filter(|s| s.net_piastres > 0) {
                csv.push_str(&format!(
                    "{},{},{}\n",
                    esc(slip.employee_name.as_deref().unwrap_or("")),
                    esc(slip.pay_account.as_deref().unwrap_or("")),
                    money(slip.net_piastres),
                ));
            }
        }
        Some("wallet") => {
            csv.push_str("employee,wallet_number,amount\n");
            for slip in slips.iter().filter(|s| s.net_piastres > 0) {
                csv.push_str(&format!(
                    "{},{},{}\n",
                    esc(slip.employee_name.as_deref().unwrap_or("")),
                    esc(slip.pay_account.as_deref().unwrap_or("")),
                    money(slip.net_piastres),
                ));
            }
        }
        _ => {
            csv.push_str(
                "employee,employee_id,pay_method,account,base,overtime,bonuses,deductions,advance,net,carry_out,worked_days,absent_days,paid_method\n",
            );
            for slip in &slips {
                csv.push_str(&format!(
                    "{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
                    esc(slip.employee_name.as_deref().unwrap_or("")),
                    esc(&slip.employee_id.to_string()),
                    esc(slip.pay_method.as_deref().unwrap_or("")),
                    esc(slip.pay_account.as_deref().unwrap_or("")),
                    money(slip.base_salary_piastres),
                    money(slip.overtime_piastres),
                    money(slip.bonuses_piastres),
                    money(slip.deductions_piastres),
                    money(slip.advance_installment_piastres),
                    money(slip.net_piastres),
                    money(slip.carry_out_piastres),
                    slip.worked_days,
                    slip.absent_days,
                    esc(slip.paid_method.as_deref().unwrap_or("")),
                ));
            }
        }
    }

    // A quoted filename: period names carry spaces ("July 2026").
    let filename = format!(
        "payroll-{}{}.csv",
        period.0.replace(['"', '\\', '/'], "-"),
        method.map(|m| format!("-{m}")).unwrap_or_default()
    );
    Ok(HttpResponse::Ok()
        .content_type("text/csv; charset=utf-8")
        .insert_header((
            "Content-Disposition",
            format!("attachment; filename=\"{filename}\""),
        ))
        .body(csv))
}

// ── Payslips ──────────────────────────────────────────────────

#[utoipa::path(
    get, path = "/staff/payroll/periods/{id}/payslips", tag = "staff",
    params(("id" = Uuid, Path, description = "Period ID")),
    responses((status = 200, description = "Payslips in the period", body = Vec<Payslip>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_payslips(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    let scope = access::scope(pool.get_ref(), &claims, org_id, Cap::HrPayrollRead).await?;

    let rows = sqlx::query_as::<_, Payslip>(&format!(
        "{PAYSLIP_SELECT} WHERE s.payroll_period_id = $1 AND s.org_id = $2 AND {} \
          ORDER BY lower(e.name)",
        access::in_scope("s.employee_id", 3)
    ))
    .bind(*id)
    .bind(org_id)
    .bind(scope.as_deref())
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    get, path = "/staff/me/payslips", tag = "staff",
    responses((status = 200, description = "The employee's own payslips", body = Vec<Payslip>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_payslips(me: Me, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    // Only periods that have actually been finalised: a draft regeneration would
    // otherwise flash half-computed numbers at the employee.
    let rows = sqlx::query_as::<_, Payslip>(&format!(
        "{PAYSLIP_SELECT} \
          WHERE s.employee_id = $1 AND pp.status IN ({}) \
          ORDER BY pp.start_date DESC",
        period_lock::CLOSED
    ))
    .bind(me.employee_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

/// The people whose money lines a caller may list: payroll readers for their
/// branches, and whoever holds any of `also` (the acts the list backs) for
/// theirs (audit B15: a manager must see the lines and advances they decide).
pub(crate) async fn money_scope(
    pool: &PgPool,
    claims: &Claims,
    org_id: Uuid,
    also: &[Cap],
) -> Result<Option<Vec<Uuid>>, AppError> {
    let mut union: BTreeMap<Uuid, ()> = BTreeMap::new();
    let mut any = false;
    let mut last_err = None;
    for cap in std::iter::once(Cap::HrPayrollRead).chain(also.iter().copied()) {
        match access::scope(pool, claims, org_id, cap).await {
            Ok(None) => return Ok(None),
            Ok(Some(at)) => {
                any = true;
                for b in at {
                    union.insert(b, ());
                }
            }
            Err(e) => last_err = Some(e),
        }
    }
    if any {
        Ok(Some(union.into_keys().collect()))
    } else {
        Err(last_err.unwrap_or_else(|| crate::authz::require::denied(Cap::HrPayrollRead)))
    }
}
