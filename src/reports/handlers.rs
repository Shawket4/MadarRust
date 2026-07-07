use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    errors::{AppError, AppErrorResponse},
    models::UserRole,
    permissions::checker::check_permission,
};
use utoipa::{IntoParams, ToSchema};

// ── Query params ──────────────────────────────────────────────

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DateRangeQuery {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: Option<i64>, // for top_items (default 20)
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BranchSalesQuery {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: Option<i64>, // for top_items (default 20)
    /// Comma-separated menu_item/bundle UUIDs left out of `total_line_items`
    /// (units sold) ONLY — revenue, top items, and categories are untouched.
    pub exclude_items: Option<String>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TimeseriesQuery {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub granularity: Option<String>, // "hourly" | "daily" | "monthly"
}

// ── Response types ────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct ShiftSummary {
    pub shift_id: Uuid,
    pub branch_id: Uuid,
    pub branch_name: String,
    pub teller_id: Uuid,
    pub teller_name: String,
    pub status: String,
    pub opened_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    pub opening_cash: i64,
    pub closing_cash_declared: Option<i64>,
    pub closing_cash_system: Option<i64>,
    pub cash_discrepancy: Option<i64>,
    pub total_orders: i64,
    pub voided_orders: i64,
    pub total_revenue: i64,
    pub revenue_by_method: serde_json::Value,
    pub total_discount: i64,
    pub total_tax: i64,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct DeductionLogRow {
    pub id: Uuid,
    pub order_id: Option<Uuid>,
    pub order_item_id: Option<Uuid>,
    pub inventory_item_id: Uuid,
    pub item_name: String,
    pub unit: String,
    pub quantity_deducted: f64,
    pub source: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CategorySales {
    pub category_id: Option<Uuid>,
    pub category_name: Option<String>,
    #[schema(value_type = Object)]
    pub category_name_translations: Option<serde_json::Value>,
    pub item_count: i64,
    pub quantity_sold: i64,
    pub revenue: i64,
    pub items: Vec<ItemSales>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct ItemSales {
    pub menu_item_id: Uuid,
    pub item_name: String,
    #[schema(value_type = Object)]
    pub item_name_translations: serde_json::Value,
    pub quantity_sold: i64,
    pub revenue: i64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct BranchSalesReport {
    pub branch_id: Uuid,
    pub branch_name: String,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub total_orders: i64,
    pub voided_orders: i64,
    pub subtotal: i64,
    pub total_discount: i64,
    pub total_tax: i64,
    pub total_revenue: i64,
    /// Units sold (SUM of order_items.quantity) across non-voided orders in
    /// range. Counts units, not distinct lines ("3× burger" contributes 3),
    /// matching quantity_sold in the item/category breakdowns.
    #[serde(default)]
    pub total_line_items: i64,
    pub revenue_by_method: serde_json::Value,
    pub top_items: Vec<ItemSales>,
    pub by_category: Vec<CategorySales>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct StockRow {
    pub branch_inventory_id: Uuid,
    pub ingredient_name: String,
    pub unit: String,
    pub current_stock: f64,
    pub reorder_threshold: f64,
    /// Piastres per unit; `null` ⟺ cost never entered.
    #[serde(with = "rust_decimal::serde::float_option")]
    #[schema(value_type = Option<f64>)]
    pub cost_per_unit: Option<Decimal>,
    pub below_reorder: bool,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct BranchStockReport {
    pub branch_id: Uuid,
    pub branch_name: String,
    pub items: Vec<StockRow>,
}

// Timeseries now includes per-payment-method breakdown
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct TimeseriesPoint {
    pub period: String,
    pub orders: i64,
    pub revenue: i64,
    pub voided: i64,
    pub discount: i64,
    pub tax: i64,
    pub revenue_by_method: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct TellerStats {
    pub teller_id: Uuid,
    pub teller_name: String,
    pub orders: i64,
    pub revenue: i64,
    pub avg_order_value: i64,
    pub voided: i64,
    pub shifts: i64,
}

/// Per-waiter sales split. Only waiter-attributed orders appear (dine-in
/// settled from a waiter's ticket — `orders.waiter_id IS NOT NULL`); direct
/// teller sales and delivery orders are out of scope, so totals here are a
/// subset of the branch overview. `attributed_orders`/`attributed_revenue`
/// on the report envelope let the UI caption that gap.
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct WaiterStats {
    pub waiter_id: Uuid,
    pub waiter_name: String,
    pub orders: i64,
    pub revenue: i64,
    pub avg_order_value: i64,
    pub voided: i64,
    /// Units sold (SUM of order_items.quantity) on this waiter's non-voided
    /// orders — the upsell signal behind avg_items_per_order.
    pub line_items: i64,
    /// line_items / orders; 0 when the waiter has no non-voided orders.
    pub avg_items_per_order: f64,
}

/// Envelope for the waiters split: rows plus the branch-level totals needed
/// to caption coverage ("X of Y orders came through waiters").
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct WaiterStatsReport {
    pub waiters: Vec<WaiterStats>,
    /// Non-voided orders in range that carry a waiter.
    pub attributed_orders: i64,
    /// All non-voided orders in range (waiter or not).
    pub total_orders: i64,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct AddonSalesRow {
    pub addon_item_id: Uuid,
    pub addon_name: String,
    #[schema(value_type = Object)]
    pub addon_name_translations: serde_json::Value,
    pub addon_type: String,
    pub quantity_sold: i64,
    pub revenue: i64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct BranchComparison {
    pub branch_id: Uuid,
    pub branch_name: String,
    pub total_orders: i64,
    pub voided_orders: i64,
    pub total_revenue: i64,
    pub revenue_by_method: serde_json::Value,
    pub avg_order_value: i64,
    pub void_rate_pct: f64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OrgComparisonReport {
    pub org_id: Uuid,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub branches: Vec<BranchComparison>,
}

// ── GET /reports/shifts/:id/summary ──────────────────────────

#[utoipa::path(
    get,
    path = "/reports/shifts/{shift_id}/summary",
    tag = "reports",
    params(("shift_id" = Uuid, Path, description = "Shift ID")),
    responses((status = 200, description = "Shift summary", body = ShiftSummary), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn shift_summary(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    shift_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "shifts", "read").await?;
    require_shift_branch_access(pool.get_ref(), &claims, *shift_id).await?;

    let summary = sqlx::query_as::<_, ShiftSummary>(
        r#"
        SELECT
            s.id                                        AS shift_id,
            s.branch_id,
            b.name                                      AS branch_name,
            s.teller_id,
            u.name                                      AS teller_name,
            s.status::text,
            s.created_at                                AS opened_at,
            s.closed_at,
            s.opening_cash::bigint,
            s.closing_cash_declared::bigint,
            s.closing_cash_system::bigint,
            s.cash_discrepancy::bigint,
            COUNT(o.id) FILTER (WHERE o.status != 'voided')::bigint     AS total_orders,
            COUNT(o.id) FILTER (WHERE o.status = 'voided')::bigint      AS voided_orders,
            COALESCE(SUM(o.total_amount) FILTER (WHERE o.status != 'voided'), 0)::bigint AS total_revenue,
            COALESCE((
              SELECT json_object_agg(method, rev) FROM (
                SELECT op.method, SUM(op.amount)::bigint AS rev
                FROM order_payments op
                JOIN orders o2 ON o2.id = op.order_id
                WHERE o2.shift_id = s.id AND o2.status != 'voided'
                GROUP BY op.method
              ) sub
            ), '{}'::json) AS revenue_by_method,
            COALESCE(SUM(o.discount_amount) FILTER (WHERE o.status != 'voided'), 0)::bigint AS total_discount,
            COALESCE(SUM(o.tax_amount)      FILTER (WHERE o.status != 'voided'), 0)::bigint AS total_tax
        FROM shifts s
        JOIN branches b ON b.id = s.branch_id
        JOIN users    u ON u.id = s.teller_id
        LEFT JOIN orders o          ON o.shift_id  = s.id
        WHERE s.id = $1
        GROUP BY s.id, b.name, u.name
        "#,
    )
    .bind(*shift_id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Shift not found".into()))?;

    Ok(HttpResponse::Ok().json(summary))
}

// ── GET /reports/shifts/:id/deductions ───────────────────────

#[utoipa::path(
    get,
    path = "/reports/shifts/{shift_id}/deductions",
    tag = "reports",
    params(("shift_id" = Uuid, Path, description = "Shift ID")),
    responses((status = 200, description = "Shift deductions", body = Vec<DeductionLogRow>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn shift_deductions(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    shift_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_shift_branch_access(pool.get_ref(), &claims, *shift_id).await?;

    // Inventory deduction logs no longer exist — deductions happen directly on branch_inventory.
    // Return empty array to maintain API compatibility.
    let rows: Vec<DeductionLogRow> = Vec::new();
    Ok(HttpResponse::Ok().json(rows))
}

// ── GET /reports/branches/:id/sales ──────────────────────────

#[utoipa::path(
    get,
    path = "/reports/branches/{branch_id}/sales",
    tag = "reports",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    params(BranchSalesQuery),
    responses((status = 200, description = "Branch sales", body = BranchSalesReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn branch_sales(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query: web::Query<BranchSalesQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    let (branch_ids, _org) =
        resolve_report_branches(pool.get_ref(), &claims, &req, *branch_id).await?;
    let branch_name = branch_label(pool.get_ref(), *branch_id).await?;

    let exclude_items = match &query.exclude_items {
        Some(raw) => crate::orders::handlers::parse_uuid_csv("exclude_items", raw)?,
        None => None,
    };

    let totals: (i64, i64, i64, i64, i64, i64, i64, serde_json::Value) = sqlx::query_as(
        r#"
        SELECT
            COUNT(*) FILTER (WHERE status != 'voided')::bigint,
            COUNT(*) FILTER (WHERE status = 'voided')::bigint,
            COALESCE(SUM(subtotal)        FILTER (WHERE status != 'voided'), 0)::bigint,
            COALESCE(SUM(discount_amount) FILTER (WHERE status != 'voided'), 0)::bigint,
            COALESCE(SUM(tax_amount)      FILTER (WHERE status != 'voided'), 0)::bigint,
            COALESCE(SUM(total_amount)    FILTER (WHERE status != 'voided'), 0)::bigint,
            COALESCE((
              SELECT SUM(oi.quantity)::bigint
              FROM order_items oi
              JOIN orders o3 ON o3.id = oi.order_id
              WHERE o3.branch_id = ANY($1) AND o3.status != 'voided'
                AND ($2::timestamptz IS NULL OR o3.created_at >= $2)
                AND ($3::timestamptz IS NULL OR o3.created_at <= $3)
                AND ($4::uuid[] IS NULL OR COALESCE(oi.menu_item_id, oi.bundle_id) != ALL($4::uuid[]))
            ), 0)::bigint,
            COALESCE((
              SELECT json_object_agg(method, rev) FROM (
                SELECT op.method, SUM(op.amount)::bigint AS rev
                FROM order_payments op
                JOIN orders o2 ON o2.id = op.order_id
                WHERE o2.branch_id = ANY($1) AND o2.status != 'voided'
                  AND ($2::timestamptz IS NULL OR o2.created_at >= $2)
                  AND ($3::timestamptz IS NULL OR o2.created_at <= $3)
                GROUP BY op.method
              ) sub
            ), '{}'::json)
        FROM orders
        WHERE branch_id = ANY($1)
          AND ($2::timestamptz IS NULL OR created_at >= $2)
          AND ($3::timestamptz IS NULL OR created_at <= $3)
        "#,
    )
    .bind(&branch_ids)
    .bind(query.from)
    .bind(query.to)
    .bind(&exclude_items)
    .fetch_one(pool.get_ref())
    .await?;

    let item_limit = query.limit.unwrap_or(20).clamp(1, 100);

    let top_items = sqlx::query_as::<_, ItemSales>(
        r#"
        SELECT COALESCE(oi.menu_item_id, oi.bundle_id) AS menu_item_id, oi.item_name,
               COALESCE((array_agg(oi.name_translations))[1], '{}'::jsonb) AS item_name_translations,
               SUM(oi.quantity)::bigint   AS quantity_sold,
               SUM(oi.line_total)::bigint AS revenue
        FROM order_items oi
        JOIN orders o ON o.id = oi.order_id
        WHERE o.branch_id = ANY($1) AND o.status != 'voided'
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY COALESCE(oi.menu_item_id, oi.bundle_id), oi.item_name
        ORDER BY revenue DESC
        LIMIT $4
        "#,
    )
    .bind(&branch_ids).bind(query.from).bind(query.to).bind(item_limit)
    .fetch_all(pool.get_ref()).await?;

    #[derive(sqlx::FromRow)]
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

    let cat_rows = sqlx::query_as::<_, CategoryItemRow>(
        r#"
        SELECT 
            CASE 
                WHEN oi.bundle_id IS NOT NULL THEN '00000000-0000-0000-0000-000000000000'::uuid
                ELSE m.category_id
            END AS category_id,
            COALESCE(c.name, CASE WHEN oi.bundle_id IS NOT NULL THEN 'Bundles' ELSE 'Uncategorized' END) AS category_name,
            (array_agg(c.name_translations))[1] AS category_name_translations,
            COALESCE(oi.menu_item_id, oi.bundle_id) AS menu_item_id,
            oi.item_name,
            COALESCE((array_agg(oi.name_translations))[1], '{}'::jsonb) AS item_name_translations,
            SUM(oi.quantity)::bigint   AS quantity_sold,
            SUM(oi.line_total)::bigint AS revenue
        FROM order_items oi
        JOIN orders o     ON o.id  = oi.order_id
        LEFT JOIN menu_items m ON m.id  = oi.menu_item_id
        LEFT JOIN categories c ON c.id = m.category_id
        WHERE o.branch_id = ANY($1) AND o.status != 'voided'
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY
            CASE 
                WHEN oi.bundle_id IS NOT NULL THEN '00000000-0000-0000-0000-000000000000'::uuid
                ELSE m.category_id
            END,
            COALESCE(c.name, CASE WHEN oi.bundle_id IS NOT NULL THEN 'Bundles' ELSE 'Uncategorized' END),
            COALESCE(oi.menu_item_id, oi.bundle_id),
            oi.item_name
        ORDER BY category_name NULLS LAST, revenue DESC
        "#,
    )
    .bind(&branch_ids).bind(query.from).bind(query.to)
    .fetch_all(pool.get_ref()).await?;

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

    Ok(HttpResponse::Ok().json(BranchSalesReport {
        branch_id: *branch_id,
        branch_name,
        from: query.from,
        to: query.to,
        total_orders: totals.0,
        voided_orders: totals.1,
        subtotal: totals.2,
        total_discount: totals.3,
        total_tax: totals.4,
        total_revenue: totals.5,
        total_line_items: totals.6,
        revenue_by_method: totals.7,
        top_items,
        by_category,
    }))
}

// ── GET /reports/branches/:id/stock ──────────────────────────

#[utoipa::path(
    get,
    path = "/reports/branches/{branch_id}/stock",
    tag = "reports",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    responses((status = 200, description = "Branch stock", body = BranchStockReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn branch_stock(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    let (branch_ids, _org) =
        resolve_report_branches(pool.get_ref(), &claims, &req, *branch_id).await?;
    let branch_name = branch_label(pool.get_ref(), *branch_id).await?;

    // For a single branch each branch_inventory row is kept (its id drives
    // stock adjustments). "All branches" (nil) has no single row id, so it
    // rolls every branch's stock up per ingredient (id = nil placeholder).
    let items = if branch_id.is_nil() {
        sqlx::query_as::<_, StockRow>(
            r#"
            SELECT
                '00000000-0000-0000-0000-000000000000'::uuid AS branch_inventory_id,
                oi.name            AS ingredient_name,
                oi.unit::text      AS unit,
                SUM(bi.current_stock)::float8     AS current_stock,
                SUM(bi.reorder_threshold)::float8 AS reorder_threshold,
                oi.cost_per_unit,
                (SUM(bi.reorder_threshold) > 0 AND SUM(bi.current_stock) <= SUM(bi.reorder_threshold)) AS below_reorder
            FROM branch_inventory bi
            JOIN org_ingredients oi ON oi.id = bi.org_ingredient_id
            WHERE bi.branch_id = ANY($1)
            GROUP BY oi.id, oi.name, oi.unit, oi.cost_per_unit
            ORDER BY below_reorder DESC, oi.name ASC
            "#,
        )
        .bind(&branch_ids)
        .fetch_all(pool.get_ref()).await?
    } else {
        sqlx::query_as::<_, StockRow>(
            r#"
            SELECT
                bi.id              AS branch_inventory_id,
                oi.name            AS ingredient_name,
                oi.unit::text      AS unit,
                bi.current_stock::float8,
                bi.reorder_threshold::float8,
                COALESCE(bi.cost_per_unit, oi.cost_per_unit) AS cost_per_unit,
                (bi.reorder_threshold > 0 AND bi.current_stock <= bi.reorder_threshold) AS below_reorder
            FROM branch_inventory bi
            JOIN org_ingredients oi ON oi.id = bi.org_ingredient_id
            WHERE bi.branch_id = ANY($1)
            ORDER BY (bi.reorder_threshold > 0 AND bi.current_stock <= bi.reorder_threshold) DESC, oi.name ASC
            "#,
        )
        .bind(&branch_ids)
        .fetch_all(pool.get_ref()).await?
    };

    Ok(HttpResponse::Ok().json(BranchStockReport {
        branch_id: *branch_id,
        branch_name,
        items,
    }))
}

// ── GET /reports/branches/:id/sales/timeseries ───────────────

#[utoipa::path(
    get,
    path = "/reports/branches/{branch_id}/sales/timeseries",
    tag = "reports",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    params(TimeseriesQuery),
    responses((status = 200, description = "Branch sales timeseries", body = Vec<TimeseriesPoint>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn branch_sales_timeseries(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query: web::Query<TimeseriesQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    let (branch_ids, org) =
        resolve_report_branches(pool.get_ref(), &claims, &req, *branch_id).await?;

    // Resolve the bucketing timezone as branch → org → Africa/Cairo. For a
    // specific branch this is its effective tz; for "all branches" (nil UUID)
    // the branch subquery is NULL and we bucket every branch in one consistent
    // org-level zone.
    let tz: String = sqlx::query_scalar(
        "SELECT COALESCE(
            (SELECT timezone::text FROM branches WHERE id = $1 AND deleted_at IS NULL),
            (SELECT timezone::text FROM organizations WHERE id = $2),
            'Africa/Cairo'
         )",
    )
    .bind(*branch_id)
    .bind(org)
    .fetch_one(pool.get_ref())
    .await?;

    let trunc = match query.granularity.as_deref().unwrap_or("daily") {
        "hourly" => "hour",
        "monthly" => "month",
        _ => "day",
    };

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
                COUNT(o.id)   FILTER (WHERE o.status != 'voided')::bigint  AS orders,
                COALESCE(SUM(o.total_amount)    FILTER (WHERE o.status != 'voided'), 0)::bigint AS revenue,
                COUNT(o.id)   FILTER (WHERE o.status  = 'voided')::bigint  AS voided,
                COALESCE(SUM(o.discount_amount) FILTER (WHERE o.status != 'voided'), 0)::bigint AS discount,
                COALESCE(SUM(o.tax_amount)      FILTER (WHERE o.status != 'voided'), 0)::bigint AS tax
            FROM orders o
            WHERE o.branch_id = ANY($1)
              AND ($2::timestamptz IS NULL OR o.created_at >= $2)
              AND ($3::timestamptz IS NULL OR o.created_at <= $3)
            GROUP BY date_trunc('{trunc}', o.created_at AT TIME ZONE $4)
        )
        SELECT
            p.period_str AS period,
            p.orders,
            p.revenue,
            p.voided,
            p.discount,
            p.tax,
            COALESCE((
              SELECT json_object_agg(method, rev) FROM (
                SELECT op2.method, SUM(op2.amount)::bigint AS rev
                FROM order_payments op2
                JOIN orders o2 ON o2.id = op2.order_id
                WHERE o2.branch_id = ANY($1) AND o2.status != 'voided'
                  AND date_trunc('{trunc}', o2.created_at AT TIME ZONE $4) = p.period_val
                GROUP BY op2.method
              ) sub
            ), '{{}}'::json) AS revenue_by_method
        FROM periods p
        ORDER BY p.period_val ASC
        "#,
        trunc = trunc,
    );

    let rows = sqlx::query_as::<_, TimeseriesPoint>(&sql)
        .bind(&branch_ids)
        .bind(query.from)
        .bind(query.to)
        .bind(&tz)
        .fetch_all(pool.get_ref())
        .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── GET /reports/branches/:id/sales/peak-hours ───────────────

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct PeakHourPoint {
    pub hour: i32,
    pub orders: i64,
    pub revenue: i64,
    pub voided: i64,
    pub discount: i64,
    pub tax: i64,
    /// Revenue in piastres averaged over the number of calendar days in the queried range.
    pub avg_revenue_per_day: i64,
    /// Orders averaged over the number of calendar days (may be fractional).
    pub avg_orders_per_day: f64,
    /// This hour's revenue as a percentage of the period total (0–100, 1 dp).
    pub revenue_pct: f64,
    /// This hour's orders as a percentage of the period total (0–100, 1 dp).
    pub orders_pct: f64,
}

#[utoipa::path(
    get,
    path = "/reports/branches/{branch_id}/sales/peak-hours",
    tag = "reports",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    params(DateRangeQuery),
    responses((status = 200, description = "Peak hours aggregation (24 rows)", body = Vec<PeakHourPoint>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn branch_sales_peak_hours(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    let (branch_ids, org) =
        resolve_report_branches(pool.get_ref(), &claims, &req, *branch_id).await?;

    let tz: String = sqlx::query_scalar(
        "SELECT COALESCE(
            (SELECT timezone::text FROM branches WHERE id = $1 AND deleted_at IS NULL),
            (SELECT timezone::text FROM organizations WHERE id = $2),
            'Africa/Cairo'
         )",
    )
    .bind(*branch_id)
    .bind(org)
    .fetch_one(pool.get_ref())
    .await?;

    // Always return all 24 hours so the chart has a complete x-axis.
    // Also surfaces per-day averages and share-of-total percentages for each bucket.
    let rows = sqlx::query_as::<_, PeakHourPoint>(
        r#"
        WITH all_hours AS (
            SELECT generate_series(0, 23)::int AS hour
        ),
        -- Number of calendar days in the requested range (branch-local time).
        -- Uses the explicit date span when from/to are bound; otherwise counts
        -- distinct order dates so we still normalise correctly for open ranges.
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
                COUNT(o.id)   FILTER (WHERE o.status != 'voided')::bigint  AS orders,
                COALESCE(SUM(o.total_amount)    FILTER (WHERE o.status != 'voided'), 0)::bigint AS revenue,
                COUNT(o.id)   FILTER (WHERE o.status  = 'voided')::bigint  AS voided,
                COALESCE(SUM(o.discount_amount) FILTER (WHERE o.status != 'voided'), 0)::bigint AS discount,
                COALESCE(SUM(o.tax_amount)      FILTER (WHERE o.status != 'voided'), 0)::bigint AS tax
            FROM orders o
            WHERE o.branch_id = ANY($1)
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
            h.hour,
            COALESCE(a.orders,   0)::bigint AS orders,
            COALESCE(a.revenue,  0)::bigint AS revenue,
            COALESCE(a.voided,   0)::bigint AS voided,
            COALESCE(a.discount, 0)::bigint AS discount,
            COALESCE(a.tax,      0)::bigint AS tax,
            ROUND(COALESCE(a.revenue, 0)::numeric / d.days)::bigint        AS avg_revenue_per_day,
            (COALESCE(a.orders,  0)::float8 / d.days::float8)              AS avg_orders_per_day,
            CASE WHEN t.total_revenue > 0
                 THEN ROUND(COALESCE(a.revenue, 0)::numeric / t.total_revenue * 100, 1)::float8
                 ELSE 0.0::float8 END                                       AS revenue_pct,
            CASE WHEN t.total_orders > 0
                 THEN ROUND(COALESCE(a.orders,  0)::numeric / t.total_orders  * 100, 1)::float8
                 ELSE 0.0::float8 END                                       AS orders_pct
        FROM all_hours h
        CROSS JOIN day_count d
        CROSS JOIN totals t
        LEFT JOIN aggregated a ON a.hour = h.hour
        ORDER BY h.hour ASC
        "#,
    )
    .bind(&branch_ids)
    .bind(query.from)
    .bind(query.to)
    .bind(&tz)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── GET /reports/branches/:id/tellers ────────────────────────

#[utoipa::path(
    get,
    path = "/reports/branches/{branch_id}/tellers",
    tag = "reports",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    params(DateRangeQuery),
    responses((status = 200, description = "Branch teller stats", body = Vec<TellerStats>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn branch_teller_stats(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    let (branch_ids, _org) =
        resolve_report_branches(pool.get_ref(), &claims, &req, *branch_id).await?;

    let rows = sqlx::query_as::<_, TellerStats>(
        r#"
        SELECT
            o.teller_id,
            u.name AS teller_name,
            COUNT(o.id) FILTER (WHERE o.status != 'voided')::bigint AS orders,
            COALESCE(SUM(o.total_amount) FILTER (WHERE o.status != 'voided'), 0)::bigint AS revenue,
            CASE
                WHEN COUNT(o.id) FILTER (WHERE o.status != 'voided') = 0 THEN 0
                ELSE (
                    COALESCE(SUM(o.total_amount) FILTER (WHERE o.status != 'voided'), 0)
                    / COUNT(o.id) FILTER (WHERE o.status != 'voided')
                )::bigint
            END AS avg_order_value,
            COUNT(o.id) FILTER (WHERE o.status = 'voided')::bigint AS voided,
            COUNT(DISTINCT o.shift_id)::bigint AS shifts
        FROM orders o
        JOIN users u ON u.id = o.teller_id
        WHERE o.branch_id = ANY($1)
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY o.teller_id, u.name
        ORDER BY revenue DESC
        "#,
    )
    .bind(&branch_ids)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── GET /reports/branches/:id/waiters ────────────────────────

#[utoipa::path(
    get,
    path = "/reports/branches/{branch_id}/waiters",
    tag = "reports",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    params(DateRangeQuery),
    responses((status = 200, description = "Branch waiter stats", body = WaiterStatsReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn branch_waiter_stats(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    let (branch_ids, _org) =
        resolve_report_branches(pool.get_ref(), &claims, &req, *branch_id).await?;

    // Served by the partial index idx_orders_waiter (WHERE waiter_id IS NOT NULL).
    let waiters = sqlx::query_as::<_, WaiterStats>(
        r#"
        SELECT
            o.waiter_id,
            w.name AS waiter_name,
            COUNT(o.id) FILTER (WHERE o.status != 'voided')::bigint AS orders,
            COALESCE(SUM(o.total_amount) FILTER (WHERE o.status != 'voided'), 0)::bigint AS revenue,
            CASE
                WHEN COUNT(o.id) FILTER (WHERE o.status != 'voided') = 0 THEN 0
                ELSE (
                    COALESCE(SUM(o.total_amount) FILTER (WHERE o.status != 'voided'), 0)
                    / COUNT(o.id) FILTER (WHERE o.status != 'voided')
                )::bigint
            END AS avg_order_value,
            COUNT(o.id) FILTER (WHERE o.status = 'voided')::bigint AS voided,
            COALESCE(SUM(iq.qty) FILTER (WHERE o.status != 'voided'), 0)::bigint AS line_items,
            CASE
                WHEN COUNT(o.id) FILTER (WHERE o.status != 'voided') = 0 THEN 0::float8
                ELSE COALESCE(SUM(iq.qty) FILTER (WHERE o.status != 'voided'), 0)::float8
                     / COUNT(o.id) FILTER (WHERE o.status != 'voided')
            END AS avg_items_per_order
        FROM orders o
        JOIN users w ON w.id = o.waiter_id
        LEFT JOIN LATERAL (
            SELECT SUM(oi.quantity)::bigint AS qty
            FROM order_items oi WHERE oi.order_id = o.id
        ) iq ON true
        WHERE o.waiter_id IS NOT NULL
          AND o.branch_id = ANY($1)
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY o.waiter_id, w.name
        ORDER BY revenue DESC
        "#,
    )
    .bind(&branch_ids)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    let (attributed_orders, total_orders): (i64, i64) = sqlx::query_as(
        r#"
        SELECT
            COUNT(*) FILTER (WHERE waiter_id IS NOT NULL)::bigint,
            COUNT(*)::bigint
        FROM orders
        WHERE status != 'voided'
          AND branch_id = ANY($1)
          AND ($2::timestamptz IS NULL OR created_at >= $2)
          AND ($3::timestamptz IS NULL OR created_at <= $3)
        "#,
    )
    .bind(&branch_ids)
    .bind(query.from)
    .bind(query.to)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(WaiterStatsReport {
        waiters,
        attributed_orders,
        total_orders,
    }))
}

// ── GET /reports/branches/:id/addons ─────────────────────────

#[utoipa::path(
    get,
    path = "/reports/branches/{branch_id}/addons",
    tag = "reports",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    params(DateRangeQuery),
    responses((status = 200, description = "Branch addon sales", body = Vec<AddonSalesRow>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn branch_addon_sales(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    let (branch_ids, _org) =
        resolve_report_branches(pool.get_ref(), &claims, &req, *branch_id).await?;

    let rows = sqlx::query_as::<_, AddonSalesRow>(
        r#"
        SELECT
            oia.addon_item_id,
            oia.addon_name,
            COALESCE((array_agg(oia.name_translations))[1], '{}'::jsonb) AS addon_name_translations,
            COALESCE(ai.type, 'extra') AS addon_type,
            SUM(oia.quantity)::bigint  AS quantity_sold,
            SUM(oia.line_total)::bigint AS revenue
        FROM order_item_addons oia
        JOIN order_items oi ON oi.id  = oia.order_item_id
        JOIN orders o       ON o.id   = oi.order_id
        LEFT JOIN addon_items ai ON ai.id = oia.addon_item_id
        WHERE o.branch_id = ANY($1)
          AND o.status != 'voided'
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY oia.addon_item_id, oia.addon_name, ai.type
        ORDER BY quantity_sold DESC
        "#,
    )
    .bind(&branch_ids)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── GET /reports/orgs/:org_id/comparison ─────────────────────

#[utoipa::path(
    get,
    path = "/reports/orgs/{org_id}/comparison",
    tag = "reports",
    params(("org_id" = Uuid, Path, description = "Org ID")),
    params(DateRangeQuery),
    responses((status = 200, description = "Org comparison report", body = OrgComparisonReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn org_branch_comparison(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    org_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;

    if claims.role != UserRole::SuperAdmin && claims.org_id() != Some(*org_id) {
        return Err(AppError::Forbidden("Not your org".into()));
    }

    #[derive(sqlx::FromRow)]
    struct Row {
        branch_id: Uuid,
        branch_name: String,
        total_orders: i64,
        voided_orders: i64,
        total_revenue: i64,
        revenue_by_method: serde_json::Value,
    }

    let rows = sqlx::query_as::<_, Row>(
        r#"
        SELECT
            b.id   AS branch_id,
            b.name AS branch_name,
            COUNT(DISTINCT o.id) FILTER (WHERE o.status != 'voided')::bigint AS total_orders,
            COUNT(DISTINCT o.id) FILTER (WHERE o.status  = 'voided')::bigint AS voided_orders,
            COALESCE(SUM(o.total_amount) FILTER (WHERE o.status != 'voided'), 0)::bigint AS total_revenue,
            COALESCE((
              SELECT json_object_agg(method, rev) FROM (
                SELECT op.method, SUM(op.amount)::bigint AS rev
                FROM order_payments op
                JOIN orders o2 ON o2.id = op.order_id
                WHERE o2.branch_id = b.id AND o2.status != 'voided'
                  AND ($2::timestamptz IS NULL OR o2.created_at >= $2)
                  AND ($3::timestamptz IS NULL OR o2.created_at <= $3)
                GROUP BY op.method
              ) sub
            ), '{}'::json) AS revenue_by_method
        FROM branches b
        LEFT JOIN orders o          ON o.branch_id = b.id
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        WHERE b.org_id = $1 AND b.deleted_at IS NULL
        GROUP BY b.id, b.name
        ORDER BY total_revenue DESC
        "#,
    )
    .bind(*org_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    let branches = rows
        .into_iter()
        .map(|r| BranchComparison {
            branch_id: r.branch_id,
            branch_name: r.branch_name,
            total_orders: r.total_orders,
            voided_orders: r.voided_orders,
            total_revenue: r.total_revenue,
            revenue_by_method: r.revenue_by_method,
            avg_order_value: if r.total_orders == 0 {
                0
            } else {
                r.total_revenue / r.total_orders
            },
            void_rate_pct: if (r.total_orders + r.voided_orders) == 0 {
                0.0
            } else {
                r.voided_orders as f64 / (r.total_orders + r.voided_orders) as f64 * 100.0
            },
        })
        .collect();

    Ok(HttpResponse::Ok().json(OrgComparisonReport {
        org_id: *org_id,
        from: query.from,
        to: query.to,
        branches,
    }))
}

// ── GET /reports/branches/:branch_id/delivery-sales ──────────

/// Delivery sales for one delivery channel (`in_mall` / `outside`). Revenue and
/// order counts are over **delivered** orders only; `cancelled_orders` is shown
/// separately so the UI can surface drop-off without inflating revenue.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct DeliveryChannelSales {
    /// Delivery channel: `in_mall` or `outside`.
    pub channel: String,
    pub orders: i64,
    /// Sum of `total` (piastres) over delivered orders on this channel.
    pub revenue: i64,
    /// Sum of `delivery_fee` (piastres) over delivered orders.
    pub delivery_fees: i64,
    pub avg_order_value: i64,
    pub cancelled_orders: i64,
}

/// Delivery sales rolled up across channels, plus a per-channel breakdown.
/// Always returns both `in_mall` and `outside` channels (zero-filled) so the
/// dashboard renders a stable shape.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct DeliverySalesReport {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub total_orders: i64,
    pub total_revenue: i64,
    pub total_delivery_fees: i64,
    pub cancelled_orders: i64,
    pub avg_order_value: i64,
    pub channels: Vec<DeliveryChannelSales>,
}

#[utoipa::path(
    get,
    path = "/reports/branches/{branch_id}/delivery-sales",
    tag = "reports",
    params(("branch_id" = Uuid, Path, description = "Branch ID, or the nil UUID for all branches in scope")),
    params(DateRangeQuery),
    responses((status = 200, description = "Delivery + per-channel sales", body = DeliverySalesReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn branch_delivery_sales(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    let (branch_ids, _org) =
        resolve_report_branches(pool.get_ref(), &claims, &req, *branch_id).await?;

    #[derive(sqlx::FromRow)]
    struct Row {
        channel: String,
        orders: i64,
        revenue: i64,
        delivery_fees: i64,
        cancelled_orders: i64,
    }

    let rows = sqlx::query_as::<_, Row>(
        r#"
        SELECT
            channel::text                                                              AS channel,
            COUNT(*) FILTER (WHERE status = 'delivered')::bigint                        AS orders,
            COALESCE(SUM(total)        FILTER (WHERE status = 'delivered'), 0)::bigint  AS revenue,
            COALESCE(SUM(delivery_fee) FILTER (WHERE status = 'delivered'), 0)::bigint  AS delivery_fees,
            COUNT(*) FILTER (WHERE status = 'cancelled')::bigint                        AS cancelled_orders
        FROM delivery_orders
        WHERE branch_id = ANY($1)
          AND ($2::timestamptz IS NULL OR created_at >= $2)
          AND ($3::timestamptz IS NULL OR created_at <= $3)
        GROUP BY channel
        "#,
    )
    .bind(&branch_ids)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    // Always emit both channels in a fixed order, zero-filling any that had no
    // orders in the period, so the dashboard shape never shifts.
    let channels: Vec<DeliveryChannelSales> = ["in_mall", "outside"]
        .iter()
        .map(|&name| {
            let row = rows.iter().find(|r| r.channel == name);
            let orders = row.map(|r| r.orders).unwrap_or(0);
            let revenue = row.map(|r| r.revenue).unwrap_or(0);
            DeliveryChannelSales {
                channel: name.to_string(),
                orders,
                revenue,
                delivery_fees: row.map(|r| r.delivery_fees).unwrap_or(0),
                avg_order_value: if orders == 0 { 0 } else { revenue / orders },
                cancelled_orders: row.map(|r| r.cancelled_orders).unwrap_or(0),
            }
        })
        .collect();

    let total_orders: i64 = channels.iter().map(|c| c.orders).sum();
    let total_revenue: i64 = channels.iter().map(|c| c.revenue).sum();
    let total_delivery_fees: i64 = channels.iter().map(|c| c.delivery_fees).sum();
    let cancelled_orders: i64 = channels.iter().map(|c| c.cancelled_orders).sum();

    Ok(HttpResponse::Ok().json(DeliverySalesReport {
        from: query.from,
        to: query.to,
        total_orders,
        total_revenue,
        total_delivery_fees,
        cancelled_orders,
        avg_order_value: if total_orders == 0 {
            0
        } else {
            total_revenue / total_orders
        },
        channels,
    }))
}

// ── Inventory valuation / low-stock / consumption / waste ─────

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct ValuationRow {
    pub org_ingredient_id: Uuid,
    pub ingredient_name: String,
    pub unit: String,
    pub current_stock: f64,
    /// Piastres per unit; `null` ⟺ unknown.
    pub cost_per_unit: Option<i64>,
    /// current_stock × cost_per_unit in piastres; `null` when cost unknown.
    pub value: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct InventoryValuationReport {
    pub total_value: i64,
    pub unknown_cost_count: i64,
    pub items: Vec<ValuationRow>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct LowStockRow {
    pub branch_id: Uuid,
    pub branch_name: String,
    pub org_ingredient_id: Uuid,
    pub ingredient_name: String,
    pub unit: String,
    pub current_stock: f64,
    pub reorder_threshold: f64,
    /// reorder_threshold − current_stock: how much to order to reach par.
    pub deficit: f64,
    /// Default supplier for this ingredient (for one-click "create PO"); may be null.
    pub supplier_id: Option<Uuid>,
    pub supplier_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct ConsumptionRow {
    pub org_ingredient_id: Uuid,
    pub ingredient_name: String,
    pub unit: String,
    pub consumed_qty: f64,
    /// Consumption valued in piastres; `null` if any contributing cost unknown.
    pub consumed_value: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct WasteReportRow {
    pub reason: String,
    pub org_ingredient_id: Uuid,
    pub ingredient_name: String,
    pub unit: String,
    pub waste_qty: f64,
    pub waste_value: Option<i64>,
}

// ── GET /reports/branches/:id/inventory-valuation ────────────

#[utoipa::path(
    get,
    path = "/reports/branches/{branch_id}/inventory-valuation",
    tag = "reports",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    responses((status = 200, description = "Branch inventory valuation", body = InventoryValuationReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn branch_inventory_valuation(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    let (branch_ids, _org) =
        resolve_report_branches(pool.get_ref(), &claims, &req, *branch_id).await?;

    // Summed per ingredient over the selected branch(es): one branch has a
    // single row per ingredient so the SUM is a no-op; "all branches" (nil)
    // rolls every branch's stock together, matching the org valuation report.
    let items = sqlx::query_as::<_, ValuationRow>(
        r#"
        SELECT oi.id AS org_ingredient_id, oi.name AS ingredient_name, oi.unit::text AS unit,
               SUM(bi.current_stock)::float8 AS current_stock,
               -- effective (stock-weighted) cost across the branch(es); each
               -- branch's stock is valued at its OWN actual cost (org default
               -- fallback), so value/qty is the blended cost.
               round(CASE WHEN SUM(bi.current_stock) <> 0
                          THEN SUM(bi.current_stock * COALESCE(bi.cost_per_unit, oi.cost_per_unit))
                               / NULLIF(SUM(bi.current_stock), 0)
                          ELSE oi.cost_per_unit END)::bigint AS cost_per_unit,
               CASE WHEN bool_or(COALESCE(bi.cost_per_unit, oi.cost_per_unit) IS NULL) THEN NULL
                    ELSE round(SUM(bi.current_stock * COALESCE(bi.cost_per_unit, oi.cost_per_unit)))::bigint
               END AS value
        FROM branch_inventory bi
        JOIN org_ingredients oi ON oi.id = bi.org_ingredient_id
        WHERE bi.branch_id = ANY($1)
        GROUP BY oi.id, oi.name, oi.unit, oi.cost_per_unit
        ORDER BY oi.name ASC
        "#,
    )
    .bind(&branch_ids)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(summarize_valuation(items)))
}

// ── GET /reports/orgs/:id/inventory-valuation ────────────────

#[utoipa::path(
    get,
    path = "/reports/orgs/{org_id}/inventory-valuation",
    tag = "reports",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    responses((status = 200, description = "Org inventory valuation (all branches)", body = InventoryValuationReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn org_inventory_valuation(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    org_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_org(&claims, *org_id)?;

    let items = sqlx::query_as::<_, ValuationRow>(
        r#"
        SELECT oi.id AS org_ingredient_id, oi.name AS ingredient_name, oi.unit::text AS unit,
               SUM(bi.current_stock)::float8 AS current_stock,
               round(CASE WHEN SUM(bi.current_stock) <> 0
                          THEN SUM(bi.current_stock * COALESCE(bi.cost_per_unit, oi.cost_per_unit))
                               / NULLIF(SUM(bi.current_stock), 0)
                          ELSE oi.cost_per_unit END)::bigint AS cost_per_unit,
               CASE WHEN bool_or(COALESCE(bi.cost_per_unit, oi.cost_per_unit) IS NULL) THEN NULL
                    ELSE round(SUM(bi.current_stock * COALESCE(bi.cost_per_unit, oi.cost_per_unit)))::bigint
               END AS value
        FROM branch_inventory bi
        JOIN branches b        ON b.id = bi.branch_id AND b.org_id = $1 AND b.deleted_at IS NULL
        JOIN org_ingredients oi ON oi.id = bi.org_ingredient_id
        GROUP BY oi.id, oi.name, oi.unit, oi.cost_per_unit
        ORDER BY oi.name ASC
        "#,
    )
    .bind(*org_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(summarize_valuation(items)))
}

// ── GET /reports/orgs/:id/low-stock ──────────────────────────

#[utoipa::path(
    get,
    path = "/reports/orgs/{org_id}/low-stock",
    tag = "reports",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    responses((status = 200, description = "Below-reorder items across branches", body = Vec<LowStockRow>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn org_low_stock(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    org_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_org(&claims, *org_id)?;

    let rows = sqlx::query_as::<_, LowStockRow>(
        r#"
        SELECT bi.branch_id, b.name AS branch_name, bi.org_ingredient_id,
               oi.name AS ingredient_name, oi.unit::text AS unit,
               bi.current_stock::float8, bi.reorder_threshold::float8,
               (bi.reorder_threshold - bi.current_stock)::float8 AS deficit,
               oi.supplier_id,
               (SELECT name FROM suppliers WHERE id = oi.supplier_id) AS supplier_name
        FROM branch_inventory bi
        JOIN branches b        ON b.id = bi.branch_id AND b.org_id = $1 AND b.deleted_at IS NULL
        JOIN org_ingredients oi ON oi.id = bi.org_ingredient_id
        WHERE bi.reorder_threshold > 0 AND bi.current_stock <= bi.reorder_threshold
        ORDER BY b.name ASC, oi.name ASC
        "#,
    )
    .bind(*org_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── GET /reports/branches/:id/low-stock ──────────────────────

#[utoipa::path(
    get,
    path = "/reports/branches/{branch_id}/low-stock",
    tag = "reports",
    params(("branch_id" = Uuid, Path, description = "Branch ID, or the all-zeros UUID for every branch in the org")),
    responses((status = 200, description = "Below-reorder items for one branch (or all branches)", body = Vec<LowStockRow>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn branch_low_stock(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    let (branch_ids, _org) =
        resolve_report_branches(pool.get_ref(), &claims, &req, *branch_id).await?;

    // Each row carries its own branch_id/branch_name, so a single-branch call is
    // genuinely scoped to that branch and an "all branches" (nil) call still
    // attributes every below-reorder line to the branch it belongs to.
    let rows = sqlx::query_as::<_, LowStockRow>(
        r#"
        SELECT bi.branch_id, b.name AS branch_name, bi.org_ingredient_id,
               oi.name AS ingredient_name, oi.unit::text AS unit,
               bi.current_stock::float8, bi.reorder_threshold::float8,
               (bi.reorder_threshold - bi.current_stock)::float8 AS deficit,
               oi.supplier_id,
               (SELECT name FROM suppliers WHERE id = oi.supplier_id) AS supplier_name
        FROM branch_inventory bi
        JOIN branches b         ON b.id = bi.branch_id AND b.deleted_at IS NULL
        JOIN org_ingredients oi ON oi.id = bi.org_ingredient_id
        WHERE bi.branch_id = ANY($1)
          AND bi.reorder_threshold > 0 AND bi.current_stock <= bi.reorder_threshold
        ORDER BY b.name ASC, oi.name ASC
        "#,
    )
    .bind(&branch_ids)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── GET /reports/branches/:id/consumption ────────────────────

#[utoipa::path(
    get,
    path = "/reports/branches/{branch_id}/consumption",
    tag = "reports",
    params(("branch_id" = Uuid, Path, description = "Branch ID"), DateRangeQuery),
    responses((status = 200, description = "Ingredient consumption over a date range", body = Vec<ConsumptionRow>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn branch_consumption(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    let (branch_ids, _org) =
        resolve_report_branches(pool.get_ref(), &claims, &req, *branch_id).await?;

    let rows = sqlx::query_as::<_, ConsumptionRow>(
        r#"
        SELECT m.org_ingredient_id, oi.name AS ingredient_name, oi.unit::text AS unit,
               (-SUM(m.quantity))::float8 AS consumed_qty,
               CASE WHEN bool_or(m.unit_cost IS NULL) THEN NULL
                    ELSE round(SUM(-m.quantity * m.unit_cost))::bigint END AS consumed_value
        FROM inventory_movements m
        JOIN org_ingredients oi ON oi.id = m.org_ingredient_id
        WHERE m.branch_id = ANY($1)
          -- void_restock (positive qty) nets out a voided-and-restocked sale's
          -- negative 'sale' movement so consumption reflects real usage.
          AND m.type IN ('sale','waste','void_restock')
          AND ($2::timestamptz IS NULL OR m.created_at >= $2)
          AND ($3::timestamptz IS NULL OR m.created_at <= $3)
        GROUP BY m.org_ingredient_id, oi.name, oi.unit
        ORDER BY consumed_qty DESC
        "#,
    )
    .bind(&branch_ids)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── GET /reports/branches/:id/waste-report ───────────────────

#[utoipa::path(
    get,
    path = "/reports/branches/{branch_id}/waste-report",
    tag = "reports",
    params(("branch_id" = Uuid, Path, description = "Branch ID"), DateRangeQuery),
    responses((status = 200, description = "Waste by reason and ingredient", body = Vec<WasteReportRow>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn branch_waste_report(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    let (branch_ids, _org) =
        resolve_report_branches(pool.get_ref(), &claims, &req, *branch_id).await?;

    let rows = sqlx::query_as::<_, WasteReportRow>(
        r#"
        SELECT COALESCE(m.reason, 'other') AS reason,
               m.org_ingredient_id, oi.name AS ingredient_name, oi.unit::text AS unit,
               (-SUM(m.quantity))::float8 AS waste_qty,
               CASE WHEN bool_or(m.unit_cost IS NULL) THEN NULL
                    ELSE round(SUM(-m.quantity * m.unit_cost))::bigint END AS waste_value
        FROM inventory_movements m
        JOIN org_ingredients oi ON oi.id = m.org_ingredient_id
        WHERE m.branch_id = ANY($1)
          AND m.type = 'waste'
          AND ($2::timestamptz IS NULL OR m.created_at >= $2)
          AND ($3::timestamptz IS NULL OR m.created_at <= $3)
        GROUP BY m.reason, m.org_ingredient_id, oi.name, oi.unit
        ORDER BY waste_qty DESC
        "#,
    )
    .bind(&branch_ids)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── GET /reports/orgs/:id/consumption ────────────────────────

#[utoipa::path(
    get,
    path = "/reports/orgs/{org_id}/consumption",
    tag = "reports",
    params(("org_id" = Uuid, Path, description = "Organization ID"), DateRangeQuery),
    responses((status = 200, description = "Ingredient consumption across the org", body = Vec<ConsumptionRow>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn org_consumption(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    org_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_org(&claims, *org_id)?;

    let rows = sqlx::query_as::<_, ConsumptionRow>(
        r#"
        SELECT m.org_ingredient_id, oi.name AS ingredient_name, oi.unit::text AS unit,
               (-SUM(m.quantity))::float8 AS consumed_qty,
               CASE WHEN bool_or(m.unit_cost IS NULL) THEN NULL
                    ELSE round(SUM(-m.quantity * m.unit_cost))::bigint END AS consumed_value
        FROM inventory_movements m
        JOIN org_ingredients oi ON oi.id = m.org_ingredient_id
        JOIN branches b ON b.id = m.branch_id AND b.org_id = $1 AND b.deleted_at IS NULL
        -- void_restock nets out voided-and-restocked sales (see branch_consumption).
        WHERE m.type IN ('sale','waste','void_restock')
          AND ($2::timestamptz IS NULL OR m.created_at >= $2)
          AND ($3::timestamptz IS NULL OR m.created_at <= $3)
        GROUP BY m.org_ingredient_id, oi.name, oi.unit
        ORDER BY consumed_qty DESC
        "#,
    )
    .bind(*org_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── GET /reports/orgs/:id/waste-report ───────────────────────

#[utoipa::path(
    get,
    path = "/reports/orgs/{org_id}/waste-report",
    tag = "reports",
    params(("org_id" = Uuid, Path, description = "Organization ID"), DateRangeQuery),
    responses((status = 200, description = "Waste by reason and ingredient across the org", body = Vec<WasteReportRow>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn org_waste_report(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    org_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_org(&claims, *org_id)?;

    let rows = sqlx::query_as::<_, WasteReportRow>(
        r#"
        SELECT COALESCE(m.reason, 'other') AS reason,
               m.org_ingredient_id, oi.name AS ingredient_name, oi.unit::text AS unit,
               (-SUM(m.quantity))::float8 AS waste_qty,
               CASE WHEN bool_or(m.unit_cost IS NULL) THEN NULL
                    ELSE round(SUM(-m.quantity * m.unit_cost))::bigint END AS waste_value
        FROM inventory_movements m
        JOIN org_ingredients oi ON oi.id = m.org_ingredient_id
        JOIN branches b ON b.id = m.branch_id AND b.org_id = $1 AND b.deleted_at IS NULL
        WHERE m.type = 'waste'
          AND ($2::timestamptz IS NULL OR m.created_at >= $2)
          AND ($3::timestamptz IS NULL OR m.created_at <= $3)
        GROUP BY m.reason, m.org_ingredient_id, oi.name, oi.unit
        ORDER BY waste_qty DESC
        "#,
    )
    .bind(*org_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct ShrinkageRow {
    /// The variance reason captured at finalize, or `unexplained` when none.
    pub reason: String,
    pub org_ingredient_id: Uuid,
    pub ingredient_name: String,
    pub unit: String,
    /// Quantity lost (positive number) from negative stock-count differences.
    pub shrinkage_qty: f64,
    /// Valued shrinkage in piastres; `null` when any contributing cost unknown.
    pub shrinkage_value: Option<i64>,
}

// ── GET /reports/branches/:id/shrinkage ──────────────────────

#[utoipa::path(
    get,
    path = "/reports/branches/{branch_id}/shrinkage",
    tag = "reports",
    params(("branch_id" = Uuid, Path, description = "Branch ID"), DateRangeQuery),
    responses((status = 200, description = "Stock-count shrinkage by reason", body = Vec<ShrinkageRow>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn branch_shrinkage(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    let (branch_ids, _org) =
        resolve_report_branches(pool.get_ref(), &claims, &req, *branch_id).await?;

    let rows = sqlx::query_as::<_, ShrinkageRow>(
        r#"
        SELECT COALESCE(m.reason, 'unexplained') AS reason,
               m.org_ingredient_id, oi.name AS ingredient_name, oi.unit::text AS unit,
               (-SUM(m.quantity))::float8 AS shrinkage_qty,
               CASE WHEN bool_or(m.unit_cost IS NULL) THEN NULL
                    ELSE round(SUM(-m.quantity * m.unit_cost))::bigint END AS shrinkage_value
        FROM inventory_movements m
        JOIN org_ingredients oi ON oi.id = m.org_ingredient_id
        WHERE m.branch_id = ANY($1) AND m.type = 'stock_count' AND m.quantity < 0
          AND ($2::timestamptz IS NULL OR m.created_at >= $2)
          AND ($3::timestamptz IS NULL OR m.created_at <= $3)
        GROUP BY COALESCE(m.reason, 'unexplained'), m.org_ingredient_id, oi.name, oi.unit
        ORDER BY shrinkage_qty DESC
        "#,
    )
    .bind(&branch_ids)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── GET /reports/orgs/:id/shrinkage ──────────────────────────

#[utoipa::path(
    get,
    path = "/reports/orgs/{org_id}/shrinkage",
    tag = "reports",
    params(("org_id" = Uuid, Path, description = "Organization ID"), DateRangeQuery),
    responses((status = 200, description = "Stock-count shrinkage by reason across the org", body = Vec<ShrinkageRow>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn org_shrinkage(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    org_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_org(&claims, *org_id)?;

    let rows = sqlx::query_as::<_, ShrinkageRow>(
        r#"
        SELECT COALESCE(m.reason, 'unexplained') AS reason,
               m.org_ingredient_id, oi.name AS ingredient_name, oi.unit::text AS unit,
               (-SUM(m.quantity))::float8 AS shrinkage_qty,
               CASE WHEN bool_or(m.unit_cost IS NULL) THEN NULL
                    ELSE round(SUM(-m.quantity * m.unit_cost))::bigint END AS shrinkage_value
        FROM inventory_movements m
        JOIN org_ingredients oi ON oi.id = m.org_ingredient_id
        JOIN branches b ON b.id = m.branch_id AND b.org_id = $1 AND b.deleted_at IS NULL
        WHERE m.type = 'stock_count' AND m.quantity < 0
          AND ($2::timestamptz IS NULL OR m.created_at >= $2)
          AND ($3::timestamptz IS NULL OR m.created_at <= $3)
        GROUP BY COALESCE(m.reason, 'unexplained'), m.org_ingredient_id, oi.name, oi.unit
        ORDER BY shrinkage_qty DESC
        "#,
    )
    .bind(*org_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

fn summarize_valuation(items: Vec<ValuationRow>) -> InventoryValuationReport {
    let mut total_value = 0i64;
    let mut unknown_cost_count = 0i64;
    for it in &items {
        match it.value {
            Some(v) => total_value += v,
            None => unknown_cost_count += 1,
        }
    }
    InventoryValuationReport {
        total_value,
        unknown_cost_count,
        items,
    }
}

fn require_org(claims: &Claims, org_id: Uuid) -> Result<(), AppError> {
    if claims.role != UserRole::SuperAdmin && claims.org_id() != Some(org_id) {
        return Err(AppError::Forbidden("Not your org".into()));
    }
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

async fn require_shift_branch_access(
    pool: &PgPool,
    claims: &Claims,
    shift_id: Uuid,
) -> Result<Uuid, AppError> {
    let branch_id: Option<Uuid> = sqlx::query_scalar("SELECT branch_id FROM shifts WHERE id = $1")
        .bind(shift_id)
        .fetch_optional(pool)
        .await?
        .flatten();

    let branch_id = branch_id.ok_or_else(|| AppError::NotFound("Shift not found".into()))?;
    require_branch_access(pool, claims, branch_id).await?;
    Ok(branch_id)
}

async fn require_branch_access(
    pool: &PgPool,
    claims: &Claims,
    branch_id: Uuid,
) -> Result<(), AppError> {
    if claims.role == UserRole::SuperAdmin {
        return Ok(());
    }

    let branch_org: Option<Uuid> =
        sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
            .bind(branch_id)
            .fetch_optional(pool)
            .await?
            .flatten();

    let branch_org = branch_org.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    if claims.org_id() != Some(branch_org) {
        return Err(AppError::Forbidden(
            "Branch belongs to a different org".into(),
        ));
    }

    if claims.role == UserRole::OrgAdmin {
        return Ok(());
    }

    let assigned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM user_branch_assignments WHERE user_id = $1 AND branch_id = $2)"
    )
    .bind(claims.user_id())
    .bind(branch_id)
    .fetch_one(pool)
    .await?;

    if !assigned {
        return Err(AppError::Forbidden("Not assigned to this branch".into()));
    }

    // A teller token is bound to the branch it authenticated for: a token minted
    // for one branch must not act on another, even when the teller is assigned to
    // both. The None guard keeps legacy/non-teller tokens working (V26).
    if claims.role == UserRole::Teller {
        if let Some(token_branch) = claims.branch_id()
            && token_branch != branch_id
        {
            return Err(AppError::Forbidden(
                "This device is signed in to a different branch.".into(),
            ));
        }
    }

    Ok(())
}

/// Resolve a report `{branch_id}` path param into the concrete set of branches
/// to report over, plus the owning org id. The all-zeros (nil) UUID means
/// "every branch in the caller's org" — the same scope as the `/reports/orgs/*`
/// endpoints; any other UUID means that one branch, after the usual access
/// check. The per-resource `check_permission` gate in each handler still
/// applies, so this does not widen who may read reports — only the scope.
/// `pub(crate)`: the insights module scopes its ledger the same way.
pub(crate) async fn resolve_report_branches(
    pool: &PgPool,
    claims: &Claims,
    req: &HttpRequest,
    branch_id: Uuid,
) -> Result<(Vec<Uuid>, Uuid), AppError> {
    if !branch_id.is_nil() {
        require_branch_access(pool, claims, branch_id).await?;
        let org: Uuid =
            sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
                .bind(branch_id)
                .fetch_optional(pool)
                .await?
                .flatten()
                .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
        return Ok((vec![branch_id], org));
    }

    let org = report_scope_org(claims, req)
        .ok_or_else(|| AppError::Forbidden("No organization in scope".into()))?;
    let ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM branches WHERE org_id = $1 AND deleted_at IS NULL")
            .bind(org)
            .fetch_all(pool)
            .await?;
    Ok((ids, org))
}

/// The org an "all branches" report rolls up over — the caller's token org, or
/// the dashboard's `X-Org-Id` header for super admins (see [`Claims::scope_org`]).
fn report_scope_org(claims: &Claims, req: &HttpRequest) -> Option<Uuid> {
    claims.scope_org(crate::auth::middleware::header_org_id(req))
}

/// Human label for a report scope: "All branches" for the nil UUID, otherwise
/// the branch's own name (404 if it does not exist).
async fn branch_label(pool: &PgPool, branch_id: Uuid) -> Result<String, AppError> {
    if branch_id.is_nil() {
        return Ok("All branches".to_string());
    }
    sqlx::query_scalar("SELECT name FROM branches WHERE id = $1 AND deleted_at IS NULL")
        .bind(branch_id)
        .fetch_optional(pool)
        .await?
        .flatten()
        .ok_or_else(|| AppError::NotFound("Branch not found".into()))
}

// ── Bundles Reporting ────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct BundleSalesRow {
    pub bundle_id: Option<Uuid>,
    pub bundle_name: String,
    pub quantity_sold: i64,
    pub revenue: i64,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct CombinedItemSalesRow {
    pub item_id: Option<Uuid>,
    pub item_name: String,
    #[schema(value_type = Object)]
    pub item_name_translations: serde_json::Value,
    pub standalone_qty: i64,
    pub bundle_qty: i64,
    pub total_qty: i64,
}

// GET /reports/branches/:id/bundles
#[utoipa::path(
    get,
    path = "/reports/branches/{branch_id}/bundles",
    tag = "reports",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    params(DateRangeQuery),
    responses((status = 200, description = "Branch bundle sales", body = Vec<BundleSalesRow>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn branch_bundle_sales(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    let (branch_ids, _org) =
        resolve_report_branches(pool.get_ref(), &claims, &req, *branch_id).await?;

    let rows = sqlx::query_as::<_, BundleSalesRow>(
        r#"
        SELECT
            oi.bundle_id AS bundle_id,
            oi.item_name AS bundle_name,
            SUM(oi.quantity)::bigint AS quantity_sold,
            SUM(oi.line_total)::bigint AS revenue
        FROM order_items oi
        JOIN orders o ON o.id = oi.order_id
        WHERE o.branch_id = ANY($1)
          AND o.status != 'voided'
          AND oi.bundle_id IS NOT NULL
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY oi.bundle_id, oi.item_name
        ORDER BY quantity_sold DESC
        "#,
    )
    .bind(&branch_ids)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// GET /reports/branches/:id/items-combined
#[utoipa::path(
    get,
    path = "/reports/branches/{branch_id}/items-combined",
    tag = "reports",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    params(DateRangeQuery),
    responses((status = 200, description = "Branch combined item sales", body = Vec<CombinedItemSalesRow>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn branch_combined_item_sales(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query: web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    let (branch_ids, _org) =
        resolve_report_branches(pool.get_ref(), &claims, &req, *branch_id).await?;

    let rows = sqlx::query_as::<_, CombinedItemSalesRow>(
        r#"
        WITH standalone_sales AS (
            SELECT
                oi.menu_item_id AS item_id,
                oi.item_name    AS item_name,
                COALESCE((array_agg(oi.name_translations))[1], '{}'::jsonb)
                    AS item_name_translations,
                SUM(oi.quantity)::bigint AS qty
            FROM order_items oi
            JOIN orders o ON o.id = oi.order_id
            WHERE o.branch_id = ANY($1)
              AND o.status != 'voided'
              AND oi.menu_item_id IS NOT NULL
              AND ($2::timestamptz IS NULL OR o.created_at >= $2)
              AND ($3::timestamptz IS NULL OR o.created_at <= $3)
            GROUP BY oi.menu_item_id, oi.item_name
        ),
        bundle_component_sales AS (
            SELECT
                bc.item_id   AS item_id,
                mi.name      AS item_name,
                COALESCE((array_agg(mi.name_translations))[1], '{}'::jsonb)
                    AS item_name_translations,
                SUM(oi.quantity * bc.quantity)::bigint AS qty
            FROM order_line_bundle_components bc
            JOIN order_items oi ON oi.id = bc.order_line_id
            JOIN orders o ON o.id = oi.order_id
            JOIN menu_items mi ON mi.id = bc.item_id
            WHERE o.branch_id = ANY($1)
              AND o.status != 'voided'
              AND oi.bundle_id IS NOT NULL
              AND ($2::timestamptz IS NULL OR o.created_at >= $2)
              AND ($3::timestamptz IS NULL OR o.created_at <= $3)
            GROUP BY bc.item_id, mi.name
        )
        SELECT
            COALESCE(s.item_id, b.item_id) AS item_id,
            COALESCE(s.item_name, b.item_name) AS item_name,
            COALESCE(s.item_name_translations, b.item_name_translations, '{}'::jsonb)
                AS item_name_translations,
            COALESCE(s.qty, 0)::bigint AS standalone_qty,
            COALESCE(b.qty, 0)::bigint AS bundle_qty,
            (COALESCE(s.qty, 0) + COALESCE(b.qty, 0))::bigint AS total_qty
        FROM standalone_sales s
        FULL OUTER JOIN bundle_component_sales b ON b.item_id = s.item_id
        ORDER BY total_qty DESC
        "#,
    )
    .bind(&branch_ids)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}
