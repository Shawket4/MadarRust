//! Legal / compliance reports: money given away or corrected after a sale,
//! with who and why. Refunds, voids, discounts and service-charge waivers
//! share one shape (a total plus a reason and an issuer breakdown); price
//! overrides has no "reason" of its own, so it breaks down by branch instead.
//!
//! "Issuer" for a discount or a price override is the order's `teller_id` —
//! there is no separate "who applied this discount" column, so the till
//! operator who rang the sale is the closest real signal, not a literal
//! approval record.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    errors::{AppError, AppErrorResponse},
    orders::SOLD,
    orgs::handlers::extract_claims,
};

use super::handlers::DateRangeQuery;

#[derive(Debug, Serialize, serde::Deserialize, sqlx::FromRow, ToSchema)]
pub struct AuditBreakdownEntry {
    pub label: String,
    pub count: i64,
    pub amount_minor: i64,
}

#[derive(Debug, Serialize, serde::Deserialize, ToSchema)]
pub struct AuditReport {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub total_count: i64,
    pub total_amount_minor: i64,
    pub by_reason: Vec<AuditBreakdownEntry>,
    pub by_issuer: Vec<AuditBreakdownEntry>,
}

/// `reports.legal`, then the branches the caller may see
/// ([`crate::authz::scope::org_read_branches`]).
///
/// `None` = the whole org (owner, super admin, org-wide assignment). Anyone
/// else holding `reports.legal` (a branch manager by default) gets only the
/// branches they work at, and a branch-bound PIN token only its own branch, so
/// an org-wide report never shows a branch the caller couldn't open on its own.
/// These reports name staff and expose refunds, so they are their own
/// capability rather than `orders.read`.
pub(crate) async fn guard(
    req: &HttpRequest,
    pool: &PgPool,
    org_id: Uuid,
) -> Result<Option<Vec<Uuid>>, AppError> {
    let claims = extract_claims(req)?;
    crate::authz::require::require(pool, &claims, crate::authz::Cap::ReportsLegal, None).await?;
    crate::authz::scope::org_read_branches(pool, &claims, org_id, None).await
}

