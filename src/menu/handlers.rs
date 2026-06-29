use actix_web::{web, HttpRequest, HttpResponse};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, Deserializer};
use sqlx::PgPool;
use uuid::Uuid;
use actix_web::HttpMessage;

use crate::{
    auth::{guards::require_same_org, jwt::Claims},
    errors::{AppError, AppErrorResponse},
    permissions::checker::check_permission,
    uploads::handlers::delete_old_image,
};
use utoipa::{IntoParams, ToSchema};

// ── Models ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct Category {
    pub id:            Uuid,
    pub org_id:        Uuid,
    pub name:          String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    #[serde(serialize_with = "crate::uploads::handlers::serialize_opt_url")]
    pub image_url:     Option<String>,
    pub is_active:     bool,
    pub created_at:    DateTime<Utc>,
    pub updated_at:    DateTime<Utc>,
    pub deleted_at:    Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct MenuItem {
    pub id:            Uuid,
    pub org_id:        Uuid,
    pub category_id:   Option<Uuid>,
    pub name:          String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub description:   Option<String>,
    #[schema(value_type = Object)]
    pub description_translations: serde_json::Value,
    #[serde(serialize_with = "crate::uploads::handlers::serialize_opt_url")]
    pub image_url:     Option<String>,
    pub base_price:    i32,
    pub is_active:     bool,
    pub created_at:    DateTime<Utc>,
    pub updated_at:    DateTime<Utc>,
    pub deleted_at:    Option<DateTime<Utc>>,
    pub default_milk_addon_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct ItemSize {
    pub id:             Uuid,
    pub menu_item_id:   Uuid,
    pub label:          String,
    pub price_override: i32,
    pub is_active:      bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct AddonItem {
    pub id:            Uuid,
    pub org_id:        Uuid,
    pub name:          String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub addon_type:    String,
    pub default_price: i32,
    pub is_active:     bool,
    pub created_at:    DateTime<Utc>,
    pub updated_at:    DateTime<Utc>,
    pub primary_ingredient_id: Option<Uuid>,
    #[serde(default)]
    #[sqlx(skip)]
    pub ingredients:   Vec<AddonItemIngredient>,
}

// ── Addon Slot models ─────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct AddonSlot {
    pub id:             Uuid,
    pub menu_item_id:   Uuid,
    pub addon_type:     String,
    pub label:          Option<String>,
    #[schema(value_type = Object)]
    pub label_translations: serde_json::Value,
    pub is_required:    bool,
    pub min_selections: i32,
    pub max_selections: Option<i32>,
    pub created_at:     DateTime<Utc>,
}

// ── Addon Override models ─────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct AddonOverride {
    pub id:                         Uuid,
    pub menu_item_id:               Uuid,
    pub addon_item_id:              Uuid,
    pub addon_item_name:            String,
    pub size_label:                 Option<String>,
    pub ingredient_name:            String,
    pub org_ingredient_id:          Option<Uuid>,
    pub ingredient_unit:            String,
    #[schema(value_type = f64)]
    pub quantity_used:              sqlx::types::BigDecimal,
    pub replaces_org_ingredient_id: Option<Uuid>,
    pub replaces_ingredient_name:   Option<String>,
    pub combo_addon_item_id:        Option<Uuid>,
    pub combo_addon_item_name:      Option<String>,
    pub created_at:                 DateTime<Utc>,
    pub updated_at:                 DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct MenuItemRecipe {
    pub org_ingredient_id: Option<Uuid>,
    #[schema(value_type = f64)]
    pub quantity_used:     sqlx::types::BigDecimal,
    pub ingredient_name:   String,
    pub ingredient_unit:   String,
    pub category:          String,
    pub size_label:        String,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct AddonItemIngredient {
    pub org_ingredient_id: Option<Uuid>,
    #[schema(value_type = f64)]
    pub quantity_used:     sqlx::types::BigDecimal,
    pub ingredient_name:   String,
    pub ingredient_unit:   String,
}

// ── MenuItemFull — slots embedded instead of option_groups ────

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, ToSchema)]
pub struct MenuItemFull {
    #[serde(flatten)]
    pub item:               MenuItem,
    pub sizes:              Vec<ItemSize>,
    pub addon_slots:        Vec<AddonSlot>,
    pub optional_fields:    Vec<OptionalField>,
    pub recipes:            Vec<MenuItemRecipe>,
    /// Explicit per-item addon allowlist. Empty = no restriction (use org catalog).
    pub allowed_addon_ids:  Vec<Uuid>,
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
    pub org_id:      Uuid,
    pub category_id: Option<Uuid>,
    /// When true, embed sizes + addon slots + optionals + recipes per item
    /// (the shape the POS/teller consumes). Always returns a plain, unpaginated
    /// array — the POS depends on this contract.
    pub full:        Option<bool>,
    /// When set, prices are branch-effective (branch override replaces base_price)
    /// and items disabled at this branch are excluded — the per-branch menu the POS
    /// consumes. Omitted → the plain org catalog (legacy behaviour).
    pub branch_id:   Option<Uuid>,
}

/// Query for the dashboard catalog endpoint: paginated menu items with embedded
/// per-SKU costs. Kept separate from [`MenuItemQuery`] so the plain `/menu-items`
/// list the POS relies on stays an unpaginated array.
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MenuCatalogQuery {
    pub org_id:      Uuid,
    pub category_id: Option<Uuid>,
    /// Case-insensitive filter on the item name.
    pub search:      Option<String>,
    /// 1-based page number (default 1).
    pub page:        Option<i64>,
    /// Page size (default 50, max 500).
    pub per_page:    Option<i64>,
    /// When set, enables the per-branch override filter/sort (LEFT JOINs the
    /// branch's overrides). Prices in the response stay org-level.
    pub branch_id:   Option<Uuid>,
    /// With `branch_id`: true → only items overridden at the branch; false →
    /// only un-overridden; null → all.
    pub overridden:  Option<bool>,
    /// `"overridden"` → overridden items first (needs `branch_id`); otherwise A–Z.
    pub sort:        Option<String>,
}

