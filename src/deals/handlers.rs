//! Deal endpoints (§2.3). Reading uses `menu.items.read`; writing `menu.deals.edit`.

use actix_web::{HttpRequest, HttpResponse, web};
use uuid::Uuid;

use crate::{
    authz::{Cap, require::require},
    combos::not_yet,
    deals::types::{DealBranchWrite, DealListQuery, DealRule, DealWrite},
    errors::{AppError, AppErrorResponse},
    menu::bases::extract_claims,
};

#[utoipa::path(
    get,
    path = "/deals",
    tag = "menu",
    params(DealListQuery),
    responses((status = 200, description = "The org's deal rules (not deleted)", body = Vec<DealRule>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_deals(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<DealListQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuItemsRead, None).await?;
    let _ = query;
    Err(not_yet())
}

#[utoipa::path(
    post,
    path = "/deals",
    tag = "menu",
    request_body = DealWrite,
    responses((status = 201, description = "The deal rule", body = DealRule), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_deal(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<DealWrite>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuDealsEdit, None).await?;
    let _ = body;
    Err(not_yet())
}

#[utoipa::path(
    put,
    path = "/deals/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Deal rule ID")),
    request_body = DealWrite,
    responses((status = 200, description = "The deal rule", body = DealRule), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_deal(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<DealWrite>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuDealsEdit, None).await?;
    let _ = (*id, body);
    Err(not_yet())
}

#[utoipa::path(
    delete,
    path = "/deals/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Deal rule ID")),
    responses((status = 204, description = "Soft-deleted; applied deals keep pointing at it"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_deal(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuDealsEdit, None).await?;
    let _ = *id;
    Err(not_yet())
}

#[utoipa::path(
    put,
    path = "/deals/{id}/branches/{branch_id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Deal rule ID"), ("branch_id" = Uuid, Path, description = "Branch ID")),
    request_body = DealBranchWrite,
    responses((status = 204, description = "The branch's on/off override saved"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_deal_branch(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(Uuid, Uuid)>,
    body: web::Json<DealBranchWrite>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let (_, branch_id) = *path;
    require(pool.get_ref(), &claims, Cap::MenuDealsEdit, Some(branch_id)).await?;
    let _ = body;
    Err(not_yet())
}

#[utoipa::path(
    delete,
    path = "/deals/{id}/branches/{branch_id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Deal rule ID"), ("branch_id" = Uuid, Path, description = "Branch ID")),
    responses((status = 204, description = "The branch inherits the rule's own active flag"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_deal_branch(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let (_, branch_id) = *path;
    require(pool.get_ref(), &claims, Cap::MenuDealsEdit, Some(branch_id)).await?;
    Err(not_yet())
}
