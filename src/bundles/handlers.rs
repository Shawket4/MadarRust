use actix_web::HttpMessage;
use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::{guards::require_same_org, jwt::Claims},
    errors::{AppError, AppErrorResponse},
    permissions::checker::check_permission,
};
use utoipa::{IntoParams, ToSchema};

// ── Models & Enums ────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, sqlx::Type, ToSchema)]
#[sqlx(type_name = "bundle_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum BundleStatus {
    Draft,
    Active,
    Archived,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct Bundle {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub name_translations: serde_json::Value,
    pub description: Option<String>,
    pub description_translations: serde_json::Value,
    pub price: i32,
    pub status: BundleStatus,
    pub image_url: Option<String>,
    pub available_from_time: Option<NaiveTime>,
    pub available_until_time: Option<NaiveTime>,
    pub available_from_date: Option<NaiveDate>,
    pub available_until_date: Option<NaiveDate>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by: Option<Uuid>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct BundleComponent {
    pub id: Uuid,
    pub bundle_id: Uuid,
    pub item_id: Uuid,
    pub quantity: i32,
    pub position: i32,
}

// WIRE COMPAT (menu unification): the deployed POS parses bundles through the
// STRICT generated client, whose old model types these costs as required
// non-nullable ints — so the wire keeps them as numbers (unknown parts counted
// as 0, exactly what the retired engine emitted) and the truth about
// completeness rides in the `*_missing` flags. The flags are ALWAYS sent but
// schema-OPTIONAL, so a NEW client's regenerated model also parses an OLD
// backend that never sends them. New consumers (dashboard/Studio) must key on
// the flags, never trust a 0 with `missing = true`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BundleComponentHydrated {
    pub id: Uuid,
    pub bundle_id: Uuid,
    pub item_id: Uuid,
    pub quantity: i32,
    pub position: i32,
    pub item_name: String,
    pub item_price: i32,
    /// Cost of the component (at its base size) in piastres. When
    /// `item_cost_missing` is true this is a PARTIAL figure (unknown = 0 on the
    /// wire for old-client compat) — display it as unknown, not as money.
    pub item_cost: i64,
    /// True when the component's cost could not be fully resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_cost_missing: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BundleWithComponents {
    #[serde(flatten)]
    pub bundle: Bundle,
    pub components: Vec<BundleComponentHydrated>,
    pub branch_ids: Vec<Uuid>,
    /// Sum of the KNOWN component costs × quantity, in piastres. When
    /// `cost_missing` is true this is a partial rollup (old-wire semantics) —
    /// render it as unknown, never as 0.
    pub computed_cost: i64,
    /// True when at least one component's cost is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_missing: Option<bool>,
}

// ── Payloads & Queries ────────────────────────────────────────

#[allow(dead_code)]
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct OrgQuery {
    pub org_id: Uuid,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListBundlesQuery {
    pub org_id: Option<Uuid>,
    pub status: Option<BundleStatus>,
    pub branch_id: Option<Uuid>,
    pub search: Option<String>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
    /// Sort: name_asc | name_desc | price_asc | price_desc | created_asc |
    /// created_desc (default).
    pub sort: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, ToSchema)]
