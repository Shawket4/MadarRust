//! Adapter — bridges Madar's schema into the engine's input types.
//!
//! ALL money leaving this module is integer **piastres**. Ingredient costs
//! (`org_ingredients` / `ingredient_cost_history` `cost_per_unit`) are also
//! stored in piastres — the dashboard converts EGP input on entry — so the
//! rollups here sum and round, with no currency conversion anywhere.
//!
//! Cost sourcing, in priority order:
//!   1. `order_items.unit_cost` — the recipe-scope cost snapshotted at sale
//!      time by the order pipeline (already piastres, swap-aware, immutable).
//!   2. Point-in-time rollup: `menu_item_recipes` × the
//!      `ingredient_cost_history` epoch covering the order timestamp,
//!      falling back to `org_ingredients.cost_per_unit` for legacy rows.
//!
//! Cost-optional contract: `COALESCE(..., 0)` is NEVER used for cost — zero
//! would collide with genuinely free items. The SQL uses
//! `bool_or(cost IS NULL)` to short-circuit a rollup to NULL when any
//! component is missing.
//!
//! Bundle handling (the schema facts the old adapter got wrong): a bundle
//! purchase stores ONE `order_items` row with `menu_item_id = NULL` and
//! `bundle_id` set; the contained SKUs live in `order_line_bundle_components`.
//! Therefore:
//!   - sales (price signal) come from standalone `order_items` lines only;
//!   - baskets (co-occurrence signal) UNION in the bundle component lines;
//!   - `bundle_only` SKUs are those appearing in component lines but never
//!     standalone, detected via EXCEPT.
//!
//! Size labels: enum columns are always compared as
//! `COALESCE(col::text, 'one_size')` to avoid enum/text operator errors.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use rust_decimal::prelude::ToPrimitive;
use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;
use crate::menu_advisor::dto::{AnalysisConfig, ItemKey};
use super::engine::{Basket, ItemSnapshot, SaleEvent};

// ─────────────────────────────────────────────────────────────────────
// Row types
// ─────────────────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct SnapshotRow {
    menu_item_id: Uuid,
    size_label: String,
    item_name: String,
    category_id: Option<Uuid>,
    current_price: i64,
    is_active: bool,
    cost_per_serving: Option<sqlx::types::BigDecimal>,
}

