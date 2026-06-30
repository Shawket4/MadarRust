//! Two-population classification (Kasavana-Smith for CM-tracked items, a
//! revenue analog for cost-missing ones), with hysteresis against the
//! previous run's classifications.
//!
//! The two populations never contaminate each other's thresholds: an item
//! with cost data is judged against the CM population's average CM, an item
//! without it against the revenue population's average price. The
//! Insufficient bucket catches everything below the unit threshold.

use std::collections::{HashMap, HashSet};

use super::kpi::ItemKpi;
use super::stats::{ratio_or, wilson_95_ci};
use crate::menu_advisor::dto::{Classification, CmQuadrant, ItemKey, RevenueClass};

/// Hold the previous quadrant side when within this relative distance of a
/// threshold — stops classification flapping between runs on noise.
const HYSTERESIS_BAND: f64 = 0.05;
/// An item this close to a threshold has a statistically unsettled
/// classification → confidence capped at Medium.
const BORDERLINE_BAND: f64 = 0.10;
/// Populations smaller than this classify degenerately (a singleton is
/// always a Star) → confidence capped at Medium.
const MIN_POPULATION_FOR_HIGH_CONFIDENCE: usize = 4;

pub(crate) struct ClassificationOutcome {
    pub map: HashMap<ItemKey, Classification>,
    /// Items whose classification is statistically unsettled (CI straddles
    /// the popularity threshold, or profit metric near its threshold).
    pub borderline: HashSet<ItemKey>,
    /// Items classified within a population of < 4 members.
    pub small_population: HashSet<ItemKey>,
}

pub(crate) fn classify_items(
    kpis: &HashMap<ItemKey, ItemKpi>,
    previous: Option<&HashMap<ItemKey, Classification>>,
) -> ClassificationOutcome {
    let mut map = HashMap::new();
    let mut borderline = HashSet::new();
    let mut small_population = HashSet::new();

    let (mut cm_eligible, mut rev_eligible): (Vec<&ItemKpi>, Vec<&ItemKpi>) = kpis
        .values()
        .filter(|k| k.sufficient)
        .partition(|k| k.cost_metrics.is_some());
    // Deterministic float reductions: population sums must not depend on
    // HashMap iteration order.
    cm_eligible.sort_by(|a, b| a.key.cmp(&b.key));
    rev_eligible.sort_by(|a, b| a.key.cmp(&b.key));

    classify_population(
        &cm_eligible,
        |k| k.cost_metrics.as_ref().map_or(0.0, |c| c.cm_per_unit),
        |prev| match prev {
            Classification::Cm { quadrant } => Some((
                matches!(quadrant, CmQuadrant::Star | CmQuadrant::Plowhorse),
                matches!(quadrant, CmQuadrant::Star | CmQuadrant::Puzzle),
            )),
            _ => None,
        },
        |high_pop, high_prof| Classification::Cm {
            quadrant: match (high_pop, high_prof) {
                (true, true) => CmQuadrant::Star,
                (true, false) => CmQuadrant::Plowhorse,
                (false, true) => CmQuadrant::Puzzle,
                (false, false) => CmQuadrant::Dog,
            },
        },
        previous,
        &mut map,
        &mut borderline,
        &mut small_population,
    );

    classify_population(
        &rev_eligible,
        |k| k.effective_price,
        |prev| match prev {
            Classification::Revenue { class } => Some((
                matches!(class, RevenueClass::Hero | RevenueClass::Steady),
                matches!(class, RevenueClass::Hero | RevenueClass::Slow),
            )),
            _ => None,
        },
        |high_pop, high_price| Classification::Revenue {
            class: match (high_pop, high_price) {
                (true, true) => RevenueClass::Hero,
                (true, false) => RevenueClass::Steady,
                (false, true) => RevenueClass::Slow,
                (false, false) => RevenueClass::Quiet,
            },
        },
        previous,
        &mut map,
        &mut borderline,
        &mut small_population,
    );

    for kpi in kpis.values() {
        if !kpi.sufficient {
            map.insert(kpi.key.clone(), Classification::Insufficient);
        }
    }

    ClassificationOutcome {
        map,
        borderline,
        small_population,
    }
}

