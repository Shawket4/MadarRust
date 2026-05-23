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
    #[serde(serialize_with = "crate::uploads::handlers::serialize_opt_url")]
    pub image_url:     Option<String>,
    pub display_order: i32,
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
    pub description:   Option<String>,
    #[serde(serialize_with = "crate::uploads::handlers::serialize_opt_url")]
    pub image_url:     Option<String>,
    pub base_price:    i32,
    pub is_active:     bool,
    pub display_order: i32,
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
    pub display_order:  i32,
    pub is_active:      bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct AddonItem {
    pub id:            Uuid,
    pub org_id:        Uuid,
    pub name:          String,
    pub addon_type:    String,
    pub default_price: i32,
    pub is_active:     bool,
    pub display_order: i32,
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
    pub is_required:    bool,
    pub min_selections: i32,
    pub max_selections: Option<i32>,
    pub display_order:  i32,
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
    pub item:            MenuItem,
    pub sizes:           Vec<ItemSize>,
    pub addon_slots:     Vec<AddonSlot>,
    pub optional_fields: Vec<OptionalField>,
    pub recipes:         Vec<MenuItemRecipe>,
}

// ── Public Menu Models ────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, ToSchema)]
pub struct PublicMenuResponse {
    pub org_id:     Uuid,
    pub org_name:   String,
    #[serde(serialize_with = "crate::uploads::handlers::serialize_opt_url")]
    pub logo_url:   Option<String>,
    pub categories: Vec<PublicCategory>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, ToSchema)]
pub struct PublicCategory {
    pub id:            Uuid,
    pub name:          String,
    #[serde(serialize_with = "crate::uploads::handlers::serialize_opt_url")]
    pub image_url:     Option<String>,
    pub display_order: i32,
    pub items:         Vec<PublicMenuItem>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, ToSchema)]
pub struct PublicMenuItem {
    pub id:            Uuid,
    pub name:          String,
    pub description:   Option<String>,
    #[serde(serialize_with = "crate::uploads::handlers::serialize_opt_url")]
    pub image_url:     Option<String>,
    pub base_price:    i32,
    pub display_order: i32,
    pub sizes:         Vec<PublicItemSize>,
    pub addon_slots:   Vec<PublicAddonSlot>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, ToSchema)]
pub struct PublicItemSize {
    pub id:             Uuid,
    pub label:          String,
    pub price_override: i32,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, ToSchema)]
pub struct PublicAddonSlot {
    pub id:             Uuid,
    pub addon_type:     String,
    pub label:          Option<String>,
    pub is_required:    bool,
    pub min_selections: i32,
    pub max_selections: Option<i32>,
    pub addon_items:    Vec<PublicAddonItem>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, ToSchema)]
