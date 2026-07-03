//! Public (unauthenticated, rate-limited) delivery surface: branch selector,
//! channel-resolved menu, OSRM-proxied delivery quote, WhatsApp OTP, and order
//! intake. Pricing is 100% server-side and frozen into the delivery_orders row.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::snapshot::{self, CartLineInput};
use super::staff::DeliveryOrder;
use super::whatsapp;
use super::{
    CHANNEL_IN_MALL, CHANNEL_OUTSIDE, CHANNEL_PICKUP, CHANNEL_UMBRELLA, MAX_ADDRESS_LEN,
    MAX_NAME_LEN, MAX_NOTES_LEN, MAX_OTP_CODE_LEN, MAX_SHORT_TEXT_LEN, channel_discount_col,
    channel_fee_col, channel_is_flat, channel_open, normalize_phone, validate_channel,
    validate_coords, validate_optional_text, validate_payment_hint, validate_required_text,
};
use crate::auth::jwt::JwtSecret;
use crate::errors::{AppError, AppErrorResponse};
use crate::geo::osrm::{LatLng, OsrmError, haversine_meters, road_distance_meters};
use crate::realtime::event::{BranchEvent, Topic};
use crate::realtime::hub::BranchEventHub;

// ── Public branch selector ────────────────────────────────────

#[derive(Serialize, ToSchema)]
pub struct PublicBranch {
    pub id: Uuid,
    pub name: String,
    pub code: String,
    pub in_mall_enabled: bool,
    pub outside_enabled: bool,
    pub umbrella_enabled: bool,
    pub pickup_enabled: bool,
    /// Effective-open right now (enabled + open shift + override + window).
    pub in_mall_open_now: bool,
    pub outside_open_now: bool,
    pub umbrella_open_now: bool,
    pub pickup_open_now: bool,
    /// When false, the public checkout skips OTP verification for this branch.
    pub otp_required: bool,
    /// When false, in-mall ordering does not require a device GPS location.
    pub in_mall_require_location: bool,
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
    umbrella_enabled: bool,
    pickup_enabled: bool,
    in_mall_override: String,
    outside_override: String,
    umbrella_override: String,
    pickup_override: String,
    in_mall_open_time: Option<chrono::NaiveTime>,
    in_mall_close_time: Option<chrono::NaiveTime>,
    outside_open_time: Option<chrono::NaiveTime>,
    outside_close_time: Option<chrono::NaiveTime>,
    umbrella_open_time: Option<chrono::NaiveTime>,
    umbrella_close_time: Option<chrono::NaiveTime>,
    pickup_open_time: Option<chrono::NaiveTime>,
    pickup_close_time: Option<chrono::NaiveTime>,
    local_time: chrono::NaiveTime,
    has_open_shift: bool,
    otp_required: bool,
    in_mall_require_location: bool,
}

impl BranchOpenRow {
    /// Effective-open for one channel, by name.
    fn open_for(&self, channel: &str) -> bool {
        let (enabled, ov, open, close) = match channel {
            CHANNEL_OUTSIDE => (
                self.outside_enabled,
                self.outside_override.as_str(),
                self.outside_open_time,
                self.outside_close_time,
            ),
            CHANNEL_UMBRELLA => (
                self.umbrella_enabled,
                self.umbrella_override.as_str(),
                self.umbrella_open_time,
                self.umbrella_close_time,
            ),
            CHANNEL_PICKUP => (
                self.pickup_enabled,
                self.pickup_override.as_str(),
                self.pickup_open_time,
                self.pickup_close_time,
            ),
            _ => (
                self.in_mall_enabled,
                self.in_mall_override.as_str(),
                self.in_mall_open_time,
                self.in_mall_close_time,
            ),
        };
        channel_open(
            enabled,
            ov,
            open,
            close,
            self.local_time,
            self.has_open_shift,
        )
    }
}

/// SELECT list shared by `public_branches` and `channel_open_now` — every
/// channel's enabled/override/window plus the org-local time + open-shift flag.
const BRANCH_OPEN_SELECT: &str = r#"b.id, b.name, b.code,
    COALESCE(s.in_mall_enabled, false)  AS in_mall_enabled,
    COALESCE(s.outside_enabled, false)  AS outside_enabled,
    COALESCE(s.umbrella_enabled, false) AS umbrella_enabled,
    COALESCE(s.pickup_enabled, false)   AS pickup_enabled,
    COALESCE(s.in_mall_override, 'auto')  AS in_mall_override,
    COALESCE(s.outside_override, 'auto')  AS outside_override,
    COALESCE(s.umbrella_override, 'auto') AS umbrella_override,
    COALESCE(s.pickup_override, 'auto')   AS pickup_override,
    s.in_mall_open_time, s.in_mall_close_time,
    s.outside_open_time, s.outside_close_time,
    s.umbrella_open_time, s.umbrella_close_time,
    s.pickup_open_time, s.pickup_close_time,
    COALESCE(s.otp_required, true) AS otp_required,
    COALESCE(s.in_mall_require_location, true) AS in_mall_require_location,
    (now() AT TIME ZONE COALESCE(b.timezone, o.timezone)::text)::time AS local_time,
    EXISTS(SELECT 1 FROM shifts sh WHERE sh.branch_id = b.id AND sh.status = 'open') AS has_open_shift"#;

#[utoipa::path(
    get, path = "/public/branches", tag = "delivery-public", params(PublicBranchesQuery),
    responses((status = 200, body = [PublicBranch]), AppErrorResponse)
)]
pub async fn public_branches(
    pool: web::Data<PgPool>,
    query: web::Query<PublicBranchesQuery>,
) -> Result<HttpResponse, AppError> {
    let rows: Vec<BranchOpenRow> = sqlx::query_as(&format!(
        r#"SELECT {BRANCH_OPEN_SELECT}
           FROM branches b
           JOIN organizations o ON o.id = b.org_id
           LEFT JOIN branch_delivery_settings s ON s.branch_id = b.id
           WHERE b.org_id = $1 AND b.is_active = true AND b.deleted_at IS NULL
             AND (COALESCE(s.in_mall_enabled, false) OR COALESCE(s.outside_enabled, false)
                  OR COALESCE(s.umbrella_enabled, false) OR COALESCE(s.pickup_enabled, false))
           ORDER BY b.name"#,
    ))
    .bind(query.org_id)
    .fetch_all(pool.get_ref())
    .await?;

    let branches: Vec<PublicBranch> = rows
        .into_iter()
        .map(|r| PublicBranch {
            in_mall_open_now: r.open_for(CHANNEL_IN_MALL),
            outside_open_now: r.open_for(CHANNEL_OUTSIDE),
            umbrella_open_now: r.open_for(CHANNEL_UMBRELLA),
            pickup_open_now: r.open_for(CHANNEL_PICKUP),
            in_mall_enabled: r.in_mall_enabled,
            outside_enabled: r.outside_enabled,
            umbrella_enabled: r.umbrella_enabled,
            pickup_enabled: r.pickup_enabled,
            otp_required: r.otp_required,
            in_mall_require_location: r.in_mall_require_location,
            id: r.id,
            name: r.name,
            code: r.code,
        })
        .collect();
    Ok(HttpResponse::Ok().json(branches))
}