// ── Branch menu overrides (per-branch price + availability layer) ─────────────
// The branch layer over the org catalog: absence of a row ⟹ inherit base_price and
// fully available. A row may set a branch price (price_override, piastres — null
// inherits) and/or disable the item at that branch (is_available=false). Applies to
// all channels including POS dine-in; a future channel layer (in-mall/outside) sits
// on top.

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct BranchMenuOverride {
    pub branch_id:      Uuid,
    pub menu_item_id:   Uuid,
    /// Branch price in piastres; null inherits the org catalog base_price.
    pub price_override: Option<i32>,
    /// False disables the item at this branch (excluded from the branch menu).
    pub is_available:   bool,
    pub updated_at:     DateTime<Utc>,
    /// Per-size branch prices for this item (empty when none). Availability is item-level.
    #[serde(default)]
    #[sqlx(skip)]
    pub sizes:          Vec<BranchSizeOverride>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct BranchSizeOverride {
    pub size_label:     String,
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
    pub branch_id:    Uuid,
    pub menu_item_id: Uuid,
}

fn override_default_available() -> bool { true }

#[derive(Deserialize, Serialize, Clone, ToSchema)]
pub struct BranchSizeOverrideInput {
    pub size_label:     String,
    pub price_override: i32,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct BranchMenuOverrideInput {
    pub branch_id:    Uuid,
    pub menu_item_id: Uuid,
    /// Branch price in piastres; null inherits the org catalog base_price.
    #[serde(default)]
    pub price_override: Option<i32>,
    #[serde(default = "override_default_available")]
    pub is_available:   bool,
    /// Per-size branch prices. `null`/omitted → leave existing size overrides untouched;
    /// a list → REPLACE the item's size overrides with exactly that set (empty clears them).
    #[serde(default)]
    pub sizes:          Option<Vec<BranchSizeOverrideInput>>,
}

// ── Branch addon overrides (per-branch addon price + availability) ────────────
// The addon analogue of BranchMenuOverride. No sizes (addons have none).

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct BranchAddonOverride {
    pub branch_id:      Uuid,
    pub addon_item_id:  Uuid,
    /// Branch price in piastres; null inherits the org default_price.
    pub price_override: Option<i32>,
    /// False disables the addon at this branch (excluded from the branch addon list).
    pub is_available:   bool,
    pub updated_at:     DateTime<Utc>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BranchAddonOverrideKeyQuery {
    pub branch_id:     Uuid,
    pub addon_item_id: Uuid,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct BranchAddonOverrideInput {
    pub branch_id:     Uuid,
    pub addon_item_id: Uuid,
    /// Branch price in piastres; null inherits the org default_price.
    #[serde(default)]
    pub price_override: Option<i32>,
    #[serde(default = "override_default_available")]
    pub is_available:   bool,
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
    pub org_id:     Uuid,
    pub addon_type: Option<String>,
    /// When set, prices are branch-effective (override replaces default_price) and
    /// addons disabled at this branch are excluded — the per-branch addon list the
    /// POS consumes. Omitted → the plain org list (legacy behaviour).
    pub branch_id:  Option<Uuid>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateCategoryRequest {
    pub org_id:        Uuid,
    pub name:          String,
    #[schema(value_type = Option<Object>)]
    pub name_translations: Option<serde_json::Value>,
    pub image_url:     Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateCategoryRequest {
    pub name:          Option<String>,
    #[schema(value_type = Option<Object>)]
    pub name_translations: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub image_url:     Option<Option<String>>,
    pub is_active:     Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateMenuItemRequest {
    pub org_id:        Uuid,
    pub category_id:   Uuid,
    pub name:          String,
    #[schema(value_type = Option<Object>)]
    pub name_translations: Option<serde_json::Value>,
    pub description:   Option<String>,
    #[schema(value_type = Option<Object>)]
    pub description_translations: Option<serde_json::Value>,
    pub image_url:     Option<String>,
    pub base_price:    i32,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateMenuItemRequest {
    pub category_id:   Option<Uuid>,
    pub name:          Option<String>,
    #[schema(value_type = Option<Object>)]
    pub name_translations: Option<serde_json::Value>,
    pub description:   Option<String>,
    #[schema(value_type = Option<Object>)]
    pub description_translations: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub image_url:     Option<Option<String>>,
    pub base_price:    Option<i32>,
    pub is_active:     Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateAddonItemRequest {
    pub org_id:        Uuid,
    pub name:          String,
    #[schema(value_type = Option<Object>)]
    pub name_translations: Option<serde_json::Value>,
    pub addon_type:    String,
    pub default_price: i32,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateAddonItemRequest {
    pub name:          Option<String>,
    #[schema(value_type = Option<Object>)]
    pub name_translations: Option<serde_json::Value>,
    pub addon_type:    Option<String>,
    pub default_price: Option<i32>,
    pub is_active:     Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpsertSizeRequest {
    pub label:          String,
    pub price_override: i32,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateAddonSlotRequest {
    pub addon_type:     Option<String>,
    pub label:          Option<String>,
    #[schema(value_type = Option<Object>)]
    pub label_translations: Option<serde_json::Value>,
    pub is_required:    Option<bool>,
    pub min_selections: Option<i32>,
    pub max_selections: Option<i32>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateAddonSlotRequest {
    pub label:          Option<String>,
    #[schema(value_type = Option<Object>)]
    pub label_translations: Option<serde_json::Value>,
    pub is_required:    Option<bool>,
    pub min_selections: Option<i32>,
    pub max_selections: Option<i32>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpsertAddonOverrideRequest {
    pub addon_item_id:              Uuid,
    pub size_label:                 Option<String>,
    pub ingredient_name:            String,
    pub org_ingredient_id:          Option<Uuid>,
    pub ingredient_unit:            String,
    pub quantity_used:              f64,
    pub replaces_org_ingredient_id: Option<Uuid>,
    pub combo_addon_item_id:        Option<Uuid>,
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
    req:   HttpRequest,
    pool:  web::Data<PgPool>,
    query: web::Query<OrgQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "categories", "read").await?;
    require_same_org(&claims, Some(query.org_id))?;

    let rows = sqlx::query_as::<_, Category>(
        "SELECT id, org_id, name, name_translations, image_url, is_active,
                created_at, updated_at, deleted_at
         FROM categories
         WHERE org_id = $1 AND deleted_at IS NULL
         ORDER BY name ASC",
    )
    .bind(query.org_id)
    .fetch_all(pool.get_ref())
    .await?;

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
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<CreateCategoryRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "categories", "create").await?;
    require_same_org(&claims, Some(body.org_id))?;

    let mut_body = body.into_inner();
    let mut name_translations = mut_body.name_translations.unwrap_or_else(|| serde_json::json!({}));
    crate::translation::ensure_translations_json(&mut name_translations, Some(&mut_body.name))
        .await
        .map_err(|_| AppError::Internal)?;

    let row = sqlx::query_as::<_, Category>(
        "INSERT INTO categories (org_id, name, name_translations, image_url)
         VALUES ($1, $2, $3, $4)
         RETURNING id, org_id, name, name_translations, image_url, is_active,
                   created_at, updated_at, deleted_at",
    )
    .bind(mut_body.org_id)
    .bind(&mut_body.name)
    .bind(name_translations)
    .bind(&mut_body.image_url)
    .fetch_one(pool.get_ref())
    .await?;

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
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
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
         RETURNING id, org_id, name, name_translations, image_url, is_active,
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

    // If explicit null, cleanup old image from storage
    if mut_body.image_url == Some(None)
        && let Some(old_url) = existing.image_url {
            let uploads_dir = std::env::var("UPLOADS_DIR").unwrap_or_else(|_| "./uploads".to_string());
            let base_url    = std::env::var("UPLOADS_BASE_URL").unwrap_or_default();
            delete_old_image(&old_url, &base_url, &uploads_dir, Some(existing.org_id)).await;
        }

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
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "categories", "delete").await?;

    let existing = fetch_category(pool.get_ref(), *id).await?;
    require_same_org(&claims, Some(existing.org_id))?;

    sqlx::query(
        "UPDATE categories SET deleted_at = NOW() WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(*id)
    .execute(pool.get_ref())
    .await?;

    Ok(HttpResponse::NoContent().finish())
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
    req:   HttpRequest,
    pool:  web::Data<PgPool>,
    cache: Option<web::Data<crate::menu::cache::MenuCache>>,
    query: web::Query<MenuItemQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    require_same_org(&claims, Some(query.org_id))?;

    // Serve from the per-org menu cache when enabled (MENU_CACHE_TTL_SECS>0). The
    // variant folds in every param that changes the body so views never alias.
    // Disabled / unregistered (every test) → `cache` is None and we hit the DB.
    let variant = format!(
        "menu|{}|{}|{}",
        query.category_id.map(|c| c.to_string()).unwrap_or_default(),
        query.branch_id.map(|b| b.to_string()).unwrap_or_default(),
        query.full.unwrap_or(false),
    );
    if let Some(c) = &cache {
        if let Some(body) = c.get(query.org_id, &variant).await {
            return Ok(HttpResponse::Ok().content_type("application/json").body(body));
        }
    }

    // When branch_id is supplied, prices are branch-effective (override replaces
    // base_price) and branch-disabled items are excluded — the per-branch menu the POS
    // consumes. When null the LEFT JOIN matches nothing (NULL branch_id) so this is the
    // plain org catalog, preserving the legacy contract.
    let items = sqlx::query_as::<_, MenuItem>(
        "SELECT mi.id, mi.org_id, mi.category_id, mi.name, mi.name_translations,
                mi.description, mi.description_translations, mi.image_url,
                COALESCE(bmo.price_override, mi.base_price) AS base_price,
                mi.is_active,
                mi.created_at, mi.updated_at, mi.deleted_at,
                (
                    SELECT a.id::text
                    FROM menu_item_recipes r
                    JOIN addon_item_ingredients ai ON ai.org_ingredient_id = r.org_ingredient_id
                    JOIN addon_items a ON a.id = ai.addon_item_id
                    WHERE r.menu_item_id = mi.id
                      AND a.type = 'milk_type'
                    LIMIT 1
                ) AS default_milk_addon_id
         FROM menu_items mi
         LEFT JOIN branch_menu_overrides bmo
                ON bmo.menu_item_id = mi.id AND bmo.branch_id = $3
         WHERE mi.org_id = $1 AND mi.deleted_at IS NULL
           AND ($2::uuid IS NULL OR mi.category_id = $2)
           AND ($3::uuid IS NULL OR COALESCE(bmo.is_available, true) = true)
         ORDER BY mi.name ASC",
    )
    .bind(query.org_id)
    .bind(query.category_id)
    .bind(query.branch_id)
    .fetch_all(pool.get_ref())
    .await?;

    // ?full=true embeds sizes + addon_slots + optionals + recipes per item.
    if query.full.unwrap_or(false) {
        let mut result: Vec<MenuItemFull> = vec![];
        for item in items {
            let mut sizes = fetch_sizes(pool.get_ref(), item.id).await?;
            // Branch menu (branch_id set): overlay this branch's per-size price overrides
            // so the POS sees branch-effective size prices, not just the catalog ones.
            if let Some(branch_id) = query.branch_id {
                apply_branch_size_overrides(pool.get_ref(), branch_id, item.id, &mut sizes).await?;
            }
            let addon_slots        = fetch_addon_slots(pool.get_ref(), item.id).await?;
            let optional_fields    = fetch_optional_fields(pool.get_ref(), item.id).await?;
            let recipes            = fetch_item_recipes(pool.get_ref(), item.id).await?;
            let allowed_addon_ids  = fetch_allowed_addon_ids(pool.get_ref(), item.id).await?;
            result.push(MenuItemFull { item, sizes, addon_slots, optional_fields, recipes, allowed_addon_ids });
        }
        let body = web::Bytes::from(serde_json::to_vec(&result).map_err(|_| AppError::Internal)?);
        if let Some(c) = &cache {
            c.put(query.org_id, &variant, body.clone()).await;
        }
        return Ok(HttpResponse::Ok().content_type("application/json").body(body));
    }

    let body = web::Bytes::from(serde_json::to_vec(&items).map_err(|_| AppError::Internal)?);
    if let Some(c) = &cache {
        c.put(query.org_id, &variant, body.clone()).await;
    }
    Ok(HttpResponse::Ok().content_type("application/json").body(body))
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
    req:   HttpRequest,
    pool:  web::Data<PgPool>,
    query: web::Query<MenuCatalogQuery>,
) -> Result<HttpResponse, AppError> {
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

    // Branch-override filter/sort: LEFT JOIN the branch's overrides ($4); a row's
    // existence means "overridden at this branch". `overridden` ($5) filters on that;
    // sort='overridden' brings overridden items first. With no branch_id the join
    // matches nothing → identical to the plain org catalog.
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM menu_items mi
         LEFT JOIN branch_menu_overrides bmo ON bmo.menu_item_id = mi.id AND bmo.branch_id = $4
         WHERE mi.org_id = $1 AND mi.deleted_at IS NULL
           AND ($2::uuid IS NULL OR mi.category_id = $2)
           AND ($3::text IS NULL OR mi.name ILIKE '%' || $3 || '%')
           AND ($5::bool IS NULL OR (bmo.branch_id IS NOT NULL) = $5)",
    )
    .bind(query.org_id)
    .bind(query.category_id)
    .bind(search.as_deref())
    .bind(query.branch_id)
    .bind(query.overridden)
    .fetch_one(pool.get_ref())
    .await?;

    let order_by = match query.sort.as_deref() {
        Some("overridden") => "(bmo.branch_id IS NOT NULL) DESC, mi.name ASC",
        _ => "mi.name ASC",
    };
    let rows = sqlx::query_as::<_, MenuItem>(&format!(
        "SELECT mi.id, mi.org_id, mi.category_id, mi.name, mi.name_translations,
                mi.description, mi.description_translations, mi.image_url,
                mi.base_price, mi.is_active,
                mi.created_at, mi.updated_at, mi.deleted_at,
                (
                    SELECT a.id::text
                    FROM menu_item_recipes r
                    JOIN addon_item_ingredients ai ON ai.org_ingredient_id = r.org_ingredient_id
                    JOIN addon_items a ON a.id = ai.addon_item_id
                    WHERE r.menu_item_id = mi.id
                      AND a.type = 'milk_type'
                    LIMIT 1
                ) AS default_milk_addon_id
         FROM menu_items mi
         LEFT JOIN branch_menu_overrides bmo ON bmo.menu_item_id = mi.id AND bmo.branch_id = $4
         WHERE mi.org_id = $1 AND mi.deleted_at IS NULL
           AND ($2::uuid IS NULL OR mi.category_id = $2)
           AND ($3::text IS NULL OR mi.name ILIKE '%' || $3 || '%')
           AND ($5::bool IS NULL OR (bmo.branch_id IS NOT NULL) = $5)
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
    .fetch_all(pool.get_ref())
    .await?;

    let total_pages = if total == 0 { 0 } else { ((total as f64) / (per_page as f64)).ceil() as i64 };

    // Embed per-SKU costs for just this page so the dashboard catalog renders
    // food-cost chips in the same round trip.
    let ids: Vec<Uuid> = rows.iter().map(|m| m.id).collect();
    let costs = crate::costing::sku_costs_for_items(pool.get_ref(), query.org_id, &ids, query.branch_id).await?;
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
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;

    let item = fetch_menu_item(pool.get_ref(), *id).await?;
    require_same_org(&claims, Some(item.org_id))?;

    let sizes             = fetch_sizes(pool.get_ref(), *id).await?;
    let addon_slots       = fetch_addon_slots(pool.get_ref(), *id).await?;
    let optional_fields   = fetch_optional_fields(pool.get_ref(), *id).await?;
    let recipes           = fetch_item_recipes(pool.get_ref(), *id).await?;
    let allowed_addon_ids = fetch_allowed_addon_ids(pool.get_ref(), *id).await?;

    Ok(HttpResponse::Ok().json(MenuItemFull { item, sizes, addon_slots, optional_fields, recipes, allowed_addon_ids }))
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
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<CreateMenuItemRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "create").await?;
    require_same_org(&claims, Some(body.org_id))?;

    let mut_body = body.into_inner();
    let mut name_translations = mut_body.name_translations.clone().unwrap_or_else(|| serde_json::json!({}));
    crate::translation::ensure_translations_json(&mut name_translations, Some(&mut_body.name))
        .await
        .map_err(|_| AppError::Internal)?;
        
    let mut description_translations = mut_body.description_translations.clone().unwrap_or_else(|| serde_json::json!({}));
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
                   created_at, updated_at, deleted_at,
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
         VALUES ($1, NULL, $2, now(), $3)"
    )
    .bind(item.id)
    .bind(mut_body.base_price)
    .bind(claims.user_id())
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(HttpResponse::Created().json(MenuItemFull {
        item,
        sizes:             vec![],
        addon_slots:       vec![],
        optional_fields:   vec![],
        recipes:           vec![],
        allowed_addon_ids: vec![],
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
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
    body: web::Json<UpdateMenuItemRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let existing = fetch_menu_item(pool.get_ref(), *id).await?;
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
    
    let mut description_translations = existing.description_translations;
    if let Some(new_desc) = &mut_body.description {
        crate::translation::ensure_translations_json(&mut description_translations, Some(new_desc))
            .await
            .map_err(|_| AppError::Internal)?;
    } else if let Some(new_tr) = mut_body.description_translations {
        description_translations = new_tr;
        if let Some(desc) = &existing.description {
            crate::translation::ensure_translations_json(&mut description_translations, Some(desc))
                .await
                .map_err(|_| AppError::Internal)?;
        }
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
                   created_at, updated_at, deleted_at,
                   (
                       SELECT a.id::text
                       FROM menu_item_recipes r
                       JOIN addon_item_ingredients ai ON ai.org_ingredient_id = r.org_ingredient_id
                       JOIN addon_items a ON a.id = ai.addon_item_id
                       WHERE r.menu_item_id = menu_items.id
                         AND a.type = 'milk_type'
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

    // Maintain price epoch whenever base_price actually changed.
    if let Some(new_price) = mut_body.base_price
        && new_price != existing.base_price {
            sqlx::query(
                "UPDATE menu_item_price_epochs \
                 SET effective_until = now() \
                 WHERE menu_item_id = $1 AND size_label IS NULL AND effective_until IS NULL"
            )
            .bind(*id)
            .execute(&mut *tx)
            .await?;

            sqlx::query(
                "INSERT INTO menu_item_price_epochs \
                     (menu_item_id, size_label, price, effective_from, changed_by) \
                 VALUES ($1, NULL, $2, now(), $3)"
            )
            .bind(*id)
            .bind(new_price)
            .bind(claims.user_id())
            .execute(&mut *tx)
            .await?;
        }

    tx.commit().await?;

    // If explicit null, cleanup old image from storage
    if mut_body.image_url == Some(None)
        && let Some(old_url) = existing.image_url {
            let uploads_dir = std::env::var("UPLOADS_DIR").unwrap_or_else(|_| "./uploads".to_string());
            let base_url    = std::env::var("UPLOADS_BASE_URL").unwrap_or_default();
            delete_old_image(&old_url, &base_url, &uploads_dir, Some(existing.org_id)).await;
        }

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
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "delete").await?;

    let mut tx = pool.get_ref().begin().await?;

    // Close base-price epoch AND every size epoch in one shot.
    sqlx::query(
        "UPDATE menu_item_price_epochs \
         SET effective_until = now() \
         WHERE menu_item_id = $1 AND effective_until IS NULL"
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
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
    body: web::Json<UpsertSizeRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let item = fetch_menu_item(pool.get_ref(), *id).await?;
    require_same_org(&claims, Some(item.org_id))?;

    // Capture old price (if exists) before the upsert.
    let old_price: Option<i32> = sqlx::query_scalar(
        "SELECT price_override FROM item_sizes \
         WHERE menu_item_id = $1 AND label = $2::item_size"
    )
    .bind(*id)
    .bind(&body.label)
    .fetch_optional(pool.get_ref())
    .await?
    .flatten();

    let mut tx = pool.get_ref().begin().await?;

    let row = sqlx::query_as::<_, ItemSize>(
        "INSERT INTO item_sizes (menu_item_id, label, price_override)
         VALUES ($1, $2::item_size, $3)
         ON CONFLICT (menu_item_id, label) DO UPDATE SET
             price_override = EXCLUDED.price_override,
             is_active      = TRUE
         RETURNING id, menu_item_id, label::text, price_override, is_active",
    )
    .bind(*id)
    .bind(&body.label)
    .bind(body.price_override)
    .fetch_one(&mut *tx)
    .await?;

    // Write price epoch if this is new or the price changed.
// Write price epoch if this is new or the price changed.
if old_price.is_none_or(|p| p != body.price_override) {
    // Always close any open epoch first. Idempotent when there's nothing
    // open; correct when an orphan epoch was left behind by a prior delete.
    sqlx::query(
        "UPDATE menu_item_price_epochs \
         SET effective_until = now() \
         WHERE menu_item_id = $1 AND size_label = $2 AND effective_until IS NULL"
    )
    .bind(*id)
    .bind(&body.label)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO menu_item_price_epochs \
             (menu_item_id, size_label, price, effective_from, changed_by) \
         VALUES ($1, $2, $3, now(), $4)"
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
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<HttpResponse, AppError> {
    let claims         = extract_claims(&req)?;
    let (item_id, sid) = path.into_inner();
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let item = fetch_menu_item(pool.get_ref(), item_id).await?;
    require_same_org(&claims, Some(item.org_id))?;

    let mut tx = pool.get_ref().begin().await?;

    // Capture the label before we delete the row so we can close the epoch.
    let label: Option<String> = sqlx::query_scalar(
        "SELECT label::text FROM item_sizes WHERE id = $1 AND menu_item_id = $2"
    )
    .bind(sid)
    .bind(item_id)
    .fetch_optional(&mut *tx)
    .await?;

    if let Some(lbl) = label.as_ref() {
        sqlx::query(
            "UPDATE menu_item_price_epochs \
             SET effective_until = now() \
             WHERE menu_item_id = $1 AND size_label = $2 AND effective_until IS NULL"
        )
        .bind(item_id)
        .bind(lbl)
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query("DELETE FROM item_sizes WHERE id = $1 AND menu_item_id = $2")
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
    responses((status = 200, description = "List addon items", body = Vec<AddonItem>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_addon_items(
    req:   HttpRequest,
    pool:  web::Data<PgPool>,
    query: web::Query<AddonItemQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    require_same_org(&claims, Some(query.org_id))?;

    // With a branch_id, default_price is branch-effective (override replaces it) and
    // branch-disabled addons are excluded. Without it ($3 NULL), the LEFT JOIN matches
    // nothing → the plain org list (legacy contract).
    let mut rows = sqlx::query_as::<_, AddonItem>(
        "SELECT a.id, a.org_id, a.name, a.name_translations, a.type as addon_type,
                COALESCE(bao.price_override, a.default_price) AS default_price,
                a.is_active, a.created_at, a.updated_at,
                (SELECT org_ingredient_id FROM addon_item_ingredients WHERE addon_item_id = a.id LIMIT 1) as primary_ingredient_id
         FROM addon_items a
         LEFT JOIN branch_addon_overrides bao
                ON bao.addon_item_id = a.id AND bao.branch_id = $3
         WHERE a.org_id = $1
           AND ($2::text IS NULL OR a.type = $2)
           AND ($3::uuid IS NULL OR COALESCE(bao.is_available, true) = true)
         ORDER BY a.type ASC, a.created_at ASC",
    )
    .bind(query.org_id)
    .bind(query.addon_type.as_deref())
    .bind(query.branch_id)
    .fetch_all(pool.get_ref())
    .await?;

    for addon in &mut rows {
        addon.ingredients = fetch_addon_ingredients(pool.get_ref(), addon.id).await?;
    }

    Ok(HttpResponse::Ok().json(rows))
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
    pub org_id:     Uuid,
    pub addon_type: Option<String>,
    /// Case-insensitive filter on the addon name.
    pub search:     Option<String>,
    pub page:       Option<i64>,
    pub per_page:   Option<i64>,
    /// Enables the per-branch override filter/sort (LEFT JOINs the branch's overrides).
    pub branch_id:  Option<Uuid>,
    /// With `branch_id`: true → only addons overridden at the branch; false → only
    /// un-overridden; null → all.
    pub overridden: Option<bool>,
    /// `"overridden"` → overridden addons first (needs `branch_id`); otherwise by type/name.
    pub sort:       Option<String>,
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
    req:   HttpRequest,
    pool:  web::Data<PgPool>,
    query: web::Query<AddonCatalogQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    require_same_org(&claims, Some(query.org_id))?;

    let page = query.page.unwrap_or(1).max(1);
    let per_page = query.per_page.unwrap_or(50).clamp(1, 500);
    let offset = (page - 1) * per_page;
    let search = query.search.as_ref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM addon_items a
         LEFT JOIN branch_addon_overrides bao ON bao.addon_item_id = a.id AND bao.branch_id = $4
         WHERE a.org_id = $1
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
                (SELECT org_ingredient_id FROM addon_item_ingredients WHERE addon_item_id = a.id LIMIT 1) as primary_ingredient_id
         FROM addon_items a
         LEFT JOIN branch_addon_overrides bao ON bao.addon_item_id = a.id AND bao.branch_id = $4
         WHERE a.org_id = $1
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

    let total_pages = if total == 0 { 0 } else { ((total as f64) / (per_page as f64)).ceil() as i64 };

    Ok(HttpResponse::Ok().json(PaginatedAddonItems { data, total, page, per_page, total_pages }))
}

#[utoipa::path(
    post,
    path = "/addon-items",
    tag = "menu",
    request_body = CreateAddonItemRequest,
    responses((status = 201, description = "Addon item created", body = AddonItem), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_addon_item(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<CreateAddonItemRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "create").await?;
    require_same_org(&claims, Some(body.org_id))?;

    let mut_body = body.into_inner();
    let mut name_translations = mut_body.name_translations.unwrap_or_else(|| serde_json::json!({}));
    crate::translation::ensure_translations_json(&mut name_translations, Some(&mut_body.name))
        .await
        .map_err(|_| AppError::Internal)?;

    let mut row = sqlx::query_as::<_, AddonItem>(
        "INSERT INTO addon_items (org_id, name, name_translations, type, default_price)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id, org_id, name, name_translations, type as addon_type, default_price,
                   is_active, created_at, updated_at,
                   NULL::uuid as primary_ingredient_id",
    )
    .bind(mut_body.org_id)
    .bind(&mut_body.name)
    .bind(name_translations)
    .bind(&mut_body.addon_type)
    .bind(mut_body.default_price)
    .fetch_one(pool.get_ref())
    .await?;

    row.ingredients = vec![];

    Ok(HttpResponse::Created().json(row))
}

#[utoipa::path(
    patch,
    path = "/addon-items/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Addon item ID")),
    request_body = UpdateAddonItemRequest,
    responses((status = 200, description = "Addon item updated", body = AddonItem), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_addon_item(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
    body: web::Json<UpdateAddonItemRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let existing = fetch_addon_item(pool.get_ref(), *id).await?;
    require_same_org(&claims, Some(existing.org_id))?;

    let mut_body = body.into_inner();

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

    let mut row = sqlx::query_as::<_, AddonItem>(
        "UPDATE addon_items SET
             name              = COALESCE($2, name),
             name_translations = $3,
             type              = COALESCE($4, type),
             default_price     = COALESCE($5, default_price),
             is_active         = COALESCE($6, is_active)
         WHERE id = $1
         RETURNING id, org_id, name, name_translations, type as addon_type, default_price,
                   is_active, created_at, updated_at,
                   (SELECT org_ingredient_id FROM addon_item_ingredients WHERE addon_item_id = addon_items.id LIMIT 1) as primary_ingredient_id",
    )
    .bind(*id)
    .bind(&mut_body.name)
    .bind(name_translations)
    .bind(&mut_body.addon_type)
    .bind(mut_body.default_price)
    .bind(mut_body.is_active)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Addon item not found".into()))?;

    row.ingredients = fetch_addon_ingredients(pool.get_ref(), *id).await?;

    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    delete,
    path = "/addon-items/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Addon item ID")),
    responses((status = 204, description = "Addon item deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_addon_item(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "delete").await?;

    let existing = fetch_addon_item(pool.get_ref(), *id).await?;
    require_same_org(&claims, Some(existing.org_id))?;

    sqlx::query("DELETE FROM addon_items WHERE id = $1")
        .bind(*id)
        .execute(pool.get_ref())
        .await?;

    Ok(HttpResponse::NoContent().finish())
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
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
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
    responses((status = 201, description = "Addon slot created", body = AddonSlot), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_addon_slot(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
    body: web::Json<CreateAddonSlotRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let item = fetch_menu_item(pool.get_ref(), *id).await?;
    require_same_org(&claims, Some(item.org_id))?;

    let mut_body = body.into_inner();
    let mut label_translations = mut_body.label_translations.clone().unwrap_or_else(|| serde_json::json!({}));
    if let Some(lbl) = &mut_body.label {
        crate::translation::ensure_translations_json(&mut label_translations, Some(lbl))
            .await
            .map_err(|_| AppError::Internal)?;
    }

    let row = sqlx::query_as::<_, AddonSlot>(
        "INSERT INTO menu_item_addon_slots
             (menu_item_id, addon_type, label, label_translations, is_required,
              min_selections, max_selections)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (menu_item_id, addon_type) DO UPDATE SET
             label              = COALESCE(EXCLUDED.label, menu_item_addon_slots.label),
             label_translations = EXCLUDED.label_translations,
             is_required        = EXCLUDED.is_required,
             min_selections     = EXCLUDED.min_selections,
             max_selections     = EXCLUDED.max_selections
         RETURNING id, menu_item_id, addon_type, label, label_translations, is_required,
                   min_selections, max_selections, created_at",
    )
    .bind(*id)
    .bind(&mut_body.addon_type)
    .bind(&mut_body.label)
    .bind(label_translations)
    .bind(mut_body.is_required.unwrap_or(false))
    .bind(mut_body.min_selections.unwrap_or(0))
    .bind(mut_body.max_selections)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Created().json(row))
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
    responses((status = 200, description = "Addon slot updated", body = AddonSlot), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_addon_slot(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<(Uuid, Uuid)>,
    body: web::Json<UpdateAddonSlotRequest>,
) -> Result<HttpResponse, AppError> {
    let claims              = extract_claims(&req)?;
    let (item_id, slot_id)  = path.into_inner();
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let item = fetch_menu_item(pool.get_ref(), item_id).await?;
    require_same_org(&claims, Some(item.org_id))?;

    let existing: AddonSlot = sqlx::query_as(
        "SELECT id, menu_item_id, addon_type, label, label_translations, is_required, min_selections, max_selections, created_at FROM menu_item_addon_slots WHERE id = $1 AND menu_item_id = $2"
    )
    .bind(slot_id)
    .bind(item_id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Addon slot not found".into()))?;

    let mut_body = body.into_inner();
    let mut label_translations = existing.label_translations;
    if let Some(new_label) = &mut_body.label {
        crate::translation::ensure_translations_json(&mut label_translations, Some(new_label))
            .await
            .map_err(|_| AppError::Internal)?;
    } else if let Some(new_tr) = mut_body.label_translations {
        label_translations = new_tr;
        if let Some(lbl) = &existing.label {
            crate::translation::ensure_translations_json(&mut label_translations, Some(lbl))
                .await
                .map_err(|_| AppError::Internal)?;
        }
    }

    let row = sqlx::query_as::<_, AddonSlot>(
        "UPDATE menu_item_addon_slots SET
             label              = COALESCE($3, label),
             label_translations = $4,
             is_required        = COALESCE($5, is_required),
             min_selections     = COALESCE($6, min_selections),
             max_selections     = COALESCE($7, max_selections)
         WHERE id = $1 AND menu_item_id = $2
         RETURNING id, menu_item_id, addon_type, label, label_translations, is_required,
                   min_selections, max_selections, created_at",
    )
    .bind(slot_id)
    .bind(item_id)
    .bind(&mut_body.label)
    .bind(label_translations)
    .bind(mut_body.is_required)
    .bind(mut_body.min_selections)
    .bind(mut_body.max_selections)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    delete,
    path = "/menu-items/{id}/addon-slots/{slot_id}",
    tag = "menu",
    params(
        ("id" = Uuid, Path, description = "Menu item ID"),
        ("slot_id" = Uuid, Path, description = "Addon slot ID")
    ),
    responses((status = 204, description = "Addon slot deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_addon_slot(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<HttpResponse, AppError> {
    let claims             = extract_claims(&req)?;
    let (item_id, slot_id) = path.into_inner();
    check_permission(pool.get_ref(), &claims, "menu_items", "delete").await?;

    let item = fetch_menu_item(pool.get_ref(), item_id).await?;
    require_same_org(&claims, Some(item.org_id))?;

    sqlx::query(
        "DELETE FROM menu_item_addon_slots WHERE id = $1 AND menu_item_id = $2",
    )
    .bind(slot_id)
    .bind(item_id)
    .execute(pool.get_ref())
    .await?;

    Ok(HttpResponse::NoContent().finish())
}

// ── Addon Overrides ───────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/menu-items/{id}/overrides",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item ID")),
    responses((status = 200, description = "List addon overrides", body = Vec<AddonOverride>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_addon_overrides(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;

    let item = fetch_menu_item(pool.get_ref(), *id).await?;
    require_same_org(&claims, Some(item.org_id))?;

    let rows = sqlx::query_as::<_, AddonOverride>(
        r#"
        SELECT
            o.id,
            o.menu_item_id,
            o.addon_item_id,
            ai.name                                      AS addon_item_name,
            o.size_label::text                           AS size_label,
            o.ingredient_name,
            o.org_ingredient_id,
            o.ingredient_unit,
            o.quantity_used,
            o.replaces_org_ingredient_id,
            ri.name                                      AS replaces_ingredient_name,
            o.combo_addon_item_id,
            ci.name                                      AS combo_addon_item_name,
            o.created_at,
            o.updated_at
        FROM  menu_item_addon_overrides o
        JOIN  addon_items ai ON ai.id = o.addon_item_id
        LEFT JOIN org_ingredients ri ON ri.id = o.replaces_org_ingredient_id
        LEFT JOIN addon_items      ci ON ci.id = o.combo_addon_item_id
        WHERE o.menu_item_id = $1
        ORDER BY ai.name ASC, o.size_label ASC NULLS FIRST,
                 o.ingredient_name ASC, o.combo_addon_item_id ASC NULLS FIRST
        "#,
    )
    .bind(*id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    post,
    path = "/menu-items/{id}/overrides",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item ID")),
    request_body = UpsertAddonOverrideRequest,
    responses((status = 200, description = "Addon override upserted", body = AddonOverride), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn upsert_addon_override(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
    body: web::Json<UpsertAddonOverrideRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let item = fetch_menu_item(pool.get_ref(), *id).await?;
    require_same_org(&claims, Some(item.org_id))?;

    // Verify the addon_item belongs to the same org
    let addon = fetch_addon_item(pool.get_ref(), body.addon_item_id).await?;
    require_same_org(&claims, Some(addon.org_id))?;

    if body.quantity_used <= 0.0 {
        return Err(AppError::BadRequest("quantity_used must be greater than 0".into()));
    }
    // Normalize the deduction to the replacement ingredient's base unit.
    let (norm_unit, norm_qty) = crate::recipes::handlers::normalize_recipe_unit(
        pool.get_ref(), addon.org_id, body.org_ingredient_id, &body.ingredient_unit, body.quantity_used,
    ).await?;

    // Upsert using the appropriate partial unique index path.
    // Because size_label and combo_addon_item_id are nullable we can't use
    // a single ON CONFLICT clause — instead we do a manual upsert:
    // try UPDATE first, INSERT if no row matched.
    let existing_id: Option<Uuid> = match (body.size_label.as_deref(), body.combo_addon_item_id) {
        (Some(size), Some(combo)) => sqlx::query_scalar(
            "SELECT id FROM menu_item_addon_overrides
             WHERE menu_item_id = $1 AND addon_item_id = $2
               AND ingredient_name = $3
               AND size_label = $4::item_size
               AND combo_addon_item_id = $5",
        )
        .bind(*id).bind(body.addon_item_id).bind(&body.ingredient_name)
        .bind(size).bind(combo)
        .fetch_optional(pool.get_ref()).await?,

        (Some(size), None) => sqlx::query_scalar(
            "SELECT id FROM menu_item_addon_overrides
             WHERE menu_item_id = $1 AND addon_item_id = $2
               AND ingredient_name = $3
               AND size_label = $4::item_size
               AND combo_addon_item_id IS NULL",
        )
        .bind(*id).bind(body.addon_item_id).bind(&body.ingredient_name)
        .bind(size)
        .fetch_optional(pool.get_ref()).await?,

        (None, Some(combo)) => sqlx::query_scalar(
            "SELECT id FROM menu_item_addon_overrides
             WHERE menu_item_id = $1 AND addon_item_id = $2
               AND ingredient_name = $3
               AND size_label IS NULL
               AND combo_addon_item_id = $4",
        )
        .bind(*id).bind(body.addon_item_id).bind(&body.ingredient_name)
        .bind(combo)
        .fetch_optional(pool.get_ref()).await?,

        (None, None) => sqlx::query_scalar(
            "SELECT id FROM menu_item_addon_overrides
             WHERE menu_item_id = $1 AND addon_item_id = $2
               AND ingredient_name = $3
               AND size_label IS NULL
               AND combo_addon_item_id IS NULL",
        )
        .bind(*id).bind(body.addon_item_id).bind(&body.ingredient_name)
        .fetch_optional(pool.get_ref()).await?,
    }
    .flatten();

    let row = if let Some(eid) = existing_id {
        sqlx::query_as::<_, AddonOverride>(
            r#"
            UPDATE menu_item_addon_overrides SET
                org_ingredient_id          = $2,
                ingredient_unit            = $3,
                quantity_used              = $4,
                replaces_org_ingredient_id = $5,
                updated_at                 = NOW()
            WHERE id = $1
            RETURNING
                id, menu_item_id, addon_item_id,
                (SELECT name FROM addon_items WHERE id = addon_item_id) AS addon_item_name,
                size_label::text,
                ingredient_name, org_ingredient_id, ingredient_unit, quantity_used,
                replaces_org_ingredient_id,
                (SELECT name FROM org_ingredients WHERE id = replaces_org_ingredient_id)
                    AS replaces_ingredient_name,
                combo_addon_item_id,
                (SELECT name FROM addon_items WHERE id = combo_addon_item_id)
                    AS combo_addon_item_name,
                created_at, updated_at
            "#,
        )
        .bind(eid)
        .bind(body.org_ingredient_id)
        .bind(&norm_unit)
        .bind(norm_qty)
        .bind(body.replaces_org_ingredient_id)
        .fetch_one(pool.get_ref())
        .await?
    } else {
        sqlx::query_as::<_, AddonOverride>(
            r#"
            INSERT INTO menu_item_addon_overrides
                (menu_item_id, addon_item_id, size_label, ingredient_name,
                 org_ingredient_id, ingredient_unit, quantity_used,
                 replaces_org_ingredient_id, combo_addon_item_id)
            VALUES ($1, $2, $3::item_size, $4, $5, $6, $7, $8, $9)
            RETURNING
                id, menu_item_id, addon_item_id,
                (SELECT name FROM addon_items WHERE id = $2) AS addon_item_name,
                size_label::text,
                ingredient_name, org_ingredient_id, ingredient_unit, quantity_used,
                replaces_org_ingredient_id,
                (SELECT name FROM org_ingredients WHERE id = $8)
                    AS replaces_ingredient_name,
                combo_addon_item_id,
                (SELECT name FROM addon_items WHERE id = $9)
                    AS combo_addon_item_name,
                created_at, updated_at
            "#,
        )
        .bind(*id)
        .bind(body.addon_item_id)
        .bind(&body.size_label)
        .bind(&body.ingredient_name)
        .bind(body.org_ingredient_id)
        .bind(&norm_unit)
        .bind(norm_qty)
        .bind(body.replaces_org_ingredient_id)
        .bind(body.combo_addon_item_id)
        .fetch_one(pool.get_ref())
        .await?
    };

    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    delete,
    path = "/menu-items/{id}/overrides/{override_id}",
    tag = "menu",
    params(
        ("id" = Uuid, Path, description = "Menu item ID"),
        ("override_id" = Uuid, Path, description = "Override ID")
    ),
    responses((status = 204, description = "Addon override deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_addon_override(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<HttpResponse, AppError> {
    let claims                  = extract_claims(&req)?;
    let (item_id, override_id)  = path.into_inner();
    check_permission(pool.get_ref(), &claims, "menu_items", "delete").await?;

    let item = fetch_menu_item(pool.get_ref(), item_id).await?;
    require_same_org(&claims, Some(item.org_id))?;

    sqlx::query(
        "DELETE FROM menu_item_addon_overrides WHERE id = $1 AND menu_item_id = $2",
    )
    .bind(override_id)
    .bind(item_id)
    .execute(pool.get_ref())
    .await?;

    Ok(HttpResponse::NoContent().finish())
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
    responses((status = 200, description = "Allowed addon IDs replaced", body = Vec<String>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_allowed_addons(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
    body: web::Json<PutAllowedAddonsRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let item = fetch_menu_item(pool.get_ref(), *id).await?;
    require_same_org(&claims, Some(item.org_id))?;

    // Validate every addon belongs to the same org.
    for addon_id in &body.addon_item_ids {
        let addon = fetch_addon_item(pool.get_ref(), *addon_id).await?;
        require_same_org(&claims, Some(addon.org_id))?;
    }

    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM menu_item_allowed_addons WHERE menu_item_id = $1")
        .bind(*id)
        .execute(&mut *tx)
        .await?;

    for (i, addon_id) in body.addon_item_ids.iter().enumerate() {
        sqlx::query(
            "INSERT INTO menu_item_allowed_addons (menu_item_id, addon_item_id, sort_order)
             VALUES ($1, $2, $3)",
        )
        .bind(*id)
        .bind(addon_id)
        .bind(i as i32)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    Ok(HttpResponse::Ok().json(&body.addon_item_ids))
}

// ═══════════════════════════════════════════════════════════════
// OPTIONAL FIELDS
// ═══════════════════════════════════════════════════════════════

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct OptionalField {
    pub id:                Uuid,
    pub menu_item_id:      Uuid,
    pub name:              String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub price:             i32,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name:   Option<String>,
    pub ingredient_unit:   Option<String>,
    #[schema(value_type = Option<f64>)]
    pub quantity_used:     Option<sqlx::types::BigDecimal>,
    pub size_label:        Option<String>,
    pub is_active:         bool,
    pub created_at:        DateTime<Utc>,
    pub updated_at:        DateTime<Utc>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateOptionalFieldRequest {
    pub name:              String,
    #[schema(value_type = Option<Object>)]
    pub name_translations: Option<serde_json::Value>,
    pub price:             Option<i32>,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name:   Option<String>,
    pub ingredient_unit:   Option<String>,
    pub quantity_used:     Option<f64>,
    pub size_label:        Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateOptionalFieldRequest {
    pub name:              Option<String>,
    #[schema(value_type = Option<Object>)]
    pub name_translations: Option<serde_json::Value>,
    pub price:             Option<i32>,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name:   Option<String>,
    pub ingredient_unit:   Option<String>,
    pub quantity_used:     Option<f64>,
    pub size_label:        Option<String>,
    pub is_active:         Option<bool>,
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
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
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
        FROM menu_item_optional_fields
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
    responses((status = 201, description = "Optional field created", body = OptionalField), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_optional_field(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
    body: web::Json<CreateOptionalFieldRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    let item = fetch_menu_item(pool.get_ref(), *id).await?;
    require_same_org(&claims, Some(item.org_id))?;

    if body.name.trim().is_empty() {
        return Err(AppError::BadRequest("name cannot be empty".into()));
    }

    // Validate: if any ingredient field is set, all required ones must be present
    let has_ingredient = body.org_ingredient_id.is_some()
        || body.ingredient_name.is_some()
        || body.ingredient_unit.is_some()
        || body.quantity_used.is_some();

    if has_ingredient {
        if body.ingredient_name.is_none() || body.ingredient_unit.is_none() || body.quantity_used.is_none() {
            return Err(AppError::BadRequest(
                "ingredient_name, ingredient_unit, and quantity_used are all required when configuring an ingredient deduction".into()
            ));
        }
        if let Some(qty) = body.quantity_used
            && qty <= 0.0 {
                return Err(AppError::BadRequest("quantity_used must be greater than 0".into()));
            }
    }

    let mut mut_body = body.into_inner();
    // Normalize the deduction quantity to the linked ingredient's base unit.
    if has_ingredient {
        let unit = mut_body.ingredient_unit.clone().unwrap_or_default();
        let qty  = mut_body.quantity_used.unwrap_or(0.0);
        let (nu, nq) = crate::recipes::handlers::normalize_recipe_unit(
            pool.get_ref(), item.org_id, mut_body.org_ingredient_id, &unit, qty,
        ).await?;
        mut_body.ingredient_unit = Some(nu);
        mut_body.quantity_used   = Some(nq);
    }
    let trimmed_name = mut_body.name.trim().to_string();
    let mut name_translations = mut_body.name_translations.unwrap_or_else(|| serde_json::json!({}));
    crate::translation::ensure_translations_json(&mut name_translations, Some(&trimmed_name))
        .await
        .map_err(|_| AppError::Internal)?;

    let row = sqlx::query_as::<_, OptionalField>(
        r#"
        INSERT INTO menu_item_optional_fields
            (menu_item_id, name, name_translations, price, org_ingredient_id, ingredient_name,
             ingredient_unit, quantity_used, size_label)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::item_size)
        RETURNING id, menu_item_id, name, name_translations, price,
                  org_ingredient_id, ingredient_name, ingredient_unit,
                  quantity_used, size_label::text,
                  is_active, created_at, updated_at
        "#,
    )
    .bind(*id)
    .bind(trimmed_name)
    .bind(name_translations)
    .bind(mut_body.price.unwrap_or(0))
    .bind(mut_body.org_ingredient_id)
    .bind(&mut_body.ingredient_name)
    .bind(&mut_body.ingredient_unit)
    .bind(mut_body.quantity_used)
    .bind(&mut_body.size_label)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Created().json(row))
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
    responses((status = 200, description = "Optional field updated", body = OptionalField), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_optional_field(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<(Uuid, Uuid)>,
    body: web::Json<UpdateOptionalFieldRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    let (item_id, field_id) = path.into_inner();
    let item = fetch_menu_item(pool.get_ref(), item_id).await?;
    require_same_org(&claims, Some(item.org_id))?;

    if let Some(qty) = body.quantity_used
        && qty <= 0.0 {
            return Err(AppError::BadRequest("quantity_used must be greater than 0".into()));
        }

    let existing: OptionalField = sqlx::query_as(
        r#"SELECT id, menu_item_id, name, name_translations, price, org_ingredient_id, ingredient_name, ingredient_unit, quantity_used, size_label::text, is_active, created_at, updated_at FROM menu_item_optional_fields WHERE id = $1 AND menu_item_id = $2"#
    )
    .bind(field_id)
    .bind(item_id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Optional field not found".into()))?;

    // Changing the linked ingredient OR its unit reinterprets the stored
    // (base-unit) quantity, so a fresh quantity_used must accompany either
    // change — otherwise the old amount is silently re-read in the new unit (V21).
    if body.quantity_used.is_none()
        && (body.ingredient_unit.is_some()
            || (body.org_ingredient_id.is_some()
                && body.org_ingredient_id != existing.org_ingredient_id))
    {
        return Err(AppError::BadRequest(
            "provide quantity_used when changing the linked ingredient or its unit".into(),
        ));
    }

    let mut mut_body = body.into_inner();
    // Normalize a newly-supplied quantity to the linked ingredient's base unit.
    if let Some(qty) = mut_body.quantity_used {
        let effective_id = mut_body.org_ingredient_id.or(existing.org_ingredient_id);
        let unit = mut_body.ingredient_unit.clone()
            .or_else(|| existing.ingredient_unit.clone())
            .unwrap_or_default();
        let (nu, nq) = crate::recipes::handlers::normalize_recipe_unit(
            pool.get_ref(), item.org_id, effective_id, &unit, qty,
        ).await?;
        mut_body.ingredient_unit = Some(nu);
        mut_body.quantity_used   = Some(nq);
    }
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

    let row = sqlx::query_as::<_, OptionalField>(
        r#"
        UPDATE menu_item_optional_fields SET
            name              = COALESCE($3, name),
            name_translations = $4,
            price             = COALESCE($5, price),
            org_ingredient_id = COALESCE($6, org_ingredient_id),
            ingredient_name   = COALESCE($7, ingredient_name),
            ingredient_unit   = COALESCE($8, ingredient_unit),
            quantity_used     = COALESCE($9, quantity_used),
            size_label        = COALESCE($10::item_size, size_label),
            is_active         = COALESCE($11, is_active)
        WHERE id = $1 AND menu_item_id = $2
        RETURNING id, menu_item_id, name, name_translations, price,
                  org_ingredient_id, ingredient_name, ingredient_unit,
                  quantity_used, size_label::text,
                  is_active, created_at, updated_at
        "#,
    )
    .bind(field_id)
    .bind(item_id)
    .bind(&mut_body.name)
    .bind(name_translations)
    .bind(mut_body.price)
    .bind(mut_body.org_ingredient_id)
    .bind(&mut_body.ingredient_name)
    .bind(&mut_body.ingredient_unit)
    .bind(mut_body.quantity_used)
    .bind(&mut_body.size_label)
    .bind(mut_body.is_active)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    delete,
    path = "/menu-items/{id}/optionals/{field_id}",
    tag = "menu",
    params(
        ("id" = Uuid, Path, description = "Menu item ID"),
        ("field_id" = Uuid, Path, description = "Field ID")
    ),
    responses((status = 204, description = "Optional field deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_optional_field(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "delete").await?;
    let (item_id, field_id) = path.into_inner();
    let item = fetch_menu_item(pool.get_ref(), item_id).await?;
    require_same_org(&claims, Some(item.org_id))?;

    sqlx::query(
        "DELETE FROM menu_item_optional_fields WHERE id = $1 AND menu_item_id = $2"
    )
    .bind(field_id)
    .bind(item_id)
    .execute(pool.get_ref())
    .await?;

    Ok(HttpResponse::NoContent().finish())
}

// ── Branch menu override handlers ─────────────────────────────

/// Resolve a branch's org and assert the caller is scoped to it. Returns the org_id.
async fn branch_in_scope(pool: &PgPool, claims: &Claims, branch_id: Uuid) -> Result<Uuid, AppError> {
    let org_id: Uuid = sqlx::query_scalar(
        "SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL",
    )
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
         FROM branch_menu_size_overrides
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
    let map: std::collections::HashMap<String, i32> =
        overrides.into_iter().map(|o| (o.size_label, o.price_override)).collect();
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
    req:   HttpRequest,
    pool:  web::Data<PgPool>,
    query: web::Query<BranchOverridesQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    branch_in_scope(pool.get_ref(), &claims, query.branch_id).await?;

    let mut rows = sqlx::query_as::<_, BranchMenuOverride>(
        "SELECT branch_id, menu_item_id, price_override, is_available, updated_at
         FROM branch_menu_overrides WHERE branch_id = $1",
    )
    .bind(query.branch_id)
    .fetch_all(pool.get_ref())
    .await?;

    // Attach each item's size overrides (single round trip, grouped by item).
    let size_rows = sqlx::query_as::<_, (Uuid, String, i32)>(
        "SELECT menu_item_id, size_label::text, price_override
         FROM branch_menu_size_overrides WHERE branch_id = $1",
    )
    .bind(query.branch_id)
    .fetch_all(pool.get_ref())
    .await?;
    let mut by_item: std::collections::HashMap<Uuid, Vec<BranchSizeOverride>> =
        std::collections::HashMap::new();
    for (item_id, size_label, price_override) in size_rows {
        by_item.entry(item_id).or_default().push(BranchSizeOverride { size_label, price_override });
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
    responses((status = 200, description = "Override upserted", body = BranchMenuOverride), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn upsert_branch_menu_override(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<BranchMenuOverrideInput>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    let org_id = branch_in_scope(pool.get_ref(), &claims, body.branch_id).await?;

    // The item must belong to the same org as the branch.
    let item_org: Option<Uuid> = sqlx::query_scalar(
        "SELECT org_id FROM menu_items WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(body.menu_item_id)
    .fetch_optional(pool.get_ref())
    .await?;
    if item_org != Some(org_id) {
        return Err(AppError::NotFound("Menu item not found in this organization".into()));
    }
    if let Some(p) = body.price_override
        && p < 0
    {
        return Err(AppError::BadRequest("price_override must be ≥ 0".into()));
    }

    // Validate any provided size overrides against the item's actual (active) sizes.
    if let Some(ref sizes) = body.sizes
        && !sizes.is_empty()
    {
        for s in sizes {
            if s.price_override < 0 {
                return Err(AppError::BadRequest("size price_override must be ≥ 0".into()));
            }
        }
        let valid: Vec<String> = sqlx::query_scalar(
            "SELECT label::text FROM item_sizes WHERE menu_item_id = $1 AND is_active = true",
        )
        .bind(body.menu_item_id)
        .fetch_all(pool.get_ref())
        .await?;
        for s in sizes {
            if !valid.contains(&s.size_label) {
                return Err(AppError::BadRequest(format!(
                    "Size '{}' is not a valid size for this item",
                    s.size_label
                )));
            }
        }
    }

    let mut tx = pool.get_ref().begin().await?;

    let mut row = sqlx::query_as::<_, BranchMenuOverride>(
        "INSERT INTO branch_menu_overrides (branch_id, menu_item_id, price_override, is_available, updated_at)
         VALUES ($1, $2, $3, $4, now())
         ON CONFLICT (branch_id, menu_item_id)
         DO UPDATE SET price_override = EXCLUDED.price_override,
                       is_available   = EXCLUDED.is_available,
                       updated_at     = now()
         RETURNING branch_id, menu_item_id, price_override, is_available, updated_at",
    )
    .bind(body.branch_id)
    .bind(body.menu_item_id)
    .bind(body.price_override)
    .bind(body.is_available)
    .fetch_one(&mut *tx)
    .await?;

    // `sizes` is a full-replacement of this item's size overrides (None leaves them as-is).
    if let Some(ref sizes) = body.sizes {
        sqlx::query("DELETE FROM branch_menu_size_overrides WHERE branch_id = $1 AND menu_item_id = $2")
            .bind(body.branch_id)
            .bind(body.menu_item_id)
            .execute(&mut *tx)
            .await?;
        for s in sizes {
            sqlx::query(
                "INSERT INTO branch_menu_size_overrides (branch_id, menu_item_id, size_label, price_override, updated_at)
                 VALUES ($1, $2, $3::item_size, $4, now())",
            )
            .bind(body.branch_id)
            .bind(body.menu_item_id)
            .bind(&s.size_label)
            .bind(s.price_override)
            .execute(&mut *tx)
            .await?;
        }
    }

    tx.commit().await?;

    row.sizes = fetch_branch_size_overrides(pool.get_ref(), body.branch_id, body.menu_item_id).await?;
    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    delete,
    path = "/branch-menu-overrides",
    tag = "menu",
    operation_id = "delete_branch_menu_override",
    params(BranchOverrideKeyQuery),
    responses((status = 204, description = "Override cleared — item (and its size overrides) revert to the org catalog"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_branch_menu_override(
    req:   HttpRequest,
    pool:  web::Data<PgPool>,
    query: web::Query<BranchOverrideKeyQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    branch_in_scope(pool.get_ref(), &claims, query.branch_id).await?;

    let mut tx = pool.get_ref().begin().await?;
    sqlx::query("DELETE FROM branch_menu_size_overrides WHERE branch_id = $1 AND menu_item_id = $2")
        .bind(query.branch_id)
        .bind(query.menu_item_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM branch_menu_overrides WHERE branch_id = $1 AND menu_item_id = $2")
        .bind(query.branch_id)
        .bind(query.menu_item_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(HttpResponse::NoContent().finish())
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
    req:   HttpRequest,
    pool:  web::Data<PgPool>,
    query: web::Query<BranchOverridesQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    branch_in_scope(pool.get_ref(), &claims, query.branch_id).await?;

    let rows = sqlx::query_as::<_, BranchAddonOverride>(
        "SELECT branch_id, addon_item_id, price_override, is_available, updated_at
         FROM branch_addon_overrides WHERE branch_id = $1",
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
    responses((status = 200, description = "Addon override upserted", body = BranchAddonOverride), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn upsert_branch_addon_override(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<BranchAddonOverrideInput>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    let org_id = branch_in_scope(pool.get_ref(), &claims, body.branch_id).await?;

    // The addon must belong to the same org as the branch.
    let addon_org: Option<Uuid> = sqlx::query_scalar(
        "SELECT org_id FROM addon_items WHERE id = $1",
    )
    .bind(body.addon_item_id)
    .fetch_optional(pool.get_ref())
    .await?;
    if addon_org != Some(org_id) {
        return Err(AppError::NotFound("Addon not found in this organization".into()));
    }
    if let Some(p) = body.price_override
        && p < 0
    {
        return Err(AppError::BadRequest("price_override must be ≥ 0".into()));
    }

    let row = sqlx::query_as::<_, BranchAddonOverride>(
        "INSERT INTO branch_addon_overrides (branch_id, addon_item_id, price_override, is_available, updated_at)
         VALUES ($1, $2, $3, $4, now())
         ON CONFLICT (branch_id, addon_item_id)
         DO UPDATE SET price_override = EXCLUDED.price_override,
                       is_available   = EXCLUDED.is_available,
                       updated_at     = now()
         RETURNING branch_id, addon_item_id, price_override, is_available, updated_at",
    )
    .bind(body.branch_id)
    .bind(body.addon_item_id)
    .bind(body.price_override)
    .bind(body.is_available)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    delete,
    path = "/branch-addon-overrides",
    tag = "menu",
    operation_id = "delete_branch_addon_override",
    params(BranchAddonOverrideKeyQuery),
    responses((status = 204, description = "Addon override cleared — reverts to the org default"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_branch_addon_override(
    req:   HttpRequest,
    pool:  web::Data<PgPool>,
    query: web::Query<BranchAddonOverrideKeyQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    branch_in_scope(pool.get_ref(), &claims, query.branch_id).await?;

    sqlx::query("DELETE FROM branch_addon_overrides WHERE branch_id = $1 AND addon_item_id = $2")
        .bind(query.branch_id)
        .bind(query.addon_item_id)
        .execute(pool.get_ref())
        .await?;

    Ok(HttpResponse::NoContent().finish())
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
        "SELECT id, org_id, name, name_translations, image_url, is_active,
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
                created_at, updated_at, deleted_at,
                (
                    SELECT a.id::text
                    FROM menu_item_recipes r
                    JOIN addon_item_ingredients ai ON ai.org_ingredient_id = r.org_ingredient_id
                    JOIN addon_items a ON a.id = ai.addon_item_id
                    WHERE r.menu_item_id = menu_items.id
                      AND a.type = 'milk_type'
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

async fn fetch_addon_item(pool: &PgPool, id: Uuid) -> Result<AddonItem, AppError> {
    sqlx::query_as::<_, AddonItem>(
        "SELECT id, org_id, name, name_translations, type as addon_type, default_price,
                is_active, created_at, updated_at,
                (SELECT org_ingredient_id
                   FROM addon_item_ingredients
                  WHERE addon_item_id = addon_items.id
                  LIMIT 1) AS primary_ingredient_id
         FROM addon_items
         WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Addon item not found".into()))
}

async fn fetch_sizes(pool: &PgPool, item_id: Uuid) -> Result<Vec<ItemSize>, AppError> {
    Ok(sqlx::query_as::<_, ItemSize>(
        "SELECT id, menu_item_id, label::text, price_override, is_active
         FROM item_sizes
         WHERE menu_item_id = $1
         ORDER BY label ASC",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await?)
}

async fn fetch_addon_slots(
    pool:    &PgPool,
    item_id: Uuid,
) -> Result<Vec<AddonSlot>, AppError> {
    Ok(sqlx::query_as::<_, AddonSlot>(
        "SELECT id, menu_item_id, addon_type, label, label_translations, is_required,
                min_selections, max_selections, created_at
         FROM menu_item_addon_slots
         WHERE menu_item_id = $1
         ORDER BY created_at ASC",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await?)
}

async fn fetch_allowed_addon_ids(
    pool:    &PgPool,
    item_id: Uuid,
) -> Result<Vec<Uuid>, AppError> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT addon_item_id FROM menu_item_allowed_addons
         WHERE menu_item_id = $1
         ORDER BY sort_order ASC, created_at ASC",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

async fn fetch_optional_fields(
    pool:    &PgPool,
    item_id: Uuid,
) -> Result<Vec<OptionalField>, AppError> {
    Ok(sqlx::query_as::<_, OptionalField>(
        "SELECT id, menu_item_id, name, name_translations, price,
                org_ingredient_id, ingredient_name, ingredient_unit,
                quantity_used, size_label::text,
                is_active, created_at, updated_at
         FROM menu_item_optional_fields
         WHERE menu_item_id = $1 AND is_active = true
         ORDER BY name ASC",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await?)
}

async fn fetch_item_recipes(
    pool:    &PgPool,
    item_id: Uuid,
) -> Result<Vec<MenuItemRecipe>, AppError> {
    Ok(sqlx::query_as::<_, MenuItemRecipe>(
        r#"SELECT r.org_ingredient_id, r.quantity_used,
                  r.ingredient_name, r.ingredient_unit,
                  COALESCE(i.category, 'general') as category,
                  r.size_label::text
           FROM   menu_item_recipes r
           LEFT JOIN org_ingredients i ON i.id = r.org_ingredient_id
           WHERE  r.menu_item_id = $1"#,
    )
    .bind(item_id)
    .fetch_all(pool)
    .await?)
}

async fn fetch_addon_ingredients(
    pool:          &PgPool,
    addon_item_id: Uuid,
) -> Result<Vec<AddonItemIngredient>, AppError> {
    Ok(sqlx::query_as::<_, AddonItemIngredient>(
        "SELECT org_ingredient_id, quantity_used, ingredient_name, ingredient_unit
         FROM   addon_item_ingredients
         WHERE  addon_item_id = $1",
    )
    .bind(addon_item_id)
    .fetch_all(pool)
    .await?)
}

pub (crate) fn deserialize_double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::deserialize(deserializer).map(Some)
}