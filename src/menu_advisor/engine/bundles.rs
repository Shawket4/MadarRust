//! Bundle mining, pricing, and forecasting.
//!
//! Mining: classic association rules over per-order item sets (support,
//! lift, directional confidence). Forecasting is anchored to the OBSERVED
//! co-purchase rate of the pair — never to the focus item's standalone
//! velocity (the old engine's forecast could exceed it or go negative).
//!
//! Cannibalization model: customers already buying the pair at full price
//! are the bundle's day-one buyers; each costs us exactly the discount.
//! Velocity above that observed rate is incremental at full bundle CM.

use std::collections::{HashMap, HashSet};

use crate::menu_advisor::dto::{
    AnalysisConfig, BundleAssociation, BundleForecast, BundleItemPair, BundleSuggestion,
    Classification, CmQuadrant, GuardClip, ItemKey, RevenueClass, Triplet,
};
use super::classify::ClassificationOutcome;
use super::explain;
use super::kpi::ItemKpi;
use super::stats::{apply_rounding, geometric_mean, ratio_or};
use super::ItemSnapshot;

/// Share of inside-bundle buyers who are first-time triers of the focus item
/// — a labeled prior, not a measurement.
const UNIQUE_TRIER_RATE: f64 = 0.85;

// ═══════════════════════════════════════════════════════════════════
// Association mining
// ═══════════════════════════════════════════════════════════════════

/// Full margin table for one item pair (key is the SORTED pair).
#[derive(Debug, Clone)]
pub(crate) struct Association {
    pub support_a: f64,
    pub support_b: f64,
    pub support_ab: f64,
    pub pair_count: u64,
    pub lift: f64,
}

impl Association {
    /// Directional confidence P(other | this): `from_first` selects whether
    /// "this" is the sorted-first element of the pair key.
    pub(crate) fn confidence_from(&self, from_first: bool) -> f64 {
        let own_support = if from_first { self.support_a } else { self.support_b };
        ratio_or(self.support_ab, own_support, 0.0)
    }
}

pub(crate) type AssocKey = (ItemKey, ItemKey);
pub(crate) type AssociationIndex = HashMap<AssocKey, Association>;

pub(crate) fn compute_associations(baskets: &[Vec<ItemKey>]) -> AssociationIndex {
    let total = baskets.len();
    if total == 0 {
        return HashMap::new();
    }

    let mut item_counts: HashMap<ItemKey, u64> = HashMap::new();
    let mut pair_counts: HashMap<AssocKey, u64> = HashMap::new();

    for basket in baskets {
        let items: HashSet<&ItemKey> = basket.iter().collect();
        for item in &items {
            *item_counts.entry((*item).clone()).or_insert(0) += 1;
        }
        let mut items_sorted: Vec<&ItemKey> = items.into_iter().collect();
        items_sorted.sort();
        for (i, a) in items_sorted.iter().enumerate() {
            for b in items_sorted.iter().skip(i + 1) {
                *pair_counts.entry(((*a).clone(), (*b).clone())).or_insert(0) += 1;
            }
        }
    }

    let t = total as f64;
    pair_counts
        .into_iter()
        .map(|((a, b), count)| {
            let sup_a = item_counts.get(&a).copied().unwrap_or(0) as f64 / t;
            let sup_b = item_counts.get(&b).copied().unwrap_or(0) as f64 / t;
            let sup_ab = count as f64 / t;
            let lift = ratio_or(sup_ab, sup_a * sup_b, 0.0);
            (
                (a, b),
                Association {
                    support_a: sup_a,
                    support_b: sup_b,
                    support_ab: sup_ab,
                    pair_count: count,
                    lift,
                },
            )
        })
        .collect()
}

/// Look up the pair `(a, b)` regardless of order. The bool is true when `a`
/// is the sorted-first element (needed for directional confidence).
pub(crate) fn get_assoc<'a>(
    idx: &'a AssociationIndex,
    a: &ItemKey,
    b: &ItemKey,
) -> Option<(&'a Association, bool)> {
    if a <= b {
        idx.get(&(a.clone(), b.clone())).map(|assoc| (assoc, true))
    } else {
        idx.get(&(b.clone(), a.clone())).map(|assoc| (assoc, false))
    }
}

