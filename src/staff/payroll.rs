//! Payroll: adjustments, advances, periods, and payslips.
//!
//!   Net = base + overtime + bonuses − deductions − advance installment
//!
//! A PAYSLIP IS A SNAPSHOT. Generating a period freezes every figure, including
//! the individual bonus/deduction/advance rows that fed it, into
//! `payslips.breakdown`. Editing an attendance record or adding a deduction
//! afterwards changes nothing until someone regenerates — which is only possible
//! while the period is still `draft` or `generated`. Once it is `paid` or
//! `closed`, the numbers are what was paid, and that is the point.
//!
//! REGENERATION IS REVERSIBLE. Generating collects installments against live
//! salary advances, which mutates `salary_advances.remaining_piastres`. Doing that
//! twice would collect the same money twice, so regeneration first REFUNDS every
//! installment recorded in the period's existing payslips, then deletes them, then
//! recomputes from scratch. The whole thing is one transaction.

use std::collections::HashMap;

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    errors::{AppError, AppErrorResponse},
    orgs::handlers::extract_claims,
    permissions::checker::check_permission,
    staff::{
        attendance::load_settings,
        requests::my_org,
        require_user_in_org,
        rules::{PayrollInputs, compute_net_salary, resolve_adjustment_piastres},
        scope_org, validate_decision,
    },
};

/// Fallback shift length when an employee has no scheduled window to average —
/// a standard eight-hour day. Only ever used as the per-minute divisor.
const DEFAULT_SHIFT_MINUTES: i64 = 480;

// ── Models ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct PayrollAdjustment {
    pub id: Uuid,
    pub org_id: Uuid,
    pub user_id: Uuid,
    #[sqlx(default)]
    pub user_name: Option<String>,
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
    pub user_id: Uuid,
    #[sqlx(default)]
    pub user_name: Option<String>,
    pub amount_piastres: i64,
    pub installments: i32,
    pub monthly_installment_piastres: i64,
    pub remaining_piastres: i64,
    pub reason: Option<String>,
    pub status: String,
    pub decided_by: Option<Uuid>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decision_note: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const ADVANCE_SELECT: &str = r#"
    SELECT a.id, a.org_id, a.user_id, u.name AS user_name, a.amount_piastres,
           a.installments, a.monthly_installment_piastres, a.remaining_piastres,
           a.reason, a.status, a.decided_by, a.decided_at, a.decision_note,
           a.created_at, a.updated_at
      FROM salary_advances a
      JOIN users u ON u.id = a.user_id
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

const PERIOD_COLS: &str = "id, org_id, name, start_date, end_date, status, employee_count, \
     total_net_piastres, generated_at, generated_by, paid_at, closed_at, created_at, updated_at";

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct Payslip {
    pub id: Uuid,
    pub org_id: Uuid,
    pub payroll_period_id: Uuid,
    pub user_id: Uuid,
    #[sqlx(default)]
    pub user_name: Option<String>,
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
    /// The period this covers, denormalised. A payslip identified only by its
    /// generation timestamp is unreadable — two months run on the same day would
    /// be indistinguishable to the employee looking at them.
    #[sqlx(default)]
    pub period_name: Option<String>,
    #[sqlx(default)]
    pub period_start: Option<NaiveDate>,
    #[sqlx(default)]
    pub period_end: Option<NaiveDate>,
}

const PAYSLIP_SELECT: &str = r#"
    SELECT s.id, s.org_id, s.payroll_period_id, s.user_id, u.name AS user_name,
           s.base_salary_piastres, s.worked_days, s.absent_days, s.leave_days,
           s.late_minutes, s.overtime_minutes, s.overtime_piastres, s.bonuses_piastres,
           s.deductions_piastres, s.advance_installment_piastres, s.net_piastres,
           s.breakdown, s.generated_at,
           pp.name AS period_name, pp.start_date AS period_start,
           pp.end_date AS period_end
      FROM payslips s
      JOIN users u ON u.id = s.user_id
      JOIN payroll_periods pp ON pp.id = s.payroll_period_id
"#;

