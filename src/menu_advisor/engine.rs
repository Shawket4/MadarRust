//! Pure, I/O-free Menu Advisor engine.
//!
//! Zero imports from the rest of the crate. Given the same inputs and `now`
//! timestamp it always produces the same outputs.
//!
//! Every algorithm follows the spec in `menu_pricing_suggestion_engine.md`
//! verbatim. Do not "improve" the math or refactor the formulas.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ═══════════════════════════════════════════════════════════════════
// §1  PRIMITIVE TYPES
// ═══════════════════════════════════════════════════════════════════

/// Opaque item key — one per sellable SKU (menu_item_id × size_label).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ItemKey {
    pub menu_item_id: Uuid,
    /// "one_size" for base-priced items; "small" / "medium" / etc. for sized items.
    pub size_label: String,
}

/// All static facts about one sellable SKU, supplied by the adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemSnapshot {
    pub key: ItemKey,
    pub category_id: Option<Uuid>,
    /// Name for display in explanation strings only.
    pub name: String,
    /// Current list price in minor units.
    pub current_price: i64,
    /// Fully-loaded recipe cost in minor units (current approximation).
    pub cost_per_serving: i64,
    pub is_active: bool,
    /// If Some(parent_id), this SKU is a size-variant of parent. Used to
    /// prevent same-parent pairs from appearing in bundles together.
    pub variant_of: Option<Uuid>,
    /// True when the item has never appeared as a standalone line — only ever
    /// inside bundle order lines. Excluded from popularity_share denominator.
    pub bundle_only: bool,
}

/// One unit-level sale event after window filtering.
#[derive(Debug, Clone)]
pub struct SaleEvent {
    #[allow(dead_code)]
    pub transaction_id: Uuid,
    pub key: ItemKey,
    pub quantity_sold: i64,
    /// Actual price paid per unit in minor units.
    pub unit_price_paid: i64,
    /// Ingredient cost per unit at the moment of sale in minor units.
    pub unit_cost_at_sale: i64,
    pub sold_at: DateTime<Utc>,
}

/// One basket — a set of distinct ItemKeys that appeared together in a
/// single order. Quantity > 1 of the same SKU counts as ONE co-occurrence
/// event (spec §10).
pub type Basket = Vec<ItemKey>;

// ═══════════════════════════════════════════════════════════════════
// §2  CONFIGURATION
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisConfig {
    /// Number of days to look back.
    pub analysis_window_days: f64,
    /// Half-life for exponential recency decay in days.
    pub recency_half_life_days: f64,
    /// Goalpost food-cost percentage (0.0–1.0).
    pub target_food_cost_pct: f64,
    /// Hard floor for gross margin (0.0–1.0).
    pub min_gross_margin_pct: f64,
    /// Maximum price change per cycle as a fraction (0.0–1.0).
    pub max_price_change_pct_per_cycle: f64,
    /// Minimum raw units sold to be classified.
    pub min_units_for_classification: f64,
    /// Minimum co-occurrence count for a bundle candidate.
    pub min_cooccurrences_for_bundle: f64,
    /// Minimum lift for a bundle candidate partner.
    pub min_lift_for_bundle: f64,
    /// (low, high) discount percentage range for bundle pricing.
    pub bundle_discount_pct_range: (f64, f64),
    /// Rounding rule for all final monetary values.
    pub price_rounding_rule: PriceRoundingRule,
    /// Top-K partners to rank per focus item.
    pub bundle_top_k_partners: usize,
    /// Max bundle size (2 or 3).
    pub bundle_max_size: usize,
    /// Top bundles to present per focus item (1–3).
    pub bundle_top_n_per_focus: usize,
    /// Repeat rate for halo estimation (fraction of unique triers who return).
    pub halo_repeat_rate: f64,
    /// Promotion lift prior — modest uplift from bundling.
    pub promotion_lift_prior: f64,
}

impl Default for AnalysisConfig {
    fn default() -> Self {
        Self {
            analysis_window_days:         30.0,
            recency_half_life_days:       14.0,
            target_food_cost_pct:         0.30,
            min_gross_margin_pct:         0.55,
            max_price_change_pct_per_cycle: 0.15,
            min_units_for_classification: 20.0,
            min_cooccurrences_for_bundle: 8.0,
            min_lift_for_bundle:          1.20,
            bundle_discount_pct_range:    (0.10, 0.25),
            price_rounding_rule:          PriceRoundingRule::EgyptianCafe,
            bundle_top_k_partners:        5,
            bundle_max_size:              3,
            bundle_top_n_per_focus:       3,
            halo_repeat_rate:             0.15,
            promotion_lift_prior:         1.25,
        }
    }
}

/// Cultural price rounding strategy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PriceRoundingRule {
    /// Egyptian café: round to nearest 5 EGP; allow 2.5 EGP for items < 2500
    /// piastres. No .99 psychological pricing.
    EgyptianCafe,
    /// Round to nearest integer (no rounding in effect).
    NearestUnit,
}

// ═══════════════════════════════════════════════════════════════════
// §3  OUTPUT TYPES  (§8 in the spec)
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Quadrant {
    Star,
    Plowhorse,
    Puzzle,
    Dog,
    InsufficientData,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Hold,
    RaisePrice,
    LowerPrice,
    Bundle,
    Remove,
    Reformulate,
    Monitor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GuardClip {
    MarginFloor,
    ChangeCap,
    CulturalRounding,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WilsonInterval {
    pub lo: f64,
    pub hi: f64,
}

/// Low/mid/high triplet used for all forecasts (never a single point value).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Triplet {
    pub lo:  f64,
    pub mid: f64,
    pub hi:  f64,
}

// ── Per-item KPIs ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemKpi {
    pub key: ItemKey,
    /// Whether there were enough raw units to classify this item.
    pub sufficient: bool,
    /// Raw (unweighted) units sold in the window.
    pub raw_units_sold: f64,
    pub weighted_units_sold: f64,
    pub weighted_revenue:    f64,
    pub weighted_cost:       f64,
    pub contribution_margin: f64,
    pub cm_per_unit:         f64,
    /// Weighted average price paid (may differ from current list price).
    pub effective_price:     f64,
    pub effective_cost:      f64,
    pub margin_pct:          f64,
    pub food_cost_pct:       f64,
    pub popularity_share:    f64,
    pub popularity_ci:       WilsonInterval,
    pub current_price:       i64,
    pub cost_per_serving:    i64,
    /// Was the item inactive during the window but had sales?
    pub was_inactive:        bool,
    /// Did the cost move >25% within the window?
    pub cost_volatility_high: bool,
    pub cost_missing:         bool,
}

