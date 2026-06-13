use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    errors::{AppError, AppErrorResponse},
    inventory::movements::{record_movement, MovementParams},
    models::UserRole,
    permissions::checker::check_permission,
};
use utoipa::{IntoParams, ToSchema};

// ── Response models ───────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct OrgIngredient {
    pub id:            Uuid,
    pub org_id:        Uuid,
    pub name:          String,
    pub unit:          String,
    pub category:      String,
    pub description:   Option<String>,
    /// Piastres per unit. `null` ⟺ never entered (unknown, NOT free) —
    /// recipes using this ingredient are cost-missing everywhere.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schema(value_type = Option<f64>)]
    pub cost_per_unit: Option<Decimal>,
    /// Default supplier for reordering this ingredient; `null` = none set.
    pub supplier_id:   Option<Uuid>,
    pub supplier_name: Option<String>,
    pub is_active:     bool,
    pub created_at:    chrono::DateTime<chrono::Utc>,
    pub updated_at:    chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct BranchInventoryItem {
    pub id:                Uuid,
    pub branch_id:         Uuid,
    pub org_ingredient_id: Uuid,
    pub ingredient_name:   String,
    pub unit:              String,
    pub description:       Option<String>,
    /// Piastres per unit; `null` ⟺ cost never entered.
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schema(value_type = Option<f64>)]
    pub cost_per_unit:     Option<Decimal>,
    #[schema(value_type = f64)]
    pub current_stock:     sqlx::types::BigDecimal,
    #[schema(value_type = f64)]
    pub reorder_threshold: sqlx::types::BigDecimal,
    pub below_reorder:     bool,
    /// When this item was last reconciled by a finalized stock count; `null` =
    /// never counted. Drives the "count due" signal on the inventory home.
    pub last_counted_at:   Option<chrono::DateTime<chrono::Utc>>,
    pub created_at:        chrono::DateTime<chrono::Utc>,
    pub updated_at:        chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct BranchInventoryAdjustment {
    pub id:                  Uuid,
    pub branch_id:           Uuid,
    pub branch_inventory_id: Uuid,
    pub ingredient_name:     String,
    pub unit:                String,
    pub adjustment_type:     String,
    #[schema(value_type = f64)]
    pub quantity:            sqlx::types::BigDecimal,
    pub note:                String,
    pub transfer_id:         Option<Uuid>,
    pub adjusted_by:         Uuid,
    pub adjusted_by_name:    String,
    pub created_at:          chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct BranchInventoryTransfer {
    pub id:                      Uuid,
    pub org_id:                  Uuid,
    pub source_branch_id:        Uuid,
    pub source_branch_name:      String,
    pub destination_branch_id:   Uuid,
    pub destination_branch_name: String,
    pub org_ingredient_id:       Uuid,
    pub ingredient_name:         String,
    pub unit:                    String,
    #[schema(value_type = f64)]
    pub quantity:                sqlx::types::BigDecimal,
    pub note:                    Option<String>,
    pub initiated_by:            Uuid,
    pub initiated_by_name:       String,
    pub initiated_at:            chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct BranchInventoryMovement {
    pub id:                  Uuid,
    pub branch_id:           Uuid,
    pub org_ingredient_id:   Uuid,
    pub ingredient_name:     String,
    pub unit:                String,
    pub branch_inventory_id: Option<Uuid>,
    /// inventory_movement_type: sale | void_restock | adjustment_add |
    /// adjustment_remove | waste | transfer_out | transfer_in | purchase_in | stock_count
    pub movement_type:       String,
    /// Signed delta applied to stock (consumption negative, replenishment positive).
    #[schema(value_type = f64)]
    pub quantity:            sqlx::types::BigDecimal,
    #[schema(value_type = Option<f64>)]
    pub balance_after:       Option<sqlx::types::BigDecimal>,
    /// Piastres per unit at movement time; `null` ⟺ unknown.
    pub unit_cost:           Option<i64>,
    pub reason:              Option<String>,
    pub below_zero:          bool,
    pub source_type:         Option<String>,
    pub source_id:           Option<Uuid>,
    pub note:                Option<String>,
    pub created_by:          Option<Uuid>,
    pub created_by_name:     Option<String>,
    pub created_at:          chrono::DateTime<chrono::Utc>,
}

// ── Request types ─────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct CreateCatalogItemRequest {
    pub name:          String,
    pub unit:          String,
    pub category:      String,
    pub description:   Option<String>,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schema(value_type = Option<f64>)]
    pub cost_per_unit: Option<Decimal>,
    /// Optional default supplier for reordering.
    pub supplier_id:   Option<Uuid>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateCatalogItemRequest {
    pub name:          Option<String>,
    pub unit:          Option<String>,
    pub category:      Option<String>,
    pub description:   Option<String>,
    #[serde(default, with = "rust_decimal::serde::float_option")]
    #[schema(value_type = Option<f64>)]
    pub cost_per_unit: Option<Decimal>,
    /// Set/replace the default supplier. (Omitted = unchanged; clearing to
    /// none is not supported via this field.)
    pub supplier_id:   Option<Uuid>,
    pub is_active:     Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct OrgInventorySettings {
    /// Stock-count variance tolerance (percent). A counted row whose |difference|
    /// is at least this percent of expected is flagged and needs a reason.
    pub stocktake_variance_threshold_pct: f64,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateInventorySettingsRequest {
    pub stocktake_variance_threshold_pct: f64,
}

#[derive(Deserialize, ToSchema)]
pub struct AddToStockRequest {
    pub org_ingredient_id: Uuid,
    pub current_stock:     Option<f64>,
    pub reorder_threshold: Option<f64>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateStockRequest {
    pub reorder_threshold: Option<f64>,
    pub current_stock:     Option<f64>,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateAdjustmentRequest {
    pub branch_inventory_id: Uuid,
    pub adjustment_type:     String, // "add" | "remove"
    pub quantity:            f64,
    pub note:                String,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateTransferRequest {
    pub source_branch_id:      Uuid,
    pub destination_branch_id: Uuid,
    pub org_ingredient_id:     Uuid,
    pub quantity:              f64,
    pub note:                  Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateTransferRequest {
    pub note: Option<String>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListTransfersQuery {
    pub direction: Option<String>, // "incoming" | "outgoing" | None = both
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListMovementsQuery {
    pub org_ingredient_id: Option<Uuid>,
    #[serde(rename = "type")]
    pub movement_type:     Option<String>,
    pub from:              Option<chrono::DateTime<chrono::Utc>>,
    pub to:                Option<chrono::DateTime<chrono::Utc>>,
    pub page:              Option<i64>,
    pub per_page:          Option<i64>,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateWasteRequest {
    pub org_ingredient_id: Uuid,
    pub quantity:          f64,
    /// expired | spoiled | damaged | overproduction | theft | other
    pub reason:            String,
    pub note:              Option<String>,
}

// ── GET /inventory/orgs/:org_id/catalog ──────────────────────

#[utoipa::path(
    get,
    path = "/inventory/orgs/{org_id}/catalog",
    tag = "inventory",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    responses((status = 200, description = "List catalog items", body = Vec<OrgIngredient>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_catalog(
    req:    HttpRequest,
    pool:   web::Data<PgPool>,
    org_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_org_access(&claims, *org_id)?;

    let rows = sqlx::query_as::<_, OrgIngredient>(
        r#"
        SELECT oi.id, oi.org_id, oi.name, oi.unit::text, oi.category, oi.description, oi.cost_per_unit,
               oi.supplier_id,
               (SELECT name FROM suppliers WHERE id = oi.supplier_id) AS supplier_name,
               oi.is_active, oi.created_at, oi.updated_at
        FROM org_ingredients oi
        WHERE oi.org_id = $1 AND oi.deleted_at IS NULL
        ORDER BY oi.name
        "#,
    )
    .bind(*org_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── POST /inventory/orgs/:org_id/catalog ─────────────────────

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
    req:    HttpRequest,
    pool:   web::Data<PgPool>,
    org_id: web::Path<Uuid>,
    body:   web::Json<CreateCatalogItemRequest>,
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

    // No cost supplied ⟹ stored as NULL = unknown. Never default to 0 —
    // zero means "genuinely free" and would flow into every cost rollup.
    let mut tx = pool.get_ref().begin().await?;

    let row = sqlx::query_as::<_, OrgIngredient>(
        r#"
        INSERT INTO org_ingredients (org_id, name, unit, category, description, cost_per_unit, supplier_id)
        VALUES ($1, $2, $3::inventory_unit, $4, $5, $6, $7)
        RETURNING id, org_id, name, unit::text, category, description, cost_per_unit,
                  supplier_id,
                  (SELECT name FROM suppliers WHERE id = supplier_id) AS supplier_name,
                  is_active, created_at, updated_at
        "#,
    )
    .bind(*org_id)
    .bind(body.name.trim())
    .bind(&body.unit)
    .bind(&body.category)
    .bind(&body.description)
    .bind(body.cost_per_unit)
    .bind(body.supplier_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(ref db) = e
            && db.code().as_deref() == Some("23505") {
                return AppError::Conflict("An ingredient with this name already exists in the catalog".into());
            }
        AppError::Db(e)
    })?;

    // Seed the first cost history row — only when a cost actually exists.
    if let Some(cost) = body.cost_per_unit {
        sqlx::query(
            "INSERT INTO ingredient_cost_history \
                 (org_ingredient_id, cost_per_unit, effective_from, changed_by, note) \
             VALUES ($1, $2, now(), $3, 'Initial cost')"
        )
        .bind(row.id)
        .bind(cost)
        .bind(claims.user_id())
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(HttpResponse::Created().json(row))
}

// ── PATCH /inventory/orgs/:org_id/catalog/:id ────────────────

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
    req:    HttpRequest,
    pool:   web::Data<PgPool>,
    path:   web::Path<(Uuid, Uuid)>,
    body:   web::Json<UpdateCatalogItemRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "update").await?;
    let (org_id, id) = path.into_inner();
    require_org_access(&claims, org_id)?;

    if let Some(ref u) = body.unit { validate_unit(u)?; }
    if let Some(sup) = body.supplier_id {
        ensure_supplier_in_org(pool.get_ref(), sup, org_id).await?;
    }

    let mut tx = pool.get_ref().begin().await?;

    // Lock the row and read its current base unit.
    let current_unit: String = sqlx::query_scalar(
        "SELECT unit::text FROM org_ingredients \
         WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL FOR UPDATE"
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
        && !new_unit.eq_ignore_ascii_case(&current_unit) {
        if body.cost_per_unit.is_some() {
            return Err(AppError::BadRequest(
                "Change the unit and the cost in separate requests — the cost is \
                 converted automatically when the unit changes.".into(),
            ));
        }
        // F = how many OLD units fit in one NEW unit (g per kg = 1000).
        // Cross-family (g↔ml, *↔pcs) returns Err ⟹ rejected here.
        let f = crate::units::convert(1.0, new_unit, &current_unit).map_err(|_| {
            AppError::BadRequest(
                "A unit can only change within the same measure: g ↔ kg or ml ↔ l.".into(),
            )
        })?;

        // Quantities are stored in the base unit → rebase old→new is ÷ F.
        for q in [
            "UPDATE menu_item_recipes        SET quantity_used = round((quantity_used / $2)::numeric, 3), ingredient_unit = $3 WHERE org_ingredient_id = $1",
            "UPDATE addon_item_ingredients   SET quantity_used = round((quantity_used / $2)::numeric, 3), ingredient_unit = $3 WHERE org_ingredient_id = $1",
            "UPDATE menu_item_optional_fields SET quantity_used = round((quantity_used / $2)::numeric, 3), ingredient_unit = $3 WHERE org_ingredient_id = $1 AND quantity_used IS NOT NULL",
        ] {
            sqlx::query(q).bind(id).bind(f).bind(new_unit).execute(&mut *tx).await?;
        }
        // Branch stock + reorder levels are in the base unit too → ÷ F.
        sqlx::query(
            "UPDATE branch_inventory \
             SET current_stock     = round((current_stock / $2)::numeric, 3), \
                 reorder_threshold = round((reorder_threshold / $2)::numeric, 3) \
             WHERE org_ingredient_id = $1"
        )
        .bind(id).bind(f).execute(&mut *tx).await?;
        // Cost is piastres per OLD unit → per NEW unit is × F (and its history).
        sqlx::query("UPDATE org_ingredients SET cost_per_unit = round((cost_per_unit * $2)::numeric, 2) WHERE id = $1 AND cost_per_unit IS NOT NULL")
            .bind(id).bind(f).execute(&mut *tx).await?;
        sqlx::query("UPDATE ingredient_cost_history SET cost_per_unit = round((cost_per_unit * $2)::numeric, 2) WHERE org_ingredient_id = $1")
            .bind(id).bind(f).execute(&mut *tx).await?;
    }

    let row = sqlx::query_as::<_, OrgIngredient>(
        r#"
        UPDATE org_ingredients SET
            name          = COALESCE($2, name),
            unit          = COALESCE($3::inventory_unit, unit),
            category      = COALESCE($4, category),
            description   = COALESCE($5, description),
            cost_per_unit = COALESCE($6, cost_per_unit),
            supplier_id   = COALESCE($9, supplier_id),
            is_active     = COALESCE($7, is_active)
        WHERE id = $1 AND org_id = $8 AND deleted_at IS NULL
        RETURNING id, org_id, name, unit::text, category, description, cost_per_unit,
                  supplier_id,
                  (SELECT name FROM suppliers WHERE id = supplier_id) AS supplier_name,
                  is_active, created_at, updated_at
        "#,
    )
    .bind(id)
    .bind(&body.name)
    .bind(&body.unit)
    .bind(&body.category)
    .bind(&body.description)
    .bind(body.cost_per_unit)
    .bind(body.is_active)
    .bind(org_id)
    .bind(body.supplier_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound("Ingredient not found".into()))?;

    // Maintain cost history whenever cost_per_unit actually changed.
    if let Some(new_cost) = body.cost_per_unit {
        let current_history_cost: Option<Decimal> = sqlx::query_scalar(
            "SELECT cost_per_unit FROM ingredient_cost_history \
             WHERE org_ingredient_id = $1 AND effective_until IS NULL"
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;

        if current_history_cost != Some(new_cost) {
            // Close the currently-active row.
            sqlx::query(
                "UPDATE ingredient_cost_history \
                 SET effective_until = now() \
                 WHERE org_ingredient_id = $1 AND effective_until IS NULL"
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;

            // Open a new row.
            sqlx::query(
                "INSERT INTO ingredient_cost_history \
                     (org_ingredient_id, cost_per_unit, effective_from, changed_by) \
                 VALUES ($1, $2, now(), $3)"
            )
            .bind(id)
            .bind(new_cost)
            .bind(claims.user_id())
            .execute(&mut *tx)
            .await?;
        }
    }

    tx.commit().await?;
    Ok(HttpResponse::Ok().json(row))
}

// ── DELETE /inventory/orgs/:org_id/catalog/:id ───────────────

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
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "delete").await?;
    let (org_id, id) = path.into_inner();
    require_org_access(&claims, org_id)?;

    // Check if referenced by any active configuration. menu_item_optional_fields
    // also carries org_ingredient_id and drives sale-time deductions, so it must
    // be guarded too — otherwise an ingredient can be soft-deleted while an
    // optional field keeps pointing at it (orphaned deductions, broken costing).
    let referenced: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM menu_item_recipes        WHERE org_ingredient_id = $1
            UNION ALL
            SELECT 1 FROM addon_item_ingredients   WHERE org_ingredient_id = $1
            UNION ALL
            SELECT 1 FROM menu_item_optional_fields WHERE org_ingredient_id = $1
            UNION ALL
            SELECT 1 FROM branch_inventory         WHERE org_ingredient_id = $1
        )
        "#,
    )
    .bind(id)
    .fetch_one(pool.get_ref())
    .await?;

    if referenced {
        return Err(AppError::Conflict(
            "Ingredient is referenced by recipes, optional fields, or branch stock. Remove those references first.".into(),
        ));
    }

    sqlx::query("UPDATE org_ingredients SET deleted_at = NOW() WHERE id = $1 AND org_id = $2")
        .bind(id)
        .bind(org_id)
        .execute(pool.get_ref())
        .await?;

    Ok(HttpResponse::NoContent().finish())
}

// ── GET /inventory/orgs/:org_id/settings ─────────────────────

#[utoipa::path(
    get,
    path = "/inventory/orgs/{org_id}/settings",
    tag = "inventory",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    responses((status = 200, description = "Org inventory settings", body = OrgInventorySettings), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_inventory_settings(
    req:    HttpRequest,
    pool:   web::Data<PgPool>,
    org_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_org_access(&claims, *org_id)?;

    let row = sqlx::query_as::<_, OrgInventorySettings>(
        "SELECT stocktake_variance_threshold_pct::float8 AS stocktake_variance_threshold_pct \
         FROM organizations WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(*org_id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Organization not found".into()))?;

    Ok(HttpResponse::Ok().json(row))
}

// ── PUT /inventory/orgs/:org_id/settings ─────────────────────

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
    req:    HttpRequest,
    pool:   web::Data<PgPool>,
    org_id: web::Path<Uuid>,
    body:   web::Json<UpdateInventorySettingsRequest>,
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
         RETURNING stocktake_variance_threshold_pct::float8 AS stocktake_variance_threshold_pct"
    )
    .bind(*org_id)
    .bind(pct)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Organization not found".into()))?;

    Ok(HttpResponse::Ok().json(row))
}

// ── GET /inventory/branches/:branch_id/stock ─────────────────

#[utoipa::path(
    get,
    path = "/inventory/branches/{branch_id}/stock",
    tag = "inventory",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    responses((status = 200, description = "List branch stock", body = Vec<BranchInventoryItem>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_branch_stock(
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    let rows = sqlx::query_as::<_, BranchInventoryItem>(
        r#"
        SELECT
            bi.id, bi.branch_id, bi.org_ingredient_id,
            oi.name AS ingredient_name,
            oi.unit::text AS unit,
            oi.description,
            oi.cost_per_unit,
            bi.current_stock,
            bi.reorder_threshold,
            (bi.reorder_threshold > 0 AND bi.current_stock <= bi.reorder_threshold) AS below_reorder,
            (SELECT max(s.finalized_at) FROM stocktakes s
               JOIN stocktake_items si ON si.stocktake_id = s.id
               WHERE s.branch_id = bi.branch_id AND si.org_ingredient_id = bi.org_ingredient_id
                 AND s.status = 'finalized' AND si.counted_qty IS NOT NULL) AS last_counted_at,
            bi.created_at, bi.updated_at
        FROM branch_inventory bi
        JOIN org_ingredients oi ON oi.id = bi.org_ingredient_id
        WHERE bi.branch_id = $1
        ORDER BY oi.name
        "#,
    )
    .bind(*branch_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── POST /inventory/branches/:branch_id/stock ────────────────

#[utoipa::path(
    post,
    path = "/inventory/branches/{branch_id}/stock",
    tag = "inventory",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    request_body = AddToStockRequest,
    responses((status = 201, description = "Added to branch stock", body = BranchInventoryItem), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn add_to_branch_stock(
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    body:      web::Json<AddToStockRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "create").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    // Verify org_ingredient belongs to this branch's org
    let branch_org: Option<Uuid> = sqlx::query_scalar(
        "SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(*branch_id)
    .fetch_optional(pool.get_ref())
    .await?
    .flatten()
    .ok_or_else(|| AppError::NotFound("Branch not found".into()))
    .map(Some)?;

    let ing_org: Option<Uuid> = sqlx::query_scalar(
        "SELECT org_id FROM org_ingredients WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(body.org_ingredient_id)
    .fetch_optional(pool.get_ref())
    .await?
    .flatten();

    if ing_org != branch_org {
        return Err(AppError::BadRequest(
            "Ingredient does not belong to this branch's organization".into(),
        ));
    }

    let row = sqlx::query_as::<_, BranchInventoryItem>(
        r#"
        INSERT INTO branch_inventory (branch_id, org_ingredient_id, current_stock, reorder_threshold)
        VALUES ($1, $2, $3, $4)
        RETURNING
            id, branch_id, org_ingredient_id,
            (SELECT name        FROM org_ingredients WHERE id = $2) AS ingredient_name,
            (SELECT unit::text  FROM org_ingredients WHERE id = $2) AS unit,
            (SELECT description FROM org_ingredients WHERE id = $2) AS description,
            (SELECT cost_per_unit FROM org_ingredients WHERE id = $2) AS cost_per_unit,
            current_stock, reorder_threshold,
            (reorder_threshold > 0 AND current_stock <= reorder_threshold) AS below_reorder,
            (SELECT max(s.finalized_at) FROM stocktakes s
               JOIN stocktake_items si ON si.stocktake_id = s.id
               WHERE s.branch_id = branch_inventory.branch_id
                 AND si.org_ingredient_id = branch_inventory.org_ingredient_id
                 AND s.status = 'finalized' AND si.counted_qty IS NOT NULL) AS last_counted_at,
            created_at, updated_at
        "#,
    )
    .bind(*branch_id)
    .bind(body.org_ingredient_id)
    .bind(body.current_stock.unwrap_or(0.0))
    .bind(body.reorder_threshold.unwrap_or(0.0))
    .fetch_one(pool.get_ref())
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(ref db) = e
            && db.code().as_deref() == Some("23505") {
                return AppError::Conflict("This ingredient is already tracked for this branch".into());
            }
        AppError::Db(e)
    })?;

    Ok(HttpResponse::Created().json(row))
}

// ── PATCH /inventory/branches/:branch_id/stock/:id ───────────

#[utoipa::path(
    patch,
    path = "/inventory/branches/{branch_id}/stock/{id}",
    tag = "inventory",
    params(
        ("branch_id" = Uuid, Path, description = "Branch ID"),
        ("id" = Uuid, Path, description = "Stock ID")
    ),
    request_body = UpdateStockRequest,
    responses((status = 200, description = "Branch stock updated", body = BranchInventoryItem), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_branch_stock(
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    path:      web::Path<(Uuid, Uuid)>,
    body:      web::Json<UpdateStockRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "update").await?;
    let (branch_id, id) = path.into_inner();
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;

    let row = sqlx::query_as::<_, BranchInventoryItem>(
        r#"
        UPDATE branch_inventory SET
            reorder_threshold = COALESCE($3, reorder_threshold),
            current_stock     = COALESCE($4, current_stock)
        WHERE id = $1 AND branch_id = $2
        RETURNING
            id, branch_id, org_ingredient_id,
            (SELECT name          FROM org_ingredients WHERE id = org_ingredient_id) AS ingredient_name,
            (SELECT unit::text    FROM org_ingredients WHERE id = org_ingredient_id) AS unit,
            (SELECT description   FROM org_ingredients WHERE id = org_ingredient_id) AS description,
            (SELECT cost_per_unit FROM org_ingredients WHERE id = org_ingredient_id) AS cost_per_unit,
            current_stock, reorder_threshold,
            (reorder_threshold > 0 AND current_stock <= reorder_threshold) AS below_reorder,
            (SELECT max(s.finalized_at) FROM stocktakes s
               JOIN stocktake_items si ON si.stocktake_id = s.id
               WHERE s.branch_id = branch_inventory.branch_id
                 AND si.org_ingredient_id = branch_inventory.org_ingredient_id
                 AND s.status = 'finalized' AND si.counted_qty IS NOT NULL) AS last_counted_at,
            created_at, updated_at
        "#,
    )
    .bind(id)
    .bind(branch_id)
    .bind(body.reorder_threshold)
    .bind(body.current_stock)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Branch inventory item not found".into()))?;

    Ok(HttpResponse::Ok().json(row))
}

// ── DELETE /inventory/branches/:branch_id/stock/:id ──────────

#[utoipa::path(
    delete,
    path = "/inventory/branches/{branch_id}/stock/{id}",
    tag = "inventory",
    params(
        ("branch_id" = Uuid, Path, description = "Branch ID"),
        ("id" = Uuid, Path, description = "Stock ID")
    ),
    responses((status = 204, description = "Removed from branch stock"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn remove_from_branch_stock(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "delete").await?;
    let (branch_id, id) = path.into_inner();
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;

    sqlx::query("DELETE FROM branch_inventory WHERE id = $1 AND branch_id = $2")
        .bind(id)
        .bind(branch_id)
        .execute(pool.get_ref())
        .await
        .map_err(|e| {
            if let sqlx::Error::Database(ref db) = e
                && db.code().as_deref() == Some("23503") {
                    return AppError::Conflict(
                        "Cannot remove ingredient with existing adjustment or transfer history".into(),
                    );
                }
            AppError::Db(e)
        })?;

    Ok(HttpResponse::NoContent().finish())
}

// ── POST /inventory/branches/:branch_id/adjustments ──────────

#[utoipa::path(
    post,
    path = "/inventory/branches/{branch_id}/adjustments",
    tag = "inventory",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    request_body = CreateAdjustmentRequest,
    responses((status = 201, description = "Adjustment created", body = BranchInventoryAdjustment), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_adjustment(
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    body:      web::Json<CreateAdjustmentRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory_adjustments", "create").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    match body.adjustment_type.as_str() {
        "add" | "remove" => {}
        _ => return Err(AppError::BadRequest("adjustment_type must be 'add' or 'remove'".into())),
    }
    if body.quantity <= 0.0 {
        return Err(AppError::BadRequest("quantity must be greater than 0".into()));
    }
    if body.note.trim().is_empty() {
        return Err(AppError::BadRequest("note is required for adjustments".into()));
    }

    let mut tx = pool.get_ref().begin().await?;

    // Lock the row inside the tx. This both verifies the item belongs to this
    // branch AND closes a TOCTOU race: without the lock two concurrent
    // "remove" requests could each read the same stock, both pass the check,
    // and drive current_stock negative. (Mirrors create_transfer's FOR UPDATE.)
    let locked: Option<(sqlx::types::BigDecimal, Uuid, Option<f64>)> = sqlx::query_as(
        "SELECT bi.current_stock, bi.org_ingredient_id, oi.cost_per_unit::float8 \
         FROM branch_inventory bi \
         JOIN org_ingredients oi ON oi.id = bi.org_ingredient_id \
         WHERE bi.id = $1 AND bi.branch_id = $2 FOR UPDATE OF bi"
    )
    .bind(body.branch_inventory_id)
    .bind(*branch_id)
    .fetch_optional(&mut *tx)
    .await?;

    let (current, org_ingredient_id, unit_cost_f) = locked
        .ok_or_else(|| AppError::BadRequest("Inventory item does not belong to this branch".into()))?;

    let qty = sqlx::types::BigDecimal::try_from(body.quantity)
        .map_err(|_| AppError::BadRequest("Invalid quantity".into()))?;

    // For remove: check sufficient stock (under the lock taken above).
    if body.adjustment_type == "remove" && current < qty {
        return Err(AppError::BadRequest(format!(
            "Insufficient stock. Current: {}, Requested: {}", current, qty
        )));
    }

    let delta: f64 = match body.adjustment_type.as_str() {
        "add"    =>  body.quantity,
        "remove" => -body.quantity,
        _        => unreachable!(),
    };

    let balance: f64 = sqlx::query_scalar(
        "UPDATE branch_inventory SET current_stock = current_stock + $1 \
         WHERE id = $2 RETURNING current_stock::float8"
    )
    .bind(delta)
    .bind(body.branch_inventory_id)
    .fetch_one(&mut *tx)
    .await?;

    let adj = sqlx::query_as::<_, BranchInventoryAdjustment>(
        r#"
        INSERT INTO branch_inventory_adjustments
            (branch_id, branch_inventory_id, type, quantity, note, adjusted_by)
        VALUES ($1, $2, $3::inventory_adjustment_type, $4, $5, $6)
        RETURNING
            id, branch_id, branch_inventory_id,
            (SELECT oi.name FROM branch_inventory bi JOIN org_ingredients oi ON oi.id = bi.org_ingredient_id WHERE bi.id = $2) AS ingredient_name,
            (SELECT oi.unit::text FROM branch_inventory bi JOIN org_ingredients oi ON oi.id = bi.org_ingredient_id WHERE bi.id = $2) AS unit,
            type::text AS adjustment_type,
            quantity, note, transfer_id, adjusted_by,
            (SELECT name FROM users WHERE id = $6) AS adjusted_by_name,
            created_at
        "#,
    )
    .bind(*branch_id)
    .bind(body.branch_inventory_id)
    .bind(&body.adjustment_type)
    .bind(body.quantity)
    .bind(body.note.trim())
    .bind(claims.user_id())
    .fetch_one(&mut *tx)
    .await?;

    record_movement(&mut *tx, MovementParams {
        branch_id:           *branch_id,
        org_ingredient_id,
        branch_inventory_id: Some(body.branch_inventory_id),
        movement_type:       if body.adjustment_type == "add" { "adjustment_add" } else { "adjustment_remove" },
        quantity:            delta,
        balance_after:       Some(balance),
        unit_cost:           unit_cost_f.map(|c| c.round() as i64),
        reason:              None,
        below_zero:          balance < 0.0,
        source_type:         Some("adjustment"),
        source_id:           Some(adj.id),
        note:                Some(body.note.trim()),
        created_by:          Some(claims.user_id()),
    })
    .await?;

    tx.commit().await?;

    Ok(HttpResponse::Created().json(adj))
}

// ── GET /inventory/branches/:branch_id/adjustments ───────────

#[utoipa::path(
    get,
    path = "/inventory/branches/{branch_id}/adjustments",
    tag = "inventory",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    responses((status = 200, description = "List adjustments", body = Vec<BranchInventoryAdjustment>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_adjustments(
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory_adjustments", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    let rows = sqlx::query_as::<_, BranchInventoryAdjustment>(
        r#"
        SELECT
            a.id, a.branch_id, a.branch_inventory_id,
            oi.name     AS ingredient_name,
            oi.unit::text AS unit,
            a.type::text AS adjustment_type,
            a.quantity, a.note, a.transfer_id, a.adjusted_by,
            u.name      AS adjusted_by_name,
            a.created_at
        FROM branch_inventory_adjustments a
        JOIN branch_inventory bi ON bi.id = a.branch_inventory_id
        JOIN org_ingredients oi  ON oi.id = bi.org_ingredient_id
        JOIN users u             ON u.id  = a.adjusted_by
        WHERE a.branch_id = $1
        ORDER BY a.created_at DESC
        "#,
    )
    .bind(*branch_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── GET /inventory/branches/:branch_id/movements ─────────────

#[utoipa::path(
    get,
    path = "/inventory/branches/{branch_id}/movements",
    tag = "inventory",
    params(("branch_id" = Uuid, Path, description = "Branch ID"), ListMovementsQuery),
    responses((status = 200, description = "List stock movements", body = Vec<BranchInventoryMovement>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_movements(
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query:     web::Query<ListMovementsQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    let per_page = query.per_page.unwrap_or(100).clamp(1, 500);
    let page     = query.page.unwrap_or(1).max(1);
    let offset   = (page - 1) * per_page;

    let rows = sqlx::query_as::<_, BranchInventoryMovement>(
        r#"
        SELECT
            m.id, m.branch_id, m.org_ingredient_id,
            oi.name       AS ingredient_name,
            oi.unit::text AS unit,
            m.branch_inventory_id,
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

// ── POST /inventory/branches/:branch_id/waste ────────────────

#[utoipa::path(
    post,
    path = "/inventory/branches/{branch_id}/waste",
    tag = "inventory",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    request_body = CreateWasteRequest,
    responses((status = 201, description = "Waste recorded", body = BranchInventoryMovement), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_waste(
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    body:      web::Json<CreateWasteRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory_waste", "create").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    validate_waste_reason(&body.reason)?;
    if body.quantity <= 0.0 {
        return Err(AppError::BadRequest("quantity must be greater than 0".into()));
    }

    let mut tx = pool.get_ref().begin().await?;

    // Lock the stock row (also validates the ingredient is tracked here).
    let locked: Option<(Uuid, sqlx::types::BigDecimal, Option<f64>)> = sqlx::query_as(
        "SELECT bi.id, bi.current_stock, oi.cost_per_unit::float8 \
         FROM branch_inventory bi \
         JOIN org_ingredients oi ON oi.id = bi.org_ingredient_id \
         WHERE bi.branch_id = $1 AND bi.org_ingredient_id = $2 FOR UPDATE OF bi"
    )
    .bind(*branch_id)
    .bind(body.org_ingredient_id)
    .fetch_optional(&mut *tx)
    .await?;

    let (bi_id, current, unit_cost_f) = locked
        .ok_or_else(|| AppError::BadRequest("Ingredient is not tracked at this branch".into()))?;

    let qty = sqlx::types::BigDecimal::try_from(body.quantity)
        .map_err(|_| AppError::BadRequest("Invalid quantity".into()))?;
    if current < qty {
        return Err(AppError::BadRequest(format!(
            "Cannot waste more than is in stock. Current: {}, Requested: {}", current, qty
        )));
    }

    let balance: f64 = sqlx::query_scalar(
        "UPDATE branch_inventory SET current_stock = current_stock - $1 \
         WHERE id = $2 RETURNING current_stock::float8"
    )
    .bind(body.quantity)
    .bind(bi_id)
    .fetch_one(&mut *tx)
    .await?;

    let note = body.note.as_deref().filter(|s| !s.trim().is_empty());
    let movement_id = record_movement(&mut *tx, MovementParams {
        branch_id:           *branch_id,
        org_ingredient_id:   body.org_ingredient_id,
        branch_inventory_id: Some(bi_id),
        movement_type:       "waste",
        quantity:            -body.quantity,
        balance_after:       Some(balance),
        unit_cost:           unit_cost_f.map(|c| c.round() as i64),
        reason:              Some(body.reason.as_str()),
        below_zero:          false,
        source_type:         Some("waste"),
        source_id:           None,
        note,
        created_by:          Some(claims.user_id()),
    })
    .await?;

    let movement = fetch_movement(&mut *tx, movement_id).await?;
    tx.commit().await?;
    Ok(HttpResponse::Created().json(movement))
}

// ── GET /inventory/branches/:branch_id/waste ─────────────────

#[utoipa::path(
    get,
    path = "/inventory/branches/{branch_id}/waste",
    tag = "inventory",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    responses((status = 200, description = "List waste movements", body = Vec<BranchInventoryMovement>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_waste(
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory_waste", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    let rows = sqlx::query_as::<_, BranchInventoryMovement>(
        r#"
        SELECT
            m.id, m.branch_id, m.org_ingredient_id,
            oi.name AS ingredient_name, oi.unit::text AS unit,
            m.branch_inventory_id, m.type::text AS movement_type,
            m.quantity, m.balance_after, m.unit_cost, m.reason, m.below_zero,
            m.source_type, m.source_id, m.note, m.created_by,
            u.name AS created_by_name, m.created_at
        FROM inventory_movements m
        JOIN org_ingredients oi ON oi.id = m.org_ingredient_id
        LEFT JOIN users u       ON u.id  = m.created_by
        WHERE m.branch_id = $1 AND m.type = 'waste'
        ORDER BY m.created_at DESC, m.id DESC
        "#,
    )
    .bind(*branch_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── POST /inventory/transfers ─────────────────────────────────

#[utoipa::path(
    post,
    path = "/inventory/transfers",
    tag = "inventory",
    request_body = CreateTransferRequest,
    responses((status = 201, description = "Transfer created", body = BranchInventoryTransfer), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_transfer(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<CreateTransferRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory_transfers", "create").await?;
    require_branch_access(pool.get_ref(), &claims, body.source_branch_id).await?;

    if body.quantity <= 0.0 {
        return Err(AppError::BadRequest("quantity must be greater than 0".into()));
    }
    if body.source_branch_id == body.destination_branch_id {
        return Err(AppError::BadRequest("Source and destination branches must be different".into()));
    }

    // Both branches must be in same org
    let src_org: Uuid = sqlx::query_scalar(
        "SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(body.source_branch_id)
    .fetch_optional(pool.get_ref())
    .await?
    .flatten()
    .ok_or_else(|| AppError::NotFound("Source branch not found".into()))?;

    let dst_org: Uuid = sqlx::query_scalar(
        "SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(body.destination_branch_id)
    .fetch_optional(pool.get_ref())
    .await?
    .flatten()
    .ok_or_else(|| AppError::NotFound("Destination branch not found".into()))?;

    if src_org != dst_org {
        return Err(AppError::BadRequest("Both branches must belong to the same organization".into()));
    }

    // Verify ingredient belongs to this org
    let ing_org: Uuid = sqlx::query_scalar(
        "SELECT org_id FROM org_ingredients WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(body.org_ingredient_id)
    .fetch_optional(pool.get_ref())
    .await?
    .flatten()
    .ok_or_else(|| AppError::NotFound("Ingredient not found in org catalog".into()))?;

    if ing_org != src_org {
        return Err(AppError::BadRequest("Ingredient does not belong to this organization".into()));
    }

    let qty = sqlx::types::BigDecimal::try_from(body.quantity)
        .map_err(|_| AppError::BadRequest("Invalid quantity".into()))?;

    let mut tx = pool.get_ref().begin().await?;

    // Lock source row and validate stock atomically — prevents TOCTOU race
    // between a concurrent transfer that reads the same stock level.
    let src_stock: Option<sqlx::types::BigDecimal> = sqlx::query_scalar(
        "SELECT current_stock FROM branch_inventory \
         WHERE branch_id = $1 AND org_ingredient_id = $2 FOR UPDATE"
    )
    .bind(body.source_branch_id)
    .bind(body.org_ingredient_id)
    .fetch_optional(&mut *tx)
    .await?
    .flatten();

    let src_stock = src_stock.ok_or_else(|| AppError::BadRequest(
        "Source branch does not track this ingredient".into()
    ))?;

    if src_stock < qty {
        return Err(AppError::BadRequest(format!(
            "Insufficient stock on source branch. Current: {}, Requested: {}", src_stock, qty
        )));
    }

    // Deduct from source (stock already locked above)
    let (src_bi_id, src_balance): (Uuid, f64) = sqlx::query_as(
        "UPDATE branch_inventory SET current_stock = current_stock - $1
         WHERE branch_id = $2 AND org_ingredient_id = $3
         RETURNING id, current_stock::float8"
    )
    .bind(body.quantity)
    .bind(body.source_branch_id)
    .bind(body.org_ingredient_id)
    .fetch_one(&mut *tx)
    .await?;

    // Upsert destination — create if not tracked, add stock if exists
    let (dst_bi_id, dst_balance): (Uuid, f64) = sqlx::query_as(
        r#"
        INSERT INTO branch_inventory (branch_id, org_ingredient_id, current_stock, reorder_threshold)
        VALUES ($1, $2, $3, 0)
        ON CONFLICT (branch_id, org_ingredient_id)
        DO UPDATE SET current_stock = branch_inventory.current_stock + EXCLUDED.current_stock
        RETURNING id, current_stock::float8
        "#,
    )
    .bind(body.destination_branch_id)
    .bind(body.org_ingredient_id)
    .bind(body.quantity)
    .fetch_one(&mut *tx)
    .await?;

    // Look up branch names for audit notes
    let src_name: String = sqlx::query_scalar(
        "SELECT name FROM branches WHERE id = $1"
    )
    .bind(body.source_branch_id)
    .fetch_one(&mut *tx)
    .await?;

    let dst_name: String = sqlx::query_scalar(
        "SELECT name FROM branches WHERE id = $1"
    )
    .bind(body.destination_branch_id)
    .fetch_one(&mut *tx)
    .await?;

    // Record transfer
    let transfer = sqlx::query_as::<_, BranchInventoryTransfer>(
        r#"
        INSERT INTO branch_inventory_transfers
            (org_id, source_branch_id, destination_branch_id, org_ingredient_id, quantity, note, initiated_by)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING
            id, org_id,
            source_branch_id,
            (SELECT name FROM branches WHERE id = $2) AS source_branch_name,
            destination_branch_id,
            (SELECT name FROM branches WHERE id = $3) AS destination_branch_name,
            org_ingredient_id,
            (SELECT name     FROM org_ingredients WHERE id = $4) AS ingredient_name,
            (SELECT unit::text FROM org_ingredients WHERE id = $4) AS unit,
            quantity, note, initiated_by,
            (SELECT name FROM users WHERE id = $7) AS initiated_by_name,
            initiated_at
        "#,
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

    // Log adjustments on both sides
    sqlx::query(
        r#"INSERT INTO branch_inventory_adjustments
            (branch_id, branch_inventory_id, type, quantity, note, transfer_id, adjusted_by)
           VALUES ($1, $2, 'transfer_out'::inventory_adjustment_type, $3, $4, $5, $6)"#,
    )
    .bind(body.source_branch_id)
    .bind(src_bi_id)
    .bind(body.quantity)
    .bind(format!("Transfer to {} — {} units", dst_name, body.quantity))
    .bind(transfer.id)
    .bind(claims.user_id())
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        r#"INSERT INTO branch_inventory_adjustments
            (branch_id, branch_inventory_id, type, quantity, note, transfer_id, adjusted_by)
           VALUES ($1, $2, 'transfer_in'::inventory_adjustment_type, $3, $4, $5, $6)"#,
    )
    .bind(body.destination_branch_id)
    .bind(dst_bi_id)
    .bind(body.quantity)
    .bind(format!("Transfer from {} — {} units", src_name, body.quantity))
    .bind(transfer.id)
    .bind(claims.user_id())
    .execute(&mut *tx)
    .await?;

    // Ledger movements on both sides (cost is the same org-level ingredient).
    let unit_cost_f: Option<f64> = sqlx::query_scalar(
        "SELECT cost_per_unit::float8 FROM org_ingredients WHERE id = $1"
    )
    .bind(body.org_ingredient_id)
    .fetch_one(&mut *tx)
    .await?;
    let unit_cost = unit_cost_f.map(|c| c.round() as i64);

    record_movement(&mut *tx, MovementParams {
        branch_id:           body.source_branch_id,
        org_ingredient_id:   body.org_ingredient_id,
        branch_inventory_id: Some(src_bi_id),
        movement_type:       "transfer_out",
        quantity:            -body.quantity,
        balance_after:       Some(src_balance),
        unit_cost,
        reason:              None,
        below_zero:          src_balance < 0.0,
        source_type:         Some("transfer"),
        source_id:           Some(transfer.id),
        note:                Some("Transfer out"),
        created_by:          Some(claims.user_id()),
    })
    .await?;

    record_movement(&mut *tx, MovementParams {
        branch_id:           body.destination_branch_id,
        org_ingredient_id:   body.org_ingredient_id,
        branch_inventory_id: Some(dst_bi_id),
        movement_type:       "transfer_in",
        quantity:            body.quantity,
        balance_after:       Some(dst_balance),
        unit_cost,
        reason:              None,
        below_zero:          false,
        source_type:         Some("transfer"),
        source_id:           Some(transfer.id),
        note:                Some("Transfer in"),
        created_by:          Some(claims.user_id()),
    })
    .await?;

    tx.commit().await?;

    Ok(HttpResponse::Created().json(transfer))
}

// ── GET /inventory/branches/:branch_id/transfers ─────────────

#[utoipa::path(
    get,
    path = "/inventory/branches/{branch_id}/transfers",
    tag = "inventory",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    params(ListTransfersQuery),
    responses((status = 200, description = "List transfers", body = Vec<BranchInventoryTransfer>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_transfers(
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query:     web::Query<ListTransfersQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory_transfers", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    let condition = match query.direction.as_deref() {
        Some("incoming") => "t.destination_branch_id = $1",
        Some("outgoing") => "t.source_branch_id = $1",
        _                => "(t.source_branch_id = $1 OR t.destination_branch_id = $1)",
    };

    let sql = format!(
        r#"
        SELECT
            t.id, t.org_id,
            t.source_branch_id,
            sb.name AS source_branch_name,
            t.destination_branch_id,
            db.name AS destination_branch_name,
            t.org_ingredient_id,
            oi.name      AS ingredient_name,
            oi.unit::text AS unit,
            t.quantity, t.note, t.initiated_by,
            u.name AS initiated_by_name,
            t.initiated_at
        FROM branch_inventory_transfers t
        JOIN branches sb        ON sb.id  = t.source_branch_id
        JOIN branches db        ON db.id  = t.destination_branch_id
        JOIN org_ingredients oi ON oi.id  = t.org_ingredient_id
        JOIN users u            ON u.id   = t.initiated_by
        WHERE {}
        ORDER BY t.initiated_at DESC
        "#,
        condition
    );

    let rows = sqlx::query_as::<_, BranchInventoryTransfer>(&sql)
        .bind(*branch_id)
        .fetch_all(pool.get_ref())
        .await?;

    Ok(HttpResponse::Ok().json(rows))
}


// ── PATCH /inventory/transfers/:id ───────────────────────────

#[utoipa::path(
    patch,
    path = "/inventory/transfers/{id}",
    tag = "inventory",
    params(("id" = Uuid, Path, description = "Transfer ID")),
    request_body = UpdateTransferRequest,
    responses((status = 200, description = "Transfer updated", body = BranchInventoryTransfer), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_transfer(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
    body: web::Json<UpdateTransferRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory_transfers", "update").await?;

    // Load transfer so we can check org access
    let transfer = sqlx::query_as::<_, BranchInventoryTransfer>(
        r#"
        SELECT
            t.id, t.org_id,
            t.source_branch_id,
            sb.name AS source_branch_name,
            t.destination_branch_id,
            db.name AS destination_branch_name,
            t.org_ingredient_id,
            oi.name       AS ingredient_name,
            oi.unit::text AS unit,
            t.quantity, t.note, t.initiated_by,
            u.name AS initiated_by_name,
            t.initiated_at
        FROM branch_inventory_transfers t
        JOIN branches sb        ON sb.id = t.source_branch_id
        JOIN branches db        ON db.id = t.destination_branch_id
        JOIN org_ingredients oi ON oi.id = t.org_ingredient_id
        JOIN users u            ON u.id  = t.initiated_by
        WHERE t.id = $1
        "#,
    )
    .bind(*id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Transfer not found".into()))?;

    require_org_access(&claims, transfer.org_id)?;

    let updated = sqlx::query_as::<_, BranchInventoryTransfer>(
        r#"
        UPDATE branch_inventory_transfers SET note = $2
        WHERE id = $1
        RETURNING
            id, org_id,
            source_branch_id,
            (SELECT name FROM branches      WHERE id = source_branch_id)      AS source_branch_name,
            destination_branch_id,
            (SELECT name FROM branches      WHERE id = destination_branch_id) AS destination_branch_name,
            org_ingredient_id,
            (SELECT name      FROM org_ingredients WHERE id = org_ingredient_id) AS ingredient_name,
            (SELECT unit::text FROM org_ingredients WHERE id = org_ingredient_id) AS unit,
            quantity, note, initiated_by,
            (SELECT name FROM users WHERE id = initiated_by) AS initiated_by_name,
            initiated_at
        "#,
    )
    .bind(*id)
    .bind(&body.note)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(updated))
}

// ── DELETE /inventory/transfers/:id ──────────────────────────

#[utoipa::path(
    delete,
    path = "/inventory/transfers/{id}",
    tag = "inventory",
    params(("id" = Uuid, Path, description = "Transfer ID")),
    responses((status = 204, description = "Transfer deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_transfer(
    req:  HttpRequest,
    pool: web::Data<PgPool>,
    id:   web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory_transfers", "delete").await?;

    // Load the transfer
    let t: Option<(Uuid, Uuid, Uuid, Uuid, sqlx::types::BigDecimal, String, String)> =
        sqlx::query_as(
            r#"
            SELECT
                t.org_id,
                t.source_branch_id,
                t.destination_branch_id,
                t.org_ingredient_id,
                t.quantity,
                sb.name AS source_branch_name,
                db.name AS destination_branch_name
            FROM branch_inventory_transfers t
            JOIN branches sb ON sb.id = t.source_branch_id
            JOIN branches db ON db.id = t.destination_branch_id
            WHERE t.id = $1
            "#,
        )
        .bind(*id)
        .fetch_optional(pool.get_ref())
        .await?;

    let (org_id, src_id, dst_id, ing_id, qty, src_name, dst_name) =
        t.ok_or_else(|| AppError::NotFound("Transfer not found".into()))?;

    require_org_access(&claims, org_id)?;

    let mut tx = pool.get_ref().begin().await?;

    // Check destination still has enough stock to reverse (lock the row first)
    let dst_stock: Option<sqlx::types::BigDecimal> = sqlx::query_scalar(
        "SELECT current_stock FROM branch_inventory \
         WHERE branch_id = $1 AND org_ingredient_id = $2 FOR UPDATE"
    )
    .bind(dst_id)
    .bind(ing_id)
    .fetch_optional(&mut *tx)
    .await?
    .flatten();

    if let Some(ref stock) = dst_stock {
        if stock < &qty {
            return Err(AppError::Conflict(format!(
                "Cannot reverse transfer: destination branch only has {} units remaining (transfer was {} units)",
                stock, qty
            )));
        }
    }

    let qty_f: f64 = qty.to_string().parse().unwrap_or(0.0);
    let unit_cost_f: Option<f64> = sqlx::query_scalar(
        "SELECT cost_per_unit::float8 FROM org_ingredients WHERE id = $1"
    )
    .bind(ing_id)
    .fetch_one(&mut *tx)
    .await?;
    let unit_cost = unit_cost_f.map(|c| c.round() as i64);

    // Reverse: add back to source
    let src_rev: Option<(Uuid, f64)> = sqlx::query_as(
        "UPDATE branch_inventory SET current_stock = current_stock + $1
         WHERE branch_id = $2 AND org_ingredient_id = $3
         RETURNING id, current_stock::float8"
    )
    .bind(&qty)
    .bind(src_id)
    .bind(ing_id)
    .fetch_optional(&mut *tx)
    .await?;

    // Reverse: deduct from destination (stock already validated and locked above)
    let dst_rev: Option<(Uuid, f64)> = sqlx::query_as(
        "UPDATE branch_inventory SET current_stock = current_stock - $1
         WHERE branch_id = $2 AND org_ingredient_id = $3
         RETURNING id, current_stock::float8"
    )
    .bind(&qty)
    .bind(dst_id)
    .bind(ing_id)
    .fetch_optional(&mut *tx)
    .await?;

    // Log compensating adjustments on both sides (audit trail)
    sqlx::query(
        r#"INSERT INTO branch_inventory_adjustments
            (branch_id, branch_inventory_id, type, quantity, note, adjusted_by)
           SELECT $1, bi.id, 'add'::inventory_adjustment_type, $3,
                  $4, $5
           FROM branch_inventory bi
           WHERE bi.branch_id = $1 AND bi.org_ingredient_id = $2"#,
    )
    .bind(src_id)
    .bind(ing_id)
    .bind(&qty)
    .bind(format!("Transfer reversal — returned from {}", dst_name))
    .bind(claims.user_id())
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        r#"INSERT INTO branch_inventory_adjustments
            (branch_id, branch_inventory_id, type, quantity, note, adjusted_by)
           SELECT $1, bi.id, 'remove'::inventory_adjustment_type, $3,
                  $4, $5
           FROM branch_inventory bi
           WHERE bi.branch_id = $1 AND bi.org_ingredient_id = $2"#,
    )
    .bind(dst_id)
    .bind(ing_id)
    .bind(&qty)
    .bind(format!("Transfer reversal — returned to {}", src_name))
    .bind(claims.user_id())
    .execute(&mut *tx)
    .await?;

    // Ledger movements for the reversal.
    if let Some((bi_id, balance)) = src_rev {
        record_movement(&mut *tx, MovementParams {
            branch_id:           src_id,
            org_ingredient_id:   ing_id,
            branch_inventory_id: Some(bi_id),
            movement_type:       "adjustment_add",
            quantity:            qty_f,
            balance_after:       Some(balance),
            unit_cost,
            reason:              None,
            below_zero:          false,
            source_type:         Some("transfer"),
            source_id:           Some(*id),
            note:                Some("Transfer reversal"),
            created_by:          Some(claims.user_id()),
        })
        .await?;
    }
    if let Some((bi_id, balance)) = dst_rev {
        record_movement(&mut *tx, MovementParams {
            branch_id:           dst_id,
            org_ingredient_id:   ing_id,
            branch_inventory_id: Some(bi_id),
            movement_type:       "adjustment_remove",
            quantity:            -qty_f,
            balance_after:       Some(balance),
            unit_cost,
            reason:              None,
            below_zero:          balance < 0.0,
            source_type:         Some("transfer"),
            source_id:           Some(*id),
            note:                Some("Transfer reversal"),
            created_by:          Some(claims.user_id()),
        })
        .await?;
    }

    // Delete the transfer record
    sqlx::query("DELETE FROM branch_inventory_transfers WHERE id = $1")
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
    if claims.role == UserRole::SuperAdmin { return Ok(()); }
    if claims.org_id() != Some(org_id) {
        return Err(AppError::Forbidden("Access denied to this org".into()));
    }
    Ok(())
}

async fn ensure_supplier_in_org<'e, E>(executor: E, supplier_id: Uuid, org_id: Uuid) -> Result<(), AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let found: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM suppliers WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL"
    )
    .bind(supplier_id)
    .bind(org_id)
    .fetch_optional(executor)
    .await?;
    if found.is_none() {
        return Err(AppError::BadRequest("Supplier does not belong to this organization".into()));
    }
    Ok(())
}

async fn require_branch_access(
    pool:      &PgPool,
    claims:    &Claims,
    branch_id: Uuid,
) -> Result<(), AppError> {
    if claims.role == UserRole::SuperAdmin { return Ok(()); }

    let branch_org: Option<Uuid> = sqlx::query_scalar(
        "SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(branch_id)
    .fetch_optional(pool)
    .await?
    .flatten();

    let branch_org = branch_org.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    if claims.org_id() != Some(branch_org) {
        return Err(AppError::Forbidden("Branch belongs to a different org".into()));
    }

    if claims.role == UserRole::OrgAdmin { return Ok(()); }

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
        Err(AppError::BadRequest("Unit must be one of: g, kg, ml, l, pcs".into()))
    }
}

fn validate_waste_reason(reason: &str) -> Result<(), AppError> {
    match reason {
        "expired" | "spoiled" | "damaged" | "overproduction" | "theft" | "other" => Ok(()),
        _ => Err(AppError::BadRequest(
            "reason must be one of: expired, spoiled, damaged, overproduction, theft, other".into(),
        )),
    }
}

async fn fetch_movement<'e, E>(executor: E, id: Uuid) -> Result<BranchInventoryMovement, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_as::<_, BranchInventoryMovement>(
        r#"
        SELECT
            m.id, m.branch_id, m.org_ingredient_id,
            oi.name AS ingredient_name, oi.unit::text AS unit,
            m.branch_inventory_id, m.type::text AS movement_type,
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