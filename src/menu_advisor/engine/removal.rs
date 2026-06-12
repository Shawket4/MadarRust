//! Removal scenarios for CM-tracked Dogs: who absorbs the demand, what
//! attached sales are lost, and whether removal is a robust win.
//!
//! Substitutes are same-category peers — co-occurrence is NOT required
//! (items never bought together are the purest substitutes; the old engine
//! excluded exactly those). Strong complements are excluded from the
//! substitute pool and instead modeled as losses: their excess co-occurrence
//! above independence, `pair_count × (1 − 1/lift)`, is the demand that
//! existed because of the removed item.

use std::collections::HashMap;

use crate::menu_advisor::dto::{
    AbsorbedBy, AnalysisConfig, ComplementaryLoss, ItemKey, RemovalRecommendation,
    RemovalScenario,
};
use super::bundles::{get_assoc, AssociationIndex};
use super::explain;
use super::kpi::ItemKpi;
use super::ItemSnapshot;

/// Share of the removed item's demand that shifts to substitutes at all —
/// a labeled prior, not a measurement.
const ABSORB_RATE: f64 = 0.60;

pub(crate) fn simulate_removal(
    target: &ItemKey,
    kpis: &HashMap<ItemKey, ItemKpi>,
    assoc: &AssociationIndex,
    snaps: &HashMap<ItemKey, &ItemSnapshot>,
    config: &AnalysisConfig,
) -> Option<RemovalScenario> {
    let target_kpi = kpis.get(target)?;
    let target_cm = target_kpi.cost_metrics.as_ref()?;
    if target_kpi.was_inactive {
        return None;
    }
    let target_snap = snaps.get(target)?;
    let item_name = target_snap.name.clone();

    // Window-total CM at raw units, for interpretability.
    let baseline_cm = target_cm.cm_per_unit * target_kpi.raw_units_sold;

    let is_strong_complement = |k: &ItemKey| {
        get_assoc(assoc, target, k)
            .is_some_and(|(a, _)| a.lift >= config.min_lift_for_bundle)
    };

    // ── Substitutes: same-category, active, standalone, sufficient peers ──
    let mut substitutes: Vec<(&ItemKey, f64)> = kpis
        .values()
        .filter(|k| {
            k.key != *target
                && k.sufficient
                && !k.was_inactive
                && snaps.get(&k.key).is_some_and(|s| {
                    s.is_active
                        && !s.bundle_only
                        && s.category_id == target_snap.category_id
                })
                && !is_strong_complement(&k.key)
        })
        .map(|k| (&k.key, k.raw_units_sold))
        .collect();
    substitutes.sort_by(|a, b| a.0.cmp(b.0));

    let total_sub_units: f64 = substitutes.iter().map(|(_, u)| u).sum();
    let mut absorbed_by = Vec::new();
    let mut total_recovered = 0.0;
    for (sub_key, units) in &substitutes {
        if total_sub_units <= 0.0 {
            break;
        }
        let absorbed_units = ABSORB_RATE * target_kpi.raw_units_sold * units / total_sub_units;
        // No CM data for the substitute → credit zero (conservative).
        let sub_cm_pu = kpis
            .get(*sub_key)
            .and_then(|k| k.cost_metrics.as_ref())
            .map_or(0.0, |cm| cm.cm_per_unit);
        let absorbed_cm = absorbed_units * sub_cm_pu;
        total_recovered += absorbed_cm;
        absorbed_by.push(AbsorbedBy {
            key: (*sub_key).clone(),
            absorbed_units,
            absorbed_cm,
        });
    }

    // ── Complementary losses: excess co-occurrence above independence ──
    let mut complement_keys: Vec<(&ItemKey, u64, f64)> = assoc
        .iter()
        .filter_map(|((a, b), pair_assoc)| {
            let other = if a == target {
                b
            } else if b == target {
                a
            } else {
                return None;
            };
            (pair_assoc.lift >= config.min_lift_for_bundle)
                .then_some((other, pair_assoc.pair_count, pair_assoc.lift))
        })
        .collect();
    complement_keys.sort_by(|a, b| a.0.cmp(b.0));

    let mut complementary_losses = Vec::new();
    let mut total_comp_loss = 0.0;
    for (other_key, pair_count, lift) in complement_keys {
        if lift <= 1.0 {
            continue;
        }
        let lost_units = pair_count as f64 * (1.0 - 1.0 / lift);
        let other_cm_pu = kpis
            .get(other_key)
            .and_then(|k| k.cost_metrics.as_ref())
            .map_or(0.0, |cm| cm.cm_per_unit);
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

    // Crosswise bounds: the pessimistic case halves recovery AND inflates
    // losses; the optimistic case does the reverse — so [lo, hi] actually
    // brackets the central estimate.
    let net = total_recovered - baseline_cm - total_comp_loss;
    let net_lo = 0.5 * total_recovered - baseline_cm - 1.5 * total_comp_loss;
    let net_hi = 1.5 * total_recovered - baseline_cm - 0.5 * total_comp_loss;

    let recommendation = if net_lo > 0.0 {
        // Removal wins even in the pessimistic case.
        RemovalRecommendation::Remove
    } else if total_comp_loss > 0.30 * baseline_cm.max(1e-9) {
        RemovalRecommendation::KeepAndBundle
    } else if target_cm.food_cost_pct > 0.45 {
        RemovalRecommendation::KeepAndReformulate
    } else {
        RemovalRecommendation::NoStrongSignal
    };

    let explanation = explain::removal(
        &item_name,
        baseline_cm,
        config.analysis_window_days,
        total_recovered,
        total_comp_loss,
        net,
        net_lo,
        net_hi,
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::super::bundles::compute_associations;
    use super::super::kpi::compute_item_kpis;
    use super::super::SaleEvent;
    use super::*;

    fn key(id: u8) -> ItemKey {
        ItemKey {
            menu_item_id: uuid::Uuid::from_u128(id as u128),
            size_label: "one_size".into(),
        }
    }

    fn snap(id: u8, price: i64, cost: Option<i64>, cat: u8) -> ItemSnapshot {
        ItemSnapshot {
            key: key(id),
            category_id: Some(uuid::Uuid::from_u128(1000 + cat as u128)),
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

    fn simulate(
        target_id: u8,
        snaps: &[ItemSnapshot],
        sales: &[SaleEvent],
        baskets: &[Vec<ItemKey>],
    ) -> Option<RemovalScenario> {
        let config = AnalysisConfig::default();
        let kpis = compute_item_kpis(snaps, sales, now(), &config).unwrap();
        let assoc = compute_associations(baskets);
        let snap_map: HashMap<ItemKey, &ItemSnapshot> =
            snaps.iter().map(|s| (s.key.clone(), s)).collect();
        simulate_removal(&key(target_id), &kpis, &assoc, &snap_map, &config)
    }

    #[test]
    fn revenue_only_items_are_skipped() {
        let snaps = vec![snap(1, 1000, None, 1)];
        let sales = vec![sale(1, 30, 1000, None)];
        assert!(simulate(1, &snaps, &sales, &[]).is_none());
    }

    /// W9 regression: a same-category peer that NEVER co-occurs with the
    /// target must still count as a substitute.
    #[test]
    fn never_cobought_same_category_peer_is_substitute() {
        let snaps = vec![
            snap(1, 1000, Some(700), 1), // target dog
            snap(2, 1000, Some(300), 1), // same-category peer, never co-bought
        ];
        let sales = vec![sale(1, 30, 1000, Some(700)), sale(2, 60, 1000, Some(300))];
        // No basket ever contains both.
        let baskets: Vec<Vec<ItemKey>> = std::iter::repeat_n(vec![key(1)], 30)
            .chain(std::iter::repeat_n(vec![key(2)], 60))
            .collect();
        let s = simulate(1, &snaps, &sales, &baskets).unwrap();
        assert_eq!(s.absorbed_by.len(), 1);
        assert_eq!(s.absorbed_by[0].key, key(2));
        // 0.60 × 30 units, all to the single substitute.
        assert!((s.absorbed_by[0].absorbed_units - 18.0).abs() < 1e-9);
        assert!((s.absorbed_by[0].absorbed_cm - 18.0 * 700.0).abs() < 1e-9);
    }

    #[test]
    fn strong_complements_excluded_from_substitutes_and_counted_as_losses() {
        let snaps = vec![
            snap(1, 1000, Some(700), 1),
            snap(2, 1000, Some(300), 1), // complement (high lift), same category
            snap(3, 1000, Some(400), 1), // substitute
        ];
        let sales = vec![
            sale(1, 30, 1000, Some(700)),
            sale(2, 40, 1000, Some(300)),
            sale(3, 60, 1000, Some(400)),
        ];
        // 100 baskets: 25 contain {1,2} (lift = 0.25/(0.3×0.4) ≈ 2.08),
        // item 1 alone ×5, item 2 alone ×15, item 3 alone ×55.
        let mut baskets: Vec<Vec<ItemKey>> = Vec::new();
        baskets.extend(std::iter::repeat_n(vec![key(1), key(2)], 25));
        baskets.extend(std::iter::repeat_n(vec![key(1)], 5));
        baskets.extend(std::iter::repeat_n(vec![key(2)], 15));
        baskets.extend(std::iter::repeat_n(vec![key(3)], 55));
        let s = simulate(1, &snaps, &sales, &baskets).unwrap();
        // Item 2 is a complement → not in absorbed_by; item 3 is.
        assert!(s.absorbed_by.iter().all(|a| a.key != key(2)));
        assert!(s.absorbed_by.iter().any(|a| a.key == key(3)));
        assert!(s.complementary_losses.iter().any(|l| l.key == key(2)));
    }

    /// W6 regression: complementary loss is the EXCESS co-occurrence,
    /// `pair_count × (1 − 1/lift)` — bounded by actual co-purchases.
    #[test]
    fn complementary_loss_is_excess_cooccurrence() {
        let snaps = vec![
            snap(1, 1000, Some(700), 1),
            snap(2, 1000, Some(300), 2), // complement in another category
        ];
        let sales = vec![sale(1, 30, 1000, Some(700)), sale(2, 40, 1000, Some(300))];
        let mut baskets: Vec<Vec<ItemKey>> = Vec::new();
        baskets.extend(std::iter::repeat_n(vec![key(1), key(2)], 20));
        baskets.extend(std::iter::repeat_n(vec![key(1)], 10));
        baskets.extend(std::iter::repeat_n(vec![key(2)], 20));
        baskets.extend(std::iter::repeat_n(vec![key(99)], 50));
        // support_1 = 0.3, support_2 = 0.4, support_12 = 0.2 → lift = 1.667.
        let s = simulate(1, &snaps, &sales, &baskets).unwrap();
        let loss = s.complementary_losses.iter().find(|l| l.key == key(2)).unwrap();
        let expected_units = 20.0 * (1.0 - 1.0 / (0.2 / (0.3 * 0.4)));
        assert!((loss.lost_units - expected_units).abs() < 1e-6);
        assert!(loss.lost_units <= 20.0); // never exceeds actual co-purchases
        assert!((loss.lost_cm - expected_units * 700.0).abs() < 1e-6);
    }

    /// W8 regression: lo ≤ net ≤ hi with both terms varied crosswise.
    #[test]
    fn bounds_bracket_net() {
        let snaps = vec![
            snap(1, 1000, Some(700), 1),
            snap(2, 1000, Some(300), 1),
            snap(3, 1000, Some(400), 2),
        ];
        let sales = vec![
            sale(1, 30, 1000, Some(700)),
            sale(2, 60, 1000, Some(300)),
            sale(3, 40, 1000, Some(400)),
        ];
        let mut baskets: Vec<Vec<ItemKey>> = Vec::new();
        baskets.extend(std::iter::repeat_n(vec![key(1), key(3)], 20));
        baskets.extend(std::iter::repeat_n(vec![key(1)], 10));
        baskets.extend(std::iter::repeat_n(vec![key(2)], 60));
        baskets.extend(std::iter::repeat_n(vec![key(3)], 20));
        let s = simulate(1, &snaps, &sales, &baskets).unwrap();
        assert!(s.net_cm_change_lo <= s.net_cm_change);
        assert!(s.net_cm_change <= s.net_cm_change_hi);
        assert!(s.net_cm_change_lo < s.net_cm_change_hi);
    }

    #[test]
    fn remove_requires_robust_positive_lo() {
        // Rich substitute, weak target: removal is a robust win.
        let snaps = vec![
            snap(1, 1000, Some(950), 1), // target: cm 50/unit
            snap(2, 1000, Some(100), 1), // substitute: cm 900/unit
        ];
        let sales = vec![sale(1, 30, 1000, Some(950)), sale(2, 90, 1000, Some(100))];
        let baskets: Vec<Vec<ItemKey>> = std::iter::repeat_n(vec![key(1)], 30)
            .chain(std::iter::repeat_n(vec![key(2)], 90))
            .collect();
        let s = simulate(1, &snaps, &sales, &baskets).unwrap();
        // baseline = 50×30 = 1500; recovered = 18×900 = 16200; lo = 8100−1500 > 0.
        assert!(s.net_cm_change_lo > 0.0);
        assert_eq!(s.recommendation, RemovalRecommendation::Remove);
    }

    #[test]
    fn no_substitutes_means_full_baseline_loss() {
        let snaps = vec![snap(1, 1000, Some(700), 1)];
        let sales = vec![sale(1, 30, 1000, Some(700))];
        let s = simulate(1, &snaps, &sales, &[]).unwrap();
        assert!(s.absorbed_by.is_empty());
        assert!((s.net_cm_change - (-9000.0)).abs() < 1e-9); // −baseline
        assert_ne!(s.recommendation, RemovalRecommendation::Remove);
    }

    #[test]
    fn reformulate_when_food_cost_high_and_no_better_signal() {
        let snaps = vec![snap(1, 1000, Some(700), 1)]; // food cost 70%
        let sales = vec![sale(1, 30, 1000, Some(700))];
        let s = simulate(1, &snaps, &sales, &[]).unwrap();
        assert_eq!(s.recommendation, RemovalRecommendation::KeepAndReformulate);
    }
}
