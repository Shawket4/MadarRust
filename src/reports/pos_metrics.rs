//! `GET /reports/branches/{branch_id}/pos-metrics` — the till's Metrics screen
//! in ONE call (capability `reports.pos_metrics`).
//!
//! The window is a run of branch-local calendar days (`from`..=`to`, both
//! `YYYY-MM-DD`), cut at local midnight in the branch's effective timezone
//! (`tz::effective_tz`), half-open `[window_from, window_to)`.
//!
//! Every figure uses the `branch_sales` definitions, so the two agree for the
//! same instants (`pos_metrics_tests::figures_agree_with_branch_sales`):
//! * a SOLD sale is `orders::SOLD` (not voided, not refunded in full);
//! * `net_sales` = Σ (total − refunds against it) over sold sales
//!   (`branch_sales.total_revenue`), `gross_sales` / `refunded_amount` likewise;
//! * tenders are the sold sales' `order_payments` by method (goods only, no tips)
//!   = `branch_sales.revenue_by_method`.
//!
//! The POS computes the same figures from its local ledger when this call is
//! unavailable (madar `madar-core/src/metrics.rs`); keep the two in step.

use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    errors::{AppError, AppErrorResponse},
};

/// The longest window one call may ask for.
pub const MAX_DAYS: i64 = 366;
/// How many items the leaderboard carries.
pub const TOP_ITEMS: i64 = 10;

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PosMetricsQuery {
    /// First branch-local day, `YYYY-MM-DD` (inclusive).
    pub from: NaiveDate,
    /// Last branch-local day, `YYYY-MM-DD` (inclusive).
    pub to: NaiveDate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, sqlx::FromRow)]
pub struct PosMetricsTender {
    pub method: String,
    pub amount: i64,
    /// Sold sales with at least one leg in this method.
    pub order_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, sqlx::FromRow)]
pub struct PosMetricsItem {
    /// The menu item or bundle; null for a line with neither.
    pub item_id: Option<Uuid>,
    pub item_name: String,
    pub quantity: i64,
    /// Σ line totals (before refunds), as `branch_sales.top_items.revenue`.
    pub revenue: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, sqlx::FromRow)]
pub struct PosMetricsHour {
    /// Branch-local hour of day, 0–23.
    pub hour: i32,
    pub order_count: i64,
    pub net_sales: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PosMetricsReport {
    pub branch_id: Uuid,
    /// The IANA zone the days were cut in.
    pub timezone: String,
    pub from: NaiveDate,
    pub to: NaiveDate,
    /// `[window_from, window_to)`: local midnight of `from` to local midnight after `to`.
    pub window_from: DateTime<Utc>,
    pub window_to: DateTime<Utc>,
    /// Sold sales net of refunds against them (`branch_sales.total_revenue`).
    pub net_sales: i64,
    pub gross_sales: i64,
    pub refunded_amount: i64,
    /// Sold sales (`branch_sales.total_orders`).
    pub order_count: i64,
    /// `net_sales / order_count`, rounded half up; 0 with no sales.
    pub average_ticket: i64,
    /// By amount, largest first.
    pub tenders: Vec<PosMetricsTender>,
    pub voided_count: i64,
    pub voided_amount: i64,
    /// Sales refunded in full (out of every sold figure above).
    pub refunded_orders_count: i64,
    /// Refunds ISSUED inside the window at this branch, whichever sale they refund.
    pub refunds_issued_count: i64,
    pub refunds_issued_amount: i64,
    /// Top [`TOP_ITEMS`] by quantity (then revenue, then name).
    pub top_items: Vec<PosMetricsItem>,
    /// Always 24 rows, hour 0 first.
    pub hourly: Vec<PosMetricsHour>,
}

/// Rounded half up; 0 when there is nothing to divide by.
pub fn average_ticket(net_sales: i64, order_count: i64) -> i64 {
    if order_count <= 0 {
        0
    } else {
        (2 * net_sales + order_count).div_euclid(2 * order_count)
    }
}

#[utoipa::path(
    get,
    path = "/reports/branches/{branch_id}/pos-metrics",
    tag = "reports",
    params(("branch_id" = Uuid, Path, description = "Branch ID"), PosMetricsQuery),
    responses((status = 200, description = "The till's metrics for branch-local days", body = PosMetricsReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
#[tracing::instrument(skip_all, fields(branch_id = %*branch_id, from = %query.from, to = %query.to))]
pub async fn branch_pos_metrics(
    req: HttpRequest,
    pool: crate::db::Db,
    branch_id: web::Path<Uuid>,
    query: web::Query<PosMetricsQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = req
        .extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))?;
    let pool = pool.get_ref();
    let branch_id = *branch_id;
    crate::authz::require::require(
        pool,
        &claims,
        crate::authz::Cap::ReportsPosMetrics,
        Some(branch_id),
    )
    .await?;
    crate::authz::scope::require_branch_access_bound(pool, &claims, branch_id).await?;

    if query.to < query.from {
        return Err(AppError::BadRequest("`to` is before `from`".into()));
    }
    if (query.to - query.from).num_days() >= MAX_DAYS {
        return Err(AppError::BadRequest(format!(
            "A window is at most {MAX_DAYS} days"
        )));
    }
    Ok(HttpResponse::Ok().json(compute(pool, branch_id, query.from, query.to).await?))
}

/// The report for `from..=to` (branch-local days).
pub async fn compute(
    pool: &PgPool,
    branch_id: Uuid,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<PosMetricsReport, AppError> {
    let tz = crate::tz::effective_tz(pool, branch_id).await?;
    let (window_from, _) = crate::bookings::handlers::service_day_bounds(tz, from);
    let (_, window_to) = crate::bookings::handlers::service_day_bounds(tz, to);
    let sold = crate::orders::SOLD;

    #[derive(sqlx::FromRow)]
    struct Totals {
        net_sales: i64,
        gross_sales: i64,
        refunded_amount: i64,
        order_count: i64,
        voided_count: i64,
        voided_amount: i64,
        refunded_orders_count: i64,
    }
    let t = sqlx::query_as::<_, Totals>(&format!(
        "SELECT
            COALESCE(SUM(o.total_amount - COALESCE(rf.refunded_amount, 0)) FILTER (WHERE o.{sold}), 0)::bigint AS net_sales,
            COALESCE(SUM(o.total_amount) FILTER (WHERE o.{sold}), 0)::bigint AS gross_sales,
            COALESCE(SUM(rf.refunded_amount) FILTER (WHERE o.{sold}), 0)::bigint AS refunded_amount,
            COUNT(*) FILTER (WHERE o.{sold})::bigint AS order_count,
            COUNT(*) FILTER (WHERE o.status = 'voided')::bigint AS voided_count,
            COALESCE(SUM(o.total_amount) FILTER (WHERE o.status = 'voided'), 0)::bigint AS voided_amount,
            COUNT(*) FILTER (WHERE o.status = 'refunded')::bigint AS refunded_orders_count
           FROM orders o
           LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id
          WHERE o.branch_id = $1 AND o.created_at >= $2 AND o.created_at < $3"
    ))
    .bind(branch_id)
    .bind(window_from)
    .bind(window_to)
    .fetch_one(pool)
    .await?;

    let tenders = sqlx::query_as::<_, PosMetricsTender>(&format!(
        "SELECT op.method, SUM(op.amount)::bigint AS amount, COUNT(DISTINCT o.id)::bigint AS order_count
           FROM order_payments op
           JOIN orders o ON o.id = op.order_id
          WHERE o.branch_id = $1 AND o.{sold} AND o.created_at >= $2 AND o.created_at < $3
          GROUP BY op.method
          ORDER BY amount DESC, op.method COLLATE \"C\""
    ))
    .bind(branch_id)
    .bind(window_from)
    .bind(window_to)
    .fetch_all(pool)
    .await?;

    let (refunds_issued_count, refunds_issued_amount): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*)::bigint, COALESCE(SUM(amount), 0)::bigint
           FROM order_refunds
          WHERE branch_id = $1 AND issued_at >= $2 AND issued_at < $3",
    )
    .bind(branch_id)
    .bind(window_from)
    .bind(window_to)
    .fetch_one(pool)
    .await?;