/// Shared 2×2 classification over one population.
///
/// - Popularity axis: within-population weighted share against the 70% rule
///   threshold (`0.70 / n` — even shares would sit at `1/n`, so an item needs
///   ≥ 70% of its "fair share" to count as popular).
/// - Profit axis: `profit_metric` against the population's units-weighted mean.
#[allow(clippy::too_many_arguments)]
fn classify_population(
    population: &[&ItemKpi],
    profit_metric: impl Fn(&ItemKpi) -> f64,
    prev_sides: impl Fn(&Classification) -> Option<(bool, bool)>,
    build: impl Fn(bool, bool) -> Classification,
    previous: Option<&HashMap<ItemKey, Classification>>,
    map: &mut HashMap<ItemKey, Classification>,
    borderline: &mut HashSet<ItemKey>,
    small_population: &mut HashSet<ItemKey>,
) {
    if population.is_empty() {
        return;
    }
    let n = population.len() as f64;
    let pop_threshold = 0.70 / n;
    let total_w_units: f64 = population.iter().map(|k| k.weighted_units_sold).sum();
    let total_raw_units: f64 = population.iter().map(|k| k.raw_units_sold).sum();
    let profit_threshold = if total_w_units > 0.0 {
        population
            .iter()
            .map(|k| profit_metric(k) * k.weighted_units_sold)
            .sum::<f64>()
            / total_w_units
    } else {
        0.0
    };

    for kpi in population {
        let share = ratio_or(kpi.weighted_units_sold, total_w_units, 0.0);
        let profit = profit_metric(kpi);
        let mut high_pop = share >= pop_threshold;
        let mut high_prof = profit >= profit_threshold;

        // Hysteresis: hold the previous side when within 5% of a threshold.
        if let Some(prev) = previous.and_then(|m| m.get(&kpi.key))
            && let Some((prev_pop, prev_prof)) = prev_sides(prev)
        {
            let pop_dist = (share - pop_threshold).abs() / pop_threshold.max(1e-9);
            let prof_dist = (profit - profit_threshold).abs() / profit_threshold.abs().max(1e-9);
            if pop_dist < HYSTERESIS_BAND {
                high_pop = prev_pop;
            }
            if prof_dist < HYSTERESIS_BAND {
                high_prof = prev_prof;
            }
        }

        map.insert(kpi.key.clone(), build(high_pop, high_prof));

        // Borderline: the within-population raw-share CI straddles the
        // popularity threshold, or the profit metric is near its threshold.
        let raw_share = ratio_or(kpi.raw_units_sold, total_raw_units, 0.0);
        let ci = wilson_95_ci(raw_share, total_raw_units);
        let pop_unsettled = ci.lo < pop_threshold && pop_threshold < ci.hi;
        let prof_unsettled =
            (profit - profit_threshold).abs() / profit_threshold.abs().max(1e-9) < BORDERLINE_BAND;
        if pop_unsettled || prof_unsettled {
            borderline.insert(kpi.key.clone());
        }
        if population.len() < MIN_POPULATION_FOR_HIGH_CONFIDENCE {
            small_population.insert(kpi.key.clone());
        }
    }
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

    use super::super::kpi::compute_item_kpis;
    use super::super::{ItemSnapshot, SaleEvent};
    use super::*;
    use crate::menu_advisor::dto::AnalysisConfig;

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

    fn classify(
        snaps: &[ItemSnapshot],
        sales: &[SaleEvent],
        previous: Option<&HashMap<ItemKey, Classification>>,
    ) -> ClassificationOutcome {
        let kpis = compute_item_kpis(snaps, sales, now(), &AnalysisConfig::default()).unwrap();
        classify_items(&kpis, previous)
    }

    /// CM items are judged against CM thresholds, revenue items against
    /// revenue thresholds — never mixed.
    #[test]
    fn mixed_mode_classifies_each_population_separately() {
        let snaps = vec![
            snap(1, 1000, Some(300)), // CM, high volume + high cm
            snap(2, 1000, Some(800)), // CM, low volume + low cm
            snap(3, 2000, None),      // revenue, high volume + high price
            snap(4, 500, None),       // revenue, low volume + low price
        ];
        let sales = vec![
            sale(1, 100, 1000, Some(300)),
            sale(2, 25, 1000, Some(800)),
            sale(3, 100, 2000, None),
            sale(4, 25, 500, None),
        ];
        let out = classify(&snaps, &sales, None);
        assert_eq!(
            out.map[&key(1)],
            Classification::Cm {
                quadrant: CmQuadrant::Star
            }
        );
        assert_eq!(
            out.map[&key(2)],
            Classification::Cm {
                quadrant: CmQuadrant::Dog
            }
        );
        assert_eq!(
            out.map[&key(3)],
            Classification::Revenue {
                class: RevenueClass::Hero
            }
        );
        assert_eq!(
            out.map[&key(4)],
            Classification::Revenue {
                class: RevenueClass::Quiet
            }
        );
    }

    #[test]
    fn revenue_only_population_classifies_alone() {
        let snaps = vec![snap(1, 2000, None), snap(2, 500, None)];
        let sales = vec![sale(1, 100, 2000, None), sale(2, 25, 500, None)];
        let out = classify(&snaps, &sales, None);
        assert_eq!(
            out.map[&key(1)],
            Classification::Revenue {
                class: RevenueClass::Hero
            }
        );
        assert_eq!(
            out.map[&key(2)],
            Classification::Revenue {
                class: RevenueClass::Quiet
            }
        );
    }

    #[test]
    fn insufficient_bucket_complete() {
        let snaps = vec![snap(1, 1000, None), snap(2, 1000, None)];
        let sales = vec![sale(1, 100, 1000, None)]; // item 2: zero sales
        let out = classify(&snaps, &sales, None);
        assert_eq!(out.map.len(), 2);
        assert_eq!(out.map[&key(2)], Classification::Insufficient);
    }

    #[test]
    fn hysteresis_holds_previous_quadrant_within_band() {
        // Three CM items; item 1 sits just 2% above the popularity threshold.
        // pop_threshold = 0.70/3 ≈ 0.2333. Give item 1 share ≈ 0.238.
        let snaps = vec![
            snap(1, 1000, Some(500)),
            snap(2, 1000, Some(500)),
            snap(3, 1000, Some(500)),
        ];
        let sales = vec![
            sale(1, 238, 1000, Some(500)),
            sale(2, 500, 1000, Some(500)),
            sale(3, 262, 1000, Some(500)),
        ];
        // Without previous: share 0.238 ≥ 0.2333 → popular → Star (all same cm).
        let fresh = classify(&snaps, &sales, None);
        assert_eq!(
            fresh.map[&key(1)],
            Classification::Cm {
                quadrant: CmQuadrant::Star
            }
        );
        // With previous = Puzzle (unpopular side held): within 5% band → hold.
        let mut prev = HashMap::new();
        prev.insert(
            key(1),
            Classification::Cm {
                quadrant: CmQuadrant::Puzzle,
            },
        );
        let held = classify(&snaps, &sales, Some(&prev));
        assert_eq!(
            held.map[&key(1)],
            Classification::Cm {
                quadrant: CmQuadrant::Puzzle
            }
        );
    }

    #[test]
    fn hysteresis_ignored_outside_band() {
        let snaps = vec![
            snap(1, 1000, Some(500)),
            snap(2, 1000, Some(500)),
            snap(3, 1000, Some(500)),
        ];
        // Item 1 share = 0.5 — far above threshold; previous Puzzle must not hold.
        let sales = vec![
            sale(1, 500, 1000, Some(500)),
            sale(2, 300, 1000, Some(500)),
            sale(3, 200, 1000, Some(500)),
        ];
        let mut prev = HashMap::new();
        prev.insert(
            key(1),
            Classification::Cm {
                quadrant: CmQuadrant::Puzzle,
            },
        );
        let out = classify(&snaps, &sales, Some(&prev));
        assert_eq!(
            out.map[&key(1)],
            Classification::Cm {
                quadrant: CmQuadrant::Star
            }
        );
    }

    #[test]
    fn singleton_population_is_star_and_flagged_small() {
        let snaps = vec![snap(1, 1000, Some(300))];
        let sales = vec![sale(1, 50, 1000, Some(300))];
        let out = classify(&snaps, &sales, None);
        assert_eq!(
            out.map[&key(1)],
            Classification::Cm {
                quadrant: CmQuadrant::Star
            }
        );
        assert!(out.small_population.contains(&key(1)));
    }
}
