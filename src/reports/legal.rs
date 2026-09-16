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
    models::UserRole,
    orgs::handlers::extract_claims,
    permissions::checker::check_permission,
};

use super::handlers::DateRangeQuery;

#[derive(Debug, Serialize, sqlx::FromRow, ToSchema)]
pub struct AuditBreakdownEntry {
    pub label: String,
    pub count: i64,
    pub amount_minor: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AuditReport {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub total_count: i64,
    pub total_amount_minor: i64,
    pub by_reason: Vec<AuditBreakdownEntry>,
    pub by_issuer: Vec<AuditBreakdownEntry>,
}

async fn guard(req: &HttpRequest, pool: &PgPool, org_id: Uuid) -> Result<(), AppError> {
    let claims = extract_claims(req)?;
    check_permission(pool, &claims, "orders", "read").await?;
    if claims.role != UserRole::SuperAdmin && claims.org_id() != Some(org_id) {
        return Err(AppError::Forbidden("Not your org".into()));
    }
    Ok(())
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
    guard(&req, pool.get_ref(), org_id).await?;

    let (total_count, total_amount_minor): (i64, i64) = sqlx::query_as(
        r#"
        SELECT COUNT(*)::bigint, COALESCE(SUM(r.amount), 0)::bigint
        FROM order_refunds r
        JOIN branches b ON b.id = r.branch_id
        WHERE b.org_id = $1
          AND ($2::timestamptz IS NULL OR r.issued_at >= $2)
          AND ($3::timestamptz IS NULL OR r.issued_at <= $3)
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_one(pool.get_ref())
    .await?;

    let by_reason: Vec<AuditBreakdownEntry> = sqlx::query_as(
        r#"
        SELECT r.reason AS label, COUNT(*)::bigint AS count, COALESCE(SUM(r.amount), 0)::bigint AS amount_minor
        FROM order_refunds r
        JOIN branches b ON b.id = r.branch_id
        WHERE b.org_id = $1
          AND ($2::timestamptz IS NULL OR r.issued_at >= $2)
          AND ($3::timestamptz IS NULL OR r.issued_at <= $3)
        GROUP BY r.reason
        ORDER BY count DESC
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    let by_issuer: Vec<AuditBreakdownEntry> = sqlx::query_as(
        r#"
        SELECT u.name AS label, COUNT(*)::bigint AS count, COALESCE(SUM(r.amount), 0)::bigint AS amount_minor
        FROM order_refunds r
        JOIN branches b ON b.id = r.branch_id
        JOIN users u ON u.id = r.issued_by
        WHERE b.org_id = $1
          AND ($2::timestamptz IS NULL OR r.issued_at >= $2)
          AND ($3::timestamptz IS NULL OR r.issued_at <= $3)
        GROUP BY u.name
        ORDER BY count DESC
        LIMIT 10
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
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
    guard(&req, pool.get_ref(), org_id).await?;

    let (total_count, total_amount_minor): (i64, i64) = sqlx::query_as(
        r#"
        SELECT COUNT(*)::bigint, COALESCE(SUM(o.total_amount), 0)::bigint
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        WHERE b.org_id = $1 AND o.status = 'voided'
          AND ($2::timestamptz IS NULL OR o.voided_at >= $2)
          AND ($3::timestamptz IS NULL OR o.voided_at <= $3)
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_one(pool.get_ref())
    .await?;

    let by_reason: Vec<AuditBreakdownEntry> = sqlx::query_as(
        r#"
        SELECT COALESCE(o.void_reason::text, 'unspecified') AS label,
               COUNT(*)::bigint AS count, COALESCE(SUM(o.total_amount), 0)::bigint AS amount_minor
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        WHERE b.org_id = $1 AND o.status = 'voided'
          AND ($2::timestamptz IS NULL OR o.voided_at >= $2)
          AND ($3::timestamptz IS NULL OR o.voided_at <= $3)
        GROUP BY o.void_reason
        ORDER BY count DESC
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    let by_issuer: Vec<AuditBreakdownEntry> = sqlx::query_as(
        r#"
        SELECT u.name AS label, COUNT(*)::bigint AS count, COALESCE(SUM(o.total_amount), 0)::bigint AS amount_minor
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        JOIN users u ON u.id = o.voided_by
        WHERE b.org_id = $1 AND o.status = 'voided'
          AND ($2::timestamptz IS NULL OR o.voided_at >= $2)
          AND ($3::timestamptz IS NULL OR o.voided_at <= $3)
        GROUP BY u.name
        ORDER BY count DESC
        LIMIT 10
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
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
    guard(&req, pool.get_ref(), org_id).await?;

    let (total_count, total_amount_minor): (i64, i64) = sqlx::query_as(
        r#"
        SELECT COUNT(*) FILTER (WHERE o.discount_amount > 0)::bigint,
               COALESCE(SUM(o.discount_amount), 0)::bigint
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        WHERE b.org_id = $1 AND o.status NOT IN ('voided', 'refunded') AND o.discount_amount > 0
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_one(pool.get_ref())
    .await?;

    let by_reason: Vec<AuditBreakdownEntry> = sqlx::query_as(
        r#"
        SELECT COALESCE(d.name, o.discount_type::text, 'unspecified') AS label,
               COUNT(*)::bigint AS count, COALESCE(SUM(o.discount_amount), 0)::bigint AS amount_minor
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        LEFT JOIN discounts d ON d.id = o.discount_id
        WHERE b.org_id = $1 AND o.status NOT IN ('voided', 'refunded') AND o.discount_amount > 0
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY COALESCE(d.name, o.discount_type::text, 'unspecified')
        ORDER BY count DESC
        LIMIT 10
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    let by_issuer: Vec<AuditBreakdownEntry> = sqlx::query_as(
        r#"
        SELECT u.name AS label, COUNT(*)::bigint AS count, COALESCE(SUM(o.discount_amount), 0)::bigint AS amount_minor
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        JOIN users u ON u.id = o.teller_id
        WHERE b.org_id = $1 AND o.status NOT IN ('voided', 'refunded') AND o.discount_amount > 0
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY u.name
        ORDER BY count DESC
        LIMIT 10
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
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
    guard(&req, pool.get_ref(), org_id).await?;

    let (total_count, total_amount_minor): (i64, i64) = sqlx::query_as(
        r#"
        SELECT COUNT(*)::bigint, COALESCE(SUM(o.service_charge_waived_amount), 0)::bigint
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        WHERE b.org_id = $1 AND o.service_charge_waived_by IS NOT NULL
          AND ($2::timestamptz IS NULL OR o.service_charge_waived_at >= $2)
          AND ($3::timestamptz IS NULL OR o.service_charge_waived_at <= $3)
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_one(pool.get_ref())
    .await?;

    // No free-text reason is captured for a waiver, so the "reason" axis is
    // the branch it happened at instead — still useful to spot a branch that
    // waives far more than its peers.
    let by_reason: Vec<AuditBreakdownEntry> = sqlx::query_as(
        r#"
        SELECT b.name AS label, COUNT(*)::bigint AS count,
               COALESCE(SUM(o.service_charge_waived_amount), 0)::bigint AS amount_minor
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        WHERE b.org_id = $1 AND o.service_charge_waived_by IS NOT NULL
          AND ($2::timestamptz IS NULL OR o.service_charge_waived_at >= $2)
          AND ($3::timestamptz IS NULL OR o.service_charge_waived_at <= $3)
        GROUP BY b.name
        ORDER BY count DESC
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    let by_issuer: Vec<AuditBreakdownEntry> = sqlx::query_as(
        r#"
        SELECT u.name AS label, COUNT(*)::bigint AS count,
               COALESCE(SUM(o.service_charge_waived_amount), 0)::bigint AS amount_minor
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        JOIN users u ON u.id = o.service_charge_waived_by
        WHERE b.org_id = $1 AND o.service_charge_waived_by IS NOT NULL
          AND ($2::timestamptz IS NULL OR o.service_charge_waived_at >= $2)
          AND ($3::timestamptz IS NULL OR o.service_charge_waived_at <= $3)
        GROUP BY u.name
        ORDER BY count DESC
        LIMIT 10
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
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
    guard(&req, pool.get_ref(), org_id).await?;

    let (total_count, total_amount_minor): (i64, i64) = sqlx::query_as(
        r#"
        SELECT COUNT(*)::bigint, COALESCE(SUM(o.total_amount), 0)::bigint
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        WHERE b.org_id = $1 AND o.price_flagged AND o.status NOT IN ('voided', 'refunded')
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_one(pool.get_ref())
    .await?;

    let by_reason: Vec<AuditBreakdownEntry> = sqlx::query_as(
        r#"
        SELECT b.name AS label, COUNT(*)::bigint AS count, COALESCE(SUM(o.total_amount), 0)::bigint AS amount_minor
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        WHERE b.org_id = $1 AND o.price_flagged AND o.status NOT IN ('voided', 'refunded')
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY b.name
        ORDER BY count DESC
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    let by_issuer: Vec<AuditBreakdownEntry> = sqlx::query_as(
        r#"
        SELECT u.name AS label, COUNT(*)::bigint AS count, COALESCE(SUM(o.total_amount), 0)::bigint AS amount_minor
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        JOIN users u ON u.id = o.teller_id
        WHERE b.org_id = $1 AND o.price_flagged AND o.status NOT IN ('voided', 'refunded')
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY u.name
        ORDER BY count DESC
        LIMIT 10
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
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