#[derive(sqlx::FromRow)]
struct SaleRow {
    menu_item_id: Uuid,
    size_label: String,
    quantity_sold: i64,
    unit_price_paid: i64,
    unit_cost_at_sale: Option<sqlx::types::BigDecimal>,
    sold_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct BasketRow {
    order_id: Uuid,
    menu_item_id: Uuid,
    size_label: String,
}

#[derive(sqlx::FromRow)]
struct KeyRow {
    menu_item_id: Uuid,
    size_label: String,
}

// ─────────────────────────────────────────────────────────────────────
// Public surface
// ─────────────────────────────────────────────────────────────────────

pub struct AdapterInputs {
    pub snapshots: Vec<ItemSnapshot>,
    pub sales: Vec<SaleEvent>,
    pub baskets: Vec<Basket>,
    pub price_changed_keys: HashSet<ItemKey>,
}

/// Load all engine inputs for `branch_id` over a window ending at `now`.
///
/// Snapshot prices reflect what a customer currently sees:
/// `item_sizes.price_override` → `menu_items.base_price`.
pub async fn load_inputs(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    now: DateTime<Utc>,
    config: &AnalysisConfig,
) -> Result<AdapterInputs, AppError> {
    let window_start = now - Duration::seconds((config.analysis_window_days * 86_400.0) as i64);

    let snapshots = load_snapshots(pool, org_id, branch_id, &window_start, &now).await?;
    let sales = load_sales(pool, org_id, branch_id, &window_start, &now).await?;
    let baskets = load_baskets(pool, org_id, branch_id, &window_start, &now).await?;
    let price_changed_keys = load_price_changed(pool, org_id, &window_start, &now).await?;

    // Sales for SKUs with no snapshot (item hard-deleted since) are dropped —
    // observably, not silently.
    let snapshot_keys: HashSet<&ItemKey> = snapshots.iter().map(|s| &s.key).collect();
    let (sales, orphaned): (Vec<_>, Vec<_>) =
        sales.into_iter().partition(|s| snapshot_keys.contains(&s.key));
    for s in &orphaned {
        tracing::warn!(
            menu_item_id = %s.key.menu_item_id,
            size_label = %s.key.size_label,
            "Dropping sale rows for SKU with no menu snapshot (deleted item?)"
        );
    }

    Ok(AdapterInputs { snapshots, sales, baskets, price_changed_keys })
}

// ─────────────────────────────────────────────────────────────────────
// 1. Snapshots
// ─────────────────────────────────────────────────────────────────────

async fn load_snapshots(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    window_start: &DateTime<Utc>,
    now: &DateTime<Utc>,
) -> Result<Vec<ItemSnapshot>, AppError> {
    // bundle_only: SKUs that moved inside bundles this window (component
    // lines) and never as a standalone order line.
    let bundle_only_rows: Vec<KeyRow> = sqlx::query_as::<_, KeyRow>(
        r#"
        SELECT t.menu_item_id, t.size_label FROM (
            SELECT c.item_id AS menu_item_id,
                   COALESCE(c.size_label::text, 'one_size') AS size_label
            FROM order_line_bundle_components c
            JOIN order_items oi ON oi.id = c.order_line_id
            JOIN orders o       ON o.id = oi.order_id
            JOIN branches b     ON b.id = o.branch_id
            WHERE b.org_id = $1
              AND o.branch_id = $2
              AND o.created_at >= $3
              AND o.created_at <= $4
              AND o.status = 'completed'
            EXCEPT
            SELECT oi.menu_item_id,
                   COALESCE(oi.size_label::text, 'one_size')
            FROM order_items oi
            JOIN orders o   ON o.id = oi.order_id
            JOIN branches b ON b.id = o.branch_id
            WHERE b.org_id = $1
              AND o.branch_id = $2
              AND o.created_at >= $3
              AND o.created_at <= $4
              AND o.status = 'completed'
              AND oi.menu_item_id IS NOT NULL
        ) t
        "#,
    )
    .bind(org_id)
    .bind(branch_id)
    .bind(window_start)
    .bind(now)
    .fetch_all(pool)
    .await?;

    let bundle_only_set: HashSet<(Uuid, String)> = bundle_only_rows
        .into_iter()
        .map(|r| (r.menu_item_id, r.size_label))
        .collect();

    // Current-cost rollup: a correlated subquery returning NULL whenever any
    // recipe ingredient lacks a resolvable cost. The open cost-history epoch
    // is read through a LATERAL so accidental duplicate open epochs can't
    // multiply rows.
    let rows: Vec<SnapshotRow> = sqlx::query_as::<_, SnapshotRow>(
        r#"
        WITH expanded AS (
            SELECT
                mi.id            AS menu_item_id,
                mi.name          AS item_name,
                mi.category_id,
                mi.is_active,
                mi.base_price,
                COALESCE(sz.label::text, 'one_size') AS size_label_text,
                sz.price_override AS size_price_override
            FROM menu_items mi
            LEFT JOIN item_sizes sz
                   ON sz.menu_item_id = mi.id
                  AND sz.is_active = TRUE
            WHERE mi.org_id = $1
              AND mi.deleted_at IS NULL
        )
        SELECT
            e.menu_item_id,
            e.size_label_text AS size_label,
            e.item_name,
            e.category_id,
            COALESCE(e.size_price_override, e.base_price)::bigint AS current_price,
            e.is_active,
            (
                SELECT
                    CASE
                        WHEN COUNT(*) = 0 THEN NULL
                        WHEN bool_or(r.org_ingredient_id IS NULL
                                     OR COALESCE(bi.cost_per_unit, oing.cost_per_unit) IS NULL)
                            THEN NULL
                        -- costs are stored in piastres; round the fractional sum.
                        -- Per-branch actual cost, falling back to the org default.
                        ELSE round(SUM(r.quantity_used
                                 * COALESCE(bi.cost_per_unit, oing.cost_per_unit)))
                    END
                FROM menu_item_recipes r
                LEFT JOIN org_ingredients oing ON oing.id = r.org_ingredient_id
                LEFT JOIN branch_inventory bi
                       ON bi.org_ingredient_id = r.org_ingredient_id
                      AND bi.branch_id = $2
                WHERE r.menu_item_id = e.menu_item_id
                  AND COALESCE(r.size_label::text, 'one_size') = e.size_label_text
            ) AS cost_per_serving
        FROM expanded e
        ORDER BY e.item_name, e.size_label_text
        "#,
    )
    .bind(org_id)
    .bind(branch_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            let cost = r.cost_per_serving.and_then(|d| d.to_i64());
            let bundle_only =
                bundle_only_set.contains(&(r.menu_item_id, r.size_label.clone()));
            ItemSnapshot {
                key: ItemKey { menu_item_id: r.menu_item_id, size_label: r.size_label },
                category_id: r.category_id,
                name: r.item_name,
                current_price: r.current_price,
                cost_per_serving: cost,
                is_active: r.is_active,
                bundle_only,
            }
        })
        .collect())
}

