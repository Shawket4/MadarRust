//! Combo endpoints (§2.2 as adjusted by §11). Reading uses `menu.items.read`;
//! writing `menu.combos.edit`; selling a combo needs nothing beyond selling.

use actix_web::{HttpRequest, HttpResponse, web};
use uuid::Uuid;

use crate::{
    authz::{Cap, require::require},
    combos::{
        not_yet,
        types::{
            ChannelOverride, Combo, ComboEconomics, ComboEconomicsRequest, ComboGetQuery,
            ComboListQuery, ComboSettings, ComboSettingsWrite, ComboWrite, MealLinkWrite,
            PaginatedCombos,
        },
    },
    errors::{AppError, AppErrorResponse},
    menu::bases::extract_claims,
};

#[utoipa::path(
    get,
    path = "/combos",
    tag = "menu",
    params(ComboListQuery),
    responses((status = 200, description = "The org's combos", body = PaginatedCombos), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_combos(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<ComboListQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuItemsRead, None).await?;
    let _ = query;
    Err(not_yet())
}

#[utoipa::path(
    post,
    path = "/combos",
    tag = "menu",
    request_body = ComboWrite,
    responses((status = 201, description = "The combo, created in one transaction", body = Combo), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_combo(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<ComboWrite>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuCombosEdit, None).await?;
    let _ = body;
    Err(not_yet())
}

#[utoipa::path(
    get,
    path = "/combos/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "The combo's menu item id"), ComboGetQuery),
    responses((status = 200, description = "The combo with its economics", body = Combo), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_combo(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    query: web::Query<ComboGetQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuItemsRead, query.branch_id).await?;
    let _ = *id;
    Err(not_yet())
}

#[utoipa::path(
    put,
    path = "/combos/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "The combo's menu item id")),
    request_body = ComboWrite,
    responses((status = 200, description = "The combo; slots and choices diffed in place by id", body = Combo), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_combo(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<ComboWrite>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuCombosEdit, None).await?;
    let _ = (*id, body);
    Err(not_yet())
}

#[utoipa::path(
    post,
    path = "/combos/economics",
    tag = "menu",
    request_body = ComboEconomicsRequest,
    responses((status = 200, description = "The editor's live panel; nothing is saved", body = ComboEconomics), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn combo_economics(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<ComboEconomicsRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuItemsRead, body.branch_id).await?;
    Err(not_yet())
}

#[utoipa::path(
    put,
    path = "/menu-items/{id}/meal",
    tag = "menu",
    params(("id" = Uuid, Path, description = "A kind=item menu item")),
    request_body = MealLinkWrite,
    responses((status = 204, description = "Linked (or unlinked when both fields are null)"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_meal(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<Option<MealLinkWrite>>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuCombosEdit, None).await?;
    let _ = (*id, body);
    Err(not_yet())
}

#[utoipa::path(
    get,
    path = "/settings/combos",
    tag = "menu",
    responses((status = 200, description = "The minimum margin and the channel toggles", body = ComboSettings), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_settings(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::OrgSettingsRead, None).await?;
    Err(not_yet())
}

#[utoipa::path(
    put,
    path = "/settings/combos",
    tag = "menu",
    request_body = ComboSettingsWrite,
    responses((status = 200, description = "The settings as saved", body = ComboSettings), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_settings(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<ComboSettingsWrite>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuCombosEdit, None).await?;
    let _ = body;
    Err(not_yet())
}

#[utoipa::path(
    put,
    path = "/settings/combos/branches/{branch_id}",
    tag = "menu",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    request_body = ChannelOverride,
    responses((status = 204, description = "The branch's channel override saved (all null = inherit)"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_branch_channels(
    req: HttpRequest,
    pool: crate::db::Db,
    branch_id: web::Path<Uuid>,
    body: web::Json<ChannelOverride>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(
        pool.get_ref(),
        &claims,
        Cap::MenuCombosEdit,
        Some(*branch_id),
    )
    .await?;
    let _ = body;
    Err(not_yet())
}

#[utoipa::path(
    delete,
    path = "/settings/combos/branches/{branch_id}",
    tag = "menu",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    responses((status = 204, description = "The branch inherits the org's toggles"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_branch_channels(
    req: HttpRequest,
    pool: crate::db::Db,
    branch_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(
        pool.get_ref(),
        &claims,
        Cap::MenuCombosEdit,
        Some(*branch_id),
    )
    .await?;
    Err(not_yet())
}