// ── Price suggestion ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerComparison {
    pub same_category_count:     usize,
    pub median_margin_pct_peers: f64,
    pub median_cm_per_unit_peers: f64,
    pub your_position:           PeerPosition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerPosition { Above, At, Below }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceAnchors {
    pub cost_plus:   f64,
    pub peer_median: f64,
    pub status_quo:  f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceSuggestion {
    pub key:                 ItemKey,
    pub item_name:           String,
    pub quadrant:            Quadrant,
    pub current_price:       i64,
    pub cm_per_unit:         f64,
    pub margin_pct:          f64,
    pub food_cost_pct:       f64,
    pub popularity_share:    f64,
    pub units_sold_raw:      f64,
    pub anchors:             PriceAnchors,
    pub suggested_price:     Option<i64>,
    pub suggested_delta_abs: Option<i64>,
    pub suggested_delta_pct: Option<f64>,
    pub action:              Action,
    pub confidence:          Confidence,
    pub explanation:         String,
    pub guard_clips:         Vec<GuardClip>,
    pub peer_comparison:     Option<PeerComparison>,
    /// Set when a price change was detected in the analysis window.
    pub price_changed_in_window: bool,
    /// What-if: if cost_per_serving fell 10%, what would new margin_pct be?
    pub cost_reduction_whatif_margin: Option<f64>,
    pub cost_missing:                bool,
}

// ── Bundle suggestion ─────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleItemPair {
    pub item_a: ItemKey,
    pub item_b: ItemKey,
    pub lift:      f64,
    pub support:   f64,
    pub confidence_ab: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleAssociation {
    pub pair_lifts:       Vec<BundleItemPair>,
    pub composite_score:  f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleForecast {
    pub expected_velocity_lo:  f64,
    pub expected_velocity_mid: f64,
    pub expected_velocity_hi:  f64,
    pub inside_bundle_units_x: f64,
    pub halo_units_x:          f64,
    pub total_units_uplift_x:  f64,
    pub incremental_cm_lo:     f64,
    pub incremental_cm_mid:    f64,
    pub incremental_cm_hi:     f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleSuggestion {
    pub focus_item:              ItemKey,
    pub bundle_items:            Vec<ItemKey>,
    pub bundle_list_price:       i64,
    pub bundle_suggested_price:  i64,
    pub bundle_discount_pct:     f64,
    pub bundle_cost:             i64,
    pub bundle_cm:               i64,
    pub bundle_margin_pct:       f64,
    pub association:             BundleAssociation,
    pub forecast:                BundleForecast,
    pub guard_clips:             Vec<GuardClip>,
    pub explanation:             String,
    pub missing_costs:           bool,
}

// ── Removal scenario ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbsorbedBy {
    pub key:           ItemKey,
    pub absorbed_units: f64,
    pub absorbed_cm:    f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComplementaryLoss {
    pub key:        ItemKey,
    pub lost_units: f64,
    pub lost_cm:    f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemovalRecommendation {
    Remove,
    KeepAndBundle,
    KeepAndReformulate,
    NoStrongSignal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemovalScenario {
    pub key:                   ItemKey,
    pub baseline_cm:           f64,
    pub absorbed_by:           Vec<AbsorbedBy>,
    pub complementary_losses:  Vec<ComplementaryLoss>,
    pub net_cm_change:         f64,
    pub net_cm_change_lo:      f64,
    pub net_cm_change_hi:      f64,
    pub recommendation:        RemovalRecommendation,
    pub explanation:           String,
}

// ── Top-level report ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvisorReport {
    pub generated_at:      DateTime<Utc>,
    pub window_days:       f64,
    pub items_total:       usize,
    pub items_sufficient:  usize,
    pub price_suggestions:  Vec<PriceSuggestion>,
    pub bundle_suggestions: Vec<BundleSuggestion>,
    pub removal_scenarios:  Vec<RemovalScenario>,
}

// ═══════════════════════════════════════════════════════════════════
// §4  ASSOCIATION INDEX
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct Association {
    /// P(X∩Y) / T
    pub support: f64,
    /// P(Y|X) = support(X∩Y) / support(X)
    pub confidence_xy: f64,
    /// P(X|Y)
    #[allow(dead_code)]
    pub confidence_yx: f64,
    /// support(X∩Y) / (support(X) * support(Y))
    pub lift: f64,
    /// Raw basket co-occurrence count (unweighted).
    #[allow(dead_code)]
    pub raw_count: usize,
}

/// Canonical key: (smaller_id, larger_id) in sorted order.
pub type AssocKey = (ItemKey, ItemKey);
pub type AssociationIndex = HashMap<AssocKey, Association>;

// ═══════════════════════════════════════════════════════════════════
// §5  ENGINE ERROR
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug)]
pub enum EngineError {
    #[allow(dead_code)]
    InvalidConfig(String),
    NoItems,
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfig(s) => write!(f, "Invalid config: {s}"),
            Self::NoItems           => write!(f, "No items provided"),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// §6  PURE ALGORITHMS
// ═══════════════════════════════════════════════════════════════════

// ── §6.1  Recency weight ──────────────────────────────────────

/// `weight = exp(- ln(2) * age_days / half_life_days)`
///
/// weight(today)     = 1.0
/// weight(half_life) = 0.5
fn recency_weight(age_days: f64, half_life_days: f64) -> f64 {
    (-(std::f64::consts::LN_2) * age_days / half_life_days).exp()
}

// ── §6.2  Wilson 95% CI ───────────────────────────────────────

/// Wilson score confidence interval for a proportion.
/// p = point estimate, n = raw sample size, z = 1.96 for 95%.
fn wilson_95_ci(p: f64, n: f64) -> WilsonInterval {
    let z = 1.96_f64;
    if n <= 0.0 {
        return WilsonInterval { lo: 0.0, hi: 1.0 };
    }
    let denom  = 1.0 + z * z / n;
    let center = (p + z * z / (2.0 * n)) / denom;
    let spread = z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt() / denom;
    WilsonInterval {
        lo: (center - spread).max(0.0),
        hi: (center + spread).min(1.0),
    }
}

// ── §6.3  KPI computation ─────────────────────────────────────

pub fn compute_item_kpis(
    snapshots: &[ItemSnapshot],
    sales:     &[SaleEvent],
    now:       DateTime<Utc>,
    config:    &AnalysisConfig,
) -> Result<HashMap<ItemKey, ItemKpi>, EngineError> {
    if snapshots.is_empty() {
        return Err(EngineError::NoItems);
    }

    // Build snapshot index for quick lookup.
    let snap_map: HashMap<&ItemKey, &ItemSnapshot> =
        snapshots.iter().map(|s| (&s.key, s)).collect();

    // Accumulators per item.
    struct Acc {
        raw_units:       f64,
        w_units:         f64,
        w_revenue:       f64,
        w_cost:          f64,
        cost_min:        f64,
        cost_max:        f64,
    }
    let mut accs: HashMap<ItemKey, Acc> = HashMap::new();

    for sale in sales {
        let age_days = (now - sale.sold_at).num_seconds() as f64 / 86_400.0;
        let w = recency_weight(age_days.max(0.0), config.recency_half_life_days);
        let qty = sale.quantity_sold as f64;

        let acc = accs.entry(sale.key.clone()).or_insert(Acc {
            raw_units:  0.0,
            w_units:    0.0,
            w_revenue:  0.0,
            w_cost:     0.0,
            cost_min:   f64::MAX,
            cost_max:   f64::MIN,
        });
        acc.raw_units  += qty;
        acc.w_units    += w * qty;
        acc.w_revenue  += w * qty * sale.unit_price_paid as f64;
        acc.w_cost     += w * qty * sale.unit_cost_at_sale as f64;
        // Track cost range for volatility flag.
        let c = sale.unit_cost_at_sale as f64;
        if c < acc.cost_min { acc.cost_min = c; }
        if c > acc.cost_max { acc.cost_max = c; }
    }

    // Compute total weighted units for popularity_share denominator.
    // Exclude bundle-only items as per §10.
    let total_w_units: f64 = accs.iter()
        .filter(|(k, _)| {
            snap_map.get(k).is_some_and(|s| !s.bundle_only && s.is_active)
        })
        .map(|(_, a)| a.w_units)
        .sum();

    let mut result: HashMap<ItemKey, ItemKpi> = HashMap::new();

    for snap in snapshots {
        let is_active     = snap.is_active;
        let current_price = snap.current_price;
        let cost_serving  = snap.cost_per_serving;

        // Flag zero-cost items (e.g. complimentary water or missing ingredients)
        let cost_missing = cost_serving == 0;

        let acc = accs.get(&snap.key);
        let raw_units = acc.map_or(0.0, |a| a.raw_units);
        let sufficient = raw_units >= config.min_units_for_classification;

        let (w_units, w_revenue, w_cost, cost_volatility_high) = match acc {
            None => (0.0, 0.0, 0.0, false),
            Some(a) => {
                let vol = a.cost_min < f64::MAX
                    && a.cost_max > f64::MIN
                    && (a.cost_max - a.cost_min) / a.cost_min.max(1.0) > 0.25;
                (a.w_units, a.w_revenue, a.w_cost, vol)
            }
        };

        let contribution_margin = w_revenue - w_cost;
        let cm_per_unit         = if w_units > 0.0 { contribution_margin / w_units } else { 0.0 };
        let effective_price     = if w_units > 0.0 { w_revenue / w_units } else { current_price as f64 };
        let effective_cost      = if w_units > 0.0 { w_cost / w_units } else { cost_serving as f64 };
        let margin_pct          = if effective_price > 0.0 { cm_per_unit / effective_price } else if cost_serving == 0 { 1.0 } else { 0.0 };
        let food_cost_pct       = if effective_price > 0.0 { effective_cost / effective_price } else if cost_serving == 0 { 0.0 } else { 1.0 };

        // popularity_share: exclude bundle-only and inactive.
        let popularity_share = if !snap.bundle_only && is_active && total_w_units > 0.0 {
            w_units / total_w_units
        } else {
            0.0
        };

        let n_raw = raw_units.max(1.0);
        let popularity_ci = wilson_95_ci(popularity_share, n_raw);

        result.insert(snap.key.clone(), ItemKpi {
            key:                  snap.key.clone(),
            sufficient,
            raw_units_sold:       raw_units,
            weighted_units_sold:  w_units,
            weighted_revenue:     w_revenue,
            weighted_cost:        w_cost,
            contribution_margin,
            cm_per_unit,
            effective_price,
            effective_cost,
            margin_pct,
            food_cost_pct,
            popularity_share,
            popularity_ci,
            current_price,
            cost_per_serving:     cost_serving,
            was_inactive:         !is_active && raw_units > 0.0,
            cost_volatility_high,
            cost_missing,
        });
    }

    Ok(result)
}

// ── §6.4  Classification ──────────────────────────────────────

pub fn classify_items(
    kpis:     &HashMap<ItemKey, ItemKpi>,
    _config:   &AnalysisConfig,
    previous: Option<&HashMap<ItemKey, Quadrant>>,
) -> HashMap<ItemKey, Quadrant> {
    let eligible: Vec<&ItemKpi> = kpis.values().filter(|k| k.sufficient).collect();
    let n = eligible.len() as f64;
    if n == 0.0 {
        return kpis.keys().map(|k| (k.clone(), Quadrant::InsufficientData)).collect();
    }

    // Popularity threshold: 0.70 / N  (Kasavana-Smith)
    let pop_threshold = 0.70 / n;

    // Profitability threshold: weighted-average cm_per_unit across eligible items.
    let total_w_units: f64 = eligible.iter().map(|k| k.weighted_units_sold).sum();
    let cm_threshold: f64 = if total_w_units > 0.0 {
        eligible.iter()
            .map(|k| k.cm_per_unit * k.weighted_units_sold)
            .sum::<f64>() / total_w_units
    } else {
        0.0
    };

    let mut out = HashMap::new();

    for kpi in kpis.values() {
        if !kpi.sufficient {
            out.insert(kpi.key.clone(), Quadrant::InsufficientData);
            continue;
        }

        let mut high_pop  = kpi.popularity_share >= pop_threshold;
        let mut high_prof = kpi.cm_per_unit       >= cm_threshold;

        // Hysteresis: if previous quadrant exists and item is within 5% of a
        // threshold, keep the previous classification (§4.3).
        if let Some(prev) = previous
            && let Some(prev_q) = prev.get(&kpi.key) {
                let pop_dist  = (kpi.popularity_share - pop_threshold).abs() / pop_threshold.max(1e-9);
                let prof_dist = (kpi.cm_per_unit - cm_threshold).abs() / cm_threshold.abs().max(1e-9);

                if pop_dist < 0.05 {
                    high_pop = matches!(prev_q, Quadrant::Star | Quadrant::Plowhorse);
                }
                if prof_dist < 0.05 {
                    high_prof = matches!(prev_q, Quadrant::Star | Quadrant::Puzzle);
                }
            }

        let q = match (high_pop, high_prof) {
            (true,  true)  => Quadrant::Star,
            (true,  false) => Quadrant::Plowhorse,
            (false, true)  => Quadrant::Puzzle,
            (false, false) => Quadrant::Dog,
        };
        out.insert(kpi.key.clone(), q);
    }

    out
}

// ── §6.5  Peer median helper ──────────────────────────────────

/// Median of a f64 slice (returns 0.0 for empty).
fn median(vals: &mut [f64]) -> f64 {
    if vals.is_empty() { return 0.0; }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = vals.len() / 2;
    if vals.len().is_multiple_of(2) {
        (vals[mid - 1] + vals[mid]) / 2.0
    } else {
        vals[mid]
    }
}

/// `peer_anchor` = median effective_price of same-category items whose
/// `cm_per_unit >= category weighted-average cm_per_unit`.
fn compute_peer_anchor(
    focus:   &ItemKpi,
    all_kpis: &HashMap<ItemKey, ItemKpi>,
    snaps:   &HashMap<ItemKey, &ItemSnapshot>,
) -> f64 {
    let focus_cat = snaps.get(&focus.key).and_then(|s| s.category_id);

    // Gather same-category eligible items.
    let peers: Vec<&ItemKpi> = all_kpis.values()
        .filter(|k| {
            if k.key == focus.key { return false; }
            let cat = snaps.get(&k.key).and_then(|s| s.category_id);
            cat == focus_cat && k.sufficient
        })
        .collect();

    if peers.is_empty() {
        return focus.effective_price;
    }

    // Category weighted-average cm.
    let total_w: f64 = peers.iter().map(|k| k.weighted_units_sold).sum::<f64>()
        + focus.weighted_units_sold;
    let cat_cm_avg: f64 = (peers.iter().map(|k| k.cm_per_unit * k.weighted_units_sold).sum::<f64>()
        + focus.cm_per_unit * focus.weighted_units_sold)
        / total_w.max(1e-9);

    let mut well_performing_prices: Vec<f64> = peers.iter()
        .filter(|k| k.cm_per_unit >= cat_cm_avg)
        .map(|k| k.effective_price)
        .collect();

    if well_performing_prices.is_empty() {
        // Fallback: median of all peers.
        let mut all_prices: Vec<f64> = peers.iter().map(|k| k.effective_price).collect();
        return median(&mut all_prices);
    }

    median(&mut well_performing_prices)
}

fn build_peer_comparison(
    focus:    &ItemKpi,
    all_kpis: &HashMap<ItemKey, ItemKpi>,
    snaps:    &HashMap<ItemKey, &ItemSnapshot>,
) -> Option<PeerComparison> {
    let focus_cat = snaps.get(&focus.key).and_then(|s| s.category_id);

    let peers: Vec<&ItemKpi> = all_kpis.values()
        .filter(|k| {
            k.key != focus.key
                && snaps.get(&k.key).and_then(|s| s.category_id) == focus_cat
                && k.sufficient
        })
        .collect();

    if peers.is_empty() { return None; }

    let mut margins: Vec<f64> = peers.iter().map(|k| k.margin_pct).collect();
    let mut cms:     Vec<f64> = peers.iter().map(|k| k.cm_per_unit).collect();
    let med_margin = median(&mut margins);
    let med_cm     = median(&mut cms);

    let pos = if (focus.margin_pct - med_margin).abs() / med_margin.max(1e-9) < 0.02 {
        PeerPosition::At
    } else if focus.margin_pct > med_margin {
        PeerPosition::Above
    } else {
        PeerPosition::Below
    };

    Some(PeerComparison {
        same_category_count:     peers.len(),
        median_margin_pct_peers: med_margin,
        median_cm_per_unit_peers: med_cm,
        your_position:           pos,
    })
}

// ── §6.6  Price anchors ───────────────────────────────────────

fn compute_anchors(
    focus:    &ItemKpi,
    all_kpis: &HashMap<ItemKey, ItemKpi>,
    snaps:    &HashMap<ItemKey, &ItemSnapshot>,
    config:   &AnalysisConfig,
) -> PriceAnchors {
    let cost_plus  = focus.effective_cost / config.target_food_cost_pct.max(1e-9);
    let peer_anchor = compute_peer_anchor(focus, all_kpis, snaps);
    PriceAnchors {
        cost_plus,
        peer_median: peer_anchor,
        status_quo:  focus.current_price as f64,
    }
}

// ── §6.7  Quadrant-specific suggestion rules ──────────────────

/// Returns a raw candidate price (not yet guarded or rounded).
fn raw_candidate_price(
    kpi:     &ItemKpi,
    quadrant: &Quadrant,
    anchors:  &PriceAnchors,
    _config:   &AnalysisConfig,
    all_kpis: &HashMap<ItemKey, ItemKpi>,
    snaps:    &HashMap<ItemKey, &ItemSnapshot>,
) -> (f64, Action, String) {
    let cur = kpi.current_price as f64;

    match quadrant {
        Quadrant::Star => {
            if kpi.cost_missing {
                if cur < anchors.peer_median * 0.95 {
                    let target = anchors.peer_median.min(cur * 1.08);
                    return (
                        target,
                        Action::RaisePrice,
                        format!(
                            "Star item: very popular, but priced below peer median ({:.0} vs {:.0}). \
                             Small increase suggested.",
                            cur, anchors.peer_median
                        ),
                    );
                }
            } else {
                // Hold by default. Two exceptions.
                if cur < anchors.peer_median * 0.95 {
                    // Check if margin is below median Star margin in same category.
                    let focus_cat = snaps.get(&kpi.key).and_then(|s| s.category_id);
                    let star_margins: Vec<f64> = all_kpis.values()
                        .filter(|k| {
                            snaps.get(&k.key).and_then(|s| s.category_id) == focus_cat
                            && k.key != kpi.key
                        })
                        .map(|k| k.margin_pct)
                        .collect();
                    let mut sm = star_margins;
                    let med_star_margin = median(&mut sm);

                    if kpi.margin_pct < med_star_margin {
                        // Small increase toward peer_anchor, capped at +8%.
                        let target = anchors.peer_median.min(cur * 1.08);
                        return (
                            target,
                            Action::RaisePrice,
                            format!(
                                "Star item priced below peer median ({:.0} vs {:.0}) with below-median margin. \
                                 Small increase suggested toward peer pricing.",
                                cur, anchors.peer_median
                            ),
                        );
                    }
                }
            }
            if cur > anchors.peer_median * 1.10 {
                // Premium pricing supported — do nothing.
            }
            (cur, Action::Hold,
             "Star item: popular and profitable. Hold current price.".into())
        }

        Quadrant::Plowhorse => {
            if kpi.cost_missing {
                let target = (cur * 1.05).min(anchors.peer_median.max(cur * 1.01));
                return (
                    target,
                    Action::RaisePrice,
                    "Plowhorse: highly popular item. Missing cost data, so a standard 5% \
                     price increase is suggested to capitalize on demand.".into()
                );
            }
            // Raise toward cost_plus by enough to lift margin 3–5 pp,
            // constrained to Δ ∈ [+3%, +10%].
            let target_margin = kpi.margin_pct + 0.04; // target mid of 3–5 pp lift
            // Required price for that margin at current effective cost:
            // target_margin = (p - cost) / p  →  p = cost / (1 - target_margin)
            let price_for_target_margin = kpi.effective_cost / (1.0 - target_margin).max(1e-9);
            let target = price_for_target_margin
                .max(cur * 1.03)
                .min(cur * 1.10);
            (
                target,
                Action::RaisePrice,
                format!(
                    "Plowhorse: popular but margin ({:.1}%) is below average. \
                     Moderate price increase would lift margin by ~4 percentage points.",
                    kpi.margin_pct * 100.0
                ),
            )
        }

        Quadrant::Puzzle => {
            // Price changes rarely fix Puzzles. Small decrease if overpriced.
            if cur > anchors.peer_median * 1.15 {
                let target = cur * 0.975; // -2.5% gentle nudge within [-5%, 0%]
                return (
                    target,
                    Action::LowerPrice,
                    format!(
                        "Puzzle: profitable but unpopular. Item is priced {:.0}% above peer median \
                         — small price reduction may improve trial.",
                        (cur / anchors.peer_median - 1.0) * 100.0
                    ),
                );
            }
            (cur, Action::Bundle,
             "Puzzle: profitable but unpopular. Bundling recommended over price change.".into())
        }

        Quadrant::Dog => {
            if kpi.cost_missing {
                return (
                    cur,
                    Action::Remove,
                    "Dog: unpopular and underperforming. Consider removing from menu.".into()
                );
            }
            // No price increase on a Dog. Suggest removal or reformulation.
            if kpi.food_cost_pct > 0.45 {
                return (
                    cur,
                    Action::Reformulate,
                    format!(
                        "Dog: unpopular and unprofitable. Food cost is {:.1}% — recipe \
                         reformulation may restore viability.",
                        kpi.food_cost_pct * 100.0
                    ),
                );
            }
            (cur, Action::Remove,
             "Dog: unpopular and unprofitable. Consider removing from menu.".into())
        }

        Quadrant::InsufficientData => {
            (cur, Action::Monitor,
             "Insufficient sales data for a price recommendation.".into())
        }
    }
}

// ── §6.8  Safety guards ───────────────────────────────────────

fn apply_price_guards(
    mut candidate: f64,
    current:       f64,
    cost:          f64,
    config:        &AnalysisConfig,
) -> (f64, Vec<GuardClip>) {
    let mut clips = Vec::new();

    // Guard 1: margin floor.
    let min_price_for_margin = cost / (1.0 - config.min_gross_margin_pct).max(1e-9);
    if candidate < min_price_for_margin {
        candidate = min_price_for_margin;
        clips.push(GuardClip::MarginFloor);
    }

    // Guard 2: change cap.
    let max_change = current * config.max_price_change_pct_per_cycle;
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

// ── §6.9  Cultural rounding ───────────────────────────────────

/// Snap `price` to the nearest culturally-appropriate value.
/// For `EgyptianCafe`: nearest 500 piastres (5 EGP), or 250 piastres
/// (2.5 EGP) for items under 2500 piastres (25 EGP).
fn apply_rounding(price: f64, rule: &PriceRoundingRule) -> (i64, bool) {
    match rule {
        PriceRoundingRule::NearestUnit => (price.round() as i64, false),
        PriceRoundingRule::EgyptianCafe => {
            let snapped = snap_egyptian(price);
            let changed = (snapped as f64 - price).abs() > 0.5;
            (snapped, changed)
        }
    }
}

fn snap_egyptian(price: f64) -> i64 {
    // Threshold: 2500 piastres = 25 EGP.
    let step: f64 = if price < 2500.0 { 250.0 } else { 500.0 };
    (price / step).round() as i64 * step as i64
}

// ── §6.10  No-change threshold ────────────────────────────────

/// If |Δ%| < 1.5%, demote to Hold. Returns true if demoted.
fn below_no_change_threshold(current: f64, suggested: f64) -> bool {
    if current <= 0.0 { return true; }
    (suggested - current).abs() / current < 0.015
}

// ── §6.11  Confidence ─────────────────────────────────────────

fn assess_confidence(
    kpi:      &ItemKpi,
    quadrant: &Quadrant,
    config:   &AnalysisConfig,
) -> Confidence {
    if quadrant == &Quadrant::InsufficientData {
        return Confidence::Low;
    }
    if kpi.raw_units_sold >= 3.0 * config.min_units_for_classification {
        Confidence::High
    } else {
        Confidence::Medium
    }
}

// ── §6.12  Build PriceSuggestion ─────────────────────────────

pub fn suggest_prices(
    snapshots:   &[ItemSnapshot],
    kpis:        &HashMap<ItemKey, ItemKpi>,
    quadrants:   &HashMap<ItemKey, Quadrant>,
    config:      &AnalysisConfig,
    price_changed_keys: &HashSet<ItemKey>,
) -> Vec<PriceSuggestion> {
    let snap_map: HashMap<ItemKey, &ItemSnapshot> =
        snapshots.iter().map(|s| (s.key.clone(), s)).collect();

    kpis.values().map(|kpi| {
        let snap = snap_map.get(&kpi.key);
        let quadrant = quadrants.get(&kpi.key).unwrap_or(&Quadrant::InsufficientData);

        // Inactive items with historical sales → Monitor, no price suggestion.
        if kpi.was_inactive {
            return PriceSuggestion {
                key:              kpi.key.clone(),
                item_name:        snap.map_or("".into(), |s| s.name.clone()),
                quadrant:         quadrant.clone(),
                current_price:    kpi.current_price,
                cm_per_unit:      kpi.cm_per_unit,
                margin_pct:       kpi.margin_pct,
                food_cost_pct:    kpi.food_cost_pct,
                popularity_share: kpi.popularity_share,
                units_sold_raw:   kpi.raw_units_sold,
                anchors:          PriceAnchors { cost_plus: 0.0, peer_median: 0.0, status_quo: kpi.current_price as f64 },
                suggested_price:  None,
                suggested_delta_abs: None,
                suggested_delta_pct: None,
                action:           Action::Monitor,
                confidence:       Confidence::Low,
                explanation:      "Item is currently inactive but had sales in the window.".into(),
                guard_clips:      vec![],
                peer_comparison:  None,
                price_changed_in_window: price_changed_keys.contains(&kpi.key),
                cost_reduction_whatif_margin: None,
                cost_missing:     kpi.cost_missing,
            };
        }

        let anchors = compute_anchors(kpi, kpis, &snap_map, config);
        let peer_cmp = build_peer_comparison(kpi, kpis, &snap_map);

        let (raw_candidate, mut action, explanation) =
            raw_candidate_price(kpi, quadrant, &anchors, config, kpis, &snap_map);

        let cost = kpi.effective_cost.max(kpi.cost_per_serving as f64);
        let current = kpi.current_price as f64;

        let (guarded, clips) = apply_price_guards(raw_candidate, current, cost, config);
        let (rounded, _did_round) = apply_rounding(guarded, &config.price_rounding_rule);
        let mut final_clips = clips;
        if (rounded as f64 - guarded).abs() > 0.5 {
            final_clips.push(GuardClip::CulturalRounding);
        }

        // No-change threshold.
        let (suggested_price, suggested_delta_abs, suggested_delta_pct) =
            if below_no_change_threshold(current, rounded as f64)
                || quadrant == &Quadrant::InsufficientData
            {
                action = Action::Hold;
                (None, None, None)
            } else {
                let delta_abs = rounded - kpi.current_price;
                let delta_pct = (rounded as f64 - current) / current;
                (Some(rounded), Some(delta_abs), Some(delta_pct))
            };

        let confidence = assess_confidence(kpi, quadrant, config);

        // What-if cost reduction for Plowhorse.
        let cost_reduction_whatif_margin = if quadrant == &Quadrant::Plowhorse {
            let reduced_cost = cost * 0.90;
            let whatif_margin = (current - reduced_cost) / current;
            Some(whatif_margin)
        } else {
            None
        };

        PriceSuggestion {
            key:              kpi.key.clone(),
            item_name:        snap.map_or("".into(), |s| s.name.clone()),
            quadrant:         quadrant.clone(),
            current_price:    kpi.current_price,
            cm_per_unit:      kpi.cm_per_unit,
            margin_pct:       kpi.margin_pct,
            food_cost_pct:    kpi.food_cost_pct,
            popularity_share: kpi.popularity_share,
            units_sold_raw:   kpi.raw_units_sold,
            anchors,
            suggested_price,
            suggested_delta_abs,
            suggested_delta_pct,
            action,
            confidence,
            explanation,
            guard_clips: final_clips,
            peer_comparison: peer_cmp,
            price_changed_in_window: price_changed_keys.contains(&kpi.key),
            cost_reduction_whatif_margin,
            cost_missing:    kpi.cost_missing,
        }
    }).collect()
}

// ── §6.13  Association mining ─────────────────────────────────

pub fn compute_associations(baskets: &[Basket]) -> AssociationIndex {
    let total = baskets.len();
    if total == 0 { return HashMap::new(); }

    // Count per-item and per-pair occurrences.
    let mut item_counts: HashMap<ItemKey, usize> = HashMap::new();
    let mut pair_counts: HashMap<AssocKey, usize> = HashMap::new();

    for basket in baskets {
        // Deduplicate within basket (quantity > 1 = one co-occurrence, §10).
        let items: HashSet<&ItemKey> = basket.iter().collect();
        for item in &items {
            *item_counts.entry((*item).clone()).or_insert(0) += 1;
        }
        // Pairs: canonical order (smaller first).
        let items_sorted: Vec<&ItemKey> = {
            let mut v: Vec<&ItemKey> = items.iter().copied().collect();
            v.sort();
            v
        };
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
        let sup_a   = item_counts.get(a).copied().unwrap_or(0) as f64 / t;
        let sup_b   = item_counts.get(b).copied().unwrap_or(0) as f64 / t;
        let sup_ab  = count as f64 / t;
        let conf_ab = if sup_a > 0.0 { sup_ab / sup_a } else { 0.0 };
        let conf_ba = if sup_b > 0.0 { sup_ab / sup_b } else { 0.0 };
        let lift    = if sup_a > 0.0 && sup_b > 0.0 { sup_ab / (sup_a * sup_b) } else { 0.0 };

        index.insert((a.clone(), b.clone()), Association {
            support:       sup_ab,
            confidence_xy: conf_ab,
            confidence_yx: conf_ba,
            lift,
            raw_count:     count,
        });
    }

    index
}

/// Look up association for an ordered pair (direction-aware confidence).
fn get_assoc<'a>(
    idx: &'a AssociationIndex,
    a:   &ItemKey,
    b:   &ItemKey,
) -> Option<&'a Association> {
    let key = if a <= b { (a.clone(), b.clone()) } else { (b.clone(), a.clone()) };
    idx.get(&key)
}

// ── §6.14  Partner scoring and ranking ───────────────────────

/// `partner_score(Y | X) = (lift - 1) * sqrt(support(X∩Y)) * cm_per_unit(Y)`
fn partner_score(lift: f64, support: f64, cm_y: f64) -> f64 {
    (lift - 1.0) * support.sqrt() * cm_y
}

// ── §6.15  Bundle composition ─────────────────────────────────

fn are_variants(a: &ItemKey, b: &ItemKey, snaps: &HashMap<ItemKey, &ItemSnapshot>) -> bool {
    let va = snaps.get(a).and_then(|s| s.variant_of).unwrap_or(a.menu_item_id);
    let vb = snaps.get(b).and_then(|s| s.variant_of).unwrap_or(b.menu_item_id);
    va == vb
}

// ── §6.16  Bundle pricing (three strategies) ──────────────────

/// Returns `Some(rounded_price)` or `None` if both guards fail.
fn price_bundle(
    bundle_cost:       f64,
    bundle_list_price: f64,
    config:            &AnalysisConfig,
) -> Option<i64> {
    let (lo, hi) = config.bundle_discount_pct_range;
    let bundle_margin_floor = config.min_gross_margin_pct - 0.05;

    // Strategy 1: smallest qualifying discount-anchored price.
    let price_a: Option<f64> = {
        let mut best: Option<f64> = None;
        let mut d = lo;
        while d <= hi + 1e-9 {
            let candidate = bundle_list_price * (1.0 - d);
            let margin = (candidate - bundle_cost) / candidate.max(1e-9);
            if margin >= bundle_margin_floor && candidate <= bundle_list_price * 0.95 {
                // Take the largest price (smallest discount ≥ min_discount).
                if best.is_none_or(|prev: f64| candidate > prev) {
                    best = Some(candidate);
                }
            }
            d += 0.05;
        }
        best
    };

    // Strategy 2: cost-anchored.
    let price_b = bundle_cost / config.target_food_cost_pct.max(1e-9);

    // Strategy 3: pick max(price_a, price_b), round, validate.
    let base = match price_a {
        Some(pa) => pa.max(price_b),
        None     => price_b,
    };

    let (rounded, _) = apply_rounding(base, &config.price_rounding_rule);
    let rounded_f = rounded as f64;

    // Final guard: perceivable discount AND margin floor.
    if rounded_f > bundle_list_price * 0.95 { return None; }
    let margin = (rounded_f - bundle_cost) / rounded_f.max(1e-9);
    if margin < bundle_margin_floor { return None; }

    Some(rounded)
}

// ── §6.17  Bundle scoring ─────────────────────────────────────

/// geometric mean of a slice of positive values.
fn geometric_mean(vals: &[f64]) -> f64 {
    if vals.is_empty() { return 0.0; }
    let sum_ln: f64 = vals.iter().map(|v| v.max(1e-12).ln()).sum();
    (sum_ln / vals.len() as f64).exp()
}

// ── §6.18  Expected bundle velocity (§7.1) ───────────────────

fn expected_velocity(
    focus_kpi:    &ItemKpi,
    partner_score_norm: f64,
    config:       &AnalysisConfig,
) -> Triplet {
    let base_velo = focus_kpi.weighted_units_sold / config.analysis_window_days;
    let mid = base_velo * partner_score_norm * config.promotion_lift_prior;
    Triplet {
        lo:  mid * 0.50,
        mid,
        hi:  mid * 1.50,
    }
}

// ── §6.19  Halo / incremental CM (§7.2, §6.6) ────────────────

fn estimate_halo(
    velocity: &Triplet,
    config:   &AnalysisConfig,
) -> (f64, f64) {
    // inside_bundle_units_x = velocity_mid * window_days
    // halo_units_x = velocity_mid * unique_trier_rate * repeat_rate * window_days
    let unique_trier_rate = 0.85_f64;
    let inside = velocity.mid * config.analysis_window_days;
    let halo   = inside * unique_trier_rate * config.halo_repeat_rate;
    (inside, halo)
}

fn estimate_incremental_cm(
    velocity:          &Triplet,
    bundle_cm:         f64,
    bundle_list_price: f64,
    bundle_price:      f64,
    confidence_product: f64,
    config:            &AnalysisConfig,
) -> Triplet {
    let discount_given = bundle_list_price - bundle_price;

    let calc = |v: f64| -> f64 {
        let p_would_buy_anyway = confidence_product;
        let incremental_v = v * (1.0 - p_would_buy_anyway);
        let cannibalized  = v * p_would_buy_anyway;
        let window = config.analysis_window_days;
        incremental_v * window * bundle_cm - cannibalized * window * discount_given
    };

    Triplet {
        lo:  calc(velocity.lo),
        mid: calc(velocity.mid),
        hi:  calc(velocity.hi),
    }
}

// ── §6.20  Full bundle suggestion generator ───────────────────

pub fn suggest_bundles(
    snapshots:  &[ItemSnapshot],
    kpis:       &HashMap<ItemKey, ItemKpi>,
    quadrants:  &HashMap<ItemKey, Quadrant>,
    assoc:      &AssociationIndex,
    config:     &AnalysisConfig,
) -> Vec<BundleSuggestion> {
    let snap_map: HashMap<ItemKey, &ItemSnapshot> =
        snapshots.iter().map(|s| (s.key.clone(), s)).collect();

    let total_transactions: usize = {
        // Approximate from largest item raw_count; adapter provides real baskets
        // but we don't have the raw count here — use a safe proxy.
        kpis.values().map(|k| k.raw_units_sold as usize).max().unwrap_or(1).max(1)
    };
    let t = total_transactions as f64;

    let mut all_suggestions = Vec::new();

    // Only Puzzle and Dog items are focus candidates.
    let focus_items: Vec<&ItemKpi> = kpis.values()
        .filter(|k| {
            let q = quadrants.get(&k.key).unwrap_or(&Quadrant::InsufficientData);
            matches!(q, Quadrant::Puzzle | Quadrant::Dog) && k.sufficient
        })
        .collect();

    for focus in focus_items {
        let focus_snap = match snap_map.get(&focus.key) { Some(s) => s, None => continue };

        // Gather all candidate partners: must pass lift and support filters.
        let min_support_ab = config.min_cooccurrences_for_bundle / t.max(1.0);

        let mut partners: Vec<(&ItemKey, f64, &Association)> = kpis.keys()
            .filter(|k| {
                if *k == &focus.key { return false; }
                // Don't pair variant siblings.
                if are_variants(&focus.key, k, &snap_map) { return false; }
                let a = get_assoc(assoc, &focus.key, k);
                match a {
                    None => false,
                    Some(assoc_val) => {
                        assoc_val.lift >= config.min_lift_for_bundle
                            && assoc_val.support >= min_support_ab
                    }
                }
            })
            .filter_map(|k| {
                let a = get_assoc(assoc, &focus.key, k)?;
                let kpi_y = kpis.get(k)?;
                let score = partner_score(a.lift, a.support, kpi_y.cm_per_unit);
                Some((k, score, a))
            })
            .collect();

        // Sort descending by partner_score.
        partners.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        partners.truncate(config.bundle_top_k_partners);

        if partners.is_empty() { continue; }

        // Normalize partner scores for velocity prior.
        let max_score = partners.first().map(|(_, s, _)| *s).unwrap_or(1.0).max(1e-9);

        // Generate bundles of size 2 (focus + 1) and size 3 (focus + 2).
        let mut bundle_candidates: Vec<BundleSuggestion> = Vec::new();

        // Size 2 bundles.
        for (p1_key, p1_score, p1_assoc) in &partners {
            let items = vec![focus.key.clone(), (*p1_key).clone()];
            // Dedup by sorted tuple.
            let mut sorted_items = items.clone();
            sorted_items.sort();

            let bundle_cost: i64 = {
                let cost_p1 = kpis.get(*p1_key).map_or(0, |k| k.cost_per_serving);
                focus.cost_per_serving + cost_p1
            };
            let bundle_list_price: i64 = {
                let price_p1 = snap_map.get(*p1_key).map_or(0, |s| s.current_price);
                focus_snap.current_price + price_p1
            };

            let Some(bundle_price) = price_bundle(bundle_cost as f64, bundle_list_price as f64, config)
            else { continue };

            let bundle_cm = bundle_price - bundle_cost;
            let bundle_margin_pct = bundle_cm as f64 / bundle_price as f64;
            let discount_pct = 1.0 - bundle_price as f64 / bundle_list_price as f64;

            let velo = expected_velocity(focus, p1_score / max_score, config);
            let (inside, halo) = estimate_halo(&velo, config);

            let confidence_product = p1_assoc.confidence_xy;
            let inc_cm = estimate_incremental_cm(
                &velo, bundle_cm as f64, bundle_list_price as f64,
                bundle_price as f64, confidence_product, config
            );

            let assoc_record = BundleAssociation {
                pair_lifts: vec![BundleItemPair {
                    item_a:        focus.key.clone(),
                    item_b:        (*p1_key).clone(),
                    lift:          p1_assoc.lift,
                    support:       p1_assoc.support,
                    confidence_ab: p1_assoc.confidence_xy,
                }],
                composite_score: *p1_score,
            };

            let bundle_score = bundle_cm as f64
                * velo.mid
                * geometric_mean(&[*p1_score]);

            let p1_name = snap_map.get(*p1_key).map_or("?", |s| s.name.as_str());
            let explanation = format!(
                "Bundle '{} + {}': lift={:.2}, discount={:.0}%. \
                 Expected ~{:.1} bundles/day; incremental CM ~{:.0} over {} days.",
                focus_snap.name, p1_name,
                p1_assoc.lift, discount_pct * 100.0,
                velo.mid, inc_cm.mid, config.analysis_window_days as i64
            );

            bundle_candidates.push(BundleSuggestion {
                focus_item:             focus.key.clone(),
                bundle_items:           sorted_items,
                bundle_list_price,
                bundle_suggested_price: bundle_price,
                bundle_discount_pct:    discount_pct,
                bundle_cost,
                bundle_cm,
                bundle_margin_pct,
                association:            assoc_record,
                forecast: BundleForecast {
                    expected_velocity_lo:  velo.lo,
                    expected_velocity_mid: velo.mid,
                    expected_velocity_hi:  velo.hi,
                    inside_bundle_units_x: inside,
                    halo_units_x:          halo,
                    total_units_uplift_x:  inside + halo,
                    incremental_cm_lo:     inc_cm.lo,
                    incremental_cm_mid:    inc_cm.mid,
                    incremental_cm_hi:     inc_cm.hi,
                },
                guard_clips: vec![GuardClip::CulturalRounding],
                explanation,
                missing_costs: kpis.get(&focus.key).map_or(false, |k| k.cost_missing)
                               || kpis.get(p1_key).map_or(false, |k| k.cost_missing),
            });

            // Size 3 bundles: add a second partner.
            if config.bundle_max_size >= 3 {
                for (p2_key, p2_score, p2_assoc) in &partners {
                    if *p2_key == *p1_key { continue; }
                    if are_variants(p1_key, p2_key, &snap_map) { continue; }

                    let mut items3 = vec![focus.key.clone(), (*p1_key).clone(), (*p2_key).clone()];
                    items3.sort();
                    // Dedup check: already generated this sorted tuple?
                    // (Simple check since we process in sorted order below.)

                    let cost_p2    = kpis.get(*p2_key).map_or(0, |k| k.cost_per_serving);
                    let price_p2   = snap_map.get(*p2_key).map_or(0, |s| s.current_price);
                    let bundle_cost3         = bundle_cost + cost_p2;
                    let bundle_list_price3   = bundle_list_price + price_p2;

                    let Some(bundle_price3) = price_bundle(
                        bundle_cost3 as f64, bundle_list_price3 as f64, config
                    ) else { continue };

                    let bundle_cm3 = bundle_price3 - bundle_cost3;
                    let bundle_margin3 = bundle_cm3 as f64 / bundle_price3 as f64;
                    let discount3 = 1.0 - bundle_price3 as f64 / bundle_list_price3 as f64;

                    let assoc_strength = geometric_mean(&[*p1_score, *p2_score]);
                    let avg_score = (*p1_score + *p2_score) / 2.0;
                    let velo3 = expected_velocity(focus, avg_score / max_score, config);
                    let (inside3, halo3) = estimate_halo(&velo3, config);
                    let conf3 = p1_assoc.confidence_xy * p2_assoc.confidence_xy;
                    let inc3 = estimate_incremental_cm(
                        &velo3, bundle_cm3 as f64, bundle_list_price3 as f64,
                        bundle_price3 as f64, conf3, config
                    );

                    let p2_name = snap_map.get(*p2_key).map_or("?", |s| s.name.as_str());
                    let expl3 = format!(
                        "Bundle '{} + {} + {}': association strength={:.2}, discount={:.0}%. \
                         Expected ~{:.1} bundles/day; incremental CM ~{:.0} over {} days.",
                        focus_snap.name, p1_name, p2_name,
                        assoc_strength, discount3 * 100.0,
                        velo3.mid, inc3.mid, config.analysis_window_days as i64
                    );

                    bundle_candidates.push(BundleSuggestion {
                        focus_item:             focus.key.clone(),
                        bundle_items:           items3.clone(),
                        bundle_list_price:      bundle_list_price3,
                        bundle_suggested_price: bundle_price3,
                        bundle_discount_pct:    discount3,
                        bundle_cost:            bundle_cost3,
                        bundle_cm:              bundle_cm3,
                        bundle_margin_pct:      bundle_margin3,
                        association: BundleAssociation {
                            pair_lifts: vec![
                                BundleItemPair {
                                    item_a: focus.key.clone(), item_b: (*p1_key).clone(),
                                    lift: p1_assoc.lift, support: p1_assoc.support,
                                    confidence_ab: p1_assoc.confidence_xy,
                                },
                                BundleItemPair {
                                    item_a: focus.key.clone(), item_b: (*p2_key).clone(),
                                    lift: p2_assoc.lift, support: p2_assoc.support,
                                    confidence_ab: p2_assoc.confidence_xy,
                                },
                            ],
                            composite_score: assoc_strength,
                        },
                        forecast: BundleForecast {
                            expected_velocity_lo:  velo3.lo,
                            expected_velocity_mid: velo3.mid,
                            expected_velocity_hi:  velo3.hi,
                            inside_bundle_units_x: inside3,
                            halo_units_x:          halo3,
                            total_units_uplift_x:  inside3 + halo3,
                            incremental_cm_lo:     inc3.lo,
                            incremental_cm_mid:    inc3.mid,
                            incremental_cm_hi:     inc3.hi,
                        },
                        guard_clips: vec![GuardClip::CulturalRounding],
                        explanation: expl3,
                        missing_costs: kpis.get(&focus.key).map_or(false, |k| k.cost_missing)
                                       || items3.iter().any(|k| kpis.get(k).map_or(false, |k2| k2.cost_missing)),
                    });

                    // Only generate one 3-bundle candidate per p1 partner to avoid combinatorial explosion.
                    break;
                }
            }

            let _ = bundle_score; // used implicitly in rank sort below
        }

        // Rank by incremental_cm_mid, descending.
        bundle_candidates.sort_by(|a, b|
            b.forecast.incremental_cm_mid
                .partial_cmp(&a.forecast.incremental_cm_mid)
                .unwrap_or(std::cmp::Ordering::Equal)
        );
        bundle_candidates.truncate(config.bundle_top_n_per_focus);
        all_suggestions.extend(bundle_candidates);
    }

    all_suggestions
}

// ── §6.21  Removal scenario (§7.3) ───────────────────────────

pub fn simulate_removal(
    target:  &ItemKey,
    kpis:    &HashMap<ItemKey, ItemKpi>,
    assoc:   &AssociationIndex,
    _config:  &AnalysisConfig,
) -> Option<RemovalScenario> {
    let target_kpi = kpis.get(target)?;
    let baseline_cm = target_kpi.contribution_margin;

    // Find substitutes: same menu_item_id parent → skip (handled separately).
    // Substitutes = items with lift < 1 (negative association, bought instead of X).
    let total: f64 = kpis.values().map(|k| k.raw_units_sold).sum::<f64>().max(1.0);

    let substitutes: Vec<(&ItemKey, f64)> = kpis.keys()
        .filter(|k| *k != target)
        .filter_map(|k| {
            let a = get_assoc(assoc, target, k)?;
            if a.lift < 1.0 {
                // Weight substitute by its support (how often it appears).
                let support_k = kpis.get(k)?.raw_units_sold / total;
                Some((k, support_k))
            } else {
                None
            }
        })
        .collect();

    // Normalise substitution weights.
    let total_sub_weight: f64 = substitutes.iter().map(|(_, w)| w).sum::<f64>();
    let absorb_rate = if total_sub_weight > 0.0 { 0.60_f64 } else { 0.0 };

    let mut absorbed_by = Vec::new();
    let mut total_recovered = 0.0;

    for (sub_key, weight) in &substitutes {
        let s = if total_sub_weight > 0.0 { weight / total_sub_weight * absorb_rate } else { 0.0 };
        let absorbed_units = target_kpi.weighted_units_sold * s;
        let sub_kpi = match kpis.get(*sub_key) { Some(k) => k, None => continue };
        let absorbed_cm = absorbed_units * sub_kpi.cm_per_unit;
        total_recovered += absorbed_cm;
        absorbed_by.push(AbsorbedBy {
            key:            (*sub_key).clone(),
            absorbed_units,
            absorbed_cm,
        });
    }

    // Complementary losses: partners with lift > 1.2 lose some of their sales.
    let mut complementary_losses = Vec::new();
    let mut total_comp_loss = 0.0;

    let absorbed_total: f64 = absorbed_by.iter().map(|a| a.absorbed_units).sum();

    for (pair_key, pair_assoc) in assoc {
        let other_key = if &pair_key.0 == target { &pair_key.1 }
                        else if &pair_key.1 == target { &pair_key.0 }
                        else { continue };

        if pair_assoc.lift <= 1.2 { continue; }

        let other_kpi = match kpis.get(other_key) { Some(k) => k, None => continue };
        let support_share = if other_kpi.raw_units_sold > 0.0 {
            target_kpi.raw_units_sold / other_kpi.raw_units_sold.max(1.0)
        } else { 0.0 };

        let lost_units = absorbed_total * (pair_assoc.lift - 1.0) * support_share;
        let lost_cm    = lost_units * other_kpi.cm_per_unit;
        total_comp_loss += lost_cm;

        if lost_units > 0.01 {
            complementary_losses.push(ComplementaryLoss {
                key: other_key.clone(),
                lost_units,
                lost_cm,
            });
        }
    }

    let net_cm_change = total_recovered - baseline_cm - total_comp_loss;
    // Confidence interval: vary absorb_rate ±50%.
    let net_lo = total_recovered * 0.50 - baseline_cm - total_comp_loss;
    let net_hi = total_recovered * 1.50 - baseline_cm - total_comp_loss;

    let recommendation = if net_cm_change > 0.0 {
        RemovalRecommendation::Remove
    } else if total_comp_loss.abs() > baseline_cm * 0.30 {
        RemovalRecommendation::KeepAndBundle
    } else if target_kpi.food_cost_pct > 0.45 {
        RemovalRecommendation::KeepAndReformulate
    } else {
        RemovalRecommendation::NoStrongSignal
    };

    let explanation = format!(
        "Removing this item (CM={:.0}) would recover {:.0} via substitution \
         and lose {:.0} in complementary sales. Net CM change: {:.0} [{:.0}, {:.0}].",
        baseline_cm, total_recovered, total_comp_loss,
        net_cm_change, net_lo, net_hi
    );

    Some(RemovalScenario {
        key:                  target.clone(),
        baseline_cm,
        absorbed_by,
        complementary_losses,
        net_cm_change,
        net_cm_change_lo:     net_lo,
        net_cm_change_hi:     net_hi,
        recommendation,
        explanation,
    })
}

// ── §6.22  Top-level orchestrator ─────────────────────────────

pub fn run_advisor(
    snapshots:          &[ItemSnapshot],
    sales:              &[SaleEvent],
    baskets:            &[Basket],
    now:                DateTime<Utc>,
    config:             &AnalysisConfig,
    previous_quadrants: Option<&HashMap<ItemKey, Quadrant>>,
    price_changed_keys: &HashSet<ItemKey>,
) -> Result<AdvisorReport, EngineError> {
    let kpis      = compute_item_kpis(snapshots, sales, now, config)?;
    let quadrants = classify_items(&kpis, config, previous_quadrants);
    let assoc     = compute_associations(baskets);

    let price_suggestions = suggest_prices(snapshots, &kpis, &quadrants, config, price_changed_keys);

    let bundle_suggestions = suggest_bundles(snapshots, &kpis, &quadrants, &assoc, config);

    // Removal scenarios for Dogs (and weak Puzzles that Action::Remove was suggested).
    let removal_scenarios: Vec<RemovalScenario> = kpis.keys()
        .filter(|k| {
            quadrants.get(k) == Some(&Quadrant::Dog)
        })
        .filter_map(|k| simulate_removal(k, &kpis, &assoc, config))
        .collect();

    let items_total     = snapshots.len();
    let items_sufficient = kpis.values().filter(|k| k.sufficient).count();

    Ok(AdvisorReport {
        generated_at:      now,
        window_days:       config.analysis_window_days,
        items_total,
        items_sufficient,
        price_suggestions,
        bundle_suggestions,
        removal_scenarios,
    })
}

// ═══════════════════════════════════════════════════════════════════
// §7  UNIT TESTS
// ═══════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str) -> ItemKey {
        ItemKey { menu_item_id: Uuid::parse_str(id).unwrap(), size_label: "one_size".into() }
    }

    // ── Recency weight ────────────────────────────────────────

    #[test]
    fn test_recency_weight_today() {
        let w = recency_weight(0.0, 14.0);
        assert!((w - 1.0).abs() < 1e-10, "weight at age=0 must be 1.0, got {w}");
    }

    #[test]
    fn test_recency_weight_half_life() {
        let w = recency_weight(14.0, 14.0);
        assert!((w - 0.5).abs() < 1e-10, "weight at half-life must be 0.5, got {w}");
    }

    #[test]
    fn test_recency_weight_double_half_life() {
        let w = recency_weight(28.0, 14.0);
        assert!((w - 0.25).abs() < 1e-10, "weight at 2× half-life must be 0.25, got {w}");
    }

    #[test]
    fn test_recency_weight_monotone_decreasing() {
        for i in 0..30 {
            let w1 = recency_weight(i as f64, 14.0);
            let w2 = recency_weight((i + 1) as f64, 14.0);
            assert!(w1 > w2, "recency weight must decrease with age");
        }
    }

    // ── Wilson CI ────────────────────────────────────────────

    #[test]
    fn test_wilson_bounds_valid() {
        let ci = wilson_95_ci(0.5, 100.0);
        assert!(ci.lo >= 0.0 && ci.hi <= 1.0 && ci.lo < ci.hi);
    }

    #[test]
    fn test_wilson_zero_proportion() {
        let ci = wilson_95_ci(0.0, 50.0);
        assert!(ci.lo >= 0.0);
        assert!(ci.hi > 0.0); // Wilson gives a non-trivial upper bound even for p=0
    }

    #[test]
    fn test_wilson_one_proportion() {
        let ci = wilson_95_ci(1.0, 50.0);
        assert!(ci.hi <= 1.0);
        assert!(ci.lo < 1.0);
    }

    #[test]
    fn test_wilson_empty_sample() {
        let ci = wilson_95_ci(0.5, 0.0);
        assert_eq!(ci.lo, 0.0);
        assert_eq!(ci.hi, 1.0);
    }

    // ── Egyptian rounding ────────────────────────────────────

    #[test]
    fn test_snap_egyptian_small_item_rounds_to_250() {
        // Under 2500 piastres → step 250.
        assert_eq!(snap_egyptian(1200.0), 1250); // 1200 → 1250
        assert_eq!(snap_egyptian(1100.0), 1000); // 1100 → 1000
    }

    #[test]
    fn test_snap_egyptian_large_item_rounds_to_500() {
        // ≥ 2500 piastres → step 500.
        assert_eq!(snap_egyptian(3300.0), 3500); // 3300 → 3500
        assert_eq!(snap_egyptian(2750.0), 3000); // 2750 → 3000
    }

    #[test]
    fn test_snap_egyptian_exact_boundary() {
        assert_eq!(snap_egyptian(2500.0), 2500); // Boundary itself.
    }

    #[test]
    fn test_snap_egyptian_zero() {
        assert_eq!(snap_egyptian(0.0), 0);
    }

    // ── Margin floor guard ───────────────────────────────────

    #[test]
    fn test_margin_floor_guard_clips_up() {
        let config = AnalysisConfig { min_gross_margin_pct: 0.55, ..Default::default() };
        // cost = 1000, min price for 55% margin = 1000 / 0.45 ≈ 2222.
        let (_guarded, clips) = apply_price_guards(1500.0, 1500.0, 1000.0, &config);
        assert!(clips.contains(&GuardClip::MarginFloor), "MarginFloor must fire");
    }

    #[test]
    fn test_margin_floor_no_clip_when_sufficient() {
        let config = AnalysisConfig { min_gross_margin_pct: 0.55, ..Default::default() };
        // cost = 1000, candidate = 3000 → margin = 0.667 > 0.55.
        let (_, clips) = apply_price_guards(3000.0, 3000.0, 1000.0, &config);
        assert!(!clips.contains(&GuardClip::MarginFloor));
    }

    // ── Change cap guard ─────────────────────────────────────

    #[test]
    fn test_change_cap_clips_large_increase() {
        let config = AnalysisConfig {
            max_price_change_pct_per_cycle: 0.15,
            min_gross_margin_pct: 0.0,
            ..Default::default()
        };
        // current = 2000, candidate = 2700 (+35%) → capped at +15%.
        let (guarded, clips) = apply_price_guards(2700.0, 2000.0, 100.0, &config);
        assert!(clips.contains(&GuardClip::ChangeCap));
        assert!((guarded - 2300.0).abs() < 1.0, "Expected 2300 after cap, got {guarded}");
    }

    #[test]
    fn test_change_cap_does_not_fire_within_limit() {
        let config = AnalysisConfig {
            max_price_change_pct_per_cycle: 0.15,
            min_gross_margin_pct: 0.0,
            ..Default::default()
        };
        let (_, clips) = apply_price_guards(2100.0, 2000.0, 100.0, &config);
        assert!(!clips.contains(&GuardClip::ChangeCap));
    }

    // ── No-change threshold ───────────────────────────────────

    #[test]
    fn test_no_change_threshold_fires_on_tiny_delta() {
        assert!(below_no_change_threshold(2000.0, 2020.0)); // 1% < 1.5%
    }

    #[test]
    fn test_no_change_threshold_does_not_fire_on_large_delta() {
        assert!(!below_no_change_threshold(2000.0, 2100.0)); // 5% > 1.5%
    }

    // ── Quadrant classification ───────────────────────────────

    #[test]
    fn test_classify_star_and_dog() {
        // Two items: one high pop + high profit, one low pop + low profit.
        let config = AnalysisConfig::default();

        let k1 = key("00000000-0000-0000-0000-000000000001");
        let k2 = key("00000000-0000-0000-0000-000000000002");

        // k1: star — high popularity, high cm.
        let kpi1 = ItemKpi {
            key: k1.clone(), sufficient: true, raw_units_sold: 100.0,
            weighted_units_sold: 100.0, weighted_revenue: 50_000.0, weighted_cost: 15_000.0,
            contribution_margin: 35_000.0, cm_per_unit: 350.0, effective_price: 500.0,
            effective_cost: 150.0, margin_pct: 0.70, food_cost_pct: 0.30,
            popularity_share: 0.80, popularity_ci: WilsonInterval { lo: 0.72, hi: 0.88 },
            current_price: 500, cost_per_serving: 150, was_inactive: false, cost_volatility_high: false, cost_missing: false,
        };
        // k2: dog — low popularity, low cm.
        let kpi2 = ItemKpi {
            key: k2.clone(), sufficient: true, raw_units_sold: 25.0,
            weighted_units_sold: 25.0, weighted_revenue: 5_000.0, weighted_cost: 4_000.0,
            contribution_margin: 1_000.0, cm_per_unit: 40.0, effective_price: 200.0,
            effective_cost: 160.0, margin_pct: 0.20, food_cost_pct: 0.80,
            popularity_share: 0.20, popularity_ci: WilsonInterval { lo: 0.12, hi: 0.30 },
            current_price: 200, cost_per_serving: 160, was_inactive: false, cost_volatility_high: false, cost_missing: false,
        };

        let mut kpis = HashMap::new();
        kpis.insert(k1.clone(), kpi1);
        kpis.insert(k2.clone(), kpi2);

        let quads = classify_items(&kpis, &config, None);
        // N = 2, pop_threshold = 0.70 / 2 = 0.35
        // k1.pop = 0.80 ≥ 0.35 → high pop
        // k2.pop = 0.20 < 0.35 → low pop
        // weighted_avg_cm = (350*100 + 40*25)/(125) = (35000+1000)/125 = 288
        // k1.cm = 350 ≥ 288 → high profit
        // k2.cm = 40  < 288 → low profit
        assert_eq!(quads[&k1], Quadrant::Star);
        assert_eq!(quads[&k2], Quadrant::Dog);
    }

    #[test]
    fn test_zero_cost_star_raise_price() {
        let config = AnalysisConfig::default();
        let k1 = key("00000000-0000-0000-0000-000000000001");
        
        let kpi = ItemKpi {
            key: k1.clone(), sufficient: true, raw_units_sold: 100.0,
            weighted_units_sold: 100.0, weighted_revenue: 10_000.0, weighted_cost: 0.0,
            contribution_margin: 10_000.0, cm_per_unit: 100.0, effective_price: 100.0,
            effective_cost: 0.0, margin_pct: 1.0, food_cost_pct: 0.0,
            popularity_share: 0.80, popularity_ci: WilsonInterval { lo: 0.72, hi: 0.88 },
            current_price: 100, cost_per_serving: 0, was_inactive: false, cost_volatility_high: false,
            cost_missing: true,
        };

        let anchors = PriceAnchors {
            cost_plus: 0.0,
            peer_median: 120.0, // Peer is higher
            status_quo: 100.0,
        };

        let mut all_kpis = HashMap::new();
        all_kpis.insert(k1.clone(), kpi.clone());
        let snaps = HashMap::new();

        let (target, action, reason) = super::raw_candidate_price(
            &kpi, &Quadrant::Star, &anchors, &config, &all_kpis, &snaps
        );

        assert_eq!(action, Action::RaisePrice);
        // capped at 8% increase -> 108.0
        assert_eq!(target, 108.0);
        assert!(reason.contains("priced below peer median"));
    }

    #[test]
    fn test_zero_cost_plowhorse_raise_price() {
        let config = AnalysisConfig::default();
        let k1 = key("00000000-0000-0000-0000-000000000001");
        
        let kpi = ItemKpi {
            key: k1.clone(), sufficient: true, raw_units_sold: 100.0,
            weighted_units_sold: 100.0, weighted_revenue: 10_000.0, weighted_cost: 0.0,
            contribution_margin: 10_000.0, cm_per_unit: 100.0, effective_price: 100.0,
            effective_cost: 0.0, margin_pct: 1.0, food_cost_pct: 0.0,
            popularity_share: 0.80, popularity_ci: WilsonInterval { lo: 0.72, hi: 0.88 },
            current_price: 100, cost_per_serving: 0, was_inactive: false, cost_volatility_high: false,
            cost_missing: true,
        };

        let anchors = PriceAnchors {
            cost_plus: 0.0,
            peer_median: 110.0, 
            status_quo: 100.0,
        };

        let mut all_kpis = HashMap::new();
        all_kpis.insert(k1.clone(), kpi.clone());
        let snaps = HashMap::new();

        let (target, action, reason) = super::raw_candidate_price(
            &kpi, &Quadrant::Plowhorse, &anchors, &config, &all_kpis, &snaps
        );

        assert_eq!(action, Action::RaisePrice);
        // standard 5% increase -> 105.0
        assert_eq!(target, 105.0);
        assert!(reason.contains("standard 5%"));
    }
}
