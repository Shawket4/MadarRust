//! Price suggestions: anchors, per-quadrant candidates, and the guard
//! pipeline.
//!
//! Guard ordering (the old engine rounded AFTER the cap and could round past
//! it, or snap small prices to zero):
//!   1. margin floor (CM-tracked only) lifts the candidate;
//!   2. the per-cycle change cap clamps it — the cap deliberately wins over
//!      the floor so prices converge gradually across cycles;
//!   3. rounding snaps to the cultural grid but must stay inside the cap
//!      bounds AND on the intended side of the current price — when the grid
//!      has no such point, the move demotes to Hold rather than violating a
//!      guard.

use std::collections::{HashMap, HashSet};

use super::ItemSnapshot;
use super::classify::ClassificationOutcome;
use super::explain;
use super::kpi::{CostMetrics, ItemKpi};
use super::stats::{apply_rounding, below_no_change_threshold, median, ratio_or, rounding_step};
use crate::menu_advisor::dto::{
    Action, AnalysisConfig, Classification, CmQuadrant, Confidence, GuardClip, ItemKey,
    PeerComparison, PeerPosition, PriceAnchors, PriceRoundingRule, PriceSuggestion, RevenueClass,
};

// ═══════════════════════════════════════════════════════════════════
// Peers & anchors
// ═══════════════════════════════════════════════════════════════════

fn peers_in_category<'a>(
    focus: &ItemKey,
    all_kpis: &'a HashMap<ItemKey, ItemKpi>,
    snaps: &HashMap<ItemKey, &ItemSnapshot>,
) -> Vec<&'a ItemKpi> {
    let focus_cat = snaps.get(focus).and_then(|s| s.category_id);
    let mut peers: Vec<&ItemKpi> = all_kpis
        .values()
        .filter(|k| {
            k.key != *focus
                && snaps.get(&k.key).and_then(|s| s.category_id) == focus_cat
                && k.sufficient
        })
        .collect();
    // Deterministic float reductions downstream (weighted sums over peers).
    peers.sort_by(|a, b| a.key.cmp(&b.key));
    peers
}