// ── Refunds audit ────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/reports/orgs/{org_id}/refunds-audit",
    tag = "reports",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    params(DateRangeQuery),
    responses((status = 200, description = "Refunds issued, by reason and by issuer", body = AuditReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn refunds_audit(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let org_id = org_id.into_inner();
    let scope = guard(&req, pool.get_ref(), org_id).await?;

    let (total_count, total_amount_minor): (i64, i64) = sqlx::query_as(&format!(
        r#"
        SELECT COUNT(*)::bigint, COALESCE(SUM(r.amount), 0)::bigint
        FROM order_refunds r
        JOIN branches b ON b.id = r.branch_id
        WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR b.id = ANY($4))
          AND ($2::timestamptz IS NULL OR r.issued_at >= $2)
          AND ($3::timestamptz IS NULL OR r.issued_at <= $3)
        "#,
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_one(pool.get_ref())
    .await?;

    let by_reason: Vec<AuditBreakdownEntry> = sqlx::query_as(&format!(
        r#"
        SELECT r.reason AS label, COUNT(*)::bigint AS count, COALESCE(SUM(r.amount), 0)::bigint AS amount_minor
        FROM order_refunds r
        JOIN branches b ON b.id = r.branch_id
        WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR b.id = ANY($4))
          AND ($2::timestamptz IS NULL OR r.issued_at >= $2)
          AND ($3::timestamptz IS NULL OR r.issued_at <= $3)
        GROUP BY r.reason
        ORDER BY count DESC
        "#,
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_all(pool.get_ref())
    .await?;

    let by_issuer: Vec<AuditBreakdownEntry> = sqlx::query_as(&format!(
        r#"
        SELECT u.name AS label, COUNT(*)::bigint AS count, COALESCE(SUM(r.amount), 0)::bigint AS amount_minor
        FROM order_refunds r
        JOIN branches b ON b.id = r.branch_id
        JOIN users u ON u.id = r.issued_by
        WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR b.id = ANY($4))
          AND ($2::timestamptz IS NULL OR r.issued_at >= $2)
          AND ($3::timestamptz IS NULL OR r.issued_at <= $3)
        GROUP BY u.id, u.name
        ORDER BY count DESC
        LIMIT 10
        "#,
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(AuditReport {
        from: query.from,
        to: query.to,
        total_count,
        total_amount_minor,
        by_reason,
        by_issuer,
    }))
}

// ── Voids audit ──────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/reports/orgs/{org_id}/voids-audit",
    tag = "reports",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    params(DateRangeQuery),
    responses((status = 200, description = "Voided orders, by reason and by issuer", body = AuditReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn voids_audit(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let org_id = org_id.into_inner();
    let scope = guard(&req, pool.get_ref(), org_id).await?;

    let (total_count, total_amount_minor): (i64, i64) = sqlx::query_as(&format!(
        r#"
        SELECT COUNT(*)::bigint, COALESCE(SUM(o.total_amount), 0)::bigint
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR b.id = ANY($4)) AND o.status = 'voided'
          AND ($2::timestamptz IS NULL OR o.voided_at >= $2)
          AND ($3::timestamptz IS NULL OR o.voided_at <= $3)
        "#,
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_one(pool.get_ref())
    .await?;

    let by_reason: Vec<AuditBreakdownEntry> = sqlx::query_as(&format!(
        r#"
        SELECT COALESCE(o.void_reason::text, 'unspecified') AS label,
               COUNT(*)::bigint AS count, COALESCE(SUM(o.total_amount), 0)::bigint AS amount_minor
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR b.id = ANY($4)) AND o.status = 'voided'
          AND ($2::timestamptz IS NULL OR o.voided_at >= $2)
          AND ($3::timestamptz IS NULL OR o.voided_at <= $3)
        GROUP BY o.void_reason
        ORDER BY count DESC
        "#,
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_all(pool.get_ref())
    .await?;

    let by_issuer: Vec<AuditBreakdownEntry> = sqlx::query_as(&format!(
        r#"
        SELECT u.name AS label, COUNT(*)::bigint AS count, COALESCE(SUM(o.total_amount), 0)::bigint AS amount_minor
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        JOIN users u ON u.id = o.voided_by
        WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR b.id = ANY($4)) AND o.status = 'voided'
          AND ($2::timestamptz IS NULL OR o.voided_at >= $2)
          AND ($3::timestamptz IS NULL OR o.voided_at <= $3)
        GROUP BY u.id, u.name
        ORDER BY count DESC
        LIMIT 10
        "#,
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(AuditReport {
        from: query.from,
        to: query.to,
        total_count,
        total_amount_minor,
        by_reason,
        by_issuer,
    }))
}

// ── Discounts audit ──────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/reports/orgs/{org_id}/discounts-audit",
    tag = "reports",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    params(DateRangeQuery),
    responses((status = 200, description = "Discounts applied, by discount and by till operator", body = AuditReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn discounts_audit(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let org_id = org_id.into_inner();
    let scope = guard(&req, pool.get_ref(), org_id).await?;

    let (total_count, total_amount_minor): (i64, i64) = sqlx::query_as(&format!(
        r#"
        SELECT COUNT(*)::bigint,
               COALESCE(SUM(o.discount_amount), 0)::bigint
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR b.id = ANY($4)) AND o.{SOLD} AND o.discount_amount > 0
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        "#,
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_one(pool.get_ref())
    .await?;

    let by_reason: Vec<AuditBreakdownEntry> = sqlx::query_as(&format!(
        r#"
        SELECT COALESCE(d.name, o.discount_type::text, 'unspecified') AS label,
               COUNT(*)::bigint AS count, COALESCE(SUM(o.discount_amount), 0)::bigint AS amount_minor
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        LEFT JOIN discounts d ON d.id = o.discount_id
        WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR b.id = ANY($4)) AND o.{SOLD} AND o.discount_amount > 0
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY COALESCE(d.name, o.discount_type::text, 'unspecified')
        ORDER BY count DESC
        LIMIT 10
        "#,
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_all(pool.get_ref())
    .await?;

    let by_issuer: Vec<AuditBreakdownEntry> = sqlx::query_as(&format!(
        r#"
        SELECT u.name AS label, COUNT(*)::bigint AS count, COALESCE(SUM(o.discount_amount), 0)::bigint AS amount_minor
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        JOIN users u ON u.id = o.teller_id
        WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR b.id = ANY($4)) AND o.{SOLD} AND o.discount_amount > 0
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY u.id, u.name
        ORDER BY count DESC
        LIMIT 10
        "#,
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(AuditReport {
        from: query.from,
        to: query.to,
        total_count,
        total_amount_minor,
        by_reason,
        by_issuer,
    }))
}

// ── Service charge waivers audit ────────────────────────────

#[utoipa::path(
    get,
    path = "/reports/orgs/{org_id}/waivers-audit",
    tag = "reports",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    params(DateRangeQuery),
    responses((status = 200, description = "Service charge waived, by issuer", body = AuditReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn waivers_audit(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let org_id = org_id.into_inner();
    let scope = guard(&req, pool.get_ref(), org_id).await?;

    let (total_count, total_amount_minor): (i64, i64) = sqlx::query_as(&format!(
        r#"
        SELECT COUNT(*)::bigint, COALESCE(SUM(o.service_charge_waived_amount), 0)::bigint
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR b.id = ANY($4)) AND o.service_charge_waived_by IS NOT NULL AND o.{SOLD}
          AND ($2::timestamptz IS NULL OR o.service_charge_waived_at >= $2)
          AND ($3::timestamptz IS NULL OR o.service_charge_waived_at <= $3)
        "#,
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_one(pool.get_ref())
    .await?;

    // No free-text reason is captured for a waiver, so the "reason" axis is
    // the branch it happened at instead — still useful to spot a branch that
    // waives far more than its peers.
    let by_reason: Vec<AuditBreakdownEntry> = sqlx::query_as(&format!(
        r#"
        SELECT b.name AS label, COUNT(*)::bigint AS count,
               COALESCE(SUM(o.service_charge_waived_amount), 0)::bigint AS amount_minor
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR b.id = ANY($4)) AND o.service_charge_waived_by IS NOT NULL AND o.{SOLD}
          AND ($2::timestamptz IS NULL OR o.service_charge_waived_at >= $2)
          AND ($3::timestamptz IS NULL OR o.service_charge_waived_at <= $3)
        GROUP BY b.name
        ORDER BY count DESC
        "#,
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_all(pool.get_ref())
    .await?;

    let by_issuer: Vec<AuditBreakdownEntry> = sqlx::query_as(&format!(
        r#"
        SELECT u.name AS label, COUNT(*)::bigint AS count,
               COALESCE(SUM(o.service_charge_waived_amount), 0)::bigint AS amount_minor
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        JOIN users u ON u.id = o.service_charge_waived_by
        WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR b.id = ANY($4)) AND o.service_charge_waived_by IS NOT NULL AND o.{SOLD}
          AND ($2::timestamptz IS NULL OR o.service_charge_waived_at >= $2)
          AND ($3::timestamptz IS NULL OR o.service_charge_waived_at <= $3)
        GROUP BY u.id, u.name
        ORDER BY count DESC
        LIMIT 10
        "#,
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(AuditReport {
        from: query.from,
        to: query.to,
        total_count,
        total_amount_minor,
        by_reason,
        by_issuer,
    }))
}

// ── Price overrides ──────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/reports/orgs/{org_id}/price-overrides",
    tag = "reports",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    params(DateRangeQuery),
    responses((status = 200, description = "Orders whose price disagreed with the catalogue, by branch and by till operator", body = AuditReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn price_overrides(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let org_id = org_id.into_inner();
    let scope = guard(&req, pool.get_ref(), org_id).await?;

    let (total_count, total_amount_minor): (i64, i64) = sqlx::query_as(&format!(
        r#"
        SELECT COUNT(*)::bigint, COALESCE(SUM(o.total_amount), 0)::bigint
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR b.id = ANY($4)) AND o.price_flagged AND o.{SOLD}
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        "#,
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_one(pool.get_ref())
    .await?;

    let by_reason: Vec<AuditBreakdownEntry> = sqlx::query_as(&format!(
        r#"
        SELECT b.name AS label, COUNT(*)::bigint AS count, COALESCE(SUM(o.total_amount), 0)::bigint AS amount_minor
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR b.id = ANY($4)) AND o.price_flagged AND o.{SOLD}
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY b.name
        ORDER BY count DESC
        "#,
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_all(pool.get_ref())
    .await?;

    let by_issuer: Vec<AuditBreakdownEntry> = sqlx::query_as(&format!(
        r#"
        SELECT u.name AS label, COUNT(*)::bigint AS count, COALESCE(SUM(o.total_amount), 0)::bigint AS amount_minor
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        JOIN users u ON u.id = o.teller_id
        WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR b.id = ANY($4)) AND o.price_flagged AND o.{SOLD}
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY u.id, u.name
        ORDER BY count DESC
        LIMIT 10
        "#,
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(AuditReport {
        from: query.from,
        to: query.to,
        total_count,
        total_amount_minor,
        by_reason,
        by_issuer,
    }))
}

// ── Payroll and attendance corrections ──────────────────────
//
// Payroll deductions carry no branch of their own. A caller scoped to some
// branches sees the deductions of people assigned to one of them, and these
// two reports also need `hr.payroll.read` on top of `reports.legal`: they
// show pay, which a manager may hold `reports.legal` without seeing.

async fn guard_with(
    req: &HttpRequest,
    pool: &PgPool,
    org_id: Uuid,
    also: crate::authz::Cap,
) -> Result<Option<Vec<Uuid>>, AppError> {
    let scope = guard(req, pool, org_id).await?;
    let claims = extract_claims(req)?;
    crate::authz::require::require(pool, &claims, also, None).await?;
    Ok(scope)
}

/// `pd` belongs to someone who works at one of the scoped branches.
const DEDUCTION_IN_SCOPE: &str = "($4::uuid[] IS NULL OR EXISTS (
    SELECT 1 FROM user_branch_assignments uba
    WHERE uba.user_id = pd.user_id AND uba.branch_id = ANY($4)))";

#[utoipa::path(
    get,
    path = "/reports/orgs/{org_id}/manual-deductions-audit",
    tag = "reports",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    params(DateRangeQuery),
    responses((status = 200, description = "Operator-entered payroll deductions (fixed amounts; percentage deductions count but add no amount), by reason and by who entered them", body = AuditReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn manual_deductions_audit(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let org_id = org_id.into_inner();
    let scope = guard_with(
        &req,
        pool.get_ref(),
        org_id,
        crate::authz::Cap::HrPayrollRead,
    )
    .await?;
    let filter = format!(
        "pd.org_id = $1 AND pd.source = 'manual' AND {DEDUCTION_IN_SCOPE}
          AND ($2::timestamptz IS NULL OR pd.created_at >= $2)
          AND ($3::timestamptz IS NULL OR pd.created_at <= $3)"
    );

    let (total_count, total_amount_minor): (i64, i64) = sqlx::query_as(&format!(
        "SELECT COUNT(*)::bigint, COALESCE(SUM(pd.amount_piastres), 0)::bigint
         FROM payroll_deductions pd WHERE {filter}"
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_one(pool.get_ref())
    .await?;

    let by_reason: Vec<AuditBreakdownEntry> = sqlx::query_as(&format!(
        "SELECT pd.reason AS label, COUNT(*)::bigint AS count,
                COALESCE(SUM(pd.amount_piastres), 0)::bigint AS amount_minor
         FROM payroll_deductions pd WHERE {filter}
         GROUP BY pd.reason ORDER BY count DESC LIMIT 10"
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_all(pool.get_ref())
    .await?;

    let by_issuer: Vec<AuditBreakdownEntry> = sqlx::query_as(&format!(
        "SELECT u.name AS label, COUNT(*)::bigint AS count,
                COALESCE(SUM(pd.amount_piastres), 0)::bigint AS amount_minor
         FROM payroll_deductions pd JOIN users u ON u.id = pd.created_by
         WHERE {filter}
         GROUP BY u.id, u.name ORDER BY count DESC LIMIT 10"
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(AuditReport {
        from: query.from,
        to: query.to,
        total_count,
        total_amount_minor,
        by_reason,
        by_issuer,
    }))
}

/// What an override or a waiver forgave: the whole original charge (waived) or
/// the drop from the original to the new amount (overridden).
const FORGIVEN: &str = "CASE
    WHEN pd.waived_at IS NOT NULL THEN COALESCE(pd.original_amount_piastres, pd.amount_piastres, 0)
    ELSE GREATEST(COALESCE(pd.original_amount_piastres, pd.amount_piastres, 0) - COALESCE(pd.amount_piastres, 0), 0)
END";

#[utoipa::path(
    get,
    path = "/reports/orgs/{org_id}/deduction-overrides-audit",
    tag = "reports",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    params(DateRangeQuery),
    responses((status = 200, description = "Automatic payroll deductions a manager overrode or waived, by type and by issuer", body = AuditReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn deduction_overrides_audit(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let org_id = org_id.into_inner();
    let scope = guard_with(
        &req,
        pool.get_ref(),
        org_id,
        crate::authz::Cap::HrPayrollRead,
    )
    .await?;
    let filter = format!(
        "pd.org_id = $1 AND (pd.overridden_at IS NOT NULL OR pd.waived_at IS NOT NULL)
          AND {DEDUCTION_IN_SCOPE}
          AND ($2::timestamptz IS NULL OR COALESCE(pd.waived_at, pd.overridden_at) >= $2)
          AND ($3::timestamptz IS NULL OR COALESCE(pd.waived_at, pd.overridden_at) <= $3)"
    );

    let (total_count, total_amount_minor): (i64, i64) = sqlx::query_as(&format!(
        "SELECT COUNT(*)::bigint, COALESCE(SUM({FORGIVEN}), 0)::bigint
         FROM payroll_deductions pd WHERE {filter}"
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_one(pool.get_ref())
    .await?;

    let by_reason: Vec<AuditBreakdownEntry> = sqlx::query_as(&format!(
        "SELECT (CASE WHEN pd.waived_at IS NOT NULL THEN 'waived' ELSE 'overridden' END) AS label,
                COUNT(*)::bigint AS count, COALESCE(SUM({FORGIVEN}), 0)::bigint AS amount_minor
         FROM payroll_deductions pd WHERE {filter}
         GROUP BY (pd.waived_at IS NOT NULL) ORDER BY count DESC"
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_all(pool.get_ref())
    .await?;

    let by_issuer: Vec<AuditBreakdownEntry> = sqlx::query_as(&format!(
        "SELECT u.name AS label, COUNT(*)::bigint AS count,
                COALESCE(SUM({FORGIVEN}), 0)::bigint AS amount_minor
         FROM payroll_deductions pd
         JOIN users u ON u.id = COALESCE(pd.waived_by, pd.overridden_by)
         WHERE {filter}
         GROUP BY u.id, u.name ORDER BY count DESC LIMIT 10"
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(AuditReport {
        from: query.from,
        to: query.to,
        total_count,
        total_amount_minor,
        by_reason,
        by_issuer,
    }))
}

// ── Loyalty manual adjustments ───────────────────────────────

#[utoipa::path(
    get,
    path = "/reports/orgs/{org_id}/loyalty-adjustments-audit",
    tag = "reports",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    params(DateRangeQuery),
    responses((status = 200, description = "Manual loyalty balance adjustments (not birthday or win-back rewards), by branch and by who made them. `amount_minor` here is POINTS or VISITS moved, in either direction, not money.", body = AuditReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn loyalty_adjustments_audit(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let org_id = org_id.into_inner();
    let scope = guard(&req, pool.get_ref(), org_id).await?;
    // `adjust` also carries the automatic birthday and win-back rewards; only a
    // person's hand (`source = 'manual'`, including undoing one) is audited.
    let filter = "t.org_id = $1 AND t.kind IN ('adjust', 'reverse_adjust') AND t.source = 'manual'
          AND ($4::uuid[] IS NULL OR t.branch_id = ANY($4))
          AND ($2::timestamptz IS NULL OR t.created_at >= $2)
          AND ($3::timestamptz IS NULL OR t.created_at <= $3)";

    let (total_count, total_amount_minor): (i64, i64) = sqlx::query_as(&format!(
        "SELECT COUNT(*)::bigint, COALESCE(SUM(ABS(t.points)), 0)::bigint
         FROM loyalty_transactions t WHERE {filter}"
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_one(pool.get_ref())
    .await?;

    let by_reason: Vec<AuditBreakdownEntry> = sqlx::query_as(&format!(
        "SELECT COALESCE(b.name, 'unspecified') AS label, COUNT(*)::bigint AS count,
                COALESCE(SUM(ABS(t.points)), 0)::bigint AS amount_minor
         FROM loyalty_transactions t LEFT JOIN branches b ON b.id = t.branch_id
         WHERE {filter}
         GROUP BY b.id, b.name ORDER BY count DESC"
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_all(pool.get_ref())
    .await?;

    let by_issuer: Vec<AuditBreakdownEntry> = sqlx::query_as(&format!(
        "SELECT u.name AS label, COUNT(*)::bigint AS count,
                COALESCE(SUM(ABS(t.points)), 0)::bigint AS amount_minor
         FROM loyalty_transactions t JOIN users u ON u.id = t.created_by
         WHERE {filter}
         GROUP BY u.id, u.name ORDER BY count DESC LIMIT 10"
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(AuditReport {
        from: query.from,
        to: query.to,
        total_count,
        total_amount_minor,
        by_reason,
        by_issuer,
    }))
}

// ── Attendance corrections ───────────────────────────────────

#[utoipa::path(
    get,
    path = "/reports/orgs/{org_id}/attendance-corrections-audit",
    tag = "reports",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    params(DateRangeQuery),
    responses((status = 200, description = "Attendance records a manager edited after the fact, by reason and by editor", body = AuditReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn attendance_corrections_audit(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let org_id = org_id.into_inner();
    let scope = guard_with(
        &req,
        pool.get_ref(),
        org_id,
        crate::authz::Cap::HrAttendanceRead,
    )
    .await?;
    // No money on a correction: the amount axis stays 0, the count matters.
    let filter = "a.org_id = $1 AND a.edited_by IS NOT NULL
          AND ($4::uuid[] IS NULL OR a.branch_id = ANY($4))
          AND ($2::timestamptz IS NULL OR a.updated_at >= $2)
          AND ($3::timestamptz IS NULL OR a.updated_at <= $3)";

    let total_count: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*)::bigint FROM attendance_records a WHERE {filter}"
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_one(pool.get_ref())
    .await?;

    let by_reason: Vec<AuditBreakdownEntry> = sqlx::query_as(&format!(
        "SELECT COALESCE(a.edit_reason, 'unspecified') AS label, COUNT(*)::bigint AS count,
                0::bigint AS amount_minor
         FROM attendance_records a WHERE {filter}
         GROUP BY a.edit_reason ORDER BY count DESC LIMIT 10"
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_all(pool.get_ref())
    .await?;

    let by_issuer: Vec<AuditBreakdownEntry> = sqlx::query_as(&format!(
        "SELECT u.name AS label, COUNT(*)::bigint AS count, 0::bigint AS amount_minor
         FROM attendance_records a JOIN users u ON u.id = a.edited_by
         WHERE {filter}
         GROUP BY u.id, u.name ORDER BY count DESC LIMIT 10"
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(AuditReport {
        from: query.from,
        to: query.to,
        total_count,
        total_amount_minor: 0,
        by_reason,
        by_issuer,
    }))
}
