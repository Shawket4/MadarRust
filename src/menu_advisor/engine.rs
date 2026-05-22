//! Pure, I/O-free Menu Advisor engine.
//!
//! Cost-optional is a TYPE-LEVEL design axis, not an afterthought:
//!   - `ItemSnapshot.cost_per_serving: Option<i64>`
//!   - `ItemKpi.cost_metrics: Option<CostMetrics>`
//!   - `Classification::Cm(_)` is only producible from CM-tracked items;
//!     `Classification::Revenue(_)` only from revenue-only items.
//!
//! Items without recipe cost data get a parallel taxonomy (Hero / Steady /
//! Slow / Quiet) and a reduced suggestion pipeline (no margin floor, no
//! removal scenarios). Two populations never contaminate each other's
//! thresholds.
//!
//! Zero imports from the rest of the crate. Given the same inputs and `now`
//! this module is fully deterministic.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ═══════════════════════════════════════════════════════════════════
// §1  PRIMITIVE TYPES
// ═══════════════════════════════════════════════════════════════════

/// One sellable SKU: a (menu_item_id, size_label) pair.
/// `size_label = "one_size"` for items without sizes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ItemKey {
    pub menu_item_id: Uuid,
    pub size_label: String,
}

/// All static facts about one SKU, supplied by the adapter.
///
/// `cost_per_serving = None` ⟺ the recipe cost rollup could not be computed
/// (any ingredient missing a cost in `ingredient_cost_history`). This is the
/// engine's single source of truth for "do we know what this costs?". The
/// adapter MUST NOT use `Some(0)` as a sentinel for missing data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemSnapshot {
    pub key: ItemKey,
    pub category_id: Option<Uuid>,
    /// Display name; used in explanation strings only.
    pub name: String,
    /// Current list price in minor units (piastres).
    pub current_price: i64,
    /// Recipe cost in minor units. `None` ⟺ any recipe ingredient lacks cost data.
    pub cost_per_serving: Option<i64>,
    pub is_active: bool,
    /// True if this SKU has only ever appeared inside bundle order lines —
    /// never standalone. Excluded from `popularity_share` denominator.
    pub bundle_only: bool,
}

/// One unit-level sale event, post window filtering.
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

/// One basket = the set of distinct ItemKeys that appeared in a single
/// completed order. Quantity > 1 of the same SKU counts as ONE co-occurrence.
pub type Basket = Vec<ItemKey>;

// ═══════════════════════════════════════════════════════════════════
// §2  CONFIGURATION
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisConfig {
    pub analysis_window_days: f64,
    pub recency_half_life_days: f64,
    pub target_food_cost_pct: f64,
    pub min_gross_margin_pct: f64,
    pub max_price_change_pct_per_cycle: f64,
    pub min_units_for_classification: f64,
    pub min_cooccurrences_for_bundle: f64,
    pub min_lift_for_bundle: f64,
    pub bundle_discount_pct_range: (f64, f64),
    pub price_rounding_rule: PriceRoundingRule,
    pub bundle_top_k_partners: usize,
    pub bundle_max_size: usize,
    pub bundle_top_n_per_focus: usize,
    pub halo_repeat_rate: f64,
    pub promotion_lift_prior: f64,
    /// Conservative max-raise cap for revenue-only items (no margin floor to
    /// guard against). Defaults to 0.05 — half the CM-mode change cap.
    pub revenue_mode_max_raise_pct: f64,
}

