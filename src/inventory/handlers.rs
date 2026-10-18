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
    /// Cups, lids, straws: a dine-in sale skips every ingredient in a packaging
    /// category. The slug `packaging` is treated as packaging too.
    pub is_packaging: bool,
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
    /// Set when the ingredient is archived: hidden from pickers, stock-count
    /// sheets (once no branch holds stock) and reorder suggestions, but kept
    /// for history and still deducted by any recipe that uses it. `null` =
    /// active. Independent of `is_active` (which the POS feed reads).
    #[serde(default)]
    #[sqlx(default)]
    pub archived_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Who archived it; `null` when active.
    #[serde(default)]
    #[sqlx(default)]
    pub archived_by: Option<Uuid>,
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
    /// The ingredient's archive time; `null` = active. An archived ingredient
    /// still holding stock here keeps appearing so it can be counted or
    /// wasted down.
    #[serde(default)]
    #[sqlx(default)]
    pub archived_at: Option<chrono::DateTime<chrono::Utc>>,
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
    /// The ingredient's archive time (history keeps showing an archived
    /// ingredient, with a badge); `null` = active.
    #[serde(default)]
    #[sqlx(default)]
    pub ingredient_archived_at: Option<chrono::DateTime<chrono::Utc>>,
    pub branch_stock_id: Option<Uuid>,
    /// inventory_movement_type: sale | void_restock | adjustment_add |
    /// adjustment_remove | waste | transfer_out | transfer_in | purchase_in |
    /// purchase_return | stock_count
    pub movement_type: String,
    /// Signed delta applied to stock (consumption negative, replenishment positive).
    #[schema(value_type = f64)]
    #[serde(serialize_with = "crate::decimals::serialize")]
    pub quantity: sqlx::types::BigDecimal,
    #[schema(value_type = f64)]
    #[serde(serialize_with = "crate::decimals::serialize")]
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
    // ── Waste log only (null elsewhere) ──
    /// `pos` | `dashboard` | `refund` (a refunded sale's stock) | `order` (a
    /// made order voided before voids always restocked).
    #[serde(default)]
    #[sqlx(default)]
    pub waste_source: Option<String>,
    /// `ingredient` | `menu_item`, when the waste was recorded with a header.
    #[serde(default)]
    #[sqlx(default)]
    pub waste_subject_kind: Option<String>,
    /// What the person picked (the menu item for an exploded item waste).
    #[serde(default)]
    #[sqlx(default)]
    pub waste_subject_name: Option<String>,
    #[serde(default)]
    #[sqlx(default)]
    pub waste_size_label: Option<String>,
    /// The quantity as the person typed it, in `waste_unit`.
    #[serde(default)]
    #[sqlx(default)]
    pub waste_quantity: Option<f64>,
    #[serde(default)]
    #[sqlx(default)]
    pub waste_unit: Option<String>,
    /// The whole waste's value (all its lines), piastres.
    #[serde(default)]
    #[sqlx(default)]
    pub waste_value_minor: Option<i64>,
    /// When the waste HAPPENED: the device's time for a till waste, the
    /// refund's `issued_at`, the void's `voided_at`, else the post time. The
    /// log is ordered by it.
    #[serde(default)]
    #[sqlx(default)]
    pub occurred_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When the server received it (= `created_at`). Differs from
    /// `occurred_at` when a till queued the waste offline.
    #[serde(default)]
    #[sqlx(default)]
    pub received_at: Option<chrono::DateTime<chrono::Utc>>,
    /// A refund's waste: the refund it came from.
    #[serde(default)]
    #[sqlx(default)]
    pub refund_id: Option<Uuid>,
    /// A refund's or void's waste: the sale.
    #[serde(default)]
    #[sqlx(default)]
    pub order_id: Option<Uuid>,
    #[serde(default)]
    #[sqlx(default)]
    pub order_display_number: Option<String>,
    #[serde(default)]
    #[sqlx(default)]
    pub device_id: Option<Uuid>,
    #[serde(default)]
    #[sqlx(default)]
    pub device_name: Option<String>,
    #[serde(default)]
    #[sqlx(default)]
    pub till_id: Option<Uuid>,
    /// The manager who approved it on the till.
    #[serde(default)]
    #[sqlx(default)]
    pub approved_by_name: Option<String>,
}

