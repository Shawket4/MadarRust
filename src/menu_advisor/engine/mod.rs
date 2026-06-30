//! Pure, I/O-free Menu Advisor engine.
//!
//! Cost-optional is a TYPE-LEVEL design axis:
//!   - `ItemSnapshot.cost_per_serving: Option<i64>`
//!   - `ItemKpi.cost_metrics: Option<CostMetrics>`
//!   - `Classification::Cm` is only producible from cost-tracked items,
//!     `Classification::Revenue` only from cost-missing ones; the two
//!     populations never contaminate each other's thresholds.
//!
//! Determinism: given the same inputs and `now`, the report is byte-stable —
//! every output vec is sorted before returning, and no clock/randomness is
//! read inside the engine.
//!
//! Panic-freedom: the lints below are denied for the whole engine tree; all
//! division and money conversion goes through `stats`, and `validate_report`
//! rejects any non-finite float before it can reach JSON (serde serializes
//! NaN as `null`, which would break the dashboard's types).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::indexing_slicing
)]

mod bundles;
mod classify;
mod explain;
mod kpi;
mod pricing;
mod removal;
mod stats;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use crate::menu_advisor::dto::{
    AdvisorReport, AnalysisConfig, Classification, CmQuadrant, ItemKey, ModeSummary,
};
use bundles::{compute_associations, suggest_bundles};
use classify::classify_items;
use kpi::compute_item_kpis;
use pricing::suggest_prices;
use removal::simulate_removal;

// ═══════════════════════════════════════════════════════════════════
// Engine inputs (constructed by the adapter)
// ═══════════════════════════════════════════════════════════════════

/// All static facts about one SKU.
///
/// `cost_per_serving = None` ⟺ the recipe cost rollup could not be computed
/// (any ingredient missing a cost). This is the engine's single source of
/// truth for "do we know what this costs?" — the adapter MUST NOT use
/// `Some(0)` as a sentinel for missing data (free items legitimately cost 0).
#[derive(Debug, Clone)]
pub struct ItemSnapshot {
    pub key: ItemKey,
    pub category_id: Option<uuid::Uuid>,
    /// Display name; used in explanation strings only.
    pub name: String,
    /// Current list price in minor units (piastres).
    pub current_price: i64,
    /// Recipe cost in minor units. `None` ⟺ any ingredient lacks cost data.
    pub cost_per_serving: Option<i64>,
    pub is_active: bool,
    /// True if this SKU only ever moved inside bundles this window — never
    /// standalone. Excluded from popularity denominators and bundle building.
    pub bundle_only: bool,
}

/// One sale line, post window filtering. Standalone lines only — bundle
/// component movements are basket signal, not price signal.
#[derive(Debug, Clone)]
pub struct SaleEvent {
    pub key: ItemKey,
    pub quantity_sold: i64,
    /// Actual price paid per unit, minor units.
    pub unit_price_paid: i64,
    /// Ingredient cost per unit AT THE MOMENT of sale, minor units.
    /// `None` if cost was unknown at sale time.
    pub unit_cost_at_sale: Option<i64>,
    pub sold_at: DateTime<Utc>,
}

/// One basket = the distinct ItemKeys of a single completed order
/// (including bundle components). Quantity > 1 counts once.
pub type Basket = Vec<ItemKey>;

// ═══════════════════════════════════════════════════════════════════
// Errors
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug)]
pub enum EngineError {
    NoItems,
    /// A non-finite float reached the report boundary — a bug upstream, but
    /// failing the run loudly beats serializing `null` into a number field.
    NonFiniteOutput {
        context: String,
    },
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoItems => write!(f, "no items to analyze"),
            Self::NonFiniteOutput { context } => {
                write!(f, "non-finite value in report: {context}")
            }
        }
    }
}

impl std::error::Error for EngineError {}

// ═══════════════════════════════════════════════════════════════════
// Orchestrator
// ═══════════════════════════════════════════════════════════════════