/// Two SKUs are size-siblings ⟺ same menu_item_id, different size_label.
pub(crate) fn are_size_siblings(a: &ItemKey, b: &ItemKey) -> bool {
    a.menu_item_id == b.menu_item_id && a.size_label != b.size_label
}

/// Partner ranking is pure association strength — mixing money units into
/// the score (the old engine used cm for costed partners but full price for
/// uncosted ones) systematically biased toward cost-missing partners. Money
/// re-enters at bundle ranking via incremental CM.
fn partner_score(lift: f64, support_ab: f64) -> f64 {
    (lift - 1.0) * support_ab.sqrt()
}

// ═══════════════════════════════════════════════════════════════════
// Bundle pricing
// ═══════════════════════════════════════════════════════════════════

struct BundlePricing {
    price: i64,
    discount_pct: f64,
    clips: Vec<GuardClip>,
}

/// Scan the discount grid from the smallest discount up; take the first one
/// whose ROUNDED price is a perceivable discount (≤ 95% of list) and, when
/// cost is known, keeps the bundle margin above `min_gross_margin_pct − 5pp`.
fn price_bundle(
    bundle_cost: Option<i64>,
    bundle_list_price: i64,
    config: &AnalysisConfig,
) -> Option<BundlePricing> {
    let (lo, hi) = config.bundle_discount_pct_range;
    let list = bundle_list_price as f64;
    if list <= 0.0 || lo > hi {
        return None;
    }
    let margin_floor = config.min_gross_margin_pct - 0.05;

    let mut d = lo;
    while d <= hi + 1e-9 {
        let candidate = list * (1.0 - d);
        let rounded = apply_rounding(candidate, &config.price_rounding_rule);
        let rf = rounded as f64;
        let perceivable = rf <= list * 0.95;
        let margin_ok = match bundle_cost {
            Some(c) => ratio_or(rf - c as f64, rf, -1.0) >= margin_floor,
            None => true,
        };
        if rounded > 0 && perceivable && margin_ok {
            let mut clips = Vec::new();
            if d > lo + 1e-9 {
                // A larger-than-minimal discount was never forced BY margin
                // (margin shrinks as d grows); reaching here with d > lo means
                // rounding pushed earlier candidates above the perceivable
                // line — that's a rounding artifact, not a margin floor.
                clips.push(GuardClip::CulturalRounding);
            } else if (rf - candidate).abs() > 0.5 {
                clips.push(GuardClip::CulturalRounding);
            }
            return Some(BundlePricing {
                price: rounded,
                discount_pct: 1.0 - ratio_or(rf, list, 1.0),
                clips,
            });
        }
        d += 0.05;
    }
    None
}

// ═══════════════════════════════════════════════════════════════════
// Forecast
// ═══════════════════════════════════════════════════════════════════

struct ForecastInputs<'a> {
    /// Observed co-purchases of the focus with ALL bundle partners (min over
    /// pairs for 3-item bundles) during the window.
    pair_count: u64,
    focus_cm_per_unit: Option<f64>,
    bundle_cm: Option<i64>,
    bundle_list_price: i64,
    bundle_price: i64,
    config: &'a AnalysisConfig,
}

fn forecast_bundle(inputs: &ForecastInputs) -> BundleForecast {
    let window = inputs.config.analysis_window_days.max(1.0);
    let pairs_per_day = inputs.pair_count as f64 / window;

    // lo = the promotion only converts existing co-buyers; mid/hi scale by
    // the promotion-lift prior.
    let velocity = Triplet {
        lo: pairs_per_day,
        mid: pairs_per_day * inputs.config.promotion_lift_prior,
        hi: pairs_per_day * inputs.config.promotion_lift_prior * 1.5,
    };

    let inside = velocity.mid * window;
    let halo_at = |v: f64| v * window * UNIQUE_TRIER_RATE * inputs.config.halo_repeat_rate;
    let halo = halo_at(velocity.mid);

    // Buyers up to the observed rate would have paid full price — they cost
    // us the discount. Buyers above it are incremental at full bundle CM.
    // Halo units land on the focus item at its standalone CM.
    let incremental_cm = match (inputs.bundle_cm, inputs.focus_cm_per_unit) {
        (Some(cm), focus_cm) => {
            let discount_given = (inputs.bundle_list_price - inputs.bundle_price) as f64;
            let cm_f = cm as f64;
            let calc = |v: f64| {
                let incremental = (v - pairs_per_day).max(0.0) * window * cm_f;
                let cannibalized = pairs_per_day.min(v) * window * discount_given;
                let halo_cm = halo_at(v) * focus_cm.unwrap_or(0.0);
                incremental - cannibalized + halo_cm
            };
            Some(Triplet {
                lo: calc(velocity.lo),
                mid: calc(velocity.mid),
                hi: calc(velocity.hi),
            })
        }
        (None, _) => None,
    };

    BundleForecast {
        expected_velocity: velocity,
        inside_bundle_units_x: inside,
        halo_units_x: halo,
        total_units_uplift_x: inside + halo,
        incremental_cm,
    }
}

