//! Deal endpoints (§2.3). Reading uses `menu.items.read`; writing `menu.deals.edit`.

use std::collections::{HashMap, HashSet};

use actix_web::{HttpRequest, HttpResponse, web};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    authz::{Cap, require::require},
    combos::handlers::{validate_windows, write_windows},
    deals::{
        load::load_rules,
        types::{DealBranchWrite, DealListQuery, DealPoolEntry, DealRule, DealWrite},
    },
    errors::{AppError, AppErrorResponse},
    menu::bases::{claims_org, extract_claims},
};

fn invalid(field: &str) -> AppError {
    AppError::CodedVars {
        status: 400,
        code: "DEAL_INVALID",
        reason: format!("Check the deal's \"{field}\"."),
        vars: serde_json::json!({ "field": field }),
    }
}

async fn validate_pool(
    conn: &mut PgConnection,
    org: Uuid,
    pool: &[DealPoolEntry],
    field: &str,
) -> Result<(), AppError> {
    let mut items = Vec::new();
    let mut cats = Vec::new();
    for e in pool {
        match (e.menu_item_id, e.category_id) {
            (Some(i), None) => items.push(i),
            (None, Some(c)) => cats.push(c),
            _ => return Err(invalid(field)),
        }
        if e.size_label.as_deref().is_some_and(|s| s.trim().is_empty()) {
            return Err(invalid(field));
        }
    }
    let live: HashMap<Uuid, String> = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT id, kind FROM menu_items WHERE org_id = $1 AND id = ANY($2) AND deleted_at IS NULL",
    )
    .bind(org)
    .bind(&items)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();
    if items
        .iter()
        .any(|i| live.get(i).is_none_or(|k| k != "item"))
    {
        return Err(invalid(field));
    }
    let known: HashSet<Uuid> = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM categories WHERE org_id = $1 AND id = ANY($2) AND deleted_at IS NULL",
    )
    .bind(org)
    .bind(&cats)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();
    if cats.iter().any(|c| !known.contains(c)) {
        return Err(invalid(field));
    }
    Ok(())
}

async fn validate(conn: &mut PgConnection, org: Uuid, b: &DealWrite) -> Result<(), AppError> {
    if b.name.trim().is_empty() {
        return Err(invalid("name"));
    }
    match b.kind.as_str() {
        "n_for_price" => {
            if !(2..=20).contains(&b.qty) {
                return Err(invalid("qty"));
            }
            if b.price.is_none_or(|p| p < 0) {
                return Err(invalid("price"));
            }
            if b.get_qty.is_some() {
                return Err(invalid("get_qty"));
            }
            if b.get_percent.is_some() {
                return Err(invalid("get_percent"));
            }
            if !b.reward_pool.is_empty() {
                return Err(invalid("reward_pool"));
            }
        }
        "buy_get" => {
            if !(1..=20).contains(&b.qty) {
                return Err(invalid("qty"));
            }
            if b.price.is_some() {
                return Err(invalid("price"));
            }
            if b.get_qty.is_none_or(|g| !(1..=20).contains(&g)) {
                return Err(invalid("get_qty"));
            }
            if b.get_percent.is_none_or(|g| !(1..=100).contains(&g)) {
                return Err(invalid("get_percent"));
            }
        }
        _ => return Err(invalid("kind")),
    }
    if b.max_per_order.is_some_and(|m| m < 1) {
        return Err(invalid("max_per_order"));
    }
    if b.pool.is_empty() {
        return Err(invalid("pool"));
    }
    validate_pool(&mut *conn, org, &b.pool, "pool").await?;
    validate_pool(&mut *conn, org, &b.reward_pool, "reward_pool").await?;
    validate_windows(&mut *conn, org, &b.windows, "DEAL_INVALID").await
}

async fn write_children(
    conn: &mut PgConnection,
    org: Uuid,
    id: Uuid,
    b: &DealWrite,
) -> Result<(), AppError> {
    sqlx::query("DELETE FROM deal_rule_items WHERE deal_rule_id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    for (role, pool) in [("pool", &b.pool), ("reward", &b.reward_pool)] {
        for (i, e) in pool.iter().enumerate() {
            sqlx::query(
                "INSERT INTO deal_rule_items (org_id, deal_rule_id, role, menu_item_id, category_id, size_label, sort) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(org)
            .bind(id)
            .bind(role)
            .bind(e.menu_item_id)
            .bind(e.category_id)
            .bind(e.size_label.as_deref())
            .bind(i as i32)
            .execute(&mut *conn)
            .await?;
        }
    }
    write_windows(&mut *conn, org, "deal_rule_id", id, &b.windows).await
}

async fn load_one(conn: &mut PgConnection, org: Uuid, id: Uuid) -> Result<DealRule, AppError> {
    load_rules(&mut *conn, org, Some(&[id]))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::NotFound("Deal not found".into()))
}

async fn live_deal(conn: &mut PgConnection, org: Uuid, id: Uuid) -> Result<(), AppError> {
    let ok: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM deal_rules WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL)",
    )
    .bind(id)
    .bind(org)
    .fetch_one(&mut *conn)
    .await?;
    if ok {
        Ok(())
    } else {
        Err(AppError::NotFound("Deal not found".into()))
    }
}

#[utoipa::path(
    get,
    path = "/deals",
    tag = "menu",
    params(DealListQuery),
    responses((status = 200, description = "The org's deal rules (not deleted)", body = Vec<DealRule>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_deals(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<DealListQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuItemsRead, None).await?;
    let org = claims_org(&claims)?;
    let mut conn = pool.acquire().await?;
    let rules: Vec<DealRule> = load_rules(&mut conn, org, None)
        .await?
        .into_iter()
        .filter(|r| query.is_active.is_none_or(|a| r.is_active == a))
        .collect();
    Ok(HttpResponse::Ok().json(rules))
}

#[utoipa::path(
    post,
    path = "/deals",
    tag = "menu",
    request_body = DealWrite,
    responses((status = 201, description = "The deal rule", body = DealRule), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_deal(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<DealWrite>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuDealsEdit, None).await?;
    let org = claims_org(&claims)?;
    let mut tx = pool.begin().await?;
    validate(&mut tx, org, &body).await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO deal_rules (org_id, name, name_translations, kind, qty, price, get_qty, get_percent, \
                                 max_per_order, sort, is_active) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) RETURNING id",
    )
    .bind(org)
    .bind(body.name.trim())
    .bind(&body.name_translations)
    .bind(&body.kind)
    .bind(body.qty)
    .bind(body.price)
    .bind(body.get_qty)
    .bind(body.get_percent)
    .bind(body.max_per_order)
    .bind(body.sort)
    .bind(body.is_active)
    .fetch_one(&mut *tx)
    .await?;
    write_children(&mut tx, org, id, &body).await?;
    tx.commit().await?;
    let mut conn = pool.acquire().await?;
    Ok(HttpResponse::Created().json(load_one(&mut conn, org, id).await?))
}

#[utoipa::path(
    put,
    path = "/deals/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Deal rule ID")),
    request_body = DealWrite,
    responses((status = 200, description = "The deal rule", body = DealRule), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_deal(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<DealWrite>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuDealsEdit, None).await?;
    let org = claims_org(&claims)?;
    let id = *id;
    let mut tx = pool.begin().await?;
    live_deal(&mut tx, org, id).await?;
    validate(&mut tx, org, &body).await?;
    sqlx::query(
        "UPDATE deal_rules SET name = $2, name_translations = $3, kind = $4, qty = $5, price = $6, \
                get_qty = $7, get_percent = $8, max_per_order = $9, sort = $10, is_active = $11, \
                updated_at = now() \
          WHERE id = $1",
    )
    .bind(id)
    .bind(body.name.trim())
    .bind(&body.name_translations)
    .bind(&body.kind)
    .bind(body.qty)
    .bind(body.price)
    .bind(body.get_qty)
    .bind(body.get_percent)
    .bind(body.max_per_order)
    .bind(body.sort)
    .bind(body.is_active)
    .execute(&mut *tx)
    .await?;
    write_children(&mut tx, org, id, &body).await?;
    tx.commit().await?;
    let mut conn = pool.acquire().await?;
    Ok(HttpResponse::Ok().json(load_one(&mut conn, org, id).await?))
}

#[utoipa::path(
    delete,
    path = "/deals/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Deal rule ID")),
    responses((status = 204, description = "Soft-deleted; applied deals keep pointing at it"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_deal(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    require(pool.get_ref(), &claims, Cap::MenuDealsEdit, None).await?;
    let org = claims_org(&claims)?;
    let done = sqlx::query(
        "UPDATE deal_rules SET deleted_at = now(), updated_at = now() \
          WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL",
    )
    .bind(*id)
    .bind(org)
    .execute(pool.get_ref())
    .await?;
    if done.rows_affected() == 0 {
        return Err(AppError::NotFound("Deal not found".into()));
    }
    Ok(HttpResponse::NoContent().finish())
}

#[utoipa::path(
    put,
    path = "/deals/{id}/branches/{branch_id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Deal rule ID"), ("branch_id" = Uuid, Path, description = "Branch ID")),
    request_body = DealBranchWrite,
    responses((status = 204, description = "The branch's on/off override saved"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_deal_branch(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(Uuid, Uuid)>,
    body: web::Json<DealBranchWrite>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let (id, branch_id) = *path;
    require(pool.get_ref(), &claims, Cap::MenuDealsEdit, Some(branch_id)).await?;
    let org = claims_org(&claims)?;
    let mut conn = pool.acquire().await?;
    live_deal(&mut conn, org, id).await?;
    let branch_ok: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM branches WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL)",
    )
    .bind(branch_id)
    .bind(org)
    .fetch_one(&mut *conn)
    .await?;
    if !branch_ok {
        return Err(AppError::NotFound("Branch not found".into()));
    }
    sqlx::query(
        "INSERT INTO deal_rule_branch_overrides (deal_rule_id, branch_id, org_id, is_active) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (deal_rule_id, branch_id) DO UPDATE SET is_active = EXCLUDED.is_active",
    )
    .bind(id)
    .bind(branch_id)
    .bind(org)
    .bind(body.is_active)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE deal_rules SET updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(HttpResponse::NoContent().finish())
}

#[utoipa::path(
    delete,
    path = "/deals/{id}/branches/{branch_id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Deal rule ID"), ("branch_id" = Uuid, Path, description = "Branch ID")),
    responses((status = 204, description = "The branch inherits the rule's own active flag"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_deal_branch(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<(Uuid, Uuid)>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let (id, branch_id) = *path;
    require(pool.get_ref(), &claims, Cap::MenuDealsEdit, Some(branch_id)).await?;
    let org = claims_org(&claims)?;
    let mut conn = pool.acquire().await?;
    live_deal(&mut conn, org, id).await?;
    sqlx::query(
        "DELETE FROM deal_rule_branch_overrides WHERE deal_rule_id = $1 AND branch_id = $2",
    )
    .bind(id)
    .bind(branch_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE deal_rules SET updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    Ok(HttpResponse::NoContent().finish())
}
