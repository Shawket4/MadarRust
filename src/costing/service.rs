//! Cost resolution service — the single source of truth for cost math.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;

/// Round a fractional-piastre Decimal (e.g. a per-gram rollup) to integer
/// piastres, half away from zero (Decimal::round defaults to banker's
/// rounding, which would send 0.5 piastres to 0).
///
/// `org_ingredients.cost_per_unit` / `ingredient_cost_history.cost_per_unit`
/// are stored in PIASTRES (the dashboard converts EGP input on entry);
/// there is deliberately no ×100 anywhere in the backend.
pub fn round_piastres(piastres: Decimal) -> i64 {
    piastres
        .round_dp_with_strategy(0, rust_decimal::RoundingStrategy::MidpointAwayFromZero)
        .to_i64()
        .unwrap_or(0)
}

/// Resolve point-in-time PIASTRE cost per unit for a set of ingredients at `at`.
///
/// Resolution order per ingredient:
///   1. `ingredient_cost_history` epoch covering `at`
///   2. fallback: `org_ingredients.cost_per_unit` (legacy rows that predate
///      history maintenance — the migration seeds baselines, but tests and
///      direct inserts may bypass the handler that writes history)
///
/// Ingredients absent from the result are unknown → cost-missing.
pub async fn ingredient_costs_at(
    pool: &PgPool,
    ingredient_ids: &[Uuid],
    at: DateTime<Utc>,
) -> Result<HashMap<Uuid, Decimal>, AppError> {
    if ingredient_ids.is_empty() {
        return Ok(HashMap::new());
    }
    // Rows whose resolved cost is NULL (never entered) are filtered out:
    // "absent from the result" IS the unknown signal callers rely on.
    let rows: Vec<(Uuid, Decimal)> = sqlx::query_as(
        r#"
        SELECT id, cost_per_unit FROM (
            SELECT oi.id,
                   COALESCE(
                       (SELECT h.cost_per_unit
                        FROM ingredient_cost_history h
                        WHERE h.org_ingredient_id = oi.id
                          AND h.effective_from <= $2
                          AND (h.effective_until IS NULL OR h.effective_until > $2)
                        ORDER BY h.effective_from DESC
                        LIMIT 1),
                       oi.cost_per_unit
                   ) AS cost_per_unit
            FROM org_ingredients oi
            WHERE oi.id = ANY($1)
        ) resolved
        WHERE cost_per_unit IS NOT NULL
        "#,
    )
    .bind(ingredient_ids)
    .bind(at)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().collect())
}

/// Computed cost for one sellable SKU (menu item × size).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SkuCost {
    pub menu_item_id: Uuid,
    /// `"one_size"` when the item has no sizes.
    pub size_label: String,
    pub item_name: String,
    pub category_id: Option<Uuid>,
    /// Current price in piastres for this SKU.
    pub price: i64,
    /// Recipe cost rollup in piastres. `null` ⟺ unknown (no recipe, or any
    /// ingredient unlinked / missing a cost).
    pub cost: Option<i64>,
    pub cost_missing: bool,
    /// `(price - cost) / price` when both known and price > 0.
    pub margin_pct: Option<f64>,
    /// `cost / price` when both known and price > 0.
    pub food_cost_pct: Option<f64>,
}