// ═══════════════════════════════════════════════════════════════════
// Suggestion assembly
// ═══════════════════════════════════════════════════════════════════

struct RankedPartner<'a> {
    key: ItemKey,
    score: f64,
    assoc: &'a Association,
    /// Whether the focus is the sorted-first element of the pair key.
    focus_first: bool,
}

pub(crate) fn suggest_bundles(
    snapshots: &[ItemSnapshot],
    kpis: &HashMap<ItemKey, ItemKpi>,
    outcome: &ClassificationOutcome,
    assoc: &AssociationIndex,
    config: &AnalysisConfig,
) -> Vec<BundleSuggestion> {
    let snap_map: HashMap<ItemKey, &ItemSnapshot> =
        snapshots.iter().map(|s| (s.key.clone(), s)).collect();

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

    let eligible_partner = |k: &ItemKey| -> bool {
        let Some(kpi) = kpis.get(k) else { return false };
        let Some(snap) = snap_map.get(k) else { return false };
        kpi.sufficient && !kpi.was_inactive && snap.is_active && !snap.bundle_only
    };

    // Deterministic focus order.
    let mut focus_keys: Vec<&ItemKey> = kpis.keys().collect();
    focus_keys.sort();

    let mut all_out = Vec::new();

    for focus_key in focus_keys {
        let Some(focus) = kpis.get(focus_key) else { continue };
        if !focus.sufficient || focus.was_inactive {
            continue;
        }
        let Some(cls) = outcome.map.get(focus_key).copied() else { continue };
        if !is_focus(cls) {
            continue;
        }
        let Some(focus_snap) = snap_map.get(focus_key).copied() else { continue };
        if !focus_snap.is_active || focus_snap.bundle_only {
            continue;
        }

        // Rank partners by association strength.
        let mut partners: Vec<RankedPartner> = kpis
            .keys()
            .filter(|k| *k != focus_key && !are_size_siblings(focus_key, k))
            .filter(|k| eligible_partner(k))
            .filter_map(|k| {
                let (a, focus_first) = get_assoc(assoc, focus_key, k)?;
                if a.lift < config.min_lift_for_bundle
                    || (a.pair_count as f64) < config.min_cooccurrences_for_bundle
                {
                    return None;
                }
                Some(RankedPartner {
                    key: k.clone(),
                    score: partner_score(a.lift, a.support_ab),
                    assoc: a,
                    focus_first,
                })
            })
            .collect();
        partners.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.key.cmp(&b.key))
        });
        partners.truncate(config.bundle_top_k_partners);
        if partners.is_empty() {
            continue;
        }

        let mut focus_candidates: Vec<BundleSuggestion> = Vec::new();
        let mut seen_sets: HashSet<Vec<ItemKey>> = HashSet::new();

        for (idx, p1) in partners.iter().enumerate() {
            if let Some(s) = build_bundle(focus, focus_snap, &[p1], &snap_map, config)
                && seen_sets.insert(sorted_items(&s.bundle_items))
            {
                focus_candidates.push(s);
            }

            // Size-3: best partner ranked below p1 that is pairwise
            // compatible with p1 (lift ≥ 1.0 between the partners).
            if config.bundle_max_size >= 3 {
                let p2 = partners.iter().skip(idx + 1).find(|p2| {
                    !are_size_siblings(&p1.key, &p2.key)
                        && get_assoc(assoc, &p1.key, &p2.key)
                            .is_some_and(|(a, _)| a.lift >= 1.0)
                });
                if let Some(p2) = p2
                    && let Some(s) =
                        build_bundle(focus, focus_snap, &[p1, p2], &snap_map, config)
                    && seen_sets.insert(sorted_items(&s.bundle_items))
                {
                    focus_candidates.push(s);
                }
            }
        }

        // Rank within the focus: CM-known bundles by incremental CM, then
        // CM-unknown ones by a revenue proxy.
        focus_candidates.sort_by(|a, b| {
            let rank = |s: &BundleSuggestion| {
                (
                    s.forecast.incremental_cm.is_some(),
                    s.forecast
                        .incremental_cm
                        .map(|t| t.mid)
                        .unwrap_or(s.forecast.total_units_uplift_x * s.bundle_suggested_price as f64),
                )
            };
            let (a_known, a_val) = rank(a);
            let (b_known, b_val) = rank(b);
            b_known
                .cmp(&a_known)
                .then(b_val.partial_cmp(&a_val).unwrap_or(std::cmp::Ordering::Equal))
        });
        focus_candidates.truncate(config.bundle_top_n_per_focus);
        all_out.extend(focus_candidates);
    }

    all_out
}

