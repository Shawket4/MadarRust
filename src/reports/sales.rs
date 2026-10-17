//! The sales reports that read the closed-day rollups (DB_ARCHITECTURE_AUDIT
//! §8 Phase 2.6): `GET /reports/branches/{id}/sales`, the daily and monthly
//! `…/sales/timeseries`, and `GET /reports/orgs/{id}/comparison`.
//!
//! Each report has two computations with one result:
//! * [`Source::Live`] — the original SQL over the ledger, kept as it was (and
//!   the reference the reconciliation compares against);
//! * [`Source::Rollups`] — `sales_daily_plan` splits the window into whole
//!   closed, covered, clean days (read from `sales_daily_sold*`) and live
//!   intervals for everything else (today, dirty days, partial days, days not
//!   yet covered); the two parts are added up in SQL.
//!
//! Both pick an item's translations the same deterministic way (the greatest
//! jsonb) and break ranking ties on the id, so the two are comparable byte for
//! byte; before, the pick and the tie order were whatever the plan produced.
//!
//! `MADAR_REPORTS_FROM_ROLLUPS=0` turns the rollup read off (live everywhere).
//! Hourly timeseries are always live (the rollups are per day).

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::sync::OnceLock;
use uuid::Uuid;

use super::handlers::{
    BranchComparison, BranchSalesReport, CategorySales, ItemSales, OrgComparisonReport,
    ServiceModeSales, TimeseriesPoint,
};
use crate::errors::AppError;

/// Where a report's figures come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Live,
    Rollups,
}

impl Source {
    /// What the handlers use: rollups unless `MADAR_REPORTS_FROM_ROLLUPS` is off.
    pub fn configured() -> Source {
        static V: OnceLock<bool> = OnceLock::new();
        let on = *V.get_or_init(|| {
            !matches!(
                std::env::var("MADAR_REPORTS_FROM_ROLLUPS").ok().as_deref(),
                Some("0" | "false" | "off")
            )
        });
        if on { Source::Rollups } else { Source::Live }
    }
}

/// One (category?, item) row, before grouping.
struct CategoryItemRow {
    category_id: Option<Uuid>,
    category_name: Option<String>,
    category_name_translations: Option<serde_json::Value>,
    menu_item_id: Uuid,
    item_name: String,
    item_name_translations: serde_json::Value,
    quantity_sold: i64,
    revenue: i64,
}

struct ModeRow {
    service_mode: String,
    orders: i64,
    revenue: i64,
}

/// The fourteen-odd totals of a branch sales report.
struct Totals {
    total_orders: i64,
    voided_orders: i64,
    subtotal: i64,
    total_discount: i64,
    total_tax: i64,
    total_revenue: i64,
    gross_sales: i64,
    refunded_amount: i64,
    total_service_charge: i64,
    service_charge_waived_count: i64,
    service_charge_waived_amount: i64,
    total_delivery_fees: i64,
    total_line_items: i64,
    revenue_by_method: serde_json::Value,
    total_tips: i64,
    cash_tips: i64,
}

/// `GET /reports/branches/{id}/sales` over `branch_ids`.
#[allow(clippy::too_many_arguments)]
pub async fn branch_sales_report(
    pool: &PgPool,
    branch_ids: &[Uuid],
    branch_id: Uuid,
    branch_name: String,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    limit: Option<i64>,
    exclude_items: Option<Vec<Uuid>>,
    source: Source,
) -> Result<BranchSalesReport, AppError> {
    // The screen wants a leaderboard and a spreadsheet wants the whole ranking,
    // and both come through here. A hundred was low enough that an export asking
    // for everything got the top hundred and no indication there was more; the
    // default is unchanged, so nothing that did not ask is affected.
    let item_limit = limit.unwrap_or(20).clamp(1, 1000);
    let ids = branch_ids.to_vec();
    let (totals, top_items, cat_rows, mode_rows) = match source {
        Source::Live => (
            live_totals(pool, &ids, from, to, &exclude_items).await?,
            live_top_items(pool, &ids, from, to, item_limit).await?,
            live_category_rows(pool, &ids, from, to).await?,
            live_modes(pool, &ids, from, to).await?,
        ),
        Source::Rollups => (
            rolled_totals(pool, &ids, from, to, &exclude_items).await?,
            rolled_top_items(pool, &ids, from, to, item_limit).await?,
            rolled_category_rows(pool, &ids, from, to).await?,
            rolled_modes(pool, &ids, from, to).await?,
        ),
    };
    Ok(BranchSalesReport {
        branch_id,
        branch_name,
        from,
        to,
        total_orders: totals.total_orders,
        voided_orders: totals.voided_orders,
        subtotal: totals.subtotal,
        total_discount: totals.total_discount,
        total_tax: totals.total_tax,
        total_revenue: totals.total_revenue,
        gross_sales: totals.gross_sales,
        refunded_amount: totals.refunded_amount,
        total_service_charge: totals.total_service_charge,
        service_charge_waived_count: totals.service_charge_waived_count,
        service_charge_waived_amount: totals.service_charge_waived_amount,
        total_delivery_fees: totals.total_delivery_fees,
        total_line_items: totals.total_line_items,
        revenue_by_method: totals.revenue_by_method,
        total_tips: totals.total_tips,
        cash_tips: totals.cash_tips,
        top_items,
        by_category: group_categories(cat_rows),
        by_service_mode: fill_modes(mode_rows),
    })
}

fn group_categories(cat_rows: Vec<CategoryItemRow>) -> Vec<CategorySales> {
    let mut by_category: Vec<CategorySales> = Vec::new();
    for row in cat_rows {
        let item = ItemSales {
            menu_item_id: row.menu_item_id,
            item_name: row.item_name,
            item_name_translations: row.item_name_translations,
            quantity_sold: row.quantity_sold,
            revenue: row.revenue,
        };
        match by_category
            .iter_mut()
            .find(|c| c.category_id == row.category_id)
        {
            Some(cat) => {
                cat.item_count += 1;
                cat.quantity_sold += item.quantity_sold;
                cat.revenue += item.revenue;
                cat.items.push(item);
            }
            None => {
                by_category.push(CategorySales {
                    category_id: row.category_id,
                    category_name: row.category_name,
                    category_name_translations: row.category_name_translations,
                    item_count: 1,
                    quantity_sold: item.quantity_sold,
                    revenue: item.revenue,
                    items: vec![item],
                });
            }
        }
    }
    by_category
}

