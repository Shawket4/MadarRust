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

/// Pure weighted moving-average blend — the arithmetic core of
/// [`apply_weighted_average_cost`], extracted so it can be unit-tested and
/// fuzzed without a database.
///
///   new = (prior_on_hand × current_cost + received_qty × received_cost)
///         / (prior_on_hand + received_qty)
///
/// `current_cost` is this branch's prior actual cost (or the org default); when
/// it is `None`, or there is no prior stock, the received cost becomes the new
/// cost. Result is kept at 2 dp (`cost_per_unit` is numeric(15,2) PIASTRES and
/// deliberately holds sub-piastre cost — rounding to whole piastres silently
/// drove cheap-per-base-unit ingredients to 0).
///
/// PRECONDITION: callers pass `received_qty > 0` (the receipt path short-circuits
/// non-positive receipts before reaching here). The division branch is only
/// entered when `prior_on_hand > 0`, so with a positive `received_qty` the
/// denominator is always strictly positive.
pub fn blend_weighted_cost(
    prior_on_hand: Decimal,
    current_cost: Option<Decimal>,
    received_qty: Decimal,
    received_unit_cost: Decimal,
) -> Decimal {
    match current_cost {
        Some(cur) if prior_on_hand > Decimal::ZERO =>
            (prior_on_hand * cur + received_qty * received_unit_cost)
                / (prior_on_hand + received_qty),
        _ => received_unit_cost,
    }
    .round_dp(2)
}

/// Apply weighted moving-average costing for a SINGLE BRANCH after receiving
/// `received_qty` units (in the ingredient's base stock unit) at
/// `received_unit_cost` piastres/unit.
///
/// MUST be called BEFORE the received stock is added to `branch_inventory` — it
/// reads the PRIOR on-hand FOR THIS BRANCH to weight the average:
///   new = (prior_on_hand × current_cost + received_qty × received_cost)
///         / (prior_on_hand + received_qty)
/// The prior cost is this branch's actual cost (`branch_inventory.cost_per_unit`)
/// or, if it has none yet, the org default (`org_ingredients.cost_per_unit`).
/// When the prior cost is unknown (NULL) or there is no prior stock, the
/// received cost becomes the new cost.
///
/// Writes the branch's actual cost (`branch_inventory.cost_per_unit`, upserting
/// the row if needed) and rolls a BRANCH-SCOPED `ingredient_cost_history` epoch.
/// The org default (`org_ingredients.cost_per_unit`, the standard cost) is NOT
/// touched by receipts. Returns the new per-unit cost in piastres.
pub async fn apply_weighted_average_cost(
    conn:               &mut sqlx::PgConnection,
    branch_id:          Uuid,
    org_ingredient_id:  Uuid,
    received_qty:       Decimal,
    received_unit_cost: Decimal,
    changed_by:         Uuid,
) -> Result<Decimal, AppError> {
    // Prior actual cost + on-hand for THIS branch; org default cost as fallback.
    let (branch_cost, branch_stock, org_cost): (Option<Decimal>, Option<Decimal>, Option<Decimal>) =
        sqlx::query_as(
            "SELECT bi.cost_per_unit, bi.current_stock, oi.cost_per_unit \
             FROM org_ingredients oi \
             LEFT JOIN branch_inventory bi \
                    ON bi.org_ingredient_id = oi.id AND bi.branch_id = $2 \
             WHERE oi.id = $1 AND oi.deleted_at IS NULL",
        )
        .bind(org_ingredient_id)
        .bind(branch_id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| AppError::NotFound("Ingredient not found".into()))?;

    let cur_cost = branch_cost.or(org_cost);
    let prior_on_hand = branch_stock.unwrap_or(Decimal::ZERO);

    if received_qty <= Decimal::ZERO {
        return Ok(cur_cost.unwrap_or(received_unit_cost));
    }

    // Blend and keep 2 dp. cost_per_unit is numeric(15,2) PIASTRES and
    // deliberately holds sub-piastre cost, so we must NOT round to whole
    // piastres — doing so silently drove cheap-per-base-unit ingredients (e.g.
    // 0.40 piastres/g) to 0 ("free") and lost precision on every blend.
    let new_cost = blend_weighted_cost(prior_on_hand, cur_cost, received_qty, received_unit_cost);

    // Only roll a new branch epoch when this branch's per-unit cost moved.
    if branch_cost.map(|c| c.round_dp(2)) != Some(new_cost) {
        // Persist the branch's actual cost (creating the row if the branch isn't
        // tracking this ingredient yet; the caller's stock upsert then adds qty).
        sqlx::query(
            "INSERT INTO branch_inventory \
                 (branch_id, org_ingredient_id, current_stock, reorder_threshold, cost_per_unit) \
             VALUES ($1, $2, 0, 0, $3) \
             ON CONFLICT (branch_id, org_ingredient_id) \
             DO UPDATE SET cost_per_unit = EXCLUDED.cost_per_unit, updated_at = now()",
        )
        .bind(branch_id).bind(org_ingredient_id).bind(new_cost)
        .execute(&mut *conn).await?;

        sqlx::query(
            "UPDATE ingredient_cost_history SET effective_until = now() \
             WHERE org_ingredient_id = $1 AND branch_id = $2 AND effective_until IS NULL",
        )
        .bind(org_ingredient_id).bind(branch_id).execute(&mut *conn).await?;
        sqlx::query(
            "INSERT INTO ingredient_cost_history \
                 (org_ingredient_id, branch_id, cost_per_unit, effective_from, changed_by, note) \
             VALUES ($1, $2, $3, now(), $4, 'Weighted average from purchase')",
        )
        .bind(org_ingredient_id).bind(branch_id).bind(new_cost).bind(changed_by)
        .execute(&mut *conn).await?;
    }

    Ok(new_cost)
}

