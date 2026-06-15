//! Public (unauthenticated, rate-limited) delivery surface: branch selector,
//! channel-resolved menu, OSRM-proxied delivery quote, WhatsApp OTP, and order
//! intake. Pricing is 100% server-side and frozen into the delivery_orders row.

use actix_web::{web, HttpRequest, HttpResponse};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::hub::{DeliveryEvent, DeliveryHub};
use super::snapshot::{self, CartLineInput};
use super::staff::DeliveryOrder;
use super::whatsapp;
use super::{channel_open, normalize_phone, validate_channel, CHANNEL_OUTSIDE};
use crate::auth::jwt::JwtSecret;
use crate::errors::{AppError, AppErrorResponse};
use crate::geo::osrm::{haversine_meters, road_distance_meters, LatLng, OsrmError};

// ── Public branch selector ────────────────────────────────────

#[derive(Serialize, ToSchema)]
pub struct PublicBranch {
    pub id: Uuid,
    pub name: String,
    pub code: String,
    pub in_mall_enabled: bool,
    pub outside_enabled: bool,
    /// Effective-open right now (enabled + open shift + override + window).
    pub in_mall_open_now: bool,
    pub outside_open_now: bool,
}

#[derive(Deserialize, IntoParams)]
pub struct PublicBranchesQuery {
    pub org_id: Uuid,
}

#[derive(sqlx::FromRow)]
struct BranchOpenRow {
    id: Uuid,
    name: String,
    code: String,
    in_mall_enabled: bool,
    outside_enabled: bool,
    in_mall_override: String,
    outside_override: String,
    in_mall_open_time: Option<chrono::NaiveTime>,
    in_mall_close_time: Option<chrono::NaiveTime>,
    outside_open_time: Option<chrono::NaiveTime>,
    outside_close_time: Option<chrono::NaiveTime>,
    local_time: chrono::NaiveTime,
    has_open_shift: bool,
}

#[utoipa::path(
    get, path = "/public/branches", tag = "delivery-public", params(PublicBranchesQuery),
    responses((status = 200, body = [PublicBranch]), AppErrorResponse)
)]
pub async fn public_branches(
    pool: web::Data<PgPool>,
    query: web::Query<PublicBranchesQuery>,
) -> Result<HttpResponse, AppError> {
    let rows: Vec<BranchOpenRow> = sqlx::query_as(
        r#"SELECT b.id, b.name, b.code,
                  COALESCE(s.in_mall_enabled, false)  AS in_mall_enabled,
                  COALESCE(s.outside_enabled, false)  AS outside_enabled,
                  COALESCE(s.in_mall_override, 'auto') AS in_mall_override,
                  COALESCE(s.outside_override, 'auto') AS outside_override,
                  s.in_mall_open_time, s.in_mall_close_time,
                  s.outside_open_time, s.outside_close_time,
                  (now() AT TIME ZONE b.timezone)::time AS local_time,
                  EXISTS(SELECT 1 FROM shifts sh WHERE sh.branch_id = b.id AND sh.status = 'open') AS has_open_shift
           FROM branches b
           LEFT JOIN branch_delivery_settings s ON s.branch_id = b.id
           WHERE b.org_id = $1 AND b.is_active = true AND b.deleted_at IS NULL
             AND (COALESCE(s.in_mall_enabled, false) OR COALESCE(s.outside_enabled, false))
           ORDER BY b.name"#,
    )
    .bind(query.org_id)
    .fetch_all(pool.get_ref())
    .await?;

    let branches: Vec<PublicBranch> = rows
        .into_iter()
        .map(|r| PublicBranch {
            in_mall_open_now: channel_open(
                r.in_mall_enabled, &r.in_mall_override, r.in_mall_open_time,
                r.in_mall_close_time, r.local_time, r.has_open_shift,
            ),
            outside_open_now: channel_open(
                r.outside_enabled, &r.outside_override, r.outside_open_time,
                r.outside_close_time, r.local_time, r.has_open_shift,
            ),
            id: r.id,
            name: r.name,
            code: r.code,
            in_mall_enabled: r.in_mall_enabled,
            outside_enabled: r.outside_enabled,
        })
        .collect();
    Ok(HttpResponse::Ok().json(branches))
}

// ── Public channel-resolved menu ──────────────────────────────

#[derive(Serialize, ToSchema)]
pub struct DeliveryMenuSize {
    pub label: String,
    pub price: i32,
}

