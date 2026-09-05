//! Inventory: the org ingredient catalog + categories, per-branch stock
//! (balances, par levels, movement history), waste and transfers.
//!
//! Model (inventory v2): the org catalog is the only setup. A branch has one
//! lazily-created `branch_stock` row per ingredient it has ever moved or set a
//! par for; a missing row means "0 on hand, never counted" and is never a
//! precondition. Quantities change ONLY through the movement ledger
//! (`super::movements`), so no handler here writes `on_hand`.

use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    errors::{AppError, AppErrorResponse},
    inventory::movements::{MovementParams, lock_on_hand, record_movement},
    models::UserRole,
    permissions::checker::check_permission,
};
use utoipa::{IntoParams, ToSchema};

// ── Response models ───────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct IngredientCategory {
    pub id: Uuid,
    pub org_id: Uuid,
    /// Stable machine key (`general`, `milk`, `coffee_bean`, …). `milk` and
    /// `coffee_bean` carry swap semantics in the menu; the slug never changes.
    pub slug: String,
    pub name: String,
    pub sort_order: i32,
    /// Live (non-deleted) ingredients in this category.
    pub ingredient_count: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct OrgIngredient {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub unit: String,
    pub category_id: Uuid,
    pub category_slug: String,
    pub category_name: String,
    pub description: Option<String>,
    /// Standard (org default) cost, piastres per unit. `null` ⟺ never entered
    /// (unknown, NOT free) — recipes using this ingredient are cost-missing.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schema(value_type = Option<f64>)]
    pub cost_per_unit: Option<Decimal>,
    /// Default supplier for reordering this ingredient; `null` = none set.
    pub supplier_id: Option<Uuid>,
    pub supplier_name: Option<String>,
    /// Named purchase pack (e.g. "case", "sack"); `null` = none.
    pub pack_unit: Option<String>,
    /// How many BASE STOCK units one `pack_unit` yields; `null` = none.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schema(value_type = Option<f64>)]
    pub pack_size: Option<Decimal>,
    /// Usable % after trim/cook loss (e.g. 70 = 70%); `null` = 100%.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schema(value_type = Option<f64>)]
    pub yield_pct: Option<Decimal>,
    /// Grams per millilitre, bridging weight↔volume in recipes; `null` = none.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schema(value_type = Option<f64>)]
    pub density_g_per_ml: Option<Decimal>,
    pub is_active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// One catalog ingredient as seen from a branch. Every live catalog ingredient
/// appears exactly once; `has_activity = false` means the branch has never
/// moved or counted it (on hand is 0 and every date is null).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct BranchStockRow {
    pub branch_id: Uuid,
    pub org_ingredient_id: Uuid,
    pub ingredient_name: String,
    pub unit: String,
    pub category_id: Uuid,
    pub category_slug: String,
    pub category_name: String,
    pub description: Option<String>,
    /// This branch's actual (weighted-average) cost, falling back to the org
    /// standard cost. Piastres per unit; `null` ⟺ unknown.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schema(value_type = Option<f64>)]
    pub cost_per_unit: Option<Decimal>,
    /// Book stock in the base unit. May be negative (sold past zero, flagged).
    pub on_hand: f64,
    /// Reorder point: below-par when `on_hand <= par_min` and `par_min > 0`.
    pub par_min: Option<f64>,
    /// Order-up-to level for reorder suggestions.
    pub par_max: Option<f64>,
    pub below_par: bool,
    /// Last finalized stock count that included this ingredient; `null` = never.
    pub last_counted_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Last ledger movement of any kind; `null` = never.
    pub last_movement_at: Option<chrono::DateTime<chrono::Utc>>,
    pub has_activity: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct StockTransfer {
    pub id: Uuid,
    pub org_id: Uuid,
    pub source_branch_id: Uuid,
    pub source_branch_name: String,
    pub destination_branch_id: Uuid,
    pub destination_branch_name: String,
    pub org_ingredient_id: Uuid,
    pub ingredient_name: String,
    pub unit: String,
    #[schema(value_type = f64)]
    pub quantity: sqlx::types::BigDecimal,
    pub note: Option<String>,
    pub initiated_by: Uuid,
    pub initiated_by_name: String,
    pub initiated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct StockMovement {
    pub id: Uuid,
    pub branch_id: Uuid,
    /// Branch name; only populated by the all-branches waste roll-up (nil
    /// {branch_id}). `None` for single-branch queries that do not select it.
    #[serde(default)]
    #[sqlx(default)]
    pub branch_name: Option<String>,
    pub org_ingredient_id: Uuid,
    pub ingredient_name: String,
    pub unit: String,
    pub branch_stock_id: Option<Uuid>,
    /// inventory_movement_type: sale | void_restock | adjustment_add |
    /// adjustment_remove | waste | transfer_out | transfer_in | purchase_in |
    /// purchase_return | stock_count
    pub movement_type: String,
    /// Signed delta applied to stock (consumption negative, replenishment positive).
    #[schema(value_type = f64)]
    pub quantity: sqlx::types::BigDecimal,
    #[schema(value_type = f64)]
    pub balance_after: sqlx::types::BigDecimal,
    /// Piastres per unit at movement time; `null` ⟺ unknown.
    pub unit_cost: Option<i64>,
    pub reason: Option<String>,
    pub below_zero: bool,
    pub source_type: Option<String>,
    pub source_id: Option<Uuid>,
    pub note: Option<String>,
    pub created_by: Option<Uuid>,
    pub created_by_name: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

// ── Request types ─────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct CreateIngredientCategoryRequest {
    pub name: String,
    /// Optional explicit slug (`[a-z0-9_]`); derived from the name when omitted.
    pub slug: Option<String>,
    pub sort_order: Option<i32>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateIngredientCategoryRequest {
    pub name: Option<String>,
    pub sort_order: Option<i32>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DeleteIngredientCategoryQuery {
    /// Category that ingredients in the deleted one move to. Required when the
    /// category still has ingredients.
    pub reassign_to: Option<Uuid>,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateCatalogItemRequest {
    pub name: String,
    pub unit: String,
    /// Omitted ⟹ the org's `general` category.
    pub category_id: Option<Uuid>,
    pub description: Option<String>,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schema(value_type = Option<f64>)]
    pub cost_per_unit: Option<Decimal>,
    pub supplier_id: Option<Uuid>,
    pub pack_unit: Option<String>,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schema(value_type = Option<f64>)]
    pub pack_size: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schema(value_type = Option<f64>)]
    pub yield_pct: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schema(value_type = Option<f64>)]
    pub density_g_per_ml: Option<Decimal>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateCatalogItemRequest {
    pub name: Option<String>,
    pub unit: Option<String>,
    pub category_id: Option<Uuid>,
    pub description: Option<String>,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schema(value_type = Option<f64>)]
    pub cost_per_unit: Option<Decimal>,
    /// Set/replace the default supplier (omitted = unchanged).
    pub supplier_id: Option<Uuid>,
    pub pack_unit: Option<String>,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schema(value_type = Option<f64>)]
    pub pack_size: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schema(value_type = Option<f64>)]
    pub yield_pct: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schema(value_type = Option<f64>)]
    pub density_g_per_ml: Option<Decimal>,
    pub is_active: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct OrgInventorySettings {
    /// Stock-count variance tolerance (percent). A counted row whose |difference|
    /// is at least this percent of book stock is flagged and needs a reason.
    pub stocktake_variance_threshold_pct: f64,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateInventorySettingsRequest {
    pub stocktake_variance_threshold_pct: f64,
}

/// Par levels for one ingredient at one branch. `null` clears a level.
#[derive(Deserialize, ToSchema)]
pub struct SetParRequest {
    pub par_min: Option<f64>,
    pub par_max: Option<f64>,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateTransferRequest {
    pub source_branch_id: Uuid,
    pub destination_branch_id: Uuid,
    pub org_ingredient_id: Uuid,
    pub quantity: f64,
    pub note: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateTransferRequest {
    pub note: Option<String>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListTransfersQuery {
    pub direction: Option<String>, // "incoming" | "outgoing" | None = both
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Opt-in pagination for the waste list. Defaults to the 200 most recent rows
/// (cap 1000) so the query is always bounded.
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListPageQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

fn page_bounds(limit: Option<i64>, offset: Option<i64>) -> (i64, i64) {
    (
        limit.unwrap_or(200).clamp(1, 1000),
        offset.unwrap_or(0).max(0),
    )
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListMovementsQuery {
    pub org_ingredient_id: Option<Uuid>,
    #[serde(rename = "type")]
    pub movement_type: Option<String>,
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateWasteRequest {
    pub org_ingredient_id: Uuid,
    pub quantity: f64,
    /// expired | spoiled | damaged | overproduction | order_cancelled | theft | other
    /// (`order_cancelled` is normally auto-logged by void/cancel, not entered here)
    pub reason: String,
    pub note: Option<String>,
}

// ── Shared SELECT fragments ───────────────────────────────────

const ORG_INGREDIENT_SELECT: &str = r#"
    SELECT oi.id, oi.org_id, oi.name, oi.unit::text AS unit,
           oi.category_id, ic.slug AS category_slug, ic.name AS category_name,
           oi.description, oi.cost_per_unit,
           oi.supplier_id, s.name AS supplier_name,
           oi.pack_unit, oi.pack_size, oi.yield_pct, oi.density_g_per_ml,
           oi.is_active, oi.created_at, oi.updated_at
    FROM org_ingredients oi
    JOIN ingredient_categories ic ON ic.id = oi.category_id
    LEFT JOIN suppliers s ON s.id = oi.supplier_id
"#;

const CATEGORY_SELECT: &str = r#"
    SELECT c.id, c.org_id, c.slug, c.name, c.sort_order,
           (SELECT count(*) FROM org_ingredients oi
             WHERE oi.category_id = c.id AND oi.deleted_at IS NULL) AS ingredient_count,
           c.created_at, c.updated_at
    FROM ingredient_categories c
"#;

const BRANCH_STOCK_SELECT: &str = r#"
    SELECT $1::uuid AS branch_id, oi.id AS org_ingredient_id,
           oi.name AS ingredient_name, oi.unit::text AS unit,
           oi.category_id, ic.slug AS category_slug, ic.name AS category_name,
           oi.description,
           COALESCE(bs.cost_per_unit, oi.cost_per_unit) AS cost_per_unit,
           COALESCE(bs.on_hand, 0)::float8 AS on_hand,
           bs.par_min::float8 AS par_min,
           bs.par_max::float8 AS par_max,
           (COALESCE(bs.par_min, 0) > 0 AND COALESCE(bs.on_hand, 0) <= bs.par_min) AS below_par,
           bs.last_counted_at, bs.last_movement_at,
           (bs.id IS NOT NULL) AS has_activity
    FROM org_ingredients oi
    JOIN ingredient_categories ic ON ic.id = oi.category_id
    LEFT JOIN branch_stock bs ON bs.org_ingredient_id = oi.id AND bs.branch_id = $1
"#;

// ── Categories ────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/inventory/orgs/{org_id}/categories",
    tag = "inventory",
    operation_id = "list_ingredient_categories",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    responses((status = 200, description = "Ingredient categories", body = Vec<IngredientCategory>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_ingredient_categories(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_org_access(&claims, *org_id)?;

    ensure_general_category(pool.get_ref(), *org_id).await?;
    let sql = format!("{CATEGORY_SELECT} WHERE c.org_id = $1 ORDER BY c.sort_order, c.name");
    let rows = sqlx::query_as::<_, IngredientCategory>(&sql)
        .bind(*org_id)
        .fetch_all(pool.get_ref())
        .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    post,
    path = "/inventory/orgs/{org_id}/categories",
    tag = "inventory",
    operation_id = "create_ingredient_category",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    request_body = CreateIngredientCategoryRequest,
    responses((status = 201, description = "Category created", body = IngredientCategory), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_ingredient_category(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
    body: web::Json<CreateIngredientCategoryRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "create").await?;
    require_org_access(&claims, *org_id)?;

    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > 80 {
        return Err(AppError::BadRequest(
            "Category name must be 1–80 characters".into(),
        ));
    }
    let slug = match body.slug.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => {
            if !is_valid_slug(s) {
                return Err(AppError::BadRequest(
                    "slug may only contain a-z, 0-9 and _ (max 64)".into(),
                ));
            }
            s.to_string()
        }
        _ => slugify(name).ok_or_else(|| {
            AppError::BadRequest("Category name must contain a letter or digit".into())
        })?,
    };

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO ingredient_categories (org_id, slug, name, sort_order) \
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(*org_id)
    .bind(&slug)
    .bind(name)
    .bind(body.sort_order.unwrap_or(10))
    .fetch_one(pool.get_ref())
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
            AppError::Conflict(format!("A category with the key '{slug}' already exists"))
        }
        _ => AppError::Db(e),
    })?;

    Ok(HttpResponse::Created().json(fetch_category(pool.get_ref(), id).await?))
}

#[utoipa::path(
    patch,
    path = "/inventory/orgs/{org_id}/categories/{id}",
    tag = "inventory",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID"),
        ("id" = Uuid, Path, description = "Category ID")
    ),
    operation_id = "update_ingredient_category",
    request_body = UpdateIngredientCategoryRequest,
    responses((status = 200, description = "Category updated", body = IngredientCategory), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_ingredient_category(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(Uuid, Uuid)>,
    body: web::Json<UpdateIngredientCategoryRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "update").await?;
    let (org_id, id) = path.into_inner();
    require_org_access(&claims, org_id)?;

    let name = body.name.as_deref().map(str::trim);
    if let Some(n) = name
        && (n.is_empty() || n.chars().count() > 80)
    {
        return Err(AppError::BadRequest(
            "Category name must be 1–80 characters".into(),
        ));
    }

    let updated: Option<Uuid> = sqlx::query_scalar(
        "UPDATE ingredient_categories \
         SET name = COALESCE($3, name), sort_order = COALESCE($4, sort_order) \
         WHERE id = $1 AND org_id = $2 RETURNING id",
    )
    .bind(id)
    .bind(org_id)
    .bind(name)
    .bind(body.sort_order)
    .fetch_optional(pool.get_ref())
    .await?;
    let id = updated.ok_or_else(|| AppError::NotFound("Category not found".into()))?;

    Ok(HttpResponse::Ok().json(fetch_category(pool.get_ref(), id).await?))
}

#[utoipa::path(
    delete,
    path = "/inventory/orgs/{org_id}/categories/{id}",
    tag = "inventory",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID"),
        ("id" = Uuid, Path, description = "Category ID"),
        DeleteIngredientCategoryQuery
    ),
    operation_id = "delete_ingredient_category",
    responses((status = 204, description = "Category deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_ingredient_category(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(Uuid, Uuid)>,
    query: web::Query<DeleteIngredientCategoryQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "delete").await?;
    let (org_id, id) = path.into_inner();
    require_org_access(&claims, org_id)?;

    let mut tx = pool.get_ref().begin().await?;

    let slug: String = sqlx::query_scalar(
        "SELECT slug FROM ingredient_categories WHERE id = $1 AND org_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(org_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound("Category not found".into()))?;
    if slug == "general" {
        return Err(AppError::BadRequest(
            "The general category cannot be deleted".into(),
        ));
    }

    let in_use: i64 =
        sqlx::query_scalar("SELECT count(*) FROM org_ingredients WHERE category_id = $1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;

    if in_use > 0 {
        let target = query.reassign_to.ok_or_else(|| {
            AppError::Conflict(format!(
                "{in_use} ingredient(s) use this category. Pass reassign_to to move them first."
            ))
        })?;
        if target == id {
            return Err(AppError::BadRequest(
                "reassign_to must be a different category".into(),
            ));
        }
        ensure_category_in_org(&mut *tx, target, org_id).await?;
        sqlx::query("UPDATE org_ingredients SET category_id = $2 WHERE category_id = $1")
            .bind(id)
            .bind(target)
            .execute(&mut *tx)
            .await?;
    }

    sqlx::query("DELETE FROM ingredient_categories WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(HttpResponse::NoContent().finish())
}

// ── Catalog ───────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/inventory/orgs/{org_id}/catalog",
    tag = "inventory",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    responses((status = 200, description = "List catalog items", body = Vec<OrgIngredient>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_catalog(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_org_access(&claims, *org_id)?;

    let sql = format!(
        "{ORG_INGREDIENT_SELECT} WHERE oi.org_id = $1 AND oi.deleted_at IS NULL ORDER BY oi.name"
    );
    let rows = sqlx::query_as::<_, OrgIngredient>(&sql)
        .bind(*org_id)
        .fetch_all(pool.get_ref())
        .await?;

    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    post,
    path = "/inventory/orgs/{org_id}/catalog",
    tag = "inventory",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    request_body = CreateCatalogItemRequest,
    responses((status = 201, description = "Catalog item created", body = OrgIngredient), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_catalog_item(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
    body: web::Json<CreateCatalogItemRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "create").await?;
    require_org_access(&claims, *org_id)?;
    validate_unit(&body.unit)?;

    if body.name.trim().is_empty() {
        return Err(AppError::BadRequest("name cannot be empty".into()));
    }
    if let Some(sup) = body.supplier_id {
        ensure_supplier_in_org(pool.get_ref(), sup, *org_id).await?;
    }

    let mut tx = pool.get_ref().begin().await?;

    let category_id = match body.category_id {
        Some(c) => {
            ensure_category_in_org(&mut *tx, c, *org_id).await?;
            c
        }
        None => ensure_general_category(&mut *tx, *org_id).await?,
    };

    // No cost supplied ⟹ stored as NULL = unknown. Never default to 0 —
    // zero means "genuinely free" and would flow into every cost rollup.
    let id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO org_ingredients
            (org_id, name, unit, category_id, description, cost_per_unit, supplier_id,
             pack_unit, pack_size, yield_pct, density_g_per_ml)
        VALUES ($1, $2, $3::inventory_unit, $4, $5, $6, $7, $8, $9, $10, $11)
        RETURNING id
        "#,
    )
    .bind(*org_id)
    .bind(body.name.trim())
    .bind(&body.unit)
    .bind(category_id)
    .bind(&body.description)
    .bind(body.cost_per_unit)
    .bind(body.supplier_id)
    .bind(&body.pack_unit)
    .bind(body.pack_size)
    .bind(body.yield_pct)
    .bind(body.density_g_per_ml)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
            AppError::Conflict("An ingredient with this name already exists in the catalog".into())
        }
        _ => AppError::Db(e),
    })?;

    // Seed the first cost history row — only when a cost actually exists.
    if let Some(cost) = body.cost_per_unit {
        sqlx::query(
            "INSERT INTO ingredient_cost_history \
                 (org_ingredient_id, cost_per_unit, effective_from, changed_by, note) \
             VALUES ($1, $2, now(), $3, 'Initial cost')",
        )
        .bind(id)
        .bind(cost)
        .bind(claims.user_id())
        .execute(&mut *tx)
        .await?;
    }

    let row = fetch_ingredient(&mut *tx, id).await?;
    tx.commit().await?;
    Ok(HttpResponse::Created().json(row))
}

#[utoipa::path(
    patch,
    path = "/inventory/orgs/{org_id}/catalog/{id}",
    tag = "inventory",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID"),
        ("id" = Uuid, Path, description = "Ingredient ID")
    ),
    request_body = UpdateCatalogItemRequest,
    responses((status = 200, description = "Catalog item updated", body = OrgIngredient), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_catalog_item(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(Uuid, Uuid)>,
    body: web::Json<UpdateCatalogItemRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "update").await?;
    let (org_id, id) = path.into_inner();
    require_org_access(&claims, org_id)?;

    if let Some(ref u) = body.unit {
        validate_unit(u)?;
    }
    if let Some(sup) = body.supplier_id {
        ensure_supplier_in_org(pool.get_ref(), sup, org_id).await?;
    }

    let mut tx = pool.get_ref().begin().await?;

    if let Some(c) = body.category_id {
        ensure_category_in_org(&mut *tx, c, org_id).await?;
    }

    // Lock the row and read its current base unit + yield.
    let (current_unit, current_yield): (String, Option<f64>) = sqlx::query_as(
        "SELECT unit::text, yield_pct::float8 FROM org_ingredients \
         WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(id)
    .bind(org_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound("Ingredient not found".into()))?;

    // Changing the base unit must stay within the same measure (g↔kg or ml↔l)
    // and rebase EVERY stored quantity + cost that references this ingredient —
    // otherwise recipes, branch stock and costing would silently be off by the
    // conversion factor. `pcs` has no conversions, so it can only stay `pcs`.
    if let Some(new_unit) = body.unit.as_deref()
        && !new_unit.eq_ignore_ascii_case(&current_unit)
    {
        if body.cost_per_unit.is_some() {
            return Err(AppError::BadRequest(
                "Change the unit and the cost in separate requests — the cost is \
                 converted automatically when the unit changes."
                    .into(),
            ));
        }
        // F = how many OLD units fit in one NEW unit (g per kg = 1000).
        // Cross-family (g↔ml, *↔pcs) returns Err ⟹ rejected here.
        let f = crate::units::convert(1.0, new_unit, &current_unit).map_err(|_| {
            AppError::BadRequest(
                "A unit can only change within the same measure: g ↔ kg or ml ↔ l.".into(),
            )
        })?;

        // Recipe quantities live in the unified `recipe_lines` (owner item_size
        // or modifier_option), keyed by ingredient_id — one rebase covers item
        // recipes, addon recipes and optional recipes. Base→new base unit is ÷ F.
        sqlx::query(
            "UPDATE recipe_lines SET quantity = round((quantity / $2)::numeric, 3), unit = $3 WHERE ingredient_id = $1",
        )
        .bind(id)
        .bind(f)
        .bind(new_unit)
        .execute(&mut *tx)
        .await?;
        // TRANSITION (menu-unification): until the shim flip, the LEGACY recipe
        // tables are still what costing/orders read — rebase them in lockstep or
        // COGS/deductions would be off by F in the deploy→flip window. Post-flip
        // they are shim VIEWS (relkind 'v', not updatable) and this self-disables.
        // Remove at SHIM_TEARDOWN.
        let legacy_live: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'public' AND c.relname = 'menu_item_recipes' AND c.relkind = 'r')",
        )
        .fetch_one(&mut *tx)
        .await?;
        if legacy_live {
            for q in [
                "UPDATE menu_item_recipes        SET quantity_used = round((quantity_used / $2)::numeric, 3), ingredient_unit = $3 WHERE org_ingredient_id = $1",
                "UPDATE addon_item_ingredients   SET quantity_used = round((quantity_used / $2)::numeric, 3), ingredient_unit = $3 WHERE org_ingredient_id = $1",
                "UPDATE menu_item_optional_fields SET quantity_used = round((quantity_used / $2)::numeric, 3), ingredient_unit = $3 WHERE org_ingredient_id = $1 AND quantity_used IS NOT NULL",
            ] {
                sqlx::query(q)
                    .bind(id)
                    .bind(f)
                    .bind(new_unit)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        // Branch balances, par levels and the ledger are all denominated in the
        // base unit → ÷ F; per-branch actual cost is per OLD unit → × F. This is
        // a re-denomination, not a stock change, so it is the one sanctioned
        // direct write to on_hand: the ledger is rescaled identically in the same
        // transaction and the guard trigger is told so via SET LOCAL.
        sqlx::query("SET LOCAL madar.stock_rebase = 'on'")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE branch_stock \
             SET on_hand       = round((on_hand / $2)::numeric, 3), \
                 par_min       = round((par_min / $2)::numeric, 3), \
                 par_max       = round((par_max / $2)::numeric, 3), \
                 cost_per_unit = round((cost_per_unit * $2)::numeric, 2) \
             WHERE org_ingredient_id = $1",
        )
        .bind(id)
        .bind(f)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE inventory_movements \
             SET quantity      = round((quantity / $2)::numeric, 3), \
                 balance_after = round((balance_after / $2)::numeric, 3) \
             WHERE org_ingredient_id = $1",
        )
        .bind(id)
        .bind(f)
        .execute(&mut *tx)
        .await?;
        // Org default cost is piastres per OLD unit → per NEW unit is × F. Both
        // the org default and EVERY epoch (org + per-branch) scale identically.
        sqlx::query("UPDATE org_ingredients SET cost_per_unit = round((cost_per_unit * $2)::numeric, 2) WHERE id = $1 AND cost_per_unit IS NOT NULL")
            .bind(id).bind(f).execute(&mut *tx).await?;
        sqlx::query("UPDATE ingredient_cost_history SET cost_per_unit = round((cost_per_unit * $2)::numeric, 2) WHERE org_ingredient_id = $1")
            .bind(id).bind(f).execute(&mut *tx).await?;
    }

    // Changing yield rebases stored recipe quantities (which are grossed-up by
    // 1/yield at save time) by old/new, so the effective consumption + COGS stay
    // correct without re-saving every recipe. NULL yield = 100%.
    if let Some(new_yield) = body.yield_pct {
        use rust_decimal::prelude::ToPrimitive;
        let yf = |pct: f64| (pct / 100.0).max(f64::MIN_POSITIVE);
        let old_yf = current_yield.map(yf).unwrap_or(1.0);
        let new_yf = new_yield.to_f64().map(yf).unwrap_or(1.0);
        let factor = old_yf / new_yf; // stored_new = stored_old * (old_yf/new_yf)
        if (factor - 1.0).abs() > 1e-9 {
            sqlx::query(
                "UPDATE recipe_lines SET quantity = round((quantity * $2)::numeric, 3) WHERE ingredient_id = $1",
            )
            .bind(id)
            .bind(factor)
            .execute(&mut *tx)
            .await?;
            // TRANSITION: keep the legacy tables (still live pre-flip) in lockstep;
            // self-disables once they become shim views. Remove at SHIM_TEARDOWN.
            let legacy_live: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = 'public' AND c.relname = 'menu_item_recipes' AND c.relkind = 'r')",
            )
            .fetch_one(&mut *tx)
            .await?;
            if legacy_live {
                for q in [
                    "UPDATE menu_item_recipes         SET quantity_used = round((quantity_used * $2)::numeric, 3) WHERE org_ingredient_id = $1",
                    "UPDATE addon_item_ingredients    SET quantity_used = round((quantity_used * $2)::numeric, 3) WHERE org_ingredient_id = $1",
                    "UPDATE menu_item_optional_fields SET quantity_used = round((quantity_used * $2)::numeric, 3) WHERE org_ingredient_id = $1 AND quantity_used IS NOT NULL",
                ] {
                    sqlx::query(q)
                        .bind(id)
                        .bind(factor)
                        .execute(&mut *tx)
                        .await?;
                }
            }
        }
    }

    let updated: Option<Uuid> = sqlx::query_scalar(
        r#"
        UPDATE org_ingredients SET
            name             = COALESCE($2, name),
            unit             = COALESCE($3::inventory_unit, unit),
            category_id      = COALESCE($4, category_id),
            description      = COALESCE($5, description),
            cost_per_unit    = COALESCE($6, cost_per_unit),
            is_active        = COALESCE($7, is_active),
            supplier_id      = COALESCE($9, supplier_id),
            pack_unit        = COALESCE($10, pack_unit),
            pack_size        = COALESCE($11, pack_size),
            yield_pct        = COALESCE($12, yield_pct),
            density_g_per_ml = COALESCE($13, density_g_per_ml)
        WHERE id = $1 AND org_id = $8 AND deleted_at IS NULL
        RETURNING id
        "#,
    )
    .bind(id)
    .bind(body.name.as_deref().map(str::trim))
    .bind(&body.unit)
    .bind(body.category_id)
    .bind(&body.description)
    .bind(body.cost_per_unit)
    .bind(body.is_active)
    .bind(org_id)
    .bind(body.supplier_id)
    .bind(&body.pack_unit)
    .bind(body.pack_size)
    .bind(body.yield_pct)
    .bind(body.density_g_per_ml)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
            AppError::Conflict("An ingredient with this name already exists in the catalog".into())
        }
        _ => AppError::Db(e),
    })?;
    updated.ok_or_else(|| AppError::NotFound("Ingredient not found".into()))?;

    // Maintain cost history whenever the org standard cost actually changed.
    // Per-branch actual-cost epochs (written by receipts) are left untouched.
    if let Some(new_cost) = body.cost_per_unit {
        let current_history_cost: Option<Decimal> = sqlx::query_scalar(
            "SELECT cost_per_unit FROM ingredient_cost_history \
             WHERE org_ingredient_id = $1 AND branch_id IS NULL AND effective_until IS NULL",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;

        if current_history_cost != Some(new_cost) {
            sqlx::query(
                "UPDATE ingredient_cost_history SET effective_until = now() \
                 WHERE org_ingredient_id = $1 AND branch_id IS NULL AND effective_until IS NULL",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO ingredient_cost_history \
                     (org_ingredient_id, cost_per_unit, effective_from, changed_by) \
                 VALUES ($1, $2, now(), $3)",
            )
            .bind(id)
            .bind(new_cost)
            .bind(claims.user_id())
            .execute(&mut *tx)
            .await?;
        }
    }

    let row = fetch_ingredient(&mut *tx, id).await?;
    tx.commit().await?;
    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    delete,
    path = "/inventory/orgs/{org_id}/catalog/{id}",
    tag = "inventory",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID"),
        ("id" = Uuid, Path, description = "Ingredient ID")
    ),
    responses((status = 204, description = "Catalog item deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_catalog_item(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "delete").await?;
    let (org_id, id) = path.into_inner();
    require_org_access(&claims, org_id)?;

    // Recipes and optional fields drive sale-time deductions, so an ingredient
    // they reference cannot go. Ledger history is kept (soft delete), but an
    // ingredient that still has stock on a shelf somewhere must be counted or
    // wasted down first — hiding it would hide real value.
    let (in_recipes, stocked): (bool, bool) = sqlx::query_as(
        r#"
        SELECT
            EXISTS (
                SELECT 1 FROM menu_item_recipes         WHERE org_ingredient_id = $1
                UNION ALL
                SELECT 1 FROM addon_item_ingredients    WHERE org_ingredient_id = $1
                UNION ALL
                SELECT 1 FROM menu_item_optional_fields WHERE org_ingredient_id = $1
            ),
            EXISTS (SELECT 1 FROM branch_stock WHERE org_ingredient_id = $1 AND on_hand <> 0)
        "#,
    )
    .bind(id)
    .fetch_one(pool.get_ref())
    .await?;

    if in_recipes {
        return Err(AppError::Conflict(
            "Ingredient is used by recipes or optional fields. Remove those references first."
                .into(),
        ));
    }
    if stocked {
        return Err(AppError::Conflict(
            "Ingredient still has stock at a branch. Count or waste it to zero first.".into(),
        ));
    }

    let affected = sqlx::query(
        "UPDATE org_ingredients SET deleted_at = now() WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(org_id)
    .execute(pool.get_ref())
    .await?
    .rows_affected();
    if affected == 0 {
        return Err(AppError::NotFound("Ingredient not found".into()));
    }

    Ok(HttpResponse::NoContent().finish())
}

// ── Org settings ──────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/inventory/orgs/{org_id}/settings",
    tag = "inventory",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    responses((status = 200, description = "Org inventory settings", body = OrgInventorySettings), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_inventory_settings(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_org_access(&claims, *org_id)?;

    let row = sqlx::query_as::<_, OrgInventorySettings>(
        "SELECT stocktake_variance_threshold_pct::float8 AS stocktake_variance_threshold_pct \
         FROM organizations WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(*org_id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Organization not found".into()))?;

    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    put,
    path = "/inventory/orgs/{org_id}/settings",
    tag = "inventory",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    request_body = UpdateInventorySettingsRequest,
    responses((status = 200, description = "Org inventory settings updated", body = OrgInventorySettings), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_inventory_settings(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
    body: web::Json<UpdateInventorySettingsRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "update").await?;
    require_org_access(&claims, *org_id)?;

    let pct = body.stocktake_variance_threshold_pct;
    if !(0.0..=100.0).contains(&pct) {
        return Err(AppError::BadRequest(
            "stocktake_variance_threshold_pct must be between 0 and 100".into(),
        ));
    }

    let row = sqlx::query_as::<_, OrgInventorySettings>(
        "UPDATE organizations SET stocktake_variance_threshold_pct = $2, updated_at = now() \
         WHERE id = $1 AND deleted_at IS NULL \
         RETURNING stocktake_variance_threshold_pct::float8 AS stocktake_variance_threshold_pct",
    )
    .bind(*org_id)
    .bind(pct)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Organization not found".into()))?;

    Ok(HttpResponse::Ok().json(row))
}

// ── Branch stock ──────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/inventory/branches/{branch_id}/stock",
    tag = "inventory",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    responses((status = 200, description = "Every catalog ingredient as seen from this branch", body = Vec<BranchStockRow>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_branch_stock(
    req: HttpRequest,
    pool: crate::db::Db,
    branch_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;
    let org_id = branch_org(pool.get_ref(), *branch_id).await?;

    let sql = format!(
        "{BRANCH_STOCK_SELECT} WHERE oi.org_id = $2 AND oi.deleted_at IS NULL \
         ORDER BY ic.sort_order, ic.name, oi.name"
    );
    let rows = sqlx::query_as::<_, BranchStockRow>(&sql)
        .bind(*branch_id)
        .bind(org_id)
        .fetch_all(pool.get_ref())
        .await?;

    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    put,
    path = "/inventory/branches/{branch_id}/stock/{org_ingredient_id}/par",
    tag = "inventory",
    params(
        ("branch_id" = Uuid, Path, description = "Branch ID"),
        ("org_ingredient_id" = Uuid, Path, description = "Ingredient ID")
    ),
    request_body = SetParRequest,
    responses((status = 200, description = "Par levels saved", body = BranchStockRow), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn set_par_levels(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(Uuid, Uuid)>,
    body: web::Json<SetParRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "update").await?;
    let (branch_id, org_ingredient_id) = path.into_inner();
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;
    let org_id = branch_org(pool.get_ref(), branch_id).await?;
    ensure_ingredient_in_org(pool.get_ref(), org_ingredient_id, org_id).await?;

    if body.par_min.is_some_and(|v| v < 0.0) || body.par_max.is_some_and(|v| v < 0.0) {
        return Err(AppError::BadRequest("Par levels cannot be negative".into()));
    }
    if let (Some(lo), Some(hi)) = (body.par_min, body.par_max)
        && hi < lo
    {
        return Err(AppError::BadRequest(
            "Max par must be at least the min par".into(),
        ));
    }

    // Insert-or-update the settings half of the row; on_hand is untouched (the
    // guard trigger only watches on_hand, so this never trips it).
    sqlx::query(
        "INSERT INTO branch_stock (branch_id, org_ingredient_id, on_hand, par_min, par_max) \
         VALUES ($1, $2, 0, $3, $4) \
         ON CONFLICT (branch_id, org_ingredient_id) \
         DO UPDATE SET par_min = EXCLUDED.par_min, par_max = EXCLUDED.par_max",
    )
    .bind(branch_id)
    .bind(org_ingredient_id)
    .bind(body.par_min)
    .bind(body.par_max)
    .execute(pool.get_ref())
    .await?;

    let row = fetch_branch_stock_row(pool.get_ref(), branch_id, org_ingredient_id).await?;
    Ok(HttpResponse::Ok().json(row))
}

// ── Movement ledger ───────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/inventory/branches/{branch_id}/movements",
    tag = "inventory",
    params(("branch_id" = Uuid, Path, description = "Branch ID"), ListMovementsQuery),
    responses((status = 200, description = "List stock movements", body = Vec<StockMovement>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_movements(
    req: HttpRequest,
    pool: crate::db::Db,
    branch_id: web::Path<Uuid>,
    query: web::Query<ListMovementsQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    let per_page = query.per_page.unwrap_or(100).clamp(1, 500);
    let page = query.page.unwrap_or(1).max(1);
    let offset = (page - 1) * per_page;

    let rows = sqlx::query_as::<_, StockMovement>(
        r#"
        SELECT
            m.id, m.branch_id, m.org_ingredient_id,
            oi.name       AS ingredient_name,
            oi.unit::text AS unit,
            m.branch_stock_id,
            m.type::text  AS movement_type,
            m.quantity, m.balance_after, m.unit_cost, m.reason, m.below_zero,
            m.source_type, m.source_id, m.note, m.created_by,
            u.name        AS created_by_name,
            m.created_at
        FROM inventory_movements m
        JOIN org_ingredients oi ON oi.id = m.org_ingredient_id
        LEFT JOIN users u       ON u.id  = m.created_by
        WHERE m.branch_id = $1
          AND ($2::uuid        IS NULL OR m.org_ingredient_id = $2)
          AND ($3::text        IS NULL OR m.type::text = $3)
          AND ($4::timestamptz IS NULL OR m.created_at >= $4)
          AND ($5::timestamptz IS NULL OR m.created_at <= $5)
        ORDER BY m.created_at DESC, m.id DESC
        LIMIT $6 OFFSET $7
        "#,
    )
    .bind(*branch_id)
    .bind(query.org_ingredient_id)
    .bind(&query.movement_type)
    .bind(query.from)
    .bind(query.to)
    .bind(per_page)
    .bind(offset)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── Waste ─────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/inventory/branches/{branch_id}/waste",
    tag = "inventory",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    request_body = CreateWasteRequest,
    responses((status = 201, description = "Waste recorded", body = StockMovement), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_waste(
    req: HttpRequest,
    pool: crate::db::Db,
    branch_id: web::Path<Uuid>,
    body: web::Json<CreateWasteRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory_waste", "create").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    validate_waste_reason(&body.reason)?;
    if body.quantity <= 0.0 {
        return Err(AppError::BadRequest(
            "quantity must be greater than 0".into(),
        ));
    }
    let org_id = branch_org(pool.get_ref(), *branch_id).await?;
    ensure_ingredient_in_org(pool.get_ref(), body.org_ingredient_id, org_id).await?;

    let mut tx = pool.get_ref().begin().await?;

    // Lock the balance (0 when the branch has no activity yet) so the check and
    // the movement are one atomic step.
    let on_hand = lock_on_hand(&mut *tx, *branch_id, body.org_ingredient_id)
        .await?
        .unwrap_or(0.0);
    if on_hand < body.quantity {
        return Err(AppError::BadRequest(format!(
            "Only {on_hand:.3} on hand — cannot waste {:.3}.",
            body.quantity
        )));
    }

    let unit_cost = branch_unit_cost(&mut *tx, *branch_id, body.org_ingredient_id).await?;
    let note = body.note.as_deref().filter(|s| !s.trim().is_empty());
    let posted = record_movement(
        &mut *tx,
        MovementParams {
            branch_id: *branch_id,
            org_ingredient_id: body.org_ingredient_id,
            movement_type: "waste",
            quantity: -body.quantity,
            unit_cost,
            reason: Some(body.reason.as_str()),
            source_type: Some("waste"),
            source_id: None,
            note,
            created_by: Some(claims.user_id()),
        },
    )
    .await?;

    let movement = fetch_movement(&mut *tx, posted.id).await?;
    tx.commit().await?;
    Ok(HttpResponse::Created().json(movement))
}

#[utoipa::path(
    get,
    path = "/inventory/branches/{branch_id}/waste",
    tag = "inventory",
    params(("branch_id" = Uuid, Path, description = "Branch ID"), ListPageQuery),
    responses((status = 200, description = "List waste movements", body = Vec<StockMovement>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_waste(
    req: HttpRequest,
    pool: crate::db::Db,
    branch_id: web::Path<Uuid>,
    page: web::Query<ListPageQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory_waste", "read").await?;
    let (limit, offset) = page_bounds(page.limit, page.offset);

    // nil UUID = every branch in the caller's org ("All branches"); any other
    // UUID is that one branch after the usual access check.
    let (scope_condition, scope_id): (&str, Uuid) = if branch_id.is_nil() {
        let org = claims
            .scope_org(crate::auth::middleware::header_org_id(&req))
            .ok_or_else(|| AppError::Forbidden("No organization in scope".into()))?;
        (
            "m.branch_id IN (SELECT id FROM branches WHERE org_id = $1 AND deleted_at IS NULL)",
            org,
        )
    } else {
        require_branch_access(pool.get_ref(), &claims, *branch_id).await?;
        ("m.branch_id = $1", *branch_id)
    };

    let sql = format!(
        r#"
        SELECT
            m.id, m.branch_id,
            b.name AS branch_name,
            m.org_ingredient_id,
            oi.name AS ingredient_name, oi.unit::text AS unit,
            m.branch_stock_id, m.type::text AS movement_type,
            m.quantity, m.balance_after, m.unit_cost, m.reason, m.below_zero,
            m.source_type, m.source_id, m.note, m.created_by,
            u.name AS created_by_name, m.created_at
        FROM inventory_movements m
        JOIN org_ingredients oi ON oi.id = m.org_ingredient_id
        JOIN branches b         ON b.id  = m.branch_id
        LEFT JOIN users u       ON u.id  = m.created_by
        WHERE {scope_condition} AND m.type = 'waste'
        ORDER BY m.created_at DESC, m.id DESC
        LIMIT $2 OFFSET $3
        "#
    );
    let rows = sqlx::query_as::<_, StockMovement>(&sql)
        .bind(scope_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool.get_ref())
        .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── Transfers ─────────────────────────────────────────────────

const TRANSFER_SELECT: &str = r#"
    SELECT
        t.id, t.org_id,
        t.source_branch_id,      sb.name AS source_branch_name,
        t.destination_branch_id, db.name AS destination_branch_name,
        t.org_ingredient_id,
        oi.name       AS ingredient_name,
        oi.unit::text AS unit,
        t.quantity, t.note, t.initiated_by,
        u.name AS initiated_by_name,
        t.initiated_at
    FROM stock_transfers t
    JOIN branches sb        ON sb.id = t.source_branch_id
    JOIN branches db        ON db.id = t.destination_branch_id
    JOIN org_ingredients oi ON oi.id = t.org_ingredient_id
    JOIN users u            ON u.id  = t.initiated_by
"#;

#[utoipa::path(
    post,
    path = "/inventory/transfers",
    tag = "inventory",
    request_body = CreateTransferRequest,
    responses((status = 201, description = "Transfer created", body = StockTransfer), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_transfer(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateTransferRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory_transfers", "create").await?;
    require_branch_access(pool.get_ref(), &claims, body.source_branch_id).await?;

    if body.quantity <= 0.0 {
        return Err(AppError::BadRequest(
            "quantity must be greater than 0".into(),
        ));
    }
    if body.source_branch_id == body.destination_branch_id {
        return Err(AppError::BadRequest(
            "Source and destination branches must be different".into(),
        ));
    }

    let src_org = branch_org(pool.get_ref(), body.source_branch_id)
        .await
        .map_err(|_| AppError::NotFound("Source branch not found".into()))?;
    let dst_org = branch_org(pool.get_ref(), body.destination_branch_id)
        .await
        .map_err(|_| AppError::NotFound("Destination branch not found".into()))?;
    if src_org != dst_org {
        return Err(AppError::BadRequest(
            "Both branches must belong to the same organization".into(),
        ));
    }
    ensure_ingredient_in_org(pool.get_ref(), body.org_ingredient_id, src_org).await?;

    let mut tx = pool.get_ref().begin().await?;

    // Lock the source balance and validate atomically (no TOCTOU with a
    // concurrent transfer or sale on the same ingredient).
    let src_on_hand = lock_on_hand(&mut *tx, body.source_branch_id, body.org_ingredient_id)
        .await?
        .unwrap_or(0.0);
    if src_on_hand < body.quantity {
        return Err(AppError::BadRequest(format!(
            "Only {src_on_hand:.3} on hand at the source branch — cannot transfer {:.3}.",
            body.quantity
        )));
    }

    // Cost travels with the goods: value both legs at the source branch's actual
    // cost (org default fallback) and blend it into the destination's WAC before
    // the stock lands there (WAC reads the destination's prior on-hand).
    let src_cost_dec: Option<Decimal> = sqlx::query_scalar(
        "SELECT COALESCE(bs.cost_per_unit, oi.cost_per_unit) \
         FROM org_ingredients oi \
         LEFT JOIN branch_stock bs ON bs.org_ingredient_id = oi.id AND bs.branch_id = $2 \
         WHERE oi.id = $1",
    )
    .bind(body.org_ingredient_id)
    .bind(body.source_branch_id)
    .fetch_one(&mut *tx)
    .await?;
    if let Some(cost) = src_cost_dec {
        let qty_dec = Decimal::from_f64_retain(body.quantity)
            .unwrap_or(Decimal::ZERO)
            .round_dp(3);
        crate::costing::service::apply_weighted_average_cost(
            &mut tx,
            body.destination_branch_id,
            body.org_ingredient_id,
            qty_dec,
            cost,
            claims.user_id(),
        )
        .await?;
    }
    let unit_cost = src_cost_dec.map(|c| {
        use rust_decimal::prelude::ToPrimitive;
        c.round().to_i64().unwrap_or(0)
    });

    let transfer_id: Uuid = sqlx::query_scalar(
        "INSERT INTO stock_transfers \
             (org_id, source_branch_id, destination_branch_id, org_ingredient_id, quantity, note, initiated_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(src_org)
    .bind(body.source_branch_id)
    .bind(body.destination_branch_id)
    .bind(body.org_ingredient_id)
    .bind(body.quantity)
    .bind(&body.note)
    .bind(claims.user_id())
    .fetch_one(&mut *tx)
    .await?;

    record_movement(
        &mut *tx,
        MovementParams {
            branch_id: body.source_branch_id,
            org_ingredient_id: body.org_ingredient_id,
            movement_type: "transfer_out",
            quantity: -body.quantity,
            unit_cost,
            reason: None,
            source_type: Some("transfer"),
            source_id: Some(transfer_id),
            note: Some("Transfer out"),
            created_by: Some(claims.user_id()),
        },
    )
    .await?;
    record_movement(
        &mut *tx,
        MovementParams {
            branch_id: body.destination_branch_id,
            org_ingredient_id: body.org_ingredient_id,
            movement_type: "transfer_in",
            quantity: body.quantity,
            unit_cost,
            reason: None,
            source_type: Some("transfer"),
            source_id: Some(transfer_id),
            note: Some("Transfer in"),
            created_by: Some(claims.user_id()),
        },
    )
    .await?;

    let transfer = fetch_transfer(&mut *tx, transfer_id).await?;
    tx.commit().await?;
    Ok(HttpResponse::Created().json(transfer))
}

#[utoipa::path(
    get,
    path = "/inventory/branches/{branch_id}/transfers",
    tag = "inventory",
    params(("branch_id" = Uuid, Path, description = "Branch ID"), ListTransfersQuery),
    responses((status = 200, description = "List transfers", body = Vec<StockTransfer>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_transfers(
    req: HttpRequest,
    pool: crate::db::Db,
    branch_id: web::Path<Uuid>,
    query: web::Query<ListTransfersQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory_transfers", "read").await?;

    // nil UUID = every branch in the caller's org ("All branches"); any other
    // UUID is that one branch after the usual access check.
    let all_branches = branch_id.is_nil();
    let scope_id: Uuid = if all_branches {
        claims
            .scope_org(crate::auth::middleware::header_org_id(&req))
            .ok_or_else(|| AppError::Forbidden("No organization in scope".into()))?
    } else {
        require_branch_access(pool.get_ref(), &claims, *branch_id).await?;
        *branch_id
    };

    let condition = if all_branches {
        let org_branches = "(SELECT id FROM branches WHERE org_id = $1 AND deleted_at IS NULL)";
        match query.direction.as_deref() {
            Some("incoming") => format!("t.destination_branch_id IN {org_branches}"),
            Some("outgoing") => format!("t.source_branch_id IN {org_branches}"),
            _ => format!(
                "(t.source_branch_id IN {org_branches} OR t.destination_branch_id IN {org_branches})"
            ),
        }
    } else {
        match query.direction.as_deref() {
            Some("incoming") => "t.destination_branch_id = $1".to_string(),
            Some("outgoing") => "t.source_branch_id = $1".to_string(),
            _ => "(t.source_branch_id = $1 OR t.destination_branch_id = $1)".to_string(),
        }
    };

    let sql = format!(
        "{TRANSFER_SELECT} WHERE {condition} ORDER BY t.initiated_at DESC LIMIT $2 OFFSET $3"
    );
    let (limit, offset) = page_bounds(query.limit, query.offset);
    let rows = sqlx::query_as::<_, StockTransfer>(&sql)
        .bind(scope_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool.get_ref())
        .await?;

    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    patch,
    path = "/inventory/transfers/{id}",
    tag = "inventory",
    params(("id" = Uuid, Path, description = "Transfer ID")),
    request_body = UpdateTransferRequest,
    responses((status = 200, description = "Transfer updated", body = StockTransfer), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_transfer(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<UpdateTransferRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory_transfers", "update").await?;

    let transfer = fetch_transfer(pool.get_ref(), *id).await?;
    require_org_access(&claims, transfer.org_id)?;

    sqlx::query("UPDATE stock_transfers SET note = $2 WHERE id = $1")
        .bind(*id)
        .bind(&body.note)
        .execute(pool.get_ref())
        .await?;

    Ok(HttpResponse::Ok().json(fetch_transfer(pool.get_ref(), *id).await?))
}

#[utoipa::path(
    delete,
    path = "/inventory/transfers/{id}",
    tag = "inventory",
    params(("id" = Uuid, Path, description = "Transfer ID")),
    responses((status = 204, description = "Transfer reversed and deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_transfer(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory_transfers", "delete").await?;

    let transfer = fetch_transfer(pool.get_ref(), *id).await?;
    require_org_access(&claims, transfer.org_id)?;
    let qty: f64 = transfer.quantity.to_string().parse().unwrap_or(0.0);

    let mut tx = pool.get_ref().begin().await?;

    // The destination must still hold the goods to send them back.
    let dst_on_hand = lock_on_hand(
        &mut *tx,
        transfer.destination_branch_id,
        transfer.org_ingredient_id,
    )
    .await?
    .unwrap_or(0.0);
    if dst_on_hand < qty {
        return Err(AppError::Conflict(format!(
            "Cannot reverse transfer: the destination branch only has {dst_on_hand:.3} left of the {qty:.3} transferred."
        )));
    }

    let unit_cost = branch_unit_cost(
        &mut *tx,
        transfer.source_branch_id,
        transfer.org_ingredient_id,
    )
    .await?;

    record_movement(
        &mut *tx,
        MovementParams {
            branch_id: transfer.destination_branch_id,
            org_ingredient_id: transfer.org_ingredient_id,
            movement_type: "transfer_out",
            quantity: -qty,
            unit_cost,
            reason: None,
            source_type: Some("transfer"),
            source_id: Some(*id),
            note: Some("Transfer reversal"),
            created_by: Some(claims.user_id()),
        },
    )
    .await?;
    record_movement(
        &mut *tx,
        MovementParams {
            branch_id: transfer.source_branch_id,
            org_ingredient_id: transfer.org_ingredient_id,
            movement_type: "transfer_in",
            quantity: qty,
            unit_cost,
            reason: None,
            source_type: Some("transfer"),
            source_id: Some(*id),
            note: Some("Transfer reversal"),
            created_by: Some(claims.user_id()),
        },
    )
    .await?;

    sqlx::query("DELETE FROM stock_transfers WHERE id = $1")
        .bind(*id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(HttpResponse::NoContent().finish())
}

// ── Helpers ───────────────────────────────────────────────────

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

fn require_org_access(claims: &Claims, org_id: Uuid) -> Result<(), AppError> {
    if claims.role == UserRole::SuperAdmin {
        return Ok(());
    }
    if claims.org_id() != Some(org_id) {
        return Err(AppError::Forbidden("Access denied to this org".into()));
    }
    Ok(())
}

/// Lower-case `[a-z0-9_]` key from a human name; `None` when nothing is left.
pub fn slugify(name: &str) -> Option<String> {
    let mut out = String::with_capacity(name.len());
    let mut pending_sep = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_sep && !out.is_empty() {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
            pending_sep = false;
        } else {
            pending_sep = true;
        }
    }
    if out.is_empty() {
        None
    } else {
        out.truncate(64);
        Some(out)
    }
}

fn is_valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The org's `general` category id, creating it if the org predates
/// categories (idempotent).
pub async fn ensure_general_category<'e, E>(executor: E, org_id: Uuid) -> Result<Uuid, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let id: Uuid = sqlx::query_scalar("SELECT ingredient_category_id($1, 'general')")
        .bind(org_id)
        .fetch_one(executor)
        .await?;
    Ok(id)
}

async fn ensure_category_in_org<'e, E>(
    executor: E,
    category_id: Uuid,
    org_id: Uuid,
) -> Result<(), AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let found: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM ingredient_categories WHERE id = $1 AND org_id = $2")
            .bind(category_id)
            .bind(org_id)
            .fetch_optional(executor)
            .await?;
    if found.is_none() {
        return Err(AppError::BadRequest(
            "Category does not belong to this organization".into(),
        ));
    }
    Ok(())
}

async fn ensure_supplier_in_org<'e, E>(
    executor: E,
    supplier_id: Uuid,
    org_id: Uuid,
) -> Result<(), AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let found: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM suppliers WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL",
    )
    .bind(supplier_id)
    .bind(org_id)
    .fetch_optional(executor)
    .await?;
    if found.is_none() {
        return Err(AppError::BadRequest(
            "Supplier does not belong to this organization".into(),
        ));
    }
    Ok(())
}

pub async fn ensure_ingredient_in_org<'e, E>(
    executor: E,
    org_ingredient_id: Uuid,
    org_id: Uuid,
) -> Result<(), AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let found: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM org_ingredients WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL",
    )
    .bind(org_ingredient_id)
    .bind(org_id)
    .fetch_optional(executor)
    .await?;
    if found.is_none() {
        return Err(AppError::BadRequest(
            "Ingredient not found in this organization's catalog".into(),
        ));
    }
    Ok(())
}

pub async fn branch_org<'e, E>(executor: E, branch_id: Uuid) -> Result<Uuid, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
        .bind(branch_id)
        .fetch_optional(executor)
        .await?
        .ok_or_else(|| AppError::NotFound("Branch not found".into()))
}

/// Whole-piastre unit cost for a ledger row: the branch's actual cost, else
/// the org standard cost, else unknown.
pub async fn branch_unit_cost<'e, E>(
    executor: E,
    branch_id: Uuid,
    org_ingredient_id: Uuid,
) -> Result<Option<i64>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let cost: Option<f64> = sqlx::query_scalar(
        "SELECT COALESCE(bs.cost_per_unit, oi.cost_per_unit)::float8 \
         FROM org_ingredients oi \
         LEFT JOIN branch_stock bs ON bs.org_ingredient_id = oi.id AND bs.branch_id = $2 \
         WHERE oi.id = $1",
    )
    .bind(org_ingredient_id)
    .bind(branch_id)
    .fetch_optional(executor)
    .await?
    .flatten();
    Ok(cost.map(|c| c.round() as i64))
}

async fn require_branch_access(
    pool: &PgPool,
    claims: &Claims,
    branch_id: Uuid,
) -> Result<(), AppError> {
    if claims.role == UserRole::SuperAdmin {
        return Ok(());
    }

    let branch_org: Option<Uuid> =
        sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
            .bind(branch_id)
            .fetch_optional(pool)
            .await?
            .flatten();

    let branch_org = branch_org.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    if claims.org_id() != Some(branch_org) {
        return Err(AppError::Forbidden(
            "Branch belongs to a different org".into(),
        ));
    }

    if claims.role == UserRole::OrgAdmin {
        return Ok(());
    }

    // D13: tellers are ORG-scoped, not branch-scoped — the org check above is the
    // boundary; any active org teller may e.g. log waste against this branch.
    if claims.role == UserRole::Teller {
        return Ok(());
    }

    // Branch managers stay branch-scoped via their explicit assignments.
    let assigned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM user_branch_assignments WHERE user_id = $1 AND branch_id = $2)"
    )
    .bind(claims.user_id())
    .bind(branch_id)
    .fetch_one(pool)
    .await?;

    if !assigned {
        return Err(AppError::Forbidden("Not assigned to this branch".into()));
    }

    Ok(())
}

fn validate_unit(unit: &str) -> Result<(), AppError> {
    if crate::units::is_valid_unit(unit) {
        Ok(())
    } else {
        Err(AppError::BadRequest(
            "Unit must be one of: g, kg, ml, l, pcs".into(),
        ))
    }
}

fn validate_waste_reason(reason: &str) -> Result<(), AppError> {
    match reason {
        // `order_cancelled` is the system reason auto-logged when a made order is
        // voided / a delivery is cancelled without restock; kept distinct from
        // `overproduction` (a kitchen-forecasting signal) so waste-by-reason
        // reports stay honest.
        "expired" | "spoiled" | "damaged" | "overproduction" | "order_cancelled" | "theft" | "other" => Ok(()),
        _ => Err(AppError::BadRequest(
            "reason must be one of: expired, spoiled, damaged, overproduction, order_cancelled, theft, other".into(),
        )),
    }
}

async fn fetch_category<'e, E>(executor: E, id: Uuid) -> Result<IngredientCategory, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let sql = format!("{CATEGORY_SELECT} WHERE c.id = $1");
    sqlx::query_as::<_, IngredientCategory>(&sql)
        .bind(id)
        .fetch_one(executor)
        .await
        .map_err(AppError::Db)
}

async fn fetch_ingredient<'e, E>(executor: E, id: Uuid) -> Result<OrgIngredient, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let sql = format!("{ORG_INGREDIENT_SELECT} WHERE oi.id = $1");
    sqlx::query_as::<_, OrgIngredient>(&sql)
        .bind(id)
        .fetch_one(executor)
        .await
        .map_err(AppError::Db)
}

async fn fetch_branch_stock_row<'e, E>(
    executor: E,
    branch_id: Uuid,
    org_ingredient_id: Uuid,
) -> Result<BranchStockRow, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let sql = format!("{BRANCH_STOCK_SELECT} WHERE oi.id = $2");
    sqlx::query_as::<_, BranchStockRow>(&sql)
        .bind(branch_id)
        .bind(org_ingredient_id)
        .fetch_one(executor)
        .await
        .map_err(AppError::Db)
}

async fn fetch_transfer<'e, E>(executor: E, id: Uuid) -> Result<StockTransfer, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let sql = format!("{TRANSFER_SELECT} WHERE t.id = $1");
    sqlx::query_as::<_, StockTransfer>(&sql)
        .bind(id)
        .fetch_optional(executor)
        .await?
        .ok_or_else(|| AppError::NotFound("Transfer not found".into()))
}

async fn fetch_movement<'e, E>(executor: E, id: Uuid) -> Result<StockMovement, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_as::<_, StockMovement>(
        r#"
        SELECT
            m.id, m.branch_id, m.org_ingredient_id,
            oi.name AS ingredient_name, oi.unit::text AS unit,
            m.branch_stock_id, m.type::text AS movement_type,
            m.quantity, m.balance_after, m.unit_cost, m.reason, m.below_zero,
            m.source_type, m.source_id, m.note, m.created_by,
            u.name AS created_by_name, m.created_at
        FROM inventory_movements m
        JOIN org_ingredients oi ON oi.id = m.org_ingredient_id
        LEFT JOIN users u       ON u.id  = m.created_by
        WHERE m.id = $1
        "#,
    )
    .bind(id)
    .fetch_one(executor)
    .await
    .map_err(AppError::Db)
}
