//! Packaging rules (menu modeling B8): "every Iced coffee Cup takes a 16oz cup, a lid
//! and a straw; every Can takes a straw". A rule matches item sizes by menu item, menu
//! category and/or size label; the most specific active match for a size contributes
//! its lines as `recipe_lines` with source='rule' (ranking in
//! [`crate::menu::recipe_expand::best_rule_for`]). An own line for the same ingredient
//! wins over the rule, and so does a base line.
//!
//! When rules take effect: rule CRUD only stores the rule. Lines are (re)expanded by
//! `POST /packaging-rules/apply` (org-wide), and for one item whenever that item's
//! sizes or recipe are saved through the studio, its base changes, or its source is
//! re-copied. Dine-in sales skip every ingredient whose category `is_packaging`
//! (slug `packaging` as fallback) regardless of where the line came from.

use actix_web::{HttpRequest, HttpResponse, web};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    auth::guards::require_same_org,
    authz::{Cap, require::require},
    errors::{AppError, AppErrorResponse},
    menu::{
        bases::{claims_org, extract_claims},
        recipe_expand,
        studio::bump_catalog_revision,
    },
    permissions::checker::check_permission,
};

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PackagingRuleLineOut {
    pub ingredient_id: Uuid,
    pub ingredient_name: String,
    /// Base-unit quantity as a string.
    pub quantity: String,
    pub unit: String,
    pub sort: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PackagingRuleOut {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    /// Menu category (`categories.id`) the rule matches, or `null` = any.
    pub match_category_id: Option<Uuid>,
    /// Exact size label the rule matches (`Cup`, `Can`), or `null` = any.
    pub match_size_label: Option<String>,
    /// One menu item the rule matches, or `null` = any.
    pub match_item_id: Option<Uuid>,
    pub sort: i32,
    pub is_active: bool,
    pub lines: Vec<PackagingRuleLineOut>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PackagingRuleLineInput {
    pub ingredient_id: Uuid,
    pub quantity: f64,
    pub unit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreatePackagingRuleRequest {
    pub name: String,
    #[serde(default)]
    pub match_category_id: Option<Uuid>,
    #[serde(default)]
    pub match_size_label: Option<String>,
    #[serde(default)]
    pub match_item_id: Option<Uuid>,
    #[serde(default)]
    pub sort: Option<i32>,
    #[serde(default)]
    pub is_active: Option<bool>,
    pub lines: Vec<PackagingRuleLineInput>,
}

/// Partial update. A match field is replaced only when its key is present
/// (`null` clears it); `lines`, when present, replaces the whole set.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct PatchPackagingRuleRequest {
    pub name: Option<String>,
    #[serde(default, deserialize_with = "crate::menu::packaging::double_option")]
    #[schema(value_type = Option<Uuid>)]
    pub match_category_id: Option<Option<Uuid>>,
    #[serde(default, deserialize_with = "crate::menu::packaging::double_option")]
    #[schema(value_type = Option<String>)]
    pub match_size_label: Option<Option<String>>,
    #[serde(default, deserialize_with = "crate::menu::packaging::double_option")]
    #[schema(value_type = Option<Uuid>)]
    pub match_item_id: Option<Option<Uuid>>,
    pub sort: Option<i32>,
    pub is_active: Option<bool>,
    pub lines: Option<Vec<PackagingRuleLineInput>>,
}

/// `absent` → None, `null` → Some(None), value → Some(Some(v)).
pub(crate) fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ApplyPackagingRulesResult {
    /// Sizes examined (every size of every live item in the org).
    pub sizes_seen: i64,
    /// Sizes (incl. linked copies) whose stored lines changed.
    pub sizes_changed: i64,
    /// Sizes that now carry at least one rule line.
    pub sizes_with_rule: i64,
    /// Sizes that still have an OWN line in a packaging category: typed by hand, they
    /// are kept (and win over a rule for the same ingredient) — review them.
    pub sizes_with_manual_packaging: i64,
    pub catalog_revision: i64,
}

async fn load_rule(pool: &PgPool, id: Uuid) -> Result<PackagingRuleOut, AppError> {
    #[allow(clippy::type_complexity)]
    let row: Option<(
        Uuid,
        Uuid,
        String,
        Option<Uuid>,
        Option<String>,
        Option<Uuid>,
        i32,
        bool,
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    )> = sqlx::query_as(
        "SELECT id, org_id, name, match_category_id, match_size_label, match_item_id, sort, \
                is_active, created_at, updated_at \
         FROM packaging_rules WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    let (id, org_id, name, cat, label, item, sort, is_active, created_at, updated_at) =
        row.ok_or_else(|| AppError::NotFound("Packaging rule not found".into()))?;
    let lines: Vec<(Uuid, String, Decimal, String, i32)> = sqlx::query_as(
        "SELECT l.ingredient_id, oi.name, l.quantity, l.unit, l.sort \
         FROM packaging_rule_lines l JOIN org_ingredients oi ON oi.id = l.ingredient_id \
         WHERE l.rule_id = $1 ORDER BY l.sort, oi.name",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    Ok(PackagingRuleOut {
        id,
        org_id,
        name,
        match_category_id: cat,
        match_size_label: label,
        match_item_id: item,
        sort,
        is_active,
        created_at,
        updated_at,
        lines: lines
            .into_iter()
            .map(
                |(ingredient_id, ingredient_name, q, unit, sort)| PackagingRuleLineOut {
                    ingredient_id,
                    ingredient_name,
                    quantity: q.normalize().to_string(),
                    unit,
                    sort,
                },
            )
            .collect(),
    })
}

async fn rule_org(
    pool: &PgPool,
    claims: &crate::auth::jwt::Claims,
    id: Uuid,
) -> Result<Uuid, AppError> {
    let org: Option<Uuid> = sqlx::query_scalar("SELECT org_id FROM packaging_rules WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    let org = org.ok_or_else(|| AppError::NotFound("Packaging rule not found".into()))?;
    require_same_org(claims, Some(org))?;
    Ok(org)
}

async fn validate_matches(
    pool: &PgPool,
    org: Uuid,
    cat: Option<Uuid>,
    item: Option<Uuid>,
) -> Result<(), AppError> {
    if let Some(c) = cat {
        let ok: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM categories WHERE id = $1 AND org_id = $2")
                .bind(c)
                .bind(org)
                .fetch_optional(pool)
                .await?;
        if ok.is_none() {
            return Err(AppError::BadRequest(
                "match_category_id is not a menu category of this organization".into(),
            ));
        }
    }
    if let Some(i) = item {
        let ok: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM menu_items WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL",
        )
        .bind(i)
        .bind(org)
        .fetch_optional(pool)
        .await?;
        if ok.is_none() {
            return Err(AppError::BadRequest(
                "match_item_id is not a menu item of this organization".into(),
            ));
        }
    }
    Ok(())
}

async fn normalize_rule_lines(
    pool: &PgPool,
    org: Uuid,
    lines: &[PackagingRuleLineInput],
) -> Result<Vec<(Uuid, Decimal, String)>, AppError> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(lines.len());
    for l in lines {
        if !seen.insert(l.ingredient_id) {
            return Err(AppError::BadRequest("Duplicate ingredient in rule".into()));
        }
        if !l.quantity.is_finite() || l.quantity < 0.0 {
            return Err(AppError::BadRequest("quantity must be >= 0".into()));
        }
        let (unit, q) = crate::recipes::handlers::normalize_recipe_unit(
            pool,
            org,
            Some(l.ingredient_id),
            &l.unit,
            l.quantity,
        )
        .await?;
        out.push((
            l.ingredient_id,
            Decimal::try_from(q).unwrap_or(Decimal::ZERO),
            unit,
        ));
    }
    Ok(out)
}

async fn replace_rule_lines(
    conn: &mut sqlx::PgConnection,
    rule: Uuid,
    lines: &[(Uuid, Decimal, String)],
) -> Result<(), AppError> {
    sqlx::query("DELETE FROM packaging_rule_lines WHERE rule_id = $1")
        .bind(rule)
        .execute(&mut *conn)
        .await?;
    for (i, (ing, q, unit)) in lines.iter().enumerate() {
        sqlx::query(
            "INSERT INTO packaging_rule_lines (rule_id, ingredient_id, quantity, unit, sort) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(rule)
        .bind(ing)
        .bind(q)
        .bind(unit)
        .bind(i as i32)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

fn clean_label(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn clean_name(name: &str) -> Result<String, AppError> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 120 {
        return Err(AppError::BadRequest("Name must be 1–120 characters".into()));
    }
    Ok(n.to_string())
}

#[utoipa::path(
    get,
    path = "/packaging-rules",
    tag = "menu",
    responses((status = 200, description = "The org's packaging rules", body = [PackagingRuleOut]), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_rules(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    let org = claims_org(&claims)?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM packaging_rules WHERE org_id = $1 ORDER BY sort, lower(name), created_at",
    )
    .bind(org)
    .fetch_all(pool.get_ref())
    .await?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(load_rule(pool.get_ref(), id).await?);
    }
    Ok(HttpResponse::Ok().json(out))
}

#[utoipa::path(
    post,
    path = "/packaging-rules",
    tag = "menu",
    request_body = CreatePackagingRuleRequest,
    responses((status = 201, description = "Rule created (not yet applied; see POST /packaging-rules/apply)", body = PackagingRuleOut), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_rule(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreatePackagingRuleRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    let org = claims_org(&claims)?;
    let b = body.into_inner();
    let name = clean_name(&b.name)?;
    let label = clean_label(b.match_size_label.as_deref());
    if b.match_category_id.is_none() && label.is_none() && b.match_item_id.is_none() {
        return Err(AppError::BadRequest(
            "A rule must match a category, a size label or an item".into(),
        ));
    }
    validate_matches(pool.get_ref(), org, b.match_category_id, b.match_item_id).await?;
    let lines = normalize_rule_lines(pool.get_ref(), org, &b.lines).await?;

    let mut tx = pool.begin().await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO packaging_rules (org_id, name, match_category_id, match_size_label, match_item_id, sort, is_active) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(org)
    .bind(&name)
    .bind(b.match_category_id)
    .bind(&label)
    .bind(b.match_item_id)
    .bind(b.sort.unwrap_or(0))
    .bind(b.is_active.unwrap_or(true))
    .fetch_one(&mut *tx)
    .await?;
    replace_rule_lines(&mut tx, id, &lines).await?;
    tx.commit().await?;
    Ok(HttpResponse::Created().json(load_rule(pool.get_ref(), id).await?))
}

#[utoipa::path(
    patch,
    path = "/packaging-rules/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Packaging rule ID")),
    request_body = PatchPackagingRuleRequest,
    responses((status = 200, description = "Rule updated (not yet applied)", body = PackagingRuleOut), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn patch_rule(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<PatchPackagingRuleRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    let org = rule_org(pool.get_ref(), &claims, *id).await?;
    let b = body.into_inner();

    let cur: (Option<Uuid>, Option<String>, Option<Uuid>) = sqlx::query_as(
        "SELECT match_category_id, match_size_label, match_item_id FROM packaging_rules WHERE id = $1",
    )
    .bind(*id)
    .fetch_one(pool.get_ref())
    .await?;
    let cat = b.match_category_id.unwrap_or(cur.0);
    let label = match &b.match_size_label {
        Some(l) => clean_label(l.as_deref()),
        None => cur.1,
    };
    let item = b.match_item_id.unwrap_or(cur.2);
    if cat.is_none() && label.is_none() && item.is_none() {
        return Err(AppError::BadRequest(
            "A rule must match a category, a size label or an item".into(),
        ));
    }
    validate_matches(pool.get_ref(), org, cat, item).await?;
    let name = b.name.as_deref().map(clean_name).transpose()?;
    let lines = match &b.lines {
        Some(l) => Some(normalize_rule_lines(pool.get_ref(), org, l).await?),
        None => None,
    };

    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE packaging_rules SET name = COALESCE($2, name), match_category_id = $3, \
             match_size_label = $4, match_item_id = $5, sort = COALESCE($6, sort), \
             is_active = COALESCE($7, is_active), updated_at = now() \
         WHERE id = $1",
    )
    .bind(*id)
    .bind(name)
    .bind(cat)
    .bind(&label)
    .bind(item)
    .bind(b.sort)
    .bind(b.is_active)
    .execute(&mut *tx)
    .await?;
    if let Some(l) = &lines {
        replace_rule_lines(&mut tx, *id, l).await?;
    }
    tx.commit().await?;
    Ok(HttpResponse::Ok().json(load_rule(pool.get_ref(), *id).await?))
}

#[utoipa::path(
    delete,
    path = "/packaging-rules/{id}",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Packaging rule ID")),
    responses((status = 204, description = "Rule deleted (its expanded lines stay until the next apply)"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_rule(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    rule_org(pool.get_ref(), &claims, *id).await?;
    sqlx::query("DELETE FROM packaging_rules WHERE id = $1")
        .bind(*id)
        .execute(pool.get_ref())
        .await?;
    Ok(HttpResponse::NoContent().finish())
}

#[utoipa::path(
    post,
    path = "/packaging-rules/apply",
    tag = "menu",
    responses((status = 200, description = "Every item size in the org re-expanded against the current rules and bases", body = ApplyPackagingRulesResult), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn apply_rules(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    // Org-wide bulk re-expansion: its own capability, owner-only by default.
    require(pool.get_ref(), &claims, Cap::MenuPackagingRulesApply, None).await?;
    let org = claims_org(&claims)?;
    let result = apply_for_org(pool.get_ref(), org).await?;
    Ok(HttpResponse::Ok().json(result))
}

/// Org-wide re-expansion (roots first, so copies re-copy fresh lines).
pub async fn apply_for_org(
    pool: &PgPool,
    org: Uuid,
) -> Result<ApplyPackagingRulesResult, AppError> {
    let mut tx = pool.begin().await?;
    let roots: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM menu_items WHERE org_id = $1 AND deleted_at IS NULL \
           AND recipe_source_item_id IS NULL ORDER BY id",
    )
    .bind(org)
    .fetch_all(&mut *tx)
    .await?;
    let mut stats = recipe_expand::rebuild_items(&mut tx, &roots).await?;
    // Orphaned copies (source deleted but link not yet cleared) still get examined.
    let orphans: Vec<Uuid> = sqlx::query_scalar(
        "SELECT c.id FROM menu_items c JOIN menu_items s ON s.id = c.recipe_source_item_id \
         WHERE c.org_id = $1 AND c.deleted_at IS NULL AND s.deleted_at IS NOT NULL",
    )
    .bind(org)
    .fetch_all(&mut *tx)
    .await?;
    stats += recipe_expand::rebuild_items(&mut tx, &orphans).await?;

    let (with_rule, manual): (i64, i64) = sqlx::query_as(
        "SELECT \
           count(DISTINCT s.id) FILTER (WHERE rl.source = 'rule'), \
           count(DISTINCT s.id) FILTER (WHERE (rl.source IS NULL OR rl.source = 'own') \
                                          AND (c.is_packaging OR c.slug = 'packaging')) \
         FROM menu_item_sizes s \
         JOIN menu_items mi ON mi.id = s.menu_item_id AND mi.deleted_at IS NULL \
         JOIN recipe_lines rl ON rl.owner_type = 'item_size' AND rl.owner_id = s.id \
         JOIN org_ingredients oi ON oi.id = rl.ingredient_id \
         JOIN ingredient_categories c ON c.id = oi.category_id \
         WHERE mi.org_id = $1",
    )
    .bind(org)
    .fetch_one(&mut *tx)
    .await?;
    let rev = bump_catalog_revision(&mut tx, org).await?;
    tx.commit().await?;
    Ok(ApplyPackagingRulesResult {
        sizes_seen: stats.sizes_seen as i64,
        sizes_changed: stats.sizes_changed as i64,
        sizes_with_rule: with_rule,
        sizes_with_manual_packaging: manual,
        catalog_revision: rev,
    })
}
