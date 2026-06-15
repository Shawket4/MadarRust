//! Delivery configuration: per-branch settings, zone rings (flat per-ring fee),
//! the POS open/close override, and per-channel menu overrides.
//!
//! Config (in-mall flat fee / hours / enable / zones) is gated by
//! `delivery_settings` (managers). The POS open/close override is gated by
//! `delivery_orders` (tellers) and can never re-open a channel the dashboard
//! disabled. Channel menu overrides reuse the `menu_items` permission. There is
//! no org-level default and no multiplier — the outside fee is the matched
//! ring's fee, the in-mall fee is the branch flat fee.

use actix_web::{web, HttpRequest, HttpResponse};
use chrono::NaiveTime;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::{extract_claims, require_branch_access, validate_channel, validate_override};
use crate::errors::{AppError, AppErrorResponse};
use crate::permissions::checker::check_permission;

// ── Branch settings ───────────────────────────────────────────

#[derive(Serialize, Deserialize, ToSchema, sqlx::FromRow)]
pub struct BranchDeliverySettings {
    pub branch_id: Uuid,
    pub in_mall_enabled: bool,
    pub outside_enabled: bool,
    pub in_mall_override: String,
    pub outside_override: String,
    #[schema(value_type = Option<String>)]
    pub in_mall_open_time: Option<NaiveTime>,
    #[schema(value_type = Option<String>)]
    pub in_mall_close_time: Option<NaiveTime>,
    #[schema(value_type = Option<String>)]
    pub outside_open_time: Option<NaiveTime>,
    #[schema(value_type = Option<String>)]
    pub outside_close_time: Option<NaiveTime>,
    pub in_mall_fee: i32,
    pub prep_time_minutes: i32,
    pub max_road_distance_meters: Option<i32>,
}

impl BranchDeliverySettings {
    fn defaults(branch_id: Uuid) -> Self {
        Self {
            branch_id,
            in_mall_enabled: false,
            outside_enabled: false,
            in_mall_override: "auto".into(),
            outside_override: "auto".into(),
            in_mall_open_time: None,
            in_mall_close_time: None,
            outside_open_time: None,
            outside_close_time: None,
            in_mall_fee: 0,
            prep_time_minutes: 20,
            max_road_distance_meters: None,
        }
    }
}

const BRANCH_SETTINGS_SELECT: &str = "SELECT branch_id, in_mall_enabled, outside_enabled, \
    in_mall_override, outside_override, in_mall_open_time, in_mall_close_time, \
    outside_open_time, outside_close_time, in_mall_fee, prep_time_minutes, \
    max_road_distance_meters FROM branch_delivery_settings WHERE branch_id = $1";

#[derive(Deserialize, IntoParams)]
pub struct BranchQuery {
    pub branch_id: Uuid,
}

