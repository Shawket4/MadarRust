//! Partner analytics pull + the org-admin surface that issues its credentials.
//!
//! Two audiences, two auth schemes, deliberately kept apart:
//!
//!   * `GET /integrations/analytics/orders` — the PARTNER endpoint. HTTP Basic
//!     ([`IntegrationCaller`]), read-only, hard-scoped to the one branch the
//!     credential was issued for.
//!   * `/integrations/credentials/*` — the OPERATOR surface behind the normal
//!     JWT, org-admin only. This is what the dashboard's integrations screen
//!     drives: issue, list, rotate, revoke.

use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::auth::guards::require_org_admin;
use crate::auth::jwt::Claims;
use crate::db::Db;
use crate::errors::AppError;
use crate::integrations::auth::IntegrationCaller;
use crate::orders::SOLD;

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

/// Fallback when neither the branch nor its org has a timezone configured —
/// the same default the reports layer uses, so a partner's day boundaries and
/// the dashboard's always agree.
const DEFAULT_TZ: &str = "Africa/Cairo";

/// Upper bound on `limit`. Pagination is OPTIONAL today (omit `limit` and the
/// full window comes back in one response); this only stops a partner asking
/// for an absurd page. If volume forces pagination to become mandatory later,
/// that is a change to the default, not to the contract shape.
const MAX_PAGE: i64 = 5_000;

// ── Partner-facing: GET /integrations/analytics/orders ────────

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AnalyticsQuery {
    /// First business day to include, `YYYY-MM-DD`, in the branch's timezone.
    pub from: NaiveDate,
    /// Last business day to include, `YYYY-MM-DD`, INCLUSIVE.
    pub to: NaiveDate,
    /// Optional page size (max 5000). Omit for the whole window.
    pub limit: Option<i64>,
    /// Optional row offset, used with `limit`. Defaults to 0.
    pub offset: Option<i64>,
}

/// One order, reduced to the money that belongs to the order itself.
#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct AnalyticsOrder {
    pub order_id: Uuid,
    /// Per-shift sequence number shown on the POS.
    pub order_number: i32,
    /// The human-readable reference printed on the receipt
    /// (`<BRANCHCODE>-<YYMMDD>-<NNNN>`). Null for orders predating it.
    pub order_ref: Option<String>,
    pub status: String,
    /// Calendar day the order belongs to, in the branch's timezone. Derived
    /// from `created_at` — the SAME derivation the receipt's `order_ref` uses,
    /// so the date here always matches the date embedded in that reference.
    pub business_date: NaiveDate,
    pub created_at: DateTime<Utc>,
    /// Piastres. Sum of the line items before discount and tax.
    pub subtotal: i32,
    pub discount_amount: i32,
    pub tax_amount: i32,
    /// Always 0: Madar has no service-charge concept. Present so the field is
    /// stable if one is ever introduced.
    pub service_charge: i32,
    /// `subtotal - discount_amount + tax_amount`. Deliberately COMPUTED rather
    /// than read from `orders.total_amount`, which also carries the delivery
    /// fee — this figure is the order's own value and nothing else. Tips are
    /// excluded too (they are not part of `total_amount` in the first place).
    pub total_amount: i32,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AnalyticsResponse {
    pub branch_id: Uuid,
    pub branch_name: String,
    /// IANA zone the business days were resolved in.
    pub timezone: String,
    pub from: NaiveDate,
    pub to: NaiveDate,
    /// The exact half-open instant window `[from_utc, to_utc)` the figures
    /// cover, echoed so there is never a question about what was included.
    pub from_utc: DateTime<Utc>,
    pub to_utc: DateTime<Utc>,
    /// Orders in the window. Voided and refunded orders are excluded here and
    /// everywhere below — they are not returned at all.
    pub total_orders: i64,
    pub subtotal: i64,
    pub total_discount: i64,
    pub total_tax: i64,
    pub total_service_charge: i64,
    /// Sum of the per-order `total_amount`.
    pub total_revenue: i64,
    /// `total_revenue / total_orders`, truncated to whole piastres. 0 when the
    /// window is empty.
    pub avg_order_total: i64,
    /// Echo of the paging actually applied, and how many rows came back.
    pub limit: Option<i64>,
    pub offset: i64,
    pub returned: i64,
    pub orders: Vec<AnalyticsOrder>,
}

