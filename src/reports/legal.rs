//! Legal / compliance reports: money given away or corrected after a sale,
//! with who and why. Refunds, voids, discounts and service-charge waivers
//! share one shape (a total plus a reason and an issuer breakdown); price
//! overrides has no "reason" of its own, so it breaks down by branch instead.
//!
//! "Issuer" for a discount is `orders.discount_applied_by` (the till operator
//! for sales from before discounts were attributed); for a price override it
//! is the order's `teller_id`.

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
    /// A stable code for a label the SERVER wrote (`unspecified`,
    /// `correction_request`, `auto_closed`, a void reason), so a client
    /// words it in its own language (AT-13, E2E B-PAY-5). Absent for a
    /// person's own words (a typed reason, a name): `label` is the text.
    #[sqlx(default)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

#[derive(Debug, Serialize, serde::Deserialize, ToSchema)]
pub struct AuditReport {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub total_count: i64,
    pub total_amount_minor: i64,
    pub by_reason: Vec<AuditBreakdownEntry>,
    pub by_issuer: Vec<AuditBreakdownEntry>,
    /// Discounts audit only: by act (`preset` / `manual_amount` /
    /// `manual_percent`; `unattributed` for sales from before). Additive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_kind: Option<Vec<AuditBreakdownEntry>>,
    /// Discounts audit only: the most recent discounted sales, newest first
    /// (at most 200). Additive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<DiscountAuditEntry>>,
    /// Deduction overrides audit only: every waive, unwaive and override
    /// event with who, when and why, newest first (at most 500) — the
    /// history, so a waiver later undone still shows (owner decision D8,
    /// AT-10). Additive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<DeductionOverrideEvent>>,
}

/// One waive, unwaive or override of a payroll deduction, from the money
/// audit log (D8).
#[derive(Debug, Serialize, serde::Deserialize, sqlx::FromRow, ToSchema)]
pub struct DeductionOverrideEvent {
    pub deduction_id: Uuid,
    pub employee_id: Option<Uuid>,
    pub employee_name: Option<String>,
    /// `waive` · `unwaive` · `override`
    pub action: String,
    pub actor_id: Option<Uuid>,
    pub actor_name: Option<String>,
    pub at: DateTime<Utc>,
    pub reason: Option<String>,
    /// What the line charged before and after this event (a waiver: after
    /// 0; undoing one: before 0).
    pub amount_before_piastres: Option<i64>,
    pub amount_after_piastres: Option<i64>,
    /// The line itself: its day, what made it (`absence`, `late_penalty`,
    /// …) and its rule's reason code. Null if the line is gone.
    pub effective_date: Option<chrono::NaiveDate>,
    pub source: Option<String>,
    pub reason_code: Option<String>,
}

/// One discounted sale in the discounts audit.
#[derive(Debug, Serialize, serde::Deserialize, sqlx::FromRow, ToSchema)]
pub struct DiscountAuditEntry {
    pub order_id: Uuid,
    pub order_ref: Option<String>,
    pub branch_name: String,
    pub created_at: DateTime<Utc>,
    /// `preset` | `manual_amount` | `manual_percent`, or `null` before attribution.
    pub kind: Option<String>,
    pub preset_id: Option<Uuid>,
    pub preset_name: Option<String>,
    pub amount_minor: i64,
    /// Basis points, for a percentage.
    pub percent_bps: Option<i32>,
    /// Who applied it (the till operator for older sales).
    pub applied_by_name: Option<String>,
    pub approval_id: Option<Uuid>,
    pub approved_by_name: Option<String>,
    /// The sale was replayed with a discount its author was not allowed and
    /// no valid manager approval (`authz_replay_flags`).
    pub flagged: bool,
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

    let (total_count, total_amount_minor): (i64, i64) = sqlx::query_as(
        r#"
        SELECT COUNT(*)::bigint, COALESCE(SUM(r.amount), 0)::bigint
        FROM order_refunds r
        JOIN branches b ON b.id = r.branch_id
        WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR b.id = ANY($4))
          AND ($2::timestamptz IS NULL OR r.issued_at >= $2)
          AND ($3::timestamptz IS NULL OR r.issued_at <= $3)
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_one(pool.get_ref())
    .await?;