    let top_items = sqlx::query_as::<_, PosMetricsItem>(&format!(
        "SELECT COALESCE(oi.menu_item_id, oi.bundle_id) AS item_id, oi.item_name,
                SUM(oi.quantity)::bigint AS quantity, SUM(oi.line_total)::bigint AS revenue
           FROM order_items oi
           JOIN orders o ON o.id = oi.order_id
          WHERE o.branch_id = $1 AND o.{sold} AND o.created_at >= $2 AND o.created_at < $3
          GROUP BY COALESCE(oi.menu_item_id, oi.bundle_id), oi.item_name
          ORDER BY quantity DESC, revenue DESC, oi.item_name COLLATE \"C\"
          LIMIT $4"
    ))
    .bind(branch_id)
    .bind(window_from)
    .bind(window_to)
    .bind(TOP_ITEMS)
    .fetch_all(pool)
    .await?;

    let hours = sqlx::query_as::<_, PosMetricsHour>(&format!(
        "SELECT EXTRACT(HOUR FROM o.created_at AT TIME ZONE $4)::int AS hour,
                COUNT(*)::bigint AS order_count,
                COALESCE(SUM(o.total_amount - COALESCE(rf.refunded_amount, 0)), 0)::bigint AS net_sales
           FROM orders o
           LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id
          WHERE o.branch_id = $1 AND o.{sold} AND o.created_at >= $2 AND o.created_at < $3
          GROUP BY 1"
    ))
    .bind(branch_id)
    .bind(window_from)
    .bind(window_to)
    .bind(tz.name())
    .fetch_all(pool)
    .await?;
    let hourly = (0..24)
        .map(|h| {
            hours
                .iter()
                .find(|r| r.hour == h)
                .cloned()
                .unwrap_or(PosMetricsHour {
                    hour: h,
                    order_count: 0,
                    net_sales: 0,
                })
        })
        .collect();

    Ok(PosMetricsReport {
        branch_id,
        timezone: tz.name().to_string(),
        from,
        to,
        window_from,
        window_to,
        net_sales: t.net_sales,
        gross_sales: t.gross_sales,
        refunded_amount: t.refunded_amount,
        order_count: t.order_count,
        average_ticket: average_ticket(t.net_sales, t.order_count),
        tenders,
        voided_count: t.voided_count,
        voided_amount: t.voided_amount,
        refunded_orders_count: t.refunded_orders_count,
        refunds_issued_count,
        refunds_issued_amount,
        top_items,
        hourly,
    })
}
