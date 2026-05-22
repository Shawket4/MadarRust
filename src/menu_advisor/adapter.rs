//! Adapter — bridges Sufrix's schema into the engine's input types.
//!
//! Cost-optional rollup contract:
//!   - `ItemSnapshot.cost_per_serving = None` ⟺ any ingredient in the recipe
//!     lacks a current cost in `ingredient_cost_history`, OR the item has no
//!     recipe defined at all.
//!   - `SaleEvent.unit_cost_at_sale = None` ⟺ any ingredient lacked a cost
//!     epoch covering the order's timestamp.
//!
//! `COALESCE(..., 0)` is NEVER used for cost — zero would collide with
//! genuinely free items (complimentary water, refills). The SQL uses
//! `bool_or(cost IS NULL)` to short-circuit the rollup to NULL when any
//! component is missing.
//!
//! Size-label matching uses `IS NOT DISTINCT FROM` so that NULL size_labels
//! (non-sized items) match correctly with recipe rows that also use NULL.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use rust_decimal::prelude::ToPrimitive;
use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;
use super::engine::{
    AnalysisConfig, Basket, ItemKey, ItemSnapshot, SaleEvent,
};

// ─────────────────────────────────────────────────────────────────────────────
// Row types
// ─────────────────────────────────────────────────────────────────────────────

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
struct BundleOnlyRow {
    menu_item_id: Uuid,
    size_label: String,
}

#[derive(sqlx::FromRow)]
struct PriceChangedRow {
    menu_item_id: Uuid,
    size_label: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Public surface
// ─────────────────────────────────────────────────────────────────────────────

pub struct AdapterInputs {
    pub snapshots: Vec<ItemSnapshot>,
    pub sales: Vec<SaleEvent>,
    pub baskets: Vec<Basket>,
    pub price_changed_keys: HashSet<ItemKey>,
}

/// Load all engine inputs for `branch_id` over a window ending at `now`.
///
/// Snapshot prices reflect what a customer of `branch_id` currently sees:
/// `branch_menu_overrides.price_override` → `item_sizes.price_override`
/// → `menu_items.base_price` (whichever is most specific).
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
    let price_changed_keys =
        load_price_changed(pool, org_id, branch_id, &window_start, &now).await?;

    Ok(AdapterInputs { snapshots, sales, baskets, price_changed_keys })
}

// ─────────────────────────────────────────────────────────────────────────────
// 1. Snapshots
// ─────────────────────────────────────────────────────────────────────────────