impl Default for AnalysisConfig {
    fn default() -> Self {
        Self {
            analysis_window_days:           30.0,
            recency_half_life_days:         14.0,
            target_food_cost_pct:           0.30,
            min_gross_margin_pct:           0.55,
            max_price_change_pct_per_cycle: 0.15,
            min_units_for_classification:   20.0,
            min_cooccurrences_for_bundle:   8.0,
            min_lift_for_bundle:            1.20,
            bundle_discount_pct_range:      (0.10, 0.25),
            price_rounding_rule:            PriceRoundingRule::EgyptianCafe,
            bundle_top_k_partners:          5,
            bundle_max_size:                3,
            bundle_top_n_per_focus:         3,
            halo_repeat_rate:               0.15,
            promotion_lift_prior:           1.25,
            revenue_mode_max_raise_pct:     0.05,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PriceRoundingRule {
    /// Nearest 5 EGP, or 2.5 EGP for items < 25 EGP. No .99 endings.
    EgyptianCafe,
    NearestUnit,
}

// ═══════════════════════════════════════════════════════════════════
// §3  CLASSIFICATION (two parallel taxonomies)
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmQuadrant {
    Star,
    Plowhorse,
    Puzzle,
    Dog,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevenueClass {
    /// High pop + high price/unit  (analog of Star)
    Hero,
    /// High pop + low price/unit   (analog of Plowhorse)
    Steady,
    /// Low pop + high price/unit   (analog of Puzzle)
    Slow,
    /// Low pop + low price/unit    (analog of Dog)
    Quiet,
}

/// The only function that produces this is `classify_items`. By construction:
///   `Cm(_)` ⟹ `kpi.cost_metrics.is_some()`
///   `Revenue(_)` ⟹ `kpi.cost_metrics.is_none()`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Classification {
    Cm { quadrant: CmQuadrant },
    Revenue { class: RevenueClass },
    Insufficient,
}

// ═══════════════════════════════════════════════════════════════════
// §4  COMMON OUTPUT TYPES
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Hold,
    RaisePrice,
    LowerPrice,
    Bundle,
    Remove,
    Reformulate,
    Monitor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardClip {
    MarginFloor,
    ChangeCap,
    CulturalRounding,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WilsonInterval {
    pub lo: f64,
    pub hi: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Triplet {
    pub lo: f64,
    pub mid: f64,
    pub hi: f64,
}

// ═══════════════════════════════════════════════════════════════════
// §5  KPI TYPES
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemKpi {
    pub key: ItemKey,
    /// Whether raw unit volume crosses the classification threshold.
    pub sufficient: bool,
    /// Was the item inactive in this window despite having sales?
    pub was_inactive: bool,
    pub current_price: i64,

    // Volume + revenue metrics — always meaningful.
    pub raw_units_sold: f64,
    pub weighted_units_sold: f64,
    pub weighted_revenue: f64,
    pub effective_price: f64,
    pub popularity_share: f64,
    pub popularity_ci: WilsonInterval,

    /// `Some` ⟺ cost was known for this item.
    /// Wherever this is `None`, margin/CM math is impossible by construction.
    pub cost_metrics: Option<CostMetrics>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostMetrics {
    pub cost_per_serving: i64,
    pub weighted_cost: f64,
    pub effective_cost: f64,
    pub contribution_margin: f64,
    pub cm_per_unit: f64,
    pub margin_pct: f64,
    pub food_cost_pct: f64,
    /// Did cost move >25% inside the window?
    pub cost_volatility_high: bool,
}

// ═══════════════════════════════════════════════════════════════════
// §6  PRICE SUGGESTION
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerComparison {
    pub same_category_count: usize,
    pub median_effective_price_peers: f64,
    /// Only set when this item is CM-tracked AND peers are CM-tracked too.
    pub median_margin_pct_peers: Option<f64>,
    pub median_cm_per_unit_peers: Option<f64>,
    pub your_position: PeerPosition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerPosition {
    Above,
    At,
    Below,
}

/// Two anchors are universal; the cost-plus anchor is only meaningful with
/// cost data, so it's optional.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceAnchors {
    pub cost_plus: Option<f64>,
    pub peer_median: f64,
    pub status_quo: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceSuggestion {
    pub key: ItemKey,
    pub item_name: String,
    pub classification: Classification,
    pub current_price: i64,

    // Volume + revenue (always present)
    pub units_sold_raw: f64,
    pub effective_price: f64,
    pub popularity_share: f64,

    // CM-only fields (None for revenue-only items)
    pub cm_per_unit: Option<f64>,
    pub margin_pct: Option<f64>,
    pub food_cost_pct: Option<f64>,

    pub anchors: PriceAnchors,
    pub suggested_price: Option<i64>,
    pub suggested_delta_abs: Option<i64>,
    pub suggested_delta_pct: Option<f64>,
    pub action: Action,
    pub confidence: Confidence,
    pub explanation: String,
    pub guard_clips: Vec<GuardClip>,
    pub peer_comparison: Option<PeerComparison>,
    pub price_changed_in_window: bool,
    /// Only computed for CM-tracked Plowhorses.
    pub cost_reduction_whatif_margin: Option<f64>,
    /// True when cost data is unavailable for this item. Mirrors
    /// `classification` mode, exposed flat for UI badge rendering.
    pub cost_missing: bool,
}

// ═══════════════════════════════════════════════════════════════════
// §7  BUNDLE SUGGESTION
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleItemPair {
    pub item_a: ItemKey,
    pub item_b: ItemKey,
    pub lift: f64,
    pub support: f64,
    pub confidence_ab: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleAssociation {
    pub pair_lifts: Vec<BundleItemPair>,
    pub composite_score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleForecast {
    pub expected_velocity: Triplet,
    pub inside_bundle_units_x: f64,
    pub halo_units_x: f64,
    pub total_units_uplift_x: f64,
    /// `None` when any component is cost-missing — CM math is impossible.
    pub incremental_cm: Option<Triplet>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleSuggestion {
    pub focus_item: ItemKey,
    pub bundle_items: Vec<ItemKey>,
    pub bundle_list_price: i64,
    pub bundle_suggested_price: i64,
    pub bundle_discount_pct: f64,
    /// All cost-derived fields are `None` when any component lacks cost data.
    pub bundle_cost: Option<i64>,
    pub bundle_cm: Option<i64>,
    pub bundle_margin_pct: Option<f64>,
    pub association: BundleAssociation,
    pub forecast: BundleForecast,
    pub guard_clips: Vec<GuardClip>,
    pub explanation: String,
    /// True ⟺ at least one component is cost-missing.
    pub missing_costs: bool,
}

// ═══════════════════════════════════════════════════════════════════
// §8  REMOVAL SCENARIO (CM-tracked items only)
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbsorbedBy {
    pub key: ItemKey,
    pub absorbed_units: f64,
    pub absorbed_cm: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComplementaryLoss {
    pub key: ItemKey,
    pub lost_units: f64,
    pub lost_cm: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovalRecommendation {
    Remove,
    KeepAndBundle,
    KeepAndReformulate,
    NoStrongSignal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemovalScenario {
    pub key: ItemKey,
    pub item_name: String,
    pub baseline_cm: f64,
    pub absorbed_by: Vec<AbsorbedBy>,
    pub complementary_losses: Vec<ComplementaryLoss>,
    pub net_cm_change: f64,
    pub net_cm_change_lo: f64,
    pub net_cm_change_hi: f64,
    pub recommendation: RemovalRecommendation,
    pub explanation: String,
}

// ═══════════════════════════════════════════════════════════════════
// §9  TOP-LEVEL REPORT
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct ModeSummary {
    pub items_total: usize,
    pub items_cm_tracked: usize,
    pub items_revenue_only: usize,
    pub items_insufficient: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvisorReport {
    pub generated_at: DateTime<Utc>,
    pub window_days: f64,
    pub mode_summary: ModeSummary,
    pub price_suggestions: Vec<PriceSuggestion>,
    pub bundle_suggestions: Vec<BundleSuggestion>,
    pub removal_scenarios: Vec<RemovalScenario>,
}

// ═══════════════════════════════════════════════════════════════════
// §10  ASSOCIATION INDEX
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct Association {
    pub support: f64,
    pub confidence_xy: f64,
    pub lift: f64,
}

pub type AssocKey = (ItemKey, ItemKey);
pub type AssociationIndex = HashMap<AssocKey, Association>;

// ═══════════════════════════════════════════════════════════════════
// §11  ENGINE ERROR
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug)]
pub enum EngineError {
    NoItems,
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoItems => write!(f, "no items to analyze"),
        }
    }
}

impl std::error::Error for EngineError {}

// ═══════════════════════════════════════════════════════════════════
// §12  PURE MATH HELPERS
// ═══════════════════════════════════════════════════════════════════

/// `weight = exp(-ln(2) * age_days / half_life_days)`
/// weight(0) = 1.0,  weight(half_life) = 0.5
fn recency_weight(age_days: f64, half_life_days: f64) -> f64 {
    (-(std::f64::consts::LN_2) * age_days / half_life_days.max(1e-9)).exp()
}

fn wilson_95_ci(p: f64, n: f64) -> WilsonInterval {
    let z = 1.96_f64;
    if n <= 0.0 {
        return WilsonInterval { lo: 0.0, hi: 1.0 };
    }
    let denom = 1.0 + z * z / n;
    let center = (p + z * z / (2.0 * n)) / denom;
    let spread = z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt() / denom;
    WilsonInterval {
        lo: (center - spread).max(0.0),
        hi: (center + spread).min(1.0),
    }
}

fn median(vals: &mut [f64]) -> f64 {
    if vals.is_empty() {
        return 0.0;
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = vals.len() / 2;
    if vals.len().is_multiple_of(2) {
        (vals[mid - 1] + vals[mid]) / 2.0
    } else {
        vals[mid]
    }
}

fn geometric_mean(vals: &[f64]) -> f64 {
    if vals.is_empty() {
        return 0.0;
    }
    let sum_ln: f64 = vals.iter().map(|v| v.max(1e-12).ln()).sum();
    (sum_ln / vals.len() as f64).exp()
}

fn snap_egyptian(price: f64) -> i64 {
    let step: f64 = if price < 2500.0 { 250.0 } else { 500.0 };
    (price / step).round() as i64 * step as i64
}

fn apply_rounding(price: f64, rule: &PriceRoundingRule) -> i64 {
    match rule {
        PriceRoundingRule::NearestUnit => price.round() as i64,
        PriceRoundingRule::EgyptianCafe => snap_egyptian(price),
    }
}

fn below_no_change_threshold(current: f64, suggested: f64) -> bool {
    if current <= 0.0 {
        return true;
    }
    (suggested - current).abs() / current < 0.015
}

// ═══════════════════════════════════════════════════════════════════
// §13  KPI COMPUTATION
// ═══════════════════════════════════════════════════════════════════

pub fn compute_item_kpis(
    snapshots: &[ItemSnapshot],
    sales: &[SaleEvent],
    now: DateTime<Utc>,
    config: &AnalysisConfig,
) -> Result<HashMap<ItemKey, ItemKpi>, EngineError> {
    if snapshots.is_empty() {
        return Err(EngineError::NoItems);
    }

    struct Acc {
        raw_units: f64,
        w_units: f64,
        w_revenue: f64,
        w_cost: f64,
        w_cost_samples: f64, // weight of sales where cost was known
        cost_min: Option<f64>,
        cost_max: Option<f64>,
    }
    let mut accs: HashMap<ItemKey, Acc> = HashMap::new();

    for sale in sales {
        let age_days = ((now - sale.sold_at).num_seconds() as f64 / 86_400.0).max(0.0);
        let w = recency_weight(age_days, config.recency_half_life_days);
        let qty = sale.quantity_sold as f64;

        let acc = accs.entry(sale.key.clone()).or_insert(Acc {
            raw_units: 0.0,
            w_units: 0.0,
            w_revenue: 0.0,
            w_cost: 0.0,
            w_cost_samples: 0.0,
            cost_min: None,
            cost_max: None,
        });
        acc.raw_units += qty;
        acc.w_units += w * qty;
        acc.w_revenue += w * qty * sale.unit_price_paid as f64;

        if let Some(uc) = sale.unit_cost_at_sale {
            let c = uc as f64;
            acc.w_cost += w * qty * c;
            acc.w_cost_samples += w * qty;
            acc.cost_min = Some(acc.cost_min.map_or(c, |m| m.min(c)));
            acc.cost_max = Some(acc.cost_max.map_or(c, |m| m.max(c)));
        }
    }

    // Build snapshot index.
    let snap_map: HashMap<&ItemKey, &ItemSnapshot> =
        snapshots.iter().map(|s| (&s.key, s)).collect();

    // Denominator for popularity_share: weighted units of active, non-bundle-only items.
    let total_w_units: f64 = accs
        .iter()
        .filter(|(k, _)| snap_map.get(k).is_some_and(|s| s.is_active && !s.bundle_only))
        .map(|(_, a)| a.w_units)
        .sum();

    let mut result: HashMap<ItemKey, ItemKpi> = HashMap::new();

    for snap in snapshots {
        let acc = accs.get(&snap.key);
        let raw_units = acc.map_or(0.0, |a| a.raw_units);
        let sufficient = raw_units >= config.min_units_for_classification;

        let (w_units, w_revenue) = acc.map_or((0.0, 0.0), |a| (a.w_units, a.w_revenue));

        let effective_price = if w_units > 0.0 {
            w_revenue / w_units
        } else {
            snap.current_price as f64
        };

        let popularity_share =
            if snap.is_active && !snap.bundle_only && total_w_units > 0.0 {
                w_units / total_w_units
            } else {
                0.0
            };
        let popularity_ci = wilson_95_ci(popularity_share, raw_units.max(1.0));

        // ── Cost metrics: present iff snapshot has cost AND we have cost
        //    samples for it (otherwise we'd be reporting margin built on the
        //    static snapshot cost, not on real sale-time cost).
        let cost_metrics = match (snap.cost_per_serving, acc) {
            (Some(cost_static), Some(a)) if a.w_cost_samples > 0.0 => {
                let effective_cost = a.w_cost / a.w_cost_samples.max(1e-9);
                let contribution_margin = w_revenue - a.w_cost;
                let cm_per_unit = if w_units > 0.0 {
                    contribution_margin / w_units
                } else {
                    0.0
                };
                let margin_pct = if effective_price > 0.0 {
                    (effective_price - effective_cost) / effective_price
                } else {
                    0.0
                };
                let food_cost_pct = if effective_price > 0.0 {
                    effective_cost / effective_price
                } else {
                    1.0
                };
                let cost_volatility_high = match (a.cost_min, a.cost_max) {
                    (Some(lo), Some(hi)) if lo > 0.0 => (hi - lo) / lo > 0.25,
                    _ => false,
                };
                Some(CostMetrics {
                    cost_per_serving: cost_static,
                    weighted_cost: a.w_cost,
                    effective_cost,
                    contribution_margin,
                    cm_per_unit,
                    margin_pct,
                    food_cost_pct,
                    cost_volatility_high,
                })
            }
            (Some(cost_static), _) => {
                // Static cost present but no sales (or no sale-time cost samples).
                // We still expose metrics derived from the static cost so the
                // item can be classified by quadrant on partial information.
                let cm_per_unit = (snap.current_price - cost_static) as f64;
                let margin_pct = if snap.current_price > 0 {
                    cm_per_unit / snap.current_price as f64
                } else {
                    0.0
                };
                let food_cost_pct = if snap.current_price > 0 {
                    cost_static as f64 / snap.current_price as f64
                } else {
                    1.0
                };
                Some(CostMetrics {
                    cost_per_serving: cost_static,
                    weighted_cost: 0.0,
                    effective_cost: cost_static as f64,
                    contribution_margin: 0.0,
                    cm_per_unit,
                    margin_pct,
                    food_cost_pct,
                    cost_volatility_high: false,
                })
            }
            _ => None,
        };

        result.insert(
            snap.key.clone(),
            ItemKpi {
                key: snap.key.clone(),
                sufficient,
                was_inactive: !snap.is_active && raw_units > 0.0,
                current_price: snap.current_price,
                raw_units_sold: raw_units,
                weighted_units_sold: w_units,
                weighted_revenue: w_revenue,
                effective_price,
                popularity_share,
                popularity_ci,
                cost_metrics,
            },
        );
    }

    Ok(result)
}

// ═══════════════════════════════════════════════════════════════════
// §14  CLASSIFICATION — two parallel populations
// ═══════════════════════════════════════════════════════════════════

pub fn classify_items(
    kpis: &HashMap<ItemKey, ItemKpi>,
    previous: Option<&HashMap<ItemKey, Classification>>,
) -> HashMap<ItemKey, Classification> {
    let mut out = HashMap::new();

    // ── 1. Eligible items split by mode ───────────────────────
    let (cm_eligible, rev_eligible): (Vec<&ItemKpi>, Vec<&ItemKpi>) = kpis
        .values()
        .filter(|k| k.sufficient)
        .partition(|k| k.cost_metrics.is_some());

    // ── 2. CM-tracked classification (Kasavana-Smith) ─────────
    if !cm_eligible.is_empty() {
        let n = cm_eligible.len() as f64;
        let pop_threshold = 0.70 / n;
        let total_w_units: f64 = cm_eligible.iter().map(|k| k.weighted_units_sold).sum();
        let cm_threshold = if total_w_units > 0.0 {
            cm_eligible
                .iter()
                .map(|k| {
                    k.cost_metrics.as_ref().unwrap().cm_per_unit * k.weighted_units_sold
                })
                .sum::<f64>()
                / total_w_units
        } else {
            0.0
        };

        for kpi in &cm_eligible {
            let cm = kpi.cost_metrics.as_ref().unwrap();
            let mut high_pop = kpi.popularity_share >= pop_threshold;
            let mut high_prof = cm.cm_per_unit >= cm_threshold;

            // Hysteresis: hold previous classification if within 5% of either threshold.
            if let Some(prev_map) = previous
                && let Some(Classification::Cm { quadrant: prev_q }) = prev_map.get(&kpi.key) {
                    let pop_dist =
                        (kpi.popularity_share - pop_threshold).abs() / pop_threshold.max(1e-9);
                    let prof_dist =
                        (cm.cm_per_unit - cm_threshold).abs() / cm_threshold.abs().max(1e-9);
                    if pop_dist < 0.05 {
                        high_pop = matches!(prev_q, CmQuadrant::Star | CmQuadrant::Plowhorse);
                    }
                    if prof_dist < 0.05 {
                        high_prof = matches!(prev_q, CmQuadrant::Star | CmQuadrant::Puzzle);
                    }
                }

            let q = match (high_pop, high_prof) {
                (true, true) => CmQuadrant::Star,
                (true, false) => CmQuadrant::Plowhorse,
                (false, true) => CmQuadrant::Puzzle,
                (false, false) => CmQuadrant::Dog,
            };
            out.insert(kpi.key.clone(), Classification::Cm { quadrant: q });
        }
    }

    // ── 3. Revenue-only classification ────────────────────────
    if !rev_eligible.is_empty() {
        let n = rev_eligible.len() as f64;
        let pop_threshold = 0.70 / n;
        let total_w_units: f64 = rev_eligible.iter().map(|k| k.weighted_units_sold).sum();
        // "Profit" axis proxy: weighted-average effective_price across revenue-only items.
        let price_threshold = if total_w_units > 0.0 {
            rev_eligible
                .iter()
                .map(|k| k.effective_price * k.weighted_units_sold)
                .sum::<f64>()
                / total_w_units
        } else {
            0.0
        };

        for kpi in &rev_eligible {
            let mut high_pop = kpi.popularity_share >= pop_threshold;
            let mut high_price = kpi.effective_price >= price_threshold;

            if let Some(prev_map) = previous
                && let Some(Classification::Revenue { class: prev_c }) = prev_map.get(&kpi.key) {
                    let pop_dist =
                        (kpi.popularity_share - pop_threshold).abs() / pop_threshold.max(1e-9);
                    let price_dist =
                        (kpi.effective_price - price_threshold).abs() / price_threshold.max(1e-9);
                    if pop_dist < 0.05 {
                        high_pop = matches!(prev_c, RevenueClass::Hero | RevenueClass::Steady);
                    }
                    if price_dist < 0.05 {
                        high_price = matches!(prev_c, RevenueClass::Hero | RevenueClass::Slow);
                    }
                }

            let c = match (high_pop, high_price) {
                (true, true) => RevenueClass::Hero,
                (true, false) => RevenueClass::Steady,
                (false, true) => RevenueClass::Slow,
                (false, false) => RevenueClass::Quiet,
            };
            out.insert(kpi.key.clone(), Classification::Revenue { class: c });
        }
    }

    // ── 4. Insufficient ───────────────────────────────────────
    for kpi in kpis.values() {
        if !kpi.sufficient {
            out.insert(kpi.key.clone(), Classification::Insufficient);
        }
    }

    out
}

// ═══════════════════════════════════════════════════════════════════
// §15  PEER COMPARISON & ANCHORS
// ═══════════════════════════════════════════════════════════════════

fn peers_in_category<'a>(
    focus: &ItemKey,
    all_kpis: &'a HashMap<ItemKey, ItemKpi>,
    snaps: &HashMap<ItemKey, &ItemSnapshot>,
) -> Vec<&'a ItemKpi> {
    let focus_cat = snaps.get(focus).and_then(|s| s.category_id);
    all_kpis
        .values()
        .filter(|k| {
            k.key != *focus
                && snaps.get(&k.key).and_then(|s| s.category_id) == focus_cat
                && k.sufficient
        })
        .collect()
}

fn compute_peer_anchor(
    focus: &ItemKpi,
    all_kpis: &HashMap<ItemKey, ItemKpi>,
    snaps: &HashMap<ItemKey, &ItemSnapshot>,
) -> f64 {
    let peers = peers_in_category(&focus.key, all_kpis, snaps);
    if peers.is_empty() {
        return focus.effective_price;
    }

    // If focus is CM-tracked, anchor against CM-tracked peers whose CM >=
    // category weighted-average. If focus is revenue-only, anchor against
    // all eligible peers (median price).
    if let Some(focus_cm) = &focus.cost_metrics {
        let cm_peers: Vec<&ItemKpi> = peers
            .iter()
            .filter(|k| k.cost_metrics.is_some())
            .copied()
            .collect();
        if !cm_peers.is_empty() {
            let total_w: f64 =
                cm_peers.iter().map(|k| k.weighted_units_sold).sum::<f64>()
                    + focus.weighted_units_sold;
            let cat_cm_avg: f64 = (cm_peers
                .iter()
                .map(|k| k.cost_metrics.as_ref().unwrap().cm_per_unit * k.weighted_units_sold)
                .sum::<f64>()
                + focus_cm.cm_per_unit * focus.weighted_units_sold)
                / total_w.max(1e-9);

            let mut well_perf: Vec<f64> = cm_peers
                .iter()
                .filter(|k| k.cost_metrics.as_ref().unwrap().cm_per_unit >= cat_cm_avg)
                .map(|k| k.effective_price)
                .collect();
            if !well_perf.is_empty() {
                return median(&mut well_perf);
            }
        }
    }

    // Fallback: median price of all peers.
    let mut prices: Vec<f64> = peers.iter().map(|k| k.effective_price).collect();
    median(&mut prices)
}

fn build_peer_comparison(
    focus: &ItemKpi,
    all_kpis: &HashMap<ItemKey, ItemKpi>,
    snaps: &HashMap<ItemKey, &ItemSnapshot>,
) -> Option<PeerComparison> {
    let peers = peers_in_category(&focus.key, all_kpis, snaps);
    if peers.is_empty() {
        return None;
    }

    let mut prices: Vec<f64> = peers.iter().map(|k| k.effective_price).collect();
    let med_price = median(&mut prices);

    let (med_margin, med_cm) = if focus.cost_metrics.is_some() {
        let mut margins: Vec<f64> = peers
            .iter()
            .filter_map(|k| k.cost_metrics.as_ref().map(|cm| cm.margin_pct))
            .collect();
        let mut cms: Vec<f64> = peers
            .iter()
            .filter_map(|k| k.cost_metrics.as_ref().map(|cm| cm.cm_per_unit))
            .collect();
        if margins.is_empty() {
            (None, None)
        } else {
            (Some(median(&mut margins)), Some(median(&mut cms)))
        }
    } else {
        (None, None)
    };

    let pos = if (focus.effective_price - med_price).abs() / med_price.max(1e-9) < 0.02 {
        PeerPosition::At
    } else if focus.effective_price > med_price {
        PeerPosition::Above
    } else {
        PeerPosition::Below
    };

    Some(PeerComparison {
        same_category_count: peers.len(),
        median_effective_price_peers: med_price,
        median_margin_pct_peers: med_margin,
        median_cm_per_unit_peers: med_cm,
        your_position: pos,
    })
}

fn compute_anchors(
    focus: &ItemKpi,
    all_kpis: &HashMap<ItemKey, ItemKpi>,
    snaps: &HashMap<ItemKey, &ItemSnapshot>,
    config: &AnalysisConfig,
) -> PriceAnchors {
    let cost_plus = focus
        .cost_metrics
        .as_ref()
        .map(|cm| cm.effective_cost / config.target_food_cost_pct.max(1e-9));
    let peer_anchor = compute_peer_anchor(focus, all_kpis, snaps);
    PriceAnchors {
        cost_plus,
        peer_median: peer_anchor,
        status_quo: focus.current_price as f64,
    }
}

// ═══════════════════════════════════════════════════════════════════
// §16  PRICE-SUGGESTION DISPATCH
// ═══════════════════════════════════════════════════════════════════

/// Generate the raw candidate price for an item, dispatched on its
/// classification AND its cost_metrics presence. The match arm structure
/// enforces the invariant that `Cm` ⟺ `Some(cost)` and `Revenue` ⟺ `None`.
fn raw_candidate(
    kpi: &ItemKpi,
    classification: Classification,
    anchors: &PriceAnchors,
    config: &AnalysisConfig,
    all_kpis: &HashMap<ItemKey, ItemKpi>,
    snaps: &HashMap<ItemKey, &ItemSnapshot>,
) -> (f64, Action, String) {
    match (classification, &kpi.cost_metrics) {
        (Classification::Cm { quadrant }, Some(cm)) => {
            cm_raw_candidate(kpi, cm, quadrant, anchors, all_kpis, snaps)
        }
        (Classification::Revenue { class }, None) => {
            revenue_raw_candidate(kpi, class, anchors, config)
        }
        (Classification::Insufficient, _) => (
            kpi.current_price as f64,
            Action::Monitor,
            "Insufficient sales data for a price recommendation.".into(),
        ),
        // The classifier never produces these combinations; treat defensively.
        (Classification::Cm { .. }, None) | (Classification::Revenue { .. }, Some(_)) => (
            kpi.current_price as f64,
            Action::Monitor,
            "Classification/cost-metrics invariant violated; skipping.".into(),
        ),
    }
}

// ── §16.1  CM-mode candidates (full Kasavana-Smith logic) ────

fn cm_raw_candidate(
    kpi: &ItemKpi,
    cm: &CostMetrics,
    quadrant: CmQuadrant,
    anchors: &PriceAnchors,
    all_kpis: &HashMap<ItemKey, ItemKpi>,
    snaps: &HashMap<ItemKey, &ItemSnapshot>,
) -> (f64, Action, String) {
    let cur = kpi.current_price as f64;
    match quadrant {
        CmQuadrant::Star => {
            if cur < anchors.peer_median * 0.95 {
                // Check Star margin vs same-category Star margins.
                let focus_cat = snaps.get(&kpi.key).and_then(|s| s.category_id);
                let mut star_margins: Vec<f64> = all_kpis
                    .values()
                    .filter(|k| {
                        k.key != kpi.key
                            && snaps.get(&k.key).and_then(|s| s.category_id) == focus_cat
                    })
                    .filter_map(|k| k.cost_metrics.as_ref().map(|c| c.margin_pct))
                    .collect();
                let med_star_margin = median(&mut star_margins);
                if cm.margin_pct < med_star_margin {
                    let target = anchors.peer_median.min(cur * 1.08);
                    return (
                        target,
                        Action::RaisePrice,
                        format!(
                            "Star item priced below peer median ({:.0} vs {:.0}) with \
                             below-median margin. Small increase suggested toward peer pricing.",
                            cur, anchors.peer_median
                        ),
                    );
                }
            }
            (
                cur,
                Action::Hold,
                "Star item: popular and profitable. Hold current price.".into(),
            )
        }

        CmQuadrant::Plowhorse => {
            // Raise to lift margin ~4 pp, bounded to [+3%, +10%].
            let target_margin = cm.margin_pct + 0.04;
            let price_for_target = cm.effective_cost / (1.0 - target_margin).max(1e-9);
            let target = price_for_target.max(cur * 1.03).min(cur * 1.10);
            (
                target,
                Action::RaisePrice,
                format!(
                    "Plowhorse: popular but margin ({:.1}%) is below average. \
                     Moderate price increase would lift margin ~4 pp.",
                    cm.margin_pct * 100.0
                ),
            )
        }

        CmQuadrant::Puzzle => {
            if cur > anchors.peer_median * 1.15 {
                let target = cur * 0.975;
                return (
                    target,
                    Action::LowerPrice,
                    format!(
                        "Puzzle: profitable but unpopular. Priced {:.0}% above peer median \
                         — small reduction may improve trial.",
                        (cur / anchors.peer_median - 1.0) * 100.0
                    ),
                );
            }
            (
                cur,
                Action::Bundle,
                "Puzzle: profitable but unpopular. Bundling recommended over price change.".into(),
            )
        }

        CmQuadrant::Dog => {
            if cm.food_cost_pct > 0.45 {
                return (
                    cur,
                    Action::Reformulate,
                    format!(
                        "Dog: unpopular and unprofitable. Food cost is {:.1}% — \
                         recipe reformulation may restore viability.",
                        cm.food_cost_pct * 100.0
                    ),
                );
            }
            (
                cur,
                Action::Remove,
                "Dog: unpopular and unprofitable. Consider removing from menu.".into(),
            )
        }
    }
}

// ── §16.2  Revenue-mode candidates (no margin reasoning) ─────

fn revenue_raw_candidate(
    kpi: &ItemKpi,
    class: RevenueClass,
    anchors: &PriceAnchors,
    config: &AnalysisConfig,
) -> (f64, Action, String) {
    let cur = kpi.current_price as f64;
    let cap_pct = config.revenue_mode_max_raise_pct;
    match class {
        RevenueClass::Hero => {
            if cur < anchors.peer_median * 0.95 {
                let target = anchors.peer_median.min(cur * (1.0 + cap_pct));
                return (
                    target,
                    Action::RaisePrice,
                    format!(
                        "Hero: popular and priced at the top of the menu, but {:.0}% \
                         below peer median. Small increase suggested. Cost data is \
                         missing — add ingredient costs to refine this.",
                        (1.0 - cur / anchors.peer_median.max(1e-9)) * 100.0
                    ),
                );
            }
            (
                cur,
                Action::Hold,
                "Hero: popular and high-priced. Hold. Cost data missing — \
                 margin analysis unavailable until ingredient costs are added."
                    .into(),
            )
        }

        RevenueClass::Steady => {
            // High popularity supports a small raise without margin info.
            let target = cur * (1.0 + cap_pct);
            (
                target,
                Action::RaisePrice,
                format!(
                    "Steady: high popularity but low average price. Small {:.0}% raise \
                     suggested. Add ingredient costs to enable margin-aware pricing.",
                    cap_pct * 100.0
                ),
            )
        }

        RevenueClass::Slow => (
            cur,
            Action::Bundle,
            "Slow: priced high but unpopular. Bundle to drive trial. \
             Removal not assessed — cost data missing."
                .into(),
        ),

        RevenueClass::Quiet => (
            cur,
            Action::Monitor,
            "Quiet: low popularity and low price. Insufficient signal for a \
             price recommendation without cost data."
                .into(),
        ),
    }
}

// ═══════════════════════════════════════════════════════════════════
// §17  SAFETY GUARDS (cost-aware vs cost-naïve)
// ═══════════════════════════════════════════════════════════════════

fn apply_guards(
    mut candidate: f64,
    current: f64,
    cost_metrics: Option<&CostMetrics>,
    config: &AnalysisConfig,
) -> (f64, Vec<GuardClip>) {
    let mut clips = Vec::new();

    // Guard 1: margin floor — CM-tracked items only.
    if let Some(cm) = cost_metrics {
        let min_for_margin =
            cm.effective_cost / (1.0 - config.min_gross_margin_pct).max(1e-9);
        if candidate < min_for_margin {
            candidate = min_for_margin;
            clips.push(GuardClip::MarginFloor);
        }
    }

    // Guard 2: change cap — applies to everyone.
    let max_change_pct = match cost_metrics {
        Some(_) => config.max_price_change_pct_per_cycle,
        None => config.revenue_mode_max_raise_pct.max(0.02),
    };
    let max_change = current * max_change_pct;
    if (candidate - current).abs() > max_change {
        candidate = if candidate > current {
            current + max_change
        } else {
            current - max_change
        };
        clips.push(GuardClip::ChangeCap);
    }

    (candidate, clips)
}

// ═══════════════════════════════════════════════════════════════════
// §18  CONFIDENCE
// ═══════════════════════════════════════════════════════════════════

fn assess_confidence(kpi: &ItemKpi, classification: Classification, config: &AnalysisConfig) -> Confidence {
    if matches!(classification, Classification::Insufficient) {
        return Confidence::Low;
    }
    // Two factors: raw sample size + Wilson CI width.
    let n_factor = if kpi.raw_units_sold >= 3.0 * config.min_units_for_classification {
        Confidence::High
    } else if kpi.raw_units_sold >= config.min_units_for_classification {
        Confidence::Medium
    } else {
        Confidence::Low
    };
    let ci_width = kpi.popularity_ci.hi - kpi.popularity_ci.lo;
    let ci_factor = if ci_width < 0.05 {
        Confidence::High
    } else if ci_width < 0.12 {
        Confidence::Medium
    } else {
        Confidence::Low
    };
    // Revenue-mode caps confidence at Medium (we're missing a major data dimension).
    let mode_cap = if matches!(classification, Classification::Revenue { .. }) {
        Confidence::Medium
    } else {
        Confidence::High
    };
    [n_factor, ci_factor, mode_cap].into_iter().min().unwrap()
}

impl Ord for Confidence {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let rank = |c: &Confidence| match c {
            Confidence::Low => 0,
            Confidence::Medium => 1,
            Confidence::High => 2,
        };
        rank(self).cmp(&rank(other))
    }
}
impl PartialOrd for Confidence {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

// ═══════════════════════════════════════════════════════════════════
// §19  PRICE SUGGESTIONS — full pipeline
// ═══════════════════════════════════════════════════════════════════

pub fn suggest_prices(
    snapshots: &[ItemSnapshot],
    kpis: &HashMap<ItemKey, ItemKpi>,
    classifications: &HashMap<ItemKey, Classification>,
    config: &AnalysisConfig,
    price_changed_keys: &HashSet<ItemKey>,
) -> Vec<PriceSuggestion> {
    let snap_map: HashMap<ItemKey, &ItemSnapshot> =
        snapshots.iter().map(|s| (s.key.clone(), s)).collect();

    let mut out = Vec::with_capacity(kpis.len());

    for kpi in kpis.values() {
        let snap = snap_map.get(&kpi.key);
        let item_name = snap.map_or(String::new(), |s| s.name.clone());
        let classification = classifications
            .get(&kpi.key)
            .copied()
            .unwrap_or(Classification::Insufficient);
        let cost_missing = kpi.cost_metrics.is_none();

        // Inactive items: monitor, no suggestion.
        if kpi.was_inactive {
            out.push(PriceSuggestion {
                key: kpi.key.clone(),
                item_name,
                classification,
                current_price: kpi.current_price,
                units_sold_raw: kpi.raw_units_sold,
                effective_price: kpi.effective_price,
                popularity_share: kpi.popularity_share,
                cm_per_unit: kpi.cost_metrics.as_ref().map(|c| c.cm_per_unit),
                margin_pct: kpi.cost_metrics.as_ref().map(|c| c.margin_pct),
                food_cost_pct: kpi.cost_metrics.as_ref().map(|c| c.food_cost_pct),
                anchors: PriceAnchors {
                    cost_plus: None,
                    peer_median: kpi.effective_price,
                    status_quo: kpi.current_price as f64,
                },
                suggested_price: None,
                suggested_delta_abs: None,
                suggested_delta_pct: None,
                action: Action::Monitor,
                confidence: Confidence::Low,
                explanation: "Item is currently inactive but had sales in the window.".into(),
                guard_clips: vec![],
                peer_comparison: None,
                price_changed_in_window: price_changed_keys.contains(&kpi.key),
                cost_reduction_whatif_margin: None,
                cost_missing,
            });
            continue;
        }

        let anchors = compute_anchors(kpi, kpis, &snap_map, config);
        let peer_cmp = build_peer_comparison(kpi, kpis, &snap_map);

        let (raw, mut action, explanation) =
            raw_candidate(kpi, classification, &anchors, config, kpis, &snap_map);

        let (guarded, mut clips) =
            apply_guards(raw, kpi.current_price as f64, kpi.cost_metrics.as_ref(), config);
        let rounded = apply_rounding(guarded, &config.price_rounding_rule);
        if (rounded as f64 - guarded).abs() > 0.5 {
            clips.push(GuardClip::CulturalRounding);
        }

        let (suggested_price, delta_abs, delta_pct) = if matches!(action, Action::Hold | Action::Monitor | Action::Bundle | Action::Remove | Action::Reformulate)
            || below_no_change_threshold(kpi.current_price as f64, rounded as f64)
            || matches!(classification, Classification::Insufficient)
        {
            // Demote to Hold if the original action implied a price move but
            // the post-guard delta is below the no-change threshold.
            if matches!(action, Action::RaisePrice | Action::LowerPrice)
                && below_no_change_threshold(kpi.current_price as f64, rounded as f64)
            {
                action = Action::Hold;
            }
            (None, None, None)
        } else {
            let abs_d = rounded - kpi.current_price;
            let pct_d = (rounded as f64 - kpi.current_price as f64)
                / (kpi.current_price as f64).max(1.0);
            (Some(rounded), Some(abs_d), Some(pct_d))
        };

        // What-if cost-reduction for CM-tracked Plowhorses only.
        let cost_reduction_whatif_margin = match (classification, &kpi.cost_metrics) {
            (Classification::Cm { quadrant: CmQuadrant::Plowhorse }, Some(cm)) => {
                let reduced_cost = cm.effective_cost * 0.90;
                let whatif = (kpi.current_price as f64 - reduced_cost)
                    / (kpi.current_price as f64).max(1e-9);
                Some(whatif)
            }
            _ => None,
        };

        let confidence = assess_confidence(kpi, classification, config);

        out.push(PriceSuggestion {
            key: kpi.key.clone(),
            item_name,
            classification,
            current_price: kpi.current_price,
            units_sold_raw: kpi.raw_units_sold,
            effective_price: kpi.effective_price,
            popularity_share: kpi.popularity_share,
            cm_per_unit: kpi.cost_metrics.as_ref().map(|c| c.cm_per_unit),
            margin_pct: kpi.cost_metrics.as_ref().map(|c| c.margin_pct),
            food_cost_pct: kpi.cost_metrics.as_ref().map(|c| c.food_cost_pct),
            anchors,
            suggested_price,
            suggested_delta_abs: delta_abs,
            suggested_delta_pct: delta_pct,
            action,
            confidence,
            explanation,
            guard_clips: clips,
            peer_comparison: peer_cmp,
            price_changed_in_window: price_changed_keys.contains(&kpi.key),
            cost_reduction_whatif_margin,
            cost_missing,
        });
    }

    out
}

// ═══════════════════════════════════════════════════════════════════
// §20  ASSOCIATION MINING
// ═══════════════════════════════════════════════════════════════════

pub fn compute_associations(baskets: &[Basket]) -> AssociationIndex {
    let total = baskets.len();
    if total == 0 {
        return HashMap::new();
    }

    let mut item_counts: HashMap<ItemKey, usize> = HashMap::new();
    let mut pair_counts: HashMap<AssocKey, usize> = HashMap::new();

    for basket in baskets {
        let items: HashSet<&ItemKey> = basket.iter().collect();
        for item in &items {
            *item_counts.entry((*item).clone()).or_insert(0) += 1;
        }
        let mut items_sorted: Vec<&ItemKey> = items.into_iter().collect();
        items_sorted.sort();
        for i in 0..items_sorted.len() {
            for j in (i + 1)..items_sorted.len() {
                let key = (items_sorted[i].clone(), items_sorted[j].clone());
                *pair_counts.entry(key).or_insert(0) += 1;
            }
        }
    }

    let t = total as f64;
    let mut index = HashMap::new();
    for ((a, b), &count) in &pair_counts {
        let sup_a = item_counts.get(a).copied().unwrap_or(0) as f64 / t;
        let sup_b = item_counts.get(b).copied().unwrap_or(0) as f64 / t;
        let sup_ab = count as f64 / t;
        let conf_ab = if sup_a > 0.0 { sup_ab / sup_a } else { 0.0 };
        let lift = if sup_a > 0.0 && sup_b > 0.0 {
            sup_ab / (sup_a * sup_b)
        } else {
            0.0
        };
        index.insert(
            (a.clone(), b.clone()),
            Association {
                support: sup_ab,
                confidence_xy: conf_ab,
                lift,
            },
        );
    }
    index
}

fn get_assoc<'a>(
    idx: &'a AssociationIndex,
    a: &ItemKey,
    b: &ItemKey,
) -> Option<&'a Association> {
    let key = if a <= b {
        (a.clone(), b.clone())
    } else {
        (b.clone(), a.clone())
    };
    idx.get(&key)
}

fn partner_score(lift: f64, support: f64, value_per_unit: f64) -> f64 {
    (lift - 1.0) * support.sqrt() * value_per_unit
}

/// Two SKUs are size-siblings ⟺ they share menu_item_id but differ in size_label.
fn are_size_siblings(a: &ItemKey, b: &ItemKey) -> bool {
    a.menu_item_id == b.menu_item_id && a.size_label != b.size_label
}

// ═══════════════════════════════════════════════════════════════════
// §21  BUNDLE PRICING
// ═══════════════════════════════════════════════════════════════════

struct BundlePricing {
    price: i64,
    discount_pct: f64,
}

fn price_bundle(
    bundle_cost: Option<i64>,
    bundle_list_price: i64,
    config: &AnalysisConfig,
) -> Option<BundlePricing> {
    let (lo, hi) = config.bundle_discount_pct_range;
    let list = bundle_list_price as f64;
    if list <= 0.0 {
        return None;
    }
    let bundle_margin_floor = config.min_gross_margin_pct - 0.05;

    // Strategy A: smallest qualifying discount-anchored price.
    let mut best_a: Option<f64> = None;
    let mut d = lo;
    while d <= hi + 1e-9 {
        let candidate = list * (1.0 - d);
        let margin_ok = match bundle_cost {
            Some(c) => {
                let m = (candidate - c as f64) / candidate.max(1e-9);
                m >= bundle_margin_floor
            }
            None => true, // no margin floor when cost is unknown
        };
        if margin_ok && candidate <= list * 0.95 {
            // Smallest discount = largest candidate; keep the max.
            if best_a.is_none_or(|prev: f64| candidate > prev) {
                best_a = Some(candidate);
            }
        }
        d += 0.05;
    }

    // Strategy B (cost-anchored): only when cost is known.
    let price_b = bundle_cost.map(|c| c as f64 / config.target_food_cost_pct.max(1e-9));

    let base = match (best_a, price_b) {
        (Some(a), Some(b)) => a.max(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };

    let rounded = apply_rounding(base, &config.price_rounding_rule);
    let rounded_f = rounded as f64;
    if rounded_f > list * 0.95 {
        return None;
    }
    let margin_guard_satisfied = match bundle_cost {
        Some(c) => {
            let m = (rounded_f - c as f64) / rounded_f.max(1e-9);
            m >= bundle_margin_floor
        }
        None => true,
    };
    if !margin_guard_satisfied {
        return None;
    }
    Some(BundlePricing {
        price: rounded,
        discount_pct: 1.0 - rounded_f / list,
    })
}

// ═══════════════════════════════════════════════════════════════════
// §22  BUNDLE FORECAST
// ═══════════════════════════════════════════════════════════════════

fn expected_velocity(
    focus_kpi: &ItemKpi,
    partner_score_norm: f64,
    config: &AnalysisConfig,
) -> Triplet {
    let base = focus_kpi.weighted_units_sold / config.analysis_window_days.max(1.0);
    let mid = base * partner_score_norm * config.promotion_lift_prior;
    Triplet { lo: mid * 0.50, mid, hi: mid * 1.50 }
}

fn estimate_halo(velocity: &Triplet, config: &AnalysisConfig) -> (f64, f64) {
    let unique_trier_rate = 0.85_f64;
    let inside = velocity.mid * config.analysis_window_days;
    let halo = inside * unique_trier_rate * config.halo_repeat_rate;
    (inside, halo)
}

fn estimate_incremental_cm(
    velocity: &Triplet,
    bundle_cm: i64,
    bundle_list_price: i64,
    bundle_price: i64,
    confidence_product: f64,
    config: &AnalysisConfig,
) -> Triplet {
    let discount_given = (bundle_list_price - bundle_price) as f64;
    let cm_f = bundle_cm as f64;
    let window = config.analysis_window_days;
    let calc = |v: f64| -> f64 {
        let p_anyway = confidence_product.clamp(0.0, 1.0);
        let incremental_v = v * (1.0 - p_anyway);
        let cannibalized = v * p_anyway;
        incremental_v * window * cm_f - cannibalized * window * discount_given
    };
    Triplet { lo: calc(velocity.lo), mid: calc(velocity.mid), hi: calc(velocity.hi) }
}

// ═══════════════════════════════════════════════════════════════════
// §23  BUNDLE SUGGESTIONS
// ═══════════════════════════════════════════════════════════════════

pub fn suggest_bundles(
    snapshots: &[ItemSnapshot],
    kpis: &HashMap<ItemKey, ItemKpi>,
    classifications: &HashMap<ItemKey, Classification>,
    assoc: &AssociationIndex,
    basket_count: usize,
    config: &AnalysisConfig,
) -> Vec<BundleSuggestion> {
    let snap_map: HashMap<ItemKey, &ItemSnapshot> =
        snapshots.iter().map(|s| (s.key.clone(), s)).collect();
    let t = (basket_count as f64).max(1.0);
    let min_support_ab = config.min_cooccurrences_for_bundle / t;

    // Focus candidates: weak performers in either mode.
    let is_focus = |c: Classification| {
        matches!(
            c,
            Classification::Cm { quadrant: CmQuadrant::Puzzle }
                | Classification::Cm { quadrant: CmQuadrant::Dog }
                | Classification::Revenue { class: RevenueClass::Slow }
                | Classification::Revenue { class: RevenueClass::Quiet }
        )
    };

    let mut all_out = Vec::new();

    for focus in kpis.values() {
        if !focus.sufficient { continue; }
        let cls = match classifications.get(&focus.key) { Some(c) => *c, None => continue };
        if !is_focus(cls) { continue; }
        let focus_snap = match snap_map.get(&focus.key) { Some(s) => *s, None => continue };

        // Rank partners.
        let mut partners: Vec<(ItemKey, f64, &Association)> = kpis
            .keys()
            .filter(|k| **k != focus.key && !are_size_siblings(&focus.key, k))
            .filter_map(|k| {
                let a = get_assoc(assoc, &focus.key, k)?;
                if a.lift < config.min_lift_for_bundle || a.support < min_support_ab {
                    return None;
                }
                let partner = kpis.get(k)?;
                // For partner ranking, prefer CM per unit if available; else use revenue per unit.
                let value_per_unit = partner
                    .cost_metrics
                    .as_ref()
                    .map(|c| c.cm_per_unit)
                    .unwrap_or(partner.effective_price);
                let score = partner_score(a.lift, a.support, value_per_unit);
                Some((k.clone(), score, a))
            })
            .collect();
        partners.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        partners.truncate(config.bundle_top_k_partners);
        if partners.is_empty() { continue; }

        let max_score = partners.first().map(|(_, s, _)| *s).unwrap_or(1.0).max(1e-9);

        let mut focus_candidates: Vec<BundleSuggestion> = Vec::new();

        // ── Size-2 bundles ────────────────────────────────────
        for (p1_key, p1_score, p1_assoc) in &partners {
            let p1_snap = match snap_map.get(p1_key) { Some(s) => *s, None => continue };
            let p1_kpi = match kpis.get(p1_key) { Some(k) => k, None => continue };

            let bundle_list = focus_snap.current_price + p1_snap.current_price;
            let component_costs = [focus_snap.cost_per_serving, p1_snap.cost_per_serving];
            let bundle_cost: Option<i64> =
                if component_costs.iter().all(|c| c.is_some()) {
                    Some(component_costs.iter().map(|c| c.unwrap()).sum())
                } else {
                    None
                };

            let pricing = match price_bundle(bundle_cost, bundle_list, config) {
                Some(p) => p,
                None => continue,
            };

            let bundle_cm = bundle_cost.map(|c| pricing.price - c);
            let bundle_margin = bundle_cm
                .map(|cm| cm as f64 / pricing.price.max(1) as f64);

            let velocity = expected_velocity(focus, p1_score / max_score, config);
            let (inside, halo) = estimate_halo(&velocity, config);
            let incremental_cm = bundle_cm.map(|cm| estimate_incremental_cm(
                    &velocity,
                    cm,
                    bundle_list,
                    pricing.price,
                    p1_assoc.confidence_xy,
                    config,
                ));

            let mut bundle_items = vec![focus.key.clone(), p1_key.clone()];
            bundle_items.sort();

            let missing_costs = bundle_cost.is_none();
            let cm_note = if missing_costs {
                " Incremental CM unavailable — cost data missing for at least one component."
            } else {
                ""
            };
            let explanation = format!(
                "Bundle '{} + {}': lift={:.2}, support={:.3}, discount={:.0}%. \
                 Expected ~{:.1} bundles/day; halo ~{:.0} units.{}",
                focus_snap.name,
                p1_snap.name,
                p1_assoc.lift,
                p1_assoc.support,
                pricing.discount_pct * 100.0,
                velocity.mid,
                halo,
                cm_note
            );

            focus_candidates.push(BundleSuggestion {
                focus_item: focus.key.clone(),
                bundle_items,
                bundle_list_price: bundle_list,
                bundle_suggested_price: pricing.price,
                bundle_discount_pct: pricing.discount_pct,
                bundle_cost,
                bundle_cm,
                bundle_margin_pct: bundle_margin,
                association: BundleAssociation {
                    pair_lifts: vec![BundleItemPair {
                        item_a: focus.key.clone(),
                        item_b: p1_key.clone(),
                        lift: p1_assoc.lift,
                        support: p1_assoc.support,
                        confidence_ab: p1_assoc.confidence_xy,
                    }],
                    composite_score: *p1_score,
                },
                forecast: BundleForecast {
                    expected_velocity: velocity,
                    inside_bundle_units_x: inside,
                    halo_units_x: halo,
                    total_units_uplift_x: inside + halo,
                    incremental_cm,
                },
                guard_clips: vec![GuardClip::CulturalRounding],
                explanation,
                missing_costs,
            });

            // ── Size-3 bundles ────────────────────────────────
            if config.bundle_max_size >= 3 {
                // Pick the best p2 distinct from p1 and not a size-sibling.
                let p2_opt = partners
                    .iter()
                    .find(|(p2k, _, _)| {
                        p2k != p1_key
                            && !are_size_siblings(p1_key, p2k)
                            && !are_size_siblings(&focus.key, p2k)
                    });
                if let Some((p2_key, p2_score, p2_assoc)) = p2_opt {
                    let p2_snap = match snap_map.get(p2_key) {
                        Some(s) => *s,
                        None => continue,
                    };
                    let bundle_list3 = bundle_list + p2_snap.current_price;
                    let costs3 = [
                        focus_snap.cost_per_serving,
                        p1_snap.cost_per_serving,
                        p2_snap.cost_per_serving,
                    ];
                    let bundle_cost3: Option<i64> = if costs3.iter().all(|c| c.is_some()) {
                        Some(costs3.iter().map(|c| c.unwrap()).sum())
                    } else {
                        None
                    };
                    let pricing3 = match price_bundle(bundle_cost3, bundle_list3, config) {
                        Some(p) => p,
                        None => continue,
                    };

                    let bundle_cm3 = bundle_cost3.map(|c| pricing3.price - c);
                    let bundle_margin3 =
                        bundle_cm3.map(|cm| cm as f64 / pricing3.price.max(1) as f64);

                    let avg_score = (*p1_score + *p2_score) / 2.0;
                    let velocity3 = expected_velocity(focus, avg_score / max_score, config);
                    let (inside3, halo3) = estimate_halo(&velocity3, config);
                    let incremental_cm3 = bundle_cm3.map(|cm| estimate_incremental_cm(
                            &velocity3,
                            cm,
                            bundle_list3,
                            pricing3.price,
                            p1_assoc.confidence_xy * p2_assoc.confidence_xy,
                            config,
                        ));

                    let mut bundle_items3 =
                        vec![focus.key.clone(), p1_key.clone(), p2_key.clone()];
                    bundle_items3.sort();

                    let missing_costs3 = bundle_cost3.is_none();
                    let assoc_strength = geometric_mean(&[*p1_score, *p2_score]);
                    let explanation3 = format!(
                        "Bundle '{} + {} + {}': assoc strength={:.2}, discount={:.0}%. \
                         Expected ~{:.1} bundles/day.{}",
                        focus_snap.name,
                        p1_snap.name,
                        p2_snap.name,
                        assoc_strength,
                        pricing3.discount_pct * 100.0,
                        velocity3.mid,
                        if missing_costs3 {
                            " Incremental CM unavailable — cost data missing."
                        } else {
                            ""
                        },
                    );
                    let _ = p1_kpi; // suppress unused warning

                    focus_candidates.push(BundleSuggestion {
                        focus_item: focus.key.clone(),
                        bundle_items: bundle_items3,
                        bundle_list_price: bundle_list3,
                        bundle_suggested_price: pricing3.price,
                        bundle_discount_pct: pricing3.discount_pct,
                        bundle_cost: bundle_cost3,
                        bundle_cm: bundle_cm3,
                        bundle_margin_pct: bundle_margin3,
                        association: BundleAssociation {
                            pair_lifts: vec![
                                BundleItemPair {
                                    item_a: focus.key.clone(),
                                    item_b: p1_key.clone(),
                                    lift: p1_assoc.lift,
                                    support: p1_assoc.support,
                                    confidence_ab: p1_assoc.confidence_xy,
                                },
                                BundleItemPair {
                                    item_a: focus.key.clone(),
                                    item_b: p2_key.clone(),
                                    lift: p2_assoc.lift,
                                    support: p2_assoc.support,
                                    confidence_ab: p2_assoc.confidence_xy,
                                },
                            ],
                            composite_score: assoc_strength,
                        },
                        forecast: BundleForecast {
                            expected_velocity: velocity3,
                            inside_bundle_units_x: inside3,
                            halo_units_x: halo3,
                            total_units_uplift_x: inside3 + halo3,
                            incremental_cm: incremental_cm3,
                        },
                        guard_clips: vec![GuardClip::CulturalRounding],
                        explanation: explanation3,
                        missing_costs: missing_costs3,
                    });
                }
            }
        }

        // Rank focus candidates: prefer those with incremental_cm.mid,
        // fall back to forecasted total_units_uplift_x when CM unknown.
        focus_candidates.sort_by(|a, b| {
            let av = a.forecast.incremental_cm.map(|t| t.mid).unwrap_or(
                a.forecast.total_units_uplift_x * a.bundle_suggested_price as f64 * 0.0001,
            );
            let bv = b.forecast.incremental_cm.map(|t| t.mid).unwrap_or(
                b.forecast.total_units_uplift_x * b.bundle_suggested_price as f64 * 0.0001,
            );
            bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal)
        });
        focus_candidates.truncate(config.bundle_top_n_per_focus);
        all_out.extend(focus_candidates);
    }

    all_out
}

// ═══════════════════════════════════════════════════════════════════
// §24  REMOVAL SCENARIOS  (CM-tracked items only)
// ═══════════════════════════════════════════════════════════════════

pub fn simulate_removal(
    target: &ItemKey,
    kpis: &HashMap<ItemKey, ItemKpi>,
    assoc: &AssociationIndex,
    snaps: &HashMap<ItemKey, &ItemSnapshot>,
) -> Option<RemovalScenario> {
    let target_kpi = kpis.get(target)?;
    let target_cm = target_kpi.cost_metrics.as_ref()?;
    let item_name = snaps.get(target).map_or(String::new(), |s| s.name.clone());

    let baseline_cm = target_cm.contribution_margin;
    let total_units: f64 = kpis.values().map(|k| k.raw_units_sold).sum::<f64>().max(1.0);

    // Substitutes: items with lift < 1 against the target.
    let substitutes: Vec<(&ItemKey, f64)> = kpis
        .keys()
        .filter(|k| **k != *target)
        .filter_map(|k| {
            let a = get_assoc(assoc, target, k)?;
            if a.lift < 1.0 {
                let support_k = kpis.get(k)?.raw_units_sold / total_units;
                Some((k, support_k))
            } else {
                None
            }
        })
        .collect();

    let total_sub_weight: f64 = substitutes.iter().map(|(_, w)| w).sum::<f64>();
    let absorb_rate = if total_sub_weight > 0.0 { 0.60_f64 } else { 0.0 };

    let mut absorbed_by = Vec::new();
    let mut total_recovered = 0.0;
    for (sub_key, weight) in &substitutes {
        let s = if total_sub_weight > 0.0 {
            weight / total_sub_weight * absorb_rate
        } else {
            0.0
        };
        let abs_units = target_kpi.weighted_units_sold * s;
        let sub_kpi = match kpis.get(*sub_key) { Some(k) => k, None => continue };
        // If substitute has no CM, treat its absorbed CM as zero (conservative).
        let sub_cm_pu = sub_kpi
            .cost_metrics
            .as_ref()
            .map(|cm| cm.cm_per_unit)
            .unwrap_or(0.0);
        let abs_cm = abs_units * sub_cm_pu;
        total_recovered += abs_cm;
        absorbed_by.push(AbsorbedBy {
            key: (*sub_key).clone(),
            absorbed_units: abs_units,
            absorbed_cm: abs_cm,
        });
    }

    let absorbed_total: f64 = absorbed_by.iter().map(|a| a.absorbed_units).sum();

    // Complementary losses: partners with lift > 1.2 lose some sales.
    let mut complementary_losses = Vec::new();
    let mut total_comp_loss = 0.0;
    for (pair_key, pair_assoc) in assoc {
        let other_key = if pair_key.0 == *target {
            &pair_key.1
        } else if pair_key.1 == *target {
            &pair_key.0
        } else {
            continue;
        };
        if pair_assoc.lift <= 1.2 { continue; }
        let other_kpi = match kpis.get(other_key) { Some(k) => k, None => continue };
        let support_share = if other_kpi.raw_units_sold > 0.0 {
            target_kpi.raw_units_sold / other_kpi.raw_units_sold.max(1.0)
        } else {
            0.0
        };
        let lost_units = absorbed_total * (pair_assoc.lift - 1.0) * support_share;
        let other_cm_pu = other_kpi
            .cost_metrics
            .as_ref()
            .map(|cm| cm.cm_per_unit)
            .unwrap_or(0.0);
        let lost_cm = lost_units * other_cm_pu;
        total_comp_loss += lost_cm;
        if lost_units > 0.01 {
            complementary_losses.push(ComplementaryLoss {
                key: other_key.clone(),
                lost_units,
                lost_cm,
            });
        }
    }

    let net = total_recovered - baseline_cm - total_comp_loss;
    let net_lo = total_recovered * 0.50 - baseline_cm - total_comp_loss;
    let net_hi = total_recovered * 1.50 - baseline_cm - total_comp_loss;

    let recommendation = if net > 0.0 {
        RemovalRecommendation::Remove
    } else if total_comp_loss.abs() > baseline_cm * 0.30 {
        RemovalRecommendation::KeepAndBundle
    } else if target_cm.food_cost_pct > 0.45 {
        RemovalRecommendation::KeepAndReformulate
    } else {
        RemovalRecommendation::NoStrongSignal
    };

    let explanation = format!(
        "Removing '{}' (CM={:.0}) would recover ~{:.0} via substitution and lose ~{:.0} \
         in complementary sales. Net CM change: {:.0} [{:.0}, {:.0}].",
        item_name, baseline_cm, total_recovered, total_comp_loss, net, net_lo, net_hi
    );

    Some(RemovalScenario {
        key: target.clone(),
        item_name,
        baseline_cm,
        absorbed_by,
        complementary_losses,
        net_cm_change: net,
        net_cm_change_lo: net_lo,
        net_cm_change_hi: net_hi,
        recommendation,
        explanation,
    })
}

// ═══════════════════════════════════════════════════════════════════
// §25  ORCHESTRATOR
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
    let classifications = classify_items(&kpis, previous);
    let assoc = compute_associations(baskets);

    let price_suggestions =
        suggest_prices(snapshots, &kpis, &classifications, config, price_changed_keys);
    let bundle_suggestions =
        suggest_bundles(snapshots, &kpis, &classifications, &assoc, baskets.len(), config);

    let snap_map: HashMap<ItemKey, &ItemSnapshot> =
        snapshots.iter().map(|s| (s.key.clone(), s)).collect();

    // Removal scenarios ONLY for CM-tracked Dogs.
    let removal_scenarios: Vec<RemovalScenario> = kpis
        .keys()
        .filter(|k| {
            matches!(
                classifications.get(k),
                Some(Classification::Cm { quadrant: CmQuadrant::Dog })
            )
        })
        .filter_map(|k| simulate_removal(k, &kpis, &assoc, &snap_map))
        .collect();

    let mode_summary = {
        let mut s = ModeSummary { items_total: snapshots.len(), ..Default::default() };
        for c in classifications.values() {
            match c {
                Classification::Cm { .. } => s.items_cm_tracked += 1,
                Classification::Revenue { .. } => s.items_revenue_only += 1,
                Classification::Insufficient => s.items_insufficient += 1,
            }
        }
        s
    };

    Ok(AdvisorReport {
        generated_at: now,
        window_days: config.analysis_window_days,
        mode_summary,
        price_suggestions,
        bundle_suggestions,
        removal_scenarios,
    })
}

// ═══════════════════════════════════════════════════════════════════
// §26  TESTS
// ═══════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str) -> ItemKey {
        ItemKey {
            menu_item_id: Uuid::parse_str(id).unwrap(),
            size_label: "one_size".into(),
        }
    }

    // ── Recency weight ────────────────────────────────────────
    #[test]
    fn recency_weight_today_is_one() {
        assert!((recency_weight(0.0, 14.0) - 1.0).abs() < 1e-10);
    }
    #[test]
    fn recency_weight_at_halflife_is_half() {
        assert!((recency_weight(14.0, 14.0) - 0.5).abs() < 1e-10);
    }
    #[test]
    fn recency_weight_decreases_monotonically() {
        for i in 0..30 {
            assert!(recency_weight(i as f64, 14.0) > recency_weight((i + 1) as f64, 14.0));
        }
    }

    // ── Wilson CI ─────────────────────────────────────────────
    #[test]
    fn wilson_bounds_valid() {
        let ci = wilson_95_ci(0.5, 100.0);
        assert!(ci.lo >= 0.0 && ci.hi <= 1.0 && ci.lo < ci.hi);
    }
    #[test]
    fn wilson_empty_sample_is_unit_interval() {
        let ci = wilson_95_ci(0.5, 0.0);
        assert_eq!(ci.lo, 0.0);
        assert_eq!(ci.hi, 1.0);
    }

    // ── Egyptian rounding ─────────────────────────────────────
    #[test]
    fn snap_egyptian_rules() {
        assert_eq!(snap_egyptian(1200.0), 1250);
        assert_eq!(snap_egyptian(1100.0), 1000);
        assert_eq!(snap_egyptian(3300.0), 3500);
        assert_eq!(snap_egyptian(2750.0), 3000);
        assert_eq!(snap_egyptian(2500.0), 2500);
        assert_eq!(snap_egyptian(0.0), 0);
    }

    // ── Guards ────────────────────────────────────────────────
    #[test]
    fn margin_floor_clips_cm_tracked() {
        let config = AnalysisConfig { min_gross_margin_pct: 0.55, ..Default::default() };
        let cm = CostMetrics {
            cost_per_serving: 1000, weighted_cost: 0.0, effective_cost: 1000.0,
            contribution_margin: 0.0, cm_per_unit: 0.0, margin_pct: 0.0,
            food_cost_pct: 0.0, cost_volatility_high: false,
        };
        let (_g, clips) = apply_guards(1500.0, 1500.0, Some(&cm), &config);
        assert!(clips.contains(&GuardClip::MarginFloor));
    }
    #[test]
    fn margin_floor_does_not_clip_revenue_only() {
        let config = AnalysisConfig::default();
        let (g, clips) = apply_guards(100.0, 100.0, None, &config);
        // No margin floor when cost_metrics is None.
        assert!(!clips.contains(&GuardClip::MarginFloor));
        assert!(g <= 100.0 * (1.0 + config.revenue_mode_max_raise_pct) + 0.5);
    }
    #[test]
    fn change_cap_fires() {
        let cfg = AnalysisConfig {
            max_price_change_pct_per_cycle: 0.15,
            min_gross_margin_pct: 0.0,
            ..Default::default()
        };
        let cm = CostMetrics {
            cost_per_serving: 100, weighted_cost: 0.0, effective_cost: 100.0,
            contribution_margin: 0.0, cm_per_unit: 0.0, margin_pct: 0.0,
            food_cost_pct: 0.0, cost_volatility_high: false,
        };
        let (g, clips) = apply_guards(2700.0, 2000.0, Some(&cm), &cfg);
        assert!(clips.contains(&GuardClip::ChangeCap));
        assert!((g - 2300.0).abs() < 1.0);
    }

    // ── No-change threshold ───────────────────────────────────
    #[test]
    fn below_threshold_demotes() {
        assert!(below_no_change_threshold(2000.0, 2020.0));
        assert!(!below_no_change_threshold(2000.0, 2100.0));
    }

    // ── Mixed-mode classification ─────────────────────────────
    #[test]
    fn mixed_mode_classifies_each_population_separately() {
        let k_cm = key("00000000-0000-0000-0000-000000000001");
        let k_rev = key("00000000-0000-0000-0000-000000000002");

        let cm_item = ItemKpi {
            key: k_cm.clone(), sufficient: true, was_inactive: false, current_price: 500,
            raw_units_sold: 100.0, weighted_units_sold: 100.0, weighted_revenue: 50_000.0,
            effective_price: 500.0, popularity_share: 0.80,
            popularity_ci: WilsonInterval { lo: 0.72, hi: 0.88 },
            cost_metrics: Some(CostMetrics {
                cost_per_serving: 150, weighted_cost: 15_000.0, effective_cost: 150.0,
                contribution_margin: 35_000.0, cm_per_unit: 350.0, margin_pct: 0.70,
                food_cost_pct: 0.30, cost_volatility_high: false,
            }),
        };
        let rev_item = ItemKpi {
            key: k_rev.clone(), sufficient: true, was_inactive: false, current_price: 200,
            raw_units_sold: 50.0, weighted_units_sold: 50.0, weighted_revenue: 10_000.0,
            effective_price: 200.0, popularity_share: 0.20,
            popularity_ci: WilsonInterval { lo: 0.10, hi: 0.30 },
            cost_metrics: None,
        };

        let mut kpis = HashMap::new();
        kpis.insert(k_cm.clone(), cm_item);
        kpis.insert(k_rev.clone(), rev_item);

        let cls = classify_items(&kpis, None);
        // CM item must be classified as Cm(_), never Revenue(_).
        assert!(matches!(cls[&k_cm], Classification::Cm { .. }));
        // Revenue item must be classified as Revenue(_), never Cm(_).
        assert!(matches!(cls[&k_rev], Classification::Revenue { .. }));
    }

    #[test]
    fn revenue_only_population_classifies_alone() {
        // Two revenue-only items, no cost data anywhere.
        let k1 = key("00000000-0000-0000-0000-000000000001");
        let k2 = key("00000000-0000-0000-0000-000000000002");
        let kpi1 = ItemKpi {
            key: k1.clone(), sufficient: true, was_inactive: false, current_price: 500,
            raw_units_sold: 100.0, weighted_units_sold: 100.0, weighted_revenue: 50_000.0,
            effective_price: 500.0, popularity_share: 0.80,
            popularity_ci: WilsonInterval { lo: 0.72, hi: 0.88 },
            cost_metrics: None,
        };
        let kpi2 = ItemKpi {
            key: k2.clone(), sufficient: true, was_inactive: false, current_price: 100,
            raw_units_sold: 30.0, weighted_units_sold: 30.0, weighted_revenue: 3_000.0,
            effective_price: 100.0, popularity_share: 0.20,
            popularity_ci: WilsonInterval { lo: 0.12, hi: 0.30 },
            cost_metrics: None,
        };
        let mut kpis = HashMap::new();
        kpis.insert(k1.clone(), kpi1);
        kpis.insert(k2.clone(), kpi2);

        let cls = classify_items(&kpis, None);
        // High pop + high price = Hero
        assert_eq!(cls[&k1], Classification::Revenue { class: RevenueClass::Hero });
        // Low pop + low price = Quiet
        assert_eq!(cls[&k2], Classification::Revenue { class: RevenueClass::Quiet });
    }

    #[test]
    fn removal_scenarios_skip_revenue_only_items() {
        // A "Quiet" revenue-only item must not appear in removal scenarios.
        let k_rev = key("00000000-0000-0000-0000-000000000001");
        let kpi = ItemKpi {
            key: k_rev.clone(), sufficient: true, was_inactive: false, current_price: 100,
            raw_units_sold: 50.0, weighted_units_sold: 50.0, weighted_revenue: 5_000.0,
            effective_price: 100.0, popularity_share: 0.1,
            popularity_ci: WilsonInterval { lo: 0.05, hi: 0.15 },
            cost_metrics: None,
        };
        let mut kpis = HashMap::new();
        kpis.insert(k_rev.clone(), kpi);
        let snaps_owned: Vec<ItemSnapshot> = vec![ItemSnapshot {
            key: k_rev.clone(),
            category_id: None,
            name: "Revenue Only Quiet".into(),
            current_price: 100,
            cost_per_serving: None,
            is_active: true,
            bundle_only: false,
        }];
        let snap_map: HashMap<ItemKey, &ItemSnapshot> =
            snaps_owned.iter().map(|s| (s.key.clone(), s)).collect();
        let assoc = HashMap::new();
        // Even called directly, simulate_removal should return None when
        // cost_metrics is missing.
        let result = simulate_removal(&k_rev, &kpis, &assoc, &snap_map);
        assert!(result.is_none());
    }

    #[test]
    fn bundle_with_mixed_cost_components_has_no_incremental_cm() {
        let cfg = AnalysisConfig { min_units_for_classification: 1.0, ..Default::default() };
        let focus_key = ItemKey {
            menu_item_id: Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            size_label: "one_size".into(),
        };
        let partner_key = ItemKey {
            menu_item_id: Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
            size_label: "one_size".into(),
        };
        let snaps = vec![
            ItemSnapshot {
                key: focus_key.clone(),
                category_id: None,
                name: "Focus".into(),
                current_price: 5000,
                cost_per_serving: Some(2000),
                is_active: true,
                bundle_only: false,
            },
            ItemSnapshot {
                key: partner_key.clone(),
                category_id: None,
                name: "Partner (no cost)".into(),
                current_price: 3000,
                cost_per_serving: None, // ← missing
                is_active: true,
                bundle_only: false,
            },
        ];

        let mut kpis = HashMap::new();
        kpis.insert(focus_key.clone(), ItemKpi {
            key: focus_key.clone(), sufficient: true, was_inactive: false, current_price: 5000,
            raw_units_sold: 50.0, weighted_units_sold: 50.0, weighted_revenue: 250_000.0,
            effective_price: 5000.0, popularity_share: 0.30,
            popularity_ci: WilsonInterval { lo: 0.20, hi: 0.40 },
            cost_metrics: Some(CostMetrics {
                cost_per_serving: 2000, weighted_cost: 100_000.0, effective_cost: 2000.0,
                contribution_margin: 150_000.0, cm_per_unit: 3000.0, margin_pct: 0.60,
                food_cost_pct: 0.40, cost_volatility_high: false,
            }),
        });
        kpis.insert(partner_key.clone(), ItemKpi {
            key: partner_key.clone(), sufficient: true, was_inactive: false, current_price: 3000,
            raw_units_sold: 50.0, weighted_units_sold: 50.0, weighted_revenue: 150_000.0,
            effective_price: 3000.0, popularity_share: 0.30,
            popularity_ci: WilsonInterval { lo: 0.20, hi: 0.40 },
            cost_metrics: None,
        });

        let mut cls = HashMap::new();
        // Force focus to be a CM-tracked "Puzzle" so it's a bundle focus.
        cls.insert(focus_key.clone(), Classification::Cm { quadrant: CmQuadrant::Puzzle });
        cls.insert(partner_key.clone(), Classification::Revenue { class: RevenueClass::Steady });

        // Build a synthetic association map so the pair passes lift/support filters.
        let mut assoc = HashMap::new();
        let pair = if focus_key <= partner_key {
            (focus_key.clone(), partner_key.clone())
        } else {
            (partner_key.clone(), focus_key.clone())
        };
        assoc.insert(pair, Association {
            support: 0.5, confidence_xy: 0.5, lift: 2.0,
        });

        let suggestions = suggest_bundles(&snaps, &kpis, &cls, &assoc, 20, &cfg);
        // At least one bundle should have been generated for the Puzzle focus.
        assert!(!suggestions.is_empty(), "expected at least one bundle suggestion");
        // Every bundle here must report missing_costs = true and no incremental CM.
        for b in &suggestions {
            assert!(b.missing_costs, "missing_costs flag must be set");
            assert!(b.bundle_cost.is_none(), "bundle_cost must be None");
            assert!(b.bundle_cm.is_none(), "bundle_cm must be None");
            assert!(b.forecast.incremental_cm.is_none(), "incremental_cm must be None");
            // Discount must still be perceivable.
            assert!(b.bundle_discount_pct >= 0.05);
        }
    }
}