// ─────────────────────────────────────────────────────────────────────
// 2. Sale events (standalone lines only — price signal)
// ─────────────────────────────────────────────────────────────────────

async fn load_sales(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    window_start: &DateTime<Utc>,
    now: &DateTime<Utc>,
) -> Result<Vec<SaleEvent>, AppError> {
    // unit_price is kept GROSS of order-level discounts: loyalty/order
    // discounts are not item price signals and would contaminate the
    // effective price. The point-in-time reconstruct only runs for legacy
    // rows (COALESCE evaluates lazily) and reads the epoch covering the
    // order timestamp through an overlap-safe LATERAL.
    let rows: Vec<SaleRow> = sqlx::query_as::<_, SaleRow>(
        r#"
        SELECT
            oi.menu_item_id,
            COALESCE(oi.size_label::text, 'one_size')  AS size_label,
            oi.quantity::bigint                        AS quantity_sold,
            oi.unit_price::bigint                      AS unit_price_paid,
            COALESCE(
                oi.unit_cost::numeric,
                (
                    SELECT
                        CASE
                            WHEN COUNT(*) = 0 THEN NULL
                            WHEN bool_or(r.org_ingredient_id IS NULL
                                         OR COALESCE(ich.cost_per_unit, ing.cost_per_unit) IS NULL)
                                THEN NULL
                            -- costs are stored in piastres; round the fractional sum
                            ELSE round(SUM(r.quantity_used
                                     * COALESCE(ich.cost_per_unit, ing.cost_per_unit)))
                        END
                    FROM menu_item_recipes r
                    LEFT JOIN org_ingredients ing ON ing.id = r.org_ingredient_id
                    LEFT JOIN LATERAL (
                        SELECT h.cost_per_unit
                        FROM ingredient_cost_history h
                        WHERE h.org_ingredient_id = r.org_ingredient_id
                          AND (h.branch_id = o.branch_id OR h.branch_id IS NULL)
                          AND h.effective_from <= o.created_at
                          AND (h.effective_until IS NULL OR h.effective_until > o.created_at)
                        -- prefer this order's branch epoch over the org default
                        ORDER BY (h.branch_id IS NULL), h.effective_from DESC
                        LIMIT 1
                    ) ich ON TRUE
                    WHERE r.menu_item_id = oi.menu_item_id
                      AND COALESCE(r.size_label::text, 'one_size')
                          = COALESCE(oi.size_label::text, 'one_size')
                )
            )                                          AS unit_cost_at_sale,
            o.created_at                               AS sold_at
        FROM order_items oi
        JOIN orders   o ON o.id = oi.order_id
        JOIN branches b ON b.id = o.branch_id
        WHERE b.org_id        = $1
          AND o.branch_id     = $2
          AND o.created_at   >= $3
          AND o.created_at   <= $4
          AND o.status        = 'completed'
          AND oi.menu_item_id IS NOT NULL
          AND oi.bundle_id    IS NULL  -- standalone lines only (defensive; bundle
                                       -- lines have menu_item_id NULL anyway)
        "#,
    )
    .bind(org_id)
    .bind(branch_id)
    .bind(window_start)
    .bind(now)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| SaleEvent {
            key: ItemKey { menu_item_id: r.menu_item_id, size_label: r.size_label },
            quantity_sold: r.quantity_sold,
            unit_price_paid: r.unit_price_paid,
            unit_cost_at_sale: r.unit_cost_at_sale.and_then(|d| d.to_i64()),
            sold_at: r.sold_at,
        })
        .collect())
}

