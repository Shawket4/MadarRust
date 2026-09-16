//! Linked copies (menu modeling B10): a staff / loyalty twin whose recipe FOLLOWS its
//! source. The copy has its own name, price, category and no choice groups (like the
//! hand-made staff twins); its sizes carry the source's labels and every recipe line
//! is a `source='linked'` copy of the source size with the same label. Every recipe
//! save on the source re-copies (see [`crate::menu::recipe_expand::rebuild_item`]).
//! Unlinking keeps the lines as the copy's own.

use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    auth::guards::require_same_org,
    errors::{AppError, AppErrorResponse},
    menu::{bases::extract_claims, recipe_expand, studio::bump_catalog_revision},
    permissions::checker::check_permission,
};

struct ItemRef {
    id: Uuid,
    org_id: Uuid,
}

async fn fetch_item_basics(pool: &PgPool, id: Uuid) -> Result<Option<ItemRef>, AppError> {
    let row: Option<(Uuid, Uuid)> =
        sqlx::query_as("SELECT id, org_id FROM menu_items WHERE id = $1 AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(id, org_id)| ItemRef { id, org_id }))
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateLinkedCopyRequest {
    pub name: String,
    /// Price in piastres for every size of the copy (0 for a staff drink).
    pub price: i32,
    /// Menu category of the copy; `null` keeps the source's category.
    #[serde(default)]
    pub category_id: Option<Uuid>,
}

/// Link state of an item, from either side.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RecipeLinkInfo {
    pub menu_item_id: Uuid,
    /// The item this one's recipe follows, or `null`.
    pub recipe_source_item_id: Option<Uuid>,
    pub recipe_source_item_name: Option<String>,
    /// Live items whose recipe follows this one.
    pub linked_copy_ids: Vec<Uuid>,
    /// For a copy: `true` when its stored lines equal the source's for every size label
    /// the copy has (lint F19, twin drift). `null` for an item that is not a copy.
    pub in_sync: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LinkedCopyResult {
    pub menu_item_id: Uuid,
    pub link: RecipeLinkInfo,
    pub catalog_revision: i64,
}

/// Link info for one item (no auth; callers check).
pub async fn link_info(pool: &PgPool, item_id: Uuid) -> Result<RecipeLinkInfo, AppError> {
    let src: Option<(Option<Uuid>, Option<String>)> = sqlx::query_as(
        "SELECT mi.recipe_source_item_id, s.name FROM menu_items mi \
         LEFT JOIN menu_items s ON s.id = mi.recipe_source_item_id \
         WHERE mi.id = $1",
    )
    .bind(item_id)
    .fetch_optional(pool)
    .await?;
    let (source_id, source_name) = src.unwrap_or((None, None));
    let copies: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM menu_items WHERE recipe_source_item_id = $1 AND deleted_at IS NULL ORDER BY name, id",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await?;
    let in_sync = match source_id {
        Some(s) => {
            let mut conn = pool.acquire().await?;
            let labels: Vec<String> =
                sqlx::query_scalar("SELECT label FROM menu_item_sizes WHERE menu_item_id = $1")
                    .bind(item_id)
                    .fetch_all(&mut *conn)
                    .await?;
            let a = recipe_expand::recipe_fingerprint(&mut conn, item_id, &labels).await?;
            let b = recipe_expand::recipe_fingerprint(&mut conn, s, &labels).await?;
            Some(a == b)
        }
        None => None,
    };
    Ok(RecipeLinkInfo {
        menu_item_id: item_id,
        recipe_source_item_id: source_id,
        recipe_source_item_name: source_name,
        linked_copy_ids: copies,
        in_sync,
    })
}

#[utoipa::path(
    post,
    path = "/menu-items/{id}/linked-copy",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Source menu item ID")),
    request_body = CreateLinkedCopyRequest,
    responses((status = 201, description = "Copy created with a recipe that follows the source", body = LinkedCopyResult), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_linked_copy(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<CreateLinkedCopyRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "create").await?;
    let basics = fetch_item_basics(pool.get_ref(), *id)
        .await?
        .ok_or_else(|| AppError::NotFound("Menu item not found".into()))?;
    require_same_org(&claims, Some(basics.org_id))?;
    let b = body.into_inner();
    let name = b.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(AppError::BadRequest("Name must be 1–200 characters".into()));
    }
    if b.price < 0 {
        return Err(AppError::BadRequest("price must be >= 0".into()));
    }
    if let Some(c) = b.category_id {
        let ok: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM categories WHERE id = $1 AND org_id = $2")
                .bind(c)
                .bind(basics.org_id)
                .fetch_optional(pool.get_ref())
                .await?;
        if ok.is_none() {
            return Err(AppError::BadRequest(
                "Category not found in this organization".into(),
            ));
        }
    }

    // A copy of a copy follows the ROOT, so a link is never a chain.
    let root: Uuid = sqlx::query_scalar(
        "SELECT COALESCE(recipe_source_item_id, id) FROM menu_items WHERE id = $1",
    )
    .bind(basics.id)
    .fetch_one(pool.get_ref())
    .await?;

    let mut tx = pool.begin().await?;
    let new_item: Uuid = sqlx::query_scalar(
        "INSERT INTO menu_items (org_id, category_id, name, description, base_price, is_active, recipe_source_item_id) \
         SELECT org_id, COALESCE($2, category_id), $3, description, $4, is_active, $5 \
         FROM menu_items WHERE id = $1 RETURNING id",
    )
    .bind(basics.id)
    .bind(b.category_id)
    .bind(&name)
    .bind(b.price)
    .bind(root)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO menu_item_sizes (menu_item_id, label, price, sort, is_active) \
         SELECT $1, label, $2, sort, is_active FROM menu_item_sizes WHERE menu_item_id = $3",
    )
    .bind(new_item)
    .bind(b.price)
    .bind(basics.id)
    .execute(&mut *tx)
    .await?;
    recipe_expand::rebuild_item(&mut tx, new_item).await?;
    let rev = bump_catalog_revision(&mut tx, basics.org_id).await?;
    tx.commit().await?;

    Ok(HttpResponse::Created().json(LinkedCopyResult {
        menu_item_id: new_item,
        link: link_info(pool.get_ref(), new_item).await?,
        catalog_revision: rev,
    }))
}