/// Resolve point-in-time PIASTRE cost per unit for a set of ingredients at `at`,
/// FOR A SPECIFIC BRANCH.
///
/// Resolution order per ingredient:
///   1. branch actual epoch (`branch_id = $3`) covering `at`
///   2. org standard epoch (`branch_id IS NULL`) covering `at`
///   3. fallback: `org_ingredients.cost_per_unit` (the current org default —
///      legacy rows that predate history maintenance)
///
/// Ingredients absent from the result are unknown → cost-missing.
pub async fn ingredient_costs_at(
    pool: &PgPool,
    branch_id: Uuid,
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
                          AND h.branch_id = $3
                          AND h.effective_from <= $2
                          AND (h.effective_until IS NULL OR h.effective_until > $2)
                        ORDER BY h.effective_from DESC
                        LIMIT 1),
                       (SELECT h.cost_per_unit
                        FROM ingredient_cost_history h
                        WHERE h.org_ingredient_id = oi.id
                          AND h.branch_id IS NULL
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
    .bind(branch_id)
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
    /// Recipe cost rollup in piastres over the ingredients that *are* priced.
    /// `null` only when there is no recipe, or no recipe ingredient has a known
    /// cost at all. A partial rollup (some ingredients unpriced) still returns
    /// the sum so far, with `cost_missing = true` flagging it as incomplete.
    pub cost: Option<i64>,
    /// `true` when at least one recipe ingredient is unlinked or has no cost, so
    /// `cost` (if any) is a partial figure rather than the full COGS.
    pub cost_missing: bool,
    /// `(price - cost) / price` — only when the cost is *complete* and price > 0.
    /// Suppressed (`null`) for partial rollups so an incomplete cost is never
    /// graded as a food-cost percentage.
    pub margin_pct: Option<f64>,
    /// `cost / price` — only when the cost is *complete* and price > 0.
    pub food_cost_pct: Option<f64>,
}

/// Current recipe-cost rollup for every active SKU in an org, in piastres.
///
/// `branch_id` selects whose actual cost to use: `Some(b)` resolves each
/// ingredient at branch `b`'s actual cost (falling back to the org default),
/// `None` uses the org default (standard) cost — for org-wide views with no
/// branch context. The rollup is partial-tolerant: priced ingredients are
/// summed even when others are unlinked or lack a cost, and `cost_missing`
/// flags that the figure is incomplete. `COALESCE(..., 0)` is never used — an
/// unpriced ingredient is excluded from the sum, not treated as free.
pub async fn org_sku_costs(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Option<Uuid>,
) -> Result<Vec<SkuCost>, AppError> {
    sku_costs_impl(pool, org_id, None, branch_id).await
}

