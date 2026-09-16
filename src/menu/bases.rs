//! Recipe bases (menu modeling B7): a named set of lines shared by many item sizes
//! ("Blended matcha" for 8 items × Cup/Can). A size points at a base with
//! `PUT /menu-item-sizes/{id}/base`; the base lines are expanded into that size's
//! `recipe_lines` (source='base') by [`crate::menu::recipe_expand`] on every base save
//! and pointer change, so nothing downstream learns what a base is.

use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    auth::{guards::require_same_org, jwt::Claims},
    errors::{AppError, AppErrorResponse},
    menu::{recipe_expand, studio::bump_catalog_revision},
    permissions::checker::check_permission,
};

pub(crate) fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

pub(crate) fn claims_org(claims: &Claims) -> Result<Uuid, AppError> {
    claims
        .org_id()
        .ok_or_else(|| AppError::Forbidden("A super admin must scope this to an org".into()))
}

// ── Shapes ───────────────────────────────────────────────────────────

/// One line of a base, stored in the ingredient's base unit.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RecipeBaseLineOut {
    pub id: Uuid,
    /// `null` = applies to every size; else only to sizes with this exact label (and
    /// wins over a `null` line for the same ingredient).
    pub size_label: Option<String>,
    pub ingredient_id: Uuid,
    pub ingredient_name: String,
    /// Base-unit quantity as a string (numeric fidelity).
    pub quantity: String,
    pub unit: String,
    pub sort: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RecipeBaseOut {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub name_ar: Option<String>,
    pub is_active: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub lines: Vec<RecipeBaseLineOut>,
    /// Item sizes currently pointing at this base.
    pub size_count: i64,
    /// Distinct menu items those sizes belong to.
    pub item_count: i64,
}

