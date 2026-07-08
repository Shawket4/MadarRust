//! Kitchen station CRUD, the category→station + per-item routing maps, and the
//! per-branch routing-mode override. Admin surface (permission resource
//! `kitchen_stations`). Mirrors the branches/tills CRUD shape.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::{extract_claims, require_branch_access};
use crate::branches::handlers::PrinterBrand;
use crate::errors::{AppError, AppErrorResponse};
use crate::permissions::checker::check_permission;

// ── Models ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct KitchenStation {
    pub id: Uuid,
    pub org_id: Uuid,
    pub branch_id: Uuid,
    pub name: String,
    pub name_translations: serde_json::Value,
    pub sort_order: i32,
    pub printer_brand: Option<PrinterBrand>,
    pub printer_ip: Option<String>,
    pub printer_port: Option<i32>,
    pub is_default: bool,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const STATION_COLS: &str = "id, org_id, branch_id, name, name_translations, sort_order, \
     printer_brand, printer_ip, printer_port, is_default, is_active, created_at, updated_at";

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BranchQuery {
    pub branch_id: Uuid,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateStationRequest {
    pub branch_id: Uuid,
    pub name: String,
    #[serde(default)]
    pub sort_order: Option<i32>,
    #[serde(default)]
    pub printer_brand: Option<PrinterBrand>,
    #[serde(default)]
    pub printer_ip: Option<String>,
    #[serde(default)]
    pub printer_port: Option<i32>,
    #[serde(default)]
    pub is_default: Option<bool>,
    #[serde(default)]
    pub is_active: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateStationRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub sort_order: Option<i32>,
    #[serde(default)]
    pub printer_brand: Option<Option<PrinterBrand>>,
    #[serde(default)]
    pub printer_ip: Option<Option<String>>,
    #[serde(default)]
    pub printer_port: Option<Option<i32>>,
    #[serde(default)]
    pub is_default: Option<bool>,
    #[serde(default)]
    pub is_active: Option<bool>,
}

async fn fetch_station(pool: &PgPool, id: Uuid) -> Result<KitchenStation, AppError> {
    sqlx::query_as::<_, KitchenStation>(&format!(
        "SELECT {STATION_COLS} FROM kitchen_stations WHERE id = $1 AND deleted_at IS NULL"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Station not found".into()))
}

// ── Station CRUD ──────────────────────────────────────────────

#[utoipa::path(get, path = "/kitchen/stations", tag = "kitchen", params(BranchQuery),
    responses((status = 200, body = Vec<KitchenStation>), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn list_stations(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<BranchQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "kitchen_stations", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;
    let rows = sqlx::query_as::<_, KitchenStation>(&format!(
        "SELECT {STATION_COLS} FROM kitchen_stations \
         WHERE branch_id = $1 AND deleted_at IS NULL ORDER BY sort_order, lower(name)"
    ))
    .bind(query.branch_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(post, path = "/kitchen/stations", tag = "kitchen", request_body = CreateStationRequest,
    responses((status = 201, body = KitchenStation), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn create_station(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateStationRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "kitchen_stations", "create").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;

    let name = body.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("Station name is required".into()));
    }
    let org_id: Uuid =
        sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
            .bind(body.branch_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    let is_default = body.is_default.unwrap_or(false);
    let mut tx = pool.get_ref().begin().await?;
    if is_default {
        sqlx::query(
            "UPDATE kitchen_stations SET is_default = false, updated_at = now() \
             WHERE branch_id = $1 AND is_default AND deleted_at IS NULL",
        )
        .bind(body.branch_id)
        .execute(&mut *tx)
        .await?;
    }
    let station = sqlx::query_as::<_, KitchenStation>(&format!(
        "INSERT INTO kitchen_stations \
            (org_id, branch_id, name, sort_order, printer_brand, printer_ip, printer_port, is_default, is_active) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING {STATION_COLS}"
    ))
    .bind(org_id)
    .bind(body.branch_id)
    .bind(name)
    .bind(body.sort_order.unwrap_or(0))
    .bind(&body.printer_brand)
    .bind(&body.printer_ip)
    .bind(body.printer_port)
    .bind(is_default)
    .bind(body.is_active.unwrap_or(true))
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(HttpResponse::Created().json(station))
}

#[utoipa::path(patch, path = "/kitchen/stations/{id}", tag = "kitchen",
    params(("id" = Uuid, Path, description = "Station ID")), request_body = UpdateStationRequest,
    responses((status = 200, body = KitchenStation), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn update_station(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<UpdateStationRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "kitchen_stations", "update").await?;
    let existing = fetch_station(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, existing.branch_id).await?;

    let new_name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if body.name.as_deref().is_some_and(|s| s.trim().is_empty()) {
        return Err(AppError::BadRequest("Station name cannot be empty".into()));
    }

    let mut tx = pool.get_ref().begin().await?;
    if body.is_default == Some(true) {
        sqlx::query(
            "UPDATE kitchen_stations SET is_default = false, updated_at = now() \
             WHERE branch_id = $1 AND is_default AND deleted_at IS NULL AND id <> $2",
        )
        .bind(existing.branch_id)
        .bind(*id)
        .execute(&mut *tx)
        .await?;
    }
    let station = sqlx::query_as::<_, KitchenStation>(&format!(
        "UPDATE kitchen_stations SET \
             name          = COALESCE($2, name), \
             sort_order    = COALESCE($3, sort_order), \
             printer_brand = CASE WHEN $4 THEN $5 ELSE printer_brand END, \
             printer_ip    = CASE WHEN $6 THEN $7 ELSE printer_ip END, \
             printer_port  = CASE WHEN $8 THEN $9 ELSE printer_port END, \
             is_default    = COALESCE($10, is_default), \
             is_active     = COALESCE($11, is_active), \
             updated_at    = now() \
         WHERE id = $1 AND deleted_at IS NULL RETURNING {STATION_COLS}"
    ))
    .bind(*id)
    .bind(new_name)
    .bind(body.sort_order)
    .bind(body.printer_brand.is_some())
    .bind(body.printer_brand.clone().flatten())
    .bind(body.printer_ip.is_some())
    .bind(body.printer_ip.clone().flatten())
    .bind(body.printer_port.is_some())
    .bind(body.printer_port.flatten())
    .bind(body.is_default)
    .bind(body.is_active)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(HttpResponse::Ok().json(station))
}

#[utoipa::path(delete, path = "/kitchen/stations/{id}", tag = "kitchen",
    params(("id" = Uuid, Path, description = "Station ID")),
    responses((status = 204), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn delete_station(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "kitchen_stations", "delete").await?;
    let existing = fetch_station(pool.get_ref(), *id).await?;
    require_branch_access(pool.get_ref(), &claims, existing.branch_id).await?;
    sqlx::query(
        "UPDATE kitchen_stations SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(*id)
    .execute(pool.get_ref())
    .await?;
    Ok(HttpResponse::NoContent().finish())
}

// ── Routing config ────────────────────────────────────────────

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct CategoryRoute {
    pub category_id: Uuid,
    pub station_id: Uuid,
}

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct ItemRoute {
    pub menu_item_id: Uuid,
    pub station_id: Uuid,
}

#[derive(Serialize, ToSchema)]
pub struct StationRoutes {
    pub categories: Vec<CategoryRoute>,
    pub items: Vec<ItemRoute>,
}

#[utoipa::path(get, path = "/kitchen/routes", tag = "kitchen", params(BranchQuery),
    responses((status = 200, body = StationRoutes), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn list_routes(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<BranchQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "kitchen_stations", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;
    let categories = sqlx::query_as::<_, CategoryRoute>(
        "SELECT category_id, station_id FROM category_station_routes WHERE branch_id = $1",
    )
    .bind(query.branch_id)
    .fetch_all(pool.get_ref())
    .await?;
    let items = sqlx::query_as::<_, ItemRoute>(
        "SELECT menu_item_id, station_id FROM menu_item_station_routes WHERE branch_id = $1",
    )
    .bind(query.branch_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(StationRoutes { categories, items }))
}

#[derive(Deserialize, ToSchema)]
pub struct CategoryRouteInput {
    pub branch_id: Uuid,
    pub category_id: Uuid,
    pub station_id: Uuid,
}

#[utoipa::path(put, path = "/kitchen/routes/category", tag = "kitchen", request_body = CategoryRouteInput,
    responses((status = 204), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn put_category_route(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CategoryRouteInput>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "kitchen_stations", "update").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    require_station_in_branch(pool.get_ref(), body.station_id, body.branch_id).await?;
    sqlx::query(
        "INSERT INTO category_station_routes (branch_id, category_id, station_id) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (branch_id, category_id) DO UPDATE SET station_id = EXCLUDED.station_id",
    )
    .bind(body.branch_id)
    .bind(body.category_id)
    .bind(body.station_id)
    .execute(pool.get_ref())
    .await?;
    Ok(HttpResponse::NoContent().finish())
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CategoryRouteKey {
    pub branch_id: Uuid,
    pub category_id: Uuid,
}

#[utoipa::path(delete, path = "/kitchen/routes/category", tag = "kitchen", params(CategoryRouteKey),
    responses((status = 204), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn delete_category_route(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<CategoryRouteKey>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "kitchen_stations", "update").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;
    sqlx::query("DELETE FROM category_station_routes WHERE branch_id = $1 AND category_id = $2")
        .bind(query.branch_id)
        .bind(query.category_id)
        .execute(pool.get_ref())
        .await?;
    Ok(HttpResponse::NoContent().finish())
}

#[derive(Deserialize, ToSchema)]
pub struct ItemRouteInput {
    pub branch_id: Uuid,
    pub menu_item_id: Uuid,
    pub station_id: Uuid,
}

#[utoipa::path(put, path = "/kitchen/routes/item", tag = "kitchen", request_body = ItemRouteInput,
    responses((status = 204), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn put_item_route(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<ItemRouteInput>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "kitchen_stations", "update").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    require_station_in_branch(pool.get_ref(), body.station_id, body.branch_id).await?;
    sqlx::query(
        "INSERT INTO menu_item_station_routes (branch_id, menu_item_id, station_id) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (branch_id, menu_item_id) DO UPDATE SET station_id = EXCLUDED.station_id",
    )
    .bind(body.branch_id)
    .bind(body.menu_item_id)
    .bind(body.station_id)
    .execute(pool.get_ref())
    .await?;
    Ok(HttpResponse::NoContent().finish())
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ItemRouteKey {
    pub branch_id: Uuid,
    pub menu_item_id: Uuid,
}

#[utoipa::path(delete, path = "/kitchen/routes/item", tag = "kitchen", params(ItemRouteKey),
    responses((status = 204), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn delete_item_route(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<ItemRouteKey>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "kitchen_stations", "update").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;
    sqlx::query("DELETE FROM menu_item_station_routes WHERE branch_id = $1 AND menu_item_id = $2")
        .bind(query.branch_id)
        .bind(query.menu_item_id)
        .execute(pool.get_ref())
        .await?;
    Ok(HttpResponse::NoContent().finish())
}

async fn require_station_in_branch(
    pool: &PgPool,
    station_id: Uuid,
    branch_id: Uuid,
) -> Result<(), AppError> {
    let ok: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM kitchen_stations \
         WHERE id = $1 AND branch_id = $2 AND deleted_at IS NULL)",
    )
    .bind(station_id)
    .bind(branch_id)
    .fetch_one(pool)
    .await?;
    if ok {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            "Station does not belong to this branch".into(),
        ))
    }
}

// ── Routing mode ──────────────────────────────────────────────

#[derive(Serialize, ToSchema)]
pub struct RoutingModeResponse {
    /// Stored override (`kds` | `till` | `both`), or null when auto.
    pub mode: Option<String>,
    /// What actually applies right now (auto resolves to kds-if-stations-else-till).
    pub effective: String,
}

#[utoipa::path(get, path = "/kitchen/routing-mode", tag = "kitchen", params(BranchQuery),
    responses((status = 200, body = RoutingModeResponse), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn get_routing_mode(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<BranchQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "kitchen_stations", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;
    let mode: Option<String> =
        sqlx::query_scalar("SELECT kitchen_routing_mode::text FROM branches WHERE id = $1")
            .bind(query.branch_id)
            .fetch_optional(pool.get_ref())
            .await?
            .flatten();
    let effective = super::effective_routing_mode(pool.get_ref(), query.branch_id).await?;
    Ok(HttpResponse::Ok().json(RoutingModeResponse { mode, effective }))
}

#[derive(Deserialize, ToSchema)]
pub struct SetRoutingModeRequest {
    pub branch_id: Uuid,
    /// `kds` | `till` | `both`, or null to clear the override (back to auto).
    #[serde(default)]
    pub mode: Option<String>,
}

#[utoipa::path(put, path = "/kitchen/routing-mode", tag = "kitchen", request_body = SetRoutingModeRequest,
    responses((status = 200, body = RoutingModeResponse), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn set_routing_mode(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<SetRoutingModeRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "kitchen_stations", "update").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    if let Some(m) = body.mode.as_deref()
        && !matches!(m, "kds" | "till" | "both" | "off")
    {
        return Err(AppError::BadRequest(
            "mode must be kds, till, both, or off".into(),
        ));
    }
    sqlx::query(
        "UPDATE branches SET kitchen_routing_mode = $2::kitchen_routing_mode WHERE id = $1",
    )
    .bind(body.branch_id)
    .bind(body.mode.as_deref())
    .execute(pool.get_ref())
    .await?;
    let effective = super::effective_routing_mode(pool.get_ref(), body.branch_id).await?;
    Ok(HttpResponse::Ok().json(RoutingModeResponse {
        mode: body.mode.clone(),
        effective,
    }))
}
