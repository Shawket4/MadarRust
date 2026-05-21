//! Adapter: queries the Sufrix DB and shapes data into engine input types.
//!
//! `cost_at_sale` is calculated at query time by joining
//! `ingredient_cost_history` against the order timestamp — no cached proxy.
//!
//! `price_changed_in_window` is detected by checking `menu_item_price_epochs`
//! and `bundle_price_epochs` for any new epoch that started inside the window.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;
use super::engine::{
    AnalysisConfig, Basket, ItemKey, ItemSnapshot, SaleEvent,
};

// ─────────────────────────────────────────────────────────────────────────────
// Internal query row types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct MenuItemRow {
    menu_item_id:    Uuid,
    size_label:      String,   // "one_size" | actual label
    item_name:       String,
    category_id:     Option<Uuid>,
    current_price:   i32,
    is_active:       bool,
    cost_per_serving: sqlx::types::BigDecimal,
}

/// A raw sale event row — cost_at_sale computed via epoch join.
#[derive(sqlx::FromRow)]
struct SaleRow {
    transaction_id:   Uuid,
    menu_item_id:     Uuid,
    size_label:       String,
    quantity_sold:    i64,
    unit_price_paid:  i32,
    unit_cost_at_sale: sqlx::types::BigDecimal,
    sold_at:          DateTime<Utc>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Public API
// ─────────────────────────────────────────────────────────────────────────────

pub struct AdapterInputs {
    pub snapshots:           Vec<ItemSnapshot>,
    pub sales:               Vec<SaleEvent>,
    pub baskets:             Vec<Basket>,
    pub price_changed_keys:  HashSet<ItemKey>,
}

/// Load all engine inputs for `org_id` over a window ending at `now`.
pub async fn load_inputs(
    pool:   &PgPool,
    org_id: Uuid,
    now:    DateTime<Utc>,
    config: &AnalysisConfig,
) -> Result<AdapterInputs, AppError> {
    let window_start = now - Duration::seconds((config.analysis_window_days * 86_400.0) as i64);

    // ── 1. Item snapshots ─────────────────────────────────────
    //
    // We expand each menu item into one row per active size (or "one_size" for
    // base-price items).  Cost is computed via the cost-per-serving formula:
    //   sum over recipe ingredients of (quantity_used × current cost_per_unit).
    //
    let snap_rows: Vec<MenuItemRow> = sqlx::query_as::<_, MenuItemRow>(
        r#"
        SELECT
            mi.id                                               AS menu_item_id,
            COALESCE(sz.label::text, 'one_size')               AS size_label,
            mi.name                                            AS item_name,
            mi.category_id,
            COALESCE(sz.price_override, mi.base_price)::int    AS current_price,
            mi.is_active,
            COALESCE(
                (
                    SELECT COALESCE(SUM(r.quantity_used * c.cost_per_unit), 0)
                    FROM   menu_item_recipes r
                    JOIN   org_ingredients  oi ON oi.id  = r.org_ingredient_id
                    JOIN   ingredient_cost_history c
                           ON  c.org_ingredient_id = r.org_ingredient_id
                           AND c.effective_until IS NULL          -- current epoch
                    WHERE  r.menu_item_id = mi.id
                      AND  r.size_label   = COALESCE(sz.label, (
                                                SELECT size_label
                                                FROM   menu_item_recipes
                                                WHERE  menu_item_id = mi.id
                                                LIMIT  1
                                           ))
                ),
                0
            )                                                  AS cost_per_serving
        FROM  menu_items mi
        LEFT JOIN item_sizes sz
               ON  sz.menu_item_id = mi.id
               AND sz.is_active    = true
        WHERE mi.org_id      = $1
          AND mi.deleted_at  IS NULL
        "#,
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?;

    let snapshots: Vec<ItemSnapshot> = snap_rows.into_iter().map(|r| {
        use rust_decimal::prelude::ToPrimitive;
        let cost: i64 = r.cost_per_serving
            .to_i64()
            .unwrap_or(0);

        ItemSnapshot {
            key: ItemKey {
                menu_item_id: r.menu_item_id,
                size_label:   r.size_label,
            },
            category_id:      r.category_id,
            name:             r.item_name,
            current_price:    r.current_price as i64,
            cost_per_serving: cost,
            is_active:        r.is_active,
            variant_of:       Some(r.menu_item_id), // all sizes share same parent
            bundle_only:      false,
        }
    }).collect();

    // ── 2. Sale events (cost-at-sale via epoch join) ──────────
    //
    // For each order_item in the window we compute the ingredient cost as it
    // stood *at the time of the sale* using the effective_from / effective_until
    // range in ingredient_cost_history.
    //
    let sale_rows: Vec<SaleRow> = sqlx::query_as::<_, SaleRow>(
        r#"
        SELECT
            oi.order_id                                         AS transaction_id,
            oi.menu_item_id,
            COALESCE(oi.size_label, 'one_size')                AS size_label,
            oi.quantity::bigint                                AS quantity_sold,
            oi.unit_price::int                                 AS unit_price_paid,
            COALESCE(
                (
                    SELECT COALESCE(SUM(r.quantity_used * c.cost_per_unit), 0)
                    FROM   menu_item_recipes r
                    JOIN   ingredient_cost_history c
                           ON  c.org_ingredient_id = r.org_ingredient_id
                           AND c.effective_from   <= o.created_at
                           AND (c.effective_until IS NULL OR c.effective_until > o.created_at)
                    WHERE  r.menu_item_id = oi.menu_item_id
                      AND  r.size_label   = COALESCE(
                                               oi.size_label,
                                               (SELECT size_label
                                                FROM   menu_item_recipes
                                                WHERE  menu_item_id = oi.menu_item_id
                                                LIMIT  1)
                                           )
                ),
                0
            )                                                  AS unit_cost_at_sale,
            o.created_at                                       AS sold_at
        FROM   order_items oi
        JOIN   orders      o  ON o.id = oi.order_id
        JOIN   branches    b  ON b.id = o.branch_id
        WHERE  b.org_id         = $1
          AND  o.created_at    >= $2
          AND  o.created_at    <= $3
          AND  o.status        = 'completed'
          AND  oi.menu_item_id IS NOT NULL
          AND  oi.bundle_id    IS NULL      -- exclude bundle order lines (counted separately)
        "#,
    )
    .bind(org_id)
    .bind(window_start)
    .bind(now)
    .fetch_all(pool)
    .await?;

    let sales: Vec<SaleEvent> = sale_rows.into_iter().map(|r| {
        use rust_decimal::prelude::ToPrimitive;
        let cost: i64 = r.unit_cost_at_sale.to_i64().unwrap_or(0);
        SaleEvent {
            transaction_id:   r.transaction_id,
            key: ItemKey {
                menu_item_id: r.menu_item_id,
                size_label:   r.size_label,
            },
            quantity_sold:    r.quantity_sold,
            unit_price_paid:  r.unit_price_paid as i64,
            unit_cost_at_sale: cost,
            sold_at:          r.sold_at,
        }
    }).collect();

    // ── 3. Baskets (one per completed order) ──────────────────

    // Fetch distinct (order_id, menu_item_id, size_label) tuples.
    #[derive(sqlx::FromRow)]
    struct BasketRow {
        order_id:     Uuid,
        menu_item_id: Uuid,
        size_label:   String,
    }

    let basket_rows: Vec<BasketRow> = sqlx::query_as::<_, BasketRow>(
        r#"
        SELECT DISTINCT
            oi.order_id,
            oi.menu_item_id,
            COALESCE(oi.size_label, 'one_size') AS size_label
        FROM   order_items oi
        JOIN   orders      o  ON o.id = oi.order_id
        JOIN   branches    b  ON b.id = o.branch_id
        WHERE  b.org_id         = $1
          AND  o.created_at    >= $2
          AND  o.created_at    <= $3
          AND  o.status        = 'completed'
          AND  oi.menu_item_id IS NOT NULL
        "#,
    )
    .bind(org_id)
    .bind(window_start)
    .bind(now)
    .fetch_all(pool)
    .await?;

    // Group by order_id → basket.
    let mut basket_map: HashMap<Uuid, Vec<ItemKey>> = HashMap::new();
    for row in basket_rows {
        basket_map
            .entry(row.order_id)
            .or_default()
            .push(ItemKey { menu_item_id: row.menu_item_id, size_label: row.size_label });
    }
    let baskets: Vec<Basket> = basket_map.into_values().collect();

    // ── 4. Detect items with price changes inside the window ──

    // Menu item price epochs that started within the window.
    #[derive(sqlx::FromRow)]
    struct PriceChangedRow {
        menu_item_id: Uuid,
        size_label:   Option<String>,
    }

    let changed_menu: Vec<PriceChangedRow> = sqlx::query_as::<_, PriceChangedRow>(
        r#"
        SELECT DISTINCT e.menu_item_id, e.size_label
        FROM   menu_item_price_epochs e
        JOIN   menu_items             mi ON mi.id = e.menu_item_id
        WHERE  mi.org_id       = $1
          AND  e.effective_from > $2
          AND  e.effective_from <= $3
        "#,
    )
    .bind(org_id)
    .bind(window_start)
    .bind(now)
    .fetch_all(pool)
    .await?;

    let mut price_changed_keys: HashSet<ItemKey> = HashSet::new();
    for r in changed_menu {
        price_changed_keys.insert(ItemKey {
            menu_item_id: r.menu_item_id,
            size_label:   r.size_label.unwrap_or_else(|| "one_size".into()),
        });
    }

    Ok(AdapterInputs {
        snapshots,
        sales,
        baskets,
        price_changed_keys,
    })
}