/// One option in the org-wide addon catalog. The catalog is global (the POS
/// model): every item can use any addon; swap-vs-additive is decided server-side
/// from the addon `type` + the item recipe at order time. `price` is the
/// channel-effective surcharge in piastres (branch_channel → branch → catalog
/// default). Channel-unavailable options are excluded from the catalog entirely.
#[derive(Serialize, ToSchema, Clone)]
pub struct DeliveryAddonOption {
    pub addon_item_id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    /// `milk_type` | `coffee_type` | `extra` — the option's category.
    pub r#type: String,
    /// Channel-effective surcharge (piastres). Always present (resolved here).
    pub price: i32,
    pub is_available: bool,
}

/// A per-item optional toggle (e.g. "Extra hot", "No sugar"). `price` is the
/// piastres surcharge; `size_label` is set when the optional only applies to a
/// specific size.
#[derive(Serialize, ToSchema)]
pub struct DeliveryOptionalField {
    pub id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub price: i32,
    pub size_label: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct DeliveryMenuItem {
    pub id: Uuid,
    pub category_id: Option<Uuid>,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub description: Option<String>,
    pub image_url: Option<String>,
    pub price: i32,
    pub sizes: Vec<DeliveryMenuSize>,
    pub optionals: Vec<DeliveryOptionalField>,
    /// The item's base/default milk: the `milk_type` addon whose ingredient
    /// matches the item recipe's milk ingredient. The online customizer
    /// pre-selects it (mirrors the POS default-milk selection). `None` when the
    /// item has no milk in its recipe or no matching milk addon exists.
    pub default_milk_addon_id: Option<Uuid>,
}

#[derive(Serialize, ToSchema)]
pub struct DeliveryMenuCategory {
    pub id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub image_url: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct DeliveryMenu {
    pub categories: Vec<DeliveryMenuCategory>,
    pub items: Vec<DeliveryMenuItem>,
    /// Org-wide addon catalog (global, POS model): channel-effective, grouped by
    /// `type`, applicable to every item. Channel-unavailable options are excluded.
    pub addons: Vec<DeliveryAddonOption>,
}

#[derive(Deserialize, IntoParams)]
pub struct ChannelParam {
    pub channel: String,
}

#[utoipa::path(
    get, path = "/public/branches/{id}/menu", tag = "delivery-public", params(ChannelParam),
    responses((status = 200, body = DeliveryMenu), AppErrorResponse)
)]
pub async fn public_menu(
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>,
    query: web::Query<ChannelParam>,
) -> Result<HttpResponse, AppError> {
    let branch_id = path.into_inner();
    validate_channel(&query.channel)?;

    let branch: Option<(Uuid, bool)> = sqlx::query_as(&format!(
        "SELECT b.org_id, COALESCE(s.{}_enabled, false) \
         FROM branches b LEFT JOIN branch_delivery_settings s ON s.branch_id = b.id \
         WHERE b.id = $1 AND b.is_active = true AND b.deleted_at IS NULL",
        query.channel
    ))
    .bind(branch_id)
    .fetch_optional(pool.get_ref())
    .await?;
    let (org_id, enabled) = branch.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    if !enabled {
        return Err(AppError::NotFound("This branch does not offer this channel".into()));
    }

    let categories: Vec<DeliveryMenuCategory> = sqlx::query_as::<_, (Uuid, String, serde_json::Value, Option<String>)>(
        "SELECT id, name, name_translations, image_url FROM categories \
         WHERE org_id = $1 AND is_active = true AND deleted_at IS NULL ORDER BY name",
    )
    .bind(org_id)
    .fetch_all(pool.get_ref())
    .await?
    .into_iter()
    .map(|(id, name, name_translations, image_url)| DeliveryMenuCategory { id, name, name_translations, image_url })
    .collect();

    #[allow(clippy::type_complexity)]
    let item_rows: Vec<(Uuid, Option<Uuid>, String, serde_json::Value, Option<String>, Option<String>, i32)> =
        sqlx::query_as(
            r#"SELECT mi.id, mi.category_id, mi.name, mi.name_translations, mi.description, mi.image_url,
                      COALESCE(bcmo.price_override, bmo.price_override, mi.base_price) AS price
               FROM menu_items mi
               LEFT JOIN branch_menu_overrides bmo
                      ON bmo.menu_item_id = mi.id AND bmo.branch_id = $1
               LEFT JOIN branch_channel_menu_overrides bcmo
                      ON bcmo.menu_item_id = mi.id AND bcmo.branch_id = $1
                     AND bcmo.channel = $2::delivery_channel
               WHERE mi.org_id = $3 AND mi.is_active = true AND mi.deleted_at IS NULL
                 AND COALESCE(bcmo.is_available, bmo.is_available, true) = true
               ORDER BY mi.name"#,
        )
        .bind(branch_id)
        .bind(&query.channel)
        .bind(org_id)
        .fetch_all(pool.get_ref())
        .await?;

    let item_ids: Vec<Uuid> = item_rows.iter().map(|r| r.0).collect();

    // Catalog size prices + branch size overrides (branch override wins).
    let catalog_sizes: Vec<(Uuid, String, i32)> = sqlx::query_as(
        "SELECT menu_item_id, label::text, price_override FROM item_sizes \
         WHERE is_active = true AND menu_item_id = ANY($1) ORDER BY label",
    )
    .bind(&item_ids)
    .fetch_all(pool.get_ref())
    .await?;
    let branch_sizes: Vec<(Uuid, String, i32)> = sqlx::query_as(
        "SELECT menu_item_id, size_label::text, price_override FROM branch_menu_size_overrides \
         WHERE branch_id = $1 AND menu_item_id = ANY($2)",
    )
    .bind(branch_id)
    .bind(&item_ids)
    .fetch_all(pool.get_ref())
    .await?;
    let branch_size_map: std::collections::HashMap<(Uuid, String), i32> = branch_sizes
        .into_iter()
        .map(|(id, label, price)| ((id, label), price))
        .collect();

    let mut sizes_by_item: std::collections::HashMap<Uuid, Vec<DeliveryMenuSize>> = std::collections::HashMap::new();
    for (item_id, label, catalog_price) in catalog_sizes {
        let price = branch_size_map
            .get(&(item_id, label.clone()))
            .copied()
            .unwrap_or(catalog_price);
        sizes_by_item.entry(item_id).or_default().push(DeliveryMenuSize { label, price });
    }

    // Global org-wide addon catalog (channel-effective), loaded once per request.
    let addons = load_addon_catalog(pool.get_ref(), org_id, branch_id, &query.channel).await?;

    // Optional fields per item.
    let mut optionals_by_item = load_optional_fields(pool.get_ref(), &item_ids).await?;

    // Default/base milk per item (POS pre-select), batched over the item list.
    let mut default_milk_by_item = load_default_milk(pool.get_ref(), &item_ids).await?;

    let items: Vec<DeliveryMenuItem> = item_rows
        .into_iter()
        .map(|(id, category_id, name, name_translations, description, image_url, price)| DeliveryMenuItem {
            sizes: sizes_by_item.remove(&id).unwrap_or_default(),
            optionals: optionals_by_item.remove(&id).unwrap_or_default(),
            default_milk_addon_id: default_milk_by_item.remove(&id),
            id,
            category_id,
            name,
            name_translations,
            description,
            image_url,
            price,
        })
        .collect();

    Ok(HttpResponse::Ok().json(DeliveryMenu { categories, items, addons }))
}