/// Whether a branch channel is open *right now* (enabled + override + window +
/// an open shift). Mirrors the per-row computation in `public_branches`, reused
/// to gate the menu, the quote, and order intake against direct-link bypass.
async fn channel_open_now(pool: &PgPool, branch_id: Uuid, channel: &str) -> Result<bool, AppError> {
    let row: Option<BranchOpenRow> = sqlx::query_as(&format!(
        r#"SELECT {BRANCH_OPEN_SELECT}
           FROM branches b
           JOIN organizations o ON o.id = b.org_id
           LEFT JOIN branch_delivery_settings s ON s.branch_id = b.id
           WHERE b.id = $1 AND b.is_active = true AND b.deleted_at IS NULL"#,
    ))
    .bind(branch_id)
    .fetch_optional(pool)
    .await?;
    let Some(r) = row else { return Ok(false) };
    Ok(r.open_for(channel))
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

/// One option inside a per-item modifier group. `option_id` is the STABLE id —
/// it equals the legacy `addon_item_id`, so order intake accepts it unchanged in
/// `addons[].addon_item_id` (menu-unification stable-id rule).
#[derive(Serialize, ToSchema, Clone)]
pub struct DeliveryModifierOption {
    pub option_id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    /// Channel-effective surcharge (piastres): branch_channel → branch →
    /// channel → catalog default. Unavailable options are excluded entirely.
    pub price: i32,
}

/// A per-item modifier group from the unified model (`menu_item_modifier_groups`
/// → `modifier_groups`/`modifier_options`), constraints resolved (attachment
/// overrides beat group defaults) and options already filtered to the
/// attachment's `included_option_ids`. Only addon-sourced options appear here —
/// the item's priced optionals stay in `optionals`. Empty until the org's
/// catalog is backfilled onto the unified tables; the customizer falls back to
/// the flat `addons` catalog + `allowed_addon_ids` in that case.
#[derive(Serialize, ToSchema)]
pub struct DeliveryModifierGroup {
    pub group_id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    /// "single" | "multi".
    pub selection_type: String,
    pub min_selections: i32,
    pub max_selections: Option<i32>,
    pub is_required: bool,
    /// The group's legacy addon type (`milk_type` / `coffee_type` / `extra` /
    /// custom) — the swap-family hint the customizer keys its delta-price
    /// estimate on. `None` for groups with no legacy lineage.
    pub addon_type: Option<String>,
    pub options: Vec<DeliveryModifierOption>,
}

#[derive(Serialize, ToSchema)]
pub struct DeliveryMenuItem {
    pub id: Uuid,
    pub category_id: Option<Uuid>,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub description: Option<String>,
    #[serde(serialize_with = "crate::uploads::handlers::serialize_opt_url")]
    pub image_url: Option<String>,
    pub price: i32,
    pub sizes: Vec<DeliveryMenuSize>,
    pub optionals: Vec<DeliveryOptionalField>,
    /// The item's base/default milk: the `milk_type` addon whose ingredient
    /// matches the item recipe's milk ingredient. The online customizer
    /// pre-selects it (mirrors the POS default-milk selection). `None` when the
    /// item has no milk in its recipe or no matching milk addon exists.
    pub default_milk_addon_id: Option<Uuid>,
    /// Explicit per-item addon allowlist (IDs from `menu_item_allowed_addons`).
    /// When non-empty the customizer filters the global catalog to these IDs by
    /// default, with a "show all" escape hatch. Empty = no restriction.
    pub allowed_addon_ids: Vec<Uuid>,
    /// The item's modifier groups (unified model), channel-effective. Empty ⇒
    /// the customizer falls back to `addons` + `allowed_addon_ids`.
    pub modifier_groups: Vec<DeliveryModifierGroup>,
}

#[derive(Serialize, ToSchema)]
pub struct DeliveryMenuCategory {
    pub id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    #[serde(serialize_with = "crate::uploads::handlers::serialize_opt_url")]
    pub image_url: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct DeliveryMenu {
    pub categories: Vec<DeliveryMenuCategory>,
    pub items: Vec<DeliveryMenuItem>,
    /// Org-wide addon catalog (global, POS model): channel-effective, grouped by
    /// `type`, applicable to every item. Channel-unavailable options are excluded.
    pub addons: Vec<DeliveryAddonOption>,
    /// The active discount for this channel (customer-facing) or `null`. Applies
    /// to the item subtotal only — the delivery fee is always charged in full.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discount: Option<DeliveryMenuDiscount>,
}

/// Customer-facing summary of a channel's active discount, so the public UI can
/// tell the customer "you've got X off" and show a discounted estimate.
#[derive(Serialize, ToSchema)]
pub struct DeliveryMenuDiscount {
    pub id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    /// "percentage" | "fixed".
    pub dtype: String,
    /// Percentage points (0-100) for `percentage`; piastres for `fixed`.
    pub value: i32,
}

#[derive(Deserialize, IntoParams)]
pub struct ChannelParam {
    pub channel: String,
    /// Read-only browse preview. When `true`, the menu is returned even if the
    /// channel is closed right now, so customers can browse while a branch is
    /// closed. This NEVER relaxes the channel-*enabled* check, and the
    /// delivery-quote / order-intake endpoints stay gated on open-now — so a
    /// preview can never become a real order against a closed channel.
    pub preview: Option<bool>,
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
        return Err(AppError::NotFound(
            "This branch does not offer this channel".into(),
        ));
    }
    // Read-only browse preview skips the open-now gate (so a closed-but-enabled
    // channel can still show its menu). The enabled check above always stands,
    // and quote/intake remain gated on open-now — preview can't become an order.
    if !query.preview.unwrap_or(false)
        && !channel_open_now(pool.get_ref(), branch_id, &query.channel).await?
    {
        return Err(AppError::Conflict(
            "This channel is closed right now.".into(),
        ));
    }

    // The channel's active discount (customer-facing), if any.
    let discount_col = channel_discount_col(&query.channel);
    let discount: Option<DeliveryMenuDiscount> =
        sqlx::query_as::<_, (Uuid, String, serde_json::Value, String, i32)>(&format!(
            "SELECT d.id, d.name, d.name_translations, d.type::text, d.value \
             FROM branch_delivery_settings s JOIN discounts d ON d.id = s.{discount_col} \
             WHERE s.branch_id = $1 AND d.is_active = true"
        ))
        .bind(branch_id)
        .fetch_optional(pool.get_ref())
        .await?
        .map(
            |(id, name, name_translations, dtype, value)| DeliveryMenuDiscount {
                id,
                name,
                name_translations,
                dtype,
                value,
            },
        );

    let categories: Vec<DeliveryMenuCategory> =
        sqlx::query_as::<_, (Uuid, String, serde_json::Value, Option<String>)>(
            "SELECT id, name, name_translations, image_url FROM categories \
         WHERE org_id = $1 AND is_active = true AND deleted_at IS NULL ORDER BY name",
        )
        .bind(org_id)
        .fetch_all(pool.get_ref())
        .await?
        .into_iter()
        .map(
            |(id, name, name_translations, image_url)| DeliveryMenuCategory {
                id,
                name,
                name_translations,
                image_url,
            },
        )
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

    let mut sizes_by_item: std::collections::HashMap<Uuid, Vec<DeliveryMenuSize>> =
        std::collections::HashMap::new();
    for (item_id, label, catalog_price) in catalog_sizes {
        let price = branch_size_map
            .get(&(item_id, label.clone()))
            .copied()
            .unwrap_or(catalog_price);
        sizes_by_item
            .entry(item_id)
            .or_default()
            .push(DeliveryMenuSize { label, price });
    }

    // Global org-wide addon catalog (channel-effective), loaded once per request.
    let addons = load_addon_catalog(pool.get_ref(), org_id, branch_id, &query.channel).await?;

    // Optional fields per item.
    let mut optionals_by_item = load_optional_fields(pool.get_ref(), &item_ids).await?;

    // Default/base milk per item (POS pre-select), batched over the item list.
    let mut default_milk_by_item = load_default_milk(pool.get_ref(), &item_ids).await?;
    let mut allowed_addons_by_item = load_allowed_addon_ids(pool.get_ref(), &item_ids).await?;
    // Per-item modifier groups (unified model), channel-effective — empty until
    // the org's catalog is backfilled onto the unified tables.
    let mut modifier_groups_by_item =
        load_modifier_groups(pool.get_ref(), &item_ids, branch_id, &query.channel).await?;

    let items: Vec<DeliveryMenuItem> = item_rows
        .into_iter()
        .map(
            |(id, category_id, name, name_translations, description, image_url, price)| {
                DeliveryMenuItem {
                    sizes: sizes_by_item.remove(&id).unwrap_or_default(),
                    optionals: optionals_by_item.remove(&id).unwrap_or_default(),
                    default_milk_addon_id: default_milk_by_item.remove(&id),
                    allowed_addon_ids: allowed_addons_by_item.remove(&id).unwrap_or_default(),
                    modifier_groups: modifier_groups_by_item.remove(&id).unwrap_or_default(),
                    id,
                    category_id,
                    name,
                    name_translations,
                    description,
                    image_url,
                    price,
                }
            },
        )
        .collect();

    Ok(HttpResponse::Ok().json(DeliveryMenu {
        categories,
        items,
        addons,
        discount,
    }))
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
        .map(
            |(addon_item_id, name, name_translations, atype, price, is_available)| {
                DeliveryAddonOption {
                    addon_item_id,
                    name,
                    name_translations,
                    r#type: atype,
                    price,
                    is_available,
                }
            },
        )
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
        by_item
            .entry(menu_item_id)
            .or_default()
            .push(DeliveryOptionalField {
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

/// Batch-load per-item modifier groups from the unified tables
/// (`menu_item_modifier_groups` → `modifier_groups`/`modifier_options`),
/// channel-effective (branch_channel → branch → channel → catalog default, per
/// CONTRACT §3). Addon-sourced options only; `included_option_ids` honoured;
/// effectively-unavailable options excluded (same convention as the flat addon
/// catalog); groups left with no options are dropped. Returns an empty map for
/// orgs not yet backfilled — callers treat that as "no unified groups".
async fn load_modifier_groups(
    pool: &PgPool,
    item_ids: &[Uuid],
    branch_id: Uuid,
    channel: &str,
) -> Result<std::collections::HashMap<Uuid, Vec<DeliveryModifierGroup>>, AppError> {
    if item_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        Uuid,              // menu_item_id
        Uuid,              // group_id
        String,            // group name
        serde_json::Value, // group name_translations
        String,            // selection_type
        i32,               // effective min
        Option<i32>,       // effective max
        bool,              // effective required
        Option<String>,    // legacy_addon_type
        Uuid,              // option id
        String,            // option name
        serde_json::Value, // option name_translations
        i32,               // effective price
        bool,              // effective availability
    )> = sqlx::query_as(
        "SELECT mimg.menu_item_id, mimg.group_id, g.name, g.name_translations, \
                g.selection_type, \
                COALESCE(mimg.min_override, g.min_selections)      AS min_selections, \
                COALESCE(mimg.max_override, g.max_selections)      AS max_selections, \
                COALESCE(mimg.is_required_override, g.is_required) AS is_required, \
                g.legacy_addon_type, \
                o.id, o.name, o.name_translations, \
                COALESCE(bc.price, b.price, c.price, o.price)                    AS price, \
                COALESCE(bc.is_available, b.is_available, c.is_available, true)  AS is_available \
         FROM menu_item_modifier_groups mimg \
         JOIN modifier_groups g  ON g.id = mimg.group_id AND g.is_active = true \
         JOIN modifier_options o ON o.group_id = g.id AND o.is_active = true \
                                AND o.legacy_source = 'addon' \
         LEFT JOIN menu_price_overrides bc \
                ON bc.target_type = 'modifier_option' AND bc.target_id = o.id \
               AND bc.scope = 'branch_channel' AND bc.branch_id = $2 \
               AND bc.channel = $3::delivery_channel \
         LEFT JOIN menu_price_overrides b \
                ON b.target_type = 'modifier_option' AND b.target_id = o.id \
               AND b.scope = 'branch' AND b.branch_id = $2 \
         LEFT JOIN menu_price_overrides c \
                ON c.target_type = 'modifier_option' AND c.target_id = o.id \
               AND c.scope = 'channel' AND c.channel = $3::delivery_channel \
         WHERE mimg.menu_item_id = ANY($1) \
           AND (mimg.included_option_ids IS NULL OR o.id = ANY(mimg.included_option_ids)) \
         ORDER BY mimg.menu_item_id, mimg.sort, g.name, o.sort, o.name",
    )
    .bind(item_ids)
    .bind(branch_id)
    .bind(channel)
    .fetch_all(pool)
    .await?;

    let mut by_item: std::collections::HashMap<Uuid, Vec<DeliveryModifierGroup>> =
        std::collections::HashMap::new();
    for (
        item_id,
        group_id,
        gname,
        gtrans,
        seltype,
        min,
        max,
        required,
        addon_type,
        oid,
        oname,
        otrans,
        price,
        avail,
    ) in rows
    {
        if !avail {
            continue; // customer-facing: unavailable options are excluded entirely
        }
        let groups = by_item.entry(item_id).or_default();
        let group = match groups.last_mut() {
            Some(g) if g.group_id == group_id => g,
            _ => {
                groups.push(DeliveryModifierGroup {
                    group_id,
                    name: gname,
                    name_translations: gtrans,
                    selection_type: seltype,
                    min_selections: min,
                    max_selections: max,
                    is_required: required,
                    addon_type,
                    options: Vec::new(),
                });
                groups.last_mut().expect("just pushed")
            }
        };
        group.options.push(DeliveryModifierOption {
            option_id: oid,
            name: oname,
            name_translations: otrans,
            price,
        });
    }
    // Drop groups whose options all resolved unavailable.
    for groups in by_item.values_mut() {
        groups.retain(|g| !g.options.is_empty());
    }
    Ok(by_item)
}

/// Batch-load per-item allowed addon IDs from `menu_item_allowed_addons`.
/// Items with no rows → absent from map (empty Vec after the `.remove` default).
async fn load_allowed_addon_ids(
    pool: &PgPool,
    item_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, Vec<Uuid>>, AppError> {
    let mut by_item: std::collections::HashMap<Uuid, Vec<Uuid>> = std::collections::HashMap::new();
    if item_ids.is_empty() {
        return Ok(by_item);
    }
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT menu_item_id, addon_item_id \
         FROM menu_item_allowed_addons \
         WHERE menu_item_id = ANY($1) \
         ORDER BY menu_item_id, sort_order ASC, created_at ASC",
    )
    .bind(item_ids)
    .fetch_all(pool)
    .await?;
    for (menu_item_id, addon_id) in rows {
        by_item.entry(menu_item_id).or_default().push(addon_id);
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

pub enum FeeOutcome {
    Ok {
        fee: i32,
        zone_id: Uuid,
        zone_name: String,
        distance_meters: i32,
    },
    OutOfRange,
    Unavailable,
}

impl FeeOutcome {
    /// The flat fee (piastres) for a covered quote, or `None` when the address is
    /// out of range / delivery is unavailable.
    pub fn fee(&self) -> Option<i32> {
        match self {
            FeeOutcome::Ok { fee, .. } => Some(*fee),
            _ => None,
        }
    }
}

#[derive(sqlx::FromRow, Clone)]
pub struct ZoneRow {
    pub id: Uuid,
    pub name: String,
    pub fee: i32,
    pub max_road_distance_meters: i32,
}

/// Pure zone match (no OSRM, no DB) so it can be unit-tested. `zones` must be
/// ordered by ring_order ASC — the smallest covering ring's flat fee wins.
pub fn select_zone_fee(distance_i: i32, max_dist: Option<i32>, zones: &[ZoneRow]) -> FeeOutcome {
    if max_dist.is_some_and(|m| distance_i > m) {
        return FeeOutcome::OutOfRange;
    }
    match zones
        .iter()
        .find(|z| z.max_road_distance_meters >= distance_i)
    {
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
    let branch_pt = LatLng {
        lat: blat,
        lng: blng,
    };
    let distance = match road_distance_meters(branch_pt, cust).await {
        Ok(d) => d,
        Err(OsrmError::NotConfigured)
        | Err(OsrmError::Unreachable)
        | Err(OsrmError::BadResponse) => haversine_meters(branch_pt, cust),
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
    validate_coords(query.lat, query.lng)?;
    let outside = query.channel == CHANNEL_OUTSIDE;
    // Channel is validated above, so interpolating it is safe.
    let fee_col = channel_fee_col(&query.channel);

    let row: Option<(bool, i32)> = sqlx::query_as(&format!(
        "SELECT COALESCE(s.{ch}_enabled, false), COALESCE(s.{fee_col}, 0) \
         FROM branches b LEFT JOIN branch_delivery_settings s ON s.branch_id = b.id \
         WHERE b.id = $1 AND b.is_active = true AND b.deleted_at IS NULL",
        ch = query.channel,
        fee_col = fee_col,
    ))
    .bind(branch_id)
    .fetch_optional(pool.get_ref())
    .await?;
    let (enabled, flat_fee) = row.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    if !enabled {
        return Err(AppError::NotFound(
            "This branch does not offer this channel".into(),
        ));
    }
    if !channel_open_now(pool.get_ref(), branch_id, &query.channel).await? {
        return Err(AppError::Conflict(
            "This channel is closed right now.".into(),
        ));
    }

    // Flat-fee channels (in-mall / umbrella / pickup): a flat per-branch fee, no
    // zones/OSRM. Only in-mall reports a walking (haversine) distance from the
    // branch as a teller spam signal; umbrella/pickup have no mappable location.
    if !outside {
        let distance_meters = if query.channel == CHANNEL_IN_MALL {
            let coords: Option<(Option<f64>, Option<f64>)> =
                sqlx::query_as("SELECT latitude, longitude FROM branches WHERE id = $1")
                    .bind(branch_id)
                    .fetch_optional(pool.get_ref())
                    .await?;
            coords.and_then(|(blat, blng)| match (blat, blng) {
                (Some(blat), Some(blng)) => Some(
                    haversine_meters(
                        LatLng {
                            lat: blat,
                            lng: blng,
                        },
                        LatLng {
                            lat: query.lat,
                            lng: query.lng,
                        },
                    )
                    .round() as i32,
                ),
                _ => None,
            })
        } else {
            None
        };
        return Ok(HttpResponse::Ok().json(QuoteResponse {
            status: "ok".into(),
            zone_id: None,
            zone_name: None,
            distance_meters,
            fee: Some(flat_fee),
        }));
    }

    let outcome = compute_outside_fee(
        pool.get_ref(),
        branch_id,
        LatLng {
            lat: query.lat,
            lng: query.lng,
        },
    )
    .await?;
    let resp = match outcome {
        FeeOutcome::Ok {
            fee,
            zone_id,
            zone_name,
            distance_meters,
        } => QuoteResponse {
            status: "ok".into(),
            zone_id: Some(zone_id),
            zone_name: Some(zone_name),
            distance_meters: Some(distance_meters),
            fee: Some(fee),
        },
        FeeOutcome::OutOfRange => QuoteResponse {
            status: "out_of_range".into(),
            zone_id: None,
            zone_name: None,
            distance_meters: None,
            fee: None,
        },
        FeeOutcome::Unavailable => QuoteResponse {
            status: "unavailable".into(),
            zone_id: None,
            zone_name: None,
            distance_meters: None,
            fee: None,
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
        return Err(AppError::Conflict(
            "A code was just sent. Please wait a minute.".into(),
        ));
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

    whatsapp::send_message(
        pool.get_ref().clone(),
        phone,
        whatsapp::build_otp_message(&code),
    );

    Ok(HttpResponse::Ok().json(OtpRequestResponse { sent: true }))
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
    // The code is a short numeric string; reject anything else before bcrypt.
    if body.code.is_empty()
        || body.code.len() > MAX_OTP_CODE_LEN
        || !body.code.chars().all(|c| c.is_ascii_digit())
    {
        return Err(AppError::BadRequest("Incorrect code.".into()));
    }

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
        return Err(AppError::BadRequest(
            "Too many attempts — request a new code.".into(),
        ));
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
    Ok(HttpResponse::Ok().json(OtpVerifyResponse {
        device_token: token,
    }))
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
    hub: web::Data<BranchEventHub>,
    body: web::Json<DeliveryOrderInput>,
) -> Result<HttpResponse, AppError> {
    validate_channel(&body.channel)?;
    // Strict field validation up front (untrusted public client): bound every
    // free-text field, the payment hint, and the coordinates before any DB work.
    validate_payment_hint(&body.payment_method_hint)?;
    validate_required_text("Customer name", &body.customer_name, MAX_NAME_LEN)?;
    validate_optional_text("Landmark", body.landmark.as_deref(), MAX_SHORT_TEXT_LEN)?;
    validate_optional_text(
        "Delivery notes",
        body.delivery_notes.as_deref(),
        MAX_NOTES_LEN,
    )?;
    // Channel-appropriate destination details. In a mall the runner finds the
    // customer by shop/company + floor + unit, so those are required and there is
    // no street address. For an outside (street) order the dropped pin sets the
    // route but a written address line finds the door, so the address is required.
    match body.channel.as_str() {
        CHANNEL_OUTSIDE => {
            validate_required_text(
                "Delivery address",
                body.address_line.as_deref().unwrap_or(""),
                MAX_ADDRESS_LEN,
            )?;
            validate_optional_text("Place name", body.place_name.as_deref(), MAX_SHORT_TEXT_LEN)?;
            validate_optional_text("Floor", body.floor.as_deref(), MAX_SHORT_TEXT_LEN)?;
            validate_optional_text(
                "Unit number",
                body.unit_number.as_deref(),
                MAX_SHORT_TEXT_LEN,
            )?;
        }
        CHANNEL_UMBRELLA => {
            // Umbrella/sunbed: the runner finds the customer by umbrella number
            // (stored in place_name); an optional section/zone goes in landmark.
            validate_required_text(
                "Umbrella number",
                body.place_name.as_deref().unwrap_or(""),
                MAX_SHORT_TEXT_LEN,
            )?;
            validate_optional_text("Section", body.landmark.as_deref(), MAX_SHORT_TEXT_LEN)?;
        }
        CHANNEL_PICKUP => {
            // Self-collect: no destination details; name + phone (below) suffice.
            validate_optional_text("Note", body.place_name.as_deref(), MAX_SHORT_TEXT_LEN)?;
        }
        _ => {
            // in_mall
            validate_required_text(
                "Shop or company name",
                body.place_name.as_deref().unwrap_or(""),
                MAX_SHORT_TEXT_LEN,
            )?;
            validate_required_text(
                "Floor",
                body.floor.as_deref().unwrap_or(""),
                MAX_SHORT_TEXT_LEN,
            )?;
            validate_required_text(
                "Unit or office",
                body.unit_number.as_deref().unwrap_or(""),
                MAX_SHORT_TEXT_LEN,
            )?;
            validate_optional_text("Address", body.address_line.as_deref(), MAX_ADDRESS_LEN)?;
        }
    }
    if let (Some(lat), Some(lng)) = (body.customer_lat, body.customer_lng) {
        validate_coords(lat, lng)?;
    }
    let phone = normalize_phone(&body.customer_phone)?;

    // OTP verification is per-branch (managers can turn it off in delivery settings).
    let otp_required: bool = sqlx::query_scalar(
        "SELECT COALESCE(otp_required, true) FROM branch_delivery_settings WHERE branch_id = $1",
    )
    .bind(body.branch_id)
    .fetch_optional(pool.get_ref())
    .await?
    .unwrap_or(true);
    if otp_required && !whatsapp::verify_device_token(&secret.0, &phone, &body.device_token) {
        return Err(AppError::Unauthorized(
            "Phone not verified on this device.".into(),
        ));
    }

    // Idempotency replay.
    let idem = req
        .headers()
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok());
    if let Some(key) = idem
        && let Some(existing) =
            super::staff::fetch_delivery_order_by_idem(pool.get_ref(), key).await?
    {
        return Ok(HttpResponse::Ok().json(existing));
    }

    // Branch + channel must be open right now (enabled + accepting + window + shift).
    let branch: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT org_id, code FROM branches WHERE id = $1 AND is_active = true AND deleted_at IS NULL",
    )
    .bind(body.branch_id)
    .fetch_optional(pool.get_ref())
    .await?;
    let (org_id, branch_code) =
        branch.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    if !channel_open_now(pool.get_ref(), body.branch_id, &body.channel).await? {
        return Err(AppError::Conflict(
            "This branch is not accepting orders for this channel right now.".into(),
        ));
    }
    let is_outside = body.channel == CHANNEL_OUTSIDE;

    let now = Utc::now();

    // Server-price + freeze the cart.
    let resolved = snapshot::resolve_cart(
        pool.get_ref(),
        org_id,
        body.branch_id,
        Some(body.channel.as_str()),
        &body.items,
        now,
    )
    .await?;

    // Server-authoritative delivery fee.
    let (delivery_fee, zone_id, road_distance): (i32, Option<Uuid>, Option<i32>) = if is_outside {
        let (Some(lat), Some(lng)) = (body.customer_lat, body.customer_lng) else {
            return Err(AppError::BadRequest(
                "A delivery location is required for outside delivery".into(),
            ));
        };
        match compute_outside_fee(pool.get_ref(), body.branch_id, LatLng { lat, lng }).await? {
            FeeOutcome::Ok {
                fee,
                zone_id,
                distance_meters,
                ..
            } => (fee, Some(zone_id), Some(distance_meters)),
            FeeOutcome::OutOfRange => {
                return Err(AppError::BadRequest(
                    "This location is outside the delivery range".into(),
                ));
            }
            FeeOutcome::Unavailable => {
                return Err(AppError::Conflict(
                    "Delivery distance is temporarily unavailable. Please try again.".into(),
                ));
            }
        }
    } else if channel_is_flat(&body.channel) {
        // Umbrella / pickup: a flat per-branch fee — no GPS, no zone, no distance.
        let fee: i32 = sqlx::query_scalar(&format!(
            "SELECT COALESCE({fee_col}, 0) FROM branch_delivery_settings WHERE branch_id = $1",
            fee_col = channel_fee_col(&body.channel),
        ))
        .bind(body.branch_id)
        .fetch_optional(pool.get_ref())
        .await?
        .unwrap_or(0);
        (fee, None, None)
    } else {
        // In-mall: the customer confirms they're at the branch with device GPS
        // (never a manual pin). Whether that location is *required* is a per-branch
        // manager toggle (`in_mall_require_location`) — indoor GPS is noisy, so a
        // branch may relax it. When a location IS sent it's registered for the
        // teller as a spam signal (walking/haversine distance); it never blocks.
        let settings: Option<(i32, bool)> = sqlx::query_as(
            "SELECT COALESCE(in_mall_fee, 0), COALESCE(in_mall_require_location, true) \
             FROM branch_delivery_settings WHERE branch_id = $1",
        )
        .bind(body.branch_id)
        .fetch_optional(pool.get_ref())
        .await?;
        let (fee, require_location) = settings.unwrap_or((0, true));

        let coords = match (body.customer_lat, body.customer_lng) {
            (Some(lat), Some(lng)) => Some((lat, lng)),
            _ if require_location => {
                return Err(AppError::BadRequest(
                    "A location is required to confirm you're at the branch.".into(),
                ));
            }
            _ => None,
        };

        let distance = if let Some((lat, lng)) = coords {
            let branch_coords: Option<(Option<f64>, Option<f64>)> =
                sqlx::query_as("SELECT latitude, longitude FROM branches WHERE id = $1")
                    .bind(body.branch_id)
                    .fetch_optional(pool.get_ref())
                    .await?;
            branch_coords.and_then(|(blat, blng)| match (blat, blng) {
                (Some(blat), Some(blng)) => Some(
                    haversine_meters(
                        LatLng {
                            lat: blat,
                            lng: blng,
                        },
                        LatLng { lat, lng },
                    )
                    .round() as i32,
                ),
                _ => None,
            })
        } else {
            None
        };
        (fee, None, distance)
    };

    let subtotal = resolved.subtotal;

    // Resolve + FREEZE the channel's discount onto this order. It applies to the
    // item subtotal only (the delivery fee is always charged in full). The
    // discount id is configured per channel on branch_delivery_settings; only an
    // *active* discount is honored, otherwise it silently drops to none.
    let discount_col = channel_discount_col(&body.channel);
    let configured_discount: Option<Uuid> = sqlx::query_scalar(&format!(
        "SELECT {discount_col} FROM branch_delivery_settings WHERE branch_id = $1"
    ))
    .bind(body.branch_id)
    .fetch_optional(pool.get_ref())
    .await?
    .flatten();

    let (discount_id, discount_type, discount_value, discount_amount): (
        Option<Uuid>,
        Option<String>,
        i32,
        i32,
    ) = match configured_discount {
        Some(did) => {
            let row: Option<(String, i32)> = sqlx::query_as(
                "SELECT type::text, value FROM discounts WHERE id = $1 AND is_active = true",
            )
            .bind(did)
            .fetch_optional(pool.get_ref())
            .await?;
            match row {
                Some((dtype, dvalue)) => {
                    let amt =
                        crate::discounts::handlers::calc_discount(Some(&dtype), dvalue, subtotal);
                    (Some(did), Some(dtype), dvalue, amt)
                }
                None => (None, None, 0, 0),
            }
        }
        None => (None, None, 0, 0),
    };

    let total = subtotal - discount_amount + delivery_fee;

    // Mint the delivery_ref from its own counter (business date in branch tz).
    let mut tx = pool.get_ref().begin().await?;
    let biz_date: chrono::NaiveDate = sqlx::query_scalar(
        "SELECT ($1::timestamptz AT TIME ZONE COALESCE(b.timezone, o.timezone)::text)::date
         FROM branches b JOIN organizations o ON o.id = b.org_id WHERE b.id = $2",
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
    let deductions_json =
        serde_json::to_value(&resolved.deductions).map_err(|_| AppError::Internal)?;

    let id: Uuid = sqlx::query_scalar(
        r#"INSERT INTO delivery_orders
            (org_id, branch_id, channel, delivery_ref, customer_name, customer_phone,
             place_name, floor, unit_number, landmark, address_line, delivery_notes,
             customer_lat, customer_lng, delivery_zone_id, road_distance_meters,
             subtotal, delivery_fee, total, cart, deductions_snapshot,
             payment_method_hint, otp_verified, idempotency_key,
             discount_id, discount_type, discount_value, discount_amount)
           VALUES ($1, $2, $3::delivery_channel, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                   $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, true, $23,
                   $24, $25::discount_type, $26, $27)
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
    .bind(discount_id)
    .bind(&discount_type)
    .bind(discount_value)
    .bind(discount_amount)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    whatsapp::send_message(
        pool.get_ref().clone(),
        phone,
        whatsapp::build_order_received_message(&delivery_ref, id),
    );

    let order = super::staff::fetch_delivery_order(pool.get_ref(), id)
        .await?
        .ok_or(AppError::Internal)?;
    hub.publish(
        order.branch_id,
        BranchEvent::new(Topic::Delivery, "delivery.created", &order),
    );
    Ok(HttpResponse::Created().json(order))
}

// ── Guest history: past orders by phone + org ─────────────────

#[derive(Deserialize, IntoParams)]
pub struct GuestHistoryQuery {
    pub phone: String,
    pub org_id: Uuid,
    #[serde(default)]
    pub device_token: Option<String>,
}

/// Compact item snapshot for the order history list.
#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct OrderHistoryItem {
    pub name: String,
    pub quantity: i32,
}

#[derive(Serialize, ToSchema)]
pub struct OrderHistorySummary {
    pub id: Uuid,
    pub delivery_ref: Option<String>,
    pub status: String,
    pub channel: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub branch_id: Uuid,
    pub branch_name: String,
    pub subtotal: i32,
    pub delivery_fee: i32,
    pub discount_amount: i32,
    pub total: i32,
    pub address_line: Option<String>,
    pub place_name: Option<String>,
    pub customer_lat: Option<f64>,
    pub customer_lng: Option<f64>,
    pub customer_name: String,
    /// Frozen cart snapshot: the items at the time of the order (for display).
    pub items: serde_json::Value,
}

#[derive(sqlx::FromRow)]
struct OrderHistoryRow {
    id: Uuid,
    delivery_ref: Option<String>,
    status: String,
    channel: String,
    created_at: chrono::DateTime<chrono::Utc>,
    branch_id: Uuid,
    branch_name: String,
    subtotal: i32,
    delivery_fee: i32,
    discount_amount: i32,
    total: i32,
    address_line: Option<String>,
    place_name: Option<String>,
    customer_lat: Option<f64>,
    customer_lng: Option<f64>,
    customer_name: String,
    cart: serde_json::Value,
}

#[utoipa::path(
    get, path = "/public/delivery-orders/history", tag = "delivery-public",
    params(GuestHistoryQuery),
    responses((status = 200, body = [OrderHistorySummary]), AppErrorResponse)
)]
pub async fn guest_order_history(
    pool: web::Data<PgPool>,
    secret: web::Data<JwtSecret>,
    query: web::Query<GuestHistoryQuery>,
) -> Result<HttpResponse, AppError> {
    let phone = normalize_phone(&query.phone)?;

    // If a device_token is supplied, it must be valid for this phone.
    // For branches without OTP (otp_required = false), no token is expected —
    // we accept phone-only reads (addresses are non-sensitive).
    if let Some(token) = &query.device_token {
        if !token.is_empty() && !whatsapp::verify_device_token(&secret.0, &phone, token) {
            return Err(AppError::Unauthorized("Invalid device token.".into()));
        }
    }

    let rows: Vec<OrderHistoryRow> = sqlx::query_as(
        "SELECT d.id, d.delivery_ref, d.status::text AS status, d.channel::text AS channel,
                d.created_at, d.branch_id, b.name AS branch_name,
                d.subtotal, d.delivery_fee, d.discount_amount, d.total,
                d.address_line, d.place_name,
                d.customer_lat, d.customer_lng, d.customer_name, d.cart
         FROM delivery_orders d
         JOIN branches b ON b.id = d.branch_id
         WHERE d.customer_phone = $1 AND d.org_id = $2
         ORDER BY d.created_at DESC
         LIMIT 50",
    )
    .bind(&phone)
    .bind(query.org_id)
    .fetch_all(pool.get_ref())
    .await?;

    let summaries: Vec<OrderHistorySummary> = rows
        .into_iter()
        .map(|r| {
            // Extract a compact [{name, quantity}] list from the frozen cart JSONB.
            let items = r
                .cart
                .as_array()
                .map(|lines| {
                    lines
                        .iter()
                        .filter_map(|l| {
                            let name = l.get("name")?.as_str()?.to_string();
                            let quantity = l.get("quantity")?.as_i64().unwrap_or(1) as i32;
                            Some(serde_json::json!({ "name": name, "quantity": quantity }))
                        })
                        .collect::<serde_json::Value>()
                })
                .unwrap_or(serde_json::Value::Array(vec![]));
            OrderHistorySummary {
                id: r.id,
                delivery_ref: r.delivery_ref,
                status: r.status,
                channel: r.channel,
                created_at: r.created_at,
                branch_id: r.branch_id,
                branch_name: r.branch_name,
                subtotal: r.subtotal,
                delivery_fee: r.delivery_fee,
                discount_amount: r.discount_amount,
                total: r.total,
                address_line: r.address_line,
                place_name: r.place_name,
                customer_lat: r.customer_lat,
                customer_lng: r.customer_lng,
                customer_name: r.customer_name,
                items,
            }
        })
        .collect();

    Ok(HttpResponse::Ok().json(summaries))
}

// ── Guest past locations: distinct delivery addresses by phone + org ──

#[derive(Deserialize, IntoParams)]
pub struct GuestLocationsQuery {
    pub phone: String,
    pub org_id: Uuid,
    #[serde(default)]
    pub branch_id: Option<Uuid>,
    #[serde(default)]
    pub device_token: Option<String>,
}

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct GuestSavedLocation {
    pub branch_id: Uuid,
    pub channel: String,
    pub address_line: Option<String>,
    pub place_name: Option<String>,
    pub floor: Option<String>,
    pub unit_number: Option<String>,
    pub landmark: Option<String>,
    pub customer_lat: Option<f64>,
    pub customer_lng: Option<f64>,
    pub last_used_at: chrono::DateTime<chrono::Utc>,
}

#[utoipa::path(
    get, path = "/public/delivery-orders/past-locations", tag = "delivery-public",
    params(GuestLocationsQuery),
    responses((status = 200, body = [GuestSavedLocation]), AppErrorResponse)
)]
pub async fn guest_past_locations(
    pool: web::Data<PgPool>,
    secret: web::Data<JwtSecret>,
    query: web::Query<GuestLocationsQuery>,
) -> Result<HttpResponse, AppError> {
    let phone = normalize_phone(&query.phone)?;

    if let Some(token) = &query.device_token {
        if !token.is_empty() && !whatsapp::verify_device_token(&secret.0, &phone, token) {
            return Err(AppError::Unauthorized("Invalid device token.".into()));
        }
    }

    // DISTINCT ON (branch_id, channel, coalesced address key) ordered by most recent.
    // Derived from order history — no separate table needed.
    let rows: Vec<GuestSavedLocation> = if let Some(bid) = query.branch_id {
        sqlx::query_as(
            "SELECT DISTINCT ON (d.branch_id, d.channel::text,
                                  COALESCE(d.address_line, d.place_name, ''))
                    d.branch_id, d.channel::text AS channel,
                    d.address_line, d.place_name, d.floor, d.unit_number, d.landmark,
                    d.customer_lat, d.customer_lng, d.created_at AS last_used_at
             FROM delivery_orders d
             WHERE d.customer_phone = $1 AND d.org_id = $2 AND d.branch_id = $3
               AND (d.address_line IS NOT NULL OR d.place_name IS NOT NULL
                    OR d.customer_lat IS NOT NULL)
             ORDER BY d.branch_id, d.channel::text,
                      COALESCE(d.address_line, d.place_name, ''),
                      d.created_at DESC",
        )
        .bind(&phone)
        .bind(query.org_id)
        .bind(bid)
        .fetch_all(pool.get_ref())
        .await?
    } else {
        sqlx::query_as(
            "SELECT DISTINCT ON (d.branch_id, d.channel::text,
                                  COALESCE(d.address_line, d.place_name, ''))
                    d.branch_id, d.channel::text AS channel,
                    d.address_line, d.place_name, d.floor, d.unit_number, d.landmark,
                    d.customer_lat, d.customer_lng, d.created_at AS last_used_at
             FROM delivery_orders d
             WHERE d.customer_phone = $1 AND d.org_id = $2
               AND (d.address_line IS NOT NULL OR d.place_name IS NOT NULL
                    OR d.customer_lat IS NOT NULL)
             ORDER BY d.branch_id, d.channel::text,
                      COALESCE(d.address_line, d.place_name, ''),
                      d.created_at DESC",
        )
        .bind(&phone)
        .bind(query.org_id)
        .fetch_all(pool.get_ref())
        .await?
    };

    Ok(HttpResponse::Ok().json(rows))
}

// ── Public order tracking (unauthenticated, capability URL) ───

/// Customer-safe tracking view of a delivery order, keyed by its opaque UUID
/// (same capability-URL trust model as the device-token flow). No phone number
/// is exposed; the destination fields are the customer's own inputs. Powers the
/// public `/track/{id}` page (polled, since the public surface has no SSE).
#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct DeliveryTracking {
    pub id: Uuid,
    pub org_id: Uuid,
    pub branch_name: String,
    pub delivery_ref: Option<String>,
    pub channel: String,
    pub status: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub confirmed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub preparing_at: Option<chrono::DateTime<chrono::Utc>>,
    pub ready_at: Option<chrono::DateTime<chrono::Utc>>,
    pub out_for_delivery_at: Option<chrono::DateTime<chrono::Utc>>,
    pub delivered_at: Option<chrono::DateTime<chrono::Utc>>,
    pub cancelled_at: Option<chrono::DateTime<chrono::Utc>>,
    pub rejected_at: Option<chrono::DateTime<chrono::Utc>>,
    pub cancel_reason: Option<String>,
    /// Branch base prep time + the teller's per-order addition (minutes).
    pub estimated_prep_minutes: i32,
    pub subtotal: i32,
    pub delivery_fee: i32,
    pub discount_amount: i32,
    pub total: i32,
    pub payment_method_hint: Option<String>,
    pub customer_name: String,
    pub place_name: Option<String>,
    pub floor: Option<String>,
    pub unit_number: Option<String>,
    pub address_line: Option<String>,
}

#[utoipa::path(
    get, path = "/public/delivery-orders/{id}/track", tag = "delivery-public",
    responses((status = 200, body = DeliveryTracking), AppErrorResponse)
)]
pub async fn track_delivery_order(
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let id = path.into_inner();
    let row: Option<DeliveryTracking> = sqlx::query_as(
        "SELECT d.id, d.org_id, b.name AS branch_name, d.delivery_ref, \
                d.channel::text AS channel, d.status::text AS status, \
                d.created_at, d.confirmed_at, d.preparing_at, d.ready_at, \
                d.out_for_delivery_at, d.delivered_at, d.cancelled_at, d.rejected_at, \
                d.cancel_reason, \
                COALESCE(s.prep_time_minutes, 0) + d.extra_prep_minutes AS estimated_prep_minutes, \
                d.subtotal, d.delivery_fee, d.discount_amount, d.total, \
                d.payment_method_hint, d.customer_name, \
                d.place_name, d.floor, d.unit_number, d.address_line \
         FROM delivery_orders d \
         JOIN branches b ON b.id = d.branch_id \
         LEFT JOIN branch_delivery_settings s ON s.branch_id = d.branch_id \
         WHERE d.id = $1",
    )
    .bind(id)
    .fetch_optional(pool.get_ref())
    .await?;
    let tracking = row.ok_or_else(|| AppError::NotFound("Order not found".into()))?;
    Ok(HttpResponse::Ok().json(tracking))
}
