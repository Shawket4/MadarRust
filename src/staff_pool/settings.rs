//! The pool's settings: how many drinks a day, and which items count.
//!
//! Scoped exactly as `loyalty_settings` is, and for the same reason: a row with
//! `branch_id IS NULL` is the org-wide default, and a branch row **replaces it
//! wholesale**. Wholesale rather than field-by-field because a half-inherited
//! allowance is impossible to reason about at a counter — an owner looking at a
//! branch sees exactly the numbers that branch runs on. Deleting the branch row
//! puts it back on the org default.
//!
//! An EMPTY `eligible_item_ids` means the pool is off, whatever `enabled` says.
//! That is the rule the shared engine enforces (`engine::decide`), not a
//! convention: a pool with nothing to spend on is not a pool.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::authz::Cap;
use crate::authz::require::require;
use crate::delivery::require_branch_access;
use crate::errors::{AppError, AppErrorResponse};
use crate::loyalty::resolve_branch_org;
use crate::orgs::handlers::extract_claims;

use super::engine;

/// The settings as the API states them, and as the PUT body accepts them.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StaffPoolSettings {
    pub org_id: Uuid,
    /// `null` = the org-wide default. A branch id = that branch's override.
    pub branch_id: Option<Uuid>,
    /// The owner's master switch for this scope.
    #[serde(default)]
    pub enabled: bool,
    /// Staff drinks this branch may give in one business day.
    #[serde(default)]
    pub daily_allowance: i32,
    /// The menu items that count. EMPTY = the pool is off.
    #[serde(default)]
    pub eligible_item_ids: Vec<Uuid>,
}

impl StaffPoolSettings {
    /// What a scope runs on before anyone has saved anything: off.
    pub fn defaults(org_id: Uuid, branch_id: Option<Uuid>) -> Self {
        Self { org_id, branch_id, enabled: false, daily_allowance: 0, eligible_item_ids: Vec::new() }
    }

    /// The same settings as the shared engine takes them. This is the only
    /// bridge between the API shape and the rule, so the rule never learns
    /// about uuids or rows.
    pub fn for_engine(&self) -> engine::StaffPoolSettings {
        engine::StaffPoolSettings {
            enabled: self.enabled,
            daily_allowance: self.daily_allowance,
            eligible_item_ids: self.eligible_item_ids.iter().map(Uuid::to_string).collect(),
        }
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    org_id: Uuid,
    branch_id: Option<Uuid>,
    enabled: bool,
    daily_allowance: i32,
    eligible_item_ids: Vec<Uuid>,
}

impl From<Row> for StaffPoolSettings {
    fn from(r: Row) -> Self {
        Self {
            org_id: r.org_id,
            branch_id: r.branch_id,
            enabled: r.enabled,
            daily_allowance: r.daily_allowance,
            eligible_item_ids: r.eligible_item_ids,
        }
    }
}

const COLS: &str = "org_id, branch_id, enabled, daily_allowance, eligible_item_ids";

/// The row saved for exactly this scope, if any. Does **not** fall back.
pub async fn load_scope<'e, E>(
    exec: E,
    org_id: Uuid,
    branch_id: Option<Uuid>,
) -> Result<Option<StaffPoolSettings>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let row: Option<Row> = sqlx::query_as(&format!(
        "SELECT {COLS} FROM staff_pool_settings WHERE org_id = $1 \
         AND COALESCE(branch_id, '00000000-0000-0000-0000-000000000000'::uuid) \
           = COALESCE($2::uuid, '00000000-0000-0000-0000-000000000000'::uuid)"
    ))
    .bind(org_id)
    .bind(branch_id)
    .fetch_optional(exec)
    .await?;
    Ok(row.map(StaffPoolSettings::from))
}