/// Load the org-wide global addon catalog (the POS model: one catalog for every
/// item), priced/availability-resolved per channel (branch_channel → branch →
/// catalog default). Channel-unavailable options are excluded. Ordered by `type`
/// then `name`. Loaded once per request, not per item.
async fn load_addon_catalog(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    channel: &str,
) -> Result<Vec<DeliveryAddonOption>, AppError> {
    // Resolve the override chain in SQL and drop options whose channel
    // availability is false.
    let rows: Vec<(Uuid, String, serde_json::Value, String, i32, bool)> = sqlx::query_as(
        "SELECT a.id, a.name, a.name_translations, a.type, \
                COALESCE(bcao.price_override, bao.price_override, a.default_price) AS price, \
                COALESCE(bcao.is_available, bao.is_available, true)               AS is_available \
         FROM addon_items a \
         LEFT JOIN branch_addon_overrides bao \
                ON bao.addon_item_id = a.id AND bao.branch_id = $1 \
         LEFT JOIN branch_channel_addon_overrides bcao \
                ON bcao.addon_item_id = a.id AND bcao.branch_id = $1 \
               AND bcao.channel = $2::delivery_channel \
         WHERE a.org_id = $3 AND a.is_active = true \
         ORDER BY a.type, a.name",
    )
    .bind(branch_id)
    .bind(channel)
    .bind(org_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter(|(_, _, _, _, _, is_available)| *is_available) // channel-disabled — never offered
        .map(|(addon_item_id, name, name_translations, atype, price, is_available)| DeliveryAddonOption {
            addon_item_id,
            name,
            name_translations,
            r#type: atype,
            price,
            is_available,
        })
        .collect())
}