#[derive(sqlx::FromRow)]
struct BranchWindow {
    name: String,
    zone: String,
    from_utc: DateTime<Utc>,
    to_utc: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct Totals {
    total_orders: i64,
    subtotal: i64,
    total_discount: i64,
    total_tax: i64,
    total_revenue: i64,
}

#[utoipa::path(
    get,
    path = "/integrations/analytics/orders",
    tag = "integrations",
    params(AnalyticsQuery),
    responses(
        (status = 200, description = "Branch order analytics for the window", body = AnalyticsResponse),
        crate::errors::AppErrorResponse
    ),
    security(("basic_integration" = []))
)]
pub async fn analytics_orders(
    caller: IntegrationCaller,
    pool: web::Data<PgPool>,
    q: web::Query<AnalyticsQuery>,
) -> Result<HttpResponse, AppError> {
    // The branch is NOT a request parameter: it comes from the credential
    // alone, so a partner has nothing to pass and nothing to get wrong, and
    // there is no id to tamper with in the first place.
    if q.from > q.to {
        return Err(AppError::BadRequest(
            "`from` must be on or before `to`".into(),
        ));
    }
    let limit = match q.limit {
        Some(n) if n < 1 => return Err(AppError::BadRequest("`limit` must be >= 1".into())),
        Some(n) => Some(n.min(MAX_PAGE)),
        None => None,
    };
    let offset = match q.offset {
        Some(n) if n < 0 => return Err(AppError::BadRequest("`offset` must be >= 0".into())),
        Some(n) => n,
        None => 0,
    };

    // Everything past authentication runs on the org's RLS-scoped pool, so the
    // database itself refuses cross-tenant reads.
    let db = Db::for_org(pool.get_ref(), caller.org_id).await;

    // Resolve the branch's effective timezone (branch → org → Cairo) and turn
    // the two calendar dates into the half-open UTC window they denote there.
    // `to` is inclusive, hence `+ 1 day` on the exclusive end. Doing this in
    // Postgres means DST transitions are handled by the tz database rather
    // than by us guessing an offset.
    let win: BranchWindow = sqlx::query_as(
        r#"
        SELECT b.name,
               COALESCE(b.timezone::text, o.timezone::text, $2)                       AS zone,
               ($3::date::timestamp AT TIME ZONE
                    COALESCE(b.timezone::text, o.timezone::text, $2))                 AS from_utc,
               (($4::date + 1)::timestamp AT TIME ZONE
                    COALESCE(b.timezone::text, o.timezone::text, $2))                 AS to_utc
          FROM branches b
          JOIN organizations o ON o.id = b.org_id
         WHERE b.id = $1 AND b.deleted_at IS NULL
        "#,
    )
    .bind(caller.branch_id)
    .bind(DEFAULT_TZ)
    .bind(q.from)
    .bind(q.to)
    .fetch_optional(db.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    // Aggregates cover the WHOLE window regardless of paging, so a partner
    // reading page 3 still sees the same totals as page 1.
    let totals: Totals = sqlx::query_as(&format!(
        r#"
        SELECT COUNT(*)::bigint                                                    AS total_orders,
               COALESCE(SUM(o.subtotal), 0)::bigint                                AS subtotal,
               COALESCE(SUM(o.discount_amount), 0)::bigint                         AS total_discount,
               COALESCE(SUM(o.tax_amount), 0)::bigint                              AS total_tax,
               COALESCE(SUM(o.subtotal - o.discount_amount + o.tax_amount), 0)::bigint
                                                                                   AS total_revenue
          FROM orders o
         WHERE o.branch_id = $1
           AND o.created_at >= $2
           AND o.created_at <  $3
           AND o.{SOLD}
        "#
    ))
    .bind(caller.branch_id)
    .bind(win.from_utc)
    .bind(win.to_utc)
    .fetch_one(db.get_ref())
    .await?;

    // LIMIT NULL means "no limit" in Postgres, so the optional page size binds
    // straight through without a second query shape.
    let orders: Vec<AnalyticsOrder> = sqlx::query_as(&format!(
        r#"
        SELECT o.id                                                AS order_id,
               o.order_number,
               o.order_ref,
               o.status::text                                      AS status,
               (o.created_at AT TIME ZONE $4)::date                AS business_date,
               o.created_at,
               o.subtotal,
               o.discount_amount,
               o.tax_amount,
               0                                                   AS service_charge,
               (o.subtotal - o.discount_amount + o.tax_amount)::int AS total_amount
          FROM orders o
         WHERE o.branch_id = $1
           AND o.created_at >= $2
           AND o.created_at <  $3
           AND o.{SOLD}
         ORDER BY o.created_at, o.id
         LIMIT $5 OFFSET $6
        "#
    ))
    .bind(caller.branch_id)
    .bind(win.from_utc)
    .bind(win.to_utc)
    .bind(&win.zone)
    .bind(limit)
    .bind(offset)
    .fetch_all(db.get_ref())
    .await?;

    let avg_order_total = if totals.total_orders > 0 {
        totals.total_revenue / totals.total_orders
    } else {
        0
    };

    Ok(HttpResponse::Ok().json(AnalyticsResponse {
        branch_id: caller.branch_id,
        branch_name: win.name,
        timezone: win.zone,
        from: q.from,
        to: q.to,
        from_utc: win.from_utc,
        to_utc: win.to_utc,
        total_orders: totals.total_orders,
        subtotal: totals.subtotal,
        total_discount: totals.total_discount,
        total_tax: totals.total_tax,
        total_service_charge: 0,
        total_revenue: totals.total_revenue,
        avg_order_total,
        limit,
        offset,
        returned: orders.len() as i64,
        orders,
    }))
}

// ── Operator-facing: /integrations/credentials ────────────────

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateCredentialRequest {
    /// Operator-facing label, e.g. "Rue — One Ninety".
    pub name: String,
    /// The single branch this credential may read.
    pub branch_id: Uuid,
    /// Basic-auth username. Unique across all orgs, case-insensitively.
    pub username: String,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct CredentialSummary {
    pub id: Uuid,
    pub name: String,
    pub username: String,
    pub branch_id: Uuid,
    pub branch_name: String,
    pub created_at: DateTime<Utc>,
    /// Last successful authentication, or null if the partner has never pulled.
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// Returned ONLY by create and rotate. The secret is bcrypt-hashed on the way
/// in and is not recoverable afterwards, so the dashboard must show it once and
/// tell the operator to copy it.
#[derive(Debug, Serialize, ToSchema)]
pub struct CredentialWithSecret {
    #[serde(flatten)]
    pub credential: CredentialSummary,
    pub secret: String,
}

/// Generate a partner secret.
///
/// Two v4 UUIDs' worth of hex — ~244 bits from the OS CSPRNG that `uuid`
/// already pulls from. Avoids taking on `rand` for one call site.
fn generate_secret() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn hash_secret(secret: &str) -> Result<String, AppError> {
    bcrypt::hash(secret, bcrypt::DEFAULT_COST).map_err(|_| AppError::Internal)
}

/// The org whose credentials this request may touch. Super admins carry no org
/// of their own, so they must pin one with `X-Org-Id` like everywhere else.
fn scope_org(req: &HttpRequest, claims: &Claims) -> Result<Uuid, AppError> {
    claims
        .scope_org(crate::auth::middleware::header_org_id(req))
        .ok_or_else(|| AppError::BadRequest("No organization in scope".into()))
}

#[utoipa::path(
    post,
    path = "/integrations/credentials",
    tag = "integrations",
    request_body = CreateCredentialRequest,
    responses(
        (status = 201, description = "Credential created; `secret` is shown this once only", body = CredentialWithSecret),
        crate::errors::AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn create_credential(
    req: HttpRequest,
    db: Db,
    body: web::Json<CreateCredentialRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require_org_admin(&claims)?;
    let org_id = scope_org(&req, &claims)?;

    let name = body.name.trim();
    let username = body.username.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("`name` is required".into()));
    }
    if username.len() < 3 || username.len() > 64 {
        return Err(AppError::BadRequest(
            "`username` must be 3–64 characters".into(),
        ));
    }

    // The branch must belong to the scoped org. Under RLS this SELECT can only
    // see the caller's own branches anyway; the explicit check turns a
    // cross-org id into a clean 404 instead of a foreign-key error.
    let branch_name: String = sqlx::query_scalar(
        "SELECT name FROM branches WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL",
    )
    .bind(body.branch_id)
    .bind(org_id)
    .fetch_optional(db.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    let secret = generate_secret();
    let secret_hash = hash_secret(&secret)?;

    let row: (Uuid, DateTime<Utc>) = sqlx::query_as(
        "INSERT INTO integration_credentials
             (org_id, branch_id, name, username, secret_hash, created_by)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id, created_at",
    )
    .bind(org_id)
    .bind(body.branch_id)
    .bind(name)
    .bind(username)
    .bind(&secret_hash)
    .bind(claims.user_id_safe().ok())
    .fetch_one(db.get_ref())
    .await
    .map_err(|e| match &e {
        // The cluster-wide unique index on lower(username).
        sqlx::Error::Database(d) if d.code().as_deref() == Some("23505") => {
            AppError::Conflict("That username is already taken".into())
        }
        _ => AppError::Db(e),
    })?;

    Ok(HttpResponse::Created().json(CredentialWithSecret {
        credential: CredentialSummary {
            id: row.0,
            name: name.to_string(),
            username: username.to_string(),
            branch_id: body.branch_id,
            branch_name,
            created_at: row.1,
            last_used_at: None,
            revoked_at: None,
        },
        secret,
    }))
}

#[utoipa::path(
    get,
    path = "/integrations/credentials",
    tag = "integrations",
    responses(
        (status = 200, description = "Credentials in this org (secrets never included)", body = Vec<CredentialSummary>),
        crate::errors::AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn list_credentials(req: HttpRequest, db: Db) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require_org_admin(&claims)?;
    let org_id = scope_org(&req, &claims)?;

    let rows: Vec<CredentialSummary> = sqlx::query_as(
        "SELECT c.id, c.name, c.username, c.branch_id, b.name AS branch_name,
                c.created_at, c.last_used_at, c.revoked_at
           FROM integration_credentials c
           JOIN branches b ON b.id = c.branch_id
          WHERE c.org_id = $1
          ORDER BY c.created_at DESC",
    )
    .bind(org_id)
    .fetch_all(db.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    post,
    path = "/integrations/credentials/{id}/rotate",
    tag = "integrations",
    params(("id" = Uuid, Path, description = "Credential ID")),
    responses(
        (status = 200, description = "New secret issued; shown this once only", body = CredentialWithSecret),
        crate::errors::AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn rotate_credential(
    req: HttpRequest,
    db: Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require_org_admin(&claims)?;
    let org_id = scope_org(&req, &claims)?;

    let secret = generate_secret();
    let secret_hash = hash_secret(&secret)?;

    // Rotating a revoked credential would silently bring it back to life;
    // reactivation should be a deliberate act, so those are left out.
    let row: Option<CredentialSummary> = sqlx::query_as(
        "WITH updated AS (
             UPDATE integration_credentials
                SET secret_hash = $1, updated_at = now()
              WHERE id = $2 AND org_id = $3 AND revoked_at IS NULL
              RETURNING id, name, username, branch_id, created_at, last_used_at, revoked_at
         )
         SELECT u.id, u.name, u.username, u.branch_id, b.name AS branch_name,
                u.created_at, u.last_used_at, u.revoked_at
           FROM updated u JOIN branches b ON b.id = u.branch_id",
    )
    .bind(&secret_hash)
    .bind(*id)
    .bind(org_id)
    .fetch_optional(db.get_ref())
    .await?;

    let credential = row.ok_or_else(|| AppError::NotFound("Credential not found".into()))?;
    Ok(HttpResponse::Ok().json(CredentialWithSecret { credential, secret }))
}

#[utoipa::path(
    delete,
    path = "/integrations/credentials/{id}",
    tag = "integrations",
    params(("id" = Uuid, Path, description = "Credential ID")),
    responses(
        (status = 204, description = "Credential revoked"),
        crate::errors::AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn revoke_credential(
    req: HttpRequest,
    db: Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require_org_admin(&claims)?;
    let org_id = scope_org(&req, &claims)?;

    // Soft revocation: the row survives as the record of who held access.
    // Idempotent — re-revoking an already-revoked credential still succeeds.
    let affected = sqlx::query(
        "UPDATE integration_credentials
            SET revoked_at = COALESCE(revoked_at, now()), updated_at = now()
          WHERE id = $1 AND org_id = $2",
    )
    .bind(*id)
    .bind(org_id)
    .execute(db.get_ref())
    .await?
    .rows_affected();

    if affected == 0 {
        return Err(AppError::NotFound("Credential not found".into()));
    }
    Ok(HttpResponse::NoContent().finish())
}