#[utoipa::path(
    get, path = "/delivery/settings", tag = "delivery", params(BranchQuery),
    responses((status = 200, body = BranchDeliverySettings), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_branch_settings(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<BranchQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "delivery_settings", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    let row: Option<BranchDeliverySettings> = sqlx::query_as(BRANCH_SETTINGS_SELECT)
        .bind(query.branch_id)
        .fetch_optional(pool.get_ref())
        .await?;

    Ok(HttpResponse::Ok().json(row.unwrap_or_else(|| BranchDeliverySettings::defaults(query.branch_id))))
}

#[derive(Deserialize, ToSchema)]
pub struct BranchSettingsInput {
    pub branch_id: Uuid,
    pub in_mall_enabled: bool,
    pub outside_enabled: bool,
    #[schema(value_type = Option<String>)]
    pub in_mall_open_time: Option<NaiveTime>,
    #[schema(value_type = Option<String>)]
    pub in_mall_close_time: Option<NaiveTime>,
    #[schema(value_type = Option<String>)]
    pub outside_open_time: Option<NaiveTime>,
    #[schema(value_type = Option<String>)]
    pub outside_close_time: Option<NaiveTime>,
    pub in_mall_fee: i32,
    pub prep_time_minutes: i32,
    pub max_road_distance_meters: Option<i32>,
}

#[utoipa::path(
    put, path = "/delivery/settings", tag = "delivery",
    request_body = BranchSettingsInput,
    responses((status = 200, body = BranchDeliverySettings), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_branch_settings(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<BranchSettingsInput>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "delivery_settings", "update").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;

    if body.in_mall_fee < 0 {
        return Err(AppError::BadRequest("in_mall_fee must be >= 0".into()));
    }
    if body.prep_time_minutes < 0 {
        return Err(AppError::BadRequest("prep_time_minutes must be >= 0".into()));
    }
    if body.max_road_distance_meters.is_some_and(|d| d <= 0) {
        return Err(AppError::BadRequest("max_road_distance_meters must be > 0".into()));
    }

    // Upsert config WITHOUT touching the *_override columns (POS-owned).
    sqlx::query(
        "INSERT INTO branch_delivery_settings
            (branch_id, in_mall_enabled, outside_enabled, in_mall_open_time, in_mall_close_time,
             outside_open_time, outside_close_time, in_mall_fee, prep_time_minutes,
             max_road_distance_meters, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now())
         ON CONFLICT (branch_id) DO UPDATE SET
             in_mall_enabled = EXCLUDED.in_mall_enabled,
             outside_enabled = EXCLUDED.outside_enabled,
             in_mall_open_time = EXCLUDED.in_mall_open_time,
             in_mall_close_time = EXCLUDED.in_mall_close_time,
             outside_open_time = EXCLUDED.outside_open_time,
             outside_close_time = EXCLUDED.outside_close_time,
             in_mall_fee = EXCLUDED.in_mall_fee,
             prep_time_minutes = EXCLUDED.prep_time_minutes,
             max_road_distance_meters = EXCLUDED.max_road_distance_meters,
             updated_at = now()",
    )
    .bind(body.branch_id)
    .bind(body.in_mall_enabled)
    .bind(body.outside_enabled)
    .bind(body.in_mall_open_time)
    .bind(body.in_mall_close_time)
    .bind(body.outside_open_time)
    .bind(body.outside_close_time)
    .bind(body.in_mall_fee)
    .bind(body.prep_time_minutes)
    .bind(body.max_road_distance_meters)
    .execute(pool.get_ref())
    .await?;

    let row: BranchDeliverySettings = sqlx::query_as(BRANCH_SETTINGS_SELECT)
        .bind(body.branch_id)
        .fetch_one(pool.get_ref())
        .await?;
    Ok(HttpResponse::Ok().json(row))
}

// ── POS open/close override (teller) ──────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct AcceptingInput {
    pub branch_id: Uuid,
    /// "in_mall" | "outside"
    pub channel: String,
    /// "auto" | "open" | "closed". Named `mode` (not `override`) because a bare
    /// `override` field is a reserved word in Dart and breaks the POS client
    /// code generator.
    pub mode: String,
}