/// Current recipe-cost rollup for every active SKU in an org, in piastres.
///
/// The rollup is NULL-propagating: any unlinked ingredient or missing cost
/// makes the whole SKU cost unknown — `COALESCE(..., 0)` is never used.
pub async fn org_sku_costs(pool: &PgPool, org_id: Uuid) -> Result<Vec<SkuCost>, AppError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        menu_item_id: Uuid,
        size_label: String,
        item_name: String,
        category_id: Option<Uuid>,
        price: i64,
        cost_piastres: Option<Decimal>,
        has_recipe: bool,
    }

    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        r#"
        WITH expanded AS (
            SELECT
                mi.id   AS menu_item_id,
                mi.name AS item_name,
                mi.category_id,
                COALESCE(sz.label::text, 'one_size')          AS size_label,
                COALESCE(sz.price_override, mi.base_price)::bigint AS price
            FROM menu_items mi
            LEFT JOIN item_sizes sz
                   ON sz.menu_item_id = mi.id AND sz.is_active = TRUE
            WHERE mi.org_id = $1
              AND mi.deleted_at IS NULL
              AND mi.is_active = TRUE
        )
        SELECT
            e.menu_item_id,
            e.size_label,
            e.item_name,
            e.category_id,
            e.price,
            r.cost_piastres,
            r.has_recipe
        FROM expanded e
        CROSS JOIN LATERAL (
            SELECT
                CASE
                    WHEN COUNT(*) = 0 THEN NULL
                    WHEN bool_or(r.org_ingredient_id IS NULL
                                 OR COALESCE(ich.cost_per_unit, oi.cost_per_unit) IS NULL)
                        THEN NULL
                    ELSE SUM(r.quantity_used * COALESCE(ich.cost_per_unit, oi.cost_per_unit))
                END        AS cost_piastres,
                COUNT(*) > 0 AS has_recipe
            FROM menu_item_recipes r
            LEFT JOIN org_ingredients oi ON oi.id = r.org_ingredient_id
            LEFT JOIN ingredient_cost_history ich
                   ON ich.org_ingredient_id = r.org_ingredient_id
                  AND ich.effective_until IS NULL
            WHERE r.menu_item_id = e.menu_item_id
              AND COALESCE(r.size_label::text, 'one_size') = e.size_label
        ) r
        ORDER BY e.item_name, e.size_label
        "#,
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let cost = r.cost_piastres.map(round_piastres);
            let (margin_pct, food_cost_pct) = match cost {
                Some(c) if r.price > 0 => (
                    Some((r.price - c) as f64 / r.price as f64),
                    Some(c as f64 / r.price as f64),
                ),
                _ => (None, None),
            };
            let _ = r.has_recipe;
            SkuCost {
                menu_item_id: r.menu_item_id,
                size_label: r.size_label,
                item_name: r.item_name,
                category_id: r.category_id,
                price: r.price,
                cost_missing: cost.is_none(),
                cost,
                margin_pct,
                food_cost_pct,
            }
        })
        .collect())
}

/// Computed cost for one addon item.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct AddonCost {
    pub addon_item_id: Uuid,
    pub name: String,
    pub addon_type: String,
    /// Default price in piastres.
    pub price: i64,
    /// Ingredient cost rollup in piastres. `null` ⟺ unknown.
    pub cost: Option<i64>,
    pub cost_missing: bool,
    pub margin_pct: Option<f64>,
}

/// Current ingredient-cost rollup for every active addon in an org, piastres.
pub async fn org_addon_costs(pool: &PgPool, org_id: Uuid) -> Result<Vec<AddonCost>, AppError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        addon_item_id: Uuid,
        name: String,
        addon_type: String,
        price: i64,
        cost_piastres: Option<Decimal>,
    }

    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        r#"
        SELECT
            a.id            AS addon_item_id,
            a.name,
            a.type          AS addon_type,
            a.default_price::bigint AS price,
            (
                SELECT
                    CASE
                        WHEN COUNT(*) = 0 THEN NULL
                        WHEN bool_or(ai.org_ingredient_id IS NULL
                                     OR COALESCE(ich.cost_per_unit, oi.cost_per_unit) IS NULL)
                            THEN NULL
                        ELSE SUM(ai.quantity_used * COALESCE(ich.cost_per_unit, oi.cost_per_unit))
                    END
                FROM addon_item_ingredients ai
                LEFT JOIN org_ingredients oi ON oi.id = ai.org_ingredient_id
                LEFT JOIN ingredient_cost_history ich
                       ON ich.org_ingredient_id = ai.org_ingredient_id
                      AND ich.effective_until IS NULL
                WHERE ai.addon_item_id = a.id
            ) AS cost_piastres
        FROM addon_items a
        WHERE a.org_id = $1 AND a.is_active = TRUE
        ORDER BY a.name
        "#,
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let cost = r.cost_piastres.map(round_piastres);
            let margin_pct = match cost {
                Some(c) if r.price > 0 => Some((r.price - c) as f64 / r.price as f64),
                _ => None,
            };
            AddonCost {
                addon_item_id: r.addon_item_id,
                name: r.name,
                addon_type: r.addon_type,
                price: r.price,
                cost_missing: cost.is_none(),
                cost,
                margin_pct,
            }
        })
        .collect())
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn round_piastres_rounds_half_away_from_zero() {
        assert_eq!(round_piastres(dec!(1250)), 1250);
        assert_eq!(round_piastres(dec!(0.5)), 1);
        assert_eq!(round_piastres(dec!(0.4)), 0);
        assert_eq!(round_piastres(dec!(300.5)), 301);
    }
}