/// A line as submitted: quantity in `unit`, normalized to the ingredient's base unit
/// (and grossed up by yield) exactly like a size recipe line.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RecipeBaseLineInput {
    #[serde(default)]
    pub size_label: Option<String>,
    pub ingredient_id: Uuid,
    pub quantity: f64,
    pub unit: String,
    #[serde(default)]
    pub sort: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateRecipeBaseRequest {
    pub name: String,
    #[serde(default)]
    pub name_ar: Option<String>,
    #[serde(default)]
    pub is_active: Option<bool>,
    /// Optional initial lines (same as `PUT /recipe-bases/{id}/lines`).
    #[serde(default)]
    pub lines: Option<Vec<RecipeBaseLineInput>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PatchRecipeBaseRequest {
    pub name: Option<String>,
    /// `""` clears the Arabic name.
    pub name_ar: Option<String>,
    /// Deactivating a base removes its expanded lines from every size using it
    /// (the pointer stays); reactivating restores them.
    pub is_active: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PutRecipeBaseLinesRequest {
    pub lines: Vec<RecipeBaseLineInput>,
}

/// Result of any write that re-expanded recipes.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RecipeBaseSaveResult {
    pub base: RecipeBaseOut,
    /// Sizes (incl. linked copies) whose stored lines changed.
    pub sizes_changed: i64,
    pub catalog_revision: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RecipeBaseUsageSize {
    pub size_id: Uuid,
    pub size_label: String,
    pub menu_item_id: Uuid,
    pub menu_item_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RecipeBaseUsage {
    pub base_id: Uuid,
    pub item_count: i64,
    pub size_count: i64,
    pub sizes: Vec<RecipeBaseUsageSize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PutSizeBaseRequest {
    /// `null` detaches the size from its base (its base lines are removed).
    pub base_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SizeBaseResult {
    pub size_id: Uuid,
    pub base_id: Option<Uuid>,
    pub sizes_changed: i64,
    pub catalog_revision: i64,
}

// ── Loaders ──────────────────────────────────────────────────────────

async fn load_base(pool: &PgPool, id: Uuid) -> Result<RecipeBaseOut, AppError> {
    #[allow(clippy::type_complexity)]
    let row: Option<(
        Uuid,
        Uuid,
        String,
        Option<String>,
        bool,
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
        i64,
        i64,
    )> = sqlx::query_as(
        "SELECT b.id, b.org_id, b.name, b.name_ar, b.is_active, b.created_at, b.updated_at, \
                (SELECT count(*) FROM menu_item_sizes s JOIN menu_items mi ON mi.id = s.menu_item_id \
                  WHERE s.base_id = b.id AND mi.deleted_at IS NULL), \
                (SELECT count(DISTINCT s.menu_item_id) FROM menu_item_sizes s JOIN menu_items mi ON mi.id = s.menu_item_id \
                  WHERE s.base_id = b.id AND mi.deleted_at IS NULL) \
         FROM recipe_bases b WHERE b.id = $1 AND b.deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    let (id, org_id, name, name_ar, is_active, created_at, updated_at, size_count, item_count) =
        row.ok_or_else(|| AppError::NotFound("Recipe base not found".into()))?;

    let lines: Vec<(Uuid, Option<String>, Uuid, String, Decimal, String, i32)> = sqlx::query_as(
        "SELECT bl.id, bl.size_label, bl.ingredient_id, oi.name, bl.quantity, bl.unit, bl.sort \
         FROM recipe_base_lines bl JOIN org_ingredients oi ON oi.id = bl.ingredient_id \
         WHERE bl.base_id = $1 ORDER BY bl.sort, bl.size_label NULLS FIRST, oi.name",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;

    Ok(RecipeBaseOut {
        id,
        org_id,
        name,
        name_ar,
        is_active,
        created_at,
        updated_at,
        size_count,
        item_count,
        lines: lines
            .into_iter()
            .map(
                |(id, size_label, ingredient_id, ingredient_name, q, unit, sort)| {
                    RecipeBaseLineOut {
                        id,
                        size_label,
                        ingredient_id,
                        ingredient_name,
                        quantity: q.normalize().to_string(),
                        unit,
                        sort,
                    }
                },
            )
            .collect(),
    })
}

/// The base's org (404 when missing or soft-deleted), after the same-org check.
async fn base_org(pool: &PgPool, claims: &Claims, id: Uuid) -> Result<Uuid, AppError> {
    let org: Option<Uuid> =
        sqlx::query_scalar("SELECT org_id FROM recipe_bases WHERE id = $1 AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let org = org.ok_or_else(|| AppError::NotFound("Recipe base not found".into()))?;
    require_same_org(claims, Some(org))?;
    Ok(org)
}

fn clean_name(name: &str) -> Result<String, AppError> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 120 {
        return Err(AppError::BadRequest("Name must be 1–120 characters".into()));
    }
    Ok(n.to_string())
}

fn clean_opt(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Validate + normalize submitted lines to `(size_label, ingredient, qty, unit, sort)`.
pub(crate) async fn normalize_lines(
    pool: &PgPool,
    org_id: Uuid,
    lines: &[RecipeBaseLineInput],
) -> Result<Vec<(Option<String>, Uuid, Decimal, String, i32)>, AppError> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(lines.len());
    for (i, l) in lines.iter().enumerate() {
        let label = clean_opt(l.size_label.as_deref());
        if !seen.insert((label.clone(), l.ingredient_id)) {
            return Err(AppError::BadRequest(
                "Duplicate ingredient for the same size in base".into(),
            ));
        }
        if !l.quantity.is_finite() || l.quantity < 0.0 {
            return Err(AppError::BadRequest("quantity must be >= 0".into()));
        }
        let (unit, q) = crate::recipes::handlers::normalize_recipe_unit(
            pool,
            org_id,
            Some(l.ingredient_id),
            &l.unit,
            l.quantity,
        )
        .await?;
        out.push((
            label,
            l.ingredient_id,
            Decimal::try_from(q).unwrap_or(Decimal::ZERO),
            unit,
            l.sort.unwrap_or(i as i32),
        ));
    }
    Ok(out)
}

async fn items_using_base(
    conn: &mut sqlx::PgConnection,
    base: Uuid,
) -> Result<Vec<Uuid>, AppError> {
    Ok(
        sqlx::query_scalar("SELECT DISTINCT menu_item_id FROM menu_item_sizes WHERE base_id = $1")
            .bind(base)
            .fetch_all(&mut *conn)
            .await?,
    )
}

async fn replace_lines(
    conn: &mut sqlx::PgConnection,
    base: Uuid,
    lines: &[(Option<String>, Uuid, Decimal, String, i32)],
) -> Result<(), AppError> {
    sqlx::query("DELETE FROM recipe_base_lines WHERE base_id = $1")
        .bind(base)
        .execute(&mut *conn)
        .await?;
    for (label, ing, q, unit, sort) in lines {
        sqlx::query(
            "INSERT INTO recipe_base_lines (base_id, size_label, ingredient_id, quantity, unit, sort) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(base)
        .bind(label)
        .bind(ing)
        .bind(q)
        .bind(unit)
        .bind(sort)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

fn conflict_on_name(e: sqlx::Error) -> AppError {
    match &e {
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
            AppError::Conflict("A recipe base with this name already exists".into())
        }
        _ => AppError::Db(e),
    }
}

// ── Handlers ─────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/recipe-bases",
    tag = "menu",
    responses((status = 200, description = "The org's recipe bases with their lines", body = [RecipeBaseOut]), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_bases(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    let org = claims_org(&claims)?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM recipe_bases WHERE org_id = $1 AND deleted_at IS NULL ORDER BY lower(name)",
    )
    .bind(org)
    .fetch_all(pool.get_ref())
    .await?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(load_base(pool.get_ref(), id).await?);
    }
    Ok(HttpResponse::Ok().json(out))
}

#[utoipa::path(
    post,
    path = "/recipe-bases",
    tag = "menu",
    request_body = CreateRecipeBaseRequest,
    responses((status = 201, description = "Recipe base created", body = RecipeBaseOut), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_base(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateRecipeBaseRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    let org = claims_org(&claims)?;
    let b = body.into_inner();
    let name = clean_name(&b.name)?;
    let lines = match &b.lines {
        Some(l) => normalize_lines(pool.get_ref(), org, l).await?,
        None => Vec::new(),
    };

    let mut tx = pool.begin().await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO recipe_bases (org_id, name, name_ar, is_active) VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(org)
    .bind(&name)
    .bind(clean_opt(b.name_ar.as_deref()))
    .bind(b.is_active.unwrap_or(true))
    .fetch_one(&mut *tx)
    .await
    .map_err(conflict_on_name)?;
    replace_lines(&mut tx, id, &lines).await?;
    tx.commit().await?;

    Ok(HttpResponse::Created().json(load_base(pool.get_ref(), id).await?))
}

#[utoipa::path(
    get,
    path = "/recipe-bases/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Recipe base ID")),
    responses((status = 200, description = "Recipe base", body = RecipeBaseOut), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_base(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    base_org(pool.get_ref(), &claims, *id).await?;
    Ok(HttpResponse::Ok().json(load_base(pool.get_ref(), *id).await?))
}

#[utoipa::path(
    patch,
    path = "/recipe-bases/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Recipe base ID")),
    request_body = PatchRecipeBaseRequest,
    responses((status = 200, description = "Recipe base updated; sizes re-expanded when is_active changed", body = RecipeBaseSaveResult), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn patch_base(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<PatchRecipeBaseRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    let org = base_org(pool.get_ref(), &claims, *id).await?;
    let b = body.into_inner();
    let name = b.name.as_deref().map(clean_name).transpose()?;

    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE recipe_bases SET name = COALESCE($2, name), \
             name_ar = CASE WHEN $3::boolean THEN $4 ELSE name_ar END, \
             is_active = COALESCE($5, is_active) \
         WHERE id = $1",
    )
    .bind(*id)
    .bind(name)
    .bind(b.name_ar.is_some())
    .bind(clean_opt(b.name_ar.as_deref()))
    .bind(b.is_active)
    .execute(&mut *tx)
    .await
    .map_err(conflict_on_name)?;
    let items = items_using_base(&mut tx, *id).await?;
    let stats = recipe_expand::rebuild_items(&mut tx, &items).await?;
    let rev = bump_catalog_revision(&mut tx, org).await?;
    tx.commit().await?;

    Ok(HttpResponse::Ok().json(RecipeBaseSaveResult {
        base: load_base(pool.get_ref(), *id).await?,
        sizes_changed: stats.sizes_changed as i64,
        catalog_revision: rev,
    }))
}

#[utoipa::path(
    delete,
    path = "/recipe-bases/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Recipe base ID")),
    responses((status = 204, description = "Base soft-deleted; sizes detached and their base lines removed"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_base(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    let org = base_org(pool.get_ref(), &claims, *id).await?;

    let mut tx = pool.begin().await?;
    let items = items_using_base(&mut tx, *id).await?;
    sqlx::query("UPDATE menu_item_sizes SET base_id = NULL WHERE base_id = $1")
        .bind(*id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE recipe_bases SET deleted_at = now(), is_active = false WHERE id = $1")
        .bind(*id)
        .execute(&mut *tx)
        .await?;
    recipe_expand::rebuild_items(&mut tx, &items).await?;
    bump_catalog_revision(&mut tx, org).await?;
    tx.commit().await?;
    Ok(HttpResponse::NoContent().finish())
}

#[utoipa::path(
    put,
    path = "/recipe-bases/{id}/lines",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Recipe base ID")),
    request_body = PutRecipeBaseLinesRequest,
    responses((status = 200, description = "Lines replaced and expanded into every size using the base", body = RecipeBaseSaveResult), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_base_lines(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<PutRecipeBaseLinesRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    let org = base_org(pool.get_ref(), &claims, *id).await?;
    let lines = normalize_lines(pool.get_ref(), org, &body.lines).await?;

    let mut tx = pool.begin().await?;
    replace_lines(&mut tx, *id, &lines).await?;
    sqlx::query("UPDATE recipe_bases SET updated_at = now() WHERE id = $1")
        .bind(*id)
        .execute(&mut *tx)
        .await?;
    let items = items_using_base(&mut tx, *id).await?;
    let stats = recipe_expand::rebuild_items(&mut tx, &items).await?;
    let rev = bump_catalog_revision(&mut tx, org).await?;
    tx.commit().await?;

    Ok(HttpResponse::Ok().json(RecipeBaseSaveResult {
        base: load_base(pool.get_ref(), *id).await?,
        sizes_changed: stats.sizes_changed as i64,
        catalog_revision: rev,
    }))
}

#[utoipa::path(
    get,
    path = "/recipe-bases/{id}/usage",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Recipe base ID")),
    responses((status = 200, description = "Item sizes using the base", body = RecipeBaseUsage), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_base_usage(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    base_org(pool.get_ref(), &claims, *id).await?;
    let rows: Vec<(Uuid, String, Uuid, String)> = sqlx::query_as(
        "SELECT s.id, s.label, mi.id, mi.name FROM menu_item_sizes s \
         JOIN menu_items mi ON mi.id = s.menu_item_id \
         WHERE s.base_id = $1 AND mi.deleted_at IS NULL \
         ORDER BY mi.name, s.sort, s.label",
    )
    .bind(*id)
    .fetch_all(pool.get_ref())
    .await?;
    let mut items: Vec<Uuid> = rows.iter().map(|r| r.2).collect();
    items.sort();
    items.dedup();
    Ok(HttpResponse::Ok().json(RecipeBaseUsage {
        base_id: *id,
        item_count: items.len() as i64,
        size_count: rows.len() as i64,
        sizes: rows
            .into_iter()
            .map(
                |(size_id, size_label, menu_item_id, menu_item_name)| RecipeBaseUsageSize {
                    size_id,
                    size_label,
                    menu_item_id,
                    menu_item_name,
                },
            )
            .collect(),
    }))
}

#[utoipa::path(
    put,
    path = "/menu-item-sizes/{size_id}/base",
    tag = "menu",
    params(("size_id" = Uuid, Path, description = "menu_item_sizes ID")),
    request_body = PutSizeBaseRequest,
    responses((status = 200, description = "Base set (or cleared) and the size re-expanded", body = SizeBaseResult), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_size_base(
    req: HttpRequest,
    pool: crate::db::Db,
    size_id: web::Path<Uuid>,
    body: web::Json<PutSizeBaseRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    let owner: Option<(Uuid, Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT mi.id, mi.org_id, mi.recipe_source_item_id FROM menu_item_sizes s \
         JOIN menu_items mi ON mi.id = s.menu_item_id \
         WHERE s.id = $1 AND mi.deleted_at IS NULL",
    )
    .bind(*size_id)
    .fetch_optional(pool.get_ref())
    .await?;
    let (item_id, org, linked) =
        owner.ok_or_else(|| AppError::NotFound("Size not found".into()))?;
    require_same_org(&claims, Some(org))?;
    if linked.is_some() && body.base_id.is_some() {
        return Err(AppError::Conflict(
            "This item's recipe follows another item; unlink it first".into(),
        ));
    }
    if let Some(b) = body.base_id {
        let ok: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM recipe_bases WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL",
        )
        .bind(b)
        .bind(org)
        .fetch_optional(pool.get_ref())
        .await?;
        if ok.is_none() {
            return Err(AppError::BadRequest(
                "Recipe base not found in this organization".into(),
            ));
        }
    }

    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE menu_item_sizes SET base_id = $2 WHERE id = $1")
        .bind(*size_id)
        .bind(body.base_id)
        .execute(&mut *tx)
        .await?;
    let stats = recipe_expand::rebuild_item(&mut tx, item_id).await?;
    let rev = bump_catalog_revision(&mut tx, org).await?;
    tx.commit().await?;

    Ok(HttpResponse::Ok().json(SizeBaseResult {
        size_id: *size_id,
        base_id: body.base_id,
        sizes_changed: stats.sizes_changed as i64,
        catalog_revision: rev,
    }))
}
