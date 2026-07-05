//! Insights handlers: the margin ledger (+ signals), margin targets, the
//! decision log (+ measured impact), and the dashboard margin-watch summary.
//!
//! Money is integer piastres. The ledger's default cost basis is the sale-time
//! SNAPSHOT (`order_items.unit_cost` — history stays truthful when ingredient
//! prices move); `cost_basis=current` re-prices realized quantities under
//! today's recipe rollups. An unknown cost is `null`, never 0.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use actix_web::HttpMessage;

use crate::auth::guards::require_same_org;
use crate::auth::jwt::Claims;
use crate::errors::AppError;
use crate::permissions::checker::check_permission;
use crate::reports::handlers::resolve_report_branches;

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

/// Built-in gross-margin bar when the org never set one. 60% is the industry
/// F&B rule-of-thumb; the header shows `target_source = "default"` so the UI
/// can nudge the operator to set a real one.
const DEFAULT_TARGET_PCT: f64 = 60.0;
/// An ingredient cost move (within the period) above this flags a cost spike.
const SPIKE_PCT: f64 = 10.0;
/// `price_candidate` fires when a top-quartile seller sits this many points
/// under the target (a nudge buffer so it doesn't fire at -0.1pt).
const PRICE_BUFFER_PCT: f64 = 5.0;
/// A dismissal suppresses that signal for the SKU for this long…
const SUPPRESS_DAYS: i64 = 30;
/// …unless the margin has worsened by at least this many points since.
const WORSENED_PTS: f64 = 5.0;
/// Decision baselines + impact windows span this many days.
const BASELINE_DAYS: i64 = 28;
/// Suggested prices round UP to whole EGP (100 piastres).
const PRICE_ROUND: i64 = 100;
/// Margin-based signals need at least this many units sold in the window —
/// two stray sales are noise, not evidence. `below_cost` is exempt (selling
/// under cost even once is a hard fact worth surfacing).
const MIN_SIGNAL_QTY: i64 = 5;
/// Elasticity estimates outside this band are treated as noise and ignored
/// (F&B own-price elasticity plausibly sits in roughly −0.2…−4).
const ELASTICITY_BAND: (f64, f64) = (-6.0, -0.05);
/// Elasticity pairs need this many units on BOTH sides and a ≥2% price move.
const ELASTICITY_MIN_QTY: f64 = 5.0;
/// Adaptive thresholds: for each signal kind, every net-dismissed SKU beyond
/// the grace count raises that kind's evidence bar, capped.
const ADAPTIVE_GRACE: i64 = 2;
const ADAPTIVE_PTS_PER: f64 = 2.0;
const ADAPTIVE_MAX_PTS: f64 = 10.0;
/// The optimizer never suggests more than this single-step price increase.
const MAX_PRICE_STEP_PCT: f64 = 25.0;

// ── Wire types ────────────────────────────────────────────────────────────────

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LedgerQuery {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    /// `snapshot` (default) | `current`.
    pub cost_basis: Option<String>,
}