#[utoipa::path(
    get,
    path = "/menu-items/{id}/recipe-link",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item ID")),
    responses((status = 200, description = "Recipe link state of the item", body = RecipeLinkInfo), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_recipe_link(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    let basics = fetch_item_basics(pool.get_ref(), *id)
        .await?
        .ok_or_else(|| AppError::NotFound("Menu item not found".into()))?;
    require_same_org(&claims, Some(basics.org_id))?;
    Ok(HttpResponse::Ok().json(link_info(pool.get_ref(), basics.id).await?))
}

#[utoipa::path(
    delete,
    path = "/menu-items/{id}/recipe-link",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Linked copy menu item ID")),
    responses((status = 200, description = "Unlinked; the copied lines are now the item's own", body = RecipeLinkInfo), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_recipe_link(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    let basics = fetch_item_basics(pool.get_ref(), *id)
        .await?
        .ok_or_else(|| AppError::NotFound("Menu item not found".into()))?;
    require_same_org(&claims, Some(basics.org_id))?;

    let mut tx = pool.begin().await?;
    let had: Option<Uuid> = sqlx::query_scalar(
        "UPDATE menu_items SET recipe_source_item_id = NULL, updated_at = now() \
         WHERE id = $1 AND recipe_source_item_id IS NOT NULL RETURNING id",
    )
    .bind(basics.id)
    .fetch_optional(&mut *tx)
    .await?;
    if had.is_some() {
        sqlx::query(
            "UPDATE recipe_lines SET source = 'own', updated_at = now() \
             WHERE owner_type = 'item_size' AND source = 'linked' \
               AND owner_id IN (SELECT id FROM menu_item_sizes WHERE menu_item_id = $1)",
        )
        .bind(basics.id)
        .execute(&mut *tx)
        .await?;
        bump_catalog_revision(&mut tx, basics.org_id).await?;
    }
    tx.commit().await?;
    Ok(HttpResponse::Ok().json(link_info(pool.get_ref(), basics.id).await?))
}