async fn load_snapshots(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    window_start: &DateTime<Utc>,
    now: &DateTime<Utc>,
) -> Result<Vec<ItemSnapshot>, AppError> {
    // bundle_only detection: an SKU is bundle_only iff every order_item row in
    // the window has bundle_id IS NOT NULL.
    let bundle_only_rows: Vec<BundleOnlyRow> = sqlx::query_as::<_, BundleOnlyRow>(
        r#"
        SELECT
            oi.menu_item_id,
            COALESCE(oi.size_label::text, 'one_size') AS size_label
        FROM order_items oi
        JOIN orders o ON o.id = oi.order_id
        JOIN branches b ON b.id = o.branch_id
        WHERE b.org_id = $1
          AND o.branch_id = $2
          AND o.created_at >= $3
          AND o.created_at <= $4
          AND o.status = 'completed'
          AND oi.menu_item_id IS NOT NULL
        GROUP BY oi.menu_item_id, COALESCE(oi.size_label::text, 'one_size')
        HAVING bool_and(oi.bundle_id IS NOT NULL) = TRUE
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

    // Cost rollup is a correlated subquery that returns NULL whenever any
    // recipe ingredient lacks a current cost — that's the cost-optional
    // signal the engine relies on.
    let rows: Vec<SnapshotRow> = sqlx::query_as::<_, SnapshotRow>(
        r#"
        WITH expanded AS (
            SELECT
                mi.id            AS menu_item_id,
                mi.name          AS item_name,
                mi.category_id,
                mi.is_active,
                mi.base_price,
                sz.label         AS size_label_enum,
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
            COALESCE(
                bmo.price_override,
                e.size_price_override,
                e.base_price
            )::bigint AS current_price,
            e.is_active,
            (
                SELECT
                    CASE
                        WHEN COUNT(*) = 0 THEN NULL
                        WHEN bool_or(ich.cost_per_unit IS NULL) THEN NULL
                        ELSE SUM(r.quantity_used * ich.cost_per_unit)
                    END
                FROM menu_item_recipes r
                LEFT JOIN ingredient_cost_history ich
                       ON ich.org_ingredient_id = r.org_ingredient_id
                      AND ich.effective_until IS NULL
                WHERE r.menu_item_id = e.menu_item_id
                  AND r.size_label IS NOT DISTINCT FROM e.size_label_enum
            ) AS cost_per_serving
        FROM expanded e
        LEFT JOIN branch_menu_overrides bmo
               ON bmo.menu_item_id = e.menu_item_id
              AND bmo.branch_id    = $2
        ORDER BY e.item_name, e.size_label_text
        "#,
    )
    .bind(org_id)
    .bind(branch_id)
    .fetch_all(pool)
    .await?;

    let _ = (window_start, now); // bundle_only_set already bounded by window

    Ok(rows
        .into_iter()
        .map(|r| {
            let cost = r.cost_per_serving.and_then(|d| d.to_i64());
            let bundle_only =
                bundle_only_set.contains(&(r.menu_item_id, r.size_label.clone()));
            ItemSnapshot {
                key: ItemKey {
                    menu_item_id: r.menu_item_id,
                    size_label: r.size_label,
                },
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

// ─────────────────────────────────────────────────────────────────────────────
// 2. Sale events
// ─────────────────────────────────────────────────────────────────────────────

async fn load_sales(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    window_start: &DateTime<Utc>,
    now: &DateTime<Utc>,
) -> Result<Vec<SaleEvent>, AppError> {
    let rows: Vec<SaleRow> = sqlx::query_as::<_, SaleRow>(
        r#"
        SELECT
            oi.menu_item_id,
            COALESCE(oi.size_label::text, 'one_size')  AS size_label,
            oi.quantity::bigint                        AS quantity_sold,
            oi.unit_price::bigint                      AS unit_price_paid,
            (
                SELECT
                    CASE
                        WHEN COUNT(*) = 0 THEN NULL
                        WHEN bool_or(ich.cost_per_unit IS NULL) THEN NULL
                        ELSE SUM(r.quantity_used * ich.cost_per_unit)
                    END
                FROM menu_item_recipes r
                LEFT JOIN ingredient_cost_history ich
                       ON ich.org_ingredient_id = r.org_ingredient_id
                      AND ich.effective_from   <= o.created_at
                      AND (ich.effective_until IS NULL OR ich.effective_until > o.created_at)
                WHERE r.menu_item_id = oi.menu_item_id
                  AND r.size_label IS NOT DISTINCT FROM oi.size_label
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
          AND oi.bundle_id    IS NULL  -- only standalone lines count toward sales
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
            key: ItemKey {
                menu_item_id: r.menu_item_id,
                size_label: r.size_label,
            },
            quantity_sold: r.quantity_sold,
            unit_price_paid: r.unit_price_paid,
            unit_cost_at_sale: r.unit_cost_at_sale.and_then(|d| d.to_i64()),
            sold_at: r.sold_at,
        })
        .collect())
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. Baskets
// ─────────────────────────────────────────────────────────────────────────────

async fn load_baskets(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    window_start: &DateTime<Utc>,
    now: &DateTime<Utc>,
) -> Result<Vec<Basket>, AppError> {
    // Bundle components are included — a basket reflects what the customer
    // actually took home. Dedup by (menu_item_id, size_label) within order.
    let rows: Vec<BasketRow> = sqlx::query_as::<_, BasketRow>(
        r#"
        SELECT DISTINCT
            oi.order_id,
            oi.menu_item_id,
            COALESCE(oi.size_label::text, 'one_size') AS size_label
        FROM order_items oi
        JOIN orders   o ON o.id = oi.order_id
        JOIN branches b ON b.id = o.branch_id
        WHERE b.org_id        = $1
          AND o.branch_id     = $2
          AND o.created_at   >= $3
          AND o.created_at   <= $4
          AND o.status        = 'completed'
          AND oi.menu_item_id IS NOT NULL
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

// ─────────────────────────────────────────────────────────────────────────────
// 4. Price-changed-in-window detection
// ─────────────────────────────────────────────────────────────────────────────

async fn load_price_changed(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    window_start: &DateTime<Utc>,
    now: &DateTime<Utc>,
) -> Result<HashSet<ItemKey>, AppError> {
    // Epochs covering: base_price (size_label NULL, branch_id NULL),
    //                  item_sizes.price_override (size_label set, branch_id NULL),
    //                  branch_menu_overrides (branch_id set).
    // Any epoch whose effective_from falls inside the window flags that SKU.
    let rows: Vec<PriceChangedRow> = sqlx::query_as::<_, PriceChangedRow>(
        r#"
        SELECT DISTINCT
            e.menu_item_id,
            COALESCE(e.size_label, 'one_size') AS size_label
        FROM menu_item_price_epochs e
        JOIN menu_items mi ON mi.id = e.menu_item_id
        WHERE mi.org_id            = $1
          AND (e.branch_id IS NULL OR e.branch_id = $2)
          AND e.effective_from    > $3
          AND e.effective_from   <= $4
        "#,
    )
    .bind(org_id)
    .bind(branch_id)
    .bind(window_start)
    .bind(now)
    .fetch_all(pool)
    .await?;

    let mut set: HashSet<ItemKey> = HashSet::new();
    for r in rows {
        set.insert(ItemKey {
            menu_item_id: r.menu_item_id,
            size_label: r.size_label.unwrap_or_else(|| "one_size".into()),
        });
    }

    // Fan out: if a row was inserted with size_label = 'one_size' representing
    // a base_price change on a sized item, fan it across all that item's sizes.
    #[derive(sqlx::FromRow)]
    struct SizeRow { menu_item_id: Uuid, size_label: String }

    let touched_items: Vec<Uuid> = set.iter().map(|k| k.menu_item_id).collect();
    if !touched_items.is_empty() {
        let size_rows: Vec<SizeRow> = sqlx::query_as::<_, SizeRow>(
            r#"
            SELECT sz.menu_item_id,
                   COALESCE(sz.label::text, 'one_size') AS size_label
            FROM item_sizes sz
            WHERE sz.menu_item_id = ANY($1)
              AND sz.is_active = TRUE
            "#,
        )
        .bind(&touched_items)
        .fetch_all(pool)
        .await?;

        // Fan out only for keys where size_label was the fallback 'one_size'
        // AND there exist real size rows.
        let touched_one_size: HashSet<Uuid> = set
            .iter()
            .filter(|k| k.size_label == "one_size")
            .map(|k| k.menu_item_id)
            .collect();
        for sr in size_rows {
            if touched_one_size.contains(&sr.menu_item_id) {
                set.insert(ItemKey {
                    menu_item_id: sr.menu_item_id,
                    size_label: sr.size_label,
                });
            }
        }
    }

    Ok(set)
}