/// Peer price anchor: median price of well-performing CM peers (CM ≥ the
/// category's units-weighted average) when the focus is CM-tracked, falling
/// back to the all-peer median, falling back to the focus's own price.
fn compute_peer_anchor(
    focus: &ItemKpi,
    all_kpis: &HashMap<ItemKey, ItemKpi>,
    snaps: &HashMap<ItemKey, &ItemSnapshot>,
) -> f64 {
    let peers = peers_in_category(&focus.key, all_kpis, snaps);
    if peers.is_empty() {
        return focus.effective_price;
    }

    if let Some(focus_cm) = &focus.cost_metrics {
        let cm_peers: Vec<&ItemKpi> = peers
            .iter()
            .filter(|k| k.cost_metrics.is_some())
            .copied()
            .collect();
        if !cm_peers.is_empty() {
            let total_w: f64 = cm_peers.iter().map(|k| k.weighted_units_sold).sum::<f64>()
                + focus.weighted_units_sold;
            let weighted_cm_sum: f64 = cm_peers
                .iter()
                .filter_map(|k| {
                    k.cost_metrics
                        .as_ref()
                        .map(|c| c.cm_per_unit * k.weighted_units_sold)
                })
                .sum::<f64>()
                + focus_cm.cm_per_unit * focus.weighted_units_sold;
            let cat_cm_avg = ratio_or(weighted_cm_sum, total_w, 0.0);

            let well_perf: Vec<f64> = cm_peers
                .iter()
                .filter(|k| {
                    k.cost_metrics
                        .as_ref()
                        .is_some_and(|c| c.cm_per_unit >= cat_cm_avg)
                })
                .map(|k| k.effective_price)
                .collect();
            if let Some(m) = median(&well_perf) {
                return m;
            }
        }
    }

    let prices: Vec<f64> = peers.iter().map(|k| k.effective_price).collect();
    median(&prices).unwrap_or(focus.effective_price)
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

    let prices: Vec<f64> = peers.iter().map(|k| k.effective_price).collect();
    let med_price = median(&prices)?;

    let (med_margin, med_cm) = if focus.cost_metrics.is_some() {
        let margins: Vec<f64> = peers
            .iter()
            .filter_map(|k| k.cost_metrics.as_ref().map(|cm| cm.margin_pct))
            .collect();
        let cms: Vec<f64> = peers
            .iter()
            .filter_map(|k| k.cost_metrics.as_ref().map(|cm| cm.cm_per_unit))
            .collect();
        (median(&margins), median(&cms))
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
    // cost_plus is a display anchor for the human, not a candidate input.
    let cost_plus = focus
        .cost_metrics
        .as_ref()
        .map(|cm| cm.effective_cost / config.target_food_cost_pct.max(1e-9));
    PriceAnchors {
        cost_plus,
        peer_median: compute_peer_anchor(focus, all_kpis, snaps),
        status_quo: focus.current_price as f64,
    }
}

// ═══════════════════════════════════════════════════════════════════
// Raw candidates (per quadrant / class)
// ═══════════════════════════════════════════════════════════════════

fn cm_raw_candidate(
    kpi: &ItemKpi,
    cm: &CostMetrics,
    quadrant: CmQuadrant,
    anchors: &PriceAnchors,
    all_kpis: &HashMap<ItemKey, ItemKpi>,
    snaps: &HashMap<ItemKey, &ItemSnapshot>,
    classifications: &HashMap<ItemKey, Classification>,
) -> (f64, Action, String) {
    let cur = kpi.current_price as f64;
    match quadrant {
        CmQuadrant::Star => {
            if cur < anchors.peer_median * 0.95 {
                // Margin benchmark against same-category STARS only — mixing
                // in Dogs/Puzzles drags the benchmark down and fires raises
                // against the wrong peer set.
                let focus_cat = snaps.get(&kpi.key).and_then(|s| s.category_id);
                let star_margins: Vec<f64> = all_kpis
                    .values()
                    .filter(|k| {
                        k.key != kpi.key
                            && snaps.get(&k.key).and_then(|s| s.category_id) == focus_cat
                            && matches!(
                                classifications.get(&k.key),
                                Some(Classification::Cm {
                                    quadrant: CmQuadrant::Star
                                })
                            )
                    })
                    .filter_map(|k| k.cost_metrics.as_ref().map(|c| c.margin_pct))
                    .collect();
                match median(&star_margins) {
                    None => return (cur, Action::Hold, explain::star_hold_no_benchmark()),
                    Some(med_star_margin) if cm.margin_pct < med_star_margin => {
                        let target = anchors.peer_median.min(cur * 1.08);
                        return (
                            target,
                            Action::RaisePrice,
                            explain::star_raise(cur, anchors.peer_median),
                        );
                    }
                    Some(_) => {}
                }
            }
            (cur, Action::Hold, explain::star_hold(cur))
        }

        CmQuadrant::Plowhorse => {
            // Raise to lift margin ~4 pp; algebra: price for margin m is
            // cost / (1 − m). Bounded to [+3%, +10%].
            let target_margin = cm.margin_pct + 0.04;
            let price_for_target = cm.effective_cost / (1.0 - target_margin).max(1e-9);
            let target = price_for_target.clamp(cur * 1.03, cur * 1.10);
            (
                target,
                Action::RaisePrice,
                explain::plowhorse_raise(cm.margin_pct),
            )
        }

        CmQuadrant::Puzzle => {
            if cur > anchors.peer_median * 1.15 {
                let premium = ratio_or(cur, anchors.peer_median, 1.0) - 1.0;
                (
                    cur * 0.975,
                    Action::LowerPrice,
                    explain::puzzle_lower(premium),
                )
            } else {
                (cur, Action::Bundle, explain::puzzle_bundle())
            }
        }

        CmQuadrant::Dog => {
            if cm.food_cost_pct > 0.45 {
                (
                    cur,
                    Action::Reformulate,
                    explain::dog_reformulate(cm.food_cost_pct),
                )
            } else {
                (cur, Action::Remove, explain::dog_remove())
            }
        }
    }
}

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
                let below = 1.0 - ratio_or(cur, anchors.peer_median, 1.0);
                (target, Action::RaisePrice, explain::hero_raise(below))
            } else {
                (cur, Action::Hold, explain::hero_hold())
            }
        }
        RevenueClass::Steady => (
            cur * (1.0 + cap_pct),
            Action::RaisePrice,
            explain::steady_raise(cap_pct),
        ),
        RevenueClass::Slow => (cur, Action::Bundle, explain::slow_bundle()),
        RevenueClass::Quiet => (cur, Action::Monitor, explain::quiet_monitor()),
    }
}