/// Same recipe-cost rollup as [`org_sku_costs`] but scoped to a specific set of
/// menu items, so list endpoints can embed per-page costs without a second
/// org-wide round trip. Empty `item_ids` ⇒ no rows (no query issued).
pub async fn sku_costs_for_items(
    pool: &PgPool,
    org_id: Uuid,
    item_ids: &[Uuid],
    branch_id: Option<Uuid>,
) -> Result<Vec<SkuCost>, AppError> {
    if item_ids.is_empty() {
        return Ok(Vec::new());
    }
    sku_costs_impl(pool, org_id, Some(item_ids), branch_id).await
}

async fn sku_costs_impl(
    pool: &PgPool,
    org_id: Uuid,
    item_ids: Option<&[Uuid]>,
    branch_id: Option<Uuid>,
) -> Result<Vec<SkuCost>, AppError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        menu_item_id: Uuid,
        size_label: String,
        item_name: String,
        category_id: Option<Uuid>,
        price: i64,
        cost_piastres: Option<Decimal>,
        cost_incomplete: Option<bool>,
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
              AND ($2::uuid[] IS NULL OR mi.id = ANY($2))
        )
        SELECT
            e.menu_item_id,
            e.size_label,
            e.item_name,
            e.category_id,
            e.price,
            r.cost_piastres,
            r.cost_incomplete,
            r.has_recipe
        FROM expanded e
        CROSS JOIN LATERAL (
            SELECT
                SUM(r.quantity_used * COALESCE(bi.cost_per_unit, oi.cost_per_unit))
                    FILTER (WHERE r.org_ingredient_id IS NOT NULL
                              AND COALESCE(bi.cost_per_unit, oi.cost_per_unit) IS NOT NULL)
                           AS cost_piastres,
                bool_or(r.org_ingredient_id IS NULL
                        OR COALESCE(bi.cost_per_unit, oi.cost_per_unit) IS NULL)
                           AS cost_incomplete,
                COUNT(*) > 0 AS has_recipe
            FROM menu_item_recipes r
            LEFT JOIN org_ingredients oi ON oi.id = r.org_ingredient_id
            LEFT JOIN branch_inventory bi
                   ON bi.org_ingredient_id = r.org_ingredient_id
                  AND bi.branch_id = $3
            WHERE r.menu_item_id = e.menu_item_id
              AND COALESCE(r.size_label::text, 'one_size') = e.size_label
        ) r
        ORDER BY e.item_name, e.size_label
        "#,
    )
    .bind(org_id)
    .bind(item_ids)
    .bind(branch_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let cost = r.cost_piastres.map(round_piastres);
            let incomplete = r.cost_incomplete.unwrap_or(false);
            // Only grade a food-cost % when the rollup is *complete* — a partial
            // cost would otherwise be flattered with a misleadingly low %.
            let (margin_pct, food_cost_pct) = match cost {
                Some(c) if !incomplete && r.price > 0 => (
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
                cost_missing: incomplete,
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
    /// Ingredient cost rollup in piastres over the ingredients that *are*
    /// priced. A partial rollup still returns the sum so far, with
    /// `cost_missing = true`; `null` only when nothing is priced.
    pub cost: Option<i64>,
    /// `true` when at least one ingredient is unlinked or has no cost, so `cost`
    /// (if any) is partial rather than the full figure.
    pub cost_missing: bool,
    /// `(price - cost) / price` — only when the cost is *complete* and price > 0.
    pub margin_pct: Option<f64>,
}

/// Current ingredient-cost rollup for every active addon in an org, piastres.
///
/// `branch_id` selects whose actual cost to use (`Some` = that branch's actual
/// cost with org-default fallback; `None` = org default / standard cost).
pub async fn org_addon_costs(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Option<Uuid>,
) -> Result<Vec<AddonCost>, AppError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        addon_item_id: Uuid,
        name: String,
        addon_type: String,
        price: i64,
        cost_piastres: Option<Decimal>,
        cost_incomplete: Option<bool>,
    }

    let rows: Vec<Row> = sqlx::query_as::<_, Row>(
        r#"
        SELECT
            a.id            AS addon_item_id,
            a.name,
            a.type          AS addon_type,
            a.default_price::bigint AS price,
            c.cost_piastres,
            c.cost_incomplete
        FROM addon_items a
        LEFT JOIN LATERAL (
            SELECT
                SUM(ai.quantity_used * COALESCE(bi.cost_per_unit, oi.cost_per_unit))
                    FILTER (WHERE ai.org_ingredient_id IS NOT NULL
                              AND COALESCE(bi.cost_per_unit, oi.cost_per_unit) IS NOT NULL)
                           AS cost_piastres,
                bool_or(ai.org_ingredient_id IS NULL
                        OR COALESCE(bi.cost_per_unit, oi.cost_per_unit) IS NULL)
                           AS cost_incomplete
            FROM addon_item_ingredients ai
            LEFT JOIN org_ingredients oi ON oi.id = ai.org_ingredient_id
            LEFT JOIN branch_inventory bi
                   ON bi.org_ingredient_id = ai.org_ingredient_id
                  AND bi.branch_id = $2
            WHERE ai.addon_item_id = a.id
        ) c ON TRUE
        WHERE a.org_id = $1 AND a.is_active = TRUE
        ORDER BY a.name
        "#,
    )
    .bind(org_id)
    .bind(branch_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let cost = r.cost_piastres.map(round_piastres);
            let incomplete = r.cost_incomplete.unwrap_or(false);
            let margin_pct = match cost {
                Some(c) if !incomplete && r.price > 0 => Some((r.price - c) as f64 / r.price as f64),
                _ => None,
            };
            AddonCost {
                addon_item_id: r.addon_item_id,
                name: r.name,
                addon_type: r.addon_type,
                price: r.price,
                cost_missing: incomplete,
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

    #[test]
    fn blend_no_prior_stock_takes_received_cost() {
        // prior_on_hand == 0 → the division branch is skipped.
        assert_eq!(blend_weighted_cost(dec!(0), Some(dec!(500)), dec!(10), dec!(700)), dec!(700));
    }

    #[test]
    fn blend_unknown_current_cost_takes_received_cost() {
        assert_eq!(blend_weighted_cost(dec!(100), None, dec!(10), dec!(700)), dec!(700));
    }

    #[test]
    fn blend_weights_by_quantity() {
        // 100 @ 500 + 100 @ 700 = (50000 + 70000) / 200 = 600.
        assert_eq!(blend_weighted_cost(dec!(100), Some(dec!(500)), dec!(100), dec!(700)), dec!(600));
    }

    #[test]
    fn blend_preserves_sub_piastre_precision() {
        // 1000 @ 0.40 + 1000 @ 0.60 = 1000 / 2000 = 0.50 — must NOT collapse to 0.
        assert_eq!(blend_weighted_cost(dec!(1000), Some(dec!(0.40)), dec!(1000), dec!(0.60)), dec!(0.50));
    }

    #[test]
    fn blend_rounds_to_two_dp() {
        // 1 @ 1 + 2 @ 2 = 5/3 = 1.6666… → 1.67.
        assert_eq!(blend_weighted_cost(dec!(1), Some(dec!(1)), dec!(2), dec!(2)), dec!(1.67));
    }

    #[test]
    fn blend_result_is_a_convex_combination() {
        // The blended cost always lies within [min, max] of the two costs.
        let r = blend_weighted_cost(dec!(3), Some(dec!(120)), dec!(7), dec!(260));
        assert!(r >= dec!(120) && r <= dec!(260), "blend {r} escaped [120, 260]");
    }

    #[test]
    fn blend_zero_received_qty_does_not_divide_by_zero() {
        // recv_qty == 0 must never trigger a 0/0 — the `prior_on_hand > 0` guard
        // routes prior==0 to the received-cost branch. Mutating that guard (to
        // `true` or `>=`) divides 0/0 here — flagged by mutation testing at
        // costing/service.rs:51.
        assert_eq!(blend_weighted_cost(Decimal::ZERO, Some(dec!(500)), Decimal::ZERO, dec!(700)), dec!(700));
        // With prior stock and no receipt, the existing cost is unchanged.
        assert_eq!(blend_weighted_cost(dec!(10), Some(dec!(500)), Decimal::ZERO, dec!(700)), dec!(500));
    }
}