// ── Request types ─────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct CreateIngredientCategoryRequest {
    pub name: String,
    /// Optional explicit slug (`[a-z0-9_]`); derived from the name when omitted.
    pub slug: Option<String>,
    pub sort_order: Option<i32>,
    /// Defaults to `true` for slug `packaging`, else `false`.
    #[serde(default)]
    pub is_packaging: Option<bool>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateIngredientCategoryRequest {
    pub name: Option<String>,
    pub sort_order: Option<i32>,
    #[serde(default)]
    pub is_packaging: Option<bool>,
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

/// Which ingredients a catalog or stock list returns, by archive state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ArchiveFilter {
    /// Not archived: what pickers offer.
    Active,
    /// Archived only (the "Archived" tab).
    Archived,
    /// Both, archived rows carrying `archived_at`.
    All,
}

impl ArchiveFilter {
    /// The SQL condition over `oi` (an `org_ingredients` alias).
    fn sql(self) -> &'static str {
        match self {
            ArchiveFilter::Active => " AND oi.archived_at IS NULL",
            ArchiveFilter::Archived => " AND oi.archived_at IS NOT NULL",
            ArchiveFilter::All => "",
        }
    }
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ArchiveStatusQuery {
    /// `active` | `archived` | `all`. Omitted = `all`: every non-deleted
    /// ingredient, exactly the rows older dashboards and tills read (they look
    /// names and costs up in this list, so an archived ingredient a recipe
    /// still uses must stay in it). Pickers ask for `active`.
    pub status: Option<ArchiveFilter>,
}

impl ArchiveStatusQuery {
    fn filter(&self) -> ArchiveFilter {
        self.status.unwrap_or(ArchiveFilter::All)
    }
}

// ── Shared SELECT fragments ───────────────────────────────────

const ORG_INGREDIENT_SELECT: &str = r#"
    SELECT oi.id, oi.org_id, oi.name, oi.unit::text AS unit,
           oi.category_id, ic.slug AS category_slug, ic.name AS category_name,
           oi.description, oi.cost_per_unit,
           oi.supplier_id, s.name AS supplier_name,
           oi.pack_unit, oi.pack_size, oi.yield_pct, oi.density_g_per_ml,
           oi.is_active, oi.created_at, oi.updated_at,
           oi.archived_at, oi.archived_by
    FROM org_ingredients oi
    JOIN ingredient_categories ic ON ic.id = oi.category_id
    LEFT JOIN suppliers s ON s.id = oi.supplier_id
"#;