pub struct PublicAddonItem {
    pub id:            Uuid,
    pub name:          String,
    pub default_price: i32,
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
    pub full:        Option<bool>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AddonItemQuery {
    pub org_id:     Uuid,
    pub addon_type: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateCategoryRequest {
    pub org_id:        Uuid,
    pub name:          String,
    pub image_url:     Option<String>,
    pub display_order: Option<i32>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateCategoryRequest {
    pub name:          Option<String>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub image_url:     Option<Option<String>>,
    pub display_order: Option<i32>,
    pub is_active:     Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateMenuItemRequest {
    pub org_id:        Uuid,
    pub category_id:   Uuid,
    pub name:          String,
    pub description:   Option<String>,
    pub image_url:     Option<String>,
    pub base_price:    i32,
    pub display_order: Option<i32>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateMenuItemRequest {
    pub category_id:   Option<Uuid>,
    pub name:          Option<String>,
    pub description:   Option<String>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub image_url:     Option<Option<String>>,
    pub base_price:    Option<i32>,
    pub display_order: Option<i32>,
    pub is_active:     Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateAddonItemRequest {
    pub org_id:        Uuid,
    pub name:          String,
    pub addon_type:    String,
    pub default_price: i32,
    pub display_order: Option<i32>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateAddonItemRequest {
    pub name:          Option<String>,
    pub addon_type:    Option<String>,
    pub default_price: Option<i32>,
    pub display_order: Option<i32>,
    pub is_active:     Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpsertSizeRequest {
    pub label:          String,
    pub price_override: i32,
    pub display_order:  Option<i32>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateAddonSlotRequest {
    pub addon_type:     String,
    pub label:          Option<String>,
    pub is_required:    Option<bool>,
    pub min_selections: Option<i32>,
    pub max_selections: Option<i32>,
    pub display_order:  Option<i32>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateAddonSlotRequest {
    pub label:          Option<String>,
    pub is_required:    Option<bool>,
    pub min_selections: Option<i32>,
    pub max_selections: Option<i32>,
    pub display_order:  Option<i32>,
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
        "SELECT id, org_id, name, image_url, display_order, is_active,
                created_at, updated_at, deleted_at
         FROM categories
         WHERE org_id = $1 AND deleted_at IS NULL
         ORDER BY display_order ASC, name ASC",
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

    let row = sqlx::query_as::<_, Category>(
        "INSERT INTO categories (org_id, name, image_url, display_order)
         VALUES ($1, $2, $3, $4)
         RETURNING id, org_id, name, image_url, display_order, is_active,
                   created_at, updated_at, deleted_at",
    )
    .bind(body.org_id)
    .bind(&body.name)
    .bind(&body.image_url)
    .bind(body.display_order.unwrap_or(0))
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

    let image_url_is_present = body.image_url.is_some();
    let image_url_val = body.image_url.as_ref().and_then(|o| o.clone());

    let row = sqlx::query_as::<_, Category>(
        "UPDATE categories SET
             name          = COALESCE($2, name),
             image_url     = CASE WHEN $6 THEN $3 ELSE image_url END,
             display_order = COALESCE($4, display_order),
             is_active     = COALESCE($5, is_active)
         WHERE id = $1 AND deleted_at IS NULL
         RETURNING id, org_id, name, image_url, display_order, is_active,
                   created_at, updated_at, deleted_at",
    )
    .bind(*id)
    .bind(&body.name)
    .bind(image_url_val)
    .bind(body.display_order)
    .bind(body.is_active)
    .bind(image_url_is_present)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Category not found".into()))?;

    // If explicit null, cleanup old image from storage
    if body.image_url == Some(None)
        && let Some(old_url) = existing.image_url {
            let uploads_dir = std::env::var("UPLOADS_DIR").unwrap_or_else(|_| "./uploads".to_string());
            let base_url    = std::env::var("UPLOADS_BASE_URL").unwrap_or_default();
            delete_old_image(&old_url, &base_url, &uploads_dir).await;
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
    params(MenuItemQuery),
    responses((status = 200, description = "List menu items", body = Vec<MenuItemFull>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_menu_items(
    req:   HttpRequest,
    pool:  web::Data<PgPool>,
    query: web::Query<MenuItemQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    require_same_org(&claims, Some(query.org_id))?;

    let items = match query.category_id {
        Some(cat_id) => sqlx::query_as::<_, MenuItem>(
            "SELECT id, org_id, category_id, name, description, image_url,
                    base_price, is_active, display_order,
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
             WHERE org_id = $1 AND deleted_at IS NULL AND category_id = $2
             ORDER BY display_order ASC, name ASC",
        )
        .bind(query.org_id)
        .bind(cat_id)
        .fetch_all(pool.get_ref())
        .await?,

        None => sqlx::query_as::<_, MenuItem>(
            "SELECT id, org_id, category_id, name, description, image_url,
                    base_price, is_active, display_order,
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
             WHERE org_id = $1 AND deleted_at IS NULL
             ORDER BY display_order ASC, name ASC",
        )
        .bind(query.org_id)
        .fetch_all(pool.get_ref())
        .await?,
    };

    // ?full=true embeds sizes + addon_slots for each item
    if query.full.unwrap_or(false) {
        let mut result: Vec<MenuItemFull> = vec![];
        for item in items {
            let sizes = fetch_sizes(pool.get_ref(), item.id).await?;
            let addon_slots = fetch_addon_slots(pool.get_ref(), item.id).await?;
            let optional_fields = fetch_optional_fields(pool.get_ref(), item.id).await?;
            let recipes = fetch_item_recipes(pool.get_ref(), item.id).await?;
            result.push(MenuItemFull { item, sizes, addon_slots, optional_fields, recipes });
        }
        return Ok(HttpResponse::Ok().json(result));
    }

    Ok(HttpResponse::Ok().json(items))
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

    let sizes       = fetch_sizes(pool.get_ref(), *id).await?;
    let addon_slots = fetch_addon_slots(pool.get_ref(), *id).await?;
    let optional_fields = fetch_optional_fields(pool.get_ref(), *id).await?;
    let recipes = fetch_item_recipes(pool.get_ref(), *id).await?;

    Ok(HttpResponse::Ok().json(MenuItemFull { item, sizes, addon_slots, optional_fields, recipes }))
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

    let mut tx = pool.get_ref().begin().await?;

    let item = sqlx::query_as::<_, MenuItem>(
        "INSERT INTO menu_items
             (org_id, category_id, name, description, image_url, base_price, display_order)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING id, org_id, category_id, name, description, image_url,
                   base_price, is_active, display_order,
                   created_at, updated_at, deleted_at,
                   NULL::text AS default_milk_addon_id",
    )
    .bind(body.org_id)
    .bind(body.category_id)
    .bind(&body.name)
    .bind(&body.description)
    .bind(&body.image_url)
    .bind(body.base_price)
    .bind(body.display_order.unwrap_or(0))
    .fetch_one(&mut *tx)
    .await?;

    // Seed initial price epoch for the advisor.
    sqlx::query(
        "INSERT INTO menu_item_price_epochs \
             (menu_item_id, size_label, price, effective_from, changed_by) \
         VALUES ($1, NULL, $2, now(), $3)"
    )
    .bind(item.id)
    .bind(body.base_price)
    .bind(claims.user_id())
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(HttpResponse::Created().json(MenuItemFull {
        item,
        sizes:           vec![],
        addon_slots:     vec![],
        optional_fields: vec![],
        recipes:         vec![],
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

    let image_url_is_present = body.image_url.is_some();
    let image_url_val = body.image_url.as_ref().and_then(|o| o.clone());

    let mut tx = pool.get_ref().begin().await?;

    let item = sqlx::query_as::<_, MenuItem>(
        "UPDATE menu_items SET
             category_id   = COALESCE($2, category_id),
             name          = COALESCE($3, name),
             description   = COALESCE($4, description),
             image_url     = CASE WHEN $9 THEN $5 ELSE image_url END,
             base_price    = COALESCE($6, base_price),
             display_order = COALESCE($7, display_order),
             is_active     = COALESCE($8, is_active)
         WHERE id = $1 AND deleted_at IS NULL
         RETURNING id, org_id, category_id, name, description, image_url,
                   base_price, is_active, display_order,
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
    .bind(body.category_id)
    .bind(&body.name)
    .bind(&body.description)
    .bind(image_url_val)
    .bind(body.base_price)
    .bind(body.display_order)
    .bind(body.is_active)
    .bind(image_url_is_present)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound("Menu item not found".into()))?;

    // Maintain price epoch whenever base_price actually changed.
    if let Some(new_price) = body.base_price
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
    if body.image_url == Some(None)
        && let Some(old_url) = existing.image_url {
            let uploads_dir = std::env::var("UPLOADS_DIR").unwrap_or_else(|_| "./uploads".to_string());
            let base_url    = std::env::var("UPLOADS_BASE_URL").unwrap_or_default();
            delete_old_image(&old_url, &base_url, &uploads_dir).await;
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
        "INSERT INTO item_sizes (menu_item_id, label, price_override, display_order)
         VALUES ($1, $2::item_size, $3, $4)
         ON CONFLICT (menu_item_id, label) DO UPDATE SET
             price_override = EXCLUDED.price_override,
             display_order  = EXCLUDED.display_order,
             is_active      = TRUE
         RETURNING id, menu_item_id, label::text, price_override, display_order, is_active",
    )
    .bind(*id)
    .bind(&body.label)
    .bind(body.price_override)
    .bind(body.display_order.unwrap_or(0))
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

    let mut rows = match &query.addon_type {
        Some(t) => sqlx::query_as::<_, AddonItem>(
            "SELECT a.id, a.org_id, a.name, a.type as addon_type, a.default_price,
                    a.is_active, a.display_order, a.created_at, a.updated_at,
                    (SELECT org_ingredient_id FROM addon_item_ingredients WHERE addon_item_id = a.id LIMIT 1) as primary_ingredient_id
             FROM addon_items a
             WHERE a.org_id = $1 AND a.type = $2
             ORDER BY a.type ASC, a.display_order ASC",
        )
        .bind(query.org_id)
        .bind(t)
        .fetch_all(pool.get_ref())
        .await?,

        None => sqlx::query_as::<_, AddonItem>(
            "SELECT a.id, a.org_id, a.name, a.type as addon_type, a.default_price,
                    a.is_active, a.display_order, a.created_at, a.updated_at,
                    (SELECT org_ingredient_id FROM addon_item_ingredients WHERE addon_item_id = a.id LIMIT 1) as primary_ingredient_id
             FROM addon_items a
             WHERE a.org_id = $1
             ORDER BY a.type ASC, a.display_order ASC",
        )
        .bind(query.org_id)
        .fetch_all(pool.get_ref())
        .await?,
    };

    for addon in &mut rows {
        addon.ingredients = fetch_addon_ingredients(pool.get_ref(), addon.id).await?;
    }

    Ok(HttpResponse::Ok().json(rows))
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

    let mut row = sqlx::query_as::<_, AddonItem>(
        "INSERT INTO addon_items (org_id, name, type, default_price, display_order)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id, org_id, name, type as addon_type, default_price,
                   is_active, display_order, created_at, updated_at,
                   NULL::uuid as primary_ingredient_id",
    )
    .bind(body.org_id)
    .bind(&body.name)
    .bind(&body.addon_type)
    .bind(body.default_price)
    .bind(body.display_order.unwrap_or(0))
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

    let mut row = sqlx::query_as::<_, AddonItem>(
        "UPDATE addon_items SET
             name          = COALESCE($2, name),
             type          = COALESCE($3, type),
             default_price = COALESCE($4, default_price),
             display_order = COALESCE($5, display_order),
             is_active     = COALESCE($6, is_active)
         WHERE id = $1
         RETURNING id, org_id, name, type as addon_type, default_price,
                   is_active, display_order, created_at, updated_at,
                   (SELECT org_ingredient_id FROM addon_item_ingredients WHERE addon_item_id = addon_items.id LIMIT 1) as primary_ingredient_id",
    )
    .bind(*id)
    .bind(&body.name)
    .bind(&body.addon_type)
    .bind(body.default_price)
    .bind(body.display_order)
    .bind(body.is_active)
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

    let row = sqlx::query_as::<_, AddonSlot>(
        "INSERT INTO menu_item_addon_slots
             (menu_item_id, addon_type, label, is_required,
              min_selections, max_selections, display_order)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (menu_item_id, addon_type) DO UPDATE SET
             label          = COALESCE(EXCLUDED.label, menu_item_addon_slots.label),
             is_required    = EXCLUDED.is_required,
             min_selections = EXCLUDED.min_selections,
             max_selections = EXCLUDED.max_selections,
             display_order  = EXCLUDED.display_order
         RETURNING id, menu_item_id, addon_type, label, is_required,
                   min_selections, max_selections, display_order, created_at",
    )
    .bind(*id)
    .bind(&body.addon_type)
    .bind(&body.label)
    .bind(body.is_required.unwrap_or(false))
    .bind(body.min_selections.unwrap_or(0))
    .bind(body.max_selections)
    .bind(body.display_order.unwrap_or(0))
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

    let row = sqlx::query_as::<_, AddonSlot>(
        "UPDATE menu_item_addon_slots SET
             label          = COALESCE($3, label),
             is_required    = COALESCE($4, is_required),
             min_selections = COALESCE($5, min_selections),
             max_selections = COALESCE($6, max_selections),
             display_order  = COALESCE($7, display_order)
         WHERE id = $1 AND menu_item_id = $2
         RETURNING id, menu_item_id, addon_type, label, is_required,
                   min_selections, max_selections, display_order, created_at",
    )
    .bind(slot_id)
    .bind(item_id)
    .bind(&body.label)
    .bind(body.is_required)
    .bind(body.min_selections)
    .bind(body.max_selections)
    .bind(body.display_order)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Addon slot not found".into()))?;

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
        .bind(&body.ingredient_unit)
        .bind(body.quantity_used)
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
        .bind(&body.ingredient_unit)
        .bind(body.quantity_used)
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
// OPTIONAL FIELDS
// ═══════════════════════════════════════════════════════════════

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct OptionalField {
    pub id:                Uuid,
    pub menu_item_id:      Uuid,
    pub name:              String,
    pub price:             i32,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name:   Option<String>,
    pub ingredient_unit:   Option<String>,
    #[schema(value_type = Option<f64>)]
    pub quantity_used:     Option<sqlx::types::BigDecimal>,
    pub size_label:        Option<String>,
    pub display_order:     i32,
    pub is_active:         bool,
    pub created_at:        DateTime<Utc>,
    pub updated_at:        DateTime<Utc>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateOptionalFieldRequest {
    pub name:              String,
    pub price:             Option<i32>,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name:   Option<String>,
    pub ingredient_unit:   Option<String>,
    pub quantity_used:     Option<f64>,
    pub size_label:        Option<String>,
    pub display_order:     Option<i32>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateOptionalFieldRequest {
    pub name:              Option<String>,
    pub price:             Option<i32>,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name:   Option<String>,
    pub ingredient_unit:   Option<String>,
    pub quantity_used:     Option<f64>,
    pub size_label:        Option<String>,
    pub display_order:     Option<i32>,
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
        SELECT id, menu_item_id, name, price,
               org_ingredient_id, ingredient_name, ingredient_unit,
               quantity_used, size_label::text,
               display_order, is_active, created_at, updated_at
        FROM menu_item_optional_fields
        WHERE menu_item_id = $1
        ORDER BY display_order ASC, name ASC
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
            && qty < 0.0 {
                return Err(AppError::BadRequest("quantity_used cannot be negative".into()));
            }
    }

    let row = sqlx::query_as::<_, OptionalField>(
        r#"
        INSERT INTO menu_item_optional_fields
            (menu_item_id, name, price, org_ingredient_id, ingredient_name,
             ingredient_unit, quantity_used, size_label, display_order)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8::item_size, $9)
        RETURNING id, menu_item_id, name, price,
                  org_ingredient_id, ingredient_name, ingredient_unit,
                  quantity_used, size_label::text,
                  display_order, is_active, created_at, updated_at
        "#,
    )
    .bind(*id)
    .bind(body.name.trim())
    .bind(body.price.unwrap_or(0))
    .bind(body.org_ingredient_id)
    .bind(&body.ingredient_name)
    .bind(&body.ingredient_unit)
    .bind(body.quantity_used)
    .bind(&body.size_label)
    .bind(body.display_order.unwrap_or(0))
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
        && qty < 0.0 {
            return Err(AppError::BadRequest("quantity_used cannot be negative".into()));
        }

    let row = sqlx::query_as::<_, OptionalField>(
        r#"
        UPDATE menu_item_optional_fields SET
            name              = COALESCE($3, name),
            price             = COALESCE($4, price),
            org_ingredient_id = COALESCE($5, org_ingredient_id),
            ingredient_name   = COALESCE($6, ingredient_name),
            ingredient_unit   = COALESCE($7, ingredient_unit),
            quantity_used     = COALESCE($8, quantity_used),
            size_label        = COALESCE($9::item_size, size_label),
            display_order     = COALESCE($10, display_order),
            is_active         = COALESCE($11, is_active)
        WHERE id = $1 AND menu_item_id = $2
        RETURNING id, menu_item_id, name, price,
                  org_ingredient_id, ingredient_name, ingredient_unit,
                  quantity_used, size_label::text,
                  display_order, is_active, created_at, updated_at
        "#,
    )
    .bind(field_id)
    .bind(item_id)
    .bind(&body.name)
    .bind(body.price)
    .bind(body.org_ingredient_id)
    .bind(&body.ingredient_name)
    .bind(&body.ingredient_unit)
    .bind(body.quantity_used)
    .bind(&body.size_label)
    .bind(body.display_order)
    .bind(body.is_active)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Optional field not found".into()))?;

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

// ── Public Menu ───────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/menu/public/{org_id}",
    tag = "menu",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    responses((status = 200, description = "Public menu", body = PublicMenuResponse), AppErrorResponse)
)]
pub async fn get_public_menu(
    pool:   web::Data<PgPool>,
    org_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let org_id = org_id.into_inner();

    // 1. Fetch Org Info
    let org_info = sqlx::query_as::<_, crate::orgs::handlers::Org>(
        "SELECT id, name, slug, logo_url, currency_code, tax_rate, receipt_footer, is_active 
         FROM organizations 
         WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(org_id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Organization not found".into()))?;

    // 2. Fetch Categories
    let categories = sqlx::query_as::<_, Category>(
        "SELECT id, org_id, name, image_url, display_order, is_active, created_at, updated_at, deleted_at 
         FROM categories 
         WHERE org_id = $1 AND deleted_at IS NULL AND is_active = true 
         ORDER BY display_order ASC",
    )
    .bind(org_id)
    .fetch_all(pool.get_ref())
    .await?;

    // 3. Fetch Items
    let items = sqlx::query_as::<_, MenuItem>(
        "SELECT id, org_id, category_id, name, description, image_url, base_price, is_active, display_order, 
                created_at, updated_at, deleted_at, 
                NULL::text as default_milk_addon_id
         FROM menu_items 
         WHERE org_id = $1 AND deleted_at IS NULL AND is_active = true 
         ORDER BY display_order ASC",
    )
    .bind(org_id)
    .fetch_all(pool.get_ref())
    .await?;

    // 4. Fetch Sizes
    let sizes = sqlx::query_as::<_, ItemSize>(
        "SELECT s.id, s.menu_item_id, s.label::text, s.price_override, s.display_order, s.is_active 
         FROM item_sizes s 
         JOIN menu_items i ON s.menu_item_id = i.id 
         WHERE i.org_id = $1 AND s.is_active = true",
    )
    .bind(org_id)
    .fetch_all(pool.get_ref())
    .await?;

    // 5. Fetch Addon Slots
    let slots = sqlx::query_as::<_, AddonSlot>(
        "SELECT s.id, s.menu_item_id, s.addon_type, s.label, s.is_required, s.min_selections, s.max_selections, s.display_order, s.created_at 
         FROM menu_item_addon_slots s 
         JOIN menu_items i ON s.menu_item_id = i.id 
         WHERE i.org_id = $1",
    )
    .bind(org_id)
    .fetch_all(pool.get_ref())
    .await?;

    // 6. Fetch Addon Items
    let addon_items_all = sqlx::query_as::<_, AddonItem>(
        "SELECT id, org_id, name, type as addon_type, default_price, is_active, display_order, created_at, updated_at, 
                NULL::uuid as primary_ingredient_id 
         FROM addon_items 
         WHERE org_id = $1 AND is_active = true",
    )
    .bind(org_id)
    .fetch_all(pool.get_ref())
    .await?;

    // 7. Synthesize "Global" Addon Slots
    // We group addon items by type and create a slot for each global type (milk_type, coffee_type, extra)
    let global_types = ["milk_type", "coffee_type", "extra"];
    let mut global_slots = Vec::new();
    
    for g_type in global_types {
        let items_for_type: Vec<_> = addon_items_all.iter()
            .filter(|a| a.addon_type == g_type)
            .map(|a| PublicAddonItem {
                id: a.id,
                name: a.name.clone(),
                default_price: a.default_price,
            })
            .collect();

        if !items_for_type.is_empty() {
            let (label, is_required, min_selections, max_selections) = match g_type {
                "milk_type"   => (Some("Milk Type".to_string()),   true,  1, Some(1)),
                "coffee_type" => (Some("Coffee Beans".to_string()), true,  1, Some(1)),
                "extra"       => (Some("Add-ons".to_string()),      false, 0, None),
                _ => (None, false, 0, None),
            };

            // Use a deterministic UUID for global slots to keep IDs stable
            let slot_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, g_type.as_bytes());

            global_slots.push(PublicAddonSlot {
                id: slot_id,
                addon_type: g_type.to_string(),
                label,
                is_required,
                min_selections,
                max_selections,
                addon_items: items_for_type,
            });
        }
    }

    // Organize into response
    let mut public_categories = Vec::new();

    for cat in categories {
        let mut public_items = Vec::new();
        let cat_items: Vec<_> = items.iter().filter(|i| i.category_id == Some(cat.id)).collect();

        for item in cat_items {
            let item_sizes: Vec<_> = sizes.iter().filter(|s| s.menu_item_id == item.id)
                .map(|s| PublicItemSize {
                    id: s.id,
                    label: s.label.clone(),
                    price_override: s.price_override,
                }).collect();

            // Item-specific slots (deprecated but kept for compatibility)
            let mut item_slots: Vec<PublicAddonSlot> = slots.iter().filter(|s| s.menu_item_id == item.id)
                .map(|s| {
                    let relevant_addons: Vec<_> = addon_items_all.iter()
                        .filter(|a| a.addon_type == s.addon_type)
                        .map(|a| PublicAddonItem {
                            id: a.id,
                            name: a.name.clone(),
                            default_price: a.default_price,
                        }).collect();

                    PublicAddonSlot {
                        id: s.id,
                        addon_type: s.addon_type.clone(),
                        label: s.label.clone(),
                        is_required: s.is_required,
                        min_selections: s.min_selections,
                        max_selections: s.max_selections,
                        addon_items: relevant_addons,
                    }
                }).collect();

            // Add global slots that aren't already explicitly defined for this item
            for gs in &global_slots {
                if !item_slots.iter().any(|s| s.addon_type == gs.addon_type) {
                    item_slots.push(gs.clone());
                }
            }

            public_items.push(PublicMenuItem {
                id: item.id,
                name: item.name.clone(),
                description: item.description.clone(),
                image_url: item.image_url.clone(),
                base_price: item.base_price,
                display_order: item.display_order,
                sizes: item_sizes,
                addon_slots: item_slots,
            });
        }

        public_categories.push(PublicCategory {
            id: cat.id,
            name: cat.name,
            image_url: cat.image_url,
            display_order: cat.display_order,
            items: public_items,
        });
    }

    Ok(HttpResponse::Ok().json(PublicMenuResponse {
        org_id,
        org_name: org_info.name,
        logo_url: org_info.logo_url,
        categories: public_categories,
    }))
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
        "SELECT id, org_id, name, image_url, display_order, is_active,
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
        "SELECT id, org_id, category_id, name, description, image_url,
                base_price, is_active, display_order,
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
        "SELECT id, org_id, name, type as addon_type, default_price,
                is_active, display_order, created_at, updated_at,
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
        "SELECT id, menu_item_id, label::text, price_override, display_order, is_active
         FROM item_sizes
         WHERE menu_item_id = $1
         ORDER BY display_order ASC",
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
        "SELECT id, menu_item_id, addon_type, label, is_required,
                min_selections, max_selections, display_order, created_at
         FROM menu_item_addon_slots
         WHERE menu_item_id = $1
         ORDER BY display_order ASC",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await?)
}

async fn fetch_optional_fields(
    pool:    &PgPool,
    item_id: Uuid,
) -> Result<Vec<OptionalField>, AppError> {
    Ok(sqlx::query_as::<_, OptionalField>(
        "SELECT id, menu_item_id, name, price,
                org_ingredient_id, ingredient_name, ingredient_unit,
                quantity_used, size_label::text,
                display_order, is_active, created_at, updated_at
         FROM menu_item_optional_fields
         WHERE menu_item_id = $1 AND is_active = true
         ORDER BY display_order ASC, name ASC",
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