/// Load active optional fields per item id, keyed by menu_item_id.
async fn load_optional_fields(
    pool: &PgPool,
    item_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, Vec<DeliveryOptionalField>>, AppError> {
    let mut by_item: std::collections::HashMap<Uuid, Vec<DeliveryOptionalField>> =
        std::collections::HashMap::new();
    if item_ids.is_empty() {
        return Ok(by_item);
    }

    let rows: Vec<(Uuid, Uuid, String, serde_json::Value, i32, Option<String>)> = sqlx::query_as(
        "SELECT id, menu_item_id, name, name_translations, price, size_label::text \
         FROM menu_item_optional_fields \
         WHERE menu_item_id = ANY($1) AND is_active = true \
         ORDER BY name",
    )
    .bind(item_ids)
    .fetch_all(pool)
    .await?;

    for (id, menu_item_id, name, name_translations, price, size_label) in rows {
        by_item.entry(menu_item_id).or_default().push(DeliveryOptionalField {
            id,
            name,
            name_translations,
            price,
            size_label,
        });
    }

    Ok(by_item)
}

/// Resolve each item's base/default milk addon, batched over the item list (no
/// N+1). Mirrors the canonical swap-base definition (orders::component_resolve):
/// the item recipe's milk ingredient is the `drink_recipe` deduction whose
/// `org_ingredients.category = 'milk'`; the base/default milk addon is the
/// `milk_type` addon whose `addon_item_ingredients.org_ingredient_id` equals
/// that ingredient. Items with no recipe milk (or no matching milk addon) are
/// simply absent from the map → `None`. Uses the one_size / first recipe row,
/// matching how `default_milk_addon_id` is computed in the authenticated menu.
async fn load_default_milk(
    pool: &PgPool,
    item_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, Uuid>, AppError> {
    let mut by_item: std::collections::HashMap<Uuid, Uuid> = std::collections::HashMap::new();
    if item_ids.is_empty() {
        return Ok(by_item);
    }

    // For each item: its recipe milk ingredient (category='milk') → the milk_type
    // addon whose ingredient matches. DISTINCT ON keeps one addon per item.
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT DISTINCT ON (r.menu_item_id) r.menu_item_id, a.id \
         FROM menu_item_recipes r \
         JOIN org_ingredients i ON i.id = r.org_ingredient_id AND i.category = 'milk' \
         JOIN addon_item_ingredients ai ON ai.org_ingredient_id = r.org_ingredient_id \
         JOIN addon_items a ON a.id = ai.addon_item_id AND a.type = 'milk_type' \
         WHERE r.menu_item_id = ANY($1) \
         ORDER BY r.menu_item_id, a.id",
    )
    .bind(item_ids)
    .fetch_all(pool)
    .await?;

    for (menu_item_id, addon_id) in rows {
        by_item.entry(menu_item_id).or_insert(addon_id);
    }

    Ok(by_item)
}

// ── Delivery quote (OSRM-proxied) ─────────────────────────────

#[derive(Serialize, ToSchema)]
pub struct QuoteResponse {
    /// "ok" | "out_of_range" | "unavailable"
    pub status: String,
    pub zone_id: Option<Uuid>,
    pub zone_name: Option<String>,
    pub distance_meters: Option<i32>,
    pub fee: Option<i32>,
}

#[derive(Deserialize, IntoParams)]
pub struct QuoteQuery {
    pub lat: f64,
    pub lng: f64,
    pub channel: String,
}

pub(crate) enum FeeOutcome {
    Ok { fee: i32, zone_id: Uuid, zone_name: String, distance_meters: i32 },
    OutOfRange,
    Unavailable,
}

#[derive(sqlx::FromRow, Clone)]
pub(crate) struct ZoneRow {
    pub id: Uuid,
    pub name: String,
    pub fee: i32,
    pub max_road_distance_meters: i32,
}

/// Pure zone match (no OSRM, no DB) so it can be unit-tested. `zones` must be
/// ordered by ring_order ASC — the smallest covering ring's flat fee wins.
pub(crate) fn select_zone_fee(distance_i: i32, max_dist: Option<i32>, zones: &[ZoneRow]) -> FeeOutcome {
    if max_dist.is_some_and(|m| distance_i > m) {
        return FeeOutcome::OutOfRange;
    }
    match zones.iter().find(|z| z.max_road_distance_meters >= distance_i) {
        Some(zone) => FeeOutcome::Ok {
            fee: zone.fee,
            zone_id: zone.id,
            zone_name: zone.name.clone(),
            distance_meters: distance_i,
        },
        None => FeeOutcome::OutOfRange,
    }
}

/// Server-authoritative outside fee: OSRM road distance → smallest matching ring →
/// fee. The order endpoint recomputes this; the client value is never trusted.
async fn compute_outside_fee(
    pool: &PgPool,
    branch_id: Uuid,
    cust: LatLng,
) -> Result<FeeOutcome, AppError> {
    let branch: Option<(Option<f64>, Option<f64>, Option<i32>)> = sqlx::query_as(
        "SELECT b.latitude, b.longitude, s.max_road_distance_meters \
         FROM branches b LEFT JOIN branch_delivery_settings s ON s.branch_id = b.id WHERE b.id = $1",
    )
    .bind(branch_id)
    .fetch_optional(pool)
    .await?;
    let Some((Some(blat), Some(blng), max_dist)) = branch else {
        return Ok(FeeOutcome::Unavailable);
    };

    // OSRM gives true road distance. If it's unset / unreachable / garbled, fall
    // back to straight-line (haversine) so a quote can still be produced from the
    // zone rings. A genuine NoRoute (OSRM reachable but no driving path) stays
    // out-of-range; a branch with no coordinates stays unavailable.
    let branch_pt = LatLng { lat: blat, lng: blng };
    let distance = match road_distance_meters(branch_pt, cust).await {
        Ok(d) => d,
        Err(OsrmError::NotConfigured) | Err(OsrmError::Unreachable) | Err(OsrmError::BadResponse) => {
            haversine_meters(branch_pt, cust)
        }
        Err(OsrmError::NoRoute) => return Ok(FeeOutcome::OutOfRange),
    };
    let distance_i = distance.round() as i32;

    let zones: Vec<ZoneRow> = sqlx::query_as(
        "SELECT id, name, fee, max_road_distance_meters \
         FROM delivery_zones WHERE branch_id = $1 AND is_active = true ORDER BY max_road_distance_meters ASC",
    )
    .bind(branch_id)
    .fetch_all(pool)
    .await?;

    Ok(select_zone_fee(distance_i, max_dist, &zones))
}

#[utoipa::path(
    get, path = "/public/branches/{id}/delivery-quote", tag = "delivery-public", params(QuoteQuery),
    responses((status = 200, body = QuoteResponse), AppErrorResponse)
)]
pub async fn delivery_quote(
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>,
    query: web::Query<QuoteQuery>,
) -> Result<HttpResponse, AppError> {
    let branch_id = path.into_inner();
    validate_channel(&query.channel)?;
    let outside = query.channel == CHANNEL_OUTSIDE;

    let row: Option<(bool, i32)> = sqlx::query_as(&format!(
        "SELECT COALESCE(s.{ch}_enabled, false), COALESCE(s.in_mall_fee, 0) \
         FROM branches b LEFT JOIN branch_delivery_settings s ON s.branch_id = b.id \
         WHERE b.id = $1 AND b.is_active = true AND b.deleted_at IS NULL",
        ch = if outside { "outside" } else { "in_mall" }
    ))
    .bind(branch_id)
    .fetch_optional(pool.get_ref())
    .await?;
    let (enabled, in_mall_fee) = row.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    if !enabled {
        return Err(AppError::NotFound("This branch does not offer this channel".into()));
    }

    // In-mall: flat per-branch fee, no distance.
    if !outside {
        return Ok(HttpResponse::Ok().json(QuoteResponse {
            status: "ok".into(),
            zone_id: None,
            zone_name: None,
            distance_meters: Some(0),
            fee: Some(in_mall_fee),
        }));
    }

    let outcome = compute_outside_fee(pool.get_ref(), branch_id, LatLng { lat: query.lat, lng: query.lng }).await?;
    let resp = match outcome {
        FeeOutcome::Ok { fee, zone_id, zone_name, distance_meters } => QuoteResponse {
            status: "ok".into(),
            zone_id: Some(zone_id),
            zone_name: Some(zone_name),
            distance_meters: Some(distance_meters),
            fee: Some(fee),
        },
        FeeOutcome::OutOfRange => QuoteResponse {
            status: "out_of_range".into(),
            zone_id: None, zone_name: None, distance_meters: None, fee: None,
        },
        FeeOutcome::Unavailable => QuoteResponse {
            status: "unavailable".into(),
            zone_id: None, zone_name: None, distance_meters: None, fee: None,
        },
    };
    Ok(HttpResponse::Ok().json(resp))
}