// ═══════════════════════════════════════════════════════════════════
// Guard pipeline
// ═══════════════════════════════════════════════════════════════════

fn change_cap_pct(cost_metrics: Option<&CostMetrics>, config: &AnalysisConfig) -> f64 {
    match cost_metrics {
        Some(_) => config.max_price_change_pct_per_cycle,
        None => config.revenue_mode_max_raise_pct.max(0.02),
    }
}

/// Guards 1 + 2: margin floor (CM only), then the change cap.
fn apply_guards(
    mut candidate: f64,
    current: f64,
    cost_metrics: Option<&CostMetrics>,
    config: &AnalysisConfig,
) -> (f64, Vec<GuardClip>) {
    let mut clips = Vec::new();

    if let Some(cm) = cost_metrics {
        let floor = cm.effective_cost / (1.0 - config.min_gross_margin_pct).max(1e-9);
        if candidate < floor {
            candidate = floor;
            clips.push(GuardClip::MarginFloor);
        }
    }

    let max_change = current * change_cap_pct(cost_metrics, config);
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

/// Guard 3: snap to the price grid without leaving the cap bounds or
/// flipping the direction of the move. `None` ⟺ no grid point qualifies
/// (grid coarser than the cap window) — the caller demotes to Hold.
fn round_within_guards(
    guarded: f64,
    current: f64,
    raise: bool,
    lo: f64,
    hi: f64,
    rule: &PriceRoundingRule,
) -> Option<i64> {
    let step = rounding_step(guarded, rule);
    let mut p = apply_rounding(guarded, rule);

    // One corrective notch back inside the cap bounds.
    if (p as f64) > hi {
        p -= step;
    } else if (p as f64) < lo {
        p += step;
    }
    // One corrective notch onto the intended side of the current price.
    if raise && (p as f64) <= current {
        p += step;
    } else if !raise && (p as f64) >= current {
        p -= step;
    }

    let pf = p as f64;
    let in_bounds = pf <= hi + 1e-6 && pf >= lo - 1e-6;
    let right_side = if raise { pf > current } else { pf < current };
    (p > 0 && in_bounds && right_side).then_some(p)
}

// ═══════════════════════════════════════════════════════════════════
// Confidence
// ═══════════════════════════════════════════════════════════════════

fn assess_confidence(
    kpi: &ItemKpi,
    classification: Classification,
    outcome: &ClassificationOutcome,
    config: &AnalysisConfig,
) -> Confidence {
    if matches!(classification, Classification::Insufficient) {
        return Confidence::Low;
    }
    let mut c = if kpi.raw_units_sold >= 3.0 * config.min_units_for_classification {
        Confidence::High
    } else if kpi.raw_units_sold >= config.min_units_for_classification {
        Confidence::Medium
    } else {
        Confidence::Low
    };
    // Statistically unsettled classification.
    if outcome.borderline.contains(&kpi.key) {
        c = c.min(Confidence::Medium);
    }
    // Degenerate population (a singleton is always a Star).
    if outcome.small_population.contains(&kpi.key) {
        c = c.min(Confidence::Medium);
    }
    // Missing the cost dimension entirely.
    if matches!(classification, Classification::Revenue { .. }) {
        c = c.min(Confidence::Medium);
    }
    // Ingredient cost moved >25% inside the window — margins are a moving target.
    if kpi
        .cost_metrics
        .as_ref()
        .is_some_and(|m| m.cost_volatility_high)
    {
        c = c.min(Confidence::Medium);
    }
    c
}

// ═══════════════════════════════════════════════════════════════════
// Pipeline
// ═══════════════════════════════════════════════════════════════════

pub(crate) fn suggest_prices(
    snapshots: &[ItemSnapshot],
    kpis: &HashMap<ItemKey, ItemKpi>,
    outcome: &ClassificationOutcome,
    config: &AnalysisConfig,
    price_changed_keys: &HashSet<ItemKey>,
) -> Vec<PriceSuggestion> {
    let snap_map: HashMap<ItemKey, &ItemSnapshot> =
        snapshots.iter().map(|s| (s.key.clone(), s)).collect();

    let mut out = Vec::with_capacity(kpis.len());

    for kpi in kpis.values() {
        let snap = snap_map.get(&kpi.key);
        let item_name = snap.map_or(String::new(), |s| s.name.clone());
        let classification = outcome
            .map
            .get(&kpi.key)
            .copied()
            .unwrap_or(Classification::Insufficient);
        let cost_missing = kpi.cost_metrics.is_none();
        let recently_repriced = price_changed_keys.contains(&kpi.key);
        let cur = kpi.current_price as f64;

        // Inactive items: monitor only, no suggestion.
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
                    status_quo: cur,
                },
                suggested_price: None,
                suggested_delta_abs: None,
                suggested_delta_pct: None,
                action: Action::Monitor,
                confidence: Confidence::Low,
                explanation: explain::inactive_with_sales(),
                guard_clips: vec![],
                peer_comparison: None,
                price_changed_in_window: recently_repriced,
                cost_reduction_whatif_margin: None,
                cost_missing,
            });
            continue;
        }

        let anchors = compute_anchors(kpi, kpis, &snap_map, config);
        let peer_comparison = build_peer_comparison(kpi, kpis, &snap_map);

        let (raw, mut action, mut explanation) = match (classification, &kpi.cost_metrics) {
            (Classification::Cm { quadrant }, Some(cm)) => {
                cm_raw_candidate(kpi, cm, quadrant, &anchors, kpis, &snap_map, &outcome.map)
            }
            (Classification::Revenue { class }, None) => {
                revenue_raw_candidate(kpi, class, &anchors, config)
            }
            (Classification::Insufficient, _) => {
                let text = if snap.is_some_and(|s| s.bundle_only) {
                    explain::bundle_only_sku()
                } else {
                    explain::insufficient(
                        kpi.raw_units_sold,
                        config.analysis_window_days,
                        config.min_units_for_classification,
                    )
                };
                (cur, Action::Monitor, text)
            }
            // The classifier never produces these; treat defensively.
            (Classification::Cm { .. }, None) | (Classification::Revenue { .. }, Some(_)) => (
                cur,
                Action::Monitor,
                "Classification/cost-metrics invariant violated; skipping.".into(),
            ),
        };

        // Epoch suppression: an SKU repriced inside the window hasn't shown
        // the demand response yet — a fresh move would compound an unmeasured
        // one. Non-price actions (Bundle/Remove/Reformulate/Monitor) stand.
        if recently_repriced && matches!(action, Action::RaisePrice | Action::LowerPrice) {
            action = Action::Hold;
            explanation.push_str(explain::suppressed_suffix());
        }

        // Guard pipeline — only price moves produce a suggested price.
        let mut suggested_price = None;
        let mut suggested_delta_abs = None;
        let mut suggested_delta_pct = None;
        let mut guard_clips: Vec<GuardClip> = vec![];

        if matches!(action, Action::RaisePrice | Action::LowerPrice) {
            let raise = raw > cur;
            let (guarded, mut clips) = apply_guards(raw, cur, kpi.cost_metrics.as_ref(), config);
            let cap_abs = cur * change_cap_pct(kpi.cost_metrics.as_ref(), config);
            let direction_intact = if raise { guarded > cur } else { guarded < cur };

            let rounded = direction_intact
                .then(|| {
                    round_within_guards(
                        guarded,
                        cur,
                        raise,
                        cur - cap_abs,
                        cur + cap_abs,
                        &config.price_rounding_rule,
                    )
                })
                .flatten();

            match rounded {
                Some(p) if !below_no_change_threshold(cur, p as f64) => {
                    if (p as f64 - guarded).abs() > 0.5 {
                        clips.push(GuardClip::CulturalRounding);
                    }
                    suggested_price = Some(p);
                    suggested_delta_abs = Some(p - kpi.current_price);
                    suggested_delta_pct = Some((p as f64 - cur) / cur.max(1.0));
                    guard_clips = clips;
                }
                Some(_) => {
                    // Post-guard move too small to matter.
                    action = Action::Hold;
                }
                None => {
                    // Guards flipped the direction or the grid has no point
                    // inside the cap window.
                    action = Action::Hold;
                    explanation.push_str(explain::grid_too_coarse_suffix());
                }
            }
        }

        // What-if cost-reduction for CM-tracked Plowhorses only.
        let cost_reduction_whatif_margin = match (classification, &kpi.cost_metrics) {
            (
                Classification::Cm {
                    quadrant: CmQuadrant::Plowhorse,
                },
                Some(cm),
            ) => Some((cur - cm.effective_cost * 0.90) / cur.max(1e-9)),
            _ => None,
        };

        let confidence = assess_confidence(kpi, classification, outcome, config);

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
            suggested_delta_abs,
            suggested_delta_pct,
            action,
            confidence,
            explanation,
            guard_clips,
            peer_comparison,
            price_changed_in_window: recently_repriced,
            cost_reduction_whatif_margin,
            cost_missing,
        });
    }

    out
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::super::SaleEvent;
    use super::super::classify::classify_items;
    use super::super::kpi::compute_item_kpis;
    use super::*;

    fn key(id: u8) -> ItemKey {
        ItemKey {
            menu_item_id: uuid::Uuid::from_u128(id as u128),
            size_label: "one_size".into(),
        }
    }

    fn snap(id: u8, price: i64, cost: Option<i64>) -> ItemSnapshot {
        ItemSnapshot {
            key: key(id),
            category_id: None,
            name: format!("item-{id}"),
            current_price: price,
            cost_per_serving: cost,
            is_active: true,
            bundle_only: false,
        }
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

    fn run(
        snaps: &[ItemSnapshot],
        sales: &[SaleEvent],
        config: &AnalysisConfig,
        changed: &HashSet<ItemKey>,
    ) -> Vec<PriceSuggestion> {
        let kpis = compute_item_kpis(snaps, sales, now(), config).unwrap();
        let outcome = classify_items(&kpis, None);
        suggest_prices(snaps, &kpis, &outcome, config, changed)
    }

    fn find<'a>(out: &'a [PriceSuggestion], id: u8) -> &'a PriceSuggestion {
        out.iter().find(|s| s.key == key(id)).unwrap()
    }

    #[test]
    fn margin_floor_clips_cm_tracked() {
        let (guarded, clips) = apply_guards(
            500.0,
            1000.0,
            Some(&CostMetrics {
                effective_cost: 450.0,
                cm_per_unit: 550.0,
                margin_pct: 0.55,
                food_cost_pct: 0.45,
                cost_volatility_high: false,
            }),
            &AnalysisConfig::default(),
        );
        // floor = 450 / 0.45 = 1000 → candidate lifted to the floor.
        assert!(clips.contains(&GuardClip::MarginFloor));
        assert!((guarded - 1000.0).abs() < 1e-9);
    }

    #[test]
    fn margin_floor_does_not_clip_revenue_only() {
        let (guarded, clips) = apply_guards(980.0, 1000.0, None, &AnalysisConfig::default());
        assert!(!clips.contains(&GuardClip::MarginFloor));
        assert!((guarded - 980.0).abs() < 1e-9);
    }

    #[test]
    fn change_cap_fires() {
        let cmm = CostMetrics {
            effective_cost: 100.0,
            cm_per_unit: 900.0,
            margin_pct: 0.9,
            food_cost_pct: 0.1,
            cost_volatility_high: false,
        };
        let (guarded, clips) = apply_guards(1500.0, 1000.0, Some(&cmm), &AnalysisConfig::default());
        assert!(clips.contains(&GuardClip::ChangeCap));
        assert!((guarded - 1150.0).abs() < 1e-9); // +15% cap
    }

    /// W7 regression: the rounded price must never exceed the change cap.
    #[test]
    fn rounding_never_exceeds_change_cap() {
        // current 1000, cap 15% → bounds [850, 1150]. Grid step 250: the only
        // candidate ≥ current is 1250 — outside bounds → no representable raise.
        let r = round_within_guards(
            1150.0,
            1000.0,
            true,
            850.0,
            1150.0,
            &PriceRoundingRule::EgyptianCafe,
        );
        assert_eq!(r, None);

        // current 2000, cap 15% → [1700, 2300]. Snap(2300)=2250 — inside, above.
        let r2 = round_within_guards(
            2300.0,
            2000.0,
            true,
            1700.0,
            2300.0,
            &PriceRoundingRule::EgyptianCafe,
        );
        assert_eq!(r2, Some(2250));
    }

    #[test]
    fn rounding_never_flips_direction() {
        // A "lower" from 2600 with bounds [2210, 2990]: snap(2550) on the
        // 500-grid is 2500 < current — fine, stays a cut.
        let r = round_within_guards(
            2550.0,
            2600.0,
            false,
            2210.0,
            2990.0,
            &PriceRoundingRule::EgyptianCafe,
        );
        assert_eq!(r, Some(2500));
        // A raise from 2400 to guarded 2450: snap = 2500 > current. OK.
        let r2 = round_within_guards(
            2450.0,
            2400.0,
            true,
            2040.0,
            2760.0,
            &PriceRoundingRule::EgyptianCafe,
        );
        assert_eq!(r2, Some(2500));
    }

    #[test]
    fn rounding_to_zero_demotes_to_hold() {
        // 1 EGP item: every grid point is 0 or 250 — both unusable for a cut.
        let r = round_within_guards(
            90.0,
            100.0,
            false,
            85.0,
            115.0,
            &PriceRoundingRule::EgyptianCafe,
        );
        assert_eq!(r, None);
    }

    #[test]
    fn below_threshold_demotes_to_hold() {
        // Steady item (revenue mode): raise capped to 5%; price 10000 → 10500,
        // snap → 10500 — fine. With price 100000, +5% = 105000 snaps to 105000.
        // Use a price where the snapped move is < 1.5%: price 25000 (250 EGP),
        // +5% → 26250 → snap(500) → 26000 = +4% → emitted. Make cap tiny via
        // config to force sub-threshold move.
        let mut config = AnalysisConfig::default();
        config.revenue_mode_max_raise_pct = 0.012; // below the 1.5% threshold
        let snaps = vec![snap(1, 100_000, None), snap(2, 200_000, None)];
        let sales = vec![sale(1, 100, 100_000, None), sale(2, 30, 200_000, None)];
        let out = run(&snaps, &sales, &config, &HashSet::new());
        let s = find(&out, 1); // Steady: popular, low price
        assert_eq!(s.action, Action::Hold);
        assert!(s.suggested_price.is_none());
        assert!(s.guard_clips.is_empty());
    }

    #[test]
    fn epoch_suppression_demotes_price_moves_only() {
        let snaps = vec![snap(1, 100_000, None), snap(2, 200_000, None)];
        let sales = vec![sale(1, 100, 100_000, None), sale(2, 30, 200_000, None)];
        let changed: HashSet<ItemKey> = [key(1)].into_iter().collect();
        let out = run(&snaps, &sales, &AnalysisConfig::default(), &changed);
        let s = find(&out, 1); // Steady → would raise, but suppressed
        assert_eq!(s.action, Action::Hold);
        assert!(s.price_changed_in_window);
        assert!(s.suggested_price.is_none());
        assert!(s.explanation.contains("Price changed recently"));
    }

    #[test]
    fn clips_empty_when_no_suggestion_emitted() {
        // Dog (food cost 44% < the 45% reformulate line) → Remove: the
        // action carries no price, so no clips either.
        let snaps = vec![
            snap(1, 1000, Some(300)),
            snap(2, 1000, Some(440)),
            snap(3, 1000, Some(310)),
            snap(4, 1000, Some(320)),
        ];
        let sales = vec![
            sale(1, 100, 1000, Some(300)),
            sale(2, 25, 1000, Some(440)),
            sale(3, 90, 1000, Some(310)),
            sale(4, 80, 1000, Some(320)),
        ];
        let out = run(&snaps, &sales, &AnalysisConfig::default(), &HashSet::new());
        let dog = find(&out, 2);
        assert_eq!(dog.action, Action::Remove);
        assert!(dog.suggested_price.is_none());
        assert!(dog.guard_clips.is_empty());
    }

    #[test]
    fn confidence_capped_medium_for_revenue_mode() {
        let snaps = vec![snap(1, 100_000, None), snap(2, 200_000, None)];
        // Plenty of volume (≥ 3× min) but revenue mode caps at Medium.
        let sales = vec![sale(1, 100, 100_000, None), sale(2, 90, 200_000, None)];
        let out = run(&snaps, &sales, &AnalysisConfig::default(), &HashSet::new());
        assert!(find(&out, 1).confidence <= Confidence::Medium);
    }

    #[test]
    fn confidence_capped_on_cost_volatility() {
        let mut sales = vec![
            sale(1, 80, 1000, Some(300)),
            sale(2, 80, 1000, Some(300)),
            sale(3, 80, 1000, Some(300)),
            sale(4, 80, 1000, Some(300)),
        ];
        sales.push(sale(1, 1, 1000, Some(450))); // +50% cost swing on item 1
        let snaps = vec![
            snap(1, 1000, Some(300)),
            snap(2, 1000, Some(300)),
            snap(3, 1000, Some(300)),
            snap(4, 1000, Some(300)),
        ];
        let out = run(&snaps, &sales, &AnalysisConfig::default(), &HashSet::new());
        assert!(find(&out, 1).confidence <= Confidence::Medium);
    }

    #[test]
    fn plowhorse_whatif_margin_formula() {
        // Plowhorse: popular (high share) + below-average cm.
        let snaps = vec![
            snap(1, 1000, Some(600)), // low margin, popular → Plowhorse
            snap(2, 1000, Some(200)), // high margin, unpopular → Puzzle
            snap(3, 1000, Some(190)),
            snap(4, 1000, Some(210)),
        ];
        let sales = vec![
            sale(1, 200, 1000, Some(600)),
            sale(2, 25, 1000, Some(200)),
            sale(3, 25, 1000, Some(190)),
            sale(4, 25, 1000, Some(210)),
        ];
        let out = run(&snaps, &sales, &AnalysisConfig::default(), &HashSet::new());
        let plow = find(&out, 1);
        assert!(matches!(
            plow.classification,
            Classification::Cm {
                quadrant: CmQuadrant::Plowhorse
            }
        ));
        let expected = (1000.0 - 600.0 * 0.9) / 1000.0;
        assert!((plow.cost_reduction_whatif_margin.unwrap() - expected).abs() < 1e-9);
    }

    #[test]
    fn peer_position_two_percent_band() {
        let snaps = vec![snap(1, 1000, None), snap(2, 1010, None), snap(3, 990, None)];
        let sales = vec![
            sale(1, 50, 1000, None),
            sale(2, 50, 1010, None),
            sale(3, 50, 990, None),
        ];
        let kpis = compute_item_kpis(&snaps, &sales, now(), &AnalysisConfig::default()).unwrap();
        let snap_map: HashMap<ItemKey, &ItemSnapshot> =
            snaps.iter().map(|s| (s.key.clone(), s)).collect();
        let pc = build_peer_comparison(&kpis[&key(1)], &kpis, &snap_map).unwrap();
        assert_eq!(pc.your_position, PeerPosition::At); // within ±2% of median 1000
    }
}