/// One advisory flag on a ledger row. `params` carries the evidence numbers the
/// client templates into a localized reason; `link` names the fix surface.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Signal {
    /// below_cost | below_target | cost_spike | price_candidate |
    /// removal_candidate | recipe_incomplete
    pub kind: String,
    #[schema(value_type = Object)]
    pub params: serde_json::Value,
    /// Where the fix lives: `pricing` | `studio` | `studio_recipe`.
    pub link: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MarginLedgerRow {
    pub menu_item_id: Uuid,
    /// `"one_size"` for items without sizes.
    pub size_label: String,
    pub item_name: String,
    pub category_id: Option<Uuid>,
    pub category_name: Option<String>,
    /// False when this SKU no longer exists on the active menu (historical
    /// sales under a removed size/item).
    pub on_menu: bool,
    pub quantity_sold: i64,
    pub revenue: i64,
    /// Piastres under the chosen basis; `null` = unknown (never 0).
    pub cost: Option<i64>,
    pub margin: Option<i64>,
    pub margin_pct: Option<f64>,
    /// This row's share of the total KNOWN margin (null when margin unknown
    /// or total margin ≤ 0).
    pub margin_share_pct: Option<f64>,
    /// Previous equal-length period, for the trend column.
    pub prev_quantity: i64,
    pub prev_margin: Option<i64>,
    /// Classic menu-engineering class (Kasavana–Smith): `star` | `workhorse` |
    /// `challenge` | `dog`. High/low popularity splits at the 70%-rule
    /// threshold (0.70/n of tracked units); high/low profit splits at the
    /// weighted-average unit contribution margin. `null` for rows that can't
    /// be classified (no sales in the period, or cost unknown).
    pub class: Option<String>,
    /// This SKU's share of tracked units (the popularity axis), when classified.
    pub popularity_pct: Option<f64>,
    pub flags: Vec<Signal>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LedgerTotals {
    pub revenue: i64,
    /// Cost summed over rows where it is known.
    pub cost_known: i64,
    pub margin_known: i64,
    pub margin_pct: Option<f64>,
    /// Revenue sitting on rows whose cost is unknown (visibly reconciles).
    pub revenue_cost_unknown: i64,
    pub prev_revenue: i64,
    pub prev_margin_known: i64,
    /// Σ(target·revenue − margin) over below-target rows — "margin left on
    /// the table" this period, in piastres.
    pub below_target_gap: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MarginLedgerReport {
    pub branch_id: Uuid,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub cost_basis: String,
    pub target_pct: f64,
    /// `branch` | `org` | `default`.
    pub target_source: String,
    pub totals: LedgerTotals,
    pub rows: Vec<MarginLedgerRow>,
    /// Rows whose cost is unknown under the chosen basis (they ARE in `rows`).
    pub rows_cost_unknown: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MarginWatch {
    pub branch_id: Uuid,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub target_pct: f64,
    pub totals: LedgerTotals,
    /// Top contributors by known margin (desc), max 3.
    pub top: Vec<MarginLedgerRow>,
    /// Worst contributors (asc, only rows with known margin), max 3.
    pub bottom: Vec<MarginLedgerRow>,
    pub open_signals: i64,
    pub rows_cost_unknown: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MarginTargets {
    pub org_default_pct: Option<f64>,
    pub branches: Vec<BranchTarget>,
    pub builtin_default_pct: f64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BranchTarget {
    pub branch_id: Uuid,
    pub target_pct: f64,
}

#[derive(Deserialize, ToSchema)]
pub struct PutTargetRequest {
    /// Omit for the org default; set for a branch override.
    pub branch_id: Option<Uuid>,
    pub target_pct: f64,
}

#[derive(Deserialize, ToSchema)]
pub struct CreateDecisionRequest {
    pub branch_id: Option<Uuid>,
    pub menu_item_id: Uuid,
    #[serde(default = "one_size")]
    pub size_label: String,
    pub signal_kind: String,
    /// `acted` | `dismissed` | `snoozed`.
    pub action: String,
    #[serde(default)]
    #[schema(value_type = Object)]
    pub detail: serde_json::Value,
}

fn one_size() -> String {
    "one_size".into()
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DecisionOut {
    pub id: Uuid,
    pub branch_id: Option<Uuid>,
    pub menu_item_id: Uuid,
    pub size_label: String,
    pub item_name: String,
    pub signal_kind: String,
    pub action: String,
    #[schema(value_type = Object)]
    pub detail: serde_json::Value,
    #[schema(value_type = Object)]
    pub baseline: serde_json::Value,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    /// Measured after-window aggregate; `null` until ≥1 day of after-data.
    #[schema(value_type = Object)]
    pub impact: Option<serde_json::Value>,
    /// True once the full baseline window has elapsed since the decision.
    pub impact_complete: bool,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DecisionsQuery {
    pub org_id: Uuid,
    pub branch_id: Option<Uuid>,
    pub limit: Option<i64>,
}

// ── Ledger core (shared by report + watch) ────────────────────────────────────

#[derive(sqlx::FromRow)]
struct SalesRow {
    menu_item_id: Uuid,
    size_label: String,
    item_name: String,
    category_id: Option<Uuid>,
    category_name: Option<String>,
    quantity_sold: i64,
    revenue: i64,
    snapshot_cost: Option<i64>,
}

/// Aggregate non-voided, non-bundle item sales per SKU over a window.
async fn sales_agg(
    pool: &PgPool,
    branch_ids: &[Uuid],
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<Vec<SalesRow>, AppError> {
    let rows = sqlx::query_as::<_, SalesRow>(
        r#"
        SELECT
            oi.menu_item_id,
            COALESCE(oi.size_label::text, 'one_size') AS size_label,
            (array_agg(oi.item_name ORDER BY o.created_at DESC))[1] AS item_name,
            mi.category_id,
            c.name AS category_name,
            SUM(oi.quantity)::bigint   AS quantity_sold,
            SUM(oi.line_total)::bigint AS revenue,
            CASE
                WHEN bool_or(oi.unit_cost IS NULL) THEN NULL
                ELSE SUM(oi.unit_cost * oi.quantity)::bigint
            END AS snapshot_cost
        FROM order_items oi
        JOIN orders o ON o.id = oi.order_id
        JOIN menu_items mi ON mi.id = oi.menu_item_id
        LEFT JOIN categories c ON c.id = mi.category_id
        WHERE o.branch_id = ANY($1)
          AND o.status != 'voided'
          AND oi.menu_item_id IS NOT NULL
          AND oi.bundle_id IS NULL
          AND ($2::timestamptz IS NULL OR o.created_at >= $2)
          AND ($3::timestamptz IS NULL OR o.created_at <= $3)
        GROUP BY oi.menu_item_id, COALESCE(oi.size_label::text, 'one_size'),
                 mi.category_id, c.name
        "#,
    )
    .bind(branch_ids)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The org's ACTIVE catalog SKUs (so zero-sale SKUs appear in the ledger —
/// removal candidates would otherwise be invisible, the old report's blindspot).
#[derive(sqlx::FromRow)]
struct CatalogSku {
    menu_item_id: Uuid,
    size_label: String,
    item_name: String,
    category_id: Option<Uuid>,
    category_name: Option<String>,
}

async fn catalog_skus(pool: &PgPool, org_id: Uuid) -> Result<Vec<CatalogSku>, AppError> {
    let rows = sqlx::query_as::<_, CatalogSku>(
        r#"
        SELECT s.menu_item_id, s.label AS size_label, mi.name AS item_name,
               mi.category_id, c.name AS category_name
        FROM menu_item_sizes s
        JOIN menu_items mi ON mi.id = s.menu_item_id
        LEFT JOIN categories c ON c.id = mi.category_id
        WHERE mi.org_id = $1 AND mi.is_active = true AND s.is_active = true
        "#,
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Resolve the effective margin target: branch override → org default → builtin.
async fn resolve_target(
    pool: &PgPool,
    org_id: Uuid,
    branch: Option<Uuid>,
) -> Result<(f64, &'static str), AppError> {
    let rows: Vec<(Option<Uuid>, f64)> = sqlx::query_as(
        "SELECT branch_id, target_pct::float8 FROM margin_targets \
         WHERE org_id = $1 AND (branch_id IS NULL OR branch_id = $2)",
    )
    .bind(org_id)
    .bind(branch)
    .fetch_all(pool)
    .await?;
    if let Some(b) = branch
        && let Some((_, pct)) = rows.iter().find(|(bid, _)| *bid == Some(b))
    {
        return Ok((*pct, "branch"));
    }
    if let Some((_, pct)) = rows.iter().find(|(bid, _)| bid.is_none()) {
        return Ok((*pct, "org"));
    }
    Ok((DEFAULT_TARGET_PCT, "default"))
}

/// Ingredient cost moves >SPIKE_PCT within [from,to], mapped to the SKUs whose
/// recipes consume them. Ingredient-level (same unit over time, so no unit
/// math): "milk ↑14%" — explainable, and names the driver.
async fn cost_spikes(
    pool: &PgPool,
    org_id: Uuid,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<std::collections::HashMap<(Uuid, String), serde_json::Value>, AppError> {
    let Some(from) = from else {
        return Ok(Default::default());
    };
    let to = to.unwrap_or_else(Utc::now);
    // Latest cost strictly before the window vs latest within it, per ingredient.
    #[derive(sqlx::FromRow)]
    struct Spike {
        menu_item_id: Uuid,
        size_label: String,
        ingredient_name: String,
        before: f64,
        after: f64,
    }
    let rows: Vec<Spike> = sqlx::query_as(
        r#"
        WITH before AS (
            SELECT DISTINCT ON (h.org_ingredient_id)
                   h.org_ingredient_id, h.cost_per_unit
            FROM ingredient_cost_history h
            JOIN org_ingredients oi ON oi.id = h.org_ingredient_id
            WHERE oi.org_id = $1 AND h.effective_from < $2
            ORDER BY h.org_ingredient_id, h.effective_from DESC
        ),
        after AS (
            SELECT DISTINCT ON (h.org_ingredient_id)
                   h.org_ingredient_id, h.cost_per_unit
            FROM ingredient_cost_history h
            JOIN org_ingredients oi ON oi.id = h.org_ingredient_id
            WHERE oi.org_id = $1 AND h.effective_from >= $2 AND h.effective_from <= $3
            ORDER BY h.org_ingredient_id, h.effective_from DESC
        )
        SELECT s.menu_item_id, s.label AS size_label, ing.name AS ingredient_name,
               before.cost_per_unit::float8 AS before, after.cost_per_unit::float8 AS after
        FROM after
        JOIN before ON before.org_ingredient_id = after.org_ingredient_id
        JOIN org_ingredients ing ON ing.id = after.org_ingredient_id
        JOIN recipe_lines rl ON rl.ingredient_id = after.org_ingredient_id
                            AND rl.owner_type = 'item_size'
        JOIN menu_item_sizes s ON s.id = rl.owner_id
        WHERE before.cost_per_unit > 0
        "#,
    )
    .bind(org_id)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;

    let mut out: std::collections::HashMap<(Uuid, String), serde_json::Value> = Default::default();
    for s in rows {
        let pct = (s.after - s.before) / s.before * 100.0;
        if pct.abs() < SPIKE_PCT {
            continue;
        }
        // Keep the largest driver per SKU.
        let entry = out.entry((s.menu_item_id, s.size_label.clone()));
        let candidate =
            json!({ "ingredient": s.ingredient_name, "pct": (pct * 10.0).round() / 10.0 });
        use std::collections::hash_map::Entry;
        match entry {
            Entry::Vacant(v) => {
                v.insert(candidate);
            }
            Entry::Occupied(mut o) => {
                let prev = o.get()["pct"].as_f64().unwrap_or(0.0).abs();
                if pct.abs() > prev {
                    o.insert(candidate);
                }
            }
        }
    }
    Ok(out)
}

/// Latest decision per (SKU, kind) within the suppression window.
#[derive(sqlx::FromRow)]
struct Suppression {
    menu_item_id: Uuid,
    size_label: String,
    signal_kind: String,
    action: String,
    baseline_margin_pct: Option<f64>,
}

/// BRANCH-SCOPED: a decision recorded for branch B only suppresses B's ledger
/// (branches differ — a dismissal in Zamalek must not silence New Cairo).
/// Org-wide decisions (branch_id NULL, recorded from the all-branches view)
/// suppress everywhere; the all-branches ledger honors ONLY org-wide ones.
async fn suppressions(
    pool: &PgPool,
    org_id: Uuid,
    branch_scope: Option<Uuid>,
) -> Result<Vec<Suppression>, AppError> {
    let rows: Vec<Suppression> = sqlx::query_as(
        r#"
        SELECT DISTINCT ON (menu_item_id, size_label, signal_kind)
               menu_item_id, size_label, signal_kind, action,
               (baseline->>'margin_pct')::float8 AS baseline_margin_pct
        FROM menu_decisions
        WHERE org_id = $1
          AND (branch_id IS NULL OR branch_id = $2)
          AND created_at > now() - make_interval(days => $3)
        ORDER BY menu_item_id, size_label, signal_kind, created_at DESC
        "#,
    )
    .bind(org_id)
    .bind(branch_scope)
    .bind(SUPPRESS_DAYS as i32)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One window spec for the batched SKU aggregate (a decision's after-window).
struct WindowSpec {
    item: Uuid,
    size_label: String,
    /// `None` = every branch in the org.
    branch: Option<Uuid>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy)]
struct WindowStats {
    quantity: i64,
    revenue: i64,
    /// `None` = any line's snapshot cost unknown, or no sales at all.
    cost: Option<i64>,
    days: f64,
}

/// Aggregate MANY (SKU, branch-scope, window) specs in ONE round trip — the
/// per-decision loop this replaces was an N+1 on the ledger hot path.
async fn sku_windows_batch(
    pool: &PgPool,
    org_branches: &[Uuid],
    specs: &[WindowSpec],
) -> Result<Vec<WindowStats>, AppError> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let idxs: Vec<i64> = (0..specs.len() as i64).collect();
    let items: Vec<Uuid> = specs.iter().map(|s| s.item).collect();
    let sizes: Vec<String> = specs.iter().map(|s| s.size_label.clone()).collect();
    let branches: Vec<Option<Uuid>> = specs.iter().map(|s| s.branch).collect();
    let froms: Vec<DateTime<Utc>> = specs.iter().map(|s| s.from).collect();
    let tos: Vec<DateTime<Utc>> = specs.iter().map(|s| s.to).collect();

    #[derive(sqlx::FromRow)]
    struct Agg {
        idx: i64,
        quantity: i64,
        revenue: i64,
        cost: Option<i64>,
        lines: i64,
    }
    let rows: Vec<Agg> = sqlx::query_as(
        r#"
        WITH spec AS (
            SELECT * FROM UNNEST($1::bigint[], $2::uuid[], $3::text[], $4::uuid[],
                                 $5::timestamptz[], $6::timestamptz[])
                AS t(idx, item_id, size_label, branch_id, w_from, w_to)
        )
        SELECT s.idx,
               COALESCE(SUM(oi.quantity), 0)::bigint   AS quantity,
               COALESCE(SUM(oi.line_total), 0)::bigint AS revenue,
               CASE WHEN COUNT(oi.id) = 0 THEN NULL
                    WHEN bool_or(oi.unit_cost IS NULL) THEN NULL
                    ELSE SUM(oi.unit_cost * oi.quantity)::bigint END AS cost,
               COUNT(oi.id)::bigint AS lines
        FROM spec s
        LEFT JOIN orders o
               ON o.status != 'voided'
              AND o.created_at >= s.w_from AND o.created_at < s.w_to
              AND ((s.branch_id IS NOT NULL AND o.branch_id = s.branch_id)
                   OR (s.branch_id IS NULL AND o.branch_id = ANY($7)))
        LEFT JOIN order_items oi
               ON oi.order_id = o.id
              AND oi.bundle_id IS NULL
              AND oi.menu_item_id = s.item_id
              AND COALESCE(oi.size_label::text, 'one_size') = s.size_label
        GROUP BY s.idx
        ORDER BY s.idx
        "#,
    )
    .bind(&idxs)
    .bind(&items)
    .bind(&sizes)
    .bind(&branches)
    .bind(&froms)
    .bind(&tos)
    .bind(org_branches)
    .fetch_all(pool)
    .await?;

    let mut out: Vec<WindowStats> = specs
        .iter()
        .map(|s| WindowStats {
            quantity: 0,
            revenue: 0,
            cost: None,
            days: ((s.to - s.from).num_seconds() as f64 / 86_400.0).max(0.01),
        })
        .collect();
    for r in rows {
        let i = r.idx as usize;
        out[i].quantity = r.quantity;
        out[i].revenue = r.revenue;
        // COUNT(oi)=0 rows come back with quantity 0 + cost NULL — the "no
        // sales" shape callers already expect.
        out[i].cost = if r.lines == 0 { None } else { r.cost };
    }
    Ok(out)
}

/// What the org's ACTED pricing decisions have measurably taught us per SKU at
/// this scope. A decision needs ≥7 days of after-data before it speaks.
struct PriorOutcome {
    /// Δ margin/day (after − baseline) of the LATEST measured change, piastres.
    margin_per_day_delta: f64,
    qty_per_day_delta: f64,
    /// Own-price elasticity estimated from measured (price, volume) pairs —
    /// median of log-arc estimates over this SKU's acted decisions. `None`
    /// until at least one clean pair exists (adequate volume both sides,
    /// ≥2% realized price move, estimate inside the sanity band).
    elasticity: Option<f64>,
}

async fn prior_pricing_outcomes(
    pool: &PgPool,
    org_id: Uuid,
    branch_scope: Option<Uuid>,
) -> Result<std::collections::HashMap<(Uuid, String), PriorOutcome>, AppError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        menu_item_id: Uuid,
        size_label: String,
        branch_id: Option<Uuid>,
        baseline: serde_json::Value,
        created_at: DateTime<Utc>,
    }
    // ALL measurable acted pricing decisions at this scope (latest first): the
    // newest drives the caution rule; every clean pair feeds the elasticity
    // estimate.
    let rows: Vec<Row> = sqlx::query_as(
        r#"
        SELECT menu_item_id, size_label, branch_id, baseline, created_at
        FROM menu_decisions
        WHERE org_id = $1
          AND (branch_id IS NULL OR branch_id = $2)
          AND action = 'acted'
          AND signal_kind IN ('price_candidate', 'below_target', 'below_cost')
          AND created_at <= now() - interval '7 days'
          AND created_at > now() - interval '180 days'
        ORDER BY menu_item_id, size_label, created_at DESC
        "#,
    )
    .bind(org_id)
    .bind(branch_scope)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(Default::default());
    }

    let org_branches: Vec<Uuid> = if rows.iter().any(|r| r.branch_id.is_none()) {
        sqlx::query_scalar("SELECT id FROM branches WHERE org_id = $1")
            .bind(org_id)
            .fetch_all(pool)
            .await?
    } else {
        Vec::new()
    };

    let now = Utc::now();
    let specs: Vec<WindowSpec> = rows
        .iter()
        .map(|r| WindowSpec {
            item: r.menu_item_id,
            size_label: r.size_label.clone(),
            branch: r.branch_id,
            from: r.created_at,
            to: (r.created_at + Duration::days(BASELINE_DAYS)).min(now),
        })
        .collect();
    let afters = sku_windows_batch(pool, &org_branches, &specs).await?;

    let mut out: std::collections::HashMap<(Uuid, String), PriorOutcome> = Default::default();
    let mut elasticity_pairs: std::collections::HashMap<(Uuid, String), Vec<f64>> =
        Default::default();

    for (r, after) in rows.iter().zip(afters.iter()) {
        let key = (r.menu_item_id, r.size_label.clone());
        let (Some(base_m), Some(base_days), Some(base_qpd), Some(base_qty), Some(base_rev)) = (
            r.baseline["margin"].as_f64(),
            r.baseline["window_days"].as_f64(),
            r.baseline["qty_per_day"].as_f64(),
            r.baseline["quantity"].as_f64(),
            r.baseline["revenue"].as_f64(),
        ) else {
            continue; // baseline incomplete — nothing to learn from
        };
        let Some(after_margin) = after.cost.map(|c| (after.revenue - c) as f64) else {
            continue; // after-window cost unknown
        };

        // Latest decision per SKU (rows are DESC) drives the caution deltas.
        let after_qpd = after.quantity as f64 / after.days;
        out.entry(key.clone()).or_insert_with(|| PriorOutcome {
            margin_per_day_delta: after_margin / after.days - base_m / base_days.max(0.01),
            qty_per_day_delta: after_qpd - base_qpd,
            elasticity: None,
        });

        // Elasticity pair: realized average unit price before vs after (derived
        // from order history itself — no reliance on operator-typed details).
        if base_qty >= ELASTICITY_MIN_QTY && after.quantity as f64 >= ELASTICITY_MIN_QTY {
            let p0 = base_rev / base_qty;
            let p1 = after.revenue as f64 / after.quantity as f64;
            if p0 > 0.0 && p1 > 0.0 && (p1 / p0 - 1.0).abs() >= 0.02 && base_qpd > 0.0 {
                let e = (after_qpd / base_qpd).ln() / (p1 / p0).ln();
                if e >= ELASTICITY_BAND.0 && e <= ELASTICITY_BAND.1 {
                    elasticity_pairs.entry(key).or_default().push(e);
                }
            }
        }
    }

    // Median elasticity per SKU (robust to one weird period).
    for (key, mut es) in elasticity_pairs {
        es.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = es[es.len() / 2];
        if let Some(o) = out.get_mut(&key) {
            o.elasticity = Some(median);
        }
    }
    Ok(out)
}

/// Per-kind adaptive evidence bars learned from the org's dismissal patterns at
/// this scope: every net-dismissed SKU of a kind beyond the grace count raises
/// that kind's bar (capped). Hard facts (`below_cost`, `cost_spike`) never damp.
async fn kind_bars(
    pool: &PgPool,
    org_id: Uuid,
    branch_scope: Option<Uuid>,
) -> Result<std::collections::HashMap<String, f64>, AppError> {
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        r#"
        SELECT signal_kind,
               COUNT(DISTINCT (menu_item_id, size_label))
                   FILTER (WHERE action IN ('dismissed', 'snoozed'))::bigint AS dismissed,
               COUNT(DISTINCT (menu_item_id, size_label))
                   FILTER (WHERE action = 'acted')::bigint AS acted
        FROM menu_decisions
        WHERE org_id = $1
          AND (branch_id IS NULL OR branch_id = $2)
          AND created_at > now() - interval '90 days'
        GROUP BY signal_kind
        "#,
    )
    .bind(org_id)
    .bind(branch_scope)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter(|(kind, ..)| kind != "below_cost" && kind != "cost_spike")
        .map(|(kind, dismissed, acted)| {
            let extra = ((dismissed - acted - ADAPTIVE_GRACE).max(0) as f64 * ADAPTIVE_PTS_PER)
                .min(ADAPTIVE_MAX_PTS);
            (kind, extra)
        })
        .filter(|(_, extra)| *extra > 0.0)
        .collect())
}

struct Ledger {
    rows: Vec<MarginLedgerRow>,
    totals: LedgerTotals,
    rows_cost_unknown: i64,
    target_pct: f64,
    target_source: &'static str,
    open_signals: i64,
}

/// Cost basis for the ledger.
#[derive(Clone, Copy, PartialEq)]
enum Basis {
    Snapshot,
    Current,
}

impl Basis {
    fn parse(s: Option<&str>) -> Result<Self, AppError> {
        match s {
            None | Some("snapshot") => Ok(Self::Snapshot),
            Some("current") => Ok(Self::Current),
            Some(other) => Err(AppError::BadRequest(format!(
                "cost_basis must be 'snapshot' or 'current', got '{other}'"
            ))),
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::Current => "current",
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn build_ledger(
    pool: &PgPool,
    org_id: Uuid,
    branch_ids: &[Uuid],
    branch_scope: Option<Uuid>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    basis: Basis,
) -> Result<Ledger, AppError> {
    // Current + previous equal-length windows (trend). Open-ended windows have
    // no previous period.
    let (prev_from, prev_to) = match (from, to) {
        (Some(f), Some(t)) if t > f => (Some(f - (t - f)), Some(f)),
        (Some(f), None) => {
            let t = Utc::now();
            (Some(f - (t - f)), Some(f))
        }
        _ => (None, None),
    };

    // Every input is independent — fetch concurrently (this runs on the
    // dashboard home page; serial awaits were pure added latency).
    let prev_fut = async {
        if prev_from.is_some() {
            sales_agg(pool, branch_ids, prev_from, prev_to).await
        } else {
            Ok(Vec::new())
        }
    };
    let (sales, prev, catalog, (target_pct, target_source), spikes, supp, outcomes, bars, skus) = tokio::try_join!(
        sales_agg(pool, branch_ids, from, to),
        prev_fut,
        catalog_skus(pool, org_id),
        resolve_target(pool, org_id, branch_scope),
        cost_spikes(pool, org_id, from, to),
        suppressions(pool, org_id, branch_scope),
        prior_pricing_outcomes(pool, org_id, branch_scope),
        kind_bars(pool, org_id, branch_scope),
        crate::costing::org_sku_costs(pool, org_id, branch_scope),
    )?;

    // Current recipe rollups: the cost source under `current`, and the
    // recipe-incomplete signal under both bases.
    let current_costs: std::collections::HashMap<(Uuid, String), Option<i64>> = skus
        .into_iter()
        .map(|s| ((s.menu_item_id, s.size_label), s.cost))
        .collect();

    // Previous-period revenue total (for the header trend) — from the same
    // prev fetch, before it is consumed into the per-SKU map.
    let prev_revenue_total: i64 = prev.iter().map(|r| r.revenue).sum();

    // Window length in days — turns the current window's quantities into a
    // rate for the elasticity forecast. `None` for open-ended windows.
    let window_days: Option<f64> = match (from, to) {
        (Some(f), t) => {
            let t = t.unwrap_or_else(Utc::now);
            (t > f).then(|| ((t - f).num_seconds() as f64 / 86_400.0).max(0.01))
        }
        _ => None,
    };

    let prev_by: std::collections::HashMap<(Uuid, String), (i64, Option<i64>)> = prev
        .into_iter()
        .map(|r| {
            (
                (r.menu_item_id, r.size_label),
                (r.quantity_sold, r.snapshot_cost.map(|c| r.revenue - c)),
            )
        })
        .collect();

    // Merge catalog SKUs (zero-sale rows included) with sales (historical rows
    // kept, marked off-menu).
    let mut by_key: std::collections::HashMap<(Uuid, String), MarginLedgerRow> = Default::default();
    for c in catalog {
        by_key.insert(
            (c.menu_item_id, c.size_label.clone()),
            MarginLedgerRow {
                menu_item_id: c.menu_item_id,
                size_label: c.size_label,
                item_name: c.item_name,
                category_id: c.category_id,
                category_name: c.category_name,
                on_menu: true,
                quantity_sold: 0,
                revenue: 0,
                cost: None,
                margin: None,
                margin_pct: None,
                margin_share_pct: None,
                prev_quantity: 0,
                prev_margin: None,
                class: None,
                popularity_pct: None,
                flags: Vec::new(),
            },
        );
    }
    for s in sales {
        let key = (s.menu_item_id, s.size_label.clone());
        let cost = match basis {
            Basis::Snapshot => s.snapshot_cost,
            Basis::Current => current_costs
                .get(&key)
                .copied()
                .flatten()
                .map(|unit| unit * s.quantity_sold),
        };
        let entry = by_key
            .entry(key.clone())
            .or_insert_with(|| MarginLedgerRow {
                menu_item_id: s.menu_item_id,
                size_label: s.size_label.clone(),
                item_name: s.item_name.clone(),
                category_id: s.category_id,
                category_name: s.category_name.clone(),
                on_menu: false,
                quantity_sold: 0,
                revenue: 0,
                cost: None,
                margin: None,
                margin_pct: None,
                margin_share_pct: None,
                prev_quantity: 0,
                prev_margin: None,
                class: None,
                popularity_pct: None,
                flags: Vec::new(),
            });
        entry.quantity_sold = s.quantity_sold;
        entry.revenue = s.revenue;
        entry.cost = cost;
        entry.margin = cost.map(|c| s.revenue - c);
        entry.margin_pct = match (cost, s.revenue) {
            (Some(c), rev) if rev > 0 => Some((rev - c) as f64 / rev as f64 * 100.0),
            _ => None,
        };
    }
    for (key, (pq, pm)) in prev_by {
        if let Some(row) = by_key.get_mut(&key) {
            row.prev_quantity = pq;
            row.prev_margin = pm;
        }
    }

    let mut rows: Vec<MarginLedgerRow> = by_key.into_values().collect();

    // ── Signals ──────────────────────────────────────────────────────────────
    let qty_q3 = {
        let mut sold: Vec<i64> = rows
            .iter()
            .filter(|r| r.quantity_sold > 0)
            .map(|r| r.quantity_sold)
            .collect();
        sold.sort_unstable();
        if sold.is_empty() {
            i64::MAX
        } else {
            // Standard upper-quartile index over n points: floor(3(n−1)/4) —
            // small menus still get a meaningful "top sellers" band.
            sold[(sold.len() - 1) * 3 / 4]
        }
    };
    let suppressed = |r: &MarginLedgerRow, kind: &str| -> bool {
        supp.iter().any(|s| {
            s.menu_item_id == r.menu_item_id
                && s.size_label == r.size_label
                && s.signal_kind == kind
                && match s.action.as_str() {
                    // Dismissed/snoozed: stay quiet unless materially worse.
                    "dismissed" | "snoozed" => match (s.baseline_margin_pct, r.margin_pct) {
                        (Some(base), Some(now)) => now > base - WORSENED_PTS,
                        _ => true,
                    },
                    // Acted: quiet while the change is measuring or once it
                    // WORKED — but a measured-BAD outcome re-raises early so
                    // the caution/revert advice reaches the operator.
                    _ => !outcomes
                        .get(&(r.menu_item_id, r.size_label.clone()))
                        .map(|o| o.margin_per_day_delta < 0.0)
                        .unwrap_or(false),
                }
        })
    };
    let mut open_signals = 0_i64;
    for r in &mut rows {
        let mut flags = Vec::new();
        let current_unknown = current_costs
            .get(&(r.menu_item_id, r.size_label.clone()))
            .map(|c| c.is_none())
            .unwrap_or(false);

        // Adaptive bars: kinds this org keeps dismissing need stronger evidence.
        let bar = |kind: &str| bars.get(kind).copied().unwrap_or(0.0);

        if let (Some(m), true) = (r.margin, r.quantity_sold > 0)
            && m < 0
        {
            // A hard fact — no volume floor, no adaptive damping.
            flags.push(Signal {
                kind: "below_cost".into(),
                params: json!({ "margin": m, "revenue": r.revenue, "cost": r.cost }),
                link: "pricing".into(),
            });
        } else if let Some(pct) = r.margin_pct
            && pct < target_pct - bar("below_target")
            && r.quantity_sold >= MIN_SIGNAL_QTY
        {
            let mut params = json!({
                "margin_pct": (pct * 10.0).round() / 10.0,
                "target_pct": target_pct,
            });
            if bar("below_target") > 0.0 {
                // Transparency: tell the operator the bar was raised (and why).
                params["adaptive_bar"] = json!(bar("below_target"));
            }
            flags.push(Signal {
                kind: "below_target".into(),
                params,
                link: "pricing".into(),
            });
        }
        if let Some(pct) = r.margin_pct
            && r.quantity_sold >= qty_q3
            && pct < target_pct - PRICE_BUFFER_PCT - bar("price_candidate")
            && let Some(cost) = r.cost
            && r.quantity_sold >= MIN_SIGNAL_QTY
        {
            // Baseline suggestion: the target-restoring price (cost ÷ (1−target)).
            let unit_cost = cost as f64 / r.quantity_sold as f64;
            let target_price_raw = unit_cost / (1.0 - target_pct / 100.0);
            let round_up =
                |p: f64| -> i64 { ((p / PRICE_ROUND as f64).ceil() as i64) * PRICE_ROUND };
            let mut suggested = round_up(target_price_raw);
            let mut expected_mpd_delta: Option<f64> = None;

            let prior = outcomes.get(&(r.menu_item_id, r.size_label.clone()));

            // ELASTICITY-AWARE OPTIMIZATION: when this SKU's measured history
            // yields an elasticity estimate, don't jump straight to the
            // target-restoring price — walk the price grid and pick the point
            // maximizing expected margin/day under a constant-elasticity demand
            // model (q ∝ p^E), bounded by the target price and a max step.
            if let (Some(o), Some(days)) = (prior, window_days)
                && let Some(e) = o.elasticity
                && r.quantity_sold > 0
                && r.revenue > 0
            {
                let p0 = r.revenue as f64 / r.quantity_sold as f64; // realized avg price
                let qpd0 = r.quantity_sold as f64 / days;
                let mpd0 = (p0 - unit_cost) * qpd0;
                let cap = (p0 * (1.0 + MAX_PRICE_STEP_PCT / 100.0))
                    .min(target_price_raw.max(p0 + PRICE_ROUND as f64));
                let mut best = (round_up(p0), mpd0);
                let mut p = p0 + PRICE_ROUND as f64;
                while p <= cap + 0.001 {
                    let qpd = qpd0 * (p / p0).powf(e);
                    let mpd = (p - unit_cost) * qpd;
                    if mpd > best.1 {
                        best = (round_up(p), mpd);
                    }
                    p += PRICE_ROUND as f64;
                }
                suggested = best.0;
                expected_mpd_delta = Some(best.1 - mpd0);
            }

            // OUTCOME CAUTION: the latest measured change on this SKU HURT net
            // margin — advise holding/reverting instead of pushing higher.
            let params = match prior {
                Some(o) if o.margin_per_day_delta < 0.0 => json!({
                    "margin_pct": (pct * 10.0).round() / 10.0,
                    "target_pct": target_pct,
                    "caution": true,
                    "last_margin_per_day_delta": o.margin_per_day_delta.round(),
                    "last_qty_per_day_delta": (o.qty_per_day_delta * 100.0).round() / 100.0,
                }),
                _ => {
                    let mut p = json!({
                        "margin_pct": (pct * 10.0).round() / 10.0,
                        "target_pct": target_pct,
                        "suggested_price": suggested,
                        // A prior change that WORKED is evidence the price has room.
                        "last_worked": prior.map(|o| o.margin_per_day_delta > 0.0),
                    });
                    if let Some(e) = prior.and_then(|o| o.elasticity) {
                        p["elasticity"] = json!((e * 100.0).round() / 100.0);
                    }
                    if let Some(d) = expected_mpd_delta {
                        p["expected_margin_per_day_delta"] = json!(d.round());
                    }
                    p
                }
            };
            flags.push(Signal {
                kind: "price_candidate".into(),
                params,
                link: "pricing".into(),
            });
        }
        // Removal: when the org keeps dismissing these, require the DOUBLE
        // signal — unsold this period AND the previous one.
        if r.on_menu
            && r.quantity_sold == 0
            && from.is_some()
            && (bar("removal_candidate") <= 0.0 || r.prev_quantity == 0)
        {
            flags.push(Signal {
                kind: "removal_candidate".into(),
                params: json!({}),
                link: "studio".into(),
            });
        }
        if r.on_menu && current_unknown {
            flags.push(Signal {
                kind: "recipe_incomplete".into(),
                params: json!({}),
                link: "studio_recipe".into(),
            });
        }
        if let Some(spike) = spikes.get(&(r.menu_item_id, r.size_label.clone())) {
            flags.push(Signal {
                kind: "cost_spike".into(),
                params: spike.clone(),
                link: "studio_recipe".into(),
            });
        }
        flags.retain(|f| !suppressed(r, &f.kind));
        open_signals += flags.len() as i64;
        r.flags = flags;
    }

    // ── Totals + share ───────────────────────────────────────────────────────
    let revenue: i64 = rows.iter().map(|r| r.revenue).sum();
    let cost_known: i64 = rows.iter().filter_map(|r| r.cost).sum();
    let margin_known: i64 = rows.iter().filter_map(|r| r.margin).sum();
    let revenue_known: i64 = rows
        .iter()
        .filter(|r| r.cost.is_some())
        .map(|r| r.revenue)
        .sum();
    let revenue_cost_unknown: i64 = rows
        .iter()
        .filter(|r| r.cost.is_none() && r.quantity_sold > 0)
        .map(|r| r.revenue)
        .sum();
    let prev_revenue = prev_revenue_total;
    let prev_margin_known: i64 = rows.iter().filter_map(|r| r.prev_margin).sum();
    let below_target_gap: i64 = rows
        .iter()
        .filter_map(|r| match (r.margin, r.margin_pct) {
            (Some(m), Some(pct)) if pct < target_pct => {
                Some(((target_pct / 100.0) * r.revenue as f64 - m as f64) as i64)
            }
            _ => None,
        })
        .sum();
    if margin_known > 0 {
        for r in &mut rows {
            r.margin_share_pct = r
                .margin
                .map(|m| (m as f64 / margin_known as f64 * 1000.0).round() / 10.0);
        }
    }

    // ── Classic menu-engineering class (the star/workhorse/challenge/dog
    // vocabulary operators already know) — a secondary lens over the same rows,
    // Kasavana–Smith as the retired report computed it: popularity splits at
    // the 70%-rule threshold (0.70/n of tracked units), profit at the
    // weighted-average unit contribution margin. Only rows that SOLD with a
    // KNOWN margin are classified; zero-sale / cost-unknown rows stay `null`
    // rather than being force-binned (honesty over false precision).
    {
        let classified: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.quantity_sold > 0 && r.margin.is_some())
            .map(|(i, _)| i)
            .collect();
        if !classified.is_empty() {
            let total_units: i64 = classified.iter().map(|&i| rows[i].quantity_sold).sum();
            let pop_threshold = 0.70 / classified.len() as f64;
            let (tracked_profit, tracked_units) =
                classified.iter().fold((0_i64, 0_i64), |acc, &i| {
                    (
                        acc.0 + rows[i].margin.unwrap_or(0),
                        acc.1 + rows[i].quantity_sold,
                    )
                });
            let avg_unit_profit = if tracked_units > 0 {
                tracked_profit as f64 / tracked_units as f64
            } else {
                0.0
            };
            for &i in &classified {
                let r = &mut rows[i];
                let pop = r.quantity_sold as f64 / total_units.max(1) as f64;
                let unit_profit = r.margin.unwrap_or(0) as f64 / r.quantity_sold as f64;
                r.popularity_pct = Some((pop * 1000.0).round() / 10.0);
                r.class = Some(
                    match (pop >= pop_threshold, unit_profit >= avg_unit_profit) {
                        (true, true) => "star",
                        (true, false) => "workhorse",
                        (false, true) => "challenge",
                        (false, false) => "dog",
                    }
                    .into(),
                );
            }
        }
    }

    let rows_cost_unknown = rows
        .iter()
        .filter(|r| r.quantity_sold > 0 && r.cost.is_none())
        .count() as i64;

    // Rank: biggest known margin first; unknown-cost rows follow by revenue;
    // zero-sale rows last.
    rows.sort_by(|a, b| match (b.margin, a.margin) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (None, None) => b.revenue.cmp(&a.revenue),
    });

    let margin_pct_total = if revenue_known > 0 {
        Some(margin_known as f64 / revenue_known as f64 * 100.0)
    } else {
        None
    };

    Ok(Ledger {
        rows,
        totals: LedgerTotals {
            revenue,
            cost_known,
            margin_known,
            margin_pct: margin_pct_total,
            revenue_cost_unknown,
            prev_revenue,
            prev_margin_known,
            below_target_gap,
        },
        rows_cost_unknown,
        target_pct,
        target_source,
        open_signals,
    })
}

// ── Handlers ──────────────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/insights/branches/{branch_id}/menu-margin",
    tag = "insights",
    params(("branch_id" = Uuid, Path, description = "Branch id, or the nil UUID for every branch in the org"), LedgerQuery),
    responses((status = 200, description = "Ranked margin ledger with live signals. Cost-unknown rows are returned flagged (margin null) — never 0, never dropped.", body = MarginLedgerReport)),
    security(("bearer_auth" = []))
)]
pub async fn menu_margin_ledger(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query: web::Query<LedgerQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    let (branch_ids, org_id) =
        resolve_report_branches(pool.get_ref(), &claims, &req, *branch_id).await?;
    let basis = Basis::parse(query.cost_basis.as_deref())?;
    let branch_scope = if branch_id.is_nil() {
        None
    } else {
        Some(*branch_id)
    };

    let ledger = build_ledger(
        pool.get_ref(),
        org_id,
        &branch_ids,
        branch_scope,
        query.from,
        query.to,
        basis,
    )
    .await?;

    Ok(HttpResponse::Ok().json(MarginLedgerReport {
        branch_id: *branch_id,
        from: query.from,
        to: query.to,
        cost_basis: basis.as_str().to_string(),
        target_pct: ledger.target_pct,
        target_source: ledger.target_source.to_string(),
        totals: ledger.totals,
        rows: ledger.rows,
        rows_cost_unknown: ledger.rows_cost_unknown,
    }))
}

#[utoipa::path(
    get,
    path = "/insights/branches/{branch_id}/margin-watch",
    tag = "insights",
    params(("branch_id" = Uuid, Path, description = "Branch id, or the nil UUID for org-wide"), LedgerQuery),
    responses((status = 200, description = "Dashboard-home margin summary: totals, top/bottom contributors, open signal count.", body = MarginWatch)),
    security(("bearer_auth" = []))
)]
pub async fn margin_watch(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query: web::Query<LedgerQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    let (branch_ids, org_id) =
        resolve_report_branches(pool.get_ref(), &claims, &req, *branch_id).await?;
    let branch_scope = if branch_id.is_nil() {
        None
    } else {
        Some(*branch_id)
    };

    let ledger = build_ledger(
        pool.get_ref(),
        org_id,
        &branch_ids,
        branch_scope,
        query.from,
        query.to,
        Basis::Snapshot,
    )
    .await?;

    let known: Vec<&MarginLedgerRow> = ledger.rows.iter().filter(|r| r.margin.is_some()).collect();
    let take = |iter: &mut dyn Iterator<Item = &&MarginLedgerRow>| -> Vec<MarginLedgerRow> {
        iter.take(3)
            .map(|r| MarginLedgerRow {
                flags: r.flags.clone(),
                class: r.class.clone(),
                item_name: r.item_name.clone(),
                size_label: r.size_label.clone(),
                category_name: r.category_name.clone(),
                ..**r
            })
            .collect()
    };
    let top = take(&mut known.iter());
    let bottom = take(&mut known.iter().rev());

    Ok(HttpResponse::Ok().json(MarginWatch {
        branch_id: *branch_id,
        from: query.from,
        to: query.to,
        target_pct: ledger.target_pct,
        totals: ledger.totals,
        top,
        bottom,
        open_signals: ledger.open_signals,
        rows_cost_unknown: ledger.rows_cost_unknown,
    }))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct OrgQuery {
    pub org_id: Uuid,
}

#[utoipa::path(
    get,
    path = "/insights/margin-target",
    tag = "insights",
    params(OrgQuery),
    responses((status = 200, body = MarginTargets)),
    security(("bearer_auth" = []))
)]
pub async fn get_margin_targets(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<OrgQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    require_same_org(&claims, Some(query.org_id))?;

    let rows: Vec<(Option<Uuid>, f64)> = sqlx::query_as(
        "SELECT branch_id, target_pct::float8 FROM margin_targets WHERE org_id = $1",
    )
    .bind(query.org_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(MarginTargets {
        org_default_pct: rows.iter().find(|(b, _)| b.is_none()).map(|(_, p)| *p),
        branches: rows
            .iter()
            .filter_map(|(b, p)| {
                b.map(|b| BranchTarget {
                    branch_id: b,
                    target_pct: *p,
                })
            })
            .collect(),
        builtin_default_pct: DEFAULT_TARGET_PCT,
    }))
}

#[utoipa::path(
    put,
    path = "/insights/margin-target",
    tag = "insights",
    request_body = PutTargetRequest,
    params(OrgQuery),
    responses((status = 200, body = MarginTargets)),
    security(("bearer_auth" = []))
)]
pub async fn put_margin_target(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<OrgQuery>,
    body: web::Json<PutTargetRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    require_same_org(&claims, Some(query.org_id))?;
    let b = body.into_inner();
    if !(b.target_pct > 0.0 && b.target_pct < 100.0) {
        return Err(AppError::BadRequest(
            "target_pct must be between 0 and 100 (exclusive)".into(),
        ));
    }

    sqlx::query(
        "INSERT INTO margin_targets (org_id, branch_id, target_pct, updated_by) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (org_id, branch_id) \
         DO UPDATE SET target_pct = EXCLUDED.target_pct, \
                       updated_by = EXCLUDED.updated_by, updated_at = now()",
    )
    .bind(query.org_id)
    .bind(b.branch_id)
    .bind(b.target_pct)
    .bind(claims.user_id_safe()?)
    .execute(pool.get_ref())
    .await?;

    get_margin_targets(req, pool, query).await
}

/// The trailing-window aggregate for one SKU (baseline + impact measurement).
async fn sku_window(
    pool: &PgPool,
    branch_ids: &[Uuid],
    item: Uuid,
    size_label: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<serde_json::Value, AppError> {
    let row: Option<(i64, i64, Option<i64>)> = sqlx::query_as(
        r#"
        SELECT SUM(oi.quantity)::bigint,
               SUM(oi.line_total)::bigint,
               CASE WHEN bool_or(oi.unit_cost IS NULL) THEN NULL
                    ELSE SUM(oi.unit_cost * oi.quantity)::bigint END
        FROM order_items oi
        JOIN orders o ON o.id = oi.order_id
        WHERE o.branch_id = ANY($1) AND o.status != 'voided'
          AND oi.bundle_id IS NULL
          AND oi.menu_item_id = $2
          AND COALESCE(oi.size_label::text, 'one_size') = $3
          AND o.created_at >= $4 AND o.created_at < $5
        HAVING SUM(oi.quantity) IS NOT NULL
        "#,
    )
    .bind(branch_ids)
    .bind(item)
    .bind(size_label)
    .bind(from)
    .bind(to)
    .fetch_optional(pool)
    .await?;

    let days = ((to - from).num_seconds() as f64 / 86_400.0).max(0.01);
    let (qty, revenue, cost) = row.unwrap_or((0, 0, None));
    let margin = cost.map(|c| revenue - c);
    Ok(json!({
        "window_days": (days * 10.0).round() / 10.0,
        "quantity": qty,
        "revenue": revenue,
        "cost": cost,
        "margin": margin,
        "margin_pct": match (margin, revenue) {
            (Some(m), rev) if rev > 0 => Some((m as f64 / rev as f64 * 1000.0).round() / 10.0),
            _ => None,
        },
        "qty_per_day": ((qty as f64 / days) * 100.0).round() / 100.0,
    }))
}

#[utoipa::path(
    post,
    path = "/insights/decisions",
    tag = "insights",
    request_body = CreateDecisionRequest,
    params(OrgQuery),
    responses((status = 201, description = "Decision recorded with a server-computed evidence baseline.", body = DecisionOut)),
    security(("bearer_auth" = []))
)]
pub async fn create_decision(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<OrgQuery>,
    body: web::Json<CreateDecisionRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "menu_items", "update").await?;
    require_same_org(&claims, Some(query.org_id))?;
    let b = body.into_inner();
    const KINDS: [&str; 6] = [
        "below_cost",
        "below_target",
        "cost_spike",
        "price_candidate",
        "removal_candidate",
        "recipe_incomplete",
    ];
    if !KINDS.contains(&b.signal_kind.as_str()) {
        return Err(AppError::BadRequest(format!(
            "unknown signal_kind '{}'",
            b.signal_kind
        )));
    }
    if !["acted", "dismissed", "snoozed"].contains(&b.action.as_str()) {
        return Err(AppError::BadRequest(
            "action must be 'acted', 'dismissed' or 'snoozed'".into(),
        ));
    }

    // Baseline scope: the decision's branch, or every org branch.
    let branch_ids: Vec<Uuid> = match b.branch_id {
        Some(br) => vec![br],
        None => {
            sqlx::query_scalar("SELECT id FROM branches WHERE org_id = $1")
                .bind(query.org_id)
                .fetch_all(pool.get_ref())
                .await?
        }
    };
    let now = Utc::now();
    let baseline = sku_window(
        pool.get_ref(),
        &branch_ids,
        b.menu_item_id,
        &b.size_label,
        now - Duration::days(BASELINE_DAYS),
        now,
    )
    .await?;

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO menu_decisions \
             (org_id, branch_id, menu_item_id, size_label, signal_kind, action, \
              detail, baseline, created_by) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id",
    )
    .bind(query.org_id)
    .bind(b.branch_id)
    .bind(b.menu_item_id)
    .bind(&b.size_label)
    .bind(&b.signal_kind)
    .bind(&b.action)
    .bind(&b.detail)
    .bind(&baseline)
    .bind(claims.user_id_safe()?)
    .fetch_one(pool.get_ref())
    .await?;

    let item_name: String = sqlx::query_scalar("SELECT name FROM menu_items WHERE id = $1")
        .bind(b.menu_item_id)
        .fetch_one(pool.get_ref())
        .await?;

    Ok(HttpResponse::Created().json(DecisionOut {
        id,
        branch_id: b.branch_id,
        menu_item_id: b.menu_item_id,
        size_label: b.size_label,
        item_name,
        signal_kind: b.signal_kind,
        action: b.action,
        detail: b.detail,
        baseline,
        created_by: Some(claims.user_id_safe()?),
        created_at: now,
        impact: None,
        impact_complete: false,
    }))
}