// ── WhatsApp OTP ──────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct OtpRequestInput {
    pub phone: String,
}

#[derive(Serialize, ToSchema)]
pub struct OtpRequestResponse {
    pub sent: bool,
    /// Only populated when SUFRIX_OTP_DEBUG=1 (dev/test). Never set in prod.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug_code: Option<String>,
}

const OTP_TTL_SECONDS: i64 = 300;
const OTP_MAX_ATTEMPTS: i32 = 5;

fn generate_otp_code() -> String {
    // uuid v4 is CSPRNG-backed; derive 4 digits from its first bytes.
    let bytes = *Uuid::new_v4().as_bytes();
    let n = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) % 10000;
    format!("{n:04}")
}

#[utoipa::path(
    post, path = "/public/otp/request", tag = "delivery-public", request_body = OtpRequestInput,
    responses((status = 200, body = OtpRequestResponse), AppErrorResponse)
)]
pub async fn otp_request(
    pool: web::Data<PgPool>,
    body: web::Json<OtpRequestInput>,
) -> Result<HttpResponse, AppError> {
    let phone = normalize_phone(&body.phone)?;

    // Per-phone cooldown: one live code per ~60s.
    let recent: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM delivery_otp \
         WHERE phone = $1 AND consumed_at IS NULL AND created_at > now() - interval '60 seconds')",
    )
    .bind(&phone)
    .fetch_one(pool.get_ref())
    .await?;
    if recent {
        return Err(AppError::Conflict("A code was just sent. Please wait a minute.".into()));
    }

    let code = generate_otp_code();
    let code_hash = bcrypt::hash(&code, bcrypt::DEFAULT_COST).map_err(|_| AppError::Internal)?;

    sqlx::query(
        "INSERT INTO delivery_otp (phone, code_hash, expires_at) \
         VALUES ($1, $2, now() + ($3 || ' seconds')::interval)",
    )
    .bind(&phone)
    .bind(&code_hash)
    .bind(OTP_TTL_SECONDS.to_string())
    .execute(pool.get_ref())
    .await?;

    whatsapp::send_message(phone, whatsapp::build_otp_message(&code));

    let debug_code = std::env::var("SUFRIX_OTP_DEBUG")
        .ok()
        .filter(|v| v == "1")
        .map(|_| code);
    Ok(HttpResponse::Ok().json(OtpRequestResponse { sent: true, debug_code }))
}

