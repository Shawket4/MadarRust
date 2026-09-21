//! Dry-run preview (`POST /menu-items/{id}/preview`, MENU_MODELING_AUDIT.md §4.3, B5):
//! "what the POS charges + what gets deducted" for one configuration of an item.
//!
//! It runs the SAME code as order creation — [`catalog_unit_price`] for the line
//! price, [`resolve_menu_item_configuration`] for sizes, swaps, the extras-follow
//! pass and optionals, [`packaging_ingredient_ids`] for the dine-in strip and
//! [`ingredient_costs_at`] for cost — and writes nothing. Lint rules F4–F10 are
//! added, scoped to this item, its size and the chosen options.
//!
//! Dine-in strips ingredients whose category is `is_packaging` or has the legacy
//! slug `packaging` (the order path's rule, through the shared helper).

use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::{BTreeMap, HashSet};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    auth::{guards::require_same_org, jwt::Claims},
    errors::{AppError, AppErrorResponse},
    menu::lint::lint_org,
    orders::component_resolve::{
        AddonInput, MenuItemResolution, ResolveWarning, resolve_menu_item_configuration,
    },
    orders::handlers::{catalog_unit_price, packaging_ingredient_ids},
    permissions::checker::check_permission,
};

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct PreviewRequest {
    /// Size to price and deduct; absent = the order path's default (base price,
    /// first size's recipe).
    #[serde(default)]
    pub size_label: Option<String>,
    /// Chosen modifier options, including item-private optional-field ids.
    #[serde(default)]
    pub option_ids: Vec<Uuid>,
    #[serde(default = "one")]
    pub quantity: i32,
    /// `takeaway` (default) | `dine_in`.
    #[serde(default)]
    pub service_mode: Option<String>,
    /// Price and cost as at this branch; absent = catalogue / org-level cost.
    #[serde(default)]
    pub branch_id: Option<Uuid>,
}