pub fn run_advisor(
    snapshots: &[ItemSnapshot],
    sales: &[SaleEvent],
    baskets: &[Basket],
    now: DateTime<Utc>,
    config: &AnalysisConfig,
    previous: Option<&HashMap<ItemKey, Classification>>,
    price_changed_keys: &HashSet<ItemKey>,
) -> Result<AdvisorReport, EngineError> {
    let kpis = compute_item_kpis(snapshots, sales, now, config)?;
    let outcome = classify_items(&kpis, previous);
    let assoc = compute_associations(baskets);

    let mut price_suggestions =
        suggest_prices(snapshots, &kpis, &outcome, config, price_changed_keys);
    let mut bundle_suggestions = suggest_bundles(snapshots, &kpis, &outcome, &assoc, config);

    let snap_map: HashMap<ItemKey, &ItemSnapshot> =
        snapshots.iter().map(|s| (s.key.clone(), s)).collect();

    // Removal scenarios ONLY for CM-tracked Dogs.
    let mut removal_scenarios: Vec<_> = outcome
        .map
        .iter()
        .filter(|(_, c)| {
            matches!(
                c,
                Classification::Cm {
                    quadrant: CmQuadrant::Dog
                }
            )
        })
        .filter_map(|(k, _)| simulate_removal(k, &kpis, &assoc, &snap_map, config))
        .collect();

    // Deterministic output order (HashMap iteration is not).
    price_suggestions.sort_by(|a, b| a.key.cmp(&b.key));
    bundle_suggestions.sort_by(|a, b| {
        a.focus_item
            .cmp(&b.focus_item)
            .then_with(|| a.bundle_items.cmp(&b.bundle_items))
    });
    removal_scenarios.sort_by(|a, b| {
        a.net_cm_change
            .partial_cmp(&b.net_cm_change)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.key.cmp(&b.key))
    });

    let mode_summary = {
        let mut s = ModeSummary {
            items_total: kpis.len(),
            ..Default::default()
        };
        for c in outcome.map.values() {
            match c {
                Classification::Cm { .. } => s.items_cm_tracked += 1,
                Classification::Revenue { .. } => s.items_revenue_only += 1,
                Classification::Insufficient => s.items_insufficient += 1,
            }
        }
        s
    };

    let report = AdvisorReport {
        generated_at: now,
        window_days: config.analysis_window_days,
        mode_summary,
        price_suggestions,
        bundle_suggestions,
        removal_scenarios,
    };
    validate_report(&report)?;
    Ok(report)
}

// ═══════════════════════════════════════════════════════════════════
// Output-boundary validation
// ═══════════════════════════════════════════════════════════════════

/// Reject any NaN/∞ before it reaches JSON. Walks every f64 in the report.
fn validate_report(report: &AdvisorReport) -> Result<(), EngineError> {
    fn check(v: f64, context: &str) -> Result<(), EngineError> {
        if v.is_finite() {
            Ok(())
        } else {
            Err(EngineError::NonFiniteOutput {
                context: context.to_string(),
            })
        }
    }
    fn check_opt(v: Option<f64>, context: &str) -> Result<(), EngineError> {
        v.map_or(Ok(()), |v| check(v, context))
    }
    fn check_triplet(
        t: &crate::menu_advisor::dto::Triplet,
        context: &str,
    ) -> Result<(), EngineError> {
        check(t.lo, context)?;
        check(t.mid, context)?;
        check(t.hi, context)
    }

    check(report.window_days, "window_days")?;

    for s in &report.price_suggestions {
        let c = &format!("price_suggestion {}", s.item_name);
        check(s.units_sold_raw, c)?;
        check(s.effective_price, c)?;
        check(s.popularity_share, c)?;
        check_opt(s.cm_per_unit, c)?;
        check_opt(s.margin_pct, c)?;
        check_opt(s.food_cost_pct, c)?;
        check_opt(s.anchors.cost_plus, c)?;
        check(s.anchors.peer_median, c)?;
        check(s.anchors.status_quo, c)?;
        check_opt(s.suggested_delta_pct, c)?;
        check_opt(s.cost_reduction_whatif_margin, c)?;
        if let Some(p) = &s.peer_comparison {
            check(p.median_effective_price_peers, c)?;
            check_opt(p.median_margin_pct_peers, c)?;
            check_opt(p.median_cm_per_unit_peers, c)?;
        }
    }

    for s in &report.bundle_suggestions {
        let c = &format!("bundle_suggestion focus={}", s.focus_item.menu_item_id);
        check(s.bundle_discount_pct, c)?;
        check_opt(s.bundle_margin_pct, c)?;
        check(s.association.composite_score, c)?;
        for p in &s.association.pair_lifts {
            check(p.lift, c)?;
            check(p.support, c)?;
            check(p.confidence_ab, c)?;
        }
        check_triplet(&s.forecast.expected_velocity, c)?;
        check(s.forecast.inside_bundle_units_x, c)?;
        check(s.forecast.halo_units_x, c)?;
        check(s.forecast.total_units_uplift_x, c)?;
        if let Some(t) = &s.forecast.incremental_cm {
            check_triplet(t, c)?;
        }
    }

    for s in &report.removal_scenarios {
        let c = &format!("removal_scenario {}", s.item_name);
        check(s.baseline_cm, c)?;
        check(s.net_cm_change, c)?;
        check(s.net_cm_change_lo, c)?;
        check(s.net_cm_change_hi, c)?;
        for a in &s.absorbed_by {
            check(a.absorbed_units, c)?;
            check(a.absorbed_cm, c)?;
        }
        for l in &s.complementary_losses {
            check(l.lost_units, c)?;
            check(l.lost_cm, c)?;
        }
    }

    Ok(())
}
