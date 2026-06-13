use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
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
    pub from:  Option<DateTime<Utc>>,
    pub to:    Option<DateTime<Utc>>,
    pub limit: Option<i64>, // for top_items (default 20)
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TimeseriesQuery {
    pub from:        Option<DateTime<Utc>>,
    pub to:          Option<DateTime<Utc>>,
    pub granularity: Option<String>, // "hourly" | "daily" | "monthly"
}

// ── Response types ────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct ShiftSummary {
    pub shift_id:               Uuid,
    pub branch_id:              Uuid,
    pub branch_name:            String,
    pub teller_id:              Uuid,
    pub teller_name:            String,
    pub status:                 String,
    pub opened_at:              DateTime<Utc>,
    pub closed_at:              Option<DateTime<Utc>>,
    pub opening_cash:           i64,
    pub closing_cash_declared:  Option<i64>,
    pub closing_cash_system:    Option<i64>,
    pub cash_discrepancy:       Option<i64>,
    pub total_orders:           i64,
    pub voided_orders:          i64,
    pub total_revenue:          i64,
    pub revenue_by_method:      serde_json::Value,
    pub total_discount:         i64,
    pub total_tax:              i64,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct DeductionLogRow {
    pub id:                Uuid,
    pub order_id:          Option<Uuid>,
    pub order_item_id:     Option<Uuid>,
    pub inventory_item_id: Uuid,
    pub item_name:         String,
    pub unit:              String,
    pub quantity_deducted: f64,
    pub source:            String,
    pub created_at:        DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CategorySales {
    pub category_id:   Option<Uuid>,
    pub category_name: Option<String>,
    #[schema(value_type = Object)]
    pub category_name_translations: Option<serde_json::Value>,
    pub item_count:    i64,
    pub quantity_sold: i64,
    pub revenue:       i64,
    pub items:         Vec<ItemSales>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct ItemSales {
    pub menu_item_id:  Uuid,
    pub item_name:     String,
    #[schema(value_type = Object)]
    pub item_name_translations: serde_json::Value,
    pub quantity_sold: i64,
    pub revenue:       i64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct BranchSalesReport {
    pub branch_id:              Uuid,
    pub branch_name:            String,
    pub from:                   Option<DateTime<Utc>>,
    pub to:                     Option<DateTime<Utc>>,
    pub total_orders:           i64,
    pub voided_orders:          i64,
    pub subtotal:               i64,
    pub total_discount:         i64,
    pub total_tax:              i64,
    pub total_revenue:          i64,
    pub revenue_by_method:      serde_json::Value,
    pub top_items:              Vec<ItemSales>,
    pub by_category:            Vec<CategorySales>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct StockRow {
    pub branch_inventory_id: Uuid,
    pub ingredient_name:     String,
    pub unit:                String,
    pub current_stock:       f64,
    pub reorder_threshold:   f64,
    /// Piastres per unit; `null` ⟺ cost never entered.
    #[serde(with = "rust_decimal::serde::float_option")]
    #[schema(value_type = Option<f64>)]
    pub cost_per_unit:       Option<Decimal>,
    pub below_reorder:       bool,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct BranchStockReport {
    pub branch_id:   Uuid,
    pub branch_name: String,
    pub items:       Vec<StockRow>,
}

// Timeseries now includes per-payment-method breakdown
#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct TimeseriesPoint {
    pub period:                 String,
    pub orders:                 i64,
    pub revenue:                i64,
    pub voided:                 i64,
    pub discount:               i64,
    pub tax:                    i64,
    pub revenue_by_method:      serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct TellerStats {
    pub teller_id:       Uuid,
    pub teller_name:     String,
    pub orders:          i64,
    pub revenue:         i64,
    pub avg_order_value: i64,
    pub voided:          i64,
    pub shifts:          i64,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct AddonSalesRow {
    pub addon_item_id: Uuid,
    pub addon_name:    String,
    #[schema(value_type = Object)]
    pub addon_name_translations: serde_json::Value,
    pub addon_type:    String,
    pub quantity_sold: i64,
    pub revenue:       i64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct BranchComparison {
    pub branch_id:              Uuid,
    pub branch_name:            String,
    pub total_orders:           i64,
    pub voided_orders:          i64,
    pub total_revenue:          i64,
    pub revenue_by_method:      serde_json::Value,
    pub avg_order_value:        i64,
    pub void_rate_pct:          f64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OrgComparisonReport {
    pub org_id:   Uuid,
    pub from:     Option<DateTime<Utc>>,
    pub to:       Option<DateTime<Utc>>,
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
    req:      HttpRequest,
    pool:     web::Data<PgPool>,
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
        LEFT JOIN order_payments op ON op.order_id = o.id
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
    req:      HttpRequest,
    pool:     web::Data<PgPool>,
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
    params(DateRangeQuery),
    responses((status = 200, description = "Branch sales", body = BranchSalesReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn branch_sales(
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query:     web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    let branch_name: String = sqlx::query_scalar(
        "SELECT name FROM branches WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(*branch_id)
    .fetch_optional(pool.get_ref())
    .await?
    .flatten()
    .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    let totals: (i64, i64, i64, i64, i64, i64, serde_json::Value) = sqlx::query_as(
        r#"
        SELECT
            COUNT(*) FILTER (WHERE status != 'voided')::bigint,
            COUNT(*) FILTER (WHERE status = 'voided')::bigint,
            COALESCE(SUM(subtotal)        FILTER (WHERE status != 'voided'), 0)::bigint,
            COALESCE(SUM(discount_amount) FILTER (WHERE status != 'voided'), 0)::bigint,
            COALESCE(SUM(tax_amount)      FILTER (WHERE status != 'voided'), 0)::bigint,
            COALESCE(SUM(total_amount)    FILTER (WHERE status != 'voided'), 0)::bigint,
            COALESCE((
              SELECT json_object_agg(method, rev) FROM (
                SELECT op.method, SUM(op.amount)::bigint AS rev
                FROM order_payments op
                JOIN orders o2 ON o2.id = op.order_id
                WHERE o2.branch_id = $1 AND o2.status != 'voided'
                  AND ($2::timestamptz IS NULL OR o2.created_at >= $2)
                  AND ($3::timestamptz IS NULL OR o2.created_at <= $3)
                GROUP BY op.method
              ) sub
            ), '{}'::json)
        FROM orders
        WHERE branch_id = $1
          AND ($2::timestamptz IS NULL OR created_at >= $2)
          AND ($3::timestamptz IS NULL OR created_at <= $3)
        "#,
    )
    .bind(*branch_id).bind(query.from).bind(query.to)
    .fetch_one(pool.get_ref()).await?;

    let item_limit = query.limit.unwrap_or(20).clamp(1, 100);

    let top_items = sqlx::query_as::<_, ItemSales>(
        r#"
        SELECT COALESCE(oi.menu_item_id, oi.bundle_id) AS menu_item_id, oi.item_name,
               COALESCE((array_agg(oi.name_translations))[1], '{}'::jsonb) AS item_name_translations,
               SUM(oi.quantity)::bigint   AS quantity_sold,
               SUM(oi.line_total)::bigint AS revenue
        FROM order_items oi
        JOIN orders o ON o.id = oi.order_id
        WHERE o.branch_id = $1 AND o.status != 'voided'
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY COALESCE(oi.menu_item_id, oi.bundle_id), oi.item_name
        ORDER BY revenue DESC
        LIMIT $4
        "#,
    )
    .bind(*branch_id).bind(query.from).bind(query.to).bind(item_limit)
    .fetch_all(pool.get_ref()).await?;

    #[derive(sqlx::FromRow)]
    struct CategoryItemRow {
        category_id:   Option<Uuid>,
        category_name: Option<String>,
        category_name_translations: Option<serde_json::Value>,
        menu_item_id:  Uuid,
        item_name:     String,
        item_name_translations: serde_json::Value,
        quantity_sold: i64,
        revenue:       i64,
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
        WHERE o.branch_id = $1 AND o.status != 'voided'
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
    .bind(*branch_id).bind(query.from).bind(query.to)
    .fetch_all(pool.get_ref()).await?;

    let mut by_category: Vec<CategorySales> = Vec::new();
    for row in cat_rows {
        let item = ItemSales {
            menu_item_id:  row.menu_item_id,
            item_name:     row.item_name,
            item_name_translations: row.item_name_translations,
            quantity_sold: row.quantity_sold,
            revenue:       row.revenue,
        };
        match by_category.iter_mut().find(|c| c.category_id == row.category_id) {
            Some(cat) => {
                cat.item_count    += 1;
                cat.quantity_sold += item.quantity_sold;
                cat.revenue       += item.revenue;
                cat.items.push(item);
            }
            None => {
                by_category.push(CategorySales {
                    category_id:   row.category_id,
                    category_name: row.category_name,
                    category_name_translations: row.category_name_translations,
                    item_count:    1,
                    quantity_sold: item.quantity_sold,
                    revenue:       item.revenue,
                    items:         vec![item],
                });
            }
        }
    }

    Ok(HttpResponse::Ok().json(BranchSalesReport {
        branch_id:              *branch_id,
        branch_name,
        from:                   query.from,
        to:                     query.to,
        total_orders:           totals.0,
        voided_orders:          totals.1,
        subtotal:               totals.2,
        total_discount:         totals.3,
        total_tax:              totals.4,
        total_revenue:          totals.5,
        revenue_by_method:      totals.6,
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
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    let branch_name: String = sqlx::query_scalar(
        "SELECT name FROM branches WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(*branch_id)
    .fetch_optional(pool.get_ref()).await?.flatten()
    .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    let items = sqlx::query_as::<_, StockRow>(
        r#"
        SELECT
            bi.id              AS branch_inventory_id,
            oi.name            AS ingredient_name,
            oi.unit::text      AS unit,
            bi.current_stock::float8,
            bi.reorder_threshold::float8,
            oi.cost_per_unit,
            (bi.reorder_threshold > 0 AND bi.current_stock <= bi.reorder_threshold) AS below_reorder
        FROM branch_inventory bi
        JOIN org_ingredients oi ON oi.id = bi.org_ingredient_id
        WHERE bi.branch_id = $1
        ORDER BY (bi.reorder_threshold > 0 AND bi.current_stock <= bi.reorder_threshold) DESC, oi.name ASC
        "#,
    )
    .bind(*branch_id)
    .fetch_all(pool.get_ref()).await?;

    Ok(HttpResponse::Ok().json(BranchStockReport {
        branch_id:   *branch_id,
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
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query:     web::Query<TimeseriesQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    let tz: String = sqlx::query_scalar(
        "SELECT timezone FROM branches WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(*branch_id)
    .fetch_optional(pool.get_ref())
    .await?
    .flatten()
    .filter(|s: &String| !s.is_empty())
    .unwrap_or_else(|| "Africa/Cairo".to_string());

    let trunc = match query.granularity.as_deref().unwrap_or("daily") {
        "hourly"  => "hour",
        "monthly" => "month",
        _         => "day",
    };

    // trunc and tz are server-controlled (enum whitelist + DB value) — safe to interpolate
    let sql = format!(
        r#"
        WITH periods AS (
            SELECT
                date_trunc('{trunc}', o.created_at AT TIME ZONE '{tz}') AS period_val,
                to_char(
                    date_trunc('{trunc}', o.created_at AT TIME ZONE '{tz}'),
                    'YYYY-MM-DD"T"HH24:MI:SS'
                ) AS period_str,
                COUNT(o.id)   FILTER (WHERE o.status != 'voided')::bigint  AS orders,
                COALESCE(SUM(o.total_amount)    FILTER (WHERE o.status != 'voided'), 0)::bigint AS revenue,
                COUNT(o.id)   FILTER (WHERE o.status  = 'voided')::bigint  AS voided,
                COALESCE(SUM(o.discount_amount) FILTER (WHERE o.status != 'voided'), 0)::bigint AS discount,
                COALESCE(SUM(o.tax_amount)      FILTER (WHERE o.status != 'voided'), 0)::bigint AS tax
            FROM orders o
            WHERE o.branch_id = $1
              AND ($2::timestamptz IS NULL OR o.created_at >= $2)
              AND ($3::timestamptz IS NULL OR o.created_at <= $3)
            GROUP BY date_trunc('{trunc}', o.created_at AT TIME ZONE '{tz}')
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
                WHERE o2.branch_id = $1 AND o2.status != 'voided'
                  AND date_trunc('{trunc}', o2.created_at AT TIME ZONE '{tz}') = p.period_val
                GROUP BY op2.method
              ) sub
            ), '{{}}'::json) AS revenue_by_method
        FROM periods p
        ORDER BY p.period_val ASC
        "#,
        trunc = trunc,
        tz    = tz,
    );

    let rows = sqlx::query_as::<_, TimeseriesPoint>(&sql)
        .bind(*branch_id)
        .bind(query.from)
        .bind(query.to)
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
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query:     web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

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
        WHERE o.branch_id = $1
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY o.teller_id, u.name
        ORDER BY revenue DESC
        "#,
    )
    .bind(*branch_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
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
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query:     web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

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
        WHERE o.branch_id = $1
          AND o.status != 'voided'
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY oia.addon_item_id, oia.addon_name, ai.type
        ORDER BY quantity_sold DESC
        "#,
    )
    .bind(*branch_id)
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
    req:    HttpRequest,
    pool:   web::Data<PgPool>,
    org_id: web::Path<Uuid>,
    query:  web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;

    if claims.role != UserRole::SuperAdmin
        && claims.org_id() != Some(*org_id) {
            return Err(AppError::Forbidden("Not your org".into()));
        }

    #[derive(sqlx::FromRow)]
    struct Row {
        branch_id:              Uuid,
        branch_name:            String,
        total_orders:           i64,
        voided_orders:          i64,
        total_revenue:          i64,
        revenue_by_method:      serde_json::Value,
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
        LEFT JOIN order_payments op ON op.order_id  = o.id
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

    let branches = rows.into_iter().map(|r| BranchComparison {
        branch_id:              r.branch_id,
        branch_name:            r.branch_name,
        total_orders:           r.total_orders,
        voided_orders:          r.voided_orders,
        total_revenue:          r.total_revenue,
        revenue_by_method:      r.revenue_by_method,
        avg_order_value: if r.total_orders == 0 { 0 }
                         else { r.total_revenue / r.total_orders },
        void_rate_pct:   if (r.total_orders + r.voided_orders) == 0 { 0.0 }
                         else { r.voided_orders as f64
                                / (r.total_orders + r.voided_orders) as f64
                                * 100.0 },
    }).collect();

    Ok(HttpResponse::Ok().json(OrgComparisonReport {
        org_id:   *org_id,
        from:     query.from,
        to:       query.to,
        branches,
    }))
}

// ── Inventory valuation / low-stock / consumption / waste ─────

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct ValuationRow {
    pub org_ingredient_id: Uuid,
    pub ingredient_name:   String,
    pub unit:              String,
    pub current_stock:     f64,
    /// Piastres per unit; `null` ⟺ unknown.
    pub cost_per_unit:     Option<i64>,
    /// current_stock × cost_per_unit in piastres; `null` when cost unknown.
    pub value:             Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct InventoryValuationReport {
    pub total_value:        i64,
    pub unknown_cost_count: i64,
    pub items:              Vec<ValuationRow>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct LowStockRow {
    pub branch_id:         Uuid,
    pub branch_name:       String,
    pub org_ingredient_id: Uuid,
    pub ingredient_name:   String,
    pub unit:              String,
    pub current_stock:     f64,
    pub reorder_threshold: f64,
    /// reorder_threshold − current_stock: how much to order to reach par.
    pub deficit:           f64,
    /// Default supplier for this ingredient (for one-click "create PO"); may be null.
    pub supplier_id:       Option<Uuid>,
    pub supplier_name:     Option<String>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct ConsumptionRow {
    pub org_ingredient_id: Uuid,
    pub ingredient_name:   String,
    pub unit:              String,
    pub consumed_qty:      f64,
    /// Consumption valued in piastres; `null` if any contributing cost unknown.
    pub consumed_value:    Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct WasteReportRow {
    pub reason:            String,
    pub org_ingredient_id: Uuid,
    pub ingredient_name:   String,
    pub unit:              String,
    pub waste_qty:         f64,
    pub waste_value:       Option<i64>,
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
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    let items = sqlx::query_as::<_, ValuationRow>(
        r#"
        SELECT bi.org_ingredient_id, oi.name AS ingredient_name, oi.unit::text AS unit,
               bi.current_stock::float8,
               round(oi.cost_per_unit)::bigint AS cost_per_unit,
               CASE WHEN oi.cost_per_unit IS NULL THEN NULL
                    ELSE round(bi.current_stock * oi.cost_per_unit)::bigint END AS value
        FROM branch_inventory bi
        JOIN org_ingredients oi ON oi.id = bi.org_ingredient_id
        WHERE bi.branch_id = $1
        ORDER BY oi.name ASC
        "#,
    )
    .bind(*branch_id)
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
    req:    HttpRequest,
    pool:   web::Data<PgPool>,
    org_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_org(&claims, *org_id)?;

    let items = sqlx::query_as::<_, ValuationRow>(
        r#"
        SELECT oi.id AS org_ingredient_id, oi.name AS ingredient_name, oi.unit::text AS unit,
               SUM(bi.current_stock)::float8 AS current_stock,
               round(oi.cost_per_unit)::bigint AS cost_per_unit,
               CASE WHEN oi.cost_per_unit IS NULL THEN NULL
                    ELSE round(SUM(bi.current_stock) * oi.cost_per_unit)::bigint END AS value
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
    req:    HttpRequest,
    pool:   web::Data<PgPool>,
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
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query:     web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    let rows = sqlx::query_as::<_, ConsumptionRow>(
        r#"
        SELECT m.org_ingredient_id, oi.name AS ingredient_name, oi.unit::text AS unit,
               (-SUM(m.quantity))::float8 AS consumed_qty,
               CASE WHEN bool_or(m.unit_cost IS NULL) THEN NULL
                    ELSE round(SUM(-m.quantity * m.unit_cost))::bigint END AS consumed_value
        FROM inventory_movements m
        JOIN org_ingredients oi ON oi.id = m.org_ingredient_id
        WHERE m.branch_id = $1
          AND m.type IN ('sale','waste')
          AND ($2::timestamptz IS NULL OR m.created_at >= $2)
          AND ($3::timestamptz IS NULL OR m.created_at <= $3)
        GROUP BY m.org_ingredient_id, oi.name, oi.unit
        ORDER BY consumed_qty DESC
        "#,
    )
    .bind(*branch_id)
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
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query:     web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    let rows = sqlx::query_as::<_, WasteReportRow>(
        r#"
        SELECT COALESCE(m.reason, 'other') AS reason,
               m.org_ingredient_id, oi.name AS ingredient_name, oi.unit::text AS unit,
               (-SUM(m.quantity))::float8 AS waste_qty,
               CASE WHEN bool_or(m.unit_cost IS NULL) THEN NULL
                    ELSE round(SUM(-m.quantity * m.unit_cost))::bigint END AS waste_value
        FROM inventory_movements m
        JOIN org_ingredients oi ON oi.id = m.org_ingredient_id
        WHERE m.branch_id = $1
          AND m.type = 'waste'
          AND ($2::timestamptz IS NULL OR m.created_at >= $2)
          AND ($3::timestamptz IS NULL OR m.created_at <= $3)
        GROUP BY m.reason, m.org_ingredient_id, oi.name, oi.unit
        ORDER BY waste_qty DESC
        "#,
    )
    .bind(*branch_id)
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
    req:    HttpRequest,
    pool:   web::Data<PgPool>,
    org_id: web::Path<Uuid>,
    query:  web::Query<DateRangeQuery>,
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
        WHERE m.type IN ('sale','waste')
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
    req:    HttpRequest,
    pool:   web::Data<PgPool>,
    org_id: web::Path<Uuid>,
    query:  web::Query<DateRangeQuery>,
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
    pub reason:            String,
    pub org_ingredient_id: Uuid,
    pub ingredient_name:   String,
    pub unit:              String,
    /// Quantity lost (positive number) from negative stock-count differences.
    pub shrinkage_qty:     f64,
    /// Valued shrinkage in piastres; `null` when any contributing cost unknown.
    pub shrinkage_value:   Option<i64>,
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
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query:     web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "inventory", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    let rows = sqlx::query_as::<_, ShrinkageRow>(
        r#"
        SELECT COALESCE(m.reason, 'unexplained') AS reason,
               m.org_ingredient_id, oi.name AS ingredient_name, oi.unit::text AS unit,
               (-SUM(m.quantity))::float8 AS shrinkage_qty,
               CASE WHEN bool_or(m.unit_cost IS NULL) THEN NULL
                    ELSE round(SUM(-m.quantity * m.unit_cost))::bigint END AS shrinkage_value
        FROM inventory_movements m
        JOIN org_ingredients oi ON oi.id = m.org_ingredient_id
        WHERE m.branch_id = $1 AND m.type = 'stock_count' AND m.quantity < 0
          AND ($2::timestamptz IS NULL OR m.created_at >= $2)
          AND ($3::timestamptz IS NULL OR m.created_at <= $3)
        GROUP BY COALESCE(m.reason, 'unexplained'), m.org_ingredient_id, oi.name, oi.unit
        ORDER BY shrinkage_qty DESC
        "#,
    )
    .bind(*branch_id)
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
    req:    HttpRequest,
    pool:   web::Data<PgPool>,
    org_id: web::Path<Uuid>,
    query:  web::Query<DateRangeQuery>,
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
            None    => unknown_cost_count += 1,
        }
    }
    InventoryValuationReport { total_value, unknown_cost_count, items }
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
    pool:     &PgPool,
    claims:   &Claims,
    shift_id: Uuid,
) -> Result<Uuid, AppError> {
    let branch_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT branch_id FROM shifts WHERE id = $1"
    )
    .bind(shift_id)
    .fetch_optional(pool)
    .await?
    .flatten();

    let branch_id = branch_id
        .ok_or_else(|| AppError::NotFound("Shift not found".into()))?;
    require_branch_access(pool, claims, branch_id).await?;
    Ok(branch_id)
}

async fn require_branch_access(
    pool:      &PgPool,
    claims:    &Claims,
    branch_id: Uuid,
) -> Result<(), AppError> {
    if claims.role == UserRole::SuperAdmin { return Ok(()); }

    let branch_org: Option<Uuid> = sqlx::query_scalar(
        "SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(branch_id)
    .fetch_optional(pool)
    .await?
    .flatten();

    let branch_org = branch_org
        .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    if claims.org_id() != Some(branch_org) {
        return Err(AppError::Forbidden("Branch belongs to a different org".into()));
    }

    if claims.role == UserRole::OrgAdmin { return Ok(()); }

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

    Ok(())
}

// ── Bundles Reporting ────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct BundleSalesRow {
    pub bundle_id:     Option<Uuid>,
    pub bundle_name:   String,
    pub quantity_sold: i64,
    pub revenue:       i64,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow, ToSchema)]
pub struct CombinedItemSalesRow {
    pub item_id:       Option<Uuid>,
    pub item_name:     String,
    #[schema(value_type = Object)]
    pub item_name_translations: serde_json::Value,
    pub standalone_qty: i64,
    pub bundle_qty:    i64,
    pub total_qty:     i64,
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
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query:     web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    let rows = sqlx::query_as::<_, BundleSalesRow>(
        r#"
        SELECT
            oi.bundle_id AS bundle_id,
            oi.item_name AS bundle_name,
            SUM(oi.quantity)::bigint AS quantity_sold,
            SUM(oi.line_total)::bigint AS revenue
        FROM order_items oi
        JOIN orders o ON o.id = oi.order_id
        WHERE o.branch_id = $1
          AND o.status != 'voided'
          AND oi.bundle_id IS NOT NULL
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY oi.bundle_id, oi.item_name
        ORDER BY quantity_sold DESC
        "#,
    )
    .bind(*branch_id)
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
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query:     web::Query<DateRangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

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
            WHERE o.branch_id = $1
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
            WHERE o.branch_id = $1
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
    .bind(*branch_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── GET /reports/branches/:id/menu-engineering ────────────────
//
// Foodics-vocabulary menu engineering table. Two cost bases, both pairing
// ITEM revenue (`line_total`) with ITEM recipe-scope cost — additive addons
// carry their own revenue AND cost rows and have their own report, so they
// belong in neither side here:
//   - `cost_basis=snapshot` (default): sale-time recipe cost snapshots
//     (order_items.unit_cost × quantity) — history stays truthful when
//     ingredient prices move; an inventory cost edit only flows in via new
//     orders (or an operator backfill).
//   - `cost_basis=current`: TODAY's recipe rollups (costing::org_sku_costs)
//     applied to realized quantities — answers "how does my menu classify
//     under current costs?" immediately after editing ingredient costs.
//
// Invariant (pinned by tests): right after `backfill-cost-snapshots`, the
// two bases return identical rows and totals.
//
// Column semantics mirror the Foodics report so migrating owners read it
// without relearning: Sales, Quantity, Total Cost, Item Profit, Total
// Profit, Popularity %, Profit Category, Popularity Category, Class.
// Classification here is the simple single-window high/low split (what
// Foodics shows); the Menu Advisor remains the statistically serious view.
//
// Rows whose cost is unresolvable under the chosen basis are EXCLUDED from
// the report entirely — they are not returned, and their quantities and row
// count do not enter the popularity/profit thresholds. The response only
// reports how many were excluded (`rows_cost_missing`).

#[derive(Serialize, ToSchema)]
pub struct MenuEngineeringRow {
    pub menu_item_id:   Uuid,
    /// `"one_size"` for items without sizes.
    pub size_label:     String,
    pub item_name:      String,
    pub category_id:    Option<Uuid>,
    pub category_name:  Option<String>,
    /// Units sold (standalone lines only — bundle lines are excluded so the
    /// per-unit economics stay clean; bundle performance has its own report).
    pub quantity_sold:  i64,
    /// Revenue from those lines, piastres.
    pub sales:          i64,
    /// Recipe-scope COGS in piastres (additive addons excluded — they have
    /// their own revenue and their own report). Snapshot basis:
    /// `SUM(unit_cost × quantity)`; current basis: today's recipe rollup ×
    /// quantity. Rows where this is unresolvable are excluded from the
    /// report, so it is always present.
    pub total_cost:     i64,
    /// Average profit per unit, piastres (`(sales - cost) / qty`).
    pub item_profit:    i64,
    /// `sales - total_cost`, piastres.
    pub total_profit:   i64,
    /// Share of units among the rows in this report (cost-tracked only).
    pub popularity_pct: f64,
    /// Lines in the window whose sale-time cost could not be resolved.
    /// Always reports snapshot data quality, regardless of `cost_basis` —
    /// under `current`, an included row can still carry snapshot gaps.
    pub cost_missing_lines: i64,
    /// "high" | "low" — vs weighted-average per-unit profit.
    pub profit_category:     String,
    /// "high" | "low" — Kasavana-Smith 70% rule (0.70 / n).
    pub popularity_category: String,
    /// star | workhorse | challenge | dog (Foodics names).
    pub class:          String,
}

#[derive(Serialize, ToSchema)]
pub struct MenuEngineeringReport {
    pub branch_id: Uuid,
    pub from:      Option<DateTime<Utc>>,
    pub to:        Option<DateTime<Utc>>,
    /// Cost basis the report was computed with: "snapshot" | "current".
    pub cost_basis: String,
    pub rows:      Vec<MenuEngineeringRow>,
    /// Totals over the returned rows.
    pub total_sales:  i64,
    pub total_cost:   i64,
    pub total_profit: i64,
    /// SKUs sold in the window but EXCLUDED from this report because their
    /// cost was unresolvable under the chosen basis.
    pub rows_cost_missing: i64,
    /// Realized revenue (piastres) carried by the excluded SKUs — explains
    /// why `total_sales` differs between cost bases: each basis excludes a
    /// different set of un-costable rows.
    pub excluded_sales: i64,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CostBasisQuery {
    /// `snapshot` (default) — COGS from sale-time order snapshots.
    /// `current` — COGS from today's recipe rollups.
    pub cost_basis: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum CostBasis {
    Snapshot,
    Current,
}

impl CostBasis {
    fn parse(s: Option<&str>) -> Result<Self, AppError> {
        match s {
            None | Some("snapshot") => Ok(Self::Snapshot),
            Some("current") => Ok(Self::Current),
            Some(_) => Err(AppError::BadRequest(
                "cost_basis must be 'snapshot' or 'current'".into(),
            )),
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::Current => "current",
        }
    }
}

#[utoipa::path(
    get,
    path = "/reports/branches/{branch_id}/menu-engineering",
    tag = "reports",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    params(DateRangeQuery, CostBasisQuery),
    responses((status = 200, description = "Foodics-style menu engineering table. cost_basis=snapshot (default) uses sale-time cost snapshots; cost_basis=current reclassifies realized sales under today's recipe costs.", body = MenuEngineeringReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn branch_menu_engineering(
    req:         HttpRequest,
    pool:        web::Data<PgPool>,
    branch_id:   web::Path<Uuid>,
    query:       web::Query<DateRangeQuery>,
    basis_query: web::Query<CostBasisQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;
    let basis = CostBasis::parse(basis_query.cost_basis.as_deref())?;

    #[derive(sqlx::FromRow)]
    struct Row {
        menu_item_id:       Uuid,
        size_label:         String,
        item_name:          String,
        category_id:        Option<Uuid>,
        category_name:      Option<String>,
        quantity_sold:      i64,
        sales:              i64,
        total_cost:         Option<i64>,
        cost_missing_lines: i64,
    }

    let mut rows: Vec<Row> = sqlx::query_as::<_, Row>(
        r#"
        SELECT
            oi.menu_item_id,
            COALESCE(oi.size_label::text, 'one_size') AS size_label,
            (array_agg(oi.item_name ORDER BY o.created_at DESC))[1] AS item_name,
            mi.category_id,
            c.name AS category_name,
            SUM(oi.quantity)::bigint   AS quantity_sold,
            SUM(oi.line_total)::bigint AS sales,
            CASE
                WHEN bool_or(oi.unit_cost IS NULL) THEN NULL
                ELSE SUM(oi.unit_cost * oi.quantity)::bigint
            END AS total_cost,
            COUNT(*) FILTER (WHERE oi.cost_missing)::bigint AS cost_missing_lines
        FROM order_items oi
        JOIN orders o ON o.id = oi.order_id
        JOIN menu_items mi ON mi.id = oi.menu_item_id
        LEFT JOIN categories c ON c.id = mi.category_id
        WHERE o.branch_id = $1
          AND o.status != 'voided'
          AND oi.menu_item_id IS NOT NULL
          AND oi.bundle_id IS NULL
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY oi.menu_item_id, COALESCE(oi.size_label::text, 'one_size'),
                 mi.category_id, c.name
        ORDER BY sales DESC
        "#,
    )
    .bind(*branch_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;

    // Current basis: swap each row's COGS for today's recipe rollup × qty
    // BEFORE the exclusion below. SKUs without a resolvable current cost
    // (no recipe, missing ingredient cost, item deleted/deactivated since
    // the sale) become unresolvable, exactly like snapshot cost-missing rows.
    if basis == CostBasis::Current {
        let org_id: Uuid = sqlx::query_scalar(
            "SELECT org_id FROM branches WHERE id = $1",
        )
        .bind(*branch_id)
        .fetch_one(pool.get_ref())
        .await?;
        let current_costs: std::collections::HashMap<(Uuid, String), Option<i64>> =
            crate::costing::org_sku_costs(pool.get_ref(), org_id)
                .await?
                .into_iter()
                .map(|s| ((s.menu_item_id, s.size_label), s.cost))
                .collect();
        for r in &mut rows {
            r.total_cost = current_costs
                .get(&(r.menu_item_id, r.size_label.clone()))
                .copied()
                .flatten()
                .map(|unit_cost| unit_cost * r.quantity_sold);
        }
    }

    // Cost-unresolvable rows are dropped HERE, before any threshold math:
    // they must not be returned, and their quantities/row count must not
    // sway the popularity split or the average-profit line. Their revenue
    // is reported so the basis-dependent totals visibly reconcile.
    let rows_total = rows.len();
    let mut excluded_sales = 0_i64;
    rows.retain(|r| {
        let keep = r.total_cost.is_some();
        if !keep {
            excluded_sales += r.sales;
        }
        keep
    });
    let rows_cost_missing = (rows_total - rows.len()) as i64;

    let total_units: i64 = rows.iter().map(|r| r.quantity_sold).sum();
    let n = rows.len().max(1) as f64;
    let pop_threshold = 0.70 / n;

    // Weighted-average per-unit profit — the profit axis split
    // (Kasavana-Smith uses the average CM as the line).
    let (tracked_profit, tracked_units): (i64, i64) = rows
        .iter()
        .filter_map(|r| r.total_cost.map(|c| (r.sales - c, r.quantity_sold)))
        .fold((0_i64, 0_i64), |acc, (p, q)| (acc.0 + p, acc.1 + q));
    let avg_unit_profit = if tracked_units > 0 {
        tracked_profit as f64 / tracked_units as f64
    } else {
        0.0
    };

    let mut out_rows = Vec::with_capacity(rows.len());
    let mut total_sales = 0_i64;
    let mut total_cost = 0_i64;

    for r in rows {
        // retain() above guarantees Some; skip defensively rather than panic.
        let Some(cost) = r.total_cost else { continue };

        let popularity_pct = if total_units > 0 {
            r.quantity_sold as f64 / total_units as f64
        } else {
            0.0
        };
        let high_pop = popularity_pct >= pop_threshold;
        let popularity_category = if high_pop { "high" } else { "low" }.to_string();

        let total_profit = r.sales - cost;
        let item_profit =
            if r.quantity_sold > 0 { total_profit / r.quantity_sold } else { 0 };
        let high_profit = (item_profit as f64) >= avg_unit_profit;
        let class = match (high_pop, high_profit) {
            (true, true)   => "star",
            (true, false)  => "workhorse",
            (false, true)  => "challenge",
            (false, false) => "dog",
        }
        .to_string();
        let profit_category = if high_profit { "high" } else { "low" }.to_string();
        total_sales += r.sales;
        total_cost += cost;

        out_rows.push(MenuEngineeringRow {
            menu_item_id: r.menu_item_id,
            size_label: r.size_label,
            item_name: r.item_name,
            category_id: r.category_id,
            category_name: r.category_name,
            quantity_sold: r.quantity_sold,
            sales: r.sales,
            total_cost: cost,
            item_profit,
            total_profit,
            popularity_pct,
            cost_missing_lines: r.cost_missing_lines,
            profit_category,
            popularity_category,
            class,
        });
    }

    Ok(HttpResponse::Ok().json(MenuEngineeringReport {
        branch_id: *branch_id,
        from: query.from,
        to: query.to,
        cost_basis: basis.as_str().to_string(),
        total_profit: total_sales - total_cost,
        total_sales,
        total_cost,
        rows: out_rows,
        rows_cost_missing,
        excluded_sales,
    }))
}