pub struct PaginatedBundles {
    pub data: Vec<BundleWithComponents>,
    pub total: i64,
    pub page: i64,
    pub per_page: i64,
    pub total_pages: i64,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateBundleComponentInput {
    pub item_id: Uuid,
    pub quantity: i32,
    pub position: Option<i32>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateBundleRequest {
    pub org_id: Uuid,
    pub name: String,
    pub name_translations: Option<serde_json::Value>,
    pub description: Option<String>,
    pub description_translations: Option<serde_json::Value>,
    pub price: i32,
    pub image_url: Option<String>,
    pub available_from_time: Option<NaiveTime>,
    pub available_until_time: Option<NaiveTime>,
    pub available_from_date: Option<NaiveDate>,
    pub available_until_date: Option<NaiveDate>,
    pub components: Vec<CreateBundleComponentInput>,
    pub branch_ids: Option<Vec<Uuid>>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpdateBundleRequest {
    pub name: Option<String>,
    pub name_translations: Option<serde_json::Value>,
    pub description: Option<String>,
    pub description_translations: Option<serde_json::Value>,
    pub price: Option<i32>,
    pub image_url: Option<String>,
    /// `null`  → clear the field (no start time restriction)
    /// omitted → keep the existing value
    /// a value → set to that time
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub available_from_time: Option<Option<NaiveTime>>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub available_until_time: Option<Option<NaiveTime>>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub available_from_date: Option<Option<NaiveDate>>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub available_until_date: Option<Option<NaiveDate>>,
    pub components: Option<Vec<CreateBundleComponentInput>>,
    pub branch_ids: Option<Vec<Uuid>>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AvailableBundlesQuery {
    pub branch_id: Uuid,
    pub at: Option<DateTime<Utc>>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PerformanceQuery {
    pub start_date: Option<DateTime<Utc>>,
    pub end_date: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct ComponentPopularity {
    pub item_id: Uuid,
    pub item_name: String,
    pub quantity_sold: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, ToSchema)]
pub struct BundlePerformanceResponse {
    pub sales_volume: i64,
    pub gross_revenue: i64,
    pub net_profit: i64,
    pub component_popularity: Vec<ComponentPopularity>,
}

// ── Serde Helpers ─────────────────────────────────────────────

/// Deserializer that maps:
///   - absent field  → `None`          (caller did not touch the field)
///   - explicit null → `Some(None)`    (caller wants to clear the field)
///   - a real value  → `Some(Some(v))` (caller wants to set a new value)
///
/// Apply with `#[serde(default, deserialize_with = "deserialize_optional_field")]`
/// on `Option<Option<T>>` fields.
fn deserialize_optional_field<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<T>::deserialize(de)?))
}

// ── Helper Functions ──────────────────────────────────────────

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

/// RETIRED from production — superseded by [`component_cost`], which delegates to the
/// canonical `costing::service`. Kept ONLY so the `bundle-margin-flip` diff bin can
/// compute the legacy cost side (arbitrary single size, `f64`, org-only, recipe-less =
/// free). Remove at SHIM_TEARDOWN. Do NOT call from handlers.
///
/// Roll up a menu item's ingredient cost in piastres. Returns:
///   * `Some(0)`   — the item has no recipe lines (no tracked ingredient cost),
///   * `Some(sum)` — every recipe line is linked to a costed ingredient,
///   * `None`      — UNKNOWN: at least one recipe line is unlinked or uncosted.
pub async fn compute_item_cost(pool: &PgPool, item_id: Uuid) -> Result<Option<i32>, sqlx::Error> {
    let cost: Option<f64> = sqlx::query_scalar(
        r#"
        SELECT CASE
            WHEN COUNT(*) = 0 THEN 0
            WHEN bool_or(r.org_ingredient_id IS NULL OR i.cost_per_unit IS NULL) THEN NULL
            ELSE SUM(r.quantity_used::float8 * i.cost_per_unit::float8)
        END
        FROM menu_item_recipes r
        LEFT JOIN org_ingredients i ON i.id = r.org_ingredient_id
        WHERE r.menu_item_id = $1
          AND r.size_label = COALESCE(
              (SELECT size_label FROM menu_item_recipes WHERE menu_item_id = $1 ORDER BY size_label LIMIT 1),
              'one_size'
          )
        "#
    )
    .bind(item_id)
    .fetch_optional(pool)
    .await?
    .flatten();

    Ok(cost.map(|c| c.round() as i32))
}

/// Cost (piastres) of a menu item at its base size, via the canonical cost engine
/// (`costing::service`) — the production replacement for [`compute_item_cost`].
///
///   * `Some(c)` — the base size's recipe is complete (every line linked & costed),
///   * `None`    — UNKNOWN: the item has no priced recipe, or the rollup is incomplete.
///                 Callers MUST treat `None` as unknown, never 0.
///
/// Base size = `one_size` for size-less items, else the lexicographically-first size
/// (`bundle_components` carry no size). Unlike the retired engine this costs the recipe
/// matched to that size, uses `Decimal`, is branch-aware, and never treats a recipe-less
/// item as free — so an unknown-cost component now correctly blocks the margin floor.
pub async fn component_cost(
    pool: &PgPool,
    org_id: Uuid,
    item_id: Uuid,
    branch_id: Option<Uuid>,
) -> Result<Option<i64>, AppError> {
    let skus = crate::costing::sku_costs_for_items(pool, org_id, &[item_id], branch_id).await?;
    // Pick the base size: `one_size` first, then the lexicographically-first label.
    let base = skus.iter().min_by(|a, b| {
        (a.size_label != "one_size", &a.size_label)
            .cmp(&(b.size_label != "one_size", &b.size_label))
    });
    Ok(match base {
        // A complete rollup: `cost` may still be None (no recipe) → unknown.
        Some(s) if !s.cost_missing => s.cost,
        // No SKU row, or an incomplete rollup → unknown.
        _ => None,
    })
}

pub async fn fetch_bundle_full(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<BundleWithComponents>, AppError> {
    let bundle = sqlx::query_as::<_, Bundle>(
        "SELECT id, org_id, name, name_translations, description, description_translations, \
                price, status, image_url, available_from_time, available_until_time, \
                available_from_date, available_until_date, created_at, updated_at, created_by \
         FROM bundles WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    let Some(bundle) = bundle else {
        return Ok(None);
    };

    let component_rows: Vec<(Uuid, Uuid, Uuid, i32, i32, String, i32)> = sqlx::query_as(
        r#"
        SELECT bc.id, bc.bundle_id, bc.item_id, bc.quantity, bc.position,
               mi.name as item_name, mi.base_price as item_price
        FROM bundle_components bc
        JOIN menu_items mi ON mi.id = bc.item_id
        WHERE bc.bundle_id = $1
        ORDER BY bc.position ASC, bc.id ASC
        "#,
    )
    .bind(id)
    .fetch_all(pool)
    .await?;

    let mut components = Vec::new();
    let mut computed_cost: i64 = 0;
    let mut cost_missing = false;

    for row in component_rows {
        // Canonical cost at the component's base size. An unknown cost counts as
        // 0 on the WIRE (old-client parse compat) but flips the missing flags so
        // new consumers never mistake the partial figure for real money.
        let item_cost = component_cost(pool, bundle.org_id, row.2, None).await?;
        if let Some(c) = item_cost {
            computed_cost += c * row.3 as i64;
        } else {
            cost_missing = true;
        }

        components.push(BundleComponentHydrated {
            id: row.0,
            bundle_id: row.1,
            item_id: row.2,
            quantity: row.3,
            position: row.4,
            item_name: row.5,
            item_price: row.6,
            item_cost: item_cost.unwrap_or(0),
            item_cost_missing: Some(item_cost.is_none()),
        });
    }

    let branch_rows: Vec<(Uuid,)> =
        sqlx::query_as("SELECT branch_id FROM bundle_branch_availability WHERE bundle_id = $1")
            .bind(id)
            .fetch_all(pool)
            .await?;

    let branch_ids = branch_rows.into_iter().map(|r| r.0).collect();

    Ok(Some(BundleWithComponents {
        bundle,
        components,
        branch_ids,
        computed_cost,
        cost_missing: Some(cost_missing),
    }))
}

async fn validate_bundle_rules(
    pool: &PgPool,
    org_id: Uuid,
    price: i32,
    components: &[CreateBundleComponentInput],
) -> Result<(), AppError> {
    // 1. Components count: [2, 6]
    if components.len() < 2 || components.len() > 6 {
        return Err(AppError::BadRequest(
            "A bundle must contain between 2 and 6 components".into(),
        ));
    }

    let item_ids: Vec<Uuid> = components.iter().map(|c| c.item_id).collect();

    // 2. No duplicates
    let mut unique_item_ids = std::collections::HashSet::new();
    for id in &item_ids {
        if !unique_item_ids.insert(*id) {
            return Err(AppError::BadRequest(
                "Duplicate components are not allowed".into(),
            ));
        }
    }

    // 3. Components are active and belong to same org
    let active_items: Vec<(Uuid, Uuid, i32, bool)> = sqlx::query_as::<_, (Uuid, Uuid, i32, bool)>(
        "SELECT id, org_id, base_price, is_active FROM menu_items WHERE id = ANY($1) AND deleted_at IS NULL"
    )
    .bind(&item_ids)
    .fetch_all(pool)
    .await?;

    if active_items.len() != item_ids.len() {
        return Err(AppError::BadRequest(
            "One or more components do not exist or have been deleted".into(),
        ));
    }

    let mut sum_costs: i64 = 0;
    let mut sum_list_prices = 0;

    for c in components {
        let item_info = active_items.iter().find(|i| i.0 == c.item_id).unwrap();
        if item_info.1 != org_id {
            return Err(AppError::BadRequest(
                "All components must belong to the same organization".into(),
            ));
        }
        if !item_info.3 {
            return Err(AppError::BadRequest(format!(
                "Component {} is inactive",
                item_info.0
            )));
        }

        sum_list_prices += item_info.2 * c.quantity;

        // Block activation when a component's cost is UNKNOWN — otherwise an
        // undercounted/zeroed cost lets an under-priced bundle pass the margin
        // floor (V24). Drafts can still be created; link all recipe ingredients
        // before activating. Via the canonical engine a recipe-less component is
        // now UNKNOWN (not free), so it too blocks activation.
        let Some(item_cost) = component_cost(pool, org_id, c.item_id, None).await? else {
            return Err(AppError::BadRequest(format!(
                "Component {} has an unknown ingredient cost — link all recipe ingredients before activating the bundle",
                item_info.0
            )));
        };
        sum_costs += item_cost * c.quantity as i64;
    }

    // 4. Margin floor: Bundle Price >= 1.20 * Sum Costs
    if (price as f64) < (sum_costs as f64 * 1.20) {
        return Err(AppError::BadRequest(format!(
            "Bundle price must be at least 20% above components cost (Min: {}, Given: {})",
            (sum_costs as f64 * 1.20).round() as i32,
            price
        )));
    }

    // 5. Discount perceivability: Bundle Price <= 0.97 * Sum List Prices
    if (price as f64) > (sum_list_prices as f64 * 0.97) {
        return Err(AppError::BadRequest(format!(
            "Bundle price must be at least 3% below sum of components list prices (Max: {}, Given: {})",
            (sum_list_prices as f64 * 0.97).round() as i32,
            price
        )));
    }

    Ok(())
}

// ── CRUD Handlers ─────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/bundles",
    tag = "bundles",
    params(ListBundlesQuery),
    responses((status = 200, description = "List bundles", body = PaginatedBundles), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_bundles(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<ListBundlesQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;

    let org_id = query
        .org_id
        .or_else(|| claims.org_id())
        .ok_or_else(|| AppError::BadRequest("org_id is required".into()))?;
    require_same_org(&claims, Some(org_id))?;

    // Clamp so a negative/zero page or out-of-range per_page can't produce a
    // negative SQL OFFSET (which Postgres rejects → 500). Matches the bounds used
    // by the inventory/menu list handlers.
    let page = query.page.unwrap_or(1).max(1);
    let per_page = query.per_page.unwrap_or(20).clamp(1, 500);
    let offset = (page - 1) * per_page;

    let search_pattern = query
        .search
        .as_ref()
        .map(|s| format!("%{}%", s.to_lowercase()));

    // Total Count
    let total: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM bundles b
        WHERE b.org_id = $1
          AND ($2::public.bundle_status IS NULL OR b.status = $2)
          AND ($3::text IS NULL OR LOWER(b.name) LIKE $3)
          AND ($4::uuid IS NULL OR (
              NOT EXISTS (SELECT 1 FROM bundle_branch_availability WHERE bundle_id = b.id) OR
              EXISTS (SELECT 1 FROM bundle_branch_availability WHERE bundle_id = b.id AND branch_id = $4)
          ))
        "#
    )
    .bind(org_id)
    .bind(query.status)
    .bind(&search_pattern)
    .bind(query.branch_id)
    .fetch_one(pool.get_ref())
    .await?;

    // Page items IDs
    let order_by = match query.sort.as_deref() {
        Some("name_asc") => "LOWER(b.name) ASC",
        Some("name_desc") => "LOWER(b.name) DESC",
        Some("price_asc") => "b.price ASC",
        Some("price_desc") => "b.price DESC",
        Some("created_asc") => "b.created_at ASC",
        _ => "b.created_at DESC",
    };
    let ids: Vec<Uuid> = sqlx::query_scalar(&format!(
        r#"
        SELECT b.id
        FROM bundles b
        WHERE b.org_id = $1
          AND ($2::public.bundle_status IS NULL OR b.status = $2)
          AND ($3::text IS NULL OR LOWER(b.name) LIKE $3)
          AND ($4::uuid IS NULL OR (
              NOT EXISTS (SELECT 1 FROM bundle_branch_availability WHERE bundle_id = b.id) OR
              EXISTS (SELECT 1 FROM bundle_branch_availability WHERE bundle_id = b.id AND branch_id = $4)
          ))
        ORDER BY {order_by}
        LIMIT $5 OFFSET $6
        "#
    ))
    .bind(org_id)
    .bind(query.status)
    .bind(&search_pattern)
    .bind(query.branch_id)
    .bind(per_page)
    .bind(offset)
    .fetch_all(pool.get_ref())
    .await?;

    let mut hydrated = Vec::new();
    for id in ids {
        if let Some(full) = fetch_bundle_full(pool.get_ref(), id).await? {
            hydrated.push(full);
        }
    }

    let total_pages = (total as f64 / per_page as f64).ceil() as i64;

    Ok(HttpResponse::Ok().json(PaginatedBundles {
        data: hydrated,
        total,
        page,
        per_page,
        total_pages,
    }))
}

#[utoipa::path(
    post,
    path = "/bundles",
    tag = "bundles",
    request_body = CreateBundleRequest,
    responses((status = 201, description = "Bundle created", body = BundleWithComponents), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_bundle(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateBundleRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    require_same_org(&claims, Some(body.org_id))?;

    if body.components.is_empty() {
        return Err(AppError::BadRequest(
            "A bundle must contain components".into(),
        ));
    }

    let mut tx = pool.begin().await?;

    let mut_body = body.into_inner();
    let mut name_translations = mut_body
        .name_translations
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));
    crate::translation::ensure_translations_json(&mut name_translations, Some(&mut_body.name))
        .await
        .map_err(|_| AppError::Internal)?;

    let mut description_translations = mut_body
        .description_translations
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(desc) = &mut_body.description {
        crate::translation::ensure_translations_json(&mut description_translations, Some(desc))
            .await
            .map_err(|_| AppError::Internal)?;
    }

    // Create bundle row (always defaults to Draft)
    let bundle = sqlx::query_as::<_, Bundle>(
        r#"
        INSERT INTO bundles (
            org_id, name, name_translations, description, description_translations,
            price, status, image_url, available_from_time,
            available_until_time, available_from_date, available_until_date, created_by
        )
        VALUES ($1, $2, $3, $4, $5, $6, 'draft'::public.bundle_status, $7, $8, $9, $10, $11, $12)
        RETURNING *
        "#,
    )
    .bind(mut_body.org_id)
    .bind(&mut_body.name)
    .bind(name_translations)
    .bind(&mut_body.description)
    .bind(description_translations)
    .bind(mut_body.price)
    .bind(&mut_body.image_url)
    .bind(mut_body.available_from_time)
    .bind(mut_body.available_until_time)
    .bind(mut_body.available_from_date)
    .bind(mut_body.available_until_date)
    .bind(claims.user_id())
    .fetch_one(&mut *tx)
    .await?;

    // Insert components
    for (i, c) in mut_body.components.iter().enumerate() {
        sqlx::query(
            r#"
            INSERT INTO bundle_components (bundle_id, item_id, quantity, position)
            VALUES ($1, $2, $3, $4)
            "#,
        )
        .bind(bundle.id)
        .bind(c.item_id)
        .bind(c.quantity)
        .bind(c.position.unwrap_or(i as i32))
        .execute(&mut *tx)
        .await?;
    }

    // Insert branch availability
    if let Some(branch_ids) = &mut_body.branch_ids {
        for b_id in branch_ids {
            sqlx::query(
                "INSERT INTO bundle_branch_availability (bundle_id, branch_id) VALUES ($1, $2)",
            )
            .bind(bundle.id)
            .bind(b_id)
            .execute(&mut *tx)
            .await?;
        }
    }

    // Seed initial price epoch for the advisor.
    sqlx::query(
        "INSERT INTO bundle_price_epochs \
             (bundle_id, price, effective_from, changed_by) \
         VALUES ($1, $2, now(), $3)",
    )
    .bind(bundle.id)
    .bind(mut_body.price)
    .bind(claims.user_id())
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    let full = fetch_bundle_full(pool.get_ref(), bundle.id)
        .await?
        .ok_or_else(|| AppError::Internal)?;

    Ok(HttpResponse::Created().json(full))
}

#[utoipa::path(
    get,
    path = "/bundles/{id}",
    tag = "bundles",
    params(("id" = Uuid, Path, description = "Bundle ID")),
    responses((status = 200, description = "Get bundle", body = BundleWithComponents), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_bundle(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;

    let full = fetch_bundle_full(pool.get_ref(), *id).await?;
    let Some(full) = full else {
        return Err(AppError::NotFound("Bundle not found".into()));
    };

    require_same_org(&claims, Some(full.bundle.org_id))?;

    Ok(HttpResponse::Ok().json(full))
}

#[utoipa::path(
    patch,
    path = "/bundles/{id}",
    tag = "bundles",
    params(("id" = Uuid, Path, description = "Bundle ID")),
    request_body = UpdateBundleRequest,
    responses((status = 200, description = "Bundle updated", body = BundleWithComponents), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_bundle(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<UpdateBundleRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let original = fetch_bundle_full(pool.get_ref(), *id).await?;
    let Some(original) = original else {
        return Err(AppError::NotFound("Bundle not found".into()));
    };

    require_same_org(&claims, Some(original.bundle.org_id))?;

    if original.bundle.status == BundleStatus::Archived {
        return Err(AppError::BadRequest(
            "Archived bundles cannot be modified".into(),
        ));
    }

    let mut_body = body.into_inner();
    let mut tx = pool.begin().await?;

    let mut name_translations = original.bundle.name_translations.clone();
    if let Some(new_name) = &mut_body.name {
        crate::translation::ensure_translations_json(&mut name_translations, Some(new_name))
            .await
            .map_err(|_| AppError::Internal)?;
    } else if let Some(new_tr) = mut_body.name_translations {
        name_translations = new_tr;
        crate::translation::ensure_translations_json(
            &mut name_translations,
            Some(&original.bundle.name),
        )
        .await
        .map_err(|_| AppError::Internal)?;
    }

    let mut description_translations = original.bundle.description_translations.clone();
    if let Some(new_desc) = &mut_body.description {
        crate::translation::ensure_translations_json(&mut description_translations, Some(new_desc))
            .await
            .map_err(|_| AppError::Internal)?;
    } else if let Some(new_tr) = mut_body.description_translations {
        description_translations = new_tr;
        if let Some(desc) = &original.bundle.description {
            crate::translation::ensure_translations_json(&mut description_translations, Some(desc))
                .await
                .map_err(|_| AppError::Internal)?;
        }
    }

    // Prepare updated values
    let name = mut_body.name.as_ref().unwrap_or(&original.bundle.name);
    let description = mut_body
        .description
        .as_ref()
        .or(original.bundle.description.as_ref());
    let price = mut_body.price.unwrap_or(original.bundle.price);
    let image_url = mut_body
        .image_url
        .as_ref()
        .or(original.bundle.image_url.as_ref());
    // Option<Option<T>> semantics:
    //   None        → field was absent from the request → keep existing value
    //   Some(None)  → field was explicitly `null`       → clear (no restriction)
    //   Some(Some(v)) → field was set to a new value    → use new value
    let available_from_time = match mut_body.available_from_time {
        Some(v) => v,                                // Some(None) clears, Some(Some(t)) sets
        None => original.bundle.available_from_time, // omitted → keep
    };
    let available_until_time = match mut_body.available_until_time {
        Some(v) => v,
        None => original.bundle.available_until_time,
    };
    let available_from_date = match mut_body.available_from_date {
        Some(v) => v,
        None => original.bundle.available_from_date,
    };
    let available_until_date = match mut_body.available_until_date {
        Some(v) => v,
        None => original.bundle.available_until_date,
    };

    // If components are being updated
    let mut updated_components = Vec::new();
    if let Some(comp_inputs) = &mut_body.components {
        // Delete old components
        sqlx::query("DELETE FROM bundle_components WHERE bundle_id = $1")
            .bind(original.bundle.id)
            .execute(&mut *tx)
            .await?;

        for (i, c) in comp_inputs.iter().enumerate() {
            sqlx::query(
                r#"
                INSERT INTO bundle_components (bundle_id, item_id, quantity, position)
                VALUES ($1, $2, $3, $4)
                "#,
            )
            .bind(original.bundle.id)
            .bind(c.item_id)
            .bind(c.quantity)
            .bind(c.position.unwrap_or(i as i32))
            .execute(&mut *tx)
            .await?;

            updated_components.push(CreateBundleComponentInput {
                item_id: c.item_id,
                quantity: c.quantity,
                position: c.position,
            });
        }
    } else {
        for c in &original.components {
            updated_components.push(CreateBundleComponentInput {
                item_id: c.item_id,
                quantity: c.quantity,
                position: Some(c.position),
            });
        }
    }

    // If branch scopes are being updated
    if let Some(branch_ids) = &mut_body.branch_ids {
        sqlx::query("DELETE FROM bundle_branch_availability WHERE bundle_id = $1")
            .bind(original.bundle.id)
            .execute(&mut *tx)
            .await?;

        for b_id in branch_ids {
            sqlx::query(
                "INSERT INTO bundle_branch_availability (bundle_id, branch_id) VALUES ($1, $2)",
            )
            .bind(original.bundle.id)
            .bind(b_id)
            .execute(&mut *tx)
            .await?;
        }
    }

    // If bundle is Active, we must run the validation checks on the modified state!
    if original.bundle.status == BundleStatus::Active {
        validate_bundle_rules(
            pool.get_ref(),
            original.bundle.org_id,
            price,
            &updated_components,
        )
        .await?;
    }

    // Update bundles row
    sqlx::query(
        r#"
        UPDATE bundles
        SET name = $1, name_translations = $2, description = $3, description_translations = $4,
            price = $5, image_url = $6, available_from_time = $7,
            available_until_time = $8, available_from_date = $9, available_until_date = $10,
            updated_at = NOW()
        WHERE id = $11
        "#,
    )
    .bind(name)
    .bind(&name_translations)
    .bind(description)
    .bind(&description_translations)
    .bind(price)
    .bind(image_url)
    .bind(available_from_time)
    .bind(available_until_time)
    .bind(available_from_date)
    .bind(available_until_date)
    .bind(original.bundle.id)
    .execute(&mut *tx)
    .await?;

    // Maintain bundle price epoch whenever price actually changed.
    if price != original.bundle.price {
        sqlx::query(
            "UPDATE bundle_price_epochs \
             SET effective_until = now() \
             WHERE bundle_id = $1 AND effective_until IS NULL",
        )
        .bind(original.bundle.id)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO bundle_price_epochs \
                 (bundle_id, price, effective_from, changed_by) \
             VALUES ($1, $2, now(), $3)",
        )
        .bind(original.bundle.id)
        .bind(price)
        .bind(claims.user_id())
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    let full = fetch_bundle_full(pool.get_ref(), original.bundle.id)
        .await?
        .unwrap();

    Ok(HttpResponse::Ok().json(full))
}

#[utoipa::path(
    delete,
    path = "/bundles/{id}",
    tag = "bundles",
    params(("id" = Uuid, Path, description = "Bundle ID")),
    responses((status = 200, description = "Bundle deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_bundle(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "delete").await?;

    let bundle_org: Option<Uuid> = sqlx::query_scalar("SELECT org_id FROM bundles WHERE id = $1")
        .bind(*id)
        .fetch_optional(pool.get_ref())
        .await?
        .flatten();

    let Some(org_id) = bundle_org else {
        return Err(AppError::NotFound("Bundle not found".into()));
    };

    require_same_org(&claims, Some(org_id))?;

    // Check if it has historical sales
    let sales_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM order_items WHERE bundle_id = $1")
            .bind(*id)
            .fetch_one(pool.get_ref())
            .await?;

    if sales_count > 0 {
        return Err(AppError::Conflict(
            "Cannot delete a bundle with historical sales. Please archive it instead.".into(),
        ));
    }

    // Hard delete
    sqlx::query("DELETE FROM bundle_price_epochs WHERE bundle_id = $1")
        .bind(*id)
        .execute(pool.get_ref())
        .await?;

    sqlx::query("DELETE FROM bundles WHERE id = $1")
        .bind(*id)
        .execute(pool.get_ref())
        .await?;

    Ok(HttpResponse::Ok().json(serde_json::json!({ "message": "Bundle deleted successfully" })))
}

// ── Lifecycle Handlers ────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/bundles/{id}/activate",
    tag = "bundles",
    params(("id" = Uuid, Path, description = "Bundle ID")),
    responses((status = 200, description = "Bundle activated", body = BundleWithComponents), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn activate_bundle(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let full = fetch_bundle_full(pool.get_ref(), *id).await?;
    let Some(full) = full else {
        return Err(AppError::NotFound("Bundle not found".into()));
    };

    require_same_org(&claims, Some(full.bundle.org_id))?;

    if full.bundle.status == BundleStatus::Active {
        return Err(AppError::BadRequest("Bundle is already active".into()));
    }
    if full.bundle.status == BundleStatus::Archived {
        return Err(AppError::BadRequest(
            "Archived bundles cannot be reactivated".into(),
        ));
    }

    // Translate hydrated components to validation format
    let components: Vec<CreateBundleComponentInput> = full
        .components
        .iter()
        .map(|c| CreateBundleComponentInput {
            item_id: c.item_id,
            quantity: c.quantity,
            position: Some(c.position),
        })
        .collect();

    // Perform strict validations on activation
    validate_bundle_rules(
        pool.get_ref(),
        full.bundle.org_id,
        full.bundle.price,
        &components,
    )
    .await?;

    // Move to Active
    sqlx::query(
        "UPDATE bundles SET status = 'active'::public.bundle_status, updated_at = NOW() WHERE id = $1"
    )
    .bind(full.bundle.id)
    .execute(pool.get_ref())
    .await?;

    let activated = fetch_bundle_full(pool.get_ref(), full.bundle.id)
        .await?
        .unwrap();

    Ok(HttpResponse::Ok().json(activated))
}

#[utoipa::path(
    post,
    path = "/bundles/{id}/archive",
    tag = "bundles",
    params(("id" = Uuid, Path, description = "Bundle ID")),
    responses((status = 200, description = "Bundle archived", body = BundleWithComponents), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn archive_bundle(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;

    let bundle: (Uuid, String) =
        sqlx::query_as("SELECT org_id, status::text FROM bundles WHERE id = $1")
            .bind(*id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound("Bundle not found".into()))?;

    require_same_org(&claims, Some(bundle.0))?;

    if bundle.1 == "archived" {
        return Err(AppError::BadRequest("Bundle is already archived".into()));
    }

    sqlx::query(
        "UPDATE bundles SET status = 'archived'::public.bundle_status, updated_at = NOW() WHERE id = $1"
    )
    .bind(*id)
    .execute(pool.get_ref())
    .await?;

    let archived = fetch_bundle_full(pool.get_ref(), *id).await?.unwrap();

    Ok(HttpResponse::Ok().json(archived))
}

// ── POS Available Catalog Handler ──────────────────────────────

#[utoipa::path(
    get,
    path = "/bundles/available",
    tag = "bundles",
    params(AvailableBundlesQuery),
    responses((status = 200, description = "List available bundles", body = Vec<BundleWithComponents>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn available_bundles(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<AvailableBundlesQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;

    // Effective branch timezone (branch override → org default → Africa/Cairo).
    let branch: (Uuid, String) = sqlx::query_as(
        "SELECT b.org_id, COALESCE(b.timezone, o.timezone)::text
         FROM branches b JOIN organizations o ON o.id = b.org_id
         WHERE b.id = $1 AND b.deleted_at IS NULL",
    )
    .bind(query.branch_id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    require_same_org(&claims, Some(branch.0))?;

    let at_time = query.at.unwrap_or_else(Utc::now);

    let ids: Vec<Uuid> = sqlx::query_scalar(
        r#"
        SELECT b.id
        FROM bundles b
        WHERE b.org_id = $1
          AND b.status = 'active'::public.bundle_status
          -- Branch availability
          AND (
              NOT EXISTS (SELECT 1 FROM bundle_branch_availability WHERE bundle_id = b.id) OR
              EXISTS (SELECT 1 FROM bundle_branch_availability WHERE bundle_id = b.id AND branch_id = $2)
          )
          -- Date range check (relative to branch local date)
          AND (
              b.available_from_date IS NULL OR
              ($3::timestamptz AT TIME ZONE $4)::date >= b.available_from_date
          )
          AND (
              b.available_until_date IS NULL OR
              ($3::timestamptz AT TIME ZONE $4)::date <= b.available_until_date
          )
          -- Time window check (relative to branch local time)
          AND (
              b.available_from_time IS NULL OR
              ($3::timestamptz AT TIME ZONE $4)::time >= b.available_from_time
          )
          AND (
              b.available_until_time IS NULL OR
              ($3::timestamptz AT TIME ZONE $4)::time <= b.available_until_time
          )
        ORDER BY b.created_at DESC
        "#
    )
    .bind(branch.0)
    .bind(query.branch_id)
    .bind(at_time)
    .bind(&branch.1)
    .fetch_all(pool.get_ref())
    .await?;

    let mut list = Vec::new();
    for id in ids {
        if let Some(full) = fetch_bundle_full(pool.get_ref(), id).await? {
            list.push(full);
        }
    }

    Ok(HttpResponse::Ok().json(list))
}

// ── Performance Handler ────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/bundles/{id}/performance",
    tag = "bundles",
    params(("id" = Uuid, Path, description = "Bundle ID")),
    params(PerformanceQuery),
    responses((status = 200, description = "Bundle performance", body = BundlePerformanceResponse), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn bundle_performance(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    query: web::Query<PerformanceQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;

    let id = id.into_inner();

    let bundle_org: Option<Uuid> = sqlx::query_scalar("SELECT org_id FROM bundles WHERE id = $1")
        .bind(id)
        .fetch_optional(pool.get_ref())
        .await?;

    let Some(org_id) = bundle_org else {
        return Err(AppError::NotFound("Bundle not found".into()));
    };
    require_same_org(&claims, Some(org_id))?;

    // Sales volume and gross revenue
    let sales_stats: (i64, i64) = sqlx::query_as(
        r#"
        SELECT COALESCE(SUM(oi.quantity), 0)::int8, COALESCE(SUM(oi.line_total), 0)::int8
        FROM order_items oi
        JOIN orders o ON o.id = oi.order_id
        WHERE oi.bundle_id = $1
          AND o.status != 'voided'
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        "#,
    )
    .bind(id)
    .bind(query.start_date)
    .bind(query.end_date)
    .fetch_one(pool.get_ref())
    .await?;

    let sales_volume = sales_stats.0;
    let gross_revenue = sales_stats.1;

    // Component Popularity
    let component_popularity = sqlx::query_as::<_, ComponentPopularity>(
        r#"
        SELECT olbc.item_id, mi.name as item_name, SUM(olbc.quantity * oi.quantity)::int8 as quantity_sold
        FROM order_line_bundle_components olbc
        JOIN order_items oi ON oi.id = olbc.order_line_id
        JOIN orders o ON o.id = oi.order_id
        JOIN menu_items mi ON mi.id = olbc.item_id
        WHERE oi.bundle_id = $1
          AND o.status != 'voided'
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY olbc.item_id, mi.name
        ORDER BY quantity_sold DESC
        "#
    )
    .bind(id)
    .bind(query.start_date)
    .bind(query.end_date)
    .fetch_all(pool.get_ref())
    .await?;

    // Net profit from the SALE-TIME COGS snapshot (order_items.line_cost), NOT
    // current ingredient costs, and only over lines whose cost was known at sale
    // time — lines flagged cost_missing are excluded entirely rather than counted
    // as zero-cost (which would silently inflate profit) (V25).
    let net_profit: i64 = sqlx::query_scalar(
        r#"
        SELECT COALESCE(SUM(oi.line_total - COALESCE(oi.line_cost, 0)), 0)::int8
        FROM order_items oi
        JOIN orders o ON o.id = oi.order_id
        WHERE oi.bundle_id = $1
          AND o.status != 'voided'
          AND oi.cost_missing = false
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        "#,
    )
    .bind(id)
    .bind(query.start_date)
    .bind(query.end_date)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(BundlePerformanceResponse {
        sales_volume,
        gross_revenue,
        net_profit,
        component_popularity,
    }))
}