const CATEGORY_SELECT: &str = r#"
    SELECT c.id, c.org_id, c.slug, c.name, c.sort_order, c.is_packaging,
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
           (bs.id IS NOT NULL) AS has_activity,
           oi.archived_at
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
        "INSERT INTO ingredient_categories (org_id, slug, name, sort_order, is_packaging) \
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(*org_id)
    .bind(&slug)
    .bind(name)
    .bind(body.sort_order.unwrap_or(10))
    .bind(body.is_packaging.unwrap_or(slug == "packaging"))
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
         SET name = COALESCE($3, name), sort_order = COALESCE($4, sort_order), \
             is_packaging = COALESCE($5, is_packaging) \
         WHERE id = $1 AND org_id = $2 RETURNING id",
    )
    .bind(id)
    .bind(org_id)
    .bind(name)
    .bind(body.sort_order)
    .bind(body.is_packaging)
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
    params(("org_id" = Uuid, Path, description = "Organization ID"), ArchiveStatusQuery),
    responses((status = 200, description = "List catalog items", body = Vec<OrgIngredient>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_catalog(
    req: HttpRequest,
    pool: crate::db::Db,
    org_id: web::Path<Uuid>,
    query: web::Query<ArchiveStatusQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_org_access(&claims, *org_id)?;

    let sql = format!(
        "{ORG_INGREDIENT_SELECT} WHERE oi.org_id = $1 AND oi.deleted_at IS NULL{} ORDER BY oi.name",
        query.filter().sql()
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
    let id: Result<Uuid, sqlx::Error> = sqlx::query_scalar(
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
    .await;
    let id = match id {
        Ok(id) => id,
        Err(e) if is_unique_violation(&e) => {
            drop(tx);
            return Err(name_taken(pool.get_ref(), *org_id, body.name.trim()).await);
        }
        Err(e) => return Err(AppError::Db(e)),
    };

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
        // (The legacy recipe tables were rebased here in lockstep until the
        // menu-unification flip. Every database has them as read-only views
        // over recipe_lines since DB Phase 2.4, so recipe_lines above is all.)
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
            // (The legacy recipe tables are views over recipe_lines since DB
            // Phase 2.4: the rebase above is the only one.)
        }
    }

    let updated: Result<Option<Uuid>, sqlx::Error> = sqlx::query_scalar(
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
    .await;
    let updated = match updated {
        Ok(u) => u,
        Err(e) if is_unique_violation(&e) => {
            drop(tx);
            let name = body.name.as_deref().map(str::trim).unwrap_or_default();
            return Err(name_taken(pool.get_ref(), org_id, name).await);
        }
        Err(e) => return Err(AppError::Db(e)),
    };
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
                -- A size's recipe, an add-on's or an optional field's line (what
                -- the old menu_item_recipes / addon_item_ingredients /
                -- menu_item_optional_fields views listed), on recipe_lines.
                SELECT 1 FROM recipe_lines r JOIN menu_item_sizes ms ON ms.id = r.owner_id
                 WHERE r.owner_type = 'item_size' AND r.ingredient_id = $1
                UNION ALL
                SELECT 1 FROM recipe_lines r JOIN modifier_options mo ON mo.id = r.owner_id
                 WHERE r.owner_type = 'modifier_option' AND r.size_label IS NULL
                   AND mo.legacy_source = 'addon' AND r.ingredient_id = $1
                UNION ALL
                SELECT 1 FROM recipe_lines r JOIN modifier_options mo ON mo.id = r.owner_id
                  JOIN menu_item_modifier_groups mimg
                       ON mimg.group_id = mo.group_id AND mimg.legacy_origin = 'options'
                 WHERE r.owner_type = 'modifier_option' AND r.size_label IS NULL
                   AND mo.legacy_source = 'optional' AND r.ingredient_id = $1
            ),
            EXISTS (SELECT 1 FROM branch_stock WHERE org_ingredient_id = $1 AND on_hand <> 0)
                -- In transit counts as stock: receiving or cancelling it needs the ingredient.
                OR EXISTS (
                    SELECT 1 FROM stock_transfer_lines l
                    JOIN stock_transfers t ON t.id = l.transfer_id
                    WHERE l.org_ingredient_id = $1
                      AND t.status IN ('requested', 'draft', 'dispatched')
                )
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
            "Ingredient still has stock at a branch or on an open transfer. Count or waste it to zero, and finish or cancel the transfer, first.".into(),
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

// ── Archive ───────────────────────────────────────────────────

/// One live thing that uses an ingredient.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, ToSchema)]
pub struct IngredientUsageRef {
    /// `recipe` (a menu item size's recipe) | `modifier_option` (an add-on's
    /// or option's recipe line) | `swap` (an option that replaces it, e.g. a
    /// milk swap) | `recipe_base` | `packaging_rule` | `purchase_order` (open:
    /// draft, ordered, partially received) | `stocktake` (open count) |
    /// `transfer` (requested, draft or dispatched).
    pub kind: String,
    /// The using entity's id (menu item, option, base, rule, PO, count, transfer).
    pub id: Uuid,
    /// Its display name (menu item / option / base / rule name, PO reference
    /// or supplier, the count's branch, `#<number>` for a transfer).
    pub name: String,
    /// Extra context: the size label, the option's group, the PO's branch, the
    /// transfer's route.
    pub detail: Option<String>,
}

/// Non-zero book stock of the ingredient at one branch.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, ToSchema)]
pub struct IngredientBranchStock {
    pub branch_id: Uuid,
    pub branch_name: String,
    /// In the ingredient's base unit; may be negative.
    pub on_hand: f64,
}

/// Where an ingredient is still used, for the archive confirmation.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, ToSchema)]
pub struct IngredientUsage {
    pub ingredient_id: Uuid,
    pub name: String,
    pub unit: String,
    pub archived_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Active recipes, add-on/option lines, swaps, bases, packaging rules and
    /// open documents that reference it. Archiving does NOT change them:
    /// those recipes keep deducting and costing the ingredient.
    pub usages: Vec<IngredientUsageRef>,
    /// Branches holding non-zero stock of it. Archiving leaves the balances
    /// alone; such a branch keeps seeing it in counts until it reaches zero.
    pub stock: Vec<IngredientBranchStock>,
    /// `usages` or `stock` is non-empty: an archive must send `confirm: true`.
    pub needs_confirmation: bool,
    /// Nothing references it at all, history included (no movement, recipe,
    /// PO, count, transfer or waste row), so a delete loses nothing.
    pub never_used: bool,
}

#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct ArchiveIngredientRequest {
    /// Required (`true`) when the ingredient is still used or holds stock
    /// (`GET …/usage` says so); otherwise the archive answers 409
    /// `ARCHIVE_NEEDS_CONFIRMATION`.
    #[serde(default)]
    pub confirm: bool,
}

async fn load_usage(
    conn: &mut sqlx::PgConnection,
    org_id: Uuid,
    id: Uuid,
) -> Result<IngredientUsage, AppError> {
    let head = sqlx::query!(
        r#"SELECT name, unit::text AS "unit!", archived_at
             FROM org_ingredients
            WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL"#,
        id,
        org_id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| AppError::NotFound("Ingredient not found".into()))?;

    let usages = sqlx::query_as!(
        IngredientUsageRef,
        r#"
        SELECT u.kind AS "kind!", u.id AS "id!", u.name AS "name!", u.detail AS "detail?"
        FROM (
            SELECT DISTINCT 'recipe'::text AS kind, mi.id, mi.name, ms.label AS detail, 1 AS ord
              FROM recipe_lines rl
              JOIN menu_item_sizes ms ON ms.id = rl.owner_id
              JOIN menu_items mi      ON mi.id = ms.menu_item_id
             WHERE rl.owner_type = 'item_size' AND rl.ingredient_id = $1
               AND mi.org_id = $2 AND mi.deleted_at IS NULL AND mi.is_active AND ms.is_active
            UNION
            SELECT DISTINCT 'modifier_option', mo.id, mo.name, g.name, 2
              FROM recipe_lines rl
              JOIN modifier_options mo ON mo.id = rl.owner_id
              JOIN modifier_groups g   ON g.id = mo.group_id
             WHERE rl.owner_type = 'modifier_option' AND rl.ingredient_id = $1
               AND g.org_id = $2 AND mo.is_active AND g.is_active
            UNION
            SELECT 'swap', mo.id, mo.name, g.name, 3
              FROM modifier_options mo
              JOIN modifier_groups g ON g.id = mo.group_id
             WHERE mo.replaces_ingredient_id = $1
               AND g.org_id = $2 AND mo.is_active AND g.is_active
            UNION
            SELECT DISTINCT 'recipe_base', rb.id, rb.name, NULL, 4
              FROM recipe_base_lines bl
              JOIN recipe_bases rb ON rb.id = bl.base_id
             WHERE bl.ingredient_id = $1
               AND rb.org_id = $2 AND rb.deleted_at IS NULL AND rb.is_active
            UNION
            SELECT DISTINCT 'packaging_rule', pr.id, pr.name, NULL, 5
              FROM packaging_rule_lines pl
              JOIN packaging_rules pr ON pr.id = pl.rule_id
             WHERE pl.ingredient_id = $1 AND pr.org_id = $2 AND pr.is_active
            UNION
            SELECT DISTINCT 'purchase_order', po.id,
                   COALESCE(NULLIF(po.reference, ''), s.name, 'PO'), b.name, 6
              FROM purchase_order_lines l
              JOIN purchase_orders po ON po.id = l.purchase_order_id
              JOIN branches b         ON b.id = po.branch_id
              LEFT JOIN suppliers s   ON s.id = po.supplier_id
             WHERE l.org_ingredient_id = $1 AND po.org_id = $2
               AND po.status IN ('draft', 'ordered', 'partially_received')
            UNION
            SELECT DISTINCT 'stocktake', st.id, b.name, NULL, 7
              FROM stocktake_items si
              JOIN stocktakes st ON st.id = si.stocktake_id
              JOIN branches b    ON b.id = st.branch_id
             WHERE si.org_ingredient_id = $1 AND st.org_id = $2
               AND st.status IN ('draft', 'in_progress')
            UNION
            SELECT DISTINCT 'transfer', t.id, '#' || t.number::text, src.name || ' → ' || dst.name, 8
              FROM stock_transfer_lines l
              JOIN stock_transfers t ON t.id = l.transfer_id
              JOIN branches src      ON src.id = t.source_branch_id
              JOIN branches dst      ON dst.id = t.destination_branch_id
             WHERE l.org_ingredient_id = $1 AND t.org_id = $2
               AND t.status IN ('requested', 'draft', 'dispatched')
        ) u
        ORDER BY u.ord, u.name, u.detail NULLS FIRST
        "#,
        id,
        org_id
    )
    .fetch_all(&mut *conn)
    .await?;

    let stock = sqlx::query_as!(
        IngredientBranchStock,
        r#"
        SELECT bs.branch_id AS "branch_id!", b.name AS "branch_name!",
               bs.on_hand::float8 AS "on_hand!"
          FROM branch_stock bs
          JOIN branches b ON b.id = bs.branch_id AND b.deleted_at IS NULL
         WHERE bs.org_ingredient_id = $1 AND b.org_id = $2 AND bs.on_hand <> 0
         ORDER BY b.name
        "#,
        id,
        org_id
    )
    .fetch_all(&mut *conn)
    .await?;

    let never_used = sqlx::query_scalar!(
        r#"
        SELECT NOT (
               EXISTS (SELECT 1 FROM inventory_movements    WHERE org_ingredient_id = $1)
            OR EXISTS (SELECT 1 FROM recipe_lines           WHERE ingredient_id = $1)
            OR EXISTS (SELECT 1 FROM recipe_base_lines      WHERE ingredient_id = $1)
            OR EXISTS (SELECT 1 FROM packaging_rule_lines   WHERE ingredient_id = $1)
            OR EXISTS (SELECT 1 FROM modifier_options       WHERE replaces_ingredient_id = $1)
            OR EXISTS (SELECT 1 FROM purchase_order_lines   WHERE org_ingredient_id = $1)
            OR EXISTS (SELECT 1 FROM goods_receipt_lines    WHERE org_ingredient_id = $1)
            OR EXISTS (SELECT 1 FROM stocktake_items        WHERE org_ingredient_id = $1)
            OR EXISTS (SELECT 1 FROM stock_transfer_lines   WHERE org_ingredient_id = $1)
            OR EXISTS (SELECT 1 FROM waste_events           WHERE org_ingredient_id = $1)
            OR EXISTS (SELECT 1 FROM branch_stock           WHERE org_ingredient_id = $1 AND on_hand <> 0)
        ) AS "never_used!"
        "#,
        id
    )
    .fetch_one(&mut *conn)
    .await?;

    let needs_confirmation = !usages.is_empty() || !stock.is_empty();
    Ok(IngredientUsage {
        ingredient_id: id,
        name: head.name,
        unit: head.unit,
        archived_at: head.archived_at,
        usages,
        stock,
        needs_confirmation,
        never_used,
    })
}

#[utoipa::path(
    get,
    path = "/inventory/orgs/{org_id}/catalog/{id}/usage",
    tag = "inventory",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID"),
        ("id" = Uuid, Path, description = "Ingredient ID")
    ),
    responses((status = 200, description = "Where the ingredient is still used, and the stock it holds", body = IngredientUsage), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_catalog_item_usage(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    let (org_id, id) = path.into_inner();
    require_org_access(&claims, org_id)?;
    let mut conn = pool.get_ref().acquire().await?;
    let usage = load_usage(&mut conn, org_id, id).await?;
    Ok(HttpResponse::Ok().json(usage))
}

#[utoipa::path(
    post,
    path = "/inventory/orgs/{org_id}/catalog/{id}/archive",
    tag = "inventory",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID"),
        ("id" = Uuid, Path, description = "Ingredient ID")
    ),
    request_body(content = Option<ArchiveIngredientRequest>, description = "`{confirm: true}` when the ingredient is still used or holds stock"),
    responses(
        (status = 200, description = "Archived (idempotent: an archived ingredient is returned unchanged)", body = OrgIngredient),
        (status = 409, description = "`ARCHIVE_NEEDS_CONFIRMATION`: still used or holding stock; resend with `confirm: true`"),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
/// Archive an ingredient: it leaves pickers, reorder suggestions and (once no
/// branch holds stock) stock-count sheets. Nothing else changes: recipes that
/// use it keep deducting and costing it, every till keeps it in its mirror,
/// history keeps showing it, no movement is posted and no balance moves.
pub async fn archive_catalog_item(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(Uuid, Uuid)>,
    body: Option<web::Json<ArchiveIngredientRequest>>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "update").await?;
    let (org_id, id) = path.into_inner();
    require_org_access(&claims, org_id)?;
    let confirm = body.map(|b| b.confirm).unwrap_or(false);

    let mut tx = pool.get_ref().begin().await?;
    let locked = sqlx::query!(
        "SELECT archived_at FROM org_ingredients \
         WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL FOR UPDATE",
        id,
        org_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound("Ingredient not found".into()))?;

    if locked.archived_at.is_none() {
        let usage = load_usage(&mut tx, org_id, id).await?;
        if usage.needs_confirmation && !confirm {
            return Err(AppError::CodedVars {
                status: 409,
                code: "ARCHIVE_NEEDS_CONFIRMATION",
                reason: format!(
                    "This ingredient is still used in {} place(s) and has stock at {} branch(es). \
                     Archiving keeps those recipes deducting it; confirm to archive.",
                    usage.usages.len(),
                    usage.stock.len()
                ),
                vars: serde_json::json!({
                    "usage_count": usage.usages.len(),
                    "branches_with_stock": usage.stock.len(),
                }),
            });
        }
        sqlx::query!(
            "UPDATE org_ingredients SET archived_at = now(), archived_by = $3 \
             WHERE id = $1 AND org_id = $2",
            id,
            org_id,
            claims.user_id()
        )
        .execute(&mut *tx)
        .await?;
    }

    let row = fetch_ingredient(&mut *tx, id).await?;
    tx.commit().await?;
    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    post,
    path = "/inventory/orgs/{org_id}/catalog/{id}/unarchive",
    tag = "inventory",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID"),
        ("id" = Uuid, Path, description = "Ingredient ID")
    ),
    responses((status = 200, description = "Active again (idempotent)", body = OrgIngredient), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
/// Bring an archived ingredient back: pickers and counts list it again. Its
/// name never left it, so this cannot conflict.
pub async fn unarchive_catalog_item(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "update").await?;
    let (org_id, id) = path.into_inner();
    require_org_access(&claims, org_id)?;

    let mut tx = pool.get_ref().begin().await?;
    let found = sqlx::query_scalar!(
        "UPDATE org_ingredients SET archived_at = NULL, archived_by = NULL \
         WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL \
         RETURNING id",
        id,
        org_id
    )
    .fetch_optional(&mut *tx)
    .await?;
    found.ok_or_else(|| AppError::NotFound("Ingredient not found".into()))?;
    let row = fetch_ingredient(&mut *tx, id).await?;
    tx.commit().await?;
    Ok(HttpResponse::Ok().json(row))
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
    params(("branch_id" = Uuid, Path, description = "Branch ID"), ArchiveStatusQuery),
    responses((status = 200, description = "Every catalog ingredient as seen from this branch", body = Vec<BranchStockRow>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_branch_stock(
    req: HttpRequest,
    pool: crate::db::Db,
    branch_id: web::Path<Uuid>,
    query: web::Query<ArchiveStatusQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;
    let org_id = branch_org(pool.get_ref(), *branch_id).await?;

    let sql = format!(
        "{BRANCH_STOCK_SELECT} WHERE oi.org_id = $2 AND oi.deleted_at IS NULL{} \
         ORDER BY ic.sort_order, ic.name, oi.name",
        query.filter().sql()
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
            oi.archived_at AS ingredient_archived_at,
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
    crate::authz::require::require(
        pool.get_ref(),
        &claims,
        crate::authz::Cap::InventoryWasteRecord,
        None,
    )
    .await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    validate_waste_reason(&body.reason)?;
    if body.quantity <= 0.0 {
        return Err(AppError::BadRequest(
            "quantity must be greater than 0".into(),
        ));
    }
    let org_id = branch_org(pool.get_ref(), *branch_id).await?;
    ensure_ingredient_in_org(pool.get_ref(), body.org_ingredient_id, org_id).await?;

    // The `max_value` limit, judged at the branch (same rule as the till's route).
    let cost = crate::inventory::waste::exact_unit_cost(
        pool.get_ref(),
        *branch_id,
        body.org_ingredient_id,
    )
    .await?;
    let (value, _) =
        crate::inventory::waste::value_of(&[(body.org_ingredient_id, body.quantity, cost)])
            .map_err(|_| {
                AppError::BadRequest(
                    "This waste does not come to a real amount. Check the quantity and the \
                     ingredient's cost."
                        .into(),
                )
            })?;
    let eff =
        crate::authz::require::effective_for_claims(pool.get_ref(), &claims, Some(*branch_id))
            .await?;
    if !madar_authz::decide(&eff, &crate::inventory::waste::limit_request(value)).is_allow() {
        return Err(AppError::Forbidden(
            "This waste is over your limit or outside your branches.".into(),
        ));
    }

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
    crate::authz::require::require(
        pool.get_ref(),
        &claims,
        crate::authz::Cap::InventoryWasteRead,
        None,
    )
    .await?;
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
            oi.archived_at AS ingredient_archived_at,
            m.branch_stock_id, m.type::text AS movement_type,
            m.quantity, m.balance_after, m.unit_cost, m.reason, m.below_zero,
            m.source_type, m.source_id, m.note, m.created_by,
            u.name AS created_by_name, m.created_at,
            CASE WHEN we.id IS NOT NULL THEN we.source
                 WHEN m.source_type = 'order' THEN 'order'
                 WHEN m.source_type = 'refund' THEN 'refund'
                 ELSE 'dashboard' END AS waste_source,
            we.subject_kind AS waste_subject_kind,
            we.subject_name AS waste_subject_name,
            we.size_label AS waste_size_label,
            we.quantity::float8 AS waste_quantity,
            we.unit AS waste_unit,
            we.value_minor AS waste_value_minor,
            COALESCE(we.occurred_at, rf.issued_at, o.voided_at, m.created_at) AS occurred_at,
            m.created_at AS received_at,
            rf.id AS refund_id,
            COALESCE(rf.order_id, o.id) AS order_id,
            COALESCE(ro.device_code || '-' || ro.order_number, ro.order_number::text,
                     o.device_code || '-' || o.order_number, o.order_number::text) AS order_display_number,
            we.device_id,
            COALESCE(d.label, d.code) AS device_name,
            we.till_id,
            ap.name AS approved_by_name
        FROM inventory_movements m
        JOIN org_ingredients oi ON oi.id = m.org_ingredient_id
        JOIN branches b         ON b.id  = m.branch_id
        LEFT JOIN users u       ON u.id  = m.created_by
        LEFT JOIN waste_events we ON m.source_type = 'waste' AND we.id = m.source_id
        LEFT JOIN order_refunds rf ON m.source_type = 'refund' AND rf.id = m.source_id
        LEFT JOIN orders ro     ON ro.id = rf.order_id
        LEFT JOIN orders o      ON m.source_type = 'order' AND o.id = m.source_id
        LEFT JOIN devices d     ON d.id  = COALESCE(we.device_id, rf.device_id)
        LEFT JOIN approvals a   ON a.id  = we.approval_id
        LEFT JOIN users ap      ON ap.id = a.approver_user_id
        WHERE {scope_condition} AND m.type = 'waste'
        -- By when the waste HAPPENED (a queued till waste lands later), never
        -- by when the server posted it; `received_at` carries the latter.
        ORDER BY COALESCE(we.occurred_at, rf.issued_at, o.voided_at, m.created_at) DESC, m.id DESC
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

// ── Helpers ───────────────────────────────────────────────────

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
}

/// The refusal for a name the catalog already holds. An archived ingredient
/// keeps its name (the partial unique index only frees a DELETED row's), so
/// when the holder is archived the client is told to unarchive it instead —
/// one ingredient, one history, rather than a twin with an empty ledger.
async fn name_taken(pool: &PgPool, org_id: Uuid, name: &str) -> AppError {
    let archived = sqlx::query!(
        "SELECT id, name FROM org_ingredients \
         WHERE org_id = $1 AND name = $2 AND deleted_at IS NULL AND archived_at IS NOT NULL",
        org_id,
        name
    )
    .fetch_optional(pool)
    .await;
    match archived {
        Ok(Some(row)) => AppError::CodedVars {
            status: 409,
            code: "INGREDIENT_NAME_ARCHIVED",
            reason: "An archived ingredient already has this name. Unarchive it instead of creating a new one.".into(),
            vars: serde_json::json!({ "ingredient_id": row.id, "name": row.name }),
        },
        _ => AppError::Conflict("An ingredient with this name already exists in the catalog".into()),
    }
}

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
) -> Result<Option<Decimal>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    // Exact (numeric(20,6) piastres): the ledger keeps it whole and rounds
    // only its legacy bigint column.
    let cost: Option<Decimal> = sqlx::query_scalar(
        "SELECT COALESCE(bs.cost_per_unit, oi.cost_per_unit) \
         FROM org_ingredients oi \
         LEFT JOIN branch_stock bs ON bs.org_ingredient_id = oi.id AND bs.branch_id = $2 \
         WHERE oi.id = $1",
    )
    .bind(org_ingredient_id)
    .bind(branch_id)
    .fetch_optional(executor)
    .await?
    .flatten();
    Ok(cost)
}

async fn require_branch_access(
    pool: &PgPool,
    claims: &Claims,
    branch_id: Uuid,
) -> Result<(), AppError> {
    // Architecture E: the branches a person may act on come from their live
    // role assignments, not from their role name (see `authz::scope`).
    crate::authz::scope::require_branch_access(pool, claims, branch_id).await
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

async fn fetch_movement<'e, E>(executor: E, id: Uuid) -> Result<StockMovement, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_as::<_, StockMovement>(
        r#"
        SELECT
            m.id, m.branch_id, m.org_ingredient_id,
            oi.name AS ingredient_name, oi.unit::text AS unit,
            oi.archived_at AS ingredient_archived_at,
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