#[derive(Deserialize, ToSchema)]
pub struct OtpVerifyInput {
    pub phone: String,
    pub code: String,
}

#[derive(Serialize, ToSchema)]
pub struct OtpVerifyResponse {
    pub device_token: String,
}

#[utoipa::path(
    post, path = "/public/otp/verify", tag = "delivery-public", request_body = OtpVerifyInput,
    responses((status = 200, body = OtpVerifyResponse), AppErrorResponse)
)]
pub async fn otp_verify(
    pool: web::Data<PgPool>,
    secret: web::Data<JwtSecret>,
    body: web::Json<OtpVerifyInput>,
) -> Result<HttpResponse, AppError> {
    let phone = normalize_phone(&body.phone)?;

    let row: Option<(Uuid, String, i32)> = sqlx::query_as(
        "SELECT id, code_hash, attempts FROM delivery_otp \
         WHERE phone = $1 AND consumed_at IS NULL AND expires_at > now() \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&phone)
    .fetch_optional(pool.get_ref())
    .await?;

    let (id, code_hash, attempts) =
        row.ok_or_else(|| AppError::BadRequest("No active code — request a new one.".into()))?;
    if attempts >= OTP_MAX_ATTEMPTS {
        return Err(AppError::BadRequest("Too many attempts — request a new code.".into()));
    }

    let ok = bcrypt::verify(&body.code, &code_hash).unwrap_or(false);
    if !ok {
        sqlx::query("UPDATE delivery_otp SET attempts = attempts + 1 WHERE id = $1")
            .bind(id)
            .execute(pool.get_ref())
            .await?;
        return Err(AppError::BadRequest("Incorrect code.".into()));
    }

    sqlx::query("UPDATE delivery_otp SET consumed_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool.get_ref())
        .await?;

    let token = whatsapp::issue_device_token(&secret.0, &phone)?;
    Ok(HttpResponse::Ok().json(OtpVerifyResponse { device_token: token }))
}

// ── Order intake ──────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct DeliveryOrderInput {
    pub branch_id: Uuid,
    pub channel: String,
    pub customer_name: String,
    pub customer_phone: String,
    #[serde(default)]
    pub place_name: Option<String>,
    #[serde(default)]
    pub floor: Option<String>,
    #[serde(default)]
    pub unit_number: Option<String>,
    #[serde(default)]
    pub landmark: Option<String>,
    #[serde(default)]
    pub address_line: Option<String>,
    #[serde(default)]
    pub delivery_notes: Option<String>,
    #[serde(default)]
    pub customer_lat: Option<f64>,
    #[serde(default)]
    pub customer_lng: Option<f64>,
    /// "cash" | "card" — a hint the teller can change at finalize.
    pub payment_method_hint: String,
    /// Device-trust token from OTP verify (proves the phone).
    pub device_token: String,
    pub items: Vec<CartLineInput>,
}

#[utoipa::path(
    post, path = "/public/delivery-orders", tag = "delivery-public", request_body = DeliveryOrderInput,
    responses((status = 201, body = DeliveryOrder), AppErrorResponse)
)]
pub async fn create_delivery_order(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    secret: web::Data<JwtSecret>,
    hub: web::Data<DeliveryHub>,
    body: web::Json<DeliveryOrderInput>,
) -> Result<HttpResponse, AppError> {
    validate_channel(&body.channel)?;
    let phone = normalize_phone(&body.customer_phone)?;

    if !whatsapp::verify_device_token(&secret.0, &phone, &body.device_token) {
        return Err(AppError::Unauthorized("Phone not verified on this device.".into()));
    }
    if body.customer_name.trim().is_empty() {
        return Err(AppError::BadRequest("Customer name is required".into()));
    }

    // Idempotency replay.
    let idem = req
        .headers()
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok());
    if let Some(key) = idem
        && let Some(existing) = super::staff::fetch_delivery_order_by_idem(pool.get_ref(), key).await?
    {
        return Ok(HttpResponse::Ok().json(existing));
    }

    // Branch + channel must be open right now (enabled + accepting + window + shift).
    let branch: Option<(Uuid, String, String)> = sqlx::query_as(
        "SELECT org_id, code, timezone FROM branches WHERE id = $1 AND is_active = true AND deleted_at IS NULL",
    )
    .bind(body.branch_id)
    .fetch_optional(pool.get_ref())
    .await?;
    let (org_id, branch_code, _tz) = branch.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    let open_row: Option<BranchOpenRow> = sqlx::query_as(
        r#"SELECT b.id, b.name, b.code,
                  COALESCE(s.in_mall_enabled, false)  AS in_mall_enabled,
                  COALESCE(s.outside_enabled, false)  AS outside_enabled,
                  COALESCE(s.in_mall_override, 'auto') AS in_mall_override,
                  COALESCE(s.outside_override, 'auto') AS outside_override,
                  s.in_mall_open_time, s.in_mall_close_time,
                  s.outside_open_time, s.outside_close_time,
                  (now() AT TIME ZONE b.timezone)::time AS local_time,
                  EXISTS(SELECT 1 FROM shifts sh WHERE sh.branch_id = b.id AND sh.status = 'open') AS has_open_shift
           FROM branches b LEFT JOIN branch_delivery_settings s ON s.branch_id = b.id WHERE b.id = $1"#,
    )
    .bind(body.branch_id)
    .fetch_optional(pool.get_ref())
    .await?;
    let r = open_row.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    let is_outside = body.channel == CHANNEL_OUTSIDE;
    let open = if is_outside {
        channel_open(r.outside_enabled, &r.outside_override, r.outside_open_time, r.outside_close_time, r.local_time, r.has_open_shift)
    } else {
        channel_open(r.in_mall_enabled, &r.in_mall_override, r.in_mall_open_time, r.in_mall_close_time, r.local_time, r.has_open_shift)
    };
    if !open {
        return Err(AppError::Conflict("This branch is not accepting orders for this channel right now.".into()));
    }

    let now = Utc::now();

    // Server-price + freeze the cart.
    let resolved = snapshot::resolve_cart(pool.get_ref(), org_id, body.branch_id, &body.channel, &body.items, now).await?;

    // Server-authoritative delivery fee.
    let (delivery_fee, zone_id, road_distance): (i32, Option<Uuid>, Option<i32>) = if is_outside {
        let (Some(lat), Some(lng)) = (body.customer_lat, body.customer_lng) else {
            return Err(AppError::BadRequest("A delivery location is required for outside delivery".into()));
        };
        match compute_outside_fee(pool.get_ref(), body.branch_id, LatLng { lat, lng }).await? {
            FeeOutcome::Ok { fee, zone_id, distance_meters, .. } => (fee, Some(zone_id), Some(distance_meters)),
            FeeOutcome::OutOfRange => return Err(AppError::BadRequest("This location is outside the delivery range".into())),
            FeeOutcome::Unavailable => return Err(AppError::Conflict("Delivery distance is temporarily unavailable. Please try again.".into())),
        }
    } else {
        let fee: i32 = sqlx::query_scalar("SELECT COALESCE(in_mall_fee, 0) FROM branch_delivery_settings WHERE branch_id = $1")
            .bind(body.branch_id)
            .fetch_optional(pool.get_ref())
            .await?
            .unwrap_or(0);
        (fee, None, None)
    };

    let subtotal = resolved.subtotal;
    let total = subtotal + delivery_fee;

    // Mint the delivery_ref from its own counter (business date in branch tz).
    let mut tx = pool.get_ref().begin().await?;
    let biz_date: chrono::NaiveDate = sqlx::query_scalar(
        "SELECT ($1::timestamptz AT TIME ZONE timezone)::date FROM branches WHERE id = $2",
    )
    .bind(now)
    .bind(body.branch_id)
    .fetch_one(&mut *tx)
    .await?;
    let seq: i32 = sqlx::query_scalar(
        "INSERT INTO delivery_ref_counters (branch_id, business_date, last_seq) VALUES ($1, $2, 1) \
         ON CONFLICT (branch_id, business_date) DO UPDATE SET last_seq = delivery_ref_counters.last_seq + 1 \
         RETURNING last_seq",
    )
    .bind(body.branch_id)
    .bind(biz_date)
    .fetch_one(&mut *tx)
    .await?;
    let delivery_ref = format!("D-{}-{}-{:04}", branch_code, biz_date.format("%y%m%d"), seq);

    let cart_json = serde_json::to_value(&resolved.snapshot).map_err(|_| AppError::Internal)?;
    let deductions_json = serde_json::to_value(&resolved.deductions).map_err(|_| AppError::Internal)?;

    let id: Uuid = sqlx::query_scalar(
        r#"INSERT INTO delivery_orders
            (org_id, branch_id, channel, delivery_ref, customer_name, customer_phone,
             place_name, floor, unit_number, landmark, address_line, delivery_notes,
             customer_lat, customer_lng, delivery_zone_id, road_distance_meters,
             subtotal, delivery_fee, total, cart, deductions_snapshot,
             payment_method_hint, otp_verified, idempotency_key)
           VALUES ($1, $2, $3::delivery_channel, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                   $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, true, $23)
           RETURNING id"#,
    )
    .bind(org_id)
    .bind(body.branch_id)
    .bind(&body.channel)
    .bind(&delivery_ref)
    .bind(body.customer_name.trim())
    .bind(&phone)
    .bind(&body.place_name)
    .bind(&body.floor)
    .bind(&body.unit_number)
    .bind(&body.landmark)
    .bind(&body.address_line)
    .bind(&body.delivery_notes)
    .bind(body.customer_lat)
    .bind(body.customer_lng)
    .bind(zone_id)
    .bind(road_distance)
    .bind(subtotal)
    .bind(delivery_fee)
    .bind(total)
    .bind(cart_json)
    .bind(deductions_json)
    .bind(&body.payment_method_hint)
    .bind(idem)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    whatsapp::send_message(phone, whatsapp::build_order_received_message(&delivery_ref));

    let order = super::staff::fetch_delivery_order(pool.get_ref(), id)
        .await?
        .ok_or(AppError::Internal)?;
    hub.publish(
        order.branch_id,
        DeliveryEvent { event_type: "created".into(), order: order.clone() },
    );
    Ok(HttpResponse::Created().json(order))
}