/// Both modes, always, in a fixed order: a screen should not have to guess
/// that a missing row means zero.
fn fill_modes(mode_rows: Vec<ModeRow>) -> Vec<ServiceModeSales> {
    ["takeaway", "dine_in"]
        .into_iter()
        .map(|mode| {
            mode_rows
                .iter()
                .find(|r| r.service_mode == mode)
                .map(|r| ServiceModeSales {
                    service_mode: r.service_mode.clone(),
                    orders: r.orders,
                    revenue: r.revenue,
                })
                .unwrap_or(ServiceModeSales {
                    service_mode: mode.into(),
                    orders: 0,
                    revenue: 0,
                })
        })
        .collect()
}

// ── live: the reports' original SQL ─────────────────────────────────────────

async fn live_totals(
    pool: &PgPool,
    ids: &[Uuid],
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    exclude_items: &Option<Vec<Uuid>>,
) -> Result<Totals, AppError> {
    let r = sqlx::query!(
        r#"
        SELECT
            COUNT(*) FILTER (WHERE o.status NOT IN ('voided', 'refunded'))::bigint AS "total_orders!",
            COUNT(*) FILTER (WHERE o.status = 'voided')::bigint AS "voided_orders!",
            COALESCE(SUM(o.subtotal)        FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS "subtotal!",
            COALESCE(SUM(o.discount_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS "total_discount!",
            COALESCE(SUM(o.tax_amount - COALESCE(rf.refunded_tax, 0)) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS "total_tax!",
            -- Net of refunds against each sale; the two lines after it are the
            -- identity `gross_sales − refunded_amount = total_revenue`. The join
            -- on v_order_refund_totals is one row per order, so nothing here
            -- fans out — the V17 lesson still holds.
            COALESCE(SUM(o.total_amount - COALESCE(rf.refunded_amount, 0))
                     FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS "total_revenue!",
            COALESCE(SUM(o.total_amount)    FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS "gross_sales!",
            COALESCE(SUM(rf.refunded_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS "refunded_amount!",
            COALESCE(SUM(o.service_charge_amount - COALESCE(rf.refunded_service_charge, 0)) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS "total_service_charge!",
            COUNT(o.id) FILTER (WHERE o.status NOT IN ('voided', 'refunded') AND o.service_charge_waived_by IS NOT NULL)::bigint AS "service_charge_waived_count!",
            COALESCE(SUM(o.service_charge_waived_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS "service_charge_waived_amount!",
            COALESCE(SUM(o.delivery_fee)    FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS "total_delivery_fees!",
            COALESCE((
              SELECT SUM(oi.quantity)::bigint
              FROM order_items oi
              JOIN orders o3 ON o3.id = oi.order_id
              WHERE o3.branch_id = ANY($1) AND o3.status NOT IN ('voided', 'refunded')
                AND oi.line_kind <> 'combo'
                AND ($2::timestamptz IS NULL OR o3.created_at >= $2)
                AND ($3::timestamptz IS NULL OR o3.created_at <= $3)
                AND ($4::uuid[] IS NULL OR oi.menu_item_id != ALL($4::uuid[]))
            ), 0)::bigint AS "total_line_items!",
            COALESCE((
              SELECT json_object_agg(method, rev) FROM (
                SELECT op.method, SUM(op.amount)::bigint AS rev
                FROM order_payments op
                JOIN orders o2 ON o2.id = op.order_id
                WHERE o2.branch_id = ANY($1) AND o2.status NOT IN ('voided', 'refunded')
                  AND ($2::timestamptz IS NULL OR o2.created_at >= $2)
                  AND ($3::timestamptz IS NULL OR o2.created_at <= $3)
                GROUP BY op.method
              ) sub
            ), '{}'::json) AS "revenue_by_method!: serde_json::Value",
            -- Tips, standalone. Deliberately NOT added into total_amount or into
            -- any method bucket above: `revenue_by_method` is goods-only so it
            -- lines up with the shift report's payment_summary.
            COALESCE(SUM(o.tip_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS "total_tips!",
            COALESCE(SUM(o.tip_amount) FILTER (
                WHERE o.status NOT IN ('voided', 'refunded')
                  AND COALESCE(o.tip_is_cash,
                               COALESCE(o.tip_payment_method, o.payment_method) = 'cash')
            ), 0)::bigint AS "cash_tips!"
        FROM orders o
        LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id
        WHERE o.branch_id = ANY($1)
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        "#,
        ids,
        from,
        to,
        exclude_items.as_deref(),
    )
    .fetch_one(pool)
    .await?;
    Ok(Totals {
        total_orders: r.total_orders,
        voided_orders: r.voided_orders,
        subtotal: r.subtotal,
        total_discount: r.total_discount,
        total_tax: r.total_tax,
        total_revenue: r.total_revenue,
        gross_sales: r.gross_sales,
        refunded_amount: r.refunded_amount,
        total_service_charge: r.total_service_charge,
        service_charge_waived_count: r.service_charge_waived_count,
        service_charge_waived_amount: r.service_charge_waived_amount,
        total_delivery_fees: r.total_delivery_fees,
        total_line_items: r.total_line_items,
        revenue_by_method: r.revenue_by_method,
        total_tips: r.total_tips,
        cash_tips: r.cash_tips,
    })
}

async fn live_top_items(
    pool: &PgPool,
    ids: &[Uuid],
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    limit: i64,
) -> Result<Vec<ItemSales>, AppError> {
    Ok(sqlx::query_as!(
        ItemSales,
        r#"
        SELECT oi.menu_item_id AS "menu_item_id!", oi.item_name,
               COALESCE((array_agg(oi.name_translations ORDER BY oi.name_translations DESC))[1], '{}'::jsonb) AS "item_name_translations!",
               SUM(oi.quantity)::bigint   AS "quantity_sold!",
               SUM(oi.line_total)::bigint AS "revenue!"
        FROM order_items oi
        JOIN orders o ON o.id = oi.order_id
        WHERE o.branch_id = ANY($1) AND o.status NOT IN ('voided', 'refunded')
          AND oi.line_kind <> 'combo'
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY oi.menu_item_id, oi.item_name
        -- One ranking everywhere (the POS metrics and the POS core too):
        -- quantity, then revenue, then name.
        ORDER BY 4 DESC, 5 DESC, oi.item_name COLLATE "C", oi.menu_item_id
        LIMIT $4
        "#,
        ids,
        from,
        to,
        limit,
    )
    .fetch_all(pool)
    .await?)
}

async fn live_category_rows(
    pool: &PgPool,
    ids: &[Uuid],
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<Vec<CategoryItemRow>, AppError> {
    Ok(sqlx::query_as!(
        CategoryItemRow,
        r#"
        SELECT
            m.category_id AS "category_id?",
            COALESCE(c.name, 'Uncategorized') AS "category_name?",
            (array_agg(c.name_translations))[1] AS "category_name_translations?",
            oi.menu_item_id AS "menu_item_id!",
            oi.item_name,
            COALESCE((array_agg(oi.name_translations ORDER BY oi.name_translations DESC))[1], '{}'::jsonb) AS "item_name_translations!",
            SUM(oi.quantity)::bigint   AS "quantity_sold!",
            SUM(oi.line_total)::bigint AS "revenue!"
        FROM order_items oi
        JOIN orders o     ON o.id  = oi.order_id
        LEFT JOIN menu_items m ON m.id  = oi.menu_item_id
        LEFT JOIN categories c ON c.id = m.category_id
        WHERE o.branch_id = ANY($1) AND o.status NOT IN ('voided', 'refunded')
          AND oi.line_kind <> 'combo'
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY
            m.category_id,
            COALESCE(c.name, 'Uncategorized'),
            oi.menu_item_id,
            oi.item_name
        ORDER BY 2 NULLS LAST, 7 DESC, 8 DESC, oi.item_name COLLATE "C", oi.menu_item_id
        "#,
        ids,
        from,
        to,
    )
    .fetch_all(pool)
    .await?)
}

async fn live_modes(
    pool: &PgPool,
    ids: &[Uuid],
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<Vec<ModeRow>, AppError> {
    // `crate::orders::SOLD`, spelled out for the compile-time check.
    Ok(sqlx::query_as!(
        ModeRow,
        r#"
        SELECT o.service_mode,
               COUNT(*)::bigint AS "orders!",
               COALESCE(SUM(o.total_amount - COALESCE(rf.refunded_amount, 0)), 0)::bigint AS "revenue!"
        FROM orders o
        LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id
        WHERE o.branch_id = ANY($1)
          AND o.status::text NOT IN ('voided', 'refunded')
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY o.service_mode
        "#,
        ids,
        from,
        to,
    )
    .fetch_all(pool)
    .await?)
}

// ── rollups: closed days from sales_daily_sold*, the rest live ─────────────
//
// Every query starts from the same plan: `p` = sales_daily_plan(...), `w` =
// its live intervals [lo, hi), `d` = its rollup days (one array per branch).
// The live half is the original SQL with the window replaced by `w`.
//
// Both halves are read through `CROSS JOIN LATERAL (… OFFSET 0)`, never a
// plain join on `w`/`d`. The planner sees the plan function as 1000 opaque
// rows and cannot turn `created_at >= w.lo AND created_at < w.hi` into an
// index range, so a plain join hash-joined against a SEQ SCAN of the org's
// whole ledger (and of every rollup row of the org): ~5 ms per scan, up to
// three scans per statement, for any window — slower than the live report.
// The OFFSET 0 fence stops the subquery being flattened back into that join,
// so each live interval is an index range on (branch_id, created_at) and each
// branch's rollup days one index probe on the (branch_id, business_date) key.

async fn rolled_totals(
    pool: &PgPool,
    ids: &[Uuid],
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    exclude_items: &Option<Vec<Uuid>>,
) -> Result<Totals, AppError> {
    let r = sqlx::query!(
        r#"
        WITH p AS (SELECT * FROM sales_daily_plan($1::uuid[], $2::timestamptz, $3::timestamptz, true)),
        w AS (SELECT p.branch_id, p.lo, p.hi FROM p WHERE p.day IS NULL),
        d AS (SELECT p.branch_id, array_agg(p.day) AS days FROM p WHERE p.day IS NOT NULL GROUP BY p.branch_id),
        lo AS (
            SELECT o.* FROM w CROSS JOIN LATERAL (
                SELECT o.id, o.status, o.subtotal, o.discount_amount, o.tax_amount, o.total_amount,
                       o.service_charge_amount, o.service_charge_waived_by, o.service_charge_waived_amount,
                       o.delivery_fee, o.tip_amount, o.tip_is_cash, o.tip_payment_method, o.payment_method
                  FROM orders o
                 WHERE o.branch_id = w.branch_id AND o.created_at >= w.lo AND o.created_at < w.hi
                OFFSET 0
            ) o
        ),
        live AS (
            SELECT
                COUNT(*) FILTER (WHERE o.status NOT IN ('voided', 'refunded'))::bigint AS total_orders,
                COUNT(*) FILTER (WHERE o.status = 'voided')::bigint AS voided_orders,
                COALESCE(SUM(o.subtotal)        FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS subtotal,
                COALESCE(SUM(o.discount_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS total_discount,
                COALESCE(SUM(o.tax_amount - COALESCE(rf.refunded_tax, 0)) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS total_tax,
                COALESCE(SUM(o.total_amount - COALESCE(rf.refunded_amount, 0))
                         FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS total_revenue,
                COALESCE(SUM(o.total_amount)    FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS gross_sales,
                COALESCE(SUM(rf.refunded_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS refunded_amount,
                COALESCE(SUM(o.service_charge_amount - COALESCE(rf.refunded_service_charge, 0)) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS total_service_charge,
                COUNT(o.id) FILTER (WHERE o.status NOT IN ('voided', 'refunded') AND o.service_charge_waived_by IS NOT NULL)::bigint AS sc_waived_count,
                COALESCE(SUM(o.service_charge_waived_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS sc_waived_amount,
                COALESCE(SUM(o.delivery_fee)    FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS delivery_fees,
                COALESCE(SUM(o.tip_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS tips,
                COALESCE(SUM(o.tip_amount) FILTER (
                    WHERE o.status NOT IN ('voided', 'refunded')
                      AND COALESCE(o.tip_is_cash,
                                   COALESCE(o.tip_payment_method, o.payment_method) = 'cash')
                ), 0)::bigint AS cash_tips
            FROM lo o
            LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id
        ),
        roll AS (
            SELECT COALESCE(SUM(s.sold_orders), 0)::bigint AS total_orders,
                   COALESCE(SUM(s.voided_orders), 0)::bigint AS voided_orders,
                   COALESCE(SUM(s.subtotal), 0)::bigint AS subtotal,
                   COALESCE(SUM(s.discount), 0)::bigint AS total_discount,
                   COALESCE(SUM(s.tax_net), 0)::bigint AS total_tax,
                   COALESCE(SUM(s.revenue_net), 0)::bigint AS total_revenue,
                   COALESCE(SUM(s.gross_sales), 0)::bigint AS gross_sales,
                   COALESCE(SUM(s.refunded_amount), 0)::bigint AS refunded_amount,
                   COALESCE(SUM(s.service_charge_net), 0)::bigint AS total_service_charge,
                   COALESCE(SUM(s.sc_waived_count), 0)::bigint AS sc_waived_count,
                   COALESCE(SUM(s.sc_waived_amount), 0)::bigint AS sc_waived_amount,
                   COALESCE(SUM(s.delivery_fees), 0)::bigint AS delivery_fees,
                   COALESCE(SUM(s.tips), 0)::bigint AS tips,
                   COALESCE(SUM(s.cash_tips), 0)::bigint AS cash_tips
              FROM d CROSS JOIN LATERAL (SELECT s.* FROM sales_daily_sold s WHERE s.branch_id = d.branch_id AND s.business_date = ANY(d.days) OFFSET 0) s
        ),
        line_items AS (
            SELECT COALESCE(SUM(q), 0)::bigint AS n FROM (
                SELECT oi.quantity::bigint AS q
                  FROM w CROSS JOIN LATERAL (SELECT o3.* FROM orders o3 WHERE o3.branch_id = w.branch_id AND o3.created_at >= w.lo AND o3.created_at < w.hi OFFSET 0) o3
                  JOIN order_items oi ON oi.order_id = o3.id
                 WHERE o3.status NOT IN ('voided', 'refunded') AND oi.line_kind <> 'combo'
                   AND ($4::uuid[] IS NULL OR oi.menu_item_id != ALL($4::uuid[]))
                UNION ALL
                SELECT si.quantity
                  FROM d CROSS JOIN LATERAL (SELECT si.* FROM sales_daily_sold_item si WHERE si.branch_id = d.branch_id AND si.business_date = ANY(d.days) OFFSET 0) si
                 WHERE ($4::uuid[] IS NULL OR si.menu_item_id != ALL($4::uuid[]))
            ) x
        ),
        methods AS (
            SELECT json_object_agg(method, rev) AS j FROM (
                SELECT method, SUM(amount)::bigint AS rev FROM (
                    SELECT op.method, op.amount::bigint AS amount
                      FROM w CROSS JOIN LATERAL (SELECT o2.* FROM orders o2 WHERE o2.branch_id = w.branch_id AND o2.created_at >= w.lo AND o2.created_at < w.hi OFFSET 0) o2
                      JOIN order_payments op ON op.order_id = o2.id
                     WHERE o2.status NOT IN ('voided', 'refunded')
                    UNION ALL
                    SELECT sm.method, sm.amount
                      FROM d CROSS JOIN LATERAL (SELECT sm.* FROM sales_daily_sold_method sm WHERE sm.branch_id = d.branch_id AND sm.business_date = ANY(d.days) OFFSET 0) sm
                ) u GROUP BY method
            ) sub
        )
        SELECT (live.total_orders + roll.total_orders)::bigint AS "total_orders!",
               (live.voided_orders + roll.voided_orders)::bigint AS "voided_orders!",
               (live.subtotal + roll.subtotal)::bigint AS "subtotal!",
               (live.total_discount + roll.total_discount)::bigint AS "total_discount!",
               (live.total_tax + roll.total_tax)::bigint AS "total_tax!",
               (live.total_revenue + roll.total_revenue)::bigint AS "total_revenue!",
               (live.gross_sales + roll.gross_sales)::bigint AS "gross_sales!",
               (live.refunded_amount + roll.refunded_amount)::bigint AS "refunded_amount!",
               (live.total_service_charge + roll.total_service_charge)::bigint AS "total_service_charge!",
               (live.sc_waived_count + roll.sc_waived_count)::bigint AS "service_charge_waived_count!",
               (live.sc_waived_amount + roll.sc_waived_amount)::bigint AS "service_charge_waived_amount!",
               (live.delivery_fees + roll.delivery_fees)::bigint AS "total_delivery_fees!",
               line_items.n AS "total_line_items!",
               COALESCE(methods.j, '{}'::json) AS "revenue_by_method!: serde_json::Value",
               (live.tips + roll.tips)::bigint AS "total_tips!",
               (live.cash_tips + roll.cash_tips)::bigint AS "cash_tips!"
          FROM live, roll, line_items, methods
        "#,
        ids,
        from,
        to,
        exclude_items.as_deref(),
    )
    .fetch_one(pool)
    .await?;
    Ok(Totals {
        total_orders: r.total_orders,
        voided_orders: r.voided_orders,
        subtotal: r.subtotal,
        total_discount: r.total_discount,
        total_tax: r.total_tax,
        total_revenue: r.total_revenue,
        gross_sales: r.gross_sales,
        refunded_amount: r.refunded_amount,
        total_service_charge: r.total_service_charge,
        service_charge_waived_count: r.service_charge_waived_count,
        service_charge_waived_amount: r.service_charge_waived_amount,
        total_delivery_fees: r.total_delivery_fees,
        total_line_items: r.total_line_items,
        revenue_by_method: r.revenue_by_method,
        total_tips: r.total_tips,
        cash_tips: r.cash_tips,
    })
}

async fn rolled_top_items(
    pool: &PgPool,
    ids: &[Uuid],
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    limit: i64,
) -> Result<Vec<ItemSales>, AppError> {
    Ok(sqlx::query_as!(
        ItemSales,
        r#"
        WITH p AS (SELECT * FROM sales_daily_plan($1::uuid[], $2::timestamptz, $3::timestamptz, true)),
        w AS (SELECT p.branch_id, p.lo, p.hi FROM p WHERE p.day IS NULL),
        d AS (SELECT p.branch_id, array_agg(p.day) AS days FROM p WHERE p.day IS NOT NULL GROUP BY p.branch_id),
        rows AS (
            SELECT oi.menu_item_id, oi.item_name, oi.name_translations AS tr,
                   oi.quantity::bigint AS q, oi.line_total::bigint AS r
              FROM w CROSS JOIN LATERAL (SELECT o.* FROM orders o WHERE o.branch_id = w.branch_id AND o.created_at >= w.lo AND o.created_at < w.hi OFFSET 0) o
              JOIN order_items oi ON oi.order_id = o.id
             WHERE o.status NOT IN ('voided', 'refunded') AND oi.line_kind <> 'combo'
            UNION ALL
            SELECT si.menu_item_id, si.item_name, si.name_translations, si.quantity, si.revenue
              FROM d CROSS JOIN LATERAL (SELECT si.* FROM sales_daily_sold_item si WHERE si.branch_id = d.branch_id AND si.business_date = ANY(d.days) OFFSET 0) si
        )
        SELECT menu_item_id AS "menu_item_id!", item_name AS "item_name!",
               COALESCE((array_agg(tr ORDER BY tr DESC))[1], '{}'::jsonb) AS "item_name_translations!",
               SUM(q)::bigint AS "quantity_sold!",
               SUM(r)::bigint AS "revenue!"
          FROM rows
         GROUP BY menu_item_id, item_name
         ORDER BY 4 DESC, 5 DESC, item_name COLLATE "C", menu_item_id
         LIMIT $4
        "#,
        ids,
        from,
        to,
        limit,
    )
    .fetch_all(pool)
    .await?)
}

async fn rolled_category_rows(
    pool: &PgPool,
    ids: &[Uuid],
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<Vec<CategoryItemRow>, AppError> {
    Ok(sqlx::query_as!(
        CategoryItemRow,
        r#"
        WITH p AS (SELECT * FROM sales_daily_plan($1::uuid[], $2::timestamptz, $3::timestamptz, true)),
        w AS (SELECT p.branch_id, p.lo, p.hi FROM p WHERE p.day IS NULL),
        d AS (SELECT p.branch_id, array_agg(p.day) AS days FROM p WHERE p.day IS NOT NULL GROUP BY p.branch_id),
        rows AS (
            SELECT oi.menu_item_id, oi.item_name, oi.name_translations AS tr,
                   oi.quantity::bigint AS q, oi.line_total::bigint AS r
              FROM w CROSS JOIN LATERAL (SELECT o.* FROM orders o WHERE o.branch_id = w.branch_id AND o.created_at >= w.lo AND o.created_at < w.hi OFFSET 0) o
              JOIN order_items oi ON oi.order_id = o.id
             WHERE o.status NOT IN ('voided', 'refunded') AND oi.line_kind <> 'combo'
            UNION ALL
            SELECT si.menu_item_id, si.item_name, si.name_translations, si.quantity, si.revenue
              FROM d CROSS JOIN LATERAL (SELECT si.* FROM sales_daily_sold_item si WHERE si.branch_id = d.branch_id AND si.business_date = ANY(d.days) OFFSET 0) si
        )
        SELECT
            m.category_id AS "category_id?",
            COALESCE(c.name, 'Uncategorized') AS "category_name?",
            (array_agg(c.name_translations))[1] AS "category_name_translations?",
            x.menu_item_id AS "menu_item_id!",
            x.item_name AS "item_name!",
            COALESCE((array_agg(x.tr ORDER BY x.tr DESC))[1], '{}'::jsonb) AS "item_name_translations!",
            SUM(x.q)::bigint AS "quantity_sold!",
            SUM(x.r)::bigint AS "revenue!"
          FROM rows x
          LEFT JOIN menu_items m ON m.id = x.menu_item_id
          LEFT JOIN categories c ON c.id = m.category_id
         GROUP BY m.category_id, COALESCE(c.name, 'Uncategorized'), x.menu_item_id, x.item_name
         ORDER BY 2 NULLS LAST, 7 DESC, 8 DESC, x.item_name COLLATE "C", x.menu_item_id
        "#,
        ids,
        from,
        to,
    )
    .fetch_all(pool)
    .await?)
}

async fn rolled_modes(
    pool: &PgPool,
    ids: &[Uuid],
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<Vec<ModeRow>, AppError> {
    Ok(sqlx::query_as!(
        ModeRow,
        r#"
        WITH p AS (SELECT * FROM sales_daily_plan($1::uuid[], $2::timestamptz, $3::timestamptz, true)),
        w AS (SELECT p.branch_id, p.lo, p.hi FROM p WHERE p.day IS NULL),
        d AS (SELECT p.branch_id, array_agg(p.day) AS days FROM p WHERE p.day IS NOT NULL GROUP BY p.branch_id),
        rows AS (
            SELECT o.service_mode, 1::bigint AS n,
                   (o.total_amount - COALESCE(rf.refunded_amount, 0))::bigint AS rev
              FROM w CROSS JOIN LATERAL (SELECT o.* FROM orders o WHERE o.branch_id = w.branch_id AND o.created_at >= w.lo AND o.created_at < w.hi OFFSET 0) o
              LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id
             WHERE o.status::text NOT IN ('voided', 'refunded')
            UNION ALL
            SELECT sm.service_mode, sm.orders, sm.revenue
              FROM d CROSS JOIN LATERAL (SELECT sm.* FROM sales_daily_sold_mode sm WHERE sm.branch_id = d.branch_id AND sm.business_date = ANY(d.days) OFFSET 0) sm
        )
        SELECT service_mode AS "service_mode!", SUM(n)::bigint AS "orders!",
               COALESCE(SUM(rev), 0)::bigint AS "revenue!"
          FROM rows GROUP BY service_mode
        "#,
        ids,
        from,
        to,
    )
    .fetch_all(pool)
    .await?)
}

// ── timeseries ──────────────────────────────────────────────────────────────

/// `GET /reports/branches/{id}/sales/timeseries` over `branch_ids`, bucketed in
/// `tz` at `granularity` ("hourly" | "daily" | "monthly"). Hourly is always
/// live; so is any branch whose own zone is not `tz` (its business days do not
/// line up with the buckets).
pub async fn sales_timeseries(
    pool: &PgPool,
    branch_ids: &[Uuid],
    tz: &str,
    granularity: Option<&str>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    source: Source,
) -> Result<Vec<TimeseriesPoint>, AppError> {
    let trunc = match granularity.unwrap_or("daily") {
        "hourly" => "hour",
        "monthly" => "month",
        _ => "day",
    };
    if source == Source::Live || trunc == "hour" {
        return live_timeseries(pool, branch_ids, tz, trunc, from, to).await;
    }
    Ok(sqlx::query_as!(
        TimeseriesPoint,
        r#"
        WITH p AS (SELECT * FROM sales_daily_plan($1::uuid[], $2::timestamptz, $3::timestamptz, true, $4)),
        w AS (SELECT p.branch_id, p.lo, p.hi FROM p WHERE p.day IS NULL),
        d AS (SELECT p.branch_id, array_agg(p.day) AS days FROM p WHERE p.day IS NOT NULL GROUP BY p.branch_id),
        per AS (
            SELECT date_trunc($5, o.created_at AT TIME ZONE $4) AS pv,
                   COUNT(o.id) FILTER (WHERE o.status NOT IN ('voided', 'refunded'))::bigint AS orders,
                   COALESCE(SUM(o.total_amount - COALESCE(rf.refunded_amount, 0))
                            FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS revenue,
                   COALESCE(SUM(rf.refunded_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS refunded,
                   COUNT(o.id) FILTER (WHERE o.status = 'voided')::bigint AS voided,
                   COALESCE(SUM(o.discount_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS discount,
                   COALESCE(SUM(o.tax_amount - COALESCE(rf.refunded_tax, 0)) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS tax,
                   0::bigint AS line_items, 0::bigint AS addons
              FROM w CROSS JOIN LATERAL (SELECT o.* FROM orders o WHERE o.branch_id = w.branch_id AND o.created_at >= w.lo AND o.created_at < w.hi OFFSET 0) o
              LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id
             GROUP BY 1
            UNION ALL
            SELECT date_trunc($5, o3.created_at AT TIME ZONE $4), 0, 0, 0, 0, 0, 0,
                   COALESCE(SUM(oi3.quantity), 0)::bigint, 0
              FROM w CROSS JOIN LATERAL (SELECT o3.* FROM orders o3 WHERE o3.branch_id = w.branch_id AND o3.created_at >= w.lo AND o3.created_at < w.hi OFFSET 0) o3
              JOIN order_items oi3 ON oi3.order_id = o3.id
             WHERE oi3.line_kind <> 'combo' AND o3.status NOT IN ('voided', 'refunded')
             GROUP BY 1
            UNION ALL
            SELECT date_trunc($5, o4.created_at AT TIME ZONE $4), 0, 0, 0, 0, 0, 0, 0,
                   COALESCE(SUM(oia4.quantity), 0)::bigint
              FROM w CROSS JOIN LATERAL (SELECT o4.* FROM orders o4 WHERE o4.branch_id = w.branch_id AND o4.created_at >= w.lo AND o4.created_at < w.hi OFFSET 0) o4
              JOIN order_items oi4 ON oi4.order_id = o4.id
              JOIN order_item_addons oia4 ON oia4.order_item_id = oi4.id
             WHERE o4.status NOT IN ('voided', 'refunded')
             GROUP BY 1
            UNION ALL
            SELECT date_trunc($5, s.business_date::timestamp), SUM(s.sold_orders)::bigint,
                   SUM(s.revenue_net)::bigint, SUM(s.refunded_amount)::bigint, SUM(s.voided_orders)::bigint,
                   SUM(s.discount)::bigint, SUM(s.tax_net)::bigint, SUM(s.line_items)::bigint, SUM(s.addons)::bigint
              FROM d CROSS JOIN LATERAL (SELECT s.* FROM sales_daily_sold s WHERE s.branch_id = d.branch_id AND s.business_date = ANY(d.days) OFFSET 0) s
             GROUP BY 1
        ),
        periods AS (
            SELECT pv, SUM(orders)::bigint AS orders, SUM(revenue)::bigint AS revenue,
                   SUM(refunded)::bigint AS refunded, SUM(voided)::bigint AS voided,
                   SUM(discount)::bigint AS discount, SUM(tax)::bigint AS tax,
                   SUM(line_items)::bigint AS line_items, SUM(addons)::bigint AS addons
              FROM per GROUP BY pv
        ),
        -- A period exists when it had an order: the live half by its orders,
        -- the rollup half by its day rows (a row exists only for a day with one).
        present AS (
            SELECT DISTINCT date_trunc($5, o.created_at AT TIME ZONE $4) AS pv
              FROM w CROSS JOIN LATERAL (SELECT o.* FROM orders o WHERE o.branch_id = w.branch_id AND o.created_at >= w.lo AND o.created_at < w.hi OFFSET 0) o
            UNION
            SELECT DISTINCT date_trunc($5, s.business_date::timestamp)
              FROM d CROSS JOIN LATERAL (SELECT s.* FROM sales_daily_sold s WHERE s.branch_id = d.branch_id AND s.business_date = ANY(d.days) OFFSET 0) s
        ),
        methods AS (
            SELECT pv, json_object_agg(method, rev) AS revenue_by_method FROM (
                SELECT pv, method, SUM(amount)::bigint AS rev FROM (
                    SELECT date_trunc($5, o2.created_at AT TIME ZONE $4) AS pv, op2.method, op2.amount::bigint AS amount
                      FROM w CROSS JOIN LATERAL (SELECT o2.* FROM orders o2 WHERE o2.branch_id = w.branch_id AND o2.created_at >= w.lo AND o2.created_at < w.hi OFFSET 0) o2
                      JOIN order_payments op2 ON op2.order_id = o2.id
                     WHERE o2.status NOT IN ('voided', 'refunded')
                    UNION ALL
                    SELECT date_trunc($5, sm.business_date::timestamp), sm.method, sm.amount
                      FROM d CROSS JOIN LATERAL (SELECT sm.* FROM sales_daily_sold_method sm WHERE sm.branch_id = d.branch_id AND sm.business_date = ANY(d.days) OFFSET 0) sm
                ) u GROUP BY 1, 2
            ) m GROUP BY pv
        )
        SELECT to_char(pr.pv, 'YYYY-MM-DD"T"HH24:MI:SS') AS "period!",
               COALESCE(p2.orders, 0)::bigint AS "orders!",
               COALESCE(p2.revenue, 0)::bigint AS "revenue!",
               COALESCE(p2.refunded, 0)::bigint AS "refunded!",
               COALESCE(p2.voided, 0)::bigint AS "voided!",
               COALESCE(p2.discount, 0)::bigint AS "discount!",
               COALESCE(p2.tax, 0)::bigint AS "tax!",
               COALESCE(m.revenue_by_method, '{}'::json) AS "revenue_by_method!: serde_json::Value",
               COALESCE(p2.line_items, 0)::bigint AS "line_items!",
               COALESCE(p2.addons, 0)::bigint AS "addons!"
          FROM present pr
          LEFT JOIN periods p2 ON p2.pv = pr.pv
          LEFT JOIN methods m ON m.pv = pr.pv
         ORDER BY pr.pv ASC
        "#,
        branch_ids,
        from,
        to,
        tz,
        trunc,
    )
    .fetch_all(pool)
    .await?)
}

/// The original timeseries SQL (unchanged).
async fn live_timeseries(
    pool: &PgPool,
    branch_ids: &[Uuid],
    tz: &str,
    trunc: &str,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<Vec<TimeseriesPoint>, AppError> {
    // `trunc` is an enum whitelist (closed match above) so it is safe to
    // interpolate. `tz` is a DB value but originates as unvalidated free text
    // on the branch, so it MUST be bound ($4), never interpolated — otherwise a
    // crafted branch timezone is a stored SQL injection.
    let sql = format!(
        r#"
        WITH periods AS (
            SELECT
                date_trunc('{trunc}', o.created_at AT TIME ZONE $4) AS period_val,
                to_char(
                    date_trunc('{trunc}', o.created_at AT TIME ZONE $4),
                    'YYYY-MM-DD"T"HH24:MI:SS'
                ) AS period_str,
                COUNT(o.id)   FILTER (WHERE o.status NOT IN ('voided', 'refunded'))::bigint  AS orders,
                -- Net of refunds, attributed to the period the SALE fell in.
                COALESCE(SUM(o.total_amount - COALESCE(rf.refunded_amount, 0))
                         FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS revenue,
                COALESCE(SUM(rf.refunded_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS refunded,
                COUNT(o.id)   FILTER (WHERE o.status  = 'voided')::bigint  AS voided,
                COALESCE(SUM(o.discount_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS discount,
                COALESCE(SUM(o.tax_amount - COALESCE(rf.refunded_tax, 0)) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS tax
            FROM orders o
            LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id
            WHERE o.branch_id = ANY($1)
              AND ($2::timestamptz IS NULL OR o.created_at >= $2)
              AND ($3::timestamptz IS NULL OR o.created_at <= $3)
            GROUP BY date_trunc('{trunc}', o.created_at AT TIME ZONE $4)
        ),
        -- Payment mix, bucketed ONCE for the whole range.
        --
        -- This used to be a correlated subquery in the SELECT list, which
        -- re-ran per period row: a 120-day chart executed it 120 times, and
        -- because the correlation predicate is `date_trunc(...) = p.period_val`
        -- — a function over the column — no index could serve it, so each run
        -- was a full scan of `orders`. Measured on 30k orders it cost ~730ms
        -- and ~3.6M row visits to draw one chart; as a single grouped pass it
        -- is ~27ms with byte-identical output. On the production box (1 vCPU,
        -- Postgres co-resident) the old shape was the dashboard's slowest read.
        methods AS (
            SELECT
                date_trunc('{trunc}', o2.created_at AT TIME ZONE $4) AS period_val,
                op2.method,
                SUM(op2.amount)::bigint AS rev
            FROM order_payments op2
            JOIN orders o2 ON o2.id = op2.order_id
            WHERE o2.branch_id = ANY($1)
              AND o2.status NOT IN ('voided', 'refunded')
              AND ($2::timestamptz IS NULL OR o2.created_at >= $2)
              AND ($3::timestamptz IS NULL OR o2.created_at <= $3)
            GROUP BY 1, 2
        ),
        methods_by_period AS (
            SELECT period_val, json_object_agg(method, rev) AS revenue_by_method
            FROM methods
            GROUP BY period_val
        ),
        -- order_items/order_item_addons fan out orders, so they're summed in
        -- their own bucketed CTEs rather than joined into `periods` above.
        line_item_totals AS (
            SELECT
                date_trunc('{trunc}', o3.created_at AT TIME ZONE $4) AS period_val,
                COALESCE(SUM(oi3.quantity), 0)::bigint AS line_items
            FROM order_items oi3
            JOIN orders o3 ON o3.id = oi3.order_id
            WHERE o3.branch_id = ANY($1)
              AND oi3.line_kind <> 'combo'
              AND o3.status NOT IN ('voided', 'refunded')
              AND ($2::timestamptz IS NULL OR o3.created_at >= $2)
              AND ($3::timestamptz IS NULL OR o3.created_at <= $3)
            GROUP BY 1
        ),
        addon_totals AS (
            SELECT
                date_trunc('{trunc}', o4.created_at AT TIME ZONE $4) AS period_val,
                COALESCE(SUM(oia4.quantity), 0)::bigint AS addons
            FROM order_item_addons oia4
            JOIN order_items oi4 ON oi4.id = oia4.order_item_id
            JOIN orders o4 ON o4.id = oi4.order_id
            WHERE o4.branch_id = ANY($1)
              AND o4.status NOT IN ('voided', 'refunded')
              AND ($2::timestamptz IS NULL OR o4.created_at >= $2)
              AND ($3::timestamptz IS NULL OR o4.created_at <= $3)
            GROUP BY 1
        )
        SELECT
            p.period_str AS period,
            p.orders,
            p.revenue,
            p.refunded,
            p.voided,
            p.discount,
            p.tax,
            COALESCE(m.revenue_by_method, '{{}}'::json) AS revenue_by_method,
            COALESCE(li.line_items, 0)::bigint AS line_items,
            COALESCE(ad.addons, 0)::bigint AS addons
        FROM periods p
        LEFT JOIN methods_by_period m ON m.period_val = p.period_val
        LEFT JOIN line_item_totals li ON li.period_val = p.period_val
        LEFT JOIN addon_totals ad ON ad.period_val = p.period_val
        ORDER BY p.period_val ASC
        "#,
        trunc = trunc,
    );

    Ok(sqlx::query_as::<_, TimeseriesPoint>(&sql)
        .bind(branch_ids)
        .bind(from)
        .bind(to)
        .bind(tz)
        .fetch_all(pool)
        .await?)
}

// ── org comparison ──────────────────────────────────────────────────────────

struct ComparisonRow {
    branch_id: Uuid,
    branch_name: String,
    total_orders: i64,
    voided_orders: i64,
    total_revenue: i64,
    gross_sales: i64,
    refunded_amount: i64,
    revenue_by_method: serde_json::Value,
    total_tips: i64,
    cash_tips: i64,
}

/// `GET /reports/orgs/{id}/comparison`.
pub async fn org_comparison(
    pool: &PgPool,
    org_id: Uuid,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    source: Source,
) -> Result<OrgComparisonReport, AppError> {
    let rows = match source {
        Source::Live => live_comparison(pool, org_id, from, to).await?,
        Source::Rollups => rolled_comparison(pool, org_id, from, to).await?,
    };
    let branches = rows
        .into_iter()
        .map(|r| BranchComparison {
            branch_id: r.branch_id,
            branch_name: r.branch_name,
            total_orders: r.total_orders,
            voided_orders: r.voided_orders,
            total_revenue: r.total_revenue,
            gross_sales: r.gross_sales,
            refunded_amount: r.refunded_amount,
            revenue_by_method: r.revenue_by_method,
            total_tips: r.total_tips,
            cash_tips: r.cash_tips,
            // The average bill is what was ordered, not what was kept.
            avg_order_value: if r.total_orders == 0 {
                0
            } else {
                r.gross_sales / r.total_orders
            },
            void_rate_pct: if (r.total_orders + r.voided_orders) == 0 {
                0.0
            } else {
                r.voided_orders as f64 / (r.total_orders + r.voided_orders) as f64 * 100.0
            },
        })
        .collect();
    Ok(OrgComparisonReport {
        org_id,
        from,
        to,
        branches,
    })
}

async fn live_comparison(
    pool: &PgPool,
    org_id: Uuid,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<Vec<ComparisonRow>, AppError> {
    Ok(sqlx::query_as!(
        ComparisonRow,
        r#"
        SELECT
            b.id   AS branch_id,
            b.name AS branch_name,
            COUNT(DISTINCT o.id) FILTER (WHERE o.status NOT IN ('voided', 'refunded'))::bigint AS "total_orders!",
            COUNT(DISTINCT o.id) FILTER (WHERE o.status  = 'voided')::bigint AS "voided_orders!",
            COALESCE(SUM(o.total_amount - COALESCE(rf.refunded_amount, 0))
                     FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS "total_revenue!",
            COALESCE(SUM(o.total_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS "gross_sales!",
            COALESCE(SUM(rf.refunded_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS "refunded_amount!",
            COALESCE((
              SELECT json_object_agg(method, rev) FROM (
                SELECT op.method, SUM(op.amount)::bigint AS rev
                FROM order_payments op
                JOIN orders o2 ON o2.id = op.order_id
                WHERE o2.branch_id = b.id AND o2.status NOT IN ('voided', 'refunded')
                  AND ($2::timestamptz IS NULL OR o2.created_at >= $2)
                  AND ($3::timestamptz IS NULL OR o2.created_at <= $3)
                GROUP BY op.method
              ) sub
            ), '{}'::json) AS "revenue_by_method!: serde_json::Value",
            COALESCE(SUM(o.tip_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS "total_tips!",
            COALESCE(SUM(o.tip_amount) FILTER (
                WHERE o.status NOT IN ('voided', 'refunded')
                  AND COALESCE(o.tip_is_cash,
                               COALESCE(o.tip_payment_method, o.payment_method) = 'cash')
            ), 0)::bigint AS "cash_tips!"
        FROM branches b
        LEFT JOIN orders o          ON o.branch_id = b.id
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id
        WHERE b.org_id = $1 AND b.deleted_at IS NULL
        GROUP BY b.id, b.name
        -- Ties by name, then id: before, equal revenue came back in plan order.
        ORDER BY 5 DESC, b.name, b.id
        "#,
        org_id,
        from,
        to,
    )
    .fetch_all(pool)
    .await?)
}

async fn rolled_comparison(
    pool: &PgPool,
    org_id: Uuid,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<Vec<ComparisonRow>, AppError> {
    Ok(sqlx::query_as!(
        ComparisonRow,
        r#"
        WITH br AS (SELECT b.id, b.name FROM branches b WHERE b.org_id = $1 AND b.deleted_at IS NULL),
        p AS (SELECT * FROM sales_daily_plan(ARRAY(SELECT id FROM br), $2::timestamptz, $3::timestamptz, true)),
        w AS (SELECT p.branch_id, p.lo, p.hi FROM p WHERE p.day IS NULL),
        d AS (SELECT p.branch_id, array_agg(p.day) AS days FROM p WHERE p.day IS NOT NULL GROUP BY p.branch_id),
        live AS (
            SELECT o.branch_id,
                   COUNT(DISTINCT o.id) FILTER (WHERE o.status NOT IN ('voided', 'refunded'))::bigint AS total_orders,
                   COUNT(DISTINCT o.id) FILTER (WHERE o.status = 'voided')::bigint AS voided_orders,
                   COALESCE(SUM(o.total_amount - COALESCE(rf.refunded_amount, 0))
                            FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS total_revenue,
                   COALESCE(SUM(o.total_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS gross_sales,
                   COALESCE(SUM(rf.refunded_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS refunded_amount,
                   COALESCE(SUM(o.tip_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS tips,
                   COALESCE(SUM(o.tip_amount) FILTER (
                       WHERE o.status NOT IN ('voided', 'refunded')
                         AND COALESCE(o.tip_is_cash,
                                      COALESCE(o.tip_payment_method, o.payment_method) = 'cash')
                   ), 0)::bigint AS cash_tips
              FROM w CROSS JOIN LATERAL (SELECT o.* FROM orders o WHERE o.branch_id = w.branch_id AND o.created_at >= w.lo AND o.created_at < w.hi OFFSET 0) o
              LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id
             GROUP BY o.branch_id
        ),
        roll AS (
            SELECT s.branch_id,
                   SUM(s.sold_orders)::bigint AS total_orders, SUM(s.voided_orders)::bigint AS voided_orders,
                   SUM(s.revenue_net)::bigint AS total_revenue, SUM(s.gross_sales)::bigint AS gross_sales,
                   SUM(s.refunded_amount)::bigint AS refunded_amount, SUM(s.tips)::bigint AS tips,
                   SUM(s.cash_tips)::bigint AS cash_tips
              FROM d CROSS JOIN LATERAL (SELECT s.* FROM sales_daily_sold s WHERE s.branch_id = d.branch_id AND s.business_date = ANY(d.days) OFFSET 0) s
             GROUP BY s.branch_id
        ),
        meth AS (
            SELECT branch_id, json_object_agg(method, rev) AS j FROM (
                SELECT branch_id, method, SUM(amount)::bigint AS rev FROM (
                    SELECT o2.branch_id, op.method, op.amount::bigint AS amount
                      FROM w CROSS JOIN LATERAL (SELECT o2.* FROM orders o2 WHERE o2.branch_id = w.branch_id AND o2.created_at >= w.lo AND o2.created_at < w.hi OFFSET 0) o2
                      JOIN order_payments op ON op.order_id = o2.id
                     WHERE o2.status NOT IN ('voided', 'refunded')
                    UNION ALL
                    SELECT sm.branch_id, sm.method, sm.amount
                      FROM d CROSS JOIN LATERAL (SELECT sm.* FROM sales_daily_sold_method sm WHERE sm.branch_id = d.branch_id AND sm.business_date = ANY(d.days) OFFSET 0) sm
                ) u GROUP BY 1, 2
            ) m GROUP BY branch_id
        )
        SELECT br.id AS "branch_id!", br.name AS "branch_name!",
               (COALESCE(l.total_orders, 0) + COALESCE(r.total_orders, 0))::bigint AS "total_orders!",
               (COALESCE(l.voided_orders, 0) + COALESCE(r.voided_orders, 0))::bigint AS "voided_orders!",
               (COALESCE(l.total_revenue, 0) + COALESCE(r.total_revenue, 0))::bigint AS "total_revenue!",
               (COALESCE(l.gross_sales, 0) + COALESCE(r.gross_sales, 0))::bigint AS "gross_sales!",
               (COALESCE(l.refunded_amount, 0) + COALESCE(r.refunded_amount, 0))::bigint AS "refunded_amount!",
               COALESCE(m.j, '{}'::json) AS "revenue_by_method!: serde_json::Value",
               (COALESCE(l.tips, 0) + COALESCE(r.tips, 0))::bigint AS "total_tips!",
               (COALESCE(l.cash_tips, 0) + COALESCE(r.cash_tips, 0))::bigint AS "cash_tips!"
          FROM br
          LEFT JOIN live l ON l.branch_id = br.id
          LEFT JOIN roll r ON r.branch_id = br.id
          LEFT JOIN meth m ON m.branch_id = br.id
         ORDER BY 5 DESC, br.name, br.id
        "#,
        org_id,
        from,
        to,
    )
    .fetch_all(pool)
    .await?)
}