#[utoipa::path(
    post, path = "/delivery/accepting", tag = "delivery",
    request_body = AcceptingInput,
    responses((status = 200, body = BranchDeliverySettings), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn set_accepting(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<AcceptingInput>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    // POS-owned: the queue permission, NOT delivery_settings.
    check_permission(pool.get_ref(), &claims, "delivery_orders", "update").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    validate_channel(&body.channel)?;
    validate_override(&body.mode)?;

    sqlx::query(
        "INSERT INTO branch_delivery_settings (branch_id) VALUES ($1) ON CONFLICT (branch_id) DO NOTHING",
    )
    .bind(body.branch_id)
    .execute(pool.get_ref())
    .await?;

    // The POS cannot re-open a channel the dashboard disabled.
    let enabled: bool = sqlx::query_scalar(&format!(
        "SELECT {}_enabled FROM branch_delivery_settings WHERE branch_id = $1",
        body.channel
    ))
    .bind(body.branch_id)
    .fetch_one(pool.get_ref())
    .await?;
    if !enabled && body.mode != "closed" {
        return Err(AppError::Conflict(
            "This channel is disabled by management and cannot be opened from the POS.".into(),
        ));
    }

    sqlx::query(&format!(
        "UPDATE branch_delivery_settings SET {}_override = $2, updated_at = now() WHERE branch_id = $1",
        body.channel
    ))
    .bind(body.branch_id)
    .bind(&body.mode)
    .execute(pool.get_ref())
    .await?;

    let row: BranchDeliverySettings = sqlx::query_as(BRANCH_SETTINGS_SELECT)
        .bind(body.branch_id)
        .fetch_one(pool.get_ref())
        .await?;
    Ok(HttpResponse::Ok().json(row))
}

// ── Zones (flat per-ring fee) ─────────────────────────────────

#[derive(Serialize, Deserialize, ToSchema, sqlx::FromRow)]
pub struct DeliveryZone {
    pub id: Uuid,
    pub branch_id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub max_road_distance_meters: i32,
    pub fee: i32,
    pub is_active: bool,
}

const ZONE_SELECT: &str = "SELECT id, branch_id, name, name_translations, \
    max_road_distance_meters, fee, is_active FROM delivery_zones";

#[utoipa::path(
    get, path = "/delivery/zones", tag = "delivery", params(BranchQuery),
    responses((status = 200, body = [DeliveryZone]), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_zones(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<BranchQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "delivery_settings", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    let zones: Vec<DeliveryZone> = sqlx::query_as(&format!(
        "{ZONE_SELECT} WHERE branch_id = $1 ORDER BY max_road_distance_meters ASC"
    ))
    .bind(query.branch_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(zones))
}

#[derive(Deserialize, ToSchema)]
pub struct ZoneInput {
    pub branch_id: Uuid,
    pub name: String,
    #[serde(default)]
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub max_road_distance_meters: i32,
    pub fee: i32,
    #[serde(default = "default_true")]
    pub is_active: bool,
}

fn default_true() -> bool {
    true
}

fn validate_zone(input: &ZoneInput) -> Result<(), AppError> {
    if input.max_road_distance_meters <= 0 {
        return Err(AppError::BadRequest("max_road_distance_meters must be > 0".into()));
    }
    if input.fee < 0 {
        return Err(AppError::BadRequest("fee must be >= 0".into()));
    }
    Ok(())
}

#[utoipa::path(
    post, path = "/delivery/zones", tag = "delivery", request_body = ZoneInput,
    responses((status = 201, body = DeliveryZone), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_zone(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<ZoneInput>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "delivery_settings", "create").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    validate_zone(&body)?;

    let zone: DeliveryZone = sqlx::query_as(
        "WITH ins AS (
            INSERT INTO delivery_zones
                (branch_id, name, name_translations, max_road_distance_meters, fee, is_active)
            VALUES ($1, $2, $3, $4, $5, $6)
            RETURNING id, branch_id, name, name_translations,
                      max_road_distance_meters, fee, is_active
         ) SELECT * FROM ins",
    )
    .bind(body.branch_id)
    .bind(&body.name)
    .bind(&body.name_translations)
    .bind(body.max_road_distance_meters)
    .bind(body.fee)
    .bind(body.is_active)
    .fetch_one(pool.get_ref())
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
            AppError::Conflict("A zone with this distance already exists for the branch".into())
        }
        other => other.into(),
    })?;
    Ok(HttpResponse::Created().json(zone))
}

#[utoipa::path(
    patch, path = "/delivery/zones/{id}", tag = "delivery", request_body = ZoneInput,
    responses((status = 200, body = DeliveryZone), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_zone(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>,
    body: web::Json<ZoneInput>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "delivery_settings", "update").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    validate_zone(&body)?;
    let id = path.into_inner();

    let zone: Option<DeliveryZone> = sqlx::query_as(
        "WITH upd AS (
            UPDATE delivery_zones SET
                name = $3, name_translations = $4,
                max_road_distance_meters = $5, fee = $6, is_active = $7, updated_at = now()
            WHERE id = $1 AND branch_id = $2
            RETURNING id, branch_id, name, name_translations,
                      max_road_distance_meters, fee, is_active
         ) SELECT * FROM upd",
    )
    .bind(id)
    .bind(body.branch_id)
    .bind(&body.name)
    .bind(&body.name_translations)
    .bind(body.max_road_distance_meters)
    .bind(body.fee)
    .bind(body.is_active)
    .fetch_optional(pool.get_ref())
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
            AppError::Conflict("A zone with this distance already exists for the branch".into())
        }
        other => other.into(),
    })?;

    zone.map(|z| HttpResponse::Ok().json(z))
        .ok_or_else(|| AppError::NotFound("Zone not found".into()))
}