    let by_reason: Vec<AuditBreakdownEntry> = sqlx::query_as(r#"
        SELECT r.reason AS label, COUNT(*)::bigint AS count, COALESCE(SUM(r.amount), 0)::bigint AS amount_minor
        FROM order_refunds r
        JOIN branches b ON b.id = r.branch_id
        WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR b.id = ANY($4))
          AND ($2::timestamptz IS NULL OR r.issued_at >= $2)
          AND ($3::timestamptz IS NULL OR r.issued_at <= $3)
        GROUP BY r.reason
        ORDER BY count DESC
        "#)
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_all(pool.get_ref())
    .await?;

    let by_issuer: Vec<AuditBreakdownEntry> = sqlx::query_as(r#"
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
        "#)
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
        by_kind: None,
        entries: None,
        history: None,
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

    let (total_count, total_amount_minor): (i64, i64) = sqlx::query_as(
        r#"
        SELECT COUNT(*)::bigint, COALESCE(SUM(o.total_amount), 0)::bigint
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR b.id = ANY($4)) AND o.status = 'voided'
          AND ($2::timestamptz IS NULL OR o.voided_at >= $2)
          AND ($3::timestamptz IS NULL OR o.voided_at <= $3)
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_one(pool.get_ref())
    .await?;

    let by_reason: Vec<AuditBreakdownEntry> = sqlx::query_as(
        r#"
        SELECT COALESCE(o.void_reason::text, 'unspecified') AS label,
               COUNT(*)::bigint AS count, COALESCE(SUM(o.total_amount), 0)::bigint AS amount_minor,
               COALESCE(o.void_reason::text, 'unspecified') AS code
        FROM orders o
        JOIN branches b ON b.id = o.branch_id
        WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR b.id = ANY($4)) AND o.status = 'voided'
          AND ($2::timestamptz IS NULL OR o.voided_at >= $2)
          AND ($3::timestamptz IS NULL OR o.voided_at <= $3)
        GROUP BY o.void_reason
        ORDER BY count DESC
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(&scope)
    .fetch_all(pool.get_ref())
    .await?;

    let by_issuer: Vec<AuditBreakdownEntry> = sqlx::query_as(r#"
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
        "#)
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
        by_kind: None,
        entries: None,
        history: None,
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

    // One pass: the window's discounted sales are read once (`f`), and the
    // total, the three breakdowns and the newest entries are all cut from
    // that set; they used to be five statements, each its own scan of the
    // org's orders. Section 0 = total, 1 = by discount, 2 = by issuer,
    // 3 = by kind, 4 = entries, each in its own rank order. A breakdown tie
    // on `count` now breaks on the label (and the person's id), where it
    // used to fall wherever the plan put it.
    // (`o.status::text NOT IN ('voided', 'refunded')` is `crate::orders::SOLD`,
    // spelled out for the compile-time check.)
    let rows = sqlx::query!(
        r#"
        WITH f AS MATERIALIZED (
            SELECT o.id, o.order_ref, o.created_at, b.name AS branch_name,
                   o.discount_amount, o.discount_type::text AS discount_type, o.discount_id,
                   o.discount_kind, o.discount_percent_bps, o.discount_approval_id,
                   COALESCE(o.discount_applied_by, o.teller_id) AS issuer
              FROM orders o
              JOIN branches b ON b.id = o.branch_id
             WHERE b.org_id = $1 AND ($4::uuid[] IS NULL OR b.id = ANY($4))
               AND o.status::text NOT IN ('voided', 'refunded') AND o.discount_amount > 0
               AND ($2::timestamptz IS NULL OR o.created_at >= $2)
               AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        ),
        by_reason AS (
            SELECT COALESCE(d.name, f.discount_type, 'unspecified') AS label,
                   COUNT(*)::bigint AS count, COALESCE(SUM(f.discount_amount), 0)::bigint AS amount,
                   -- A named discount is its own words; a bare type or nothing is a code.
                   MIN(CASE WHEN d.name IS NULL
                            THEN COALESCE(f.discount_type, 'unspecified') END) AS code,
                   row_number() OVER (ORDER BY COUNT(*) DESC, COALESCE(d.name, f.discount_type, 'unspecified')) AS rn
              FROM f LEFT JOIN discounts d ON d.id = f.discount_id
             GROUP BY COALESCE(d.name, f.discount_type, 'unspecified')
        ),
        by_issuer AS (
            SELECT u.name AS label, COUNT(*)::bigint AS count,
                   COALESCE(SUM(f.discount_amount), 0)::bigint AS amount,
                   row_number() OVER (ORDER BY COUNT(*) DESC, u.name, u.id) AS rn
              FROM f JOIN users u ON u.id = f.issuer
             GROUP BY u.id, u.name
        ),
        by_kind AS (
            SELECT COALESCE(f.discount_kind, 'unattributed') AS label,
                   COUNT(*)::bigint AS count, COALESCE(SUM(f.discount_amount), 0)::bigint AS amount,
                   row_number() OVER (ORDER BY COUNT(*) DESC, COALESCE(f.discount_kind, 'unattributed')) AS rn
              FROM f
             GROUP BY COALESCE(f.discount_kind, 'unattributed')
        ),
        entries AS (
            SELECT f.*, d.name AS preset_name, u.name AS applied_by_name, au.name AS approved_by_name,
                   EXISTS (SELECT 1 FROM authz_replay_flags fl
                            WHERE fl.subject_id = f.id AND fl.capability LIKE 'orders.discount.%') AS flagged,
                   row_number() OVER (ORDER BY f.created_at DESC) AS rn
              FROM f
              LEFT JOIN discounts d ON d.id = f.discount_id
              LEFT JOIN users u ON u.id = f.issuer
              LEFT JOIN approvals ap ON ap.id = f.discount_approval_id
              LEFT JOIN users au ON au.id = ap.approver_user_id
             ORDER BY f.created_at DESC
             LIMIT 200
        )
        SELECT section AS "section!", rn AS "rn!", label, count, amount, code,
               order_id, order_ref, branch_name, created_at, kind, preset_id, preset_name,
               percent_bps, applied_by_name, approval_id, approved_by_name, flagged
          FROM (
            SELECT 0 AS section, 0::bigint AS rn, NULL::text AS label, COUNT(*)::bigint AS count,
                   COALESCE(SUM(f.discount_amount), 0)::bigint AS amount, NULL::text AS code,
                   NULL::uuid AS order_id, NULL::text AS order_ref, NULL::text AS branch_name,
                   NULL::timestamptz AS created_at, NULL::text AS kind, NULL::uuid AS preset_id,
                   NULL::text AS preset_name, NULL::int AS percent_bps, NULL::text AS applied_by_name,
                   NULL::uuid AS approval_id, NULL::text AS approved_by_name, NULL::bool AS flagged
              FROM f
            UNION ALL
            SELECT 1, rn, label, count, amount, code, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                   NULL, NULL, NULL, NULL, NULL
              FROM by_reason WHERE rn <= 10
            UNION ALL
            SELECT 2, rn, label, count, amount, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                   NULL, NULL, NULL, NULL, NULL
              FROM by_issuer WHERE rn <= 10
            UNION ALL
            SELECT 3, rn, label, count, amount, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                   NULL, NULL, NULL, NULL, NULL
              FROM by_kind
            UNION ALL
            SELECT 4, rn, NULL, NULL, discount_amount::bigint, NULL, id, order_ref, branch_name,
                   created_at, discount_kind, discount_id, preset_name, discount_percent_bps,
                   applied_by_name, discount_approval_id, approved_by_name, flagged
              FROM entries
          ) x
         ORDER BY section, rn
        "#,
        org_id,
        query.from,
        query.to,
        scope.as_deref(),
    )
    .fetch_all(pool.get_ref())
    .await?;

    let (mut total_count, mut total_amount_minor) = (0i64, 0i64);
    let (mut by_reason, mut by_issuer, mut by_kind, mut entries) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for r in rows {
        let entry = || AuditBreakdownEntry {
            label: r.label.clone().unwrap_or_default(),
            count: r.count.unwrap_or(0),
            amount_minor: r.amount.unwrap_or(0),
            code: r.code.clone(),
        };
        match r.section {
            0 => {
                total_count = r.count.unwrap_or(0);
                total_amount_minor = r.amount.unwrap_or(0);
            }
            1 => by_reason.push(entry()),
            2 => by_issuer.push(entry()),
            3 => by_kind.push(entry()),
            _ => entries.push(DiscountAuditEntry {
                order_id: r.order_id.unwrap_or_default(),
                order_ref: r.order_ref,
                branch_name: r.branch_name.unwrap_or_default(),
                created_at: r.created_at.unwrap_or_default(),
                kind: r.kind,
                preset_id: r.preset_id,
                preset_name: r.preset_name,
                amount_minor: r.amount.unwrap_or(0),
                percent_bps: r.percent_bps,
                applied_by_name: r.applied_by_name,
                approval_id: r.approval_id,
                approved_by_name: r.approved_by_name,
                flagged: r.flagged.unwrap_or(false),
            }),
        }
    }

    Ok(HttpResponse::Ok().json(AuditReport {
        from: query.from,
        to: query.to,
        total_count,
        total_amount_minor,
        by_reason,
        by_issuer,
        by_kind: Some(by_kind),
        entries: Some(entries),
        history: None,
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
        by_kind: None,
        entries: None,
        history: None,
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
        by_kind: None,
        entries: None,
        history: None,
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
    SELECT 1 FROM employee_branches eb
    WHERE eb.employee_id = pd.employee_id AND eb.branch_id = ANY($4)))";

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
        by_kind: None,
        entries: None,
        history: None,
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
    responses((status = 200, description = "Automatic payroll deductions a manager overrode or waived, by type and by issuer, with `history`: every waive, unwaive and override event (who, when, why)", body = AuditReport), AppErrorResponse),
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

    // The history (D8): every waive, unwaive and override from the money
    // audit log, not just each line's state today.
    let history: Vec<DeductionOverrideEvent> = sqlx::query_as(
        "SELECT l.entity_id AS deduction_id, l.employee_id, e.name AS employee_name,
                split_part(l.action, '.', 2) AS action, l.actor_id, u.name AS actor_name,
                l.created_at AS at, l.reason,
                CASE l.action
                    WHEN 'deduction.override' THEN (l.details->>'from_piastres')::bigint
                    WHEN 'deduction.waive' THEN (l.details->>'amount_piastres')::bigint
                    ELSE 0 END AS amount_before_piastres,
                CASE l.action
                    WHEN 'deduction.override' THEN (l.details->>'to_piastres')::bigint
                    WHEN 'deduction.unwaive' THEN (l.details->>'amount_piastres')::bigint
                    ELSE 0 END AS amount_after_piastres,
                pd.effective_date, pd.source, pd.reason_code
           FROM payroll_audit_log l
           LEFT JOIN payroll_deductions pd ON pd.id = l.entity_id
           LEFT JOIN employees e ON e.id = l.employee_id
           LEFT JOIN users u ON u.id = l.actor_id
          WHERE l.org_id = $1 AND l.entity = 'payroll_deductions' AND l.entity_id IS NOT NULL
            AND l.action IN ('deduction.waive', 'deduction.unwaive', 'deduction.override')
            AND ($4::uuid[] IS NULL OR EXISTS (
                SELECT 1 FROM employee_branches eb
                 WHERE eb.employee_id = l.employee_id AND eb.branch_id = ANY($4)))
            AND ($2::timestamptz IS NULL OR l.created_at >= $2)
            AND ($3::timestamptz IS NULL OR l.created_at <= $3)
          ORDER BY l.created_at DESC, l.id LIMIT 500",
    )
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
        by_kind: None,
        entries: None,
        history: Some(history),
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
        by_kind: None,
        entries: None,
        history: None,
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
                0::bigint AS amount_minor,
                CASE WHEN a.edit_reason IS NULL THEN 'unspecified'
                     WHEN a.edit_reason = 'Approved punch correction request' THEN 'correction_request'
                     WHEN a.edit_reason = 'Written by an approved punch correction' THEN 'correction_request'
                     WHEN a.edit_reason = 'Auto-closed: no checkout recorded' THEN 'auto_closed'
                     WHEN a.edit_reason = 'Marked automatically: no check-in' THEN 'marked_absent'
                END AS code
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
        by_kind: None,
        entries: None,
        history: None,
    }))
}