// ── Requests ──────────────────────────────────────────────────

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateAdjustmentRequest {
    pub user_id: Uuid,
    /// Exactly one of `amount_piastres` or `percent_of_base`.
    #[serde(default)]
    pub amount_piastres: Option<i64>,
    #[serde(default)]
    pub percent_of_base: Option<Decimal>,
    pub reason: String,
    pub effective_date: NaiveDate,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateAdvanceRequest {
    /// Admin-only; omitted on `/staff/me/*`.
    #[serde(default)]
    pub user_id: Option<Uuid>,
    pub amount_piastres: i64,
    /// Defaults to 1 — repaid in full from the next payslip.
    #[serde(default)]
    pub installments: Option<i32>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Deciding a SALARY ADVANCE. Deliberately not called `DecisionRequest`: staff
/// requests have their own decision body carrying `is_paid`, and two structs
/// sharing a name collapse into one OpenAPI schema — which silently gave every
/// generated client the wrong shape for one of them.
#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct AdvanceDecision {
    /// `approved` | `rejected` | `cancelled`.
    pub status: String,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreatePeriodRequest {
    pub name: String,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct PeriodStatusRequest {
    /// `draft` | `generated` | `paid` | `closed`.
    pub status: String,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct AdjustmentQuery {
    #[serde(default)]
    pub user_id: Option<Uuid>,
    #[serde(default)]
    pub from: Option<NaiveDate>,
    #[serde(default)]
    pub to: Option<NaiveDate>,
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
        "SELECT a.id, a.org_id, a.user_id, u.name AS user_name, a.amount_piastres, \
                a.percent_of_base, a.reason, a.effective_date, a.source, a.status, \
                {overrides}, \
                a.created_by, a.created_at, a.updated_at \
           FROM {table} a JOIN users u ON u.id = a.user_id"
    )
}

async fn list_adjustments(
    req: &HttpRequest,
    pool: &crate::db::Db,
    query: &AdjustmentQuery,
    table: &str,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(req)?;
    check_permission(pool.get_ref(), &claims, "payroll", "read").await?;
    let org_id = scope_org(req, &claims)?;

    let rows = sqlx::query_as::<_, PayrollAdjustment>(&format!(
        "{} WHERE a.org_id = $1 \
             AND ($2::uuid IS NULL OR a.user_id = $2) \
             AND ($3::date IS NULL OR a.effective_date >= $3) \
             AND ($4::date IS NULL OR a.effective_date <= $4) \
           ORDER BY a.effective_date DESC, lower(u.name)",
        adjustment_select(table)
    ))
    .bind(org_id)
    .bind(query.user_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

async fn create_adjustment(
    req: &HttpRequest,
    pool: &crate::db::Db,
    body: &CreateAdjustmentRequest,
    table: &str,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(req)?;
    check_permission(pool.get_ref(), &claims, "payroll", "create").await?;
    let org_id = scope_org(req, &claims)?;
    require_user_in_org(pool.get_ref(), org_id, body.user_id).await?;

    let reason = body.reason.trim();
    if reason.is_empty() {
        return Err(AppError::BadRequest("A reason is required".into()));
    }
    match (body.amount_piastres, body.percent_of_base) {
        (Some(_), Some(_)) => {
            return Err(AppError::BadRequest(
                "Give either an amount or a percentage, not both".into(),
            ));
        }
        (None, None) => {
            return Err(AppError::BadRequest(
                "Give either an amount or a percentage".into(),
            ));
        }
        (Some(amount), None) if amount <= 0 => {
            return Err(AppError::BadRequest("Amount must be positive".into()));
        }
        (None, Some(percent)) if percent <= Decimal::ZERO => {
            return Err(AppError::BadRequest("Percentage must be positive".into()));
        }
        _ => {}
    }

    let id = sqlx::query_scalar::<_, Uuid>(&format!(
        "INSERT INTO {table} (org_id, user_id, amount_piastres, percent_of_base, reason, \
                              effective_date, source, status, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, 'manual', 'approved', $7) RETURNING id"
    ))
    .bind(org_id)
    .bind(body.user_id)
    .bind(body.amount_piastres)
    .bind(body.percent_of_base)
    .bind(reason)
    .bind(body.effective_date)
    .bind(claims.user_id())
    .fetch_one(pool.get_ref())
    .await?;

    let row = sqlx::query_as::<_, PayrollAdjustment>(&format!(
        "{} WHERE a.id = $1",
        adjustment_select(table)
    ))
    .bind(id)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Created().json(row))
}

async fn delete_adjustment(
    req: &HttpRequest,
    pool: &crate::db::Db,
    id: Uuid,
    table: &str,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(req)?;
    check_permission(pool.get_ref(), &claims, "payroll", "delete").await?;
    let org_id = scope_org(req, &claims)?;

    let deleted = sqlx::query(&format!(
        "DELETE FROM {table} WHERE id = $1 AND org_id = $2"
    ))
    .bind(id)
    .bind(org_id)
    .execute(pool.get_ref())
    .await?
    .rows_affected();
    if deleted == 0 {
        return Err(AppError::NotFound("Adjustment not found".into()));
    }
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
    post, path = "/staff/payroll/deductions", tag = "staff", request_body = CreateAdjustmentRequest,
    responses((status = 201, description = "Deduction created", body = PayrollAdjustment), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_deduction(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateAdjustmentRequest>,
) -> Result<HttpResponse, AppError> {
    create_adjustment(&req, &pool, &body, "payroll_deductions").await
}

#[utoipa::path(
    delete, path = "/staff/payroll/deductions/{id}", tag = "staff",
    params(("id" = Uuid, Path, description = "Deduction ID")),
    responses((status = 204, description = "Deduction deleted"), AppErrorResponse),
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
    post, path = "/staff/payroll/bonuses", tag = "staff", request_body = CreateAdjustmentRequest,
    responses((status = 201, description = "Bonus created", body = PayrollAdjustment), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_bonus(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateAdjustmentRequest>,
) -> Result<HttpResponse, AppError> {
    create_adjustment(&req, &pool, &body, "payroll_bonuses").await
}

#[utoipa::path(
    delete, path = "/staff/payroll/bonuses/{id}", tag = "staff",
    params(("id" = Uuid, Path, description = "Bonus ID")),
    responses((status = 204, description = "Bonus deleted"), AppErrorResponse),
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

#[utoipa::path(
    patch, path = "/staff/payroll/deductions/{id}/override", tag = "staff",
    params(("id" = Uuid, Path, description = "Deduction ID")),
    request_body = OverrideDeductionRequest,
    responses((status = 200, description = "Deduction overridden", body = PayrollAdjustment), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn override_deduction(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<OverrideDeductionRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "payroll", "update").await?;
    let org_id = scope_org(&req, &claims)?;

    let reason = body.reason.trim();
    if reason.is_empty() {
        return Err(AppError::BadRequest("An override needs a reason".into()));
    }
    if body.amount_piastres < 0 {
        return Err(AppError::BadRequest("Amount cannot be negative".into()));
    }

    // `original_amount_piastres` is only ever set from the CURRENT amount when it
    // is still NULL, so overriding twice does not lose what the rule first said.
    let updated = sqlx::query(
        "UPDATE payroll_deductions SET \
             original_amount_piastres = COALESCE(original_amount_piastres, amount_piastres), \
             amount_piastres = $3, overridden_at = now(), overridden_by = $4, \
             override_reason = $5, updated_at = now() \
          WHERE id = $1 AND org_id = $2",
    )
    .bind(*id)
    .bind(org_id)
    .bind(body.amount_piastres)
    .bind(claims.user_id())
    .bind(reason)
    .execute(pool.get_ref())
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(AppError::NotFound("Deduction not found".into()));
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

#[utoipa::path(
    patch, path = "/staff/payroll/deductions/{id}/waive", tag = "staff",
    params(("id" = Uuid, Path, description = "Deduction ID")),
    request_body = WaiveDeductionRequest,
    responses((status = 200, description = "Deduction waived", body = PayrollAdjustment), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn waive_deduction(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<WaiveDeductionRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "payroll", "update").await?;
    let org_id = scope_org(&req, &claims)?;

    let reason = body.reason.trim();
    if reason.is_empty() {
        return Err(AppError::BadRequest("A waiver needs a reason".into()));
    }

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
    .bind(claims.user_id())
    .bind(reason)
    .execute(pool.get_ref())
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(AppError::NotFound(
            "Deduction not found, or already waived".into(),
        ));
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
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "payroll", "read").await?;
    let org_id = scope_org(&req, &claims)?;

    let rows = sqlx::query_as::<_, SalaryAdvance>(&format!(
        "{ADVANCE_SELECT} WHERE a.org_id = $1 AND ($2::uuid IS NULL OR a.user_id = $2) \
          ORDER BY a.created_at DESC"
    ))
    .bind(org_id)
    .bind(query.user_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

async fn insert_advance(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    body: &CreateAdvanceRequest,
) -> Result<SalaryAdvance, AppError> {
    if body.amount_piastres <= 0 {
        return Err(AppError::BadRequest("Amount must be positive".into()));
    }
    let installments = body.installments.unwrap_or(1);
    if installments <= 0 {
        return Err(AppError::BadRequest(
            "Installments must be at least 1".into(),
        ));
    }
    // Round the installment UP so the final one is the small remainder rather
    // than leaving a few piastres outstanding forever.
    // Both operands are checked positive above, so the unsigned round-trip is
    // safe — and `div_ceil` is only stable for unsigned integers.
    let monthly = (body.amount_piastres as u64).div_ceil(installments as u64) as i64;

    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO salary_advances (org_id, user_id, amount_piastres, installments, \
                                      monthly_installment_piastres, remaining_piastres, reason) \
         VALUES ($1, $2, $3, $4, $5, $3, $6) RETURNING id",
    )
    .bind(org_id)
    .bind(user_id)
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

    Ok(
        sqlx::query_as::<_, SalaryAdvance>(&format!("{ADVANCE_SELECT} WHERE a.id = $1"))
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

#[utoipa::path(
    post, path = "/staff/payroll/advances", tag = "staff", request_body = CreateAdvanceRequest,
    responses((status = 201, description = "Advance created", body = SalaryAdvance), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_advance_admin(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateAdvanceRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "payroll", "create").await?;
    let org_id = scope_org(&req, &claims)?;
    let user_id = body
        .user_id
        .ok_or_else(|| AppError::BadRequest("user_id is required".into()))?;
    require_user_in_org(pool.get_ref(), org_id, user_id).await?;

    let row = insert_advance(pool.get_ref(), org_id, user_id, &body).await?;
    Ok(HttpResponse::Created().json(row))
}

#[utoipa::path(
    post, path = "/staff/me/advances", tag = "staff", request_body = CreateAdvanceRequest,
    responses((status = 201, description = "Advance requested", body = SalaryAdvance), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_my_advance(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateAdvanceRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let user_id = claims.user_id_safe()?;
    let org_id = my_org(pool.get_ref(), user_id).await?;

    let row = insert_advance(pool.get_ref(), org_id, user_id, &body).await?;
    Ok(HttpResponse::Created().json(row))
}

#[utoipa::path(
    get, path = "/staff/me/advances", tag = "staff",
    responses((status = 200, description = "The employee's own advances", body = Vec<SalaryAdvance>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_advances(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let user_id = claims.user_id_safe()?;
    my_org(pool.get_ref(), user_id).await?;

    let rows = sqlx::query_as::<_, SalaryAdvance>(&format!(
        "{ADVANCE_SELECT} WHERE a.user_id = $1 ORDER BY a.created_at DESC"
    ))
    .bind(user_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    patch, path = "/staff/payroll/advances/{id}/decision", tag = "staff",
    params(("id" = Uuid, Path, description = "Advance ID")),
    request_body = AdvanceDecision,
    responses((status = 200, description = "Decision recorded", body = SalaryAdvance), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn decide_advance(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<AdvanceDecision>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "payroll", "update").await?;
    let org_id = scope_org(&req, &claims)?;
    let decision = validate_decision(&body.status)?;

    let updated = sqlx::query(
        "UPDATE salary_advances SET status = $3, decided_by = $4, decided_at = now(), \
             decision_note = $5, updated_at = now() \
          WHERE id = $1 AND org_id = $2 AND status = 'pending'",
    )
    .bind(*id)
    .bind(org_id)
    .bind(decision)
    .bind(claims.user_id())
    .bind(
        body.note
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty()),
    )
    .execute(pool.get_ref())
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(AppError::Conflict(
            "This advance has already been decided".into(),
        ));
    }

    let row = sqlx::query_as::<_, SalaryAdvance>(&format!("{ADVANCE_SELECT} WHERE a.id = $1"))
        .bind(*id)
        .fetch_one(pool.get_ref())
        .await?;
    Ok(HttpResponse::Ok().json(row))
}

// ── Periods ───────────────────────────────────────────────────

#[utoipa::path(
    get, path = "/staff/payroll/periods", tag = "staff",
    responses((status = 200, description = "Payroll periods", body = Vec<PayrollPeriod>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_periods(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "payroll", "read").await?;
    let org_id = scope_org(&req, &claims)?;

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
    responses((status = 201, description = "Period created", body = PayrollPeriod), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_period(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreatePeriodRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "payroll", "create").await?;
    let org_id = scope_org(&req, &claims)?;

    let name = body.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("Period name is required".into()));
    }
    if body.end_date < body.start_date {
        return Err(AppError::BadRequest("End date is before start date".into()));
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

#[utoipa::path(
    patch, path = "/staff/payroll/periods/{id}/status", tag = "staff",
    params(("id" = Uuid, Path, description = "Period ID")),
    request_body = PeriodStatusRequest,
    responses((status = 200, description = "Status changed", body = PayrollPeriod), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn set_period_status(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<PeriodStatusRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "payroll", "update").await?;
    let org_id = scope_org(&req, &claims)?;

    let target = match body.status.as_str() {
        s @ ("draft" | "generated" | "paid" | "closed") => s,
        other => {
            return Err(AppError::BadRequest(format!(
                "Unknown period status '{other}' — expected draft, generated, paid, or closed"
            )));
        }
    };

    let current: String =
        sqlx::query_scalar("SELECT status FROM payroll_periods WHERE id = $1 AND org_id = $2")
            .bind(*id)
            .bind(org_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound("Payroll period not found".into()))?;

    // Money that has been paid does not become unpaid. Once a period is `paid`
    // the only move left is `closed`.
    let allowed = match current.as_str() {
        "draft" => matches!(target, "draft" | "generated"),
        "generated" => matches!(target, "draft" | "generated" | "paid"),
        "paid" => matches!(target, "paid" | "closed"),
        "closed" => target == "closed",
        _ => false,
    };
    if !allowed {
        return Err(AppError::Conflict(format!(
            "A {current} period cannot move to {target}"
        )));
    }

    let row = sqlx::query_as::<_, PayrollPeriod>(&format!(
        "UPDATE payroll_periods SET status = $3, \
             paid_at   = CASE WHEN $3 = 'paid'   THEN now() ELSE paid_at   END, \
             closed_at = CASE WHEN $3 = 'closed' THEN now() ELSE closed_at END, \
             updated_at = now() \
          WHERE id = $1 AND org_id = $2 RETURNING {PERIOD_COLS}"
    ))
    .bind(*id)
    .bind(org_id)
    .bind(target)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    delete, path = "/staff/payroll/periods/{id}", tag = "staff",
    params(("id" = Uuid, Path, description = "Period ID")),
    responses((status = 204, description = "Period deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_period(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "payroll", "delete").await?;
    let org_id = scope_org(&req, &claims)?;

    let mut tx = pool.begin().await?;
    let status: String = sqlx::query_scalar(
        "SELECT status FROM payroll_periods WHERE id = $1 AND org_id = $2 FOR UPDATE",
    )
    .bind(*id)
    .bind(org_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound("Payroll period not found".into()))?;
    if status == "paid" || status == "closed" {
        return Err(AppError::Conflict(format!(
            "A {status} period cannot be deleted"
        )));
    }

    // Deleting cascades to the payslips, so the advances they collected against
    // have to be refunded first or that money silently vanishes.
    refund_advances(&mut tx, *id).await?;
    sqlx::query("DELETE FROM payroll_periods WHERE id = $1 AND org_id = $2")
        .bind(*id)
        .bind(org_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(HttpResponse::NoContent().finish())
}

// ── Generation ────────────────────────────────────────────────

/// Attendance totals for one employee over the period.
#[derive(Debug, Default, Clone, Copy)]
struct AttendanceTotals {
    worked_days: Decimal,
    absent_days: Decimal,
    leave_days: Decimal,
    late_minutes: i64,
    overtime_minutes: i64,
    /// Average scheduled shift length, the per-minute pay divisor.
    scheduled_minutes: i64,
}

/// One employee's pay for a period, computed but not yet written.
///
/// PREVIEW AND GENERATE SHARE THIS. The preview endpoint exists so a manager can
/// see what payroll is about to do — a figure that would be worthless if it came
/// from a second implementation that could drift from the real one. So the
/// generator computes these first and then persists them, and the preview
/// computes exactly the same values and persists nothing.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ComputedPayslip {
    pub user_id: Uuid,
    pub name: String,
    pub base_salary_piastres: i64,
    pub worked_days: Decimal,
    pub absent_days: Decimal,
    pub leave_days: Decimal,
    pub late_minutes: i64,
    pub overtime_minutes: i64,
    /// After the attendance proration — what the days actually worked earn.
    pub base_piastres: i64,
    pub overtime_piastres: i64,
    pub bonuses_piastres: i64,
    pub deductions_piastres: i64,
    /// What the advances WANT versus what the payslip can afford differ when net
    /// pay would go negative; this is the affordable figure, the one collected.
    pub advance_installment_piastres: i64,
    pub net_piastres: i64,
    /// Line-by-line, so a preview can name each deduction rather than showing a
    /// lump sum nobody can argue with.
    pub breakdown: serde_json::Value,
    /// How much of each advance this payslip would collect. Applied by the
    /// generator; ignored by the preview.
    #[serde(skip)]
    #[schema(ignore)]
    pub advance_applications: Vec<(Uuid, i64)>,
}

/// Compute every payslip for a window without writing anything.
async fn compute_payslips(
    conn: &mut sqlx::PgConnection,
    org_id: Uuid,
    start_date: NaiveDate,
    end_date: NaiveDate,
    settings: &crate::staff::attendance::AttendanceSettings,
) -> Result<Vec<ComputedPayslip>, AppError> {
    // ── Everyone who was employed during the window ──────────────
    #[derive(sqlx::FromRow)]
    struct Staff {
        user_id: Uuid,
        name: String,
        base_salary_piastres: i64,
    }
    let staff: Vec<Staff> = sqlx::query_as(
        "SELECT p.user_id, u.name, p.base_salary_piastres \
           FROM staff_profiles p \
           JOIN users u ON u.id = p.user_id AND u.deleted_at IS NULL \
          WHERE p.org_id = $1 \
            AND p.employment_status <> 'suspended' \
            AND (p.hire_date        IS NULL OR p.hire_date        <= $3) \
            AND (p.termination_date IS NULL OR p.termination_date >= $2) \
          ORDER BY lower(u.name)",
    )
    .bind(org_id)
    .bind(start_date)
    .bind(end_date)
    .fetch_all(&mut *conn)
    .await?;

    // ── Attendance totals, one query for the whole org ───────────
    #[derive(sqlx::FromRow)]
    struct TotalsRow {
        user_id: Uuid,
        worked_days: Decimal,
        absent_days: Decimal,
        leave_days: Decimal,
        late_minutes: i64,
        overtime_minutes: i64,
        avg_scheduled_minutes: Option<Decimal>,
    }
    let totals_rows: Vec<TotalsRow> = sqlx::query_as(
        r#"
        SELECT a.user_id,
               COALESCE(SUM(CASE WHEN a.status IN ('present','late') THEN 1
                                 WHEN a.status = 'half_day'          THEN 0.5
                                 ELSE 0 END), 0)::numeric                     AS worked_days,
               -- ::numeric on purpose. A SUM over integers comes back BIGINT,
               -- which does not decode into the Decimal these columns are, and
               -- the mismatch only surfaces once an org HAS attendance rows —
               -- i.e. in production, not on an empty test org.
               COALESCE(SUM(CASE WHEN a.status = 'absent'   THEN 1 ELSE 0 END), 0)::numeric AS absent_days,
               COALESCE(SUM(CASE WHEN a.status = 'on_leave' THEN 1 ELSE 0 END), 0)::numeric AS leave_days,
               COALESCE(SUM(a.late_minutes), 0)::bigint     AS late_minutes,
               COALESCE(SUM(a.overtime_minutes), 0)::bigint AS overtime_minutes,
               AVG(EXTRACT(EPOCH FROM (a.scheduled_end_at - a.scheduled_start_at)) / 60.0)
                   FILTER (WHERE a.scheduled_start_at IS NOT NULL
                             AND a.scheduled_end_at   IS NOT NULL)            AS avg_scheduled_minutes
          FROM attendance_records a
         WHERE a.org_id = $1 AND a.business_date BETWEEN $2 AND $3
         GROUP BY a.user_id
        "#,
    )
    .bind(org_id)
    .bind(start_date)
    .bind(end_date)
    .fetch_all(&mut *conn)
    .await?;

    let mut totals: HashMap<Uuid, AttendanceTotals> = HashMap::new();
    for row in totals_rows {
        totals.insert(
            row.user_id,
            AttendanceTotals {
                worked_days: row.worked_days,
                absent_days: row.absent_days,
                leave_days: row.leave_days,
                late_minutes: row.late_minutes,
                overtime_minutes: row.overtime_minutes,
                scheduled_minutes: row
                    .avg_scheduled_minutes
                    .and_then(|d| d.round().to_i64())
                    .filter(|m| *m > 0)
                    .unwrap_or(DEFAULT_SHIFT_MINUTES),
            },
        );
    }

    // ── Approved adjustments in the window ───────────────────────
    #[derive(sqlx::FromRow)]
    struct AdjRow {
        id: Uuid,
        user_id: Uuid,
        amount_piastres: Option<i64>,
        percent_of_base: Option<Decimal>,
        reason: String,
    }
    async fn load_adjustments(
        conn: &mut sqlx::PgConnection,
        table: &str,
        org_id: Uuid,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<HashMap<Uuid, Vec<AdjRow>>, AppError> {
        // `waived_at IS NULL` is what makes an override stick: a waived deduction
        // stays visible in the ledger but never reaches a payslip. The bonuses
        // table has no waive columns, hence the per-table predicate.
        let waive_filter = if table == "payroll_deductions" {
            "AND waived_at IS NULL"
        } else {
            ""
        };
        let rows: Vec<AdjRow> = sqlx::query_as(&format!(
            "SELECT id, user_id, amount_piastres, percent_of_base, reason FROM {table} \
              WHERE org_id = $1 AND status = 'approved' \
                AND effective_date BETWEEN $2 AND $3 {waive_filter}"
        ))
        .bind(org_id)
        .bind(from)
        .bind(to)
        .fetch_all(&mut *conn)
        .await?;
        let mut map: HashMap<Uuid, Vec<AdjRow>> = HashMap::new();
        for row in rows {
            map.entry(row.user_id).or_default().push(row);
        }
        Ok(map)
    }
    let bonus_rows =
        load_adjustments(&mut *conn, "payroll_bonuses", org_id, start_date, end_date).await?;
    let deduction_rows = load_adjustments(
        &mut *conn,
        "payroll_deductions",
        org_id,
        start_date,
        end_date,
    )
    .await?;

    let mut out = Vec::with_capacity(staff.len());
    for person in &staff {
        // No attendance rows at all (a new hire, or a month nobody clocked):
        // every total is zero, but the pay divisor still needs a sane day length.
        let attendance = totals
            .get(&person.user_id)
            .copied()
            .unwrap_or(AttendanceTotals {
                scheduled_minutes: DEFAULT_SHIFT_MINUTES,
                ..Default::default()
            });

        let resolve = |rows: Option<&Vec<AdjRow>>| -> (i64, Vec<serde_json::Value>) {
            let mut total = 0i64;
            let mut lines = Vec::new();
            for row in rows.map(Vec::as_slice).unwrap_or(&[]) {
                let amount = resolve_adjustment_piastres(
                    row.amount_piastres,
                    row.percent_of_base,
                    person.base_salary_piastres,
                );
                total = total.saturating_add(amount);
                lines.push(json!({
                    "id": row.id, "reason": row.reason, "piastres": amount,
                }));
            }
            (total, lines)
        };
        let (bonus_total, bonus_lines) = resolve(bonus_rows.get(&person.user_id));
        let (deduction_total, deduction_lines) = resolve(deduction_rows.get(&person.user_id));

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
              WHERE user_id = $1 AND org_id = $2 AND status = 'approved' \
                AND remaining_piastres > 0 \
              ORDER BY created_at",
        )
        .bind(person.user_id)
        .bind(org_id)
        .fetch_all(&mut *conn)
        .await?;
        let wanted: i64 = advances
            .iter()
            .map(|a| a.monthly_installment_piastres.min(a.remaining_piastres))
            .sum();

        let result = compute_net_salary(&PayrollInputs {
            base_salary_piastres: person.base_salary_piastres,
            working_days_per_month: settings.working_days_per_month,
            scheduled_minutes_per_day: attendance.scheduled_minutes,
            overtime_minutes: attendance.overtime_minutes,
            overtime_multiplier: settings.default_overtime_multiplier,
            bonuses_piastres: bonus_total,
            deductions_piastres: deduction_total,
            advance_installment_piastres: wanted,
        });

        // Distribute whatever the payslip could actually afford across the
        // advances in order, so a partial collection settles the oldest debt
        // first and the rest stays owed.
        let mut left = result.advance_installment_piastres;
        let mut advance_applications = Vec::new();
        let mut advance_lines = Vec::new();
        for advance in &advances {
            if left <= 0 {
                break;
            }
            let take = advance
                .monthly_installment_piastres
                .min(advance.remaining_piastres)
                .min(left);
            if take <= 0 {
                continue;
            }
            left -= take;
            advance_applications.push((advance.id, take));
            advance_lines.push(json!({ "id": advance.id, "applied_piastres": take }));
        }

        out.push(ComputedPayslip {
            user_id: person.user_id,
            name: person.name.clone(),
            base_salary_piastres: person.base_salary_piastres,
            worked_days: attendance.worked_days,
            absent_days: attendance.absent_days,
            leave_days: attendance.leave_days,
            late_minutes: attendance.late_minutes,
            overtime_minutes: attendance.overtime_minutes,
            base_piastres: result.base_piastres,
            overtime_piastres: result.overtime_piastres,
            bonuses_piastres: result.bonuses_piastres,
            deductions_piastres: result.deductions_piastres,
            advance_installment_piastres: result.advance_installment_piastres,
            net_piastres: result.net_piastres,
            breakdown: json!({
                "bonuses": bonus_lines,
                "deductions": deduction_lines,
                "advances": advance_lines,
                "overtime_multiplier": settings.default_overtime_multiplier,
                "scheduled_minutes_per_day": attendance.scheduled_minutes,
                "working_days_per_month": settings.working_days_per_month,
            }),
            advance_applications,
        });
    }
    Ok(out)
}

/// Give back every advance installment the period's existing payslips collected.
/// Called before a regeneration or a delete, inside the caller's transaction.
async fn refund_advances(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    period_id: Uuid,
) -> Result<(), AppError> {
    let breakdowns: Vec<serde_json::Value> =
        sqlx::query_scalar("SELECT breakdown FROM payslips WHERE payroll_period_id = $1")
            .bind(period_id)
            .fetch_all(&mut **tx)
            .await?;

    for breakdown in breakdowns {
        let Some(advances) = breakdown.get("advances").and_then(|a| a.as_array()) else {
            continue;
        };
        for entry in advances {
            let (Some(id), Some(applied)) = (
                entry
                    .get("id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok()),
                entry.get("applied_piastres").and_then(|v| v.as_i64()),
            ) else {
                continue;
            };
            sqlx::query(
                "UPDATE salary_advances \
                    SET remaining_piastres = LEAST(remaining_piastres + $2, amount_piastres), \
                        status = CASE WHEN status = 'settled' THEN 'approved' ELSE status END, \
                        updated_at = now() \
                  WHERE id = $1",
            )
            .bind(id)
            .bind(applied)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

#[utoipa::path(
    post, path = "/staff/payroll/periods/{id}/generate", tag = "staff",
    params(("id" = Uuid, Path, description = "Period ID")),
    responses(
        (status = 200, description = "Payslips generated", body = Vec<Payslip>),
        (status = 409, description = "A paid or closed period cannot be regenerated"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn generate_period(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "payroll", "create").await?;
    let org_id = scope_org(&req, &claims)?;

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

    if period.status == "paid" || period.status == "closed" {
        return Err(AppError::Conflict(format!(
            "A {} period cannot be regenerated — the payslips are what was paid",
            period.status
        )));
    }

    // Start from a clean slate: refund, then drop, then recompute.
    refund_advances(&mut tx, *id).await?;
    sqlx::query("DELETE FROM payslips WHERE payroll_period_id = $1")
        .bind(*id)
        .execute(&mut *tx)
        .await?;

    let settings = load_settings(pool.get_ref(), org_id, None).await?;
    let computed = compute_payslips(
        &mut tx,
        org_id,
        period.start_date,
        period.end_date,
        &settings,
    )
    .await?;

    let mut employee_count = 0i32;
    let mut grand_total = 0i64;

    for slip in &computed {
        // Collect the installments this payslip affords. Doing it here rather
        // than inside the computation is what keeps the preview side-effect free.
        for (advance_id, take) in &slip.advance_applications {
            sqlx::query(
                "UPDATE salary_advances \
                    SET remaining_piastres = remaining_piastres - $2, \
                        status = CASE WHEN remaining_piastres - $2 <= 0 THEN 'settled' ELSE status END, \
                        updated_at = now() \
                  WHERE id = $1",
            )
            .bind(advance_id)
            .bind(take)
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query(
            "INSERT INTO payslips (
                 org_id, payroll_period_id, user_id, base_salary_piastres, worked_days,
                 absent_days, leave_days, late_minutes, overtime_minutes, overtime_piastres,
                 bonuses_piastres, deductions_piastres, advance_installment_piastres,
                 net_piastres, breakdown
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
        )
        .bind(org_id)
        .bind(*id)
        .bind(slip.user_id)
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
        .execute(&mut *tx)
        .await?;

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
    .bind(claims.user_id())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let slips = sqlx::query_as::<_, Payslip>(&format!(
        "{PAYSLIP_SELECT} WHERE s.payroll_period_id = $1 ORDER BY lower(u.name)"
    ))
    .bind(*id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(slips))
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
    let claims = extract_claims(&req)?;
    // READ, not create: seeing what payroll would do is not running it.
    check_permission(pool.get_ref(), &claims, "payroll", "read").await?;
    let org_id = scope_org(&req, &claims)?;

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
    let computed = compute_payslips(&mut conn, org_id, period.0, period.1, &settings).await?;
    Ok(HttpResponse::Ok().json(computed))
}

/// The generated period as a bank-ready CSV.
///
/// Deliberately serves the PAYSLIPS, not a fresh computation: the file handed to
/// a bank must be exactly what was approved, even if a deduction has been edited
/// since. A period that has not been generated has nothing to export.
#[utoipa::path(
    get, path = "/staff/payroll/periods/{id}/export.csv", tag = "staff",
    params(("id" = Uuid, Path, description = "Period ID")),
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
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "payroll", "read").await?;
    let org_id = scope_org(&req, &claims)?;

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
        "{PAYSLIP_SELECT} WHERE s.payroll_period_id = $1 ORDER BY lower(u.name)"
    ))
    .bind(*id)
    .fetch_all(pool.get_ref())
    .await?;

    // Amounts are written in MAJOR units with two decimals — piastres are an
    // internal representation, and a bank importing "480000" would pay a
    // hundredfold. Every field is quoted and inner quotes doubled, so a name
    // containing a comma cannot shift a column.
    let esc = |value: &str| format!("\"{}\"", value.replace('"', "\"\""));
    let money = |piastres: i64| format!("{}.{:02}", piastres / 100, (piastres % 100).abs());

    let mut csv = String::from(
        "employee,employee_id,base,overtime,bonuses,deductions,advance,net,worked_days,absent_days\n",
    );
    for slip in &slips {
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{}\n",
            esc(slip.user_name.as_deref().unwrap_or("")),
            esc(&slip.user_id.to_string()),
            money(slip.base_salary_piastres),
            money(slip.overtime_piastres),
            money(slip.bonuses_piastres),
            money(slip.deductions_piastres),
            money(slip.advance_installment_piastres),
            money(slip.net_piastres),
            slip.worked_days,
            slip.absent_days,
        ));
    }

    // A quoted filename: period names carry spaces ("July 2026").
    let filename = format!("payroll-{}.csv", period.0.replace(['"', '\\', '/'], "-"));
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
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "payroll", "read").await?;
    let org_id = scope_org(&req, &claims)?;

    let rows = sqlx::query_as::<_, Payslip>(&format!(
        "{PAYSLIP_SELECT} WHERE s.payroll_period_id = $1 AND s.org_id = $2 ORDER BY lower(u.name)"
    ))
    .bind(*id)
    .bind(org_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    get, path = "/staff/me/payslips", tag = "staff",
    responses((status = 200, description = "The employee's own payslips", body = Vec<Payslip>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_payslips(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let user_id = claims.user_id_safe()?;
    my_org(pool.get_ref(), user_id).await?;

    // Only periods that have actually been finalised: a draft regeneration would
    // otherwise flash half-computed numbers at the employee.
    let rows = sqlx::query_as::<_, Payslip>(&format!(
        "{PAYSLIP_SELECT} \
          WHERE s.user_id = $1 AND pp.status IN ('generated', 'paid', 'closed') \
          ORDER BY pp.start_date DESC"
    ))
    .bind(user_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}
