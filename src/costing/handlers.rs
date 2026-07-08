//! Cost endpoints — dashboard-facing views of the cost engine.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::Deserialize;
use utoipa::IntoParams;
use uuid::Uuid;

use crate::{
    auth::guards::require_same_org, auth::jwt::Claims, errors::AppError,
    permissions::checker::check_permission,
};

use super::service;

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    use actix_web::HttpMessage;
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

#[derive(Deserialize, IntoParams)]
pub struct OrgQuery {
    pub org_id: Uuid,
    /// Optional: resolve costs at this branch's actual cost (falling back to the
    /// org default per ingredient). Omit for the org default / standard cost.
    pub branch_id: Option<Uuid>,
}

// ── GET /costing/menu-items ───────────────────────────────────

#[utoipa::path(
    get,
    path = "/costing/menu-items",
    tag = "costing",
    params(OrgQuery),
    responses((status = 200, description = "Current recipe-cost rollup per SKU (piastres)", body = [service::SkuCost]), crate::errors::AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_sku_costs(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<OrgQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    require_same_org(&claims, Some(query.org_id))?;

    let costs = service::org_sku_costs(pool.get_ref(), query.org_id, query.branch_id).await?;
    Ok(HttpResponse::Ok().json(costs))
}

// ── GET /costing/addon-items ──────────────────────────────────

#[utoipa::path(
    get,
    path = "/costing/addon-items",
    tag = "costing",
    params(OrgQuery),
    responses((status = 200, description = "Current ingredient-cost rollup per addon (piastres)", body = [service::AddonCost]), crate::errors::AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_addon_costs(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<OrgQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    require_same_org(&claims, Some(query.org_id))?;

    let costs = service::org_addon_costs(pool.get_ref(), query.org_id, query.branch_id).await?;
    Ok(HttpResponse::Ok().json(costs))
}