#[utoipa::path(
    delete, path = "/delivery/zones/{id}", tag = "delivery", params(BranchQuery),
    responses((status = 204, description = "Deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_zone(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>,
    query: web::Query<BranchQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "delivery_settings", "delete").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    let done = sqlx::query("DELETE FROM delivery_zones WHERE id = $1 AND branch_id = $2")
        .bind(path.into_inner())
        .bind(query.branch_id)
        .execute(pool.get_ref())
        .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::NotFound("Zone not found".into()));
    }
    Ok(HttpResponse::NoContent().finish())
}

// ── Channel menu overrides (admin) ────────────────────────────

#[derive(Serialize, Deserialize, ToSchema, sqlx::FromRow)]
pub struct ChannelMenuOverride {
    pub branch_id: Uuid,
    pub menu_item_id: Uuid,
    pub channel: String,
    pub price_override: Option<i32>,
    pub is_available: Option<bool>,
}

#[derive(Deserialize, IntoParams)]
pub struct ChannelOverrideQuery {
    pub branch_id: Uuid,
    pub channel: String,
}

#[utoipa::path(
    get, path = "/delivery/channel-overrides", tag = "delivery", params(ChannelOverrideQuery),
    responses((status = 200, body = [ChannelMenuOverride]), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_channel_overrides(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<ChannelOverrideQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;
    validate_channel(&query.channel)?;

    let rows: Vec<ChannelMenuOverride> = sqlx::query_as(
        "SELECT branch_id, menu_item_id, channel::text, price_override, is_available \
         FROM branch_channel_menu_overrides WHERE branch_id = $1 AND channel = $2::delivery_channel \
         ORDER BY menu_item_id",
    )
    .bind(query.branch_id)
    .bind(&query.channel)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[derive(Deserialize, ToSchema)]
pub struct ChannelOverrideInput {
    pub branch_id: Uuid,
    pub menu_item_id: Uuid,
    pub channel: String,
    pub price_override: Option<i32>,
    pub is_available: Option<bool>,
}

#[utoipa::path(
    put, path = "/delivery/channel-overrides", tag = "delivery", request_body = ChannelOverrideInput,
    responses((status = 200, body = ChannelMenuOverride), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn upsert_channel_override(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<ChannelOverrideInput>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    validate_channel(&body.channel)?;
    if body.price_override.is_some_and(|p| p < 0) {
        return Err(AppError::BadRequest("price_override must be >= 0".into()));
    }

    let row: ChannelMenuOverride = sqlx::query_as(
        "INSERT INTO branch_channel_menu_overrides
            (branch_id, menu_item_id, channel, price_override, is_available, updated_at)
         VALUES ($1, $2, $3::delivery_channel, $4, $5, now())
         ON CONFLICT (branch_id, menu_item_id, channel) DO UPDATE SET
             price_override = EXCLUDED.price_override,
             is_available = EXCLUDED.is_available,
             updated_at = now()
         RETURNING branch_id, menu_item_id, channel::text, price_override, is_available",
    )
    .bind(body.branch_id)
    .bind(body.menu_item_id)
    .bind(&body.channel)
    .bind(body.price_override)
    .bind(body.is_available)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(row))
}

#[derive(Deserialize, IntoParams)]
pub struct ChannelOverrideDeleteQuery {
    pub branch_id: Uuid,
    pub menu_item_id: Uuid,
    pub channel: String,
}

#[utoipa::path(
    delete, path = "/delivery/channel-overrides", tag = "delivery", params(ChannelOverrideDeleteQuery),
    responses((status = 204, description = "Deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_channel_override(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<ChannelOverrideDeleteQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;
    validate_channel(&query.channel)?;

    sqlx::query(
        "DELETE FROM branch_channel_menu_overrides \
         WHERE branch_id = $1 AND menu_item_id = $2 AND channel = $3::delivery_channel",
    )
    .bind(query.branch_id)
    .bind(query.menu_item_id)
    .bind(&query.channel)
    .execute(pool.get_ref())
    .await?;
    Ok(HttpResponse::NoContent().finish())
}

// ── Channel addon overrides (admin) ───────────────────────────

#[derive(Serialize, Deserialize, ToSchema, sqlx::FromRow)]
pub struct ChannelAddonOverride {
    pub branch_id: Uuid,
    pub addon_item_id: Uuid,
    pub channel: String,
    pub price_override: Option<i32>,
    pub is_available: Option<bool>,
}

#[utoipa::path(
    get, path = "/delivery/channel-addon-overrides", tag = "delivery", params(ChannelOverrideQuery),
    responses((status = 200, body = [ChannelAddonOverride]), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_channel_addon_overrides(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<ChannelOverrideQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "addon_items", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;
    validate_channel(&query.channel)?;

    let rows: Vec<ChannelAddonOverride> = sqlx::query_as(
        "SELECT branch_id, addon_item_id, channel::text, price_override, is_available \
         FROM branch_channel_addon_overrides WHERE branch_id = $1 AND channel = $2::delivery_channel \
         ORDER BY addon_item_id",
    )
    .bind(query.branch_id)
    .bind(&query.channel)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[derive(Deserialize, ToSchema)]
pub struct ChannelAddonOverrideInput {
    pub branch_id: Uuid,
    pub addon_item_id: Uuid,
    pub channel: String,
    pub price_override: Option<i32>,
    pub is_available: Option<bool>,
}

#[utoipa::path(
    put, path = "/delivery/channel-addon-overrides", tag = "delivery", request_body = ChannelAddonOverrideInput,
    responses((status = 200, body = ChannelAddonOverride), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn upsert_channel_addon_override(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<ChannelAddonOverrideInput>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "addon_items", "update").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    validate_channel(&body.channel)?;
    if body.price_override.is_some_and(|p| p < 0) {
        return Err(AppError::BadRequest("price_override must be >= 0".into()));
    }

    let row: ChannelAddonOverride = sqlx::query_as(
        "INSERT INTO branch_channel_addon_overrides
            (branch_id, addon_item_id, channel, price_override, is_available, updated_at)
         VALUES ($1, $2, $3::delivery_channel, $4, $5, now())
         ON CONFLICT (branch_id, addon_item_id, channel) DO UPDATE SET
             price_override = EXCLUDED.price_override,
             is_available = EXCLUDED.is_available,
             updated_at = now()
         RETURNING branch_id, addon_item_id, channel::text, price_override, is_available",
    )
    .bind(body.branch_id)
    .bind(body.addon_item_id)
    .bind(&body.channel)
    .bind(body.price_override)
    .bind(body.is_available)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(row))
}

#[derive(Deserialize, IntoParams)]
pub struct ChannelAddonOverrideDeleteQuery {
    pub branch_id: Uuid,
    pub addon_item_id: Uuid,
    pub channel: String,
}

#[utoipa::path(
    delete, path = "/delivery/channel-addon-overrides", tag = "delivery", params(ChannelAddonOverrideDeleteQuery),
    responses((status = 204, description = "Deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_channel_addon_override(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<ChannelAddonOverrideDeleteQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "addon_items", "update").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;
    validate_channel(&query.channel)?;

    sqlx::query(
        "DELETE FROM branch_channel_addon_overrides \
         WHERE branch_id = $1 AND addon_item_id = $2 AND channel = $3::delivery_channel",
    )
    .bind(query.branch_id)
    .bind(query.addon_item_id)
    .bind(&query.channel)
    .execute(pool.get_ref())
    .await?;
    Ok(HttpResponse::NoContent().finish())
}