fn one() -> i32 {
    1
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PreviewOptionPrice {
    pub option_id: Uuid,
    pub name: String,
    /// Piastres added to one unit (swap = difference over the default).
    pub price_delta: i32,
    /// `swap over <default>` | `adds` | `none`.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PreviewPrice {
    /// Unit price of the item/size, piastres.
    pub base: i32,
    pub options: Vec<PreviewOptionPrice>,
    /// (base + options) × quantity, piastres.
    pub total: i32,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PreviewDeduction {
    pub ingredient_id: Option<Uuid>,
    pub name: String,
    pub category_slug: String,
    pub quantity: f64,
    pub unit: String,
    /// `recipe` | `swap` | `option` | `packaging`.
    pub source: String,
    /// `swapped from X` | `follows the chosen X` | `skipped on dine-in`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Shown but not deducted (dine-in packaging).
    pub skipped: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PreviewCost {
    /// Piastres over the deducted lines with a known cost.
    pub total: i64,
    /// At least one deducted line has no cost: `total` is partial.
    pub cost_missing: bool,
    /// `(price − cost) / price` (fraction, like `/costing`); null when the cost
    /// is partial or the price is 0.
    pub margin_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PreviewResponse {
    /// The size the preview resolved (the request's, else the default size).
    pub size_label: Option<String>,
    pub quantity: i32,
    pub price: PreviewPrice,
    pub deductions: Vec<PreviewDeduction>,
    pub cost: PreviewCost,
    pub warnings: Vec<ResolveWarning>,
    /// Swap groups: group id → the option preselected by the recipe.
    #[schema(value_type = std::collections::HashMap<String, Uuid>)]
    pub defaults: BTreeMap<Uuid, Uuid>,
}

const PREVIEW_RULES: &[&str] = &["F4", "F5", "F6", "F7", "F8", "F9", "F10"];

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

#[utoipa::path(
    post,
    path = "/menu-items/{id}/preview",
    tag = "menu",
    params(("id" = Uuid, Path, description = "Menu item id")),
    request_body = PreviewRequest,
    responses(
        (status = 200, description = "Dry-run price, deductions, cost and warnings (nothing is written)", body = PreviewResponse),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn preview_menu_item(
    req: HttpRequest,
    pool: crate::db::Db,
    path: web::Path<Uuid>,
    body: web::Json<PreviewRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "read").await?;
    let item_id = path.into_inner();
    let org_id: Uuid =
        sqlx::query_scalar("SELECT org_id FROM menu_items WHERE id = $1 AND deleted_at IS NULL")
            .bind(item_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Menu item {item_id} not found")))?;
    require_same_org(&claims, Some(org_id))?;
    if let Some(b) = body.branch_id {
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM branches WHERE id = $1 AND org_id = $2)",
        )
        .bind(b)
        .bind(org_id)
        .fetch_one(pool.get_ref())
        .await?;
        if !ok {
            return Err(AppError::NotFound(format!("Branch {b} not found")));
        }
    }
    let out = preview(pool.get_ref(), org_id, item_id, &body).await?;
    Ok(HttpResponse::Ok().json(out))
}

/// The engine: pure reads, callable from tests.
pub async fn preview(
    pool: &PgPool,
    org_id: Uuid,
    item_id: Uuid,
    body: &PreviewRequest,
) -> Result<PreviewResponse, AppError> {
    if body.quantity <= 0 {
        return Err(AppError::BadRequest("Quantity must be > 0".into()));
    }
    // A nil branch matches no override row: catalogue price, org-level cost.
    let branch_id = body.branch_id.unwrap_or(Uuid::nil());
    let dine_in = body.service_mode.as_deref() == Some("dine_in");

    // Split the chosen ids: this item's optional fields vs addon options.
    let optional_ids: HashSet<Uuid> = sqlx::query_scalar(
        "SELECT id FROM menu_item_optional_fields WHERE menu_item_id = $1 AND id = ANY($2)",
    )
    .bind(item_id)
    .bind(&body.option_ids)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    let optional_field_ids: Vec<Uuid> = body
        .option_ids
        .iter()
        .copied()
        .filter(|id| optional_ids.contains(id))
        .collect();
    let addons: Vec<AddonInput> = body
        .option_ids
        .iter()
        .filter(|id| !optional_ids.contains(id))
        .map(|&addon_item_id| AddonInput {
            addon_item_id,
            quantity: 1,
            unit_price: None,
        })
        .collect();

    // Resolve the size ONCE, before pricing: with no size given, the price, the
    // deductions and the reported `size_label` must all describe the same size
    // (the item's first size as listed). Pricing with `None` used the item's
    // base price while the resolver deducted the first size's recipe.
    let size_label: Option<String> = match &body.size_label {
        Some(s) => Some(s.clone()),
        None => {
            sqlx::query_scalar(
                "SELECT label FROM menu_item_sizes WHERE menu_item_id = $1 \
             ORDER BY is_active IS NOT TRUE, sort NULLS LAST, label LIMIT 1",
            )
            .bind(item_id)
            .fetch_optional(pool)
            .await?
        }
    };
    let (_, _, base, _) =
        catalog_unit_price(pool, item_id, size_label.as_deref(), branch_id).await?;
    let MenuItemResolution {
        deductions,
        addons: resolved_addons,
        optionals,
        addon_line,
        optional_line,
        mut warnings,
    } = resolve_menu_item_configuration(
        pool,
        item_id,
        size_label.clone(),
        body.quantity,
        &addons,
        &optional_field_ids,
        branch_id,
    )
    .await?;

    // ── price ──
    let mut options: Vec<PreviewOptionPrice> = resolved_addons
        .iter()
        .map(|a| PreviewOptionPrice {
            option_id: a.addon_item_id,
            name: a.addon_name.clone(),
            price_delta: a.unit_price * a.quantity,
            reason: match (&a.swap_over, a.is_swap, a.unit_price) {
                (Some(d), true, _) => format!("swap over {d}"),
                (None, true, 0) => "none".into(),
                _ => "adds".into(),
            },
        })
        .collect();
    options.extend(optionals.iter().map(|o| PreviewOptionPrice {
        option_id: o.optional_field_id,
        name: o.field_name.clone(),
        price_delta: o.price,
        reason: if o.price == 0 { "none" } else { "adds" }.into(),
    }));
    let total = (base + addon_line + optional_line) * body.quantity;

    // ── deductions ──
    let packaging = if dine_in {
        packaging_ingredient_ids(pool, org_id).await?
    } else {
        HashSet::new()
    };
    let out_deductions: Vec<PreviewDeduction> = deductions
        .into_iter()
        .filter(|d| !d.undeducted)
        .map(|d| {
            let is_pack = d.category == "packaging";
            let skipped = d
                .org_ingredient_id
                .is_some_and(|id| packaging.contains(&id));
            let source = if d.source.starts_with("addon_swap:") {
                "swap"
            } else if d.source == "drink_recipe" {
                if is_pack { "packaging" } else { "recipe" }
            } else {
                "option"
            };
            PreviewDeduction {
                ingredient_id: d.org_ingredient_id,
                name: d.ingredient_name,
                category_slug: d.category,
                quantity: d.quantity,
                unit: d.unit,
                source: source.into(),
                note: if skipped {
                    Some("skipped on dine-in".into())
                } else {
                    d.note
                },
                skipped,
            }
        })
        .collect();

    // ── cost ──
    let ids: Vec<Uuid> = out_deductions
        .iter()
        .filter(|d| !d.skipped)
        .filter_map(|d| d.ingredient_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let costs =
        crate::costing::ingredient_costs_at(pool, branch_id, &ids, chrono::Utc::now()).await?;
    let mut cost_total = 0f64;
    let mut cost_missing = false;
    for d in out_deductions.iter().filter(|d| !d.skipped) {
        use rust_decimal::prelude::ToPrimitive;
        match d
            .ingredient_id
            .and_then(|id| costs.get(&id))
            .and_then(|c| c.to_f64())
        {
            Some(c) => cost_total += d.quantity * c,
            None => cost_missing = true,
        }
    }
    let cost_total = cost_total.round() as i64;
    let margin_pct =
        (!cost_missing && total > 0).then(|| (total as i64 - cost_total) as f64 / total as f64);

    // ── defaults, lint ──
    let defaults = swap_defaults(pool, item_id, size_label.as_deref()).await?;

    let chosen: HashSet<Uuid> = body.option_ids.iter().copied().collect();
    let deducted: HashSet<Uuid> = out_deductions
        .iter()
        .filter_map(|d| d.ingredient_id)
        .collect();
    for issue in lint_org(pool, org_id).await? {
        if !PREVIEW_RULES.contains(&issue.rule.as_str()) {
            continue;
        }
        let relevant = match issue.entity_type.as_str() {
            "item" => {
                issue.item_id == Some(item_id)
                    && (issue.size_label.is_none() || issue.size_label == size_label)
            }
            "option" => chosen.contains(&issue.entity_id),
            "ingredient" => deducted.contains(&issue.entity_id),
            _ => false,
        };
        if relevant {
            warnings.push(ResolveWarning {
                rule: issue.rule,
                message: issue.message,
            });
        }
    }

    Ok(PreviewResponse {
        size_label,
        quantity: body.quantity,
        price: PreviewPrice {
            base,
            options,
            total,
        },
        deductions: out_deductions,
        cost: PreviewCost {
            total: cost_total,
            cost_missing,
            margin_pct,
        },
        warnings,
        defaults,
    })
}

/// For each swap group attached to the item: the offered option that carries the
/// recipe's ingredient of the swapped category (the POS preselection), in display
/// order (`sort, name, id`).
async fn swap_defaults(
    pool: &PgPool,
    item_id: Uuid,
    size_label: Option<&str>,
) -> Result<BTreeMap<Uuid, Uuid>, AppError> {
    let Some(size_label) = size_label else {
        return Ok(BTreeMap::new());
    };
    let rows: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
        "WITH swap AS (
            SELECT g.id, m.included_option_ids,
                   COALESCE(c.slug, CASE g.legacy_addon_type WHEN 'milk_type' THEN 'milk'
                                                             ELSE 'coffee_bean' END) AS slug
              FROM menu_item_modifier_groups m
              JOIN modifier_groups g ON g.id = m.group_id AND g.is_active
              LEFT JOIN ingredient_categories c
                     ON c.id = g.swap_category_id AND g.effect = 'swaps'
             WHERE m.menu_item_id = $1
               AND (c.id IS NOT NULL OR g.legacy_addon_type IN ('milk_type', 'coffee_type'))),
         base AS (
            SELECT DISTINCT ON (ic.slug) ic.slug, rl.ingredient_id
              FROM recipe_lines rl
              JOIN menu_item_sizes s ON s.id = rl.owner_id AND rl.owner_type = 'item_size'
              JOIN org_ingredients oi ON oi.id = rl.ingredient_id
              JOIN ingredient_categories ic ON ic.id = oi.category_id
             WHERE s.menu_item_id = $1 AND s.label = $2
             ORDER BY ic.slug, oi.name, rl.ingredient_id)
         SELECT sw.id,
                (SELECT o.id FROM modifier_options o
                  WHERE o.group_id = sw.id AND o.is_active
                    AND (sw.included_option_ids IS NULL OR o.id = ANY(sw.included_option_ids))
                    AND (o.replaces_ingredient_id = b.ingredient_id
                         OR EXISTS (SELECT 1 FROM recipe_lines r
                                     WHERE r.owner_type = 'modifier_option' AND r.owner_id = o.id
                                       AND r.ingredient_id = b.ingredient_id))
                  ORDER BY o.sort NULLS LAST, o.name, o.id LIMIT 1)
           FROM swap sw JOIN base b ON b.slug = sw.slug",
    )
    .bind(item_id)
    .bind(size_label)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(g, o)| o.map(|o| (g, o)))
        .collect())
}

