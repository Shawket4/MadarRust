use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    errors::AppError,
    models::UserRole,
    permissions::checker::check_permission,
};

// ── Query params ──────────────────────────────────────────────

#[derive(Deserialize)]
pub struct DateRangeQuery {
    pub from:  Option<DateTime<Utc>>,
    pub to:    Option<DateTime<Utc>>,
    pub limit: Option<i64>, // for top_items (default 20)
}

#[derive(Deserialize)]
pub struct TimeseriesQuery {
    pub from:        Option<DateTime<Utc>>,
    pub to:          Option<DateTime<Utc>>,
    pub granularity: Option<String>, // "hourly" | "daily" | "monthly"
}

// ── Response types ────────────────────────────────────────────

#[derive(Debug, Serialize, sqlx::FromRow)]
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
    pub cash_revenue:           i64,
    pub card_revenue:           i64,
    pub digital_wallet_revenue: i64,
    pub mixed_revenue:          i64,
    pub talabat_online_revenue: i64,
    pub talabat_cash_revenue:   i64,
    pub total_discount:         i64,
    pub total_tax:              i64,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct InventoryDiscrepancy {
    pub branch_inventory_id: Uuid,
    pub ingredient_name:     String,
    pub unit:                String,
    pub expected_stock:      f64,
    pub actual_count:        Option<f64>,
    pub discrepancy:         Option<f64>,
    pub note:                Option<String>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
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

#[derive(Debug, Serialize)]
pub struct CategorySales {
    pub category_id:   Option<Uuid>,
    pub category_name: Option<String>,
    pub item_count:    i64,
    pub quantity_sold: i64,
    pub revenue:       i64,
    pub items:         Vec<ItemSales>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ItemSales {
    pub menu_item_id:  Uuid,
    pub item_name:     String,
    pub quantity_sold: i64,
    pub revenue:       i64,
}

#[derive(Debug, Serialize)]
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
    pub cash_revenue:           i64,
    pub card_revenue:           i64,
    pub digital_wallet_revenue: i64,
    pub mixed_revenue:          i64,
    pub talabat_online_revenue: i64,
    pub talabat_cash_revenue:   i64,
    pub top_items:              Vec<ItemSales>,
    pub by_category:            Vec<CategorySales>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct StockRow {
    pub branch_inventory_id: Uuid,
    pub ingredient_name:     String,
    pub unit:                String,
    pub current_stock:       f64,
    pub reorder_threshold:   f64,
    #[serde(with = "rust_decimal::serde::float")]
    pub cost_per_unit:       Decimal,
    pub below_reorder:       bool,
}

#[derive(Debug, Serialize)]
pub struct BranchStockReport {
    pub branch_id:   Uuid,
    pub branch_name: String,
    pub items:       Vec<StockRow>,
}

// Timeseries now includes per-payment-method breakdown
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct TimeseriesPoint {
    pub period:                 String,
    pub orders:                 i64,
    pub revenue:                i64,
    pub voided:                 i64,
    pub discount:               i64,
    pub tax:                    i64,
    pub cash_revenue:           i64,
    pub card_revenue:           i64,
    pub digital_wallet_revenue: i64,
    pub mixed_revenue:          i64,
    pub talabat_online_revenue: i64,
    pub talabat_cash_revenue:   i64,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct TellerStats {
    pub teller_id:       Uuid,
    pub teller_name:     String,
    pub orders:          i64,
    pub revenue:         i64,
    pub avg_order_value: i64,
    pub voided:          i64,
    pub shifts:          i64,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct AddonSalesRow {
    pub addon_item_id: Uuid,
    pub addon_name:    String,
    pub addon_type:    String,
    pub quantity_sold: i64,
    pub revenue:       i64,
}

#[derive(Debug, Serialize)]
pub struct BranchComparison {
    pub branch_id:              Uuid,
    pub branch_name:            String,
    pub total_orders:           i64,
    pub voided_orders:          i64,
    pub total_revenue:          i64,
    pub cash_revenue:           i64,
    pub card_revenue:           i64,
    pub digital_wallet_revenue: i64,
    pub mixed_revenue:          i64,
    pub talabat_online_revenue: i64,
    pub talabat_cash_revenue:   i64,
    pub avg_order_value:        i64,
    pub void_rate_pct:          f64,
}

#[derive(Debug, Serialize)]
pub struct OrgComparisonReport {
    pub org_id:   Uuid,
    pub from:     Option<DateTime<Utc>>,
    pub to:       Option<DateTime<Utc>>,
    pub branches: Vec<BranchComparison>,
}

// ── GET /reports/shifts/:id/summary ──────────────────────────

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
            COALESCE(SUM(op.amount) FILTER (WHERE o.status != 'voided' AND op.method = 'cash'),           0)::bigint AS cash_revenue,
            COALESCE(SUM(op.amount) FILTER (WHERE o.status != 'voided' AND op.method = 'card'),           0)::bigint AS card_revenue,
            COALESCE(SUM(op.amount) FILTER (WHERE o.status != 'voided' AND op.method = 'digital_wallet'), 0)::bigint AS digital_wallet_revenue,
            COALESCE(SUM(op.amount) FILTER (WHERE o.status != 'voided' AND op.method = 'mixed'),          0)::bigint AS mixed_revenue,
            COALESCE(SUM(op.amount) FILTER (WHERE o.status != 'voided' AND op.method = 'talabat_online'), 0)::bigint AS talabat_online_revenue,
            COALESCE(SUM(op.amount) FILTER (WHERE o.status != 'voided' AND op.method = 'talabat_cash'),   0)::bigint AS talabat_cash_revenue,
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

// ── GET /reports/shifts/:id/inventory ────────────────────────

pub async fn shift_inventory_discrepancies(
    req:      HttpRequest,
    pool:     web::Data<PgPool>,
    shift_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "shift_counts", "read").await?;
    require_shift_branch_access(pool.get_ref(), &claims, *shift_id).await?;

    let rows = sqlx::query_as::<_, InventoryDiscrepancy>(
        r#"
        SELECT
            sic.branch_inventory_id,
            oi.name         AS ingredient_name,
            oi.unit::text   AS unit,
            sic.expected_stock::float8,
            sic.actual_stock::float8 AS actual_count,
            sic.discrepancy::float8,
            sic.note
        FROM shift_inventory_counts sic
        JOIN branch_inventory bi ON bi.id = sic.branch_inventory_id
        JOIN org_ingredients oi  ON oi.id = bi.org_ingredient_id
        WHERE sic.shift_id = $1
        ORDER BY oi.name ASC
        "#,
    )
    .bind(*shift_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ── GET /reports/shifts/:id/deductions ───────────────────────

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

    // 12-column aggregate — includes talabat variants
    #[allow(clippy::type_complexity)]
    let totals: (i64,i64,i64,i64,i64,i64,i64,i64,i64,i64,i64,i64) = sqlx::query_as(
        r#"
        SELECT
            COUNT(*) FILTER (WHERE status != 'voided'),
            COUNT(*) FILTER (WHERE status = 'voided'),
            COALESCE(SUM(subtotal)        FILTER (WHERE status != 'voided'), 0),
            COALESCE(SUM(discount_amount) FILTER (WHERE status != 'voided'), 0),
            COALESCE(SUM(tax_amount)      FILTER (WHERE status != 'voided'), 0),
            COALESCE(SUM(total_amount)    FILTER (WHERE status != 'voided'), 0),
                        COALESCE((SELECT SUM(op.amount) FROM order_payments op JOIN orders oi ON oi.id = op.order_id WHERE oi.branch_id = $1 AND oi.status != 'voided' AND ($2::timestamptz IS NULL OR oi.created_at >= $2) AND ($3::timestamptz IS NULL OR oi.created_at <= $3) AND op.method = 'cash'), 0),
            COALESCE((SELECT SUM(op.amount) FROM order_payments op JOIN orders oi ON oi.id = op.order_id WHERE oi.branch_id = $1 AND oi.status != 'voided' AND ($2::timestamptz IS NULL OR oi.created_at >= $2) AND ($3::timestamptz IS NULL OR oi.created_at <= $3) AND op.method = 'card'), 0),
            COALESCE((SELECT SUM(op.amount) FROM order_payments op JOIN orders oi ON oi.id = op.order_id WHERE oi.branch_id = $1 AND oi.status != 'voided' AND ($2::timestamptz IS NULL OR oi.created_at >= $2) AND ($3::timestamptz IS NULL OR oi.created_at <= $3) AND op.method = 'digital_wallet'), 0),
            COALESCE((SELECT SUM(op.amount) FROM order_payments op JOIN orders oi ON oi.id = op.order_id WHERE oi.branch_id = $1 AND oi.status != 'voided' AND ($2::timestamptz IS NULL OR oi.created_at >= $2) AND ($3::timestamptz IS NULL OR oi.created_at <= $3) AND op.method = 'mixed'), 0),
            COALESCE((SELECT SUM(op.amount) FROM order_payments op JOIN orders oi ON oi.id = op.order_id WHERE oi.branch_id = $1 AND oi.status != 'voided' AND ($2::timestamptz IS NULL OR oi.created_at >= $2) AND ($3::timestamptz IS NULL OR oi.created_at <= $3) AND op.method = 'talabat_online'), 0),
            COALESCE((SELECT SUM(op.amount) FROM order_payments op JOIN orders oi ON oi.id = op.order_id WHERE oi.branch_id = $1 AND oi.status != 'voided' AND ($2::timestamptz IS NULL OR oi.created_at >= $2) AND ($3::timestamptz IS NULL OR oi.created_at <= $3) AND op.method = 'talabat_cash'), 0)
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
        SELECT oi.menu_item_id, oi.item_name,
               SUM(oi.quantity)::bigint   AS quantity_sold,
               SUM(oi.line_total)::bigint AS revenue
        FROM order_items oi
        JOIN orders o ON o.id = oi.order_id
        WHERE o.branch_id = $1 AND o.status != 'voided'
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY oi.menu_item_id, oi.item_name
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
        menu_item_id:  Uuid,
        item_name:     String,
        quantity_sold: i64,
        revenue:       i64,
    }

    let cat_rows = sqlx::query_as::<_, CategoryItemRow>(
        r#"
        SELECT m.category_id, c.name AS category_name,
               oi.menu_item_id, oi.item_name,
               SUM(oi.quantity)::bigint   AS quantity_sold,
               SUM(oi.line_total)::bigint AS revenue
        FROM order_items oi
        JOIN orders o     ON o.id  = oi.order_id
        JOIN menu_items m ON m.id  = oi.menu_item_id
        LEFT JOIN categories c ON c.id = m.category_id
        WHERE o.branch_id = $1 AND o.status != 'voided'
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY m.category_id, c.name, oi.menu_item_id, oi.item_name
        ORDER BY c.name NULLS LAST, revenue DESC
        "#,
    )
    .bind(*branch_id).bind(query.from).bind(query.to)
    .fetch_all(pool.get_ref()).await?;

    let mut by_category: Vec<CategorySales> = Vec::new();
    for row in cat_rows {
        let item = ItemSales {
            menu_item_id:  row.menu_item_id,
            item_name:     row.item_name,
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
        cash_revenue:           totals.6,
        card_revenue:           totals.7,
        digital_wallet_revenue: totals.8,
        mixed_revenue:          totals.9,
        talabat_online_revenue: totals.10,
        talabat_cash_revenue:   totals.11,
        top_items,
        by_category,
    }))
}

// ── GET /reports/branches/:id/stock ──────────────────────────

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
            (bi.current_stock <= bi.reorder_threshold) AS below_reorder
        FROM branch_inventory bi
        JOIN org_ingredients oi ON oi.id = bi.org_ingredient_id
        WHERE bi.branch_id = $1
        ORDER BY (bi.current_stock <= bi.reorder_threshold) DESC, oi.name ASC
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

pub async fn branch_sales_timeseries(
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query:     web::Query<TimeseriesQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    let trunc = match query.granularity.as_deref().unwrap_or("daily") {
        "hourly"  => "hour",
        "monthly" => "month",
        _         => "day",
    };

    // trunc is server-controlled — not user input, safe to interpolate
    let sql = format!(
        r#"
        SELECT
            to_char(
                date_trunc('{trunc}', o.created_at AT TIME ZONE 'Africa/Cairo'),
                'YYYY-MM-DD"T"HH24:MI:SS'
            ) AS period,
            COUNT(o.id)   FILTER (WHERE o.status != 'voided')::bigint  AS orders,
            COALESCE(SUM(o.total_amount)    FILTER (WHERE o.status != 'voided'), 0)::bigint AS revenue,
            COUNT(o.id)   FILTER (WHERE o.status  = 'voided')::bigint  AS voided,
            COALESCE(SUM(o.discount_amount) FILTER (WHERE o.status != 'voided'), 0)::bigint AS discount,
            COALESCE(SUM(o.tax_amount)      FILTER (WHERE o.status != 'voided'), 0)::bigint AS tax,
            COALESCE(SUM(op.amount) FILTER (WHERE o.status != 'voided' AND op.method = 'cash'),           0)::bigint AS cash_revenue,
            COALESCE(SUM(op.amount) FILTER (WHERE o.status != 'voided' AND op.method = 'card'),           0)::bigint AS card_revenue,
            COALESCE(SUM(op.amount) FILTER (WHERE o.status != 'voided' AND op.method = 'digital_wallet'), 0)::bigint AS digital_wallet_revenue,
            COALESCE(SUM(op.amount) FILTER (WHERE o.status != 'voided' AND op.method = 'mixed'),          0)::bigint AS mixed_revenue,
            COALESCE(SUM(op.amount) FILTER (WHERE o.status != 'voided' AND op.method = 'talabat_online'), 0)::bigint AS talabat_online_revenue,
            COALESCE(SUM(op.amount) FILTER (WHERE o.status != 'voided' AND op.method = 'talabat_cash'),   0)::bigint AS talabat_cash_revenue
        FROM orders o
        LEFT JOIN order_payments op ON op.order_id = o.id
        WHERE o.branch_id = $1
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY date_trunc('{trunc}', o.created_at AT TIME ZONE 'Africa/Cairo')
        ORDER BY 1 ASC
        "#,
        trunc = trunc
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
        cash_revenue:           i64,
        card_revenue:           i64,
        digital_wallet_revenue: i64,
        mixed_revenue:          i64,
        talabat_online_revenue: i64,
        talabat_cash_revenue:   i64,
    }

    let rows = sqlx::query_as::<_, Row>(
        r#"
        SELECT
            b.id   AS branch_id,
            b.name AS branch_name,
            COUNT(DISTINCT o.id) FILTER (WHERE o.status != 'voided')::bigint AS total_orders,
            COUNT(DISTINCT o.id) FILTER (WHERE o.status  = 'voided')::bigint AS voided_orders,
            COALESCE(SUM(o.total_amount) FILTER (WHERE o.status != 'voided'), 0)::bigint AS total_revenue,
            COALESCE(SUM(op.amount) FILTER (WHERE o.status != 'voided' AND op.method = 'cash'),           0)::bigint AS cash_revenue,
            COALESCE(SUM(op.amount) FILTER (WHERE o.status != 'voided' AND op.method = 'card'),           0)::bigint AS card_revenue,
            COALESCE(SUM(op.amount) FILTER (WHERE o.status != 'voided' AND op.method = 'digital_wallet'), 0)::bigint AS digital_wallet_revenue,
            COALESCE(SUM(op.amount) FILTER (WHERE o.status != 'voided' AND op.method = 'mixed'),          0)::bigint AS mixed_revenue,
            COALESCE(SUM(op.amount) FILTER (WHERE o.status != 'voided' AND op.method = 'talabat_online'), 0)::bigint AS talabat_online_revenue,
            COALESCE(SUM(op.amount) FILTER (WHERE o.status != 'voided' AND op.method = 'talabat_cash'),   0)::bigint AS talabat_cash_revenue
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
        cash_revenue:           r.cash_revenue,
        card_revenue:           r.card_revenue,
        digital_wallet_revenue: r.digital_wallet_revenue,
        mixed_revenue:          r.mixed_revenue,
        talabat_online_revenue: r.talabat_online_revenue,
        talabat_cash_revenue:   r.talabat_cash_revenue,
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