/// What a branch actually runs on: its own override, else the org default, else
/// off. This is the only reader the record and replay paths use.
pub async fn load_effective(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
) -> Result<StaffPoolSettings, AppError> {
    if let Some(s) = load_scope(pool, org_id, Some(branch_id)).await? {
        return Ok(s);
    }
    if let Some(s) = load_scope(pool, org_id, None).await? {
        // Reported under the branch that asked, so a caller never has to know
        // which scope answered.
        return Ok(StaffPoolSettings { branch_id: Some(branch_id), ..s });
    }
    Ok(StaffPoolSettings::defaults(org_id, Some(branch_id)))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ScopeQuery {
    /// Omit for the org-wide default; supply a branch for its override.
    pub branch_id: Option<Uuid>,
}

async fn scope_org(
    pool: &PgPool,
    req: &HttpRequest,
    branch_id: Option<Uuid>,
) -> Result<(Uuid, crate::auth::jwt::Claims), AppError> {
    let claims = extract_claims(req)?;
    let org_id = match branch_id {
        Some(b) => resolve_branch_org(pool, b).await?,
        None => claims
            .scope_org(crate::auth::middleware::header_org_id(req))
            .ok_or_else(|| AppError::BadRequest("Pick an organisation first".into()))?,
    };
    Ok((org_id, claims))
}

#[utoipa::path(get, path = "/staff-pool/settings", tag = "staff_pool",
    operation_id = "get_staff_pool_settings", params(ScopeQuery),
    responses((status = 200, body = StaffPoolSettings), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn get_settings(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<ScopeQuery>,
) -> Result<HttpResponse, AppError> {
    let (org_id, claims) = scope_org(pool.get_ref(), &req, query.branch_id).await?;
    // Reading the rule is part of holding the act: a manager who may record a
    // staff drink must be able to see what the allowance is. Editing it is the
    // owner's settings capability, checked on PUT.
    require(pool.get_ref(), &claims, Cap::OrdersStaffDrinkRecord, query.branch_id).await?;
    if let Some(b) = query.branch_id {
        require_branch_access(pool.get_ref(), &claims, b).await?;
    }
    // A branch scope reports what that branch RUNS ON (inherited or its own),
    // so the dashboard shows the numbers in force rather than an empty form.
    let settings = match query.branch_id {
        Some(b) => load_effective(pool.get_ref(), org_id, b).await?,
        None => load_scope(pool.get_ref(), org_id, None)
            .await?
            .unwrap_or_else(|| StaffPoolSettings::defaults(org_id, None)),
    };
    Ok(HttpResponse::Ok().json(settings))
}

#[utoipa::path(put, path = "/staff-pool/settings", tag = "staff_pool",
    operation_id = "put_staff_pool_settings", request_body = StaffPoolSettings,
    responses((status = 200, body = StaffPoolSettings), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn put_settings(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<StaffPoolSettings>,
) -> Result<HttpResponse, AppError> {
    let incoming = body.into_inner();
    let (org_id, claims) = scope_org(pool.get_ref(), &req, incoming.branch_id).await?;
    // Setting a branch's allowance is changing the shop's rules, not working
    // the till: it is the org settings capability, as every other settings
    // screen is.
    require(pool.get_ref(), &claims, Cap::OrgSettingsEdit, incoming.branch_id).await?;
    if let Some(b) = incoming.branch_id {
        require_branch_access(pool.get_ref(), &claims, b).await?;
    }
    if incoming.daily_allowance < 0 {
        return Err(AppError::BadRequest(
            "A daily allowance cannot be less than zero".into(),
        ));
    }

    // Every eligible item must be a live item of THIS org. A stale id would
    // otherwise sit in the list silently never matching, which reads to the
    // shop as the pool being broken.
    if !incoming.eligible_item_ids.is_empty() {
        let live: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM menu_items \
              WHERE org_id = $1 AND deleted_at IS NULL AND id = ANY($2)",
        )
        .bind(org_id)
        .bind(&incoming.eligible_item_ids)
        .fetch_one(pool.get_ref())
        .await?;
        let asked = {
            let mut ids = incoming.eligible_item_ids.clone();
            ids.sort();
            ids.dedup();
            ids.len() as i64
        };
        if live != asked {
            return Err(AppError::BadRequest(
                "Some of those drinks are not on this organisation's menu any more".into(),
            ));
        }
    }

    let row: Row = sqlx::query_as(&format!(
        "INSERT INTO staff_pool_settings (org_id, branch_id, enabled, daily_allowance, eligible_item_ids) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (org_id, COALESCE(branch_id, '00000000-0000-0000-0000-000000000000'::uuid)) \
         DO UPDATE SET enabled = EXCLUDED.enabled, \
                       daily_allowance = EXCLUDED.daily_allowance, \
                       eligible_item_ids = EXCLUDED.eligible_item_ids, \
                       updated_at = now() \
         RETURNING {COLS}"
    ))
    .bind(org_id)
    .bind(incoming.branch_id)
    .bind(incoming.enabled)
    .bind(incoming.daily_allowance)
    .bind(&incoming.eligible_item_ids)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(StaffPoolSettings::from(row)))
}

#[utoipa::path(delete, path = "/staff-pool/settings", tag = "staff_pool",
    operation_id = "delete_staff_pool_settings", params(ScopeQuery),
    responses((status = 204), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn delete_settings(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<ScopeQuery>,
) -> Result<HttpResponse, AppError> {
    // The capability is asked FIRST, before the shape of the request is
    // judged: someone who may not edit settings must be told that, not handed
    // a hint about which argument was missing.
    let (org_id, claims) = scope_org(pool.get_ref(), &req, query.branch_id).await?;
    require(pool.get_ref(), &claims, Cap::OrgSettingsEdit, query.branch_id).await?;
    let branch_id = query.branch_id.ok_or_else(|| {
        AppError::BadRequest("Name the branch whose override you want to remove".into())
    })?;
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;
    sqlx::query("DELETE FROM staff_pool_settings WHERE org_id = $1 AND branch_id = $2")
        .bind(org_id)
        .bind(branch_id)
        .execute(pool.get_ref())
        .await?;
    Ok(HttpResponse::NoContent().finish())
}
