//! The Bundles report (C6, COMBOS_CONTRACT.md §2.6): each combo, and each
//! deal, as its own line. Item sales keep counting a combo's parts as their
//! items (`line_kind <> 'combo'`); only this report reads the header.
//!
//! Revenue is before the order-level discount (the item-sales basis), voided
//! orders are excluded and refunds are netted, exactly as item sales do.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    authz::{Cap, require::require},
    errors::{AppError, AppErrorResponse},
    menu::bases::extract_claims,
    orders::SOLD,
};

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BundlesQuery {
    /// Business date, inclusive.
    pub from: NaiveDate,
    /// Business date, inclusive.
    pub to: NaiveDate,
    pub branch_id: Option<Uuid>,
    /// `combo` | `deal`; omitted = both.
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct BundlesMixQuery {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub branch_id: Option<Uuid>,
}

/// One combo or deal over the period.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct BundlesRow {
    /// `combo` | `deal`.
    pub kind: String,
    /// The combo's menu item id, or the deal rule's id.
    pub id: Uuid,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    /// Combo units (Σ header quantity, refunds netted) or deal applications (Σ times).
    pub sold: i64,
    pub orders: i64,
    /// A combo: Σ its parts' line_total + their add-ons. A deal: Σ the
    /// consumed lines' line_total (after the deal).
    pub revenue: i64,
    /// The same lines at their normal prices.
    pub list_value: i64,
    /// `list_value − revenue`.
    pub saving: i64,
    pub cost: i64,
    /// True when any line's cost was unknown (`cost` then counts the known part).
    pub cost_missing: bool,
    /// `(revenue − cost) / revenue` as a fraction string; `null` when revenue is 0.
    pub margin: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
pub struct BundlesTotals {
    pub sold: i64,
    pub revenue: i64,
    pub list_value: i64,
    pub saving: i64,
    pub cost: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct BundlesReport {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub rows: Vec<BundlesRow>,
    pub totals: BundlesTotals,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct MixPick {
    pub menu_item_id: Option<Uuid>,
    pub name: String,
    #[schema(value_type = Object)]
    pub name_translations: serde_json::Value,
    pub size_label: Option<String>,
    /// Units picked (Σ part quantity, refunds netted).
    pub count: i64,
    pub surcharge_total: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct MixSlot {
    /// `null` for parts whose slot was deleted since.
    pub slot_id: Option<Uuid>,
    pub name: String,
    pub picks: Vec<MixPick>,
}

/// What customers picked in each slot of one combo.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ComboMix {
    pub combo_id: Uuid,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub slots: Vec<MixSlot>,
}

/// The scope every Bundles query shares: the org's sold orders (voided and
/// fully refunded out, as item sales) whose branch-local business date is in
/// `[$2, $3]`, at the branches in `$4` (`NULL` = every branch of the org).
fn scoped_orders() -> String {
    format!(
        "SELECT o.id FROM orders o JOIN branches b ON b.id = o.branch_id \
          WHERE b.org_id = $1 AND o.{SOLD} \
            AND (o.created_at AT TIME ZONE effective_timezone(o.branch_id))::date BETWEEN $2 AND $3 \
            AND ($4::uuid[] IS NULL OR o.branch_id = ANY($4))"
    )
}

/// Each line's refunded quantity (every refund against it).
const REFUNDED: &str = "SELECT rl.order_item_id, SUM(rl.quantity)::numeric AS r \
                          FROM order_refund_lines rl GROUP BY rl.order_item_id";

/// `(revenue − cost) / revenue` to 4 places, half away from zero.
fn margin(revenue: i64, cost: i64) -> Option<String> {
    use rust_decimal::{Decimal, RoundingStrategy};
    if revenue == 0 {
        return None;
    }
    let m = (Decimal::from(revenue - cost) / Decimal::from(revenue))
        .round_dp_with_strategy(4, RoundingStrategy::MidpointAwayFromZero);
    Some(format!("{m:.4}"))
}

async fn scope(
    pool: &PgPool,
    req: &HttpRequest,
    branch_id: Option<Uuid>,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<(Uuid, Option<Vec<Uuid>>), AppError> {
    let claims = extract_claims(req)?;
    require(pool, &claims, Cap::ReportsBundles, branch_id).await?;
    if to < from {
        return Err(AppError::BadRequest("`to` is before `from`".into()));
    }
    let org = claims
        .org_id()
        .ok_or_else(|| AppError::Forbidden("No organization".into()))?;
    let branches = crate::authz::scope::org_read_branches(pool, &claims, org, branch_id).await?;
    Ok((org, branches))
}

type RowT = (
    Uuid,
    String,
    serde_json::Value,
    i64,
    i64,
    i64,
    i64,
    i64,
    bool,
);

async fn combo_rows(
    pool: &PgPool,
    org: Uuid,
    q: &BundlesQuery,
    branches: &Option<Vec<Uuid>>,
) -> Result<Vec<RowT>, AppError> {
    // A part keeps (q − r)/q of its line (refunds netted per line); a header's
    // units are its quantity less its refunded quantity.
    let sql = format!(
        "WITH s AS ({scoped}), rf AS ({REFUNDED}), \
         h AS (SELECT oi.id, oi.order_id, oi.menu_item_id, oi.item_name, \
                      oi.quantity - COALESCE(rf.r, 0) AS units \
                 FROM order_items oi JOIN s ON s.id = oi.order_id \
                 LEFT JOIN rf ON rf.order_item_id = oi.id \
                WHERE oi.line_kind = 'combo' AND oi.menu_item_id IS NOT NULL), \
         p AS (SELECT h.menu_item_id AS combo_id, \
                      (oi.quantity - COALESCE(rf.r, 0)) / NULLIF(oi.quantity, 0)::numeric AS f, \
                      oi.line_total + COALESCE(ad.total, 0) AS revenue, \
                      oi.unit_price::bigint * oi.quantity + COALESCE(ad.total, 0) AS list, \
                      COALESCE(oi.line_cost, 0) AS cost, oi.cost_missing \
                 FROM order_items oi JOIN h ON h.id = oi.combo_line_id \
                 LEFT JOIN rf ON rf.order_item_id = oi.id \
                 LEFT JOIN LATERAL (SELECT SUM(a.line_total)::bigint AS total \
                                      FROM order_item_addons a WHERE a.order_item_id = oi.id) ad ON true \
                WHERE oi.line_kind = 'combo_part'), \
         hs AS (SELECT menu_item_id, MAX(item_name) AS item_name, SUM(units) AS sold, \
                       COUNT(DISTINCT order_id) FILTER (WHERE units > 0) AS orders \
                  FROM h GROUP BY menu_item_id), \
         ps AS (SELECT combo_id, ROUND(SUM(revenue * COALESCE(f, 0)))::bigint AS revenue, \
                       ROUND(SUM(list * COALESCE(f, 0)))::bigint AS list, \
                       ROUND(SUM(cost * COALESCE(f, 0)))::bigint AS cost, \
                       COALESCE(BOOL_OR(cost_missing AND f > 0), false) AS cost_missing \
                  FROM p GROUP BY combo_id) \
         SELECT hs.menu_item_id, COALESCE(mi.name, hs.item_name), \
                COALESCE(mi.name_translations, '{{}}'::jsonb), \
                hs.sold::bigint, hs.orders, COALESCE(ps.revenue, 0), COALESCE(ps.list, 0), \
                COALESCE(ps.cost, 0), COALESCE(ps.cost_missing, false) \
           FROM hs LEFT JOIN ps ON ps.combo_id = hs.menu_item_id \
           LEFT JOIN menu_items mi ON mi.id = hs.menu_item_id",
        scoped = scoped_orders()
    );
    Ok(sqlx::query_as(&sql)
        .bind(org)
        .bind(q.from)
        .bind(q.to)
        .bind(branches)
        .fetch_all(pool)
        .await?)
}

async fn deal_rows(
    pool: &PgPool,
    org: Uuid,
    q: &BundlesQuery,
    branches: &Option<Vec<Uuid>>,
) -> Result<Vec<RowT>, AppError> {
    // Only the units a deal consumed: revenue = units × unit_price − the
    // deal's cut, cost = the line's cost for those units; each netted by the
    // line's refunded share.
    let sql = format!(
        "WITH s AS ({scoped}), rf AS ({REFUNDED}), \
         l AS (SELECT od.deal_rule_id, od.order_id, od.id AS od_id, od.times, od.deal_name, \
                      od.name_translations, \
                      (oi.quantity - COALESCE(rf.r, 0)) / NULLIF(oi.quantity, 0)::numeric AS f, \
                      dl.units::bigint * oi.unit_price - dl.discount AS revenue, \
                      dl.units::bigint * oi.unit_price AS list, \
                      COALESCE(oi.line_cost, 0) * dl.units / NULLIF(oi.quantity, 0)::numeric AS cost, \
                      oi.cost_missing \
                 FROM order_deals od JOIN s ON s.id = od.order_id \
                 JOIN order_deal_lines dl ON dl.order_deal_id = od.id \
                 JOIN order_items oi ON oi.id = dl.order_item_id \
                 LEFT JOIN rf ON rf.order_item_id = oi.id), \
         t AS (SELECT DISTINCT od_id, deal_rule_id, times FROM l) \
         SELECT l.deal_rule_id, COALESCE(MAX(dr.name), MAX(l.deal_name)), \
                COALESCE((ARRAY_AGG(dr.name_translations))[1], (ARRAY_AGG(l.name_translations))[1], '{{}}'::jsonb), \
                (SELECT COALESCE(SUM(t.times), 0) FROM t WHERE t.deal_rule_id = l.deal_rule_id)::bigint, \
                COUNT(DISTINCT l.order_id), \
                ROUND(SUM(l.revenue * COALESCE(l.f, 0)))::bigint, \
                ROUND(SUM(l.list * COALESCE(l.f, 0)))::bigint, \
                ROUND(SUM(l.cost * COALESCE(l.f, 0)))::bigint, \
                COALESCE(BOOL_OR(l.cost_missing AND l.f > 0), false) \
           FROM l LEFT JOIN deal_rules dr ON dr.id = l.deal_rule_id \
          GROUP BY l.deal_rule_id",
        scoped = scoped_orders()
    );
    Ok(sqlx::query_as(&sql)
        .bind(org)
        .bind(q.from)
        .bind(q.to)
        .bind(branches)
        .fetch_all(pool)
        .await?)
}

#[utoipa::path(
    get,
    path = "/reports/bundles",
    tag = "reports",
    params(BundlesQuery),
    responses((status = 200, description = "Each combo and deal as its own line", body = BundlesReport), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
#[tracing::instrument(skip_all, fields(from = %query.from, to = %query.to))]
pub async fn bundles_report(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<BundlesQuery>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let (org, branches) = scope(pool, &req, query.branch_id, query.from, query.to).await?;
    let (combos, deals) = match query.kind.as_deref() {
        None => (true, true),
        Some("combo") => (true, false),
        Some("deal") => (false, true),
        Some(_) => {
            return Err(AppError::BadRequest("`kind` is `combo` or `deal`".into()));
        }
    };
    let mut rows = Vec::new();
    for (kind, on) in [("combo", combos), ("deal", deals)] {
        if !on {
            continue;
        }
        let raw = if kind == "combo" {
            combo_rows(pool, org, &query, &branches).await?
        } else {
            deal_rows(pool, org, &query, &branches).await?
        };
        let mut part: Vec<BundlesRow> = raw
            .into_iter()
            .map(
                |(
                    id,
                    name,
                    name_translations,
                    sold,
                    orders,
                    revenue,
                    list_value,
                    cost,
                    cost_missing,
                )| {
                    BundlesRow {
                        kind: kind.into(),
                        id,
                        name,
                        name_translations,
                        sold,
                        orders,
                        revenue,
                        list_value,
                        saving: list_value - revenue,
                        cost,
                        cost_missing,
                        margin: margin(revenue, cost),
                    }
                },
            )
            .collect();
        part.sort_by(|a, b| {
            b.revenue
                .cmp(&a.revenue)
                .then_with(|| a.name.cmp(&b.name))
                .then(a.id.cmp(&b.id))
        });
        rows.extend(part);
    }
    let mut totals = BundlesTotals::default();
    for r in &rows {
        totals.sold += r.sold;
        totals.revenue += r.revenue;
        totals.list_value += r.list_value;
        totals.saving += r.saving;
        totals.cost += r.cost;
    }
    Ok(HttpResponse::Ok().json(BundlesReport {
        from: query.from,
        to: query.to,
        rows,
        totals,
    }))
}

#[utoipa::path(
    get,
    path = "/reports/bundles/combos/{id}/mix",
    tag = "reports",
    params(("id" = Uuid, Path, description = "The combo's menu item id"), BundlesMixQuery),
    responses((status = 200, description = "What customers picked per slot", body = ComboMix), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
#[tracing::instrument(skip_all, fields(combo_id = %*id, from = %query.from, to = %query.to))]
pub async fn combo_mix(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
    query: web::Query<BundlesMixQuery>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let combo_id = *id;
    let (org, branches) = scope(pool, &req, query.branch_id, query.from, query.to).await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM menu_items WHERE id = $1 AND org_id = $2 AND kind = 'combo')",
    )
    .bind(combo_id)
    .bind(org)
    .fetch_one(pool)
    .await?;
    if !exists {
        return Err(AppError::NotFound("Combo not found".into()));
    }
    let sql = format!(
        "WITH s AS ({scoped}), rf AS ({REFUNDED}), \
         p AS (SELECT oi.combo_slot_id, oi.combo_slot_name, oi.menu_item_id, oi.item_name, oi.size_label, \
                      oi.quantity - COALESCE(rf.r, 0) AS units, \
                      oi.combo_surcharge * (oi.quantity - COALESCE(rf.r, 0)) / NULLIF(oi.quantity, 0)::numeric AS surcharge \
                 FROM order_items h JOIN s ON s.id = h.order_id \
                 JOIN order_items oi ON oi.combo_line_id = h.id AND oi.line_kind = 'combo_part' \
                 LEFT JOIN rf ON rf.order_item_id = oi.id \
                WHERE h.line_kind = 'combo' AND h.menu_item_id = $5) \
         SELECT p.combo_slot_id, COALESCE(MAX(cs.name), MAX(p.combo_slot_name), ''), \
                MIN(COALESCE(cs.sort, 2147483647)), p.menu_item_id, \
                COALESCE(MAX(mi.name), MAX(p.item_name)), \
                COALESCE((ARRAY_AGG(mi.name_translations))[1], '{{}}'::jsonb), p.size_label, \
                SUM(p.units)::bigint, ROUND(COALESCE(SUM(p.surcharge), 0))::bigint \
           FROM p LEFT JOIN combo_slots cs ON cs.id = p.combo_slot_id \
           LEFT JOIN menu_items mi ON mi.id = p.menu_item_id \
          GROUP BY p.combo_slot_id, p.menu_item_id, p.size_label \
         HAVING SUM(p.units) > 0",
        scoped = scoped_orders()
    );
    #[allow(clippy::type_complexity)]
    let raw: Vec<(
        Option<Uuid>,
        String,
        i32,
        Option<Uuid>,
        String,
        serde_json::Value,
        Option<String>,
        i64,
        i64,
    )> = sqlx::query_as(&sql)
        .bind(org)
        .bind(query.from)
        .bind(query.to)
        .bind(&branches)
        .bind(combo_id)
        .fetch_all(pool)
        .await?;
    let mut slots: Vec<(i32, MixSlot)> = Vec::new();
    for (
        slot_id,
        slot_name,
        sort,
        menu_item_id,
        name,
        name_translations,
        size_label,
        count,
        surcharge_total,
    ) in raw
    {
        let pick = MixPick {
            menu_item_id,
            name,
            name_translations,
            size_label,
            count,
            surcharge_total,
        };
        match slots.iter_mut().find(|(_, s)| s.slot_id == slot_id) {
            Some((_, s)) => s.picks.push(pick),
            None => slots.push((
                sort,
                MixSlot {
                    slot_id,
                    name: slot_name,
                    picks: vec![pick],
                },
            )),
        }
    }
    slots.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
    let slots = slots
        .into_iter()
        .map(|(_, mut s)| {
            s.picks.sort_by(|a, b| {
                b.count
                    .cmp(&a.count)
                    .then_with(|| a.name.cmp(&b.name))
                    .then_with(|| a.size_label.cmp(&b.size_label))
            });
            s
        })
        .collect();
    Ok(HttpResponse::Ok().json(ComboMix {
        combo_id,
        from: query.from,
        to: query.to,
        slots,
    }))
}
