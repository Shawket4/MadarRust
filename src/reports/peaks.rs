//! Peak hours, peak days and the add-on report, read from the closed-day
//! rollups like the sales reports in [`super::sales`] (QUERY_SURVEY_2026-10-17
//! #8): `GET /reports/branches/{id}/sales/peak-hours`, `…/sales/peak-days` and
//! `…/addons`.
//!
//! Same two computations, one result, as there:
//! * [`Source::Live`] — the reports' original SQL over the ledger (the
//!   reference `report-rollup-reconcile` compares against);
//! * [`Source::Rollups`] — `sales_daily_plan` splits the window; whole closed,
//!   covered, clean days add up `sales_daily_sold_hour` (peak hours),
//!   `sales_daily_sold` (peak days: a weekday is a sum of business days) and
//!   `sales_daily_sold_addon` (add-ons); everything else is read live through
//!   the same `CROSS JOIN LATERAL (… OFFSET 0)` index ranges.
//!
//! The plan is asked for the report's zone (`p_tz`): a branch in another zone
//! is read live over the whole window, since its business days do not bucket
//! into this zone's hours or weekdays.
//!
//! The add-on report picks an add-on's translations as the greatest of its
//! lines' (jsonb order), where it used to take whichever line the plan read
//! first, and breaks a ranking tie on the id: deterministic, and the same rule
//! the rollup applies when it merges days.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use super::handlers::{AddonSalesRow, PeakDayPoint, PeakHourPoint};
use super::sales::Source;
use crate::errors::AppError;

/// `GET /reports/branches/{id}/sales/peak-hours` over `ids`, bucketed in `tz`.
pub async fn peak_hours(
    pool: &PgPool,
    ids: &[Uuid],
    tz: &str,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    source: Source,
) -> Result<Vec<PeakHourPoint>, AppError> {
    match source {
        Source::Live => live_peak_hours(pool, ids, tz, from, to).await,
        Source::Rollups => rolled_peak_hours(pool, ids, tz, from, to).await,
    }
}

/// `GET /reports/branches/{id}/sales/peak-days` over `ids`, in `tz`.
pub async fn peak_days(
    pool: &PgPool,
    ids: &[Uuid],
    tz: &str,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    source: Source,
) -> Result<Vec<PeakDayPoint>, AppError> {
    match source {
        Source::Live => live_peak_days(pool, ids, tz, from, to).await,
        Source::Rollups => rolled_peak_days(pool, ids, tz, from, to).await,
    }
}

/// `GET /reports/branches/{id}/addons` over `ids`.
pub async fn addon_sales(
    pool: &PgPool,
    ids: &[Uuid],
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    source: Source,
) -> Result<Vec<AddonSalesRow>, AppError> {
    match source {
        Source::Live => live_addon_sales(pool, ids, from, to).await,
        Source::Rollups => rolled_addon_sales(pool, ids, from, to).await,
    }
}

// ── live: the reports' original SQL ─────────────────────────────────────────

