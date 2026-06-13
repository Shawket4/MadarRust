use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    errors::{AppError, AppErrorResponse},
    permissions::checker::check_permission,
};
use utoipa::{IntoParams, ToSchema};

// ── Models ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct DrinkRecipe {
    pub id:               Uuid,
    pub menu_item_id:     Uuid,
    pub size_label:       String,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name:  String,
    pub unit:             String,
    #[schema(value_type = f64)]
    pub quantity_used:    sqlx::types::BigDecimal,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, sqlx::FromRow, ToSchema)]
pub struct AddonIngredient {
    pub id:                Uuid,
    pub addon_item_id:     Uuid,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name:   String,
    pub unit:              String,
    #[schema(value_type = f64)]
    pub quantity_used:     sqlx::types::BigDecimal,
}

// ── Request types ─────────────────────────────────────────────

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpsertDrinkRecipeRequest {
    pub size_label:        String,
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name:   String,
    #[serde(alias = "unit")]
    pub ingredient_unit:   String,
    pub quantity_used:     f64,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpsertAddonIngredientRequest {
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name:   String,
    #[serde(alias = "unit")]
    pub ingredient_unit:   String,
    pub quantity_used:     f64,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DeleteRecipeQuery {
    pub ingredient_name: String,
}

// ── GET /recipes/drinks/:menu_item_id ─────────────────────────

#[utoipa::path(
    get,
    path = "/recipes/drinks/{menu_item_id}",
    tag = "recipes",
    params(("menu_item_id" = Uuid, Path, description = "Menu item ID")),
    responses((status = 200, description = "List drink recipes", body = Vec<DrinkRecipe>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_drink_recipes(
    req:          HttpRequest,
    pool:         web::Data<PgPool>,
    menu_item_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "recipes", "read").await?;
    require_menu_item_org(pool.get_ref(), &claims, *menu_item_id).await?;

    let rows = sqlx::query_as::<_, DrinkRecipe>(
        r#"
        SELECT id, menu_item_id, size_label::text,
               org_ingredient_id,
               ingredient_name,
               ingredient_unit AS unit,
               quantity_used
        FROM menu_item_recipes
        WHERE menu_item_id = $1
        ORDER BY size_label, ingredient_name
        "#,
    )
    .bind(*menu_item_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── POST /recipes/drinks/:menu_item_id ────────────────────────

#[utoipa::path(
    post,
    path = "/recipes/drinks/{menu_item_id}",
    tag = "recipes",
    params(("menu_item_id" = Uuid, Path, description = "Menu item ID")),
    request_body = UpsertDrinkRecipeRequest,
    responses((status = 200, description = "Drink recipe upserted", body = DrinkRecipe), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn upsert_drink_recipe(
    req:          HttpRequest,
    pool:         web::Data<PgPool>,
    menu_item_id: web::Path<Uuid>,
    body:         web::Json<UpsertDrinkRecipeRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "recipes", "create").await?;
    require_menu_item_org(pool.get_ref(), &claims, *menu_item_id).await?;

    if body.quantity_used <= 0.0 {
        return Err(AppError::BadRequest("quantity_used must be greater than 0".into()));
    }

    let (ingredient_unit, quantity_used) = normalize_recipe_unit(
        pool.get_ref(), body.org_ingredient_id, &body.ingredient_unit, body.quantity_used,
    ).await?;

    let row = sqlx::query_as::<_, DrinkRecipe>(
        r#"
        INSERT INTO menu_item_recipes
            (menu_item_id, size_label, org_ingredient_id, ingredient_name, ingredient_unit, quantity_used)
        VALUES ($1, $2::item_size, $3, $4, $5, $6)
        ON CONFLICT (menu_item_id, size_label, ingredient_name)
        DO UPDATE SET
            org_ingredient_id = EXCLUDED.org_ingredient_id,
            ingredient_unit   = EXCLUDED.ingredient_unit,
            quantity_used     = EXCLUDED.quantity_used
        RETURNING id, menu_item_id, size_label::text,
                  org_ingredient_id,
                  ingredient_name,
                  ingredient_unit AS unit,
                  quantity_used
        "#,
    )
    .bind(*menu_item_id)
    .bind(&body.size_label)
    .bind(body.org_ingredient_id)
    .bind(&body.ingredient_name)
    .bind(&ingredient_unit)
    .bind(quantity_used)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(row))
}

// ── DELETE /recipes/drinks/:menu_item_id/:size ────────────────

#[utoipa::path(
    delete,
    path = "/recipes/drinks/{menu_item_id}/{size}",
    tag = "recipes",
    params(
        ("menu_item_id" = Uuid, Path, description = "Menu item ID"),
        ("size" = String, Path, description = "Size label")
    ),
    params(DeleteRecipeQuery),
    responses((status = 204, description = "Drink recipe deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_drink_recipe(
    req:   HttpRequest,
    pool:  web::Data<PgPool>,
    path:  web::Path<(Uuid, String)>,
    query: web::Query<DeleteRecipeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "recipes", "delete").await?;

    let (menu_item_id, size_label) = path.into_inner();
    require_menu_item_org(pool.get_ref(), &claims, menu_item_id).await?;

    sqlx::query(
        r#"
        DELETE FROM menu_item_recipes
        WHERE menu_item_id    = $1
          AND size_label      = $2::item_size
          AND ingredient_name = $3
        "#,
    )
    .bind(menu_item_id)
    .bind(&size_label)
    .bind(&query.ingredient_name)
    .execute(pool.get_ref())
    .await?;

    Ok(HttpResponse::NoContent().finish())
}

// ── GET /recipes/addons/:addon_item_id ────────────────────────

#[utoipa::path(
    get,
    path = "/recipes/addons/{addon_item_id}",
    tag = "recipes",
    params(("addon_item_id" = Uuid, Path, description = "Addon item ID")),
    responses((status = 200, description = "List addon ingredients", body = Vec<AddonIngredient>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_addon_ingredients(
    req:           HttpRequest,
    pool:          web::Data<PgPool>,
    addon_item_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "recipes", "read").await?;
    require_addon_org(pool.get_ref(), &claims, *addon_item_id).await?;

    let rows = sqlx::query_as::<_, AddonIngredient>(
        r#"
        SELECT id, addon_item_id,
               org_ingredient_id,
               ingredient_name,
               ingredient_unit AS unit,
               quantity_used
        FROM addon_item_ingredients
        WHERE addon_item_id = $1
        ORDER BY ingredient_name
        "#,
    )
    .bind(*addon_item_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── POST /recipes/addons/:addon_item_id ───────────────────────

#[utoipa::path(
    post,
    path = "/recipes/addons/{addon_item_id}",
    tag = "recipes",
    params(("addon_item_id" = Uuid, Path, description = "Addon item ID")),
    request_body = UpsertAddonIngredientRequest,
    responses((status = 200, description = "Addon ingredient upserted", body = AddonIngredient), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn upsert_addon_ingredient(
    req:           HttpRequest,
    pool:          web::Data<PgPool>,
    addon_item_id: web::Path<Uuid>,
    body:          web::Json<UpsertAddonIngredientRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "recipes", "create").await?;
    require_addon_org(pool.get_ref(), &claims, *addon_item_id).await?;

    if body.quantity_used <= 0.0 {
        return Err(AppError::BadRequest("quantity_used must be greater than 0".into()));
    }

    let (ingredient_unit, quantity_used) = normalize_recipe_unit(
        pool.get_ref(), body.org_ingredient_id, &body.ingredient_unit, body.quantity_used,
    ).await?;

    let row = sqlx::query_as::<_, AddonIngredient>(
        r#"
        INSERT INTO addon_item_ingredients
            (addon_item_id, org_ingredient_id, ingredient_name, ingredient_unit, quantity_used)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (addon_item_id, ingredient_name)
        DO UPDATE SET
            org_ingredient_id = EXCLUDED.org_ingredient_id,
            ingredient_unit   = EXCLUDED.ingredient_unit,
            quantity_used     = EXCLUDED.quantity_used
        RETURNING id, addon_item_id,
                  org_ingredient_id,
                  ingredient_name,
                  ingredient_unit AS unit,
                  quantity_used
        "#,
    )
    .bind(*addon_item_id)
    .bind(body.org_ingredient_id)
    .bind(&body.ingredient_name)
    .bind(&ingredient_unit)
    .bind(quantity_used)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(row))
}

// ── DELETE /recipes/addons/:addon_item_id ─────────────────────

#[utoipa::path(
    delete,
    path = "/recipes/addons/{addon_item_id}",
    tag = "recipes",
    params(("addon_item_id" = Uuid, Path, description = "Addon item ID")),
    params(DeleteRecipeQuery),
    responses((status = 204, description = "Addon ingredient deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_addon_ingredient(
    req:   HttpRequest,
    pool:  web::Data<PgPool>,
    path:  web::Path<Uuid>,
    query: web::Query<DeleteRecipeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "recipes", "delete").await?;

    let addon_item_id = path.into_inner();
    require_addon_org(pool.get_ref(), &claims, addon_item_id).await?;

    sqlx::query(
        "DELETE FROM addon_item_ingredients WHERE addon_item_id = $1 AND ingredient_name = $2"
    )
    .bind(addon_item_id)
    .bind(&query.ingredient_name)
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

/// When a recipe line links a catalog ingredient, normalize the submitted
/// quantity to that ingredient's base stock unit (so every stored quantity is
/// in base units — the invariant the deduction + cost rollups rely on). For
/// unlinked (name-only) lines we keep the free-text unit as-is.
pub async fn normalize_recipe_unit(
    pool:              &PgPool,
    org_ingredient_id: Option<Uuid>,
    recipe_unit:       &str,
    qty:               f64,
) -> Result<(String, f64), AppError> {
    match org_ingredient_id {
        Some(id) => {
            let base_unit: String = sqlx::query_scalar(
                "SELECT unit::text FROM org_ingredients WHERE id = $1 AND deleted_at IS NULL"
            )
            .bind(id)
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| AppError::BadRequest("Linked ingredient not found in catalog".into()))?;
            let (u, q) = crate::units::normalize_to_base(qty, recipe_unit, &base_unit)?;
            // A positive input that rounds to 0 in the base unit (e.g. 0.4 g into a
            // kg-base ingredient → 0.000 kg) would silently store a no-op recipe
            // line: no deduction, no COGS. Reject it instead of losing the quantity (V22).
            if qty > 0.0 && q <= 0.0 {
                return Err(AppError::BadRequest(format!(
                    "quantity {qty} {recipe_unit} is too small for base unit {base_unit} (rounds to 0)"
                )));
            }
            Ok((u, q))
        }
        None => Ok((recipe_unit.to_string(), qty)),
    }
}

async fn require_menu_item_org(
    pool:         &PgPool,
    claims:       &Claims,
    menu_item_id: Uuid,
) -> Result<(), AppError> {
    use crate::models::UserRole;
    if claims.role == UserRole::SuperAdmin { return Ok(()); }

    let item_org: Option<Uuid> = sqlx::query_scalar(
        "SELECT org_id FROM menu_items WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(menu_item_id)
    .fetch_optional(pool)
    .await?
    .flatten();

    let item_org = item_org.ok_or_else(|| AppError::NotFound("Menu item not found".into()))?;
    if claims.org_id() != Some(item_org) {
        return Err(AppError::Forbidden("Menu item belongs to a different org".into()));
    }
    Ok(())
}

async fn require_addon_org(
    pool:          &PgPool,
    claims:        &Claims,
    addon_item_id: Uuid,
) -> Result<(), AppError> {
    use crate::models::UserRole;
    if claims.role == UserRole::SuperAdmin { return Ok(()); }

    let addon_org: Option<Uuid> = sqlx::query_scalar(
        "SELECT org_id FROM addon_items WHERE id = $1"
    )
    .bind(addon_item_id)
    .fetch_optional(pool)
    .await?
    .flatten();

    let addon_org = addon_org.ok_or_else(|| AppError::NotFound("Addon item not found".into()))?;
    if claims.org_id() != Some(addon_org) {
        return Err(AppError::Forbidden("Addon item belongs to a different org".into()));
    }
    Ok(())
}