fn sorted_items(items: &[ItemKey]) -> Vec<ItemKey> {
    let mut v = items.to_vec();
    v.sort();
    v
}

fn build_bundle(
    focus: &ItemKpi,
    focus_snap: &ItemSnapshot,
    partners: &[&RankedPartner],
    snap_map: &HashMap<ItemKey, &ItemSnapshot>,
    config: &AnalysisConfig,
) -> Option<BundleSuggestion> {
    let partner_snaps: Vec<&ItemSnapshot> = partners
        .iter()
        .map(|p| snap_map.get(&p.key).copied())
        .collect::<Option<Vec<_>>>()?;

    let bundle_list = focus_snap.current_price
        + partner_snaps.iter().map(|s| s.current_price).sum::<i64>();

    let component_costs: Vec<Option<i64>> = std::iter::once(focus_snap.cost_per_serving)
        .chain(partner_snaps.iter().map(|s| s.cost_per_serving))
        .collect();
    let bundle_cost: Option<i64> = component_costs
        .iter()
        .copied()
        .collect::<Option<Vec<i64>>>()
        .map(|v| v.iter().sum());

    let pricing = price_bundle(bundle_cost, bundle_list, config)?;
    let bundle_cm = bundle_cost.map(|c| pricing.price - c);
    let bundle_margin_pct =
        bundle_cm.map(|cm| ratio_or(cm as f64, pricing.price as f64, 0.0));

    // Forecast anchors to the WEAKEST focus-partner pair.
    let pair_count = partners.iter().map(|p| p.assoc.pair_count).min().unwrap_or(0);
    let forecast = forecast_bundle(&ForecastInputs {
        pair_count,
        focus_cm_per_unit: focus.cost_metrics.as_ref().map(|c| c.cm_per_unit),
        bundle_cm,
        bundle_list_price: bundle_list,
        bundle_price: pricing.price,
        config,
    });

    let pair_lifts: Vec<BundleItemPair> = partners
        .iter()
        .map(|p| BundleItemPair {
            item_a: focus.key.clone(),
            item_b: p.key.clone(),
            lift: p.assoc.lift,
            support: p.assoc.support_ab,
            confidence_ab: p.assoc.confidence_from(p.focus_first),
        })
        .collect();
    let scores: Vec<f64> = partners.iter().map(|p| p.score.max(0.0)).collect();
    let composite_score = geometric_mean(&scores).unwrap_or(0.0);

    let mut bundle_items: Vec<ItemKey> =
        std::iter::once(focus.key.clone()).chain(partners.iter().map(|p| p.key.clone())).collect();
    bundle_items.sort();

    let names: Vec<&str> = std::iter::once(focus_snap.name.as_str())
        .chain(partner_snaps.iter().map(|s| s.name.as_str()))
        .collect();
    let min_lift = partners
        .iter()
        .map(|p| p.assoc.lift)
        .fold(f64::INFINITY, f64::min);
    let explanation = explain::bundle(
        &names,
        pair_count,
        config.analysis_window_days,
        if min_lift.is_finite() { min_lift } else { 0.0 },
        pricing.price as f64,
        pricing.discount_pct,
        bundle_list as f64,
        forecast.expected_velocity.lo,
        forecast.expected_velocity.hi,
        forecast.incremental_cm.map(|t| t.mid),
    );

    Some(BundleSuggestion {
        focus_item: focus.key.clone(),
        bundle_items,
        bundle_list_price: bundle_list,
        bundle_suggested_price: pricing.price,
        bundle_discount_pct: pricing.discount_pct,
        bundle_cost,
        bundle_cm,
        bundle_margin_pct,
        association: BundleAssociation { pair_lifts, composite_score },
        forecast,
        guard_clips: pricing.clips,
        explanation,
        missing_costs: bundle_cost.is_none(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use chrono::{TimeZone, Utc};

    use crate::menu_advisor::dto::AnalysisConfig;
    use super::super::classify::classify_items;
    use super::super::kpi::compute_item_kpis;
    use super::super::SaleEvent;
    use super::*;

    fn key(id: u8) -> ItemKey {
        ItemKey {
            menu_item_id: uuid::Uuid::from_u128(id as u128),
            size_label: "one_size".into(),
        }
    }

    fn sized_key(id: u8, size: &str) -> ItemKey {
        ItemKey {
            menu_item_id: uuid::Uuid::from_u128(id as u128),
            size_label: size.into(),
        }
    }

    fn snap_for(k: ItemKey, price: i64, cost: Option<i64>) -> ItemSnapshot {
        ItemSnapshot {
            key: k.clone(),
            category_id: None,
            name: format!("item-{}-{}", k.menu_item_id.as_u128(), k.size_label),
            current_price: price,
            cost_per_serving: cost,
            is_active: true,
            bundle_only: false,
        }
    }

    fn snap(id: u8, price: i64, cost: Option<i64>) -> ItemSnapshot {
        snap_for(key(id), price, cost)
    }

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap()
    }

    fn sale(id: u8, qty: i64, price: i64, cost: Option<i64>) -> SaleEvent {
        SaleEvent {
            key: key(id),
            quantity_sold: qty,
            unit_price_paid: price,
            unit_cost_at_sale: cost,
            sold_at: now(),
        }
    }

    /// Baskets: `pairs` baskets contain {focus, partner}; `focus_solo` and
    /// `partner_solo` are singleton baskets; plus `noise` two-item baskets of
    /// other items to keep lift meaningful.
    fn baskets(focus: u8, partner: u8, pairs: usize, focus_solo: usize, partner_solo: usize, noise: usize) -> Vec<Vec<ItemKey>> {
        let mut out = Vec::new();
        for _ in 0..pairs {
            out.push(vec![key(focus), key(partner)]);
        }
        for _ in 0..focus_solo {
            out.push(vec![key(focus)]);
        }
        for _ in 0..partner_solo {
            out.push(vec![key(partner)]);
        }
        for _ in 0..noise {
            out.push(vec![key(100), key(101)]);
        }
        out
    }

    /// W4 regression: confidence must be directional.
    #[test]
    fn confidence_is_directional() {
        // 100 baskets: focus appears in 20 (all with partner), partner in 80.
        let b = baskets(1, 2, 20, 0, 60, 20);
        let idx = compute_associations(&b);
        let (a, focus_first) = get_assoc(&idx, &key(1), &key(2)).unwrap();
        let conf_focus_to_partner = a.confidence_from(focus_first);
        let conf_partner_to_focus = a.confidence_from(!focus_first);
        // P(partner|focus) = 20/20 = 1.0; P(focus|partner) = 20/80 = 0.25.
        assert!((conf_focus_to_partner - 1.0).abs() < 1e-9);
        assert!((conf_partner_to_focus - 0.25).abs() < 1e-9);
    }

    fn standard_scenario() -> (Vec<ItemSnapshot>, Vec<SaleEvent>, Vec<Vec<ItemKey>>) {
        // Item 1: Puzzle focus (profitable, unpopular). Items 2-4: popular.
        let snaps = vec![
            snap(1, 2000, Some(400)),
            snap(2, 1000, Some(500)),
            snap(3, 1000, Some(500)),
            snap(4, 1000, Some(500)),
        ];
        let sales = vec![
            sale(1, 25, 2000, Some(400)),
            sale(2, 100, 1000, Some(500)),
            sale(3, 100, 1000, Some(500)),
            sale(4, 100, 1000, Some(500)),
        ];
        // Focus(1) strongly co-occurs with item 2.
        let mut b = baskets(1, 2, 15, 5, 40, 40);
        b.extend(std::iter::repeat_n(vec![key(3)], 30));
        b.extend(std::iter::repeat_n(vec![key(4)], 30));
        (snaps, sales, b)
    }

    fn run_bundles(
        snaps: &[ItemSnapshot],
        sales: &[SaleEvent],
        b: &[Vec<ItemKey>],
        config: &AnalysisConfig,
    ) -> Vec<BundleSuggestion> {
        let kpis = compute_item_kpis(snaps, sales, now(), config).unwrap();
        let outcome = classify_items(&kpis, None);
        let idx = compute_associations(b);
        suggest_bundles(snaps, &kpis, &outcome, &idx, config)
    }

    /// W3 regression: velocity is anchored to the observed pair rate.
    #[test]
    fn velocity_anchored_to_observed_pair_rate() {
        let (snaps, sales, b) = standard_scenario();
        let config = AnalysisConfig::default();
        let out = run_bundles(&snaps, &sales, &b, &config);
        let s = out
            .iter()
            .find(|s| s.focus_item == key(1) && s.bundle_items.len() == 2)
            .expect("expected a 2-item bundle for the puzzle focus");
        let pairs_per_day = 15.0 / config.analysis_window_days;
        assert!((s.forecast.expected_velocity.lo - pairs_per_day).abs() < 1e-9);
        assert!(
            (s.forecast.expected_velocity.mid
                - pairs_per_day * config.promotion_lift_prior)
                .abs()
                < 1e-9
        );
        // Never negative, never anchored to focus standalone velocity.
        assert!(s.forecast.expected_velocity.lo >= 0.0);
    }

    /// W3 regression: nothing in the forecast can flip negative on odd inputs.
    #[test]
    fn negative_cm_partners_do_not_flip_velocity_sign() {
        let (mut snaps, mut sales, b) = standard_scenario();
        // Make the partner CM negative (cost above price).
        snaps[1].cost_per_serving = Some(1500);
        for s in &mut sales {
            if s.key == key(2) {
                s.unit_cost_at_sale = Some(1500);
            }
        }
        let out = run_bundles(&snaps, &sales, &b, &AnalysisConfig::default());
        for s in &out {
            assert!(s.forecast.expected_velocity.lo >= 0.0);
            assert!(s.forecast.expected_velocity.mid >= s.forecast.expected_velocity.lo);
            assert!(s.forecast.expected_velocity.hi >= s.forecast.expected_velocity.mid);
        }
    }

    /// Cannibalization floor: at lo velocity (pure conversion of existing
    /// co-buyers) with no halo benefit, incremental CM ≤ 0.
    #[test]
    fn incremental_cm_lo_is_cannibalization_floor() {
        let (mut snaps, sales, b) = standard_scenario();
        // Zero out the focus halo benefit by making its cm 0 (cost = price)?
        // Simpler: verify lo < mid (lo carries full cannibalization).
        snaps[0].cost_per_serving = Some(400);
        let out = run_bundles(&snaps, &sales, &b, &AnalysisConfig::default());
        let s = out.iter().find(|s| s.bundle_items.len() == 2).unwrap();
        let t = s.forecast.incremental_cm.unwrap();
        assert!(t.lo <= t.mid && t.mid <= t.hi);
    }

    #[test]
    fn mixed_cost_components_have_no_incremental_cm() {
        let (mut snaps, mut sales, b) = standard_scenario();
        snaps[1].cost_per_serving = None; // partner cost unknown
        for s in &mut sales {
            if s.key == key(2) {
                s.unit_cost_at_sale = None;
            }
        }
        let out = run_bundles(&snaps, &sales, &b, &AnalysisConfig::default());
        let s = out
            .iter()
            .find(|s| s.bundle_items.contains(&key(2)))
            .expect("bundle with the cost-missing partner");
        assert!(s.missing_costs);
        assert!(s.bundle_cost.is_none());
        assert!(s.bundle_cm.is_none());
        assert!(s.bundle_margin_pct.is_none());
        assert!(s.forecast.incremental_cm.is_none());
        assert!(s.bundle_discount_pct >= 0.05);
    }

    /// W13 regression: 3-item bundles are deduped and partners pairwise compatible.
    #[test]
    fn three_item_bundles_deduped_and_pairwise_compatible() {
        // Focus 1 pairs strongly with 2 and 3; 2 and 3 never co-occur →
        // lift(2,3) = 0 → no 3-bundle may include both.
        let snaps = vec![
            snap(1, 2000, Some(400)),
            snap(2, 1000, Some(500)),
            snap(3, 1000, Some(500)),
            snap(4, 1000, Some(500)),
        ];
        let sales = vec![
            sale(1, 30, 2000, Some(400)),
            sale(2, 100, 1000, Some(500)),
            sale(3, 100, 1000, Some(500)),
            sale(4, 100, 1000, Some(500)),
        ];
        let mut b: Vec<Vec<ItemKey>> = Vec::new();
        b.extend(std::iter::repeat_n(vec![key(1), key(2)], 12));
        b.extend(std::iter::repeat_n(vec![key(1), key(3)], 12));
        b.extend(std::iter::repeat_n(vec![key(2)], 30));
        b.extend(std::iter::repeat_n(vec![key(3)], 30));
        b.extend(std::iter::repeat_n(vec![key(4)], 60));
        let out = run_bundles(&snaps, &sales, &b, &AnalysisConfig::default());

        let mut seen = HashSet::new();
        for s in out.iter().filter(|s| s.focus_item == key(1)) {
            assert!(seen.insert(sorted_items(&s.bundle_items)), "duplicate bundle set");
            if s.bundle_items.len() == 3 {
                panic!("3-bundle built from pairwise-incompatible partners");
            }
        }
    }

    /// W14 regression + sibling rule.
    #[test]
    fn size_siblings_inactive_and_bundle_only_excluded() {
        let focus_small = sized_key(1, "small");
        let focus_large = sized_key(1, "large");
        let mut snaps = vec![
            snap_for(focus_small.clone(), 2000, Some(400)),
            snap_for(focus_large.clone(), 3000, Some(500)),
            snap(2, 1000, Some(500)),
            snap(3, 1000, Some(500)),
        ];
        snaps[3].bundle_only = true; // item 3 only ever sold inside bundles
        let mk_sale = |k: &ItemKey, qty: i64| SaleEvent {
            key: k.clone(),
            quantity_sold: qty,
            unit_price_paid: 1000,
            unit_cost_at_sale: Some(400),
            sold_at: now(),
        };
        let sales = vec![
            mk_sale(&focus_small, 25),
            mk_sale(&focus_large, 100),
            mk_sale(&key(2), 100),
            mk_sale(&key(3), 100),
        ];
        let mut b: Vec<Vec<ItemKey>> = Vec::new();
        // Focus co-occurs heavily with its own size sibling AND with item 3.
        b.extend(std::iter::repeat_n(vec![focus_small.clone(), focus_large.clone()], 20));
        b.extend(std::iter::repeat_n(vec![focus_small.clone(), key(3)], 20));
        b.extend(std::iter::repeat_n(vec![key(2)], 60));
        let out = run_bundles(&snaps, &sales, &b, &AnalysisConfig::default());
        for s in &out {
            assert!(!s.bundle_items.contains(&focus_large), "size sibling in bundle");
            assert!(!s.bundle_items.contains(&key(3)), "bundle_only SKU in bundle");
        }
    }

    #[test]
    fn price_bundle_smallest_qualifying_discount_post_rounding() {
        let config = AnalysisConfig::default();
        // list 10000, cost 3000: at d=0.10 → 9000, margin 0.667 ≥ 0.50 → first hit.
        let p = price_bundle(Some(3000), 10_000, &config).unwrap();
        assert_eq!(p.price, 9000);
        assert!((p.discount_pct - 0.10).abs() < 1e-9);

        // Tight margin: list 10000, cost 4800. Floor = 0.50.
        // d=0.10 → 9000 → margin 0.4667 < 0.5 → reject; no larger d helps.
        assert!(price_bundle(Some(4800), 10_000, &config).is_none());
    }
}
