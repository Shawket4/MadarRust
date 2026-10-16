use actix_web::HttpMessage;
use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::{guards::require_same_org, jwt::Claims},
    errors::{AppError, AppErrorResponse},
    permissions::checker::check_permission,
};
use utoipa::{IntoParams, ToSchema};

// ── Image slot (Track B4) ─────────────────────────────────────

/// After a JSON write of `image_url`: `Some(None)` clears `image_group_id`;
/// `Some(Some(url))` different from before is staged into the asset pipeline.
pub(crate) async fn image_url_side_effects(
    pool: &PgPool,
    table: crate::assets::ingest::AssetTable,
    org_id: Uuid,
    id: Uuid,
    new: Option<&Option<String>>,
    old: Option<&str>,
    claims: &Claims,
) -> Result<(), AppError> {
    let tbl = match table {
        crate::assets::ingest::AssetTable::Categories => "categories",
        crate::assets::ingest::AssetTable::MenuItems => "menu_items",
        _ => return Ok(()),
    };
    match new {
        Some(None) => {
            sqlx::query(&format!(
                "UPDATE {tbl} SET image_group_id = NULL WHERE id = $1"
            ))
            .bind(id)
            .execute(pool)
            .await?;
        }
        Some(Some(url)) if Some(url.as_str()) != old && !url.is_empty() => {
            crate::uploads::handlers::stage_image_url(
                pool,
                org_id,
                table,
                id,
                url,
                claims.user_id_safe().ok(),
            )
            .await?;
        }
        _ => {}
    }
    Ok(())
}

async fn attach_item_refs(
    pool: &PgPool,
    org_id: Uuid,
    items: &mut [MenuItem],
) -> Result<(), AppError> {
    let ids: Vec<Uuid> = items.iter().map(|i| i.id).collect();
    let mut refs = crate::assets::refs::slot_refs(
        pool,
        org_id,
        crate::assets::ingest::AssetTable::MenuItems,
        crate::assets::ingest::AssetField::Image,
        &ids,
    )
    .await?;
    for i in items.iter_mut() {
        i.image = refs.remove(&i.id);
    }
    Ok(())
}

async fn attach_category_refs(
    pool: &PgPool,
    org_id: Uuid,
    rows: &mut [Category],
) -> Result<(), AppError> {
    let ids: Vec<Uuid> = rows.iter().map(|i| i.id).collect();
    let mut refs = crate::assets::refs::slot_refs(
        pool,
        org_id,
        crate::assets::ingest::AssetTable::Categories,
        crate::assets::ingest::AssetField::Image,
        &ids,
    )
    .await?;
    for i in rows.iter_mut() {
        i.image = refs.remove(&i.id);
    }
    Ok(())
}

// ── Models ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct Category {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    #[serde(serialize_with = "crate::uploads::handlers::serialize_opt_url")]
    pub image_url: Option<String>,
    /// Asset refs (Track B4, §11.10); null when no asset or not attached by this endpoint.
    #[sqlx(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<crate::assets::refs::AssetGroupRef>,
    pub is_active: bool,
    /// Drag-and-drop position (lower first); ties break on name. Set via
    /// `PUT /categories/order`.
    pub display_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ReorderCategoriesRequest {
    pub org_id: Uuid,
    /// Category IDs in the desired display order (first = top).
    pub ordered_ids: Vec<Uuid>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct MenuItem {
    pub id: Uuid,
    pub org_id: Uuid,
    pub category_id: Option<Uuid>,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub description: Option<String>,
    #[schema(value_type = Object)]
    pub description_translations: serde_json::Value,
    #[serde(serialize_with = "crate::uploads::handlers::serialize_opt_url")]
    pub image_url: Option<String>,
    /// Asset refs (Track B4, §11.10); null when no asset or not attached by this endpoint.
    #[sqlx(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<crate::assets::refs::AssetGroupRef>,
    pub base_price: i32,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub default_milk_addon_id: Option<String>,
    /// `item` | `combo` (combos module). A combo's price is its `one_size`
    /// row like any item; its slots are on `GET /combos/{id}`. Additive.
    #[sqlx(default)]
    #[serde(default = "crate::combos::types::item_kind")]
    pub kind: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct ItemSize {
    pub id: Uuid,
    pub menu_item_id: Uuid,
    pub label: String,
    pub price_override: i32,
    pub is_active: bool,
}

// An add-on as the old wire knows it: an option of a group that has a legacy
// type (`milk_type`, `coffee_type`, `extra`, …), which old tills require.
//
// Every reader of this shape filters `type IS NOT NULL`. Once the contract
// shim has run, `addon_items` is a view over `modifier_options` that also
// lists the options of a CUSTOM group (`legacy_addon_type` NULL, new clients
// only) with `type` NULL — the order path prices every option through it, so
// the view keeps them — and a NULL type decoded here was a 500 on every
// `/sync/pull` of the org's branches.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct AddonItem {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub addon_type: String,
    pub default_price: i32,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub primary_ingredient_id: Option<Uuid>,
    #[serde(default)]
    #[sqlx(skip)]
    pub ingredients: Vec<AddonItemIngredient>,
    /// How a sale line charges this option: madar-catalog's `OptionView`
    /// (branch-effective price, its group's effect and swap category, the
    /// ingredient it replaces, its lines per size). The till prices lines with
    /// it exactly as the order path does. Additive; older tills ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[sqlx(skip)]
    #[schema(value_type = Option<Object>)]
    pub pricing: Option<madar_catalog::OptionView>,
}

// ── Addon Slot models ─────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct AddonSlot {
    pub id: Uuid,
    pub menu_item_id: Uuid,
    pub addon_type: String,
    pub label: Option<String>,
    #[schema(value_type = Object)]
    pub label_translations: serde_json::Value,
    pub is_required: bool,
    pub min_selections: i32,
    pub max_selections: Option<i32>,
    pub created_at: DateTime<Utc>,
}

// ── Addon Override models ─────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct AddonOverride {
    pub id: Uuid,
    pub menu_item_id: Uuid,
    pub addon_item_id: Uuid,
    pub addon_item_name: String,
    pub size_label: Option<String>,
    pub ingredient_name: String,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_unit: String,
    #[schema(value_type = f64)]
    #[serde(serialize_with = "crate::decimals::serialize")]
    pub quantity_used: sqlx::types::BigDecimal,
    pub replaces_org_ingredient_id: Option<Uuid>,
    pub replaces_ingredient_name: Option<String>,
    pub combo_addon_item_id: Option<Uuid>,
    pub combo_addon_item_name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct MenuItemRecipe {
    pub org_ingredient_id: Option<Uuid>,
    #[schema(value_type = f64)]
    #[serde(serialize_with = "crate::decimals::serialize")]
    pub quantity_used: sqlx::types::BigDecimal,
    pub ingredient_name: String,
    pub ingredient_unit: String,
    pub category: String,
    pub size_label: String,
    /// The usable amount before yield loss: `quantity_used` × the linked
    /// ingredient's yield, 3 dp (madar-units `usable_qty`). What the catalog
    /// item dialog shows.
    #[sqlx(skip)]
    pub usable_quantity: f64,
    /// The linked ingredient's yield percentage, for `usable_quantity`.
    #[serde(skip)]
    pub yield_pct: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct AddonItemIngredient {
    pub org_ingredient_id: Option<Uuid>,
    #[schema(value_type = f64)]
    #[serde(serialize_with = "crate::decimals::serialize")]
    pub quantity_used: sqlx::types::BigDecimal,
    pub ingredient_name: String,
    pub ingredient_unit: String,
    /// The ingredient's category (additive, B12): lets the POS know an extra
    /// shot is a `coffee_bean` and follows the drink's chosen bean.
    #[serde(default)]
    pub category_id: Option<Uuid>,
    /// Slug of [`Self::category_id`] (`milk`, `coffee_bean`, `packaging`, …).
    #[serde(default)]
    pub category_slug: Option<String>,
}

// ── MenuItemFull — slots embedded instead of option_groups ────

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, ToSchema)]
pub struct MenuItemFull {
    #[serde(flatten)]
    pub item: MenuItem,
    /// LEGACY SHAPE — unchanged for clients at or below v0.7.11: the synthetic
    /// `one_size` row that now carries a single-price item's price is hidden
    /// here, so an old till still sees a size-less item exactly as it did.
    pub sizes: Vec<ItemSize>,
    /// Every size row, INCLUDING the synthetic `one_size` one. Additive: this is
    /// where price actually lives, and it is what the dashboard's size editor
    /// and new POS builds read. An item always has at least one entry.
    #[serde(default)]
    pub all_sizes: Vec<ItemSize>,
    pub addon_slots: Vec<AddonSlot>,
    pub optional_fields: Vec<OptionalField>,
    pub recipes: Vec<MenuItemRecipe>,
    /// How the item is made, in order. Each preset step carries its animation's
    /// address and fingerprint, so a device downloads only what its own menu
    /// uses and never the whole library.
    #[serde(default)]
    pub recipe_steps: Vec<crate::recipes::steps::RecipeStep>,
    /// Explicit per-item addon allowlist. Empty = no restriction (use org catalog).
    pub allowed_addon_ids: Vec<Uuid>,
    /// How a sale line of this item is priced at the requested branch:
    /// madar-catalog's `ItemView` (sizes with their branch prices, the
    /// branch's item price, the recipe's swap bases and their candidates, the
    /// optional fields). The till prices with it exactly as the order path
    /// does. Present on `?full=true` lists; additive, older tills ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub pricing: Option<madar_catalog::ItemView>,
    /// A kind=item row: its "make it a meal" upsell (C14), or `null`.
    #[serde(default)]
    pub meal: Option<crate::combos::types::MealLink>,
    /// A kind=combo row: its slots, windows and channel toggles for the
    /// requested branch; `null` for an item. Combo rows are served only to a
    /// client that can sell them (a browser, POS ≥ 0.9.0, the KDS).
    #[serde(default)]
    pub combo: Option<crate::combos::types::ComboFeed>,
}