// ─────────────────────────────────────────────────────────────────────
// 3. Baskets (co-occurrence signal — bundle components INCLUDED)
// ─────────────────────────────────────────────────────────────────────

async fn load_baskets(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    window_start: &DateTime<Utc>,
    now: &DateTime<Utc>,
) -> Result<Vec<Basket>, AppError> {
    // A basket reflects what the customer actually took home, so bundle
    // component lines count toward co-occurrence while staying out of
    // standalone velocity. Dedup by SKU within order.
    let rows: Vec<BasketRow> = sqlx::query_as::<_, BasketRow>(
        r#"
        SELECT DISTINCT t.order_id, t.menu_item_id, t.size_label FROM (
            SELECT oi.order_id,
                   oi.menu_item_id,
                   COALESCE(oi.size_label::text, 'one_size') AS size_label
            FROM order_items oi
            JOIN orders o   ON o.id = oi.order_id
            JOIN branches b ON b.id = o.branch_id
            WHERE b.org_id = $1
              AND o.branch_id = $2
              AND o.created_at >= $3
              AND o.created_at <= $4
              AND o.status = 'completed'
              AND oi.menu_item_id IS NOT NULL
            UNION ALL
            SELECT oi.order_id,
                   c.item_id AS menu_item_id,
                   COALESCE(c.size_label::text, 'one_size') AS size_label
            FROM order_line_bundle_components c
            JOIN order_items oi ON oi.id = c.order_line_id
            JOIN orders o       ON o.id = oi.order_id
            JOIN branches b     ON b.id = o.branch_id
            WHERE b.org_id = $1
              AND o.branch_id = $2
              AND o.created_at >= $3
              AND o.created_at <= $4
              AND o.status = 'completed'
        ) t
        "#,
    )
    .bind(org_id)
    .bind(branch_id)
    .bind(window_start)
    .bind(now)
    .fetch_all(pool)
    .await?;

    let mut by_order: HashMap<Uuid, Vec<ItemKey>> = HashMap::new();
    for r in rows {
        by_order.entry(r.order_id).or_default().push(ItemKey {
            menu_item_id: r.menu_item_id,
            size_label: r.size_label,
        });
    }
    Ok(by_order.into_values().collect())
}

// ─────────────────────────────────────────────────────────────────────
// 4. Price-changed-in-window detection
// ─────────────────────────────────────────────────────────────────────

async fn load_price_changed(
    pool: &PgPool,
    org_id: Uuid,
    window_start: &DateTime<Utc>,
    now: &DateTime<Utc>,
) -> Result<HashSet<ItemKey>, AppError> {
    // Only GENUINE changes count: item creation seeds a first epoch
    // (menu/handlers.rs), so an epoch only flags the SKU when an EARLIER
    // epoch exists for the same scope.
    //
    // No size fan-out: `item_sizes.price_override` is NOT NULL, so a sized
    // SKU's customer-visible price only ever changes via size epochs
    // (size_label set); base-price epochs (size_label NULL → 'one_size')
    // only affect the 'one_size' SKU, which exists exactly when the item
    // has no size rows.
    let rows: Vec<KeyRow> = sqlx::query_as::<_, KeyRow>(
        r#"
        SELECT DISTINCT
            e.menu_item_id,
            COALESCE(e.size_label::text, 'one_size') AS size_label
        FROM menu_item_price_epochs e
        JOIN menu_items mi ON mi.id = e.menu_item_id
        WHERE mi.org_id          = $1
          AND e.effective_from  >  $2
          AND e.effective_from  <= $3
          AND EXISTS (
              SELECT 1 FROM menu_item_price_epochs p
              WHERE p.menu_item_id = e.menu_item_id
                AND COALESCE(p.size_label::text, 'one_size')
                    = COALESCE(e.size_label::text, 'one_size')
                AND p.effective_from < e.effective_from
          )
        "#,
    )
    .bind(org_id)
    .bind(window_start)
    .bind(now)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| ItemKey { menu_item_id: r.menu_item_id, size_label: r.size_label })
        .collect())
}