#[utoipa::path(
    get,
    path = "/insights/decisions",
    tag = "insights",
    params(DecisionsQuery),
    responses((status = 200, description = "Decision log, newest first, each with its measured after-window impact.", body = [DecisionOut])),
    security(("bearer_auth" = []))
)]
pub async fn list_decisions(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<DecisionsQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "orders", "read").await?;
    require_same_org(&claims, Some(query.org_id))?;
    let limit = query.limit.unwrap_or(50).clamp(1, 100);

    #[derive(sqlx::FromRow)]
    struct Row {
        id: Uuid,
        branch_id: Option<Uuid>,
        menu_item_id: Uuid,
        size_label: String,
        item_name: String,
        signal_kind: String,
        action: String,
        detail: serde_json::Value,
        baseline: serde_json::Value,
        created_by: Option<Uuid>,
        created_at: DateTime<Utc>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        r#"
        SELECT d.id, d.branch_id, d.menu_item_id, d.size_label, mi.name AS item_name,
               d.signal_kind, d.action, d.detail, d.baseline, d.created_by, d.created_at
        FROM menu_decisions d
        JOIN menu_items mi ON mi.id = d.menu_item_id
        WHERE d.org_id = $1 AND ($2::uuid IS NULL OR d.branch_id = $2)
        ORDER BY d.created_at DESC
        LIMIT $3
        "#,
    )
    .bind(query.org_id)
    .bind(query.branch_id)
    .bind(limit)
    .fetch_all(pool.get_ref())
    .await?;

    let org_branches: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM branches WHERE org_id = $1")
        .bind(query.org_id)
        .fetch_all(pool.get_ref())
        .await?;

    // One batched round trip for every measurable decision's after-window
    // (the per-decision loop was an N+1 with up to `limit` queries).
    let now = Utc::now();
    let measurable: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| now > r.created_at + Duration::days(1))
        .map(|(i, _)| i)
        .collect();
    let specs: Vec<WindowSpec> = measurable
        .iter()
        .map(|&i| {
            let r = &rows[i];
            WindowSpec {
                item: r.menu_item_id,
                size_label: r.size_label.clone(),
                branch: r.branch_id,
                from: r.created_at,
                to: (r.created_at + Duration::days(BASELINE_DAYS)).min(now),
            }
        })
        .collect();
    let stats = sku_windows_batch(pool.get_ref(), &org_branches, &specs).await?;
    let mut impacts: std::collections::HashMap<usize, serde_json::Value> = Default::default();
    for (&i, s) in measurable.iter().zip(stats.iter()) {
        let margin = s.cost.map(|c| s.revenue - c);
        impacts.insert(
            i,
            json!({
                "window_days": (s.days * 10.0).round() / 10.0,
                "quantity": s.quantity,
                "revenue": s.revenue,
                "cost": s.cost,
                "margin": margin,
                "margin_pct": match (margin, s.revenue) {
                    (Some(m), rev) if rev > 0 =>
                        Some((m as f64 / rev as f64 * 1000.0).round() / 10.0),
                    _ => None,
                },
                "qty_per_day": ((s.quantity as f64 / s.days) * 100.0).round() / 100.0,
            }),
        );
    }

    let out: Vec<DecisionOut> = rows
        .into_iter()
        .enumerate()
        .map(|(i, r)| DecisionOut {
            id: r.id,
            branch_id: r.branch_id,
            menu_item_id: r.menu_item_id,
            size_label: r.size_label,
            item_name: r.item_name,
            signal_kind: r.signal_kind,
            action: r.action,
            detail: r.detail,
            baseline: r.baseline,
            created_by: r.created_by,
            created_at: r.created_at,
            impact: impacts.remove(&i),
            impact_complete: now >= r.created_at + Duration::days(BASELINE_DAYS),
        })
        .collect();

    Ok(HttpResponse::Ok().json(out))
}