async fn live_peak_hours(
    pool: &PgPool,
    ids: &[Uuid],
    tz: &str,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<Vec<PeakHourPoint>, AppError> {
    // Always all 24 hours, so the chart has a complete x-axis; per-day
    // averages divide by the calendar days of the range (branch-local), or,
    // for an open range, by the distinct days that had an order.
    Ok(sqlx::query_as!(
        PeakHourPoint,
        r#"
        WITH all_hours AS (
            SELECT generate_series(0, 23)::int AS hour
        ),
        day_count AS (
            SELECT GREATEST(1,
                COALESCE(
                    CASE WHEN $2::timestamptz IS NOT NULL AND $3::timestamptz IS NOT NULL
                         THEN (($3::timestamptz AT TIME ZONE $4)::date
                               - ($2::timestamptz AT TIME ZONE $4)::date)::int + 1
                    END,
                    (SELECT COUNT(DISTINCT (o2.created_at AT TIME ZONE $4)::date)::int
                     FROM orders o2
                     WHERE o2.branch_id = ANY($1)
                       AND ($2::timestamptz IS NULL OR o2.created_at >= $2)
                       AND ($3::timestamptz IS NULL OR o2.created_at <= $3))
                )
            )::int AS days
        ),
        aggregated AS (
            SELECT
                EXTRACT(hour FROM o.created_at AT TIME ZONE $4)::int AS hour,
                COUNT(o.id)   FILTER (WHERE o.status NOT IN ('voided', 'refunded'))::bigint  AS orders,
                COALESCE(SUM(o.total_amount - COALESCE(rf.refunded_amount, 0))
                         FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS revenue,
                COUNT(o.id)   FILTER (WHERE o.status  = 'voided')::bigint  AS voided,
                COALESCE(SUM(o.discount_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS discount,
                COALESCE(SUM(o.tax_amount - COALESCE(rf.refunded_tax, 0)) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS tax
            FROM orders o
            LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id
            WHERE o.branch_id = ANY($1)
              AND ($2::timestamptz IS NULL OR o.created_at >= $2)
              AND ($3::timestamptz IS NULL OR o.created_at <= $3)
            GROUP BY 1
        ),
        -- order_items/order_item_addons fan out orders, so they're summed in
        -- their own bucketed CTEs rather than joined into `aggregated` above.
        line_items_agg AS (
            SELECT
                EXTRACT(hour FROM o.created_at AT TIME ZONE $4)::int AS hour,
                COALESCE(SUM(oi.quantity), 0)::bigint AS line_items
            FROM order_items oi
            JOIN orders o ON o.id = oi.order_id
            WHERE o.branch_id = ANY($1)
              AND oi.line_kind <> 'combo'
              AND o.status NOT IN ('voided', 'refunded')
              AND ($2::timestamptz IS NULL OR o.created_at >= $2)
              AND ($3::timestamptz IS NULL OR o.created_at <= $3)
            GROUP BY 1
        ),
        addons_agg AS (
            SELECT
                EXTRACT(hour FROM o.created_at AT TIME ZONE $4)::int AS hour,
                COALESCE(SUM(oia.quantity), 0)::bigint AS addons
            FROM order_item_addons oia
            JOIN order_items oi ON oi.id = oia.order_item_id
            JOIN orders o ON o.id = oi.order_id
            WHERE o.branch_id = ANY($1)
              AND o.status NOT IN ('voided', 'refunded')
              AND ($2::timestamptz IS NULL OR o.created_at >= $2)
              AND ($3::timestamptz IS NULL OR o.created_at <= $3)
            GROUP BY 1
        ),
        totals AS (
            SELECT
                COALESCE(SUM(revenue), 0) AS total_revenue,
                COALESCE(SUM(orders),  0) AS total_orders
            FROM aggregated
        )
        SELECT
            h.hour AS "hour!",
            COALESCE(a.orders,   0)::bigint AS "orders!",
            COALESCE(a.revenue,  0)::bigint AS "revenue!",
            COALESCE(a.voided,   0)::bigint AS "voided!",
            COALESCE(a.discount, 0)::bigint AS "discount!",
            COALESCE(a.tax,      0)::bigint AS "tax!",
            COALESCE(li.line_items, 0)::bigint AS "line_items!",
            COALESCE(ad.addons,     0)::bigint AS "addons!",
            ROUND(COALESCE(a.revenue, 0)::numeric / d.days)::bigint        AS "avg_revenue_per_day!",
            (COALESCE(a.orders,  0)::float8 / d.days::float8)              AS "avg_orders_per_day!",
            CASE WHEN t.total_revenue > 0
                 THEN ROUND(COALESCE(a.revenue, 0)::numeric / t.total_revenue * 100, 1)::float8
                 ELSE 0.0::float8 END                                       AS "revenue_pct!",
            CASE WHEN t.total_orders > 0
                 THEN ROUND(COALESCE(a.orders,  0)::numeric / t.total_orders  * 100, 1)::float8
                 ELSE 0.0::float8 END                                       AS "orders_pct!"
        FROM all_hours h
        CROSS JOIN day_count d
        CROSS JOIN totals t
        LEFT JOIN aggregated a ON a.hour = h.hour
        LEFT JOIN line_items_agg li ON li.hour = h.hour
        LEFT JOIN addons_agg ad ON ad.hour = h.hour
        ORDER BY h.hour ASC
        "#,
        ids,
        from,
        to,
        tz,
    )
    .fetch_all(pool)
    .await?)
}

async fn live_peak_days(
    pool: &PgPool,
    ids: &[Uuid],
    tz: &str,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<Vec<PeakDayPoint>, AppError> {
    // Always all 7 weekdays. "Per occurrence" averages divide by how many
    // times that weekday fell in the range, not the range's day count.
    Ok(sqlx::query_as!(
        PeakDayPoint,
        r#"
        WITH all_days AS (
            SELECT generate_series(0, 6)::int AS day_of_week
        ),
        day_occurrences AS (
            SELECT EXTRACT(dow FROM d)::int AS day_of_week, COUNT(*)::int AS occurrences
            FROM generate_series(
                COALESCE(
                    ($2::timestamptz AT TIME ZONE $4)::date,
                    (SELECT MIN((created_at AT TIME ZONE $4)::date) FROM orders WHERE branch_id = ANY($1))
                ),
                COALESCE(
                    ($3::timestamptz AT TIME ZONE $4)::date,
                    (SELECT MAX((created_at AT TIME ZONE $4)::date) FROM orders WHERE branch_id = ANY($1))
                ),
                interval '1 day'
            ) d
            GROUP BY 1
        ),
        aggregated AS (
            SELECT
                EXTRACT(dow FROM o.created_at AT TIME ZONE $4)::int AS day_of_week,
                COUNT(o.id)   FILTER (WHERE o.status NOT IN ('voided', 'refunded'))::bigint  AS orders,
                COALESCE(SUM(o.total_amount - COALESCE(rf.refunded_amount, 0))
                         FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS revenue,
                COUNT(o.id)   FILTER (WHERE o.status  = 'voided')::bigint  AS voided,
                COALESCE(SUM(o.discount_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS discount,
                COALESCE(SUM(o.tax_amount - COALESCE(rf.refunded_tax, 0)) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS tax
            FROM orders o
            LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id
            WHERE o.branch_id = ANY($1)
              AND ($2::timestamptz IS NULL OR o.created_at >= $2)
              AND ($3::timestamptz IS NULL OR o.created_at <= $3)
            GROUP BY 1
        ),
        line_items_agg AS (
            SELECT
                EXTRACT(dow FROM o.created_at AT TIME ZONE $4)::int AS day_of_week,
                COALESCE(SUM(oi.quantity), 0)::bigint AS line_items
            FROM order_items oi
            JOIN orders o ON o.id = oi.order_id
            WHERE o.branch_id = ANY($1)
              AND oi.line_kind <> 'combo'
              AND o.status NOT IN ('voided', 'refunded')
              AND ($2::timestamptz IS NULL OR o.created_at >= $2)
              AND ($3::timestamptz IS NULL OR o.created_at <= $3)
            GROUP BY 1
        ),
        addons_agg AS (
            SELECT
                EXTRACT(dow FROM o.created_at AT TIME ZONE $4)::int AS day_of_week,
                COALESCE(SUM(oia.quantity), 0)::bigint AS addons
            FROM order_item_addons oia
            JOIN order_items oi ON oi.id = oia.order_item_id
            JOIN orders o ON o.id = oi.order_id
            WHERE o.branch_id = ANY($1)
              AND o.status NOT IN ('voided', 'refunded')
              AND ($2::timestamptz IS NULL OR o.created_at >= $2)
              AND ($3::timestamptz IS NULL OR o.created_at <= $3)
            GROUP BY 1
        ),
        totals AS (
            SELECT
                COALESCE(SUM(revenue), 0) AS total_revenue,
                COALESCE(SUM(orders),  0) AS total_orders
            FROM aggregated
        )
        SELECT
            w.day_of_week AS "day_of_week!",
            COALESCE(a.orders,   0)::bigint AS "orders!",
            COALESCE(a.revenue,  0)::bigint AS "revenue!",
            COALESCE(a.voided,   0)::bigint AS "voided!",
            COALESCE(a.discount, 0)::bigint AS "discount!",
            COALESCE(a.tax,      0)::bigint AS "tax!",
            COALESCE(li.line_items, 0)::bigint AS "line_items!",
            COALESCE(ad.addons,     0)::bigint AS "addons!",
            ROUND(COALESCE(a.revenue, 0)::numeric / GREATEST(1, COALESCE(o.occurrences, 1)))::bigint AS "avg_revenue_per_day!",
            (COALESCE(a.orders,  0)::float8 / GREATEST(1, COALESCE(o.occurrences, 1))::float8)        AS "avg_orders_per_day!",
            CASE WHEN t.total_revenue > 0
                 THEN ROUND(COALESCE(a.revenue, 0)::numeric / t.total_revenue * 100, 1)::float8
                 ELSE 0.0::float8 END                                       AS "revenue_pct!",
            CASE WHEN t.total_orders > 0
                 THEN ROUND(COALESCE(a.orders,  0)::numeric / t.total_orders  * 100, 1)::float8
                 ELSE 0.0::float8 END                                       AS "orders_pct!"
        FROM all_days w
        CROSS JOIN totals t
        LEFT JOIN aggregated a ON a.day_of_week = w.day_of_week
        LEFT JOIN line_items_agg li ON li.day_of_week = w.day_of_week
        LEFT JOIN addons_agg ad ON ad.day_of_week = w.day_of_week
        LEFT JOIN day_occurrences o ON o.day_of_week = w.day_of_week
        ORDER BY w.day_of_week ASC
        "#,
        ids,
        from,
        to,
        tz,
    )
    .fetch_all(pool)
    .await?)
}

async fn live_addon_sales(
    pool: &PgPool,
    ids: &[Uuid],
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<Vec<AddonSalesRow>, AppError> {
    Ok(sqlx::query_as!(
        AddonSalesRow,
        r#"
        SELECT
            oia.addon_item_id,
            oia.addon_name,
            COALESCE((array_agg(oia.name_translations ORDER BY oia.name_translations DESC))[1], '{}'::jsonb) AS "addon_name_translations!",
            COALESCE(mg.legacy_addon_type, 'extra') AS "addon_type!",
            SUM(oia.quantity)::bigint  AS "quantity_sold!",
            SUM(oia.line_total)::bigint AS "revenue!"
        FROM order_item_addons oia
        JOIN order_items oi ON oi.id  = oia.order_item_id
        JOIN orders o       ON o.id   = oi.order_id
        -- An add-on's type is its group's legacy_addon_type (the old addon_items).
        LEFT JOIN modifier_options mo ON mo.id = oia.addon_item_id AND mo.legacy_source = 'addon'
        LEFT JOIN modifier_groups mg ON mg.id = mo.group_id
        WHERE o.branch_id = ANY($1)
          AND o.status NOT IN ('voided', 'refunded')
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY oia.addon_item_id, oia.addon_name, mg.legacy_addon_type
        ORDER BY 5 DESC, 6 DESC, oia.addon_name COLLATE "C", oia.addon_item_id
        "#,
        ids,
        from,
        to,
    )
    .fetch_all(pool)
    .await?)
}

// ── rollups: closed days from sales_daily_sold*, the rest live ─────────────

async fn rolled_peak_hours(
    pool: &PgPool,
    ids: &[Uuid],
    tz: &str,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<Vec<PeakHourPoint>, AppError> {
    Ok(sqlx::query_as!(
        PeakHourPoint,
        r#"
        WITH p AS (SELECT * FROM sales_daily_plan($1::uuid[], $2::timestamptz, $3::timestamptz, true, $4)),
        w AS (SELECT p.branch_id, p.lo, p.hi FROM p WHERE p.day IS NULL),
        d AS (SELECT p.branch_id, array_agg(p.day) AS days FROM p WHERE p.day IS NOT NULL GROUP BY p.branch_id),
        all_hours AS (
            SELECT generate_series(0, 23)::int AS hour
        ),
        day_count AS (
            SELECT GREATEST(1,
                COALESCE(
                    CASE WHEN $2::timestamptz IS NOT NULL AND $3::timestamptz IS NOT NULL
                         THEN (($3::timestamptz AT TIME ZONE $4)::date
                               - ($2::timestamptz AT TIME ZONE $4)::date)::int + 1
                    END,
                    (SELECT COUNT(DISTINCT (o2.created_at AT TIME ZONE $4)::date)::int
                     FROM orders o2
                     WHERE o2.branch_id = ANY($1)
                       AND ($2::timestamptz IS NULL OR o2.created_at >= $2)
                       AND ($3::timestamptz IS NULL OR o2.created_at <= $3))
                )
            )::int AS days
        ),
        lo AS (
            SELECT o.* FROM w CROSS JOIN LATERAL (
                SELECT o.id, o.status, o.created_at, o.total_amount, o.discount_amount, o.tax_amount
                  FROM orders o
                 WHERE o.branch_id = w.branch_id AND o.created_at >= w.lo AND o.created_at < w.hi
                OFFSET 0
            ) o
        ),
        aggregated AS (
            SELECT hour, SUM(orders) AS orders, SUM(revenue) AS revenue, SUM(voided) AS voided,
                   SUM(discount) AS discount, SUM(tax) AS tax
              FROM (
                SELECT
                    EXTRACT(hour FROM o.created_at AT TIME ZONE $4)::int AS hour,
                    COUNT(o.id)   FILTER (WHERE o.status NOT IN ('voided', 'refunded'))::bigint  AS orders,
                    COALESCE(SUM(o.total_amount - COALESCE(rf.refunded_amount, 0))
                             FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS revenue,
                    COUNT(o.id)   FILTER (WHERE o.status  = 'voided')::bigint  AS voided,
                    COALESCE(SUM(o.discount_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS discount,
                    COALESCE(SUM(o.tax_amount - COALESCE(rf.refunded_tax, 0)) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS tax
                FROM lo o
                LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id
                GROUP BY 1
                UNION ALL
                SELECT h.hour::int, h.sold_orders, h.revenue_net, h.voided_orders, h.discount, h.tax_net
                  FROM d CROSS JOIN LATERAL (SELECT h.* FROM sales_daily_sold_hour h WHERE h.branch_id = d.branch_id AND h.business_date = ANY(d.days) OFFSET 0) h
              ) u
             GROUP BY hour
        ),
        line_items_agg AS (
            SELECT hour, SUM(q) AS line_items FROM (
                SELECT EXTRACT(hour FROM o.created_at AT TIME ZONE $4)::int AS hour, oi.quantity::bigint AS q
                  FROM lo o JOIN order_items oi ON oi.order_id = o.id
                 WHERE oi.line_kind <> 'combo' AND o.status NOT IN ('voided', 'refunded')
                UNION ALL
                SELECT h.hour::int, h.line_items
                  FROM d CROSS JOIN LATERAL (SELECT h.* FROM sales_daily_sold_hour h WHERE h.branch_id = d.branch_id AND h.business_date = ANY(d.days) OFFSET 0) h
            ) u GROUP BY hour
        ),
        addons_agg AS (
            SELECT hour, SUM(q) AS addons FROM (
                SELECT EXTRACT(hour FROM o.created_at AT TIME ZONE $4)::int AS hour, oia.quantity::bigint AS q
                  FROM lo o JOIN order_items oi ON oi.order_id = o.id
                  JOIN order_item_addons oia ON oia.order_item_id = oi.id
                 WHERE o.status NOT IN ('voided', 'refunded')
                UNION ALL
                SELECT h.hour::int, h.addons
                  FROM d CROSS JOIN LATERAL (SELECT h.* FROM sales_daily_sold_hour h WHERE h.branch_id = d.branch_id AND h.business_date = ANY(d.days) OFFSET 0) h
            ) u GROUP BY hour
        ),
        totals AS (
            SELECT
                COALESCE(SUM(revenue), 0) AS total_revenue,
                COALESCE(SUM(orders),  0) AS total_orders
            FROM aggregated
        )
        SELECT
            h.hour AS "hour!",
            COALESCE(a.orders,   0)::bigint AS "orders!",
            COALESCE(a.revenue,  0)::bigint AS "revenue!",
            COALESCE(a.voided,   0)::bigint AS "voided!",
            COALESCE(a.discount, 0)::bigint AS "discount!",
            COALESCE(a.tax,      0)::bigint AS "tax!",
            COALESCE(li.line_items, 0)::bigint AS "line_items!",
            COALESCE(ad.addons,     0)::bigint AS "addons!",
            ROUND(COALESCE(a.revenue, 0)::numeric / dc.days)::bigint        AS "avg_revenue_per_day!",
            (COALESCE(a.orders,  0)::float8 / dc.days::float8)              AS "avg_orders_per_day!",
            CASE WHEN t.total_revenue > 0
                 THEN ROUND(COALESCE(a.revenue, 0)::numeric / t.total_revenue * 100, 1)::float8
                 ELSE 0.0::float8 END                                       AS "revenue_pct!",
            CASE WHEN t.total_orders > 0
                 THEN ROUND(COALESCE(a.orders,  0)::numeric / t.total_orders  * 100, 1)::float8
                 ELSE 0.0::float8 END                                       AS "orders_pct!"
        FROM all_hours h
        CROSS JOIN day_count dc
        CROSS JOIN totals t
        LEFT JOIN aggregated a ON a.hour = h.hour
        LEFT JOIN line_items_agg li ON li.hour = h.hour
        LEFT JOIN addons_agg ad ON ad.hour = h.hour
        ORDER BY h.hour ASC
        "#,
        ids,
        from,
        to,
        tz,
    )
    .fetch_all(pool)
    .await?)
}

async fn rolled_peak_days(
    pool: &PgPool,
    ids: &[Uuid],
    tz: &str,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<Vec<PeakDayPoint>, AppError> {
    Ok(sqlx::query_as!(
        PeakDayPoint,
        r#"
        WITH p AS (SELECT * FROM sales_daily_plan($1::uuid[], $2::timestamptz, $3::timestamptz, true, $4)),
        w AS (SELECT p.branch_id, p.lo, p.hi FROM p WHERE p.day IS NULL),
        d AS (SELECT p.branch_id, array_agg(p.day) AS days FROM p WHERE p.day IS NOT NULL GROUP BY p.branch_id),
        all_days AS (
            SELECT generate_series(0, 6)::int AS day_of_week
        ),
        day_occurrences AS (
            SELECT EXTRACT(dow FROM g)::int AS day_of_week, COUNT(*)::int AS occurrences
            FROM generate_series(
                COALESCE(
                    ($2::timestamptz AT TIME ZONE $4)::date,
                    (SELECT MIN((created_at AT TIME ZONE $4)::date) FROM orders WHERE branch_id = ANY($1))
                ),
                COALESCE(
                    ($3::timestamptz AT TIME ZONE $4)::date,
                    (SELECT MAX((created_at AT TIME ZONE $4)::date) FROM orders WHERE branch_id = ANY($1))
                ),
                interval '1 day'
            ) g
            GROUP BY 1
        ),
        lo AS (
            SELECT o.* FROM w CROSS JOIN LATERAL (
                SELECT o.id, o.status, o.created_at, o.total_amount, o.discount_amount, o.tax_amount
                  FROM orders o
                 WHERE o.branch_id = w.branch_id AND o.created_at >= w.lo AND o.created_at < w.hi
                OFFSET 0
            ) o
        ),
        rolled AS (
            SELECT s.* FROM d CROSS JOIN LATERAL (SELECT s.* FROM sales_daily_sold s WHERE s.branch_id = d.branch_id AND s.business_date = ANY(d.days) OFFSET 0) s
        ),
        aggregated AS (
            SELECT day_of_week, SUM(orders) AS orders, SUM(revenue) AS revenue, SUM(voided) AS voided,
                   SUM(discount) AS discount, SUM(tax) AS tax
              FROM (
                SELECT
                    EXTRACT(dow FROM o.created_at AT TIME ZONE $4)::int AS day_of_week,
                    COUNT(o.id)   FILTER (WHERE o.status NOT IN ('voided', 'refunded'))::bigint  AS orders,
                    COALESCE(SUM(o.total_amount - COALESCE(rf.refunded_amount, 0))
                             FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS revenue,
                    COUNT(o.id)   FILTER (WHERE o.status  = 'voided')::bigint  AS voided,
                    COALESCE(SUM(o.discount_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS discount,
                    COALESCE(SUM(o.tax_amount - COALESCE(rf.refunded_tax, 0)) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0)::bigint AS tax
                FROM lo o
                LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id
                GROUP BY 1
                UNION ALL
                SELECT EXTRACT(dow FROM s.business_date)::int, s.sold_orders, s.revenue_net, s.voided_orders, s.discount, s.tax_net
                  FROM rolled s
              ) u
             GROUP BY day_of_week
        ),
        line_items_agg AS (
            SELECT day_of_week, SUM(q) AS line_items FROM (
                SELECT EXTRACT(dow FROM o.created_at AT TIME ZONE $4)::int AS day_of_week, oi.quantity::bigint AS q
                  FROM lo o JOIN order_items oi ON oi.order_id = o.id
                 WHERE oi.line_kind <> 'combo' AND o.status NOT IN ('voided', 'refunded')
                UNION ALL
                SELECT EXTRACT(dow FROM s.business_date)::int, s.line_items FROM rolled s
            ) u GROUP BY day_of_week
        ),
        addons_agg AS (
            SELECT day_of_week, SUM(q) AS addons FROM (
                SELECT EXTRACT(dow FROM o.created_at AT TIME ZONE $4)::int AS day_of_week, oia.quantity::bigint AS q
                  FROM lo o JOIN order_items oi ON oi.order_id = o.id
                  JOIN order_item_addons oia ON oia.order_item_id = oi.id
                 WHERE o.status NOT IN ('voided', 'refunded')
                UNION ALL
                SELECT EXTRACT(dow FROM s.business_date)::int, s.addons FROM rolled s
            ) u GROUP BY day_of_week
        ),
        totals AS (
            SELECT
                COALESCE(SUM(revenue), 0) AS total_revenue,
                COALESCE(SUM(orders),  0) AS total_orders
            FROM aggregated
        )
        SELECT
            wd.day_of_week AS "day_of_week!",
            COALESCE(a.orders,   0)::bigint AS "orders!",
            COALESCE(a.revenue,  0)::bigint AS "revenue!",
            COALESCE(a.voided,   0)::bigint AS "voided!",
            COALESCE(a.discount, 0)::bigint AS "discount!",
            COALESCE(a.tax,      0)::bigint AS "tax!",
            COALESCE(li.line_items, 0)::bigint AS "line_items!",
            COALESCE(ad.addons,     0)::bigint AS "addons!",
            ROUND(COALESCE(a.revenue, 0)::numeric / GREATEST(1, COALESCE(o.occurrences, 1)))::bigint AS "avg_revenue_per_day!",
            (COALESCE(a.orders,  0)::float8 / GREATEST(1, COALESCE(o.occurrences, 1))::float8)        AS "avg_orders_per_day!",
            CASE WHEN t.total_revenue > 0
                 THEN ROUND(COALESCE(a.revenue, 0)::numeric / t.total_revenue * 100, 1)::float8
                 ELSE 0.0::float8 END                                       AS "revenue_pct!",
            CASE WHEN t.total_orders > 0
                 THEN ROUND(COALESCE(a.orders,  0)::numeric / t.total_orders  * 100, 1)::float8
                 ELSE 0.0::float8 END                                       AS "orders_pct!"
        FROM all_days wd
        CROSS JOIN totals t
        LEFT JOIN aggregated a ON a.day_of_week = wd.day_of_week
        LEFT JOIN line_items_agg li ON li.day_of_week = wd.day_of_week
        LEFT JOIN addons_agg ad ON ad.day_of_week = wd.day_of_week
        LEFT JOIN day_occurrences o ON o.day_of_week = wd.day_of_week
        ORDER BY wd.day_of_week ASC
        "#,
        ids,
        from,
        to,
        tz,
    )
    .fetch_all(pool)
    .await?)
}

async fn rolled_addon_sales(
    pool: &PgPool,
    ids: &[Uuid],
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<Vec<AddonSalesRow>, AppError> {
    Ok(sqlx::query_as!(
        AddonSalesRow,
        r#"
        WITH p AS (SELECT * FROM sales_daily_plan($1::uuid[], $2::timestamptz, $3::timestamptz, true)),
        w AS (SELECT p.branch_id, p.lo, p.hi FROM p WHERE p.day IS NULL),
        d AS (SELECT p.branch_id, array_agg(p.day) AS days FROM p WHERE p.day IS NOT NULL GROUP BY p.branch_id),
        rows AS (
            SELECT oia.addon_item_id, oia.addon_name, oia.name_translations AS tr,
                   oia.quantity::bigint AS q, oia.line_total::bigint AS r
              FROM w CROSS JOIN LATERAL (SELECT o.id, o.status FROM orders o WHERE o.branch_id = w.branch_id AND o.created_at >= w.lo AND o.created_at < w.hi OFFSET 0) o
              JOIN order_items oi ON oi.order_id = o.id
              JOIN order_item_addons oia ON oia.order_item_id = oi.id
             WHERE o.status NOT IN ('voided', 'refunded')
            UNION ALL
            SELECT sa.addon_item_id, sa.addon_name, sa.name_translations, sa.quantity, sa.revenue
              FROM d CROSS JOIN LATERAL (SELECT sa.* FROM sales_daily_sold_addon sa WHERE sa.branch_id = d.branch_id AND sa.business_date = ANY(d.days) OFFSET 0) sa
        )
        SELECT
            x.addon_item_id AS "addon_item_id!",
            x.addon_name AS "addon_name!",
            COALESCE((array_agg(x.tr ORDER BY x.tr DESC))[1], '{}'::jsonb) AS "addon_name_translations!",
            COALESCE(mg.legacy_addon_type, 'extra') AS "addon_type!",
            SUM(x.q)::bigint AS "quantity_sold!",
            SUM(x.r)::bigint AS "revenue!"
        FROM rows x
        LEFT JOIN modifier_options mo ON mo.id = x.addon_item_id AND mo.legacy_source = 'addon'
        LEFT JOIN modifier_groups mg ON mg.id = mo.group_id
        GROUP BY x.addon_item_id, x.addon_name, mg.legacy_addon_type
        ORDER BY 5 DESC, 6 DESC, x.addon_name COLLATE "C", x.addon_item_id
        "#,
        ids,
        from,
        to,
    )
    .fetch_all(pool)
    .await?)
}