// ── Request types ─────────────────────────────────────────────

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct OrgQuery {
    pub org_id: Uuid,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MenuItemQuery {
    pub org_id: Uuid,
    pub category_id: Option<Uuid>,
    /// When true, embed sizes + addon slots + optionals + recipes per item
    /// (the shape the POS/teller consumes). Always returns a plain, unpaginated
    /// array — the POS depends on this contract.
    pub full: Option<bool>,
    /// When set, prices are branch-effective (branch override replaces base_price)
    /// and items disabled at this branch are excluded — the per-branch menu the POS
    /// consumes. Omitted → the plain org catalog (legacy behaviour).
    pub branch_id: Option<Uuid>,
}

/// Query for the dashboard catalog endpoint: paginated menu items with embedded
/// per-SKU costs. Kept separate from [`MenuItemQuery`] so the plain `/menu-items`
/// list the POS relies on stays an unpaginated array.
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MenuCatalogQuery {
    pub org_id: Uuid,
    pub category_id: Option<Uuid>,
    /// Case-insensitive filter on the item name.
    pub search: Option<String>,
    /// 1-based page number (default 1).
    pub page: Option<i64>,
    /// Page size (default 50, max 500).
    pub per_page: Option<i64>,
    /// When set, enables the per-branch override filter/sort (LEFT JOINs the
    /// branch's overrides). Prices in the response stay org-level.
    pub branch_id: Option<Uuid>,
    /// With `branch_id`: true → only items overridden at the branch; false →
    /// only un-overridden; null → all.
    pub overridden: Option<bool>,
    /// `"overridden"` → overridden items first (needs `branch_id`); otherwise A–Z.
    pub sort: Option<String>,
    /// `false` → only items with NO recipe on any size: the onboarding
    /// worklist, everything that still deducts nothing and costs zero.
    /// `true` → only items that have one. Absent → all.
    pub has_recipe: Option<bool>,
}

// ── Branch menu overrides (per-branch price + availability layer) ─────────────
// The branch layer over the org catalog: absence of a row ⟹ inherit base_price and
// fully available. A row may set a branch price (price_override, piastres — null
// inherits) and/or disable the item at that branch (is_available=false). Applies to
// all channels including POS dine-in; a future channel layer (in-mall/outside) sits
// on top.

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct BranchMenuOverride {
    pub branch_id: Uuid,
    pub menu_item_id: Uuid,
    /// Branch price in piastres; null inherits the org catalog base_price.
    pub price_override: Option<i32>,
    /// False disables the item at this branch (excluded from the branch menu).
    pub is_available: bool,
    pub updated_at: DateTime<Utc>,
    /// Per-size branch prices for this item (empty when none). Availability is item-level.
    #[serde(default)]
    #[sqlx(skip)]
    pub sizes: Vec<BranchSizeOverride>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct BranchSizeOverride {
    pub size_label: String,
    /// Branch price for this size in piastres.
    pub price_override: i32,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BranchOverridesQuery {
    pub branch_id: Uuid,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BranchOverrideKeyQuery {
    pub branch_id: Uuid,
    pub menu_item_id: Uuid,
}

fn override_default_available() -> bool {
    true
}

#[derive(Deserialize, Serialize, Clone, ToSchema)]
pub struct BranchSizeOverrideInput {
    pub size_label: String,
    pub price_override: i32,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct BranchMenuOverrideInput {
    pub branch_id: Uuid,
    pub menu_item_id: Uuid,
    /// Branch price in piastres; null inherits the org catalog base_price.
    #[serde(default)]
    pub price_override: Option<i32>,
    #[serde(default = "override_default_available")]
    pub is_available: bool,
    /// Per-size branch prices. `null`/omitted → leave existing size overrides untouched;
    /// a list → REPLACE the item's size overrides with exactly that set (empty clears them).
    #[serde(default)]
    pub sizes: Option<Vec<BranchSizeOverrideInput>>,
}

// ── Branch addon overrides (per-branch addon price + availability) ────────────
// The addon analogue of BranchMenuOverride. No sizes (addons have none).

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct BranchAddonOverride {
    pub branch_id: Uuid,
    pub addon_item_id: Uuid,
    /// Branch price in piastres; null inherits the org default_price.
    pub price_override: Option<i32>,
    /// False disables the addon at this branch (excluded from the branch addon list).
    pub is_available: bool,
    pub updated_at: DateTime<Utc>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BranchAddonOverrideKeyQuery {
    pub branch_id: Uuid,
    pub addon_item_id: Uuid,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct BranchAddonOverrideInput {
    pub branch_id: Uuid,
    pub addon_item_id: Uuid,
    /// Branch price in piastres; null inherits the org default_price.
    #[serde(default)]
    pub price_override: Option<i32>,
    #[serde(default = "override_default_available")]
    pub is_available: bool,
}

/// A menu item with its per-SKU recipe-cost rollup embedded, so the catalog
/// list needs no separate `/costing/menu-items` round trip. `sku_costs` is one
/// row per sellable size (or a single `one_size` row); empty when the item is
/// inactive or has no recipe.
#[derive(Serialize, Deserialize, Clone, Debug, ToSchema)]
pub struct MenuItemWithCosts {
    #[serde(flatten)]
    pub item: MenuItem,
    pub sku_costs: Vec<crate::costing::SkuCost>,
}

#[derive(Serialize, Deserialize, Clone, Debug, ToSchema)]
pub struct PaginatedMenuItems {
    pub data: Vec<MenuItemWithCosts>,
    pub total: i64,
    pub page: i64,
    pub per_page: i64,
    pub total_pages: i64,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AddonItemQuery {
    pub org_id: Uuid,
    pub addon_type: Option<String>,
    /// When set, prices are branch-effective (override replaces default_price) and
    /// addons disabled at this branch are excluded — the per-branch addon list the
    /// POS consumes. Omitted → the plain org list (legacy behaviour).
    pub branch_id: Option<Uuid>,
    /// Case-insensitive filter on the addon name.
    pub search: Option<String>,
    /// Sending `page` or `per_page` switches the response to the paginated
    /// shape (`PaginatedAddonItems`); without either it stays the plain array
    /// the POS and old clients read.
    pub page: Option<i64>,
    pub per_page: Option<i64>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateCategoryRequest {
    pub org_id: Uuid,
    pub name: String,
    #[schema(value_type = Option<Object>)]
    pub name_translations: Option<serde_json::Value>,
    pub image_url: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateCategoryRequest {
    pub name: Option<String>,
    #[schema(value_type = Option<Object>)]
    pub name_translations: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub image_url: Option<Option<String>>,
    pub is_active: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateMenuItemRequest {
    pub org_id: Uuid,
    pub category_id: Uuid,
    pub name: String,
    #[schema(value_type = Option<Object>)]
    pub name_translations: Option<serde_json::Value>,
    pub description: Option<String>,
    #[schema(value_type = Option<Object>)]
    pub description_translations: Option<serde_json::Value>,
    pub image_url: Option<String>,
    pub base_price: i32,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateMenuItemRequest {
    pub category_id: Option<Uuid>,
    pub name: Option<String>,
    #[schema(value_type = Option<Object>)]
    pub name_translations: Option<serde_json::Value>,
    pub description: Option<String>,
    #[schema(value_type = Option<Object>)]
    pub description_translations: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub image_url: Option<Option<String>>,
    pub base_price: Option<i32>,
    pub is_active: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateAddonItemRequest {
    pub org_id: Uuid,
    pub name: String,
    #[schema(value_type = Option<Object>)]
    pub name_translations: Option<serde_json::Value>,
    pub addon_type: String,
    pub default_price: i32,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateAddonItemRequest {
    pub name: Option<String>,
    #[schema(value_type = Option<Object>)]
    pub name_translations: Option<serde_json::Value>,
    pub addon_type: Option<String>,
    pub default_price: Option<i32>,
    pub is_active: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpsertSizeRequest {
    pub label: String,
    pub price_override: i32,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateAddonSlotRequest {
    pub addon_type: Option<String>,
    pub label: Option<String>,
    #[schema(value_type = Option<Object>)]
    pub label_translations: Option<serde_json::Value>,
    pub is_required: Option<bool>,
    pub min_selections: Option<i32>,
    pub max_selections: Option<i32>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateAddonSlotRequest {
    pub label: Option<String>,
    #[schema(value_type = Option<Object>)]
    pub label_translations: Option<serde_json::Value>,
    pub is_required: Option<bool>,
    pub min_selections: Option<i32>,
    pub max_selections: Option<i32>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpsertAddonOverrideRequest {
    pub addon_item_id: Uuid,
    pub size_label: Option<String>,
    pub ingredient_name: String,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_unit: String,
    pub quantity_used: f64,
    pub replaces_org_ingredient_id: Option<Uuid>,
    pub combo_addon_item_id: Option<Uuid>,
}

// ── Categories ────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/categories",
    tag = "menu",
    params(OrgQuery),
    responses((status = 200, description = "List categories", body = Vec<Category>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_categories(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<OrgQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "categories", "read").await?;
    require_same_org(&claims, Some(query.org_id))?;

    let rows = sqlx::query_as::<_, Category>(
        "SELECT id, org_id, name, name_translations, image_url, is_active, display_order,
                created_at, updated_at, deleted_at
         FROM categories
         WHERE org_id = $1 AND deleted_at IS NULL
         ORDER BY display_order ASC, name ASC",
    )
    .bind(query.org_id)
    .fetch_all(pool.get_ref())
    .await?;

    let mut rows = rows;
    attach_category_refs(pool.get_ref(), query.org_id, &mut rows).await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    post,
    path = "/categories",
    tag = "menu",
    request_body = CreateCategoryRequest,
    responses((status = 201, description = "Category created", body = Category), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_category(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateCategoryRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "categories", "create").await?;
    require_same_org(&claims, Some(body.org_id))?;

    let mut_body = body.into_inner();
    let mut name_translations = mut_body
        .name_translations
        .unwrap_or_else(|| serde_json::json!({}));
    crate::translation::ensure_translations_json(&mut name_translations, Some(&mut_body.name))
        .await
        .map_err(|_| AppError::Internal)?;

    let row = sqlx::query_as::<_, Category>(
        "INSERT INTO categories (org_id, name, name_translations, image_url, display_order)
         VALUES ($1, $2, $3, $4,
                 COALESCE((SELECT max(display_order) + 1 FROM categories
                           WHERE org_id = $1 AND deleted_at IS NULL), 0))
         RETURNING id, org_id, name, name_translations, image_url, is_active, display_order,
                   created_at, updated_at, deleted_at",
    )
    .bind(mut_body.org_id)
    .bind(&mut_body.name)
    .bind(name_translations)
    .bind(&mut_body.image_url)
    .fetch_one(pool.get_ref())
    .await?;

    if let Some(url) = mut_body.image_url.clone() {
        image_url_side_effects(
            pool.get_ref(),
            crate::assets::ingest::AssetTable::Categories,
            row.org_id,
            row.id,
            Some(&Some(url)),
            None,
            &claims,
        )
        .await?;
    }

    let mut row = row;
    attach_category_refs(pool.get_ref(), row.org_id, std::slice::from_mut(&mut row)).await?;
    Ok(HttpResponse::Created().json(row))
}

#[utoipa::path(
    patch,
    path = "/categories/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Category ID")),
    request_body = UpdateCategoryRequest,
    responses((status = 200, description = "Category updated", body = Category), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_category(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<UpdateCategoryRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "categories", "update").await?;

    let existing = fetch_category(pool.get_ref(), *id).await?;
    require_same_org(&claims, Some(existing.org_id))?;

    let mut_body = body.into_inner();
    let image_url_is_present = mut_body.image_url.is_some();
    let image_url_val = mut_body.image_url.as_ref().and_then(|o| o.clone());

    let mut name_translations = existing.name_translations;
    if let Some(new_name) = &mut_body.name {
        crate::translation::ensure_translations_json(&mut name_translations, Some(new_name))
            .await
            .map_err(|_| AppError::Internal)?;
    } else if let Some(new_tr) = mut_body.name_translations {
        name_translations = new_tr;
        crate::translation::ensure_translations_json(&mut name_translations, Some(&existing.name))
            .await
            .map_err(|_| AppError::Internal)?;
    }

    let row = sqlx::query_as::<_, Category>(
        "UPDATE categories SET
             name              = COALESCE($2, name),
             name_translations = $3,
             image_url         = CASE WHEN $6 THEN $4 ELSE image_url END,
             is_active         = COALESCE($5, is_active)
         WHERE id = $1 AND deleted_at IS NULL
         RETURNING id, org_id, name, name_translations, image_url, is_active, display_order,
                   created_at, updated_at, deleted_at",
    )
    .bind(*id)
    .bind(&mut_body.name)
    .bind(name_translations)
    .bind(image_url_val)
    .bind(mut_body.is_active)
    .bind(image_url_is_present)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Category not found".into()))?;

    // Image slot (§11.5 W3): null clears the asset reference (no file delete —
    // assets are content-addressed); a new URL is routed through the asset
    // pipeline.
    image_url_side_effects(
        pool.get_ref(),
        crate::assets::ingest::AssetTable::Categories,
        existing.org_id,
        *id,
        mut_body.image_url.as_ref(),
        existing.image_url.as_deref(),
        &claims,
    )
    .await?;

    let mut row = row;
    attach_category_refs(
        pool.get_ref(),
        existing.org_id,
        std::slice::from_mut(&mut row),
    )
    .await?;
    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    delete,
    path = "/categories/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Category ID")),
    responses((status = 204, description = "Category deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_category(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "categories", "delete").await?;

    let existing = fetch_category(pool.get_ref(), *id).await?;
    require_same_org(&claims, Some(existing.org_id))?;

    sqlx::query("UPDATE categories SET deleted_at = NOW() WHERE id = $1 AND deleted_at IS NULL")
        .bind(*id)
        .execute(pool.get_ref())
        .await?;

    Ok(HttpResponse::NoContent().finish())
}

#[utoipa::path(
    put,
    path = "/categories/order",
    tag = "menu",
    request_body = ReorderCategoriesRequest,
    responses((status = 200, description = "Categories reordered", body = Vec<Category>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn reorder_categories(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<ReorderCategoriesRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "categories", "update").await?;
    require_same_org(&claims, Some(body.org_id))?;

    let body = body.into_inner();
    let mut tx = pool.get_ref().begin().await?;

    // Lock and validate: every id must exist, belong to this org, and not be
    // deleted — and every live category for the org must be present exactly
    // once, so a stale client can't silently drop or duplicate one.
    let live_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM categories WHERE org_id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(body.org_id)
    .fetch_all(&mut *tx)
    .await?;

    let mut requested: Vec<Uuid> = body.ordered_ids.clone();
    requested.sort();
    let mut expected = live_ids.clone();
    expected.sort();
    if requested != expected {
        return Err(AppError::BadRequest(
            "ordered_ids must contain exactly the org's live categories".into(),
        ));
    }

    for (idx, cat_id) in body.ordered_ids.iter().enumerate() {
        sqlx::query(
            "UPDATE categories SET display_order = $2 WHERE id = $1 AND org_id = $3 AND deleted_at IS NULL",
        )
        .bind(cat_id)
        .bind(idx as i32)
        .bind(body.org_id)
        .execute(&mut *tx)
        .await?;
    }

    let rows = sqlx::query_as::<_, Category>(
        "SELECT id, org_id, name, name_translations, image_url, is_active, display_order,
                created_at, updated_at, deleted_at
         FROM categories
         WHERE org_id = $1 AND deleted_at IS NULL
         ORDER BY display_order ASC, name ASC",
    )
    .bind(body.org_id)
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;

    let mut rows = rows;
    attach_category_refs(pool.get_ref(), body.org_id, &mut rows).await?;
    Ok(HttpResponse::Ok().json(rows))
}

// ── Menu Items ────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/menu-items",
    tag = "menu",
    operation_id = "list_menu_items",
    params(MenuItemQuery),
    responses((status = 200, description = "List menu items as a plain array; ?full=true embeds sizes/addons/optionals/recipes (POS contract)", body = Vec<MenuItem>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_menu_items(
    req: HttpRequest,
    pool: crate::db::Db,
    cache: Option<web::Data<crate::menu::cache::MenuCache>>,
    query: web::Query<MenuItemQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    require_same_org(&claims, Some(query.org_id))?;

    // Serve from the per-org menu cache when enabled (MENU_CACHE_TTL_SECS>0). The
    // variant folds in every param that changes the body so views never alias.
    // Disabled / unregistered (every test) → `cache` is None and we hit the DB.
    // Combos (COMBOS_CONTRACT.md §2.4): a till older than POS 0.9.0 builds its
    // menu from this endpoint and cannot sell a combo, so `kind=combo` rows are
    // left out for it. Part of the cache key: two clients, two bodies.
    let sells_combos = crate::client_seen::sells_combos(req.headers());
    // Everything the body depends on besides the query is in the key
    // (`menu::cache::fingerprint`; the full list is in cache.rs), so the next
    // read after any write is fresh. Off on a non-prod-shaped database.
    let fingerprint = match &cache {
        Some(c) if c.usable(pool.get_ref()).await => {
            Some(crate::menu::cache::fingerprint(pool.get_ref(), query.org_id).await?)
        }
        _ => None,
    };
    let cache = cache.filter(|_| fingerprint.is_some());
    let variant = format!(
        "menu|{}|{}|{}|{}|{}",
        fingerprint.as_deref().unwrap_or(""),
        query.category_id.map(|c| c.to_string()).unwrap_or_default(),
        query.branch_id.map(|b| b.to_string()).unwrap_or_default(),
        query.full.unwrap_or(false),
        sells_combos,
    );
    if let Some(c) = &cache
        && let Some(body) = c.get(query.org_id, &variant).await
    {
        return Ok(HttpResponse::Ok()
            .content_type("application/json")
            .body(body));
    }

    // When branch_id is supplied, prices are branch-effective (override replaces
    // base_price) and branch-disabled items are excluded — the per-branch menu the POS
    // consumes. When null the LEFT JOIN matches nothing (NULL branch_id) so this is the
    // plain org catalog, preserving the legacy contract.
    let mut items = sqlx::query_as::<_, MenuItem>(
        "SELECT mi.id, mi.org_id, mi.category_id, mi.name, mi.name_translations,
                mi.description, mi.description_translations, mi.image_url,
                COALESCE(bmo.price_override, mi.base_price) AS base_price,
                mi.is_active,
                mi.created_at, mi.updated_at, mi.deleted_at, mi.kind,
                (
                    SELECT a.id::text
                    FROM compat.menu_item_recipes r
                    JOIN compat.addon_item_ingredients ai ON ai.org_ingredient_id = r.org_ingredient_id
                    JOIN compat.addon_items a ON a.id = ai.addon_item_id
                    WHERE r.menu_item_id = mi.id
                      AND a.type = 'milk_type'
                    -- Deterministic: the group's display order (component_resolve agrees).
                    ORDER BY (SELECT mo.sort FROM modifier_options mo WHERE mo.id = a.id) NULLS LAST, a.name, a.id
                    LIMIT 1
                ) AS default_milk_addon_id
         FROM menu_items mi
         LEFT JOIN compat.branch_menu_overrides bmo
                ON bmo.menu_item_id = mi.id AND bmo.branch_id = $3
         WHERE mi.org_id = $1 AND mi.deleted_at IS NULL
           AND ($2::uuid IS NULL OR mi.category_id = $2)
           AND ($3::uuid IS NULL OR COALESCE(bmo.is_available, true) = true)
           AND ($4 OR mi.kind <> 'combo')
         ORDER BY mi.name ASC",
    )
    .bind(query.org_id)
    .bind(query.category_id)
    .bind(query.branch_id)
    .bind(sells_combos)
    .fetch_all(pool.get_ref())
    .await?;

    // ?full=true embeds sizes + addon_slots + optionals + recipes + steps per item.
    if query.full.unwrap_or(false) {
        // Steps for the whole org in ONE query, then handed out per item: this
        // endpoint is what the POS syncs, and a query per item would grow with
        // the menu.
        let mut steps_by_item =
            crate::recipes::steps::fetch_org_steps(pool.get_ref(), query.org_id).await?;
        let mut result: Vec<MenuItemFull> = vec![];
        attach_item_refs(pool.get_ref(), query.org_id, &mut items).await?;
        // Every item's pricing view in one batched load (the order path's).
        let mut pricing = crate::orders::catalog_view::Catalog::new(query.branch_id);
        let ids: Vec<Uuid> = items.iter().map(|i| i.id).collect();
        pricing.ensure_on(pool.get_ref(), &ids, &[]).await?;
        // Combos: the `combo` object of every combo row and the "make it a
        // meal" link of every item row (additive; old tills ignore both).
        let (mut combo_feeds, meals) = {
            let mut conn = pool.get_ref().acquire().await?;
            let combo_ids: Vec<Uuid> = items
                .iter()
                .filter(|i| i.kind == "combo")
                .map(|i| i.id)
                .collect();
            let feeds = crate::combos::load::feeds_for(
                &mut conn,
                query.org_id,
                query.branch_id,
                &combo_ids,
            )
            .await?;
            let meals = if sells_combos {
                crate::combos::load::meal_links(&mut conn, query.org_id).await?
            } else {
                Default::default()
            };
            (feeds, meals)
        };
        for item in items {
            let mut sizes = fetch_sizes(pool.get_ref(), item.id).await?;
            let mut all_sizes = fetch_all_sizes(pool.get_ref(), item.id).await?;
            // Branch menu (branch_id set): overlay this branch's per-size price overrides
            // so the POS sees branch-effective size prices, not just the catalog ones.
            if let Some(branch_id) = query.branch_id {
                apply_branch_size_overrides(pool.get_ref(), branch_id, item.id, &mut sizes).await?;
                apply_branch_size_overrides(pool.get_ref(), branch_id, item.id, &mut all_sizes)
                    .await?;
            }
            let addon_slots = fetch_addon_slots(pool.get_ref(), item.id).await?;
            let optional_fields = fetch_optional_fields(pool.get_ref(), item.id).await?;
            let recipes = fetch_item_recipes(pool.get_ref(), item.id).await?;
            let allowed_addon_ids = fetch_allowed_addon_ids(pool.get_ref(), item.id).await?;
            let recipe_steps = steps_by_item.remove(&item.id).unwrap_or_default();
            let item_pricing = pricing.item(item.id).map(|i| i.view.clone());
            let meal = meals.get(&item.id).copied();
            let combo = combo_feeds.remove(&item.id);
            result.push(MenuItemFull {
                item,
                sizes,
                all_sizes,
                addon_slots,
                optional_fields,
                recipes,
                recipe_steps,
                allowed_addon_ids,
                pricing: item_pricing,
                meal,
                combo,
            });
        }
        let body = web::Bytes::from(serde_json::to_vec(&result).map_err(|_| AppError::Internal)?);
        if let Some(c) = &cache {
            c.put(query.org_id, &variant, body.clone()).await;
        }
        return Ok(HttpResponse::Ok()
            .content_type("application/json")
            .body(body));
    }

    attach_item_refs(pool.get_ref(), query.org_id, &mut items).await?;
    let body = web::Bytes::from(serde_json::to_vec(&items).map_err(|_| AppError::Internal)?);
    if let Some(c) = &cache {
        c.put(query.org_id, &variant, body.clone()).await;
    }
    Ok(HttpResponse::Ok()
        .content_type("application/json")
        .body(body))
}

#[utoipa::path(
    get,
    path = "/costing/catalog",
    tag = "menu",
    operation_id = "list_menu_catalog",
    params(MenuCatalogQuery),
    responses((status = 200, description = "Dashboard catalog: paginated menu items with embedded per-SKU costs", body = PaginatedMenuItems), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_menu_catalog(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<MenuCatalogQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    require_same_org(&claims, Some(query.org_id))?;
    // Filtering by whether an item has a recipe reads the recipes: it needs
    // recipes.read on top of the menu read (architecture E).
    if query.has_recipe.is_some() {
        crate::authz::require::require(
            pool.get_ref(),
            &claims,
            crate::authz::Cap::RecipesRead,
            query.branch_id,
        )
        .await?;
    }

    let page = query.page.unwrap_or(1).max(1);
    let per_page = query.per_page.unwrap_or(50).clamp(1, 500);
    let offset = (page - 1) * per_page;
    let search = query
        .search
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // Branch-override filter/sort: LEFT JOIN the branch's overrides ($4); a row's
    // existence means "overridden at this branch". `overridden` ($5) filters on that;
    // sort='overridden' brings overridden items first. With no branch_id the join
    // matches nothing → identical to the plain org catalog.
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM menu_items mi
         LEFT JOIN compat.branch_menu_overrides bmo ON bmo.menu_item_id = mi.id AND bmo.branch_id = $4
         WHERE mi.org_id = $1 AND mi.deleted_at IS NULL
           AND ($2::uuid IS NULL OR mi.category_id = $2)
           AND ($3::text IS NULL OR mi.name ILIKE '%' || $3 || '%')
           AND ($5::bool IS NULL OR (bmo.branch_id IS NOT NULL) = $5)
           AND ($6::bool IS NULL OR EXISTS (
                 SELECT 1 FROM menu_item_sizes sz
                 JOIN recipe_lines rl ON rl.owner_type = 'item_size' AND rl.owner_id = sz.id
                 WHERE sz.menu_item_id = mi.id) = $6)",
    )
    .bind(query.org_id)
    .bind(query.category_id)
    .bind(search.as_deref())
    .bind(query.branch_id)
    .bind(query.overridden)
    .bind(query.has_recipe)
    .fetch_one(pool.get_ref())
    .await?;

    let order_by = match query.sort.as_deref() {
        Some("overridden") => "(bmo.branch_id IS NOT NULL) DESC, mi.name ASC",
        _ => "mi.name ASC",
    };
    let mut rows = sqlx::query_as::<_, MenuItem>(&format!(
        "SELECT mi.id, mi.org_id, mi.category_id, mi.name, mi.name_translations,
                mi.description, mi.description_translations, mi.image_url,
                mi.base_price, mi.is_active,
                mi.created_at, mi.updated_at, mi.deleted_at, mi.kind,
                (
                    SELECT a.id::text
                    FROM compat.menu_item_recipes r
                    JOIN compat.addon_item_ingredients ai ON ai.org_ingredient_id = r.org_ingredient_id
                    JOIN compat.addon_items a ON a.id = ai.addon_item_id
                    WHERE r.menu_item_id = mi.id
                      AND a.type = 'milk_type'
                    -- Deterministic: the group's display order (component_resolve agrees).
                    ORDER BY (SELECT mo.sort FROM modifier_options mo WHERE mo.id = a.id) NULLS LAST, a.name, a.id
                    LIMIT 1
                ) AS default_milk_addon_id
         FROM menu_items mi
         LEFT JOIN compat.branch_menu_overrides bmo ON bmo.menu_item_id = mi.id AND bmo.branch_id = $4
         WHERE mi.org_id = $1 AND mi.deleted_at IS NULL
           AND ($2::uuid IS NULL OR mi.category_id = $2)
           AND ($3::text IS NULL OR mi.name ILIKE '%' || $3 || '%')
           AND ($5::bool IS NULL OR (bmo.branch_id IS NOT NULL) = $5)
           AND ($8::bool IS NULL OR EXISTS (
                 SELECT 1 FROM menu_item_sizes sz
                 JOIN recipe_lines rl ON rl.owner_type = 'item_size' AND rl.owner_id = sz.id
                 WHERE sz.menu_item_id = mi.id) = $8)
         ORDER BY {order_by}
         LIMIT $6 OFFSET $7",
    ))
    .bind(query.org_id)
    .bind(query.category_id)
    .bind(search.as_deref())
    .bind(query.branch_id)
    .bind(query.overridden)
    .bind(per_page)
    .bind(offset)
    .bind(query.has_recipe)
    .fetch_all(pool.get_ref())
    .await?;

    let total_pages = if total == 0 {
        0
    } else {
        ((total as f64) / (per_page as f64)).ceil() as i64
    };

    // Same `image` asset refs as GET /menu-items/{id}.
    attach_item_refs(pool.get_ref(), query.org_id, &mut rows).await?;

    // Embed per-SKU costs for just this page so the dashboard catalog renders
    // food-cost chips in the same round trip.
    let ids: Vec<Uuid> = rows.iter().map(|m| m.id).collect();
    let costs =
        crate::costing::sku_costs_for_items(pool.get_ref(), query.org_id, &ids, query.branch_id)
            .await?;
    let mut by_item: std::collections::HashMap<Uuid, Vec<crate::costing::SkuCost>> =
        std::collections::HashMap::new();
    for c in costs {
        by_item.entry(c.menu_item_id).or_default().push(c);
    }
    let data: Vec<MenuItemWithCosts> = rows
        .into_iter()
        .map(|item| {
            let sku_costs = by_item.remove(&item.id).unwrap_or_default();
            MenuItemWithCosts { item, sku_costs }
        })
        .collect();

    Ok(HttpResponse::Ok().json(PaginatedMenuItems {
        data,
        total,
        page,
        per_page,
        total_pages,
    }))
}

#[utoipa::path(
    get,
    path = "/menu-items/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item ID")),
    responses((status = 200, description = "Get menu item", body = MenuItemFull), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_menu_item(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;

    let mut item = fetch_menu_item(pool.get_ref(), *id).await?;
    require_same_org(&claims, Some(item.org_id))?;
    attach_item_refs(pool.get_ref(), item.org_id, std::slice::from_mut(&mut item)).await?;

    let sizes = fetch_sizes(pool.get_ref(), *id).await?;
    let all_sizes = fetch_all_sizes(pool.get_ref(), *id).await?;
    let addon_slots = fetch_addon_slots(pool.get_ref(), *id).await?;
    let optional_fields = fetch_optional_fields(pool.get_ref(), *id).await?;
    let recipes = fetch_item_recipes(pool.get_ref(), *id).await?;
    let allowed_addon_ids = fetch_allowed_addon_ids(pool.get_ref(), *id).await?;
    let recipe_steps = crate::recipes::steps::fetch_item_steps(pool.get_ref(), *id).await?;
    // Its "make it a meal" link (C14), as the list and the feed carry it, so
    // the studio shows what is saved; only while the combo is still live.
    let meal = sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT mi.meal_combo_id, mi.meal_slot_id FROM menu_items mi \
           JOIN menu_items c ON c.id = mi.meal_combo_id AND c.kind = 'combo' AND c.deleted_at IS NULL \
          WHERE mi.id = $1 AND mi.meal_slot_id IS NOT NULL",
    )
    .bind(*id)
    .fetch_optional(pool.get_ref())
    .await?
    .map(|(combo_id, slot_id)| crate::combos::types::MealLink { combo_id, slot_id });

    Ok(HttpResponse::Ok().json(MenuItemFull {
        item,
        sizes,
        all_sizes,
        addon_slots,
        optional_fields,
        recipes,
        recipe_steps,
        allowed_addon_ids,
        pricing: None,
        meal,
        combo: None,
    }))
}

#[utoipa::path(
    post,
    path = "/menu-items",
    tag = "menu",
    request_body = CreateMenuItemRequest,
    responses((status = 201, description = "Menu item created", body = MenuItemFull), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_menu_item(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateMenuItemRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "create").await?;
    require_same_org(&claims, Some(body.org_id))?;

    let mut_body = body.into_inner();
    let mut name_translations = mut_body
        .name_translations
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));
    crate::translation::ensure_translations_json(&mut name_translations, Some(&mut_body.name))
        .await
        .map_err(|_| AppError::Internal)?;

    let mut description_translations = mut_body
        .description_translations
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(desc) = &mut_body.description {
        crate::translation::ensure_translations_json(&mut description_translations, Some(desc))
            .await
            .map_err(|_| AppError::Internal)?;
    }

    let mut tx = pool.get_ref().begin().await?;

    let item = sqlx::query_as::<_, MenuItem>(
        "INSERT INTO menu_items
             (org_id, category_id, name, name_translations, description, description_translations, image_url, base_price)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING id, org_id, category_id, name, name_translations, description, description_translations, image_url,
                   base_price, is_active,
                   created_at, updated_at, deleted_at, kind,
                   NULL::text AS default_milk_addon_id",
    )
    .bind(mut_body.org_id)
    .bind(mut_body.category_id)
    .bind(&mut_body.name)
    .bind(name_translations)
    .bind(&mut_body.description)
    .bind(description_translations)
    .bind(&mut_body.image_url)
    .bind(mut_body.base_price)
    .fetch_one(&mut *tx)
    .await?;

    // Seed initial price epoch for the advisor.
    sqlx::query(
        "INSERT INTO menu_item_price_epochs \
             (menu_item_id, size_label, price, effective_from, changed_by) \
         VALUES ($1, NULL, $2, now(), $3)",
    )
    .bind(item.id)
    .bind(mut_body.base_price)
    .bind(claims.user_id())
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    if let Some(url) = mut_body.image_url.clone() {
        image_url_side_effects(
            pool.get_ref(),
            crate::assets::ingest::AssetTable::MenuItems,
            item.org_id,
            item.id,
            Some(&Some(url)),
            None,
            &claims,
        )
        .await?;
    }

    let mut item = item;
    attach_item_refs(pool.get_ref(), item.org_id, std::slice::from_mut(&mut item)).await?;
    // The item was born with a `one_size` row carrying the price it was created
    // with (schema trigger), so it is never price-less. `sizes` stays empty —
    // the synthetic row is hidden from the legacy projection on purpose.
    let all_sizes = fetch_all_sizes(pool.get_ref(), item.id).await?;
    Ok(HttpResponse::Created().json(MenuItemFull {
        item,
        sizes: vec![],
        all_sizes,
        addon_slots: vec![],
        optional_fields: vec![],
        recipes: vec![],
        recipe_steps: vec![],
        allowed_addon_ids: vec![],
        pricing: None,
        meal: None,
        combo: None,
    }))
}

#[utoipa::path(
    patch,
    path = "/menu-items/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item ID")),
    request_body = UpdateMenuItemRequest,
    responses((status = 200, description = "Menu item updated", body = MenuItem), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_menu_item(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<UpdateMenuItemRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let existing = fetch_menu_item(pool.get_ref(), *id).await?;
    require_same_org(&claims, Some(existing.org_id))?;

    let mut_body = body.into_inner();
    let image_url_is_present = mut_body.image_url.is_some();
    let image_url_val = mut_body.image_url.as_ref().and_then(|o| o.clone());

    // The plain `name` column is the source of truth for the base ("en") locale.
    // Overlay any client-supplied translations first (so an Arabic edit sent in the
    // same request as a rename — the dashboard always sends `name` — isn't dropped),
    // then sync "en" to the effective base name and backfill missing languages.
    let mut name_translations = existing.name_translations;
    if let Some(new_tr) = &mut_body.name_translations {
        crate::translation::merge_translations_json(&mut name_translations, new_tr);
    }
    let base_name = mut_body.name.as_deref().unwrap_or(existing.name.as_str());
    crate::translation::ensure_translations_json(&mut name_translations, Some(base_name))
        .await
        .map_err(|_| AppError::Internal)?;

    let mut description_translations = existing.description_translations;
    if let Some(new_tr) = &mut_body.description_translations {
        crate::translation::merge_translations_json(&mut description_translations, new_tr);
    }
    if let Some(desc) = mut_body
        .description
        .as_deref()
        .or(existing.description.as_deref())
    {
        crate::translation::ensure_translations_json(&mut description_translations, Some(desc))
            .await
            .map_err(|_| AppError::Internal)?;
    }

    let mut tx = pool.get_ref().begin().await?;

    let item = sqlx::query_as::<_, MenuItem>(
        "UPDATE menu_items SET
             category_id              = COALESCE($2, category_id),
             name                     = COALESCE($3, name),
             name_translations        = $4,
             description              = COALESCE($5, description),
             description_translations = $6,
             image_url                = CASE WHEN $10 THEN $7 ELSE image_url END,
             base_price               = COALESCE($8, base_price),
             is_active                = COALESCE($9, is_active)
         WHERE id = $1 AND deleted_at IS NULL
         RETURNING id, org_id, category_id, name, name_translations, description, description_translations, image_url,
                   base_price, is_active,
                   created_at, updated_at, deleted_at, kind,
                   (
                       SELECT a.id::text
                       FROM compat.menu_item_recipes r
                       JOIN compat.addon_item_ingredients ai ON ai.org_ingredient_id = r.org_ingredient_id
                       JOIN compat.addon_items a ON a.id = ai.addon_item_id
                       WHERE r.menu_item_id = menu_items.id
                         AND a.type = 'milk_type'
                       -- Deterministic: the group's display order (component_resolve agrees).
                       ORDER BY (SELECT mo.sort FROM modifier_options mo WHERE mo.id = a.id) NULLS LAST, a.name, a.id
                       LIMIT 1
                   ) AS default_milk_addon_id",
    )
    .bind(*id)
    .bind(mut_body.category_id)
    .bind(&mut_body.name)
    .bind(name_translations)
    .bind(&mut_body.description)
    .bind(description_translations)
    .bind(image_url_val)
    .bind(mut_body.base_price)
    .bind(mut_body.is_active)
    .bind(image_url_is_present)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound("Menu item not found".into()))?;

    // `base_price` is a mirror, not truth. A caller that still sends one — a
    // legacy client, a CSV import — is expressing "this item costs X", which is
    // only meaningful for a single-size item; push it into that one size row so
    // the charged price actually moves. A multi-size item ignores it: its price
    // lives in its sizes and its displayed number is the lowest of them.
    // (The schema trigger then re-mirrors base_price from the sizes.)
    if let Some(new_price) = mut_body.base_price {
        sqlx::query(
            "UPDATE menu_item_sizes z SET price = $2
              WHERE z.menu_item_id = $1 AND z.is_active
                AND (SELECT count(*) FROM menu_item_sizes s
                      WHERE s.menu_item_id = $1 AND s.is_active) = 1",
        )
        .bind(*id)
        .bind(new_price)
        .execute(&mut *tx)
        .await?;
    }

    // Maintain price epoch whenever base_price actually changed.
    if let Some(new_price) = mut_body.base_price
        && new_price != existing.base_price
    {
        sqlx::query(
            "UPDATE menu_item_price_epochs \
                 SET effective_until = now() \
                 WHERE menu_item_id = $1 AND size_label IS NULL AND effective_until IS NULL",
        )
        .bind(*id)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO menu_item_price_epochs \
                     (menu_item_id, size_label, price, effective_from, changed_by) \
                 VALUES ($1, NULL, $2, now(), $3)",
        )
        .bind(*id)
        .bind(new_price)
        .bind(claims.user_id())
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    // Image slot (§11.5 W4).
    image_url_side_effects(
        pool.get_ref(),
        crate::assets::ingest::AssetTable::MenuItems,
        existing.org_id,
        *id,
        mut_body.image_url.as_ref(),
        existing.image_url.as_deref(),
        &claims,
    )
    .await?;

    let mut item = item;
    attach_item_refs(
        pool.get_ref(),
        existing.org_id,
        std::slice::from_mut(&mut item),
    )
    .await?;
    Ok(HttpResponse::Ok().json(item))
}

#[utoipa::path(
    delete,
    path = "/menu-items/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item ID")),
    responses((status = 204, description = "Menu item deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_menu_item(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "delete").await?;

    let mut tx = pool.get_ref().begin().await?;

    // Close base-price epoch AND every size epoch in one shot.
    sqlx::query(
        "UPDATE menu_item_price_epochs \
         SET effective_until = now() \
         WHERE menu_item_id = $1 AND effective_until IS NULL",
    )
    .bind(*id)
    .execute(&mut *tx)
    .await?;

    sqlx::query("UPDATE menu_items SET deleted_at = NOW() WHERE id = $1 AND deleted_at IS NULL")
        .bind(*id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(HttpResponse::NoContent().finish())
}

// ── Sizes ─────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/menu-items/{id}/sizes",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item ID")),
    request_body = UpsertSizeRequest,
    responses((status = 200, description = "Size upserted", body = ItemSize), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn upsert_size(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<UpsertSizeRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let item = fetch_menu_item(pool.get_ref(), *id).await?;
    require_same_org(&claims, Some(item.org_id))?;

    // Capture old price (if exists) before the upsert.
    let old_price: Option<i32> = sqlx::query_scalar(
        "SELECT price FROM menu_item_sizes \
         WHERE menu_item_id = $1 AND label = $2",
    )
    .bind(*id)
    .bind(&body.label)
    .fetch_optional(pool.get_ref())
    .await?;

    let mut tx = pool.get_ref().begin().await?;

    let row = sqlx::query_as::<_, ItemSize>(
        "INSERT INTO menu_item_sizes (menu_item_id, label, price)
         VALUES ($1, $2, $3)
         ON CONFLICT (menu_item_id, label) DO UPDATE SET
             price     = EXCLUDED.price,
             is_active = TRUE
         RETURNING id, menu_item_id, label::text, price AS price_override, is_active",
    )
    .bind(*id)
    .bind(&body.label)
    .bind(body.price_override)
    .fetch_one(&mut *tx)
    .await?;

    // Giving a simple item its first REAL size retires the `one_size` row it
    // was born with — the sentinel is not a size anyone picks, and leaving it
    // would show the item as multi-size with a phantom cheapest size. Only the
    // synthetic row, and only while nothing has been authored against it.
    if body.label != "one_size" {
        sqlx::query(
            "DELETE FROM menu_item_sizes z
              WHERE z.menu_item_id = $1
                AND z.label = 'one_size'
                AND z.id = (md5(z.menu_item_id::text || ':one_size'))::uuid
                AND NOT EXISTS (SELECT 1 FROM recipe_lines rl
                                 WHERE rl.owner_type = 'item_size' AND rl.owner_id = z.id)",
        )
        .bind(*id)
        .execute(&mut *tx)
        .await?;
    }

    // Write price epoch if this is new or the price changed.
    // Write price epoch if this is new or the price changed.
    if old_price.is_none_or(|p| p != body.price_override) {
        // Always close any open epoch first. Idempotent when there's nothing
        // open; correct when an orphan epoch was left behind by a prior delete.
        sqlx::query(
            "UPDATE menu_item_price_epochs \
         SET effective_until = now() \
         WHERE menu_item_id = $1 AND size_label = $2 AND effective_until IS NULL",
        )
        .bind(*id)
        .bind(&body.label)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO menu_item_price_epochs \
             (menu_item_id, size_label, price, effective_from, changed_by) \
         VALUES ($1, $2, $3, now(), $4)",
        )
        .bind(*id)
        .bind(&body.label)
        .bind(body.price_override)
        .bind(claims.user_id())
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    delete,
    path = "/menu-items/{id}/sizes/{sid}",
    tag = "menu",
    params(
        ("id" = Uuid, Path, description = "Menu item ID"),
        ("sid" = Uuid, Path, description = "Size ID")
    ),
    responses((status = 204, description = "Size deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_size(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let (item_id, sid) = path.into_inner();
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let item = fetch_menu_item(pool.get_ref(), item_id).await?;
    require_same_org(&claims, Some(item.org_id))?;

    let mut tx = pool.get_ref().begin().await?;

    // Capture the label before we delete the row so we can close the epoch.
    let label: Option<String> = sqlx::query_scalar(
        "SELECT label::text FROM menu_item_sizes WHERE id = $1 AND menu_item_id = $2",
    )
    .bind(sid)
    .bind(item_id)
    .fetch_optional(&mut *tx)
    .await?;

    if let Some(lbl) = label.as_ref() {
        sqlx::query(
            "UPDATE menu_item_price_epochs \
             SET effective_until = now() \
             WHERE menu_item_id = $1 AND size_label = $2 AND effective_until IS NULL",
        )
        .bind(item_id)
        .bind(lbl)
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query("DELETE FROM menu_item_sizes WHERE id = $1 AND menu_item_id = $2")
        .bind(sid)
        .bind(item_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(HttpResponse::NoContent().finish())
}

// ── Addon Items ───────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/addon-items",
    tag = "menu",
    params(AddonItemQuery),
    responses((status = 200, description = "List addon items. A plain array; with `page` or `per_page` the paginated `PaginatedAddonItems` shape.", body = Vec<AddonItem>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_addon_items(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<AddonItemQuery>,
) -> Result<HttpResponse, AppError> {
    crate::client_seen::legacy_hit_at(crate::compat_usage::KIND_MENU_LEGACY_READ, "GET /addon-items");
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    require_same_org(&claims, Some(query.org_id))?;

    // With a branch_id, default_price is branch-effective (override replaces it) and
    // branch-disabled addons are excluded. Without it ($3 NULL), the LEFT JOIN matches
    // nothing → the plain org list (legacy contract). A custom group's options
    // (type NULL) are not addons: see `AddonItem`.
    const FILTER: &str = "FROM compat.addon_items a
         LEFT JOIN compat.branch_addon_overrides bao
                ON bao.addon_item_id = a.id AND bao.branch_id = $3
         WHERE a.org_id = $1 AND a.type IS NOT NULL
           AND ($2::text IS NULL OR a.type = $2)
           AND ($3::uuid IS NULL OR COALESCE(bao.is_available, true) = true)
           AND ($4::text IS NULL OR a.name ILIKE '%' || $4 || '%')";
    let search = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let paginated = query.page.is_some() || query.per_page.is_some();
    let per_page = query.per_page.unwrap_or(50).clamp(1, 500);
    let page = query.page.unwrap_or(1).max(1);
    let (limit, offset) = if paginated {
        (per_page, (page - 1) * per_page)
    } else {
        (i64::MAX, 0)
    };

    let mut rows = sqlx::query_as::<_, AddonItem>(&format!(
        "SELECT a.id, a.org_id, a.name, a.name_translations, a.type as addon_type,
                COALESCE(bao.price_override, a.default_price) AS default_price,
                a.is_active, a.created_at, a.updated_at,
                (SELECT org_ingredient_id FROM compat.addon_item_ingredients WHERE addon_item_id = a.id ORDER BY ingredient_name, org_ingredient_id LIMIT 1) as primary_ingredient_id
         {FILTER}
         ORDER BY a.type ASC, a.created_at ASC
         LIMIT $5 OFFSET $6"
    ))
    .bind(query.org_id)
    .bind(query.addon_type.as_deref())
    .bind(query.branch_id)
    .bind(search)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool.get_ref())
    .await?;

    for addon in &mut rows {
        addon.ingredients = fetch_addon_ingredients(pool.get_ref(), addon.id).await?;
    }
    // Each option's pricing view, branch-effective, in one batched load.
    {
        let mut pricing = crate::orders::catalog_view::Catalog::new(query.branch_id);
        let ids: Vec<Uuid> = rows.iter().map(|a| a.id).collect();
        pricing.ensure_on(pool.get_ref(), &[], &ids).await?;
        for addon in &mut rows {
            addon.pricing = pricing.option(addon.id).map(|o| o.view.clone());
        }
    }

    if !paginated {
        return Ok(HttpResponse::Ok().json(rows));
    }
    let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) {FILTER}"))
        .bind(query.org_id)
        .bind(query.addon_type.as_deref())
        .bind(query.branch_id)
        .bind(search)
        .fetch_one(pool.get_ref())
        .await?;
    Ok(HttpResponse::Ok().json(PaginatedAddonItems {
        data: rows,
        total,
        page,
        per_page,
        total_pages: (total + per_page - 1) / per_page,
    }))
}

// ── Addon catalog (paginated; powers the Branch Overrides add-on grid) ────────
// Separate from the plain `/addon-items` array (which the POS + menu rely on):
// this paginates + searches + filters/sorts by per-branch override status, and
// returns the ORG default_price so the dashboard can show org vs branch.

#[derive(Serialize, Deserialize, Clone, Debug, ToSchema)]
pub struct PaginatedAddonItems {
    pub data: Vec<AddonItem>,
    pub total: i64,
    pub page: i64,
    pub per_page: i64,
    pub total_pages: i64,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AddonCatalogQuery {
    pub org_id: Uuid,
    pub addon_type: Option<String>,
    /// Case-insensitive filter on the addon name.
    pub search: Option<String>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
    /// Enables the per-branch override filter/sort (LEFT JOINs the branch's overrides).
    pub branch_id: Option<Uuid>,
    /// With `branch_id`: true → only addons overridden at the branch; false → only
    /// un-overridden; null → all.
    pub overridden: Option<bool>,
    /// `"overridden"` → overridden addons first (needs `branch_id`); otherwise by type/name.
    pub sort: Option<String>,
}

#[utoipa::path(
    get,
    path = "/addon-items/catalog",
    tag = "menu",
    operation_id = "list_addon_catalog",
    params(AddonCatalogQuery),
    responses((status = 200, description = "Paginated addon catalog (org prices) with per-branch override filter/sort", body = PaginatedAddonItems), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_addon_catalog(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<AddonCatalogQuery>,
) -> Result<HttpResponse, AppError> {
    crate::client_seen::legacy_hit_at(crate::compat_usage::KIND_MENU_LEGACY_READ, "GET /addon-items/catalog");
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    require_same_org(&claims, Some(query.org_id))?;

    let page = query.page.unwrap_or(1).max(1);
    let per_page = query.per_page.unwrap_or(50).clamp(1, 500);
    let offset = (page - 1) * per_page;
    let search = query
        .search
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM compat.addon_items a
         LEFT JOIN compat.branch_addon_overrides bao ON bao.addon_item_id = a.id AND bao.branch_id = $4
         WHERE a.org_id = $1 AND a.type IS NOT NULL
           AND ($2::text IS NULL OR a.type = $2)
           AND ($3::text IS NULL OR a.name ILIKE '%' || $3 || '%')
           AND ($5::bool IS NULL OR (bao.branch_id IS NOT NULL) = $5)",
    )
    .bind(query.org_id)
    .bind(query.addon_type.as_deref())
    .bind(search.as_deref())
    .bind(query.branch_id)
    .bind(query.overridden)
    .fetch_one(pool.get_ref())
    .await?;

    let order_by = match query.sort.as_deref() {
        Some("overridden") => "(bao.branch_id IS NOT NULL) DESC, a.name ASC",
        _ => "a.type ASC, a.name ASC",
    };
    let data = sqlx::query_as::<_, AddonItem>(&format!(
        "SELECT a.id, a.org_id, a.name, a.name_translations, a.type as addon_type,
                a.default_price, a.is_active, a.created_at, a.updated_at,
                (SELECT org_ingredient_id FROM compat.addon_item_ingredients WHERE addon_item_id = a.id ORDER BY ingredient_name, org_ingredient_id LIMIT 1) as primary_ingredient_id
         FROM compat.addon_items a
         LEFT JOIN compat.branch_addon_overrides bao ON bao.addon_item_id = a.id AND bao.branch_id = $4
         WHERE a.org_id = $1 AND a.type IS NOT NULL
           AND ($2::text IS NULL OR a.type = $2)
           AND ($3::text IS NULL OR a.name ILIKE '%' || $3 || '%')
           AND ($5::bool IS NULL OR (bao.branch_id IS NOT NULL) = $5)
         ORDER BY {order_by}
         LIMIT $6 OFFSET $7",
    ))
    .bind(query.org_id)
    .bind(query.addon_type.as_deref())
    .bind(search.as_deref())
    .bind(query.branch_id)
    .bind(query.overridden)
    .bind(per_page)
    .bind(offset)
    .fetch_all(pool.get_ref())
    .await?;

    let total_pages = if total == 0 {
        0
    } else {
        ((total as f64) / (per_page as f64)).ceil() as i64
    };

    Ok(HttpResponse::Ok().json(PaginatedAddonItems {
        data,
        total,
        page,
        per_page,
        total_pages,
    }))
}

#[utoipa::path(
    post,
    path = "/addon-items",
    tag = "menu",
    request_body = CreateAddonItemRequest,
    responses((status = 410, description = "Retired: in production this writes a read-only compat view. Answers 410 SHIM_WRITE_REMOVED.", body = crate::errors::ErrorBody), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_addon_item(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateAddonItemRequest>,
) -> Result<HttpResponse, AppError> {
    let _claims = extract_claims(&req)?;
    let _ = (pool, body);
    Err(crate::compat_usage::shim_write_removed("POST /addon-items"))
}

#[utoipa::path(
    patch,
    path = "/addon-items/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Addon item ID")),
    request_body = UpdateAddonItemRequest,
    responses((status = 410, description = "Retired: in production this writes a read-only compat view. Answers 410 SHIM_WRITE_REMOVED.", body = crate::errors::ErrorBody), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_addon_item(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<UpdateAddonItemRequest>,
) -> Result<HttpResponse, AppError> {
    let _claims = extract_claims(&req)?;
    let _ = (pool, id, body);
    Err(crate::compat_usage::shim_write_removed("PATCH /addon-items/{id}"))
}

#[utoipa::path(
    delete,
    path = "/addon-items/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Addon item ID")),
    responses((status = 410, description = "Retired: in production this writes a read-only compat view. Answers 410 SHIM_WRITE_REMOVED.", body = crate::errors::ErrorBody), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_addon_item(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let _claims = extract_claims(&req)?;
    let _ = (pool, id);
    Err(crate::compat_usage::shim_write_removed("DELETE /addon-items/{id}"))
}

// ── Addon Slots ───────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/menu-items/{id}/addon-slots",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item ID")),
    responses((status = 200, description = "List addon slots", body = Vec<AddonSlot>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_addon_slots(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;

    let item = fetch_menu_item(pool.get_ref(), *id).await?;
    require_same_org(&claims, Some(item.org_id))?;

    let slots = fetch_addon_slots(pool.get_ref(), *id).await?;
    Ok(HttpResponse::Ok().json(slots))
}

#[utoipa::path(
    post,
    path = "/menu-items/{id}/addon-slots",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item ID")),
    request_body = CreateAddonSlotRequest,
    responses((status = 410, description = "Retired: in production this writes a read-only compat view. Answers 410 SHIM_WRITE_REMOVED.", body = crate::errors::ErrorBody), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_addon_slot(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<CreateAddonSlotRequest>,
) -> Result<HttpResponse, AppError> {
    let _claims = extract_claims(&req)?;
    let _ = (pool, id, body);
    Err(crate::compat_usage::shim_write_removed("POST /menu-items/{id}/addon-slots"))
}

#[utoipa::path(
    patch,
    path = "/menu-items/{id}/addon-slots/{slot_id}",
    tag = "menu",
    params(
        ("id" = Uuid, Path, description = "Menu item ID"),
        ("slot_id" = Uuid, Path, description = "Addon slot ID")
    ),
    request_body = UpdateAddonSlotRequest,
    responses((status = 410, description = "Retired: in production this writes a read-only compat view. Answers 410 SHIM_WRITE_REMOVED.", body = crate::errors::ErrorBody), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_addon_slot(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(Uuid, Uuid)>,
    body: web::Json<UpdateAddonSlotRequest>,
) -> Result<HttpResponse, AppError> {
    let _claims = extract_claims(&req)?;
    let _ = (pool, path, body);
    Err(crate::compat_usage::shim_write_removed("PATCH /menu-items/{id}/addon-slots/{slot_id}"))
}

#[utoipa::path(
    delete,
    path = "/menu-items/{id}/addon-slots/{slot_id}",
    tag = "menu",
    params(
        ("id" = Uuid, Path, description = "Menu item ID"),
        ("slot_id" = Uuid, Path, description = "Addon slot ID")
    ),
    responses((status = 410, description = "Retired: in production this writes a read-only compat view. Answers 410 SHIM_WRITE_REMOVED.", body = crate::errors::ErrorBody), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_addon_slot(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<HttpResponse, AppError> {
    let _claims = extract_claims(&req)?;
    let _ = (pool, path);
    Err(crate::compat_usage::shim_write_removed("DELETE /menu-items/{id}/addon-slots/{slot_id}"))
}

// ── Addon Overrides ───────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/menu-items/{id}/overrides",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item ID")),
    responses((status = 410, description = "Retired: in production this writes a read-only compat view. Answers 410 SHIM_WRITE_REMOVED.", body = crate::errors::ErrorBody), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_addon_overrides(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let _claims = extract_claims(&req)?;
    let _ = (pool, id);
    Err(crate::compat_usage::shim_write_removed("GET /menu-items/{id}/overrides"))
}

#[utoipa::path(
    post,
    path = "/menu-items/{id}/overrides",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item ID")),
    request_body = UpsertAddonOverrideRequest,
    responses((status = 410, description = "Retired: in production this writes a read-only compat view. Answers 410 SHIM_WRITE_REMOVED.", body = crate::errors::ErrorBody), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn upsert_addon_override(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<UpsertAddonOverrideRequest>,
) -> Result<HttpResponse, AppError> {
    let _claims = extract_claims(&req)?;
    let _ = (pool, id, body);
    Err(crate::compat_usage::shim_write_removed("POST /menu-items/{id}/overrides"))
}

#[utoipa::path(
    delete,
    path = "/menu-items/{id}/overrides/{override_id}",
    tag = "menu",
    params(
        ("id" = Uuid, Path, description = "Menu item ID"),
        ("override_id" = Uuid, Path, description = "Override ID")
    ),
    responses((status = 410, description = "Retired: in production this writes a read-only compat view. Answers 410 SHIM_WRITE_REMOVED.", body = crate::errors::ErrorBody), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_addon_override(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<HttpResponse, AppError> {
    let _claims = extract_claims(&req)?;
    let _ = (pool, path);
    Err(crate::compat_usage::shim_write_removed("DELETE /menu-items/{id}/overrides/{override_id}"))
}

// ═══════════════════════════════════════════════════════════════
// PER-ITEM ALLOWED ADDON LIST
// ═══════════════════════════════════════════════════════════════

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct PutAllowedAddonsRequest {
    /// Full replacement set of addon item IDs allowed on this menu item.
    /// Send an empty array to clear the restriction (falls back to org catalog).
    pub addon_item_ids: Vec<Uuid>,
}

#[utoipa::path(
    put,
    path = "/menu-items/{id}/allowed-addons",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item ID")),
    request_body = PutAllowedAddonsRequest,
    responses((status = 410, description = "Retired: in production this writes a read-only compat view. Answers 410 SHIM_WRITE_REMOVED.", body = crate::errors::ErrorBody), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_allowed_addons(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<PutAllowedAddonsRequest>,
) -> Result<HttpResponse, AppError> {
    let _claims = extract_claims(&req)?;
    let _ = (pool, id, body);
    Err(crate::compat_usage::shim_write_removed("PUT /menu-items/{id}/allowed-addons"))
}

// ═══════════════════════════════════════════════════════════════
// OPTIONAL FIELDS
// ═══════════════════════════════════════════════════════════════

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct OptionalField {
    pub id: Uuid,
    pub menu_item_id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub price: i32,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name: Option<String>,
    pub ingredient_unit: Option<String>,
    #[schema(value_type = Option<f64>)]
    #[serde(serialize_with = "crate::decimals::serialize_opt")]
    pub quantity_used: Option<sqlx::types::BigDecimal>,
    pub size_label: Option<String>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateOptionalFieldRequest {
    pub name: String,
    #[schema(value_type = Option<Object>)]
    pub name_translations: Option<serde_json::Value>,
    pub price: Option<i32>,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name: Option<String>,
    pub ingredient_unit: Option<String>,
    pub quantity_used: Option<f64>,
    pub size_label: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateOptionalFieldRequest {
    pub name: Option<String>,
    #[schema(value_type = Option<Object>)]
    pub name_translations: Option<serde_json::Value>,
    pub price: Option<i32>,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name: Option<String>,
    pub ingredient_unit: Option<String>,
    pub quantity_used: Option<f64>,
    pub size_label: Option<String>,
    pub is_active: Option<bool>,
}

#[utoipa::path(
    get,
    path = "/menu-items/{id}/optionals",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item ID")),
    responses((status = 200, description = "List optional fields", body = Vec<OptionalField>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_optional_fields(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    let item = fetch_menu_item(pool.get_ref(), *id).await?;
    require_same_org(&claims, Some(item.org_id))?;

    let rows = sqlx::query_as::<_, OptionalField>(
        r#"
        SELECT id, menu_item_id, name, name_translations, price,
               org_ingredient_id, ingredient_name, ingredient_unit,
               quantity_used, size_label::text,
               is_active, created_at, updated_at
        FROM compat.menu_item_optional_fields
        WHERE menu_item_id = $1
        ORDER BY name ASC
        "#,
    )
    .bind(*id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    post,
    path = "/menu-items/{id}/optionals",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item ID")),
    request_body = CreateOptionalFieldRequest,
    responses((status = 410, description = "Retired: in production this writes a read-only compat view. Answers 410 SHIM_WRITE_REMOVED.", body = crate::errors::ErrorBody), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_optional_field(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<CreateOptionalFieldRequest>,
) -> Result<HttpResponse, AppError> {
    let _claims = extract_claims(&req)?;
    let _ = (pool, id, body);
    Err(crate::compat_usage::shim_write_removed("POST /menu-items/{id}/optionals"))
}

#[utoipa::path(
    patch,
    path = "/menu-items/{id}/optionals/{field_id}",
    tag = "menu",
    params(
        ("id" = Uuid, Path, description = "Menu item ID"),
        ("field_id" = Uuid, Path, description = "Field ID")
    ),
    request_body = UpdateOptionalFieldRequest,
    responses((status = 410, description = "Retired: in production this writes a read-only compat view. Answers 410 SHIM_WRITE_REMOVED.", body = crate::errors::ErrorBody), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_optional_field(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(Uuid, Uuid)>,
    body: web::Json<UpdateOptionalFieldRequest>,
) -> Result<HttpResponse, AppError> {
    let _claims = extract_claims(&req)?;
    let _ = (pool, path, body);
    Err(crate::compat_usage::shim_write_removed("PATCH /menu-items/{id}/optionals/{field_id}"))
}

#[utoipa::path(
    delete,
    path = "/menu-items/{id}/optionals/{field_id}",
    tag = "menu",
    params(
        ("id" = Uuid, Path, description = "Menu item ID"),
        ("field_id" = Uuid, Path, description = "Field ID")
    ),
    responses((status = 410, description = "Retired: in production this writes a read-only compat view. Answers 410 SHIM_WRITE_REMOVED.", body = crate::errors::ErrorBody), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_optional_field(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<HttpResponse, AppError> {
    let _claims = extract_claims(&req)?;
    let _ = (pool, path);
    Err(crate::compat_usage::shim_write_removed("DELETE /menu-items/{id}/optionals/{field_id}"))
}

// ── Branch menu override handlers ─────────────────────────────

/// Resolve a branch's org and assert the caller is scoped to it. Returns the org_id.
async fn branch_in_scope(
    pool: &PgPool,
    claims: &Claims,
    branch_id: Uuid,
) -> Result<Uuid, AppError> {
    let org_id: Uuid =
        sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
            .bind(branch_id)
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    require_same_org(claims, Some(org_id))?;
    Ok(org_id)
}

/// Per-(branch, item) size price overrides, ordered for stable output.
async fn fetch_branch_size_overrides(
    pool: &PgPool,
    branch_id: Uuid,
    menu_item_id: Uuid,
) -> Result<Vec<BranchSizeOverride>, AppError> {
    Ok(sqlx::query_as::<_, BranchSizeOverride>(
        "SELECT size_label::text AS size_label, price_override
         FROM compat.branch_menu_size_overrides
         WHERE branch_id = $1 AND menu_item_id = $2
         ORDER BY size_label",
    )
    .bind(branch_id)
    .bind(menu_item_id)
    .fetch_all(pool)
    .await?)
}

/// Overlay a branch's size price overrides onto catalog sizes (POS branch menu).
async fn apply_branch_size_overrides(
    pool: &PgPool,
    branch_id: Uuid,
    menu_item_id: Uuid,
    sizes: &mut [ItemSize],
) -> Result<(), AppError> {
    if sizes.is_empty() {
        return Ok(());
    }
    let overrides = fetch_branch_size_overrides(pool, branch_id, menu_item_id).await?;
    if overrides.is_empty() {
        return Ok(());
    }
    let map: std::collections::HashMap<String, i32> = overrides
        .into_iter()
        .map(|o| (o.size_label, o.price_override))
        .collect();
    for s in sizes.iter_mut() {
        if let Some(p) = map.get(&s.label) {
            s.price_override = *p;
        }
    }
    Ok(())
}

#[utoipa::path(
    get,
    path = "/branch-menu-overrides",
    tag = "menu",
    operation_id = "list_branch_menu_overrides",
    params(BranchOverridesQuery),
    responses((status = 200, description = "Per-branch menu item overrides for the branch (each with its size overrides)", body = Vec<BranchMenuOverride>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_branch_menu_overrides(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<BranchOverridesQuery>,
) -> Result<HttpResponse, AppError> {
    crate::client_seen::legacy_hit_at(crate::compat_usage::KIND_MENU_LEGACY_READ, "GET /branch-menu-overrides");
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    branch_in_scope(pool.get_ref(), &claims, query.branch_id).await?;

    let mut rows = sqlx::query_as::<_, BranchMenuOverride>(
        "SELECT branch_id, menu_item_id, price_override, is_available, updated_at
         FROM compat.branch_menu_overrides WHERE branch_id = $1",
    )
    .bind(query.branch_id)
    .fetch_all(pool.get_ref())
    .await?;

    // Attach each item's size overrides (single round trip, grouped by item).
    let size_rows = sqlx::query_as::<_, (Uuid, String, i32)>(
        "SELECT menu_item_id, size_label::text, price_override
         FROM compat.branch_menu_size_overrides WHERE branch_id = $1",
    )
    .bind(query.branch_id)
    .fetch_all(pool.get_ref())
    .await?;
    let mut by_item: std::collections::HashMap<Uuid, Vec<BranchSizeOverride>> =
        std::collections::HashMap::new();
    for (item_id, size_label, price_override) in size_rows {
        by_item
            .entry(item_id)
            .or_default()
            .push(BranchSizeOverride {
                size_label,
                price_override,
            });
    }
    for row in rows.iter_mut() {
        row.sizes = by_item.remove(&row.menu_item_id).unwrap_or_default();
    }

    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    put,
    path = "/branch-menu-overrides",
    tag = "menu",
    operation_id = "upsert_branch_menu_override",
    request_body = BranchMenuOverrideInput,
    responses((status = 410, description = "Retired: in production this writes a read-only compat view. Answers 410 SHIM_WRITE_REMOVED.", body = crate::errors::ErrorBody), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn upsert_branch_menu_override(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<BranchMenuOverrideInput>,
) -> Result<HttpResponse, AppError> {
    let _claims = extract_claims(&req)?;
    let _ = (pool, body);
    Err(crate::compat_usage::shim_write_removed("PUT /branch-menu-overrides"))
}

#[utoipa::path(
    delete,
    path = "/branch-menu-overrides",
    tag = "menu",
    operation_id = "delete_branch_menu_override",
    params(BranchOverrideKeyQuery),
    responses((status = 410, description = "Retired: in production this writes a read-only compat view. Answers 410 SHIM_WRITE_REMOVED.", body = crate::errors::ErrorBody), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_branch_menu_override(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<BranchOverrideKeyQuery>,
) -> Result<HttpResponse, AppError> {
    let _claims = extract_claims(&req)?;
    let _ = (pool, query);
    Err(crate::compat_usage::shim_write_removed("DELETE /branch-menu-overrides"))
}

// ── Branch addon override handlers ────────────────────────────

#[utoipa::path(
    get,
    path = "/branch-addon-overrides",
    tag = "menu",
    operation_id = "list_branch_addon_overrides",
    params(BranchOverridesQuery),
    responses((status = 200, description = "Per-branch addon overrides for the branch", body = Vec<BranchAddonOverride>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_branch_addon_overrides(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<BranchOverridesQuery>,
) -> Result<HttpResponse, AppError> {
    crate::client_seen::legacy_hit_at(crate::compat_usage::KIND_MENU_LEGACY_READ, "GET /branch-addon-overrides");
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    branch_in_scope(pool.get_ref(), &claims, query.branch_id).await?;

    let rows = sqlx::query_as::<_, BranchAddonOverride>(
        "SELECT branch_id, addon_item_id, price_override, is_available, updated_at
         FROM compat.branch_addon_overrides WHERE branch_id = $1",
    )
    .bind(query.branch_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    put,
    path = "/branch-addon-overrides",
    tag = "menu",
    operation_id = "upsert_branch_addon_override",
    request_body = BranchAddonOverrideInput,
    responses((status = 410, description = "Retired: in production this writes a read-only compat view. Answers 410 SHIM_WRITE_REMOVED.", body = crate::errors::ErrorBody), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn upsert_branch_addon_override(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<BranchAddonOverrideInput>,
) -> Result<HttpResponse, AppError> {
    let _claims = extract_claims(&req)?;
    let _ = (pool, body);
    Err(crate::compat_usage::shim_write_removed("PUT /branch-addon-overrides"))
}

#[utoipa::path(
    delete,
    path = "/branch-addon-overrides",
    tag = "menu",
    operation_id = "delete_branch_addon_override",
    params(BranchAddonOverrideKeyQuery),
    responses((status = 410, description = "Retired: in production this writes a read-only compat view. Answers 410 SHIM_WRITE_REMOVED.", body = crate::errors::ErrorBody), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_branch_addon_override(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<BranchAddonOverrideKeyQuery>,
) -> Result<HttpResponse, AppError> {
    let _claims = extract_claims(&req)?;
    let _ = (pool, query);
    Err(crate::compat_usage::shim_write_removed("DELETE /branch-addon-overrides"))
}

// ── Helpers ───────────────────────────────────────────────────

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

async fn fetch_category(pool: &PgPool, id: Uuid) -> Result<Category, AppError> {
    sqlx::query_as::<_, Category>(
        "SELECT id, org_id, name, name_translations, image_url, is_active, display_order,
                created_at, updated_at, deleted_at
         FROM categories
         WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Category not found".into()))
}

async fn fetch_menu_item(pool: &PgPool, id: Uuid) -> Result<MenuItem, AppError> {
    sqlx::query_as::<_, MenuItem>(
        "SELECT id, org_id, category_id, name, name_translations, description, description_translations, image_url,
                base_price, is_active,
                created_at, updated_at, deleted_at, kind,
                (
                    SELECT a.id::text
                    FROM compat.menu_item_recipes r
                    JOIN compat.addon_item_ingredients ai ON ai.org_ingredient_id = r.org_ingredient_id
                    JOIN compat.addon_items a ON a.id = ai.addon_item_id
                    WHERE r.menu_item_id = menu_items.id
                      AND a.type = 'milk_type'
                    -- Deterministic: the group's display order (component_resolve agrees).
                    ORDER BY (SELECT mo.sort FROM modifier_options mo WHERE mo.id = a.id) NULLS LAST, a.name, a.id
                    LIMIT 1
                ) AS default_milk_addon_id
         FROM menu_items
         WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Menu item not found".into()))
}

/// Every size row of an item, including the synthetic `one_size` row — i.e.
/// `menu_item_sizes` itself rather than the old-client `item_sizes` view.
/// This is the priced truth; `fetch_sizes` is the legacy projection.
async fn fetch_all_sizes(pool: &PgPool, item_id: Uuid) -> Result<Vec<ItemSize>, AppError> {
    Ok(sqlx::query_as::<_, ItemSize>(
        "SELECT id, menu_item_id, label::text, price AS price_override, is_active
         FROM menu_item_sizes
         WHERE menu_item_id = $1
         ORDER BY sort ASC, label ASC",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await?)
}

async fn fetch_sizes(pool: &PgPool, item_id: Uuid) -> Result<Vec<ItemSize>, AppError> {
    Ok(sqlx::query_as::<_, ItemSize>(
        "SELECT id, menu_item_id, label::text, price_override, is_active
         FROM compat.item_sizes
         WHERE menu_item_id = $1
         ORDER BY label ASC",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await?)
}

async fn fetch_addon_slots(pool: &PgPool, item_id: Uuid) -> Result<Vec<AddonSlot>, AppError> {
    Ok(sqlx::query_as::<_, AddonSlot>(
        "SELECT id, menu_item_id, addon_type, label, label_translations, is_required,
                min_selections, max_selections, created_at
         FROM compat.menu_item_addon_slots
         WHERE menu_item_id = $1
         ORDER BY created_at ASC",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await?)
}

async fn fetch_allowed_addon_ids(pool: &PgPool, item_id: Uuid) -> Result<Vec<Uuid>, AppError> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT addon_item_id FROM compat.menu_item_allowed_addons
         WHERE menu_item_id = $1
         ORDER BY sort_order ASC, created_at ASC",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

async fn fetch_optional_fields(
    pool: &PgPool,
    item_id: Uuid,
) -> Result<Vec<OptionalField>, AppError> {
    Ok(sqlx::query_as::<_, OptionalField>(
        "SELECT id, menu_item_id, name, name_translations, price,
                org_ingredient_id, ingredient_name, ingredient_unit,
                quantity_used, size_label::text,
                is_active, created_at, updated_at
         FROM compat.menu_item_optional_fields
         WHERE menu_item_id = $1 AND is_active = true
         ORDER BY name ASC",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await?)
}

async fn fetch_item_recipes(pool: &PgPool, item_id: Uuid) -> Result<Vec<MenuItemRecipe>, AppError> {
    Ok(sqlx::query_as::<_, MenuItemRecipe>(
        r#"SELECT r.org_ingredient_id, r.quantity_used,
                  r.ingredient_name, r.ingredient_unit,
                  (SELECT ic.slug FROM ingredient_categories ic WHERE ic.id = i.category_id) as category,
                  r.size_label::text,
                  i.yield_pct::float8 AS yield_pct
           FROM   compat.menu_item_recipes r
           LEFT JOIN org_ingredients i ON i.id = r.org_ingredient_id
           WHERE  r.menu_item_id = $1"#,
    )
    .bind(item_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|mut r| {
        use bigdecimal::ToPrimitive;
        let stored = r.quantity_used.to_f64().unwrap_or(0.0);
        r.usable_quantity = madar_units::usable_qty(stored, r.yield_pct);
        r
    })
    .collect())
}

/// The branch-effective `/addon-items` shape for a set of addon ids, keyed by id,
/// on one connection (sync pull projection). Unlike the list route a
/// branch-disabled addon is KEPT with `is_available: false`, so turning one back
/// on is an ordinary upsert on the device.
pub(crate) async fn addon_items_by_ids(
    conn: &mut sqlx::PgConnection,
    org_id: Uuid,
    branch_id: Uuid,
    ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, serde_json::Value>, AppError> {
    let mut rows = sqlx::query_as::<_, AddonItem>(
        "SELECT a.id, a.org_id, a.name, a.name_translations, a.type as addon_type,
                COALESCE(bao.price_override, a.default_price) AS default_price,
                a.is_active, a.created_at, a.updated_at,
                (SELECT org_ingredient_id FROM compat.addon_item_ingredients WHERE addon_item_id = a.id ORDER BY ingredient_name, org_ingredient_id LIMIT 1) as primary_ingredient_id
         FROM compat.addon_items a
         LEFT JOIN compat.branch_addon_overrides bao ON bao.addon_item_id = a.id AND bao.branch_id = $2
         WHERE a.org_id = $1 AND a.id = ANY($3) AND a.type IS NOT NULL",
    )
    .bind(org_id)
    .bind(branch_id)
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    let available: std::collections::HashMap<Uuid, bool> = sqlx::query_as::<_, (Uuid, bool)>(
        "SELECT addon_item_id, is_available FROM compat.branch_addon_overrides WHERE branch_id = $1 AND addon_item_id = ANY($2)",
    )
    .bind(branch_id)
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();
    let mut ingredients: std::collections::HashMap<Uuid, Vec<AddonItemIngredient>> =
        std::collections::HashMap::new();
    type IngRow = (
        Uuid,
        Option<Uuid>,
        sqlx::types::BigDecimal,
        String,
        String,
        Option<Uuid>,
        Option<String>,
    );
    for (
        addon,
        org_ingredient_id,
        quantity_used,
        ingredient_name,
        ingredient_unit,
        category_id,
        category_slug,
    ) in sqlx::query_as::<_, IngRow>(
        "SELECT aii.addon_item_id, aii.org_ingredient_id, aii.quantity_used, aii.ingredient_name, \
                aii.ingredient_unit, ic.id, ic.slug \
           FROM compat.addon_item_ingredients aii \
           LEFT JOIN org_ingredients oi ON oi.id = aii.org_ingredient_id \
           LEFT JOIN ingredient_categories ic ON ic.id = oi.category_id \
          WHERE aii.addon_item_id = ANY($1) ORDER BY aii.addon_item_id, aii.ingredient_name",
    )
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?
    {
        ingredients
            .entry(addon)
            .or_default()
            .push(AddonItemIngredient {
                org_ingredient_id,
                quantity_used,
                ingredient_name,
                ingredient_unit,
                category_id,
                category_slug,
            });
    }
    let mut pricing = crate::orders::catalog_view::Catalog::new(Some(branch_id));
    pricing.ensure(&mut *conn, &[], ids).await?;
    let mut out = std::collections::HashMap::new();
    for mut a in rows.drain(..) {
        a.ingredients = ingredients.remove(&a.id).unwrap_or_default();
        a.pricing = pricing.option(a.id).map(|o| o.view.clone());
        let id = a.id;
        let mut v = serde_json::to_value(&a).unwrap_or(serde_json::Value::Null);
        if let serde_json::Value::Object(m) = &mut v {
            m.remove("org_id");
            m.insert(
                "is_available".into(),
                serde_json::Value::Bool(available.get(&id).copied().unwrap_or(true)),
            );
        }
        out.insert(id, v);
    }
    Ok(out)
}

async fn fetch_addon_ingredients(
    pool: &PgPool,
    addon_item_id: Uuid,
) -> Result<Vec<AddonItemIngredient>, AppError> {
    Ok(sqlx::query_as::<_, AddonItemIngredient>(
        "SELECT aii.org_ingredient_id, aii.quantity_used, aii.ingredient_name, aii.ingredient_unit,
                ic.id AS category_id, ic.slug AS category_slug
         FROM   compat.addon_item_ingredients aii
         LEFT JOIN org_ingredients oi ON oi.id = aii.org_ingredient_id
         LEFT JOIN ingredient_categories ic ON ic.id = oi.category_id
         WHERE  aii.addon_item_id = $1
         ORDER BY aii.ingredient_name, aii.org_ingredient_id",
    )
    .bind(addon_item_id)
    .fetch_all(pool)
    .await?)
}

pub(crate) fn deserialize_double_option<'de, T, D>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::deserialize(deserializer).map(Some)
}
