//! Engine integration tests: the frozen wire contract, determinism, and
//! pathological-input robustness.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::{HashMap, HashSet};

use chrono::{Duration, TimeZone, Utc};
use uuid::Uuid;

use crate::menu_advisor::dto::{
    Action, AnalysisConfig, BundleAssociation, BundleForecast, BundleItemPair,
    BundleSuggestion, Classification, CmQuadrant, Confidence, GuardClip, ItemKey,
    PeerComparison, PeerPosition, PriceAnchors, PriceSuggestion, RemovalRecommendation,
    Triplet,
};
use super::{run_advisor, EngineError, ItemSnapshot, SaleEvent};

fn key(id: u8) -> ItemKey {
    ItemKey { menu_item_id: Uuid::from_u128(id as u128), size_label: "one_size".into() }
}

fn snap(id: u8, price: i64, cost: Option<i64>) -> ItemSnapshot {
    ItemSnapshot {
        key: key(id),
        category_id: Some(Uuid::from_u128(9999)),
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

fn sale(id: u8, qty: i64, price: i64, cost: Option<i64>, days_ago: i64) -> SaleEvent {
    SaleEvent {
        key: key(id),
        quantity_sold: qty,
        unit_price_paid: price,
        unit_cost_at_sale: cost,
        sold_at: now() - Duration::days(days_ago),
    }
}

/// The wire-shape regression net: serialize fully-populated suggestion
/// bodies and pin the exact key sets and tag values the dashboard parses.
#[test]
fn frozen_contract_json_shapes() {
    let suggestion = PriceSuggestion {
        key: key(1),
        item_name: "Latte".into(),
        classification: Classification::Cm { quadrant: CmQuadrant::Star },
        current_price: 4500,
        units_sold_raw: 120.0,
        effective_price: 4450.0,
        popularity_share: 0.21,
        cm_per_unit: Some(2400.0),
        margin_pct: Some(0.54),
        food_cost_pct: Some(0.46),
        anchors: PriceAnchors {
            cost_plus: Some(6833.0),
            peer_median: 5000.0,
            status_quo: 4500.0,
        },
        suggested_price: Some(5000),
        suggested_delta_abs: Some(500),
        suggested_delta_pct: Some(0.111),
        action: Action::RaisePrice,
        confidence: Confidence::High,
        explanation: "x".into(),
        guard_clips: vec![GuardClip::MarginFloor, GuardClip::CulturalRounding],
        peer_comparison: Some(PeerComparison {
            same_category_count: 4,
            median_effective_price_peers: 5000.0,
            median_margin_pct_peers: Some(0.5),
            median_cm_per_unit_peers: Some(2500.0),
            your_position: PeerPosition::Below,
        }),
        price_changed_in_window: false,
        cost_reduction_whatif_margin: None,
        cost_missing: false,
    };
    let v = serde_json::to_value(&suggestion).unwrap();
    assert_eq!(v["classification"]["mode"], "cm");
    assert_eq!(v["classification"]["quadrant"], "star");
    assert_eq!(v["action"], "raise_price");
    assert_eq!(v["confidence"], "high");
    assert_eq!(v["guard_clips"], serde_json::json!(["margin_floor", "cultural_rounding"]));
    assert_eq!(v["peer_comparison"]["your_position"], "below");
    assert_eq!(v["key"]["size_label"], "one_size");
    let keys: HashSet<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    for expected in [
        "key", "item_name", "classification", "current_price", "units_sold_raw",
        "effective_price", "popularity_share", "cm_per_unit", "margin_pct",
        "food_cost_pct", "anchors", "suggested_price", "suggested_delta_abs",
        "suggested_delta_pct", "action", "confidence", "explanation", "guard_clips",
        "peer_comparison", "price_changed_in_window", "cost_reduction_whatif_margin",
        "cost_missing",
    ] {
        assert!(keys.contains(expected), "missing key {expected}");
    }
    assert_eq!(keys.len(), 22, "unexpected extra keys in PriceSuggestion");

    // Revenue + insufficient classification tags.
    assert_eq!(
        serde_json::to_value(Classification::Revenue {
            class: crate::menu_advisor::dto::RevenueClass::Hero
        })
        .unwrap(),
        serde_json::json!({"mode": "revenue", "class": "hero"})
    );
    assert_eq!(
        serde_json::to_value(Classification::Insufficient).unwrap(),
        serde_json::json!({"mode": "insufficient"})
    );

    let bundle = BundleSuggestion {
        focus_item: key(1),
        bundle_items: vec![key(1), key(2)],
        bundle_list_price: 10_000,
        bundle_suggested_price: 9000,
        bundle_discount_pct: 0.10,
        bundle_cost: Some(3000),
        bundle_cm: Some(6000),
        bundle_margin_pct: Some(0.667),
        association: BundleAssociation {
            pair_lifts: vec![BundleItemPair {
                item_a: key(1),
                item_b: key(2),
                lift: 1.8,
                support: 0.12,
                confidence_ab: 0.6,
            }],
            composite_score: 0.27,
        },
        forecast: BundleForecast {
            expected_velocity: Triplet { lo: 0.5, mid: 0.625, hi: 0.9375 },
            inside_bundle_units_x: 18.75,
            halo_units_x: 2.39,
            total_units_uplift_x: 21.14,
            incremental_cm: Some(Triplet { lo: -300.0, mid: 500.0, hi: 1400.0 }),
        },
        guard_clips: vec![],
        explanation: "x".into(),
        missing_costs: false,
    };
    let v = serde_json::to_value(&bundle).unwrap();
    let keys: HashSet<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    for expected in [
        "focus_item", "bundle_items", "bundle_list_price", "bundle_suggested_price",
        "bundle_discount_pct", "bundle_cost", "bundle_cm", "bundle_margin_pct",
        "association", "forecast", "guard_clips", "explanation", "missing_costs",
    ] {
        assert!(keys.contains(expected), "missing key {expected}");
    }
    assert!(v["forecast"]["expected_velocity"]["mid"].is_number());
    assert_eq!(
        serde_json::to_value(RemovalRecommendation::KeepAndBundle).unwrap(),
        serde_json::json!("keep_and_bundle")
    );

    // AnalysisConfig: tuple range serializes as [lo, hi]; rounding rule PascalCase.
    let cfg = serde_json::to_value(AnalysisConfig::default()).unwrap();
    assert_eq!(cfg["bundle_discount_pct_range"], serde_json::json!([0.10, 0.25]));
    assert_eq!(cfg["price_rounding_rule"], "EgyptianCafe");
    // Partial configs deserialize with defaults.
    let partial: AnalysisConfig =
        serde_json::from_value(serde_json::json!({"analysis_window_days": 60.0})).unwrap();
    assert_eq!(partial.analysis_window_days, 60.0);
    assert_eq!(partial.min_units_for_classification, 20.0);
}

fn scenario() -> (Vec<ItemSnapshot>, Vec<SaleEvent>, Vec<Vec<ItemKey>>) {
    let snaps = vec![
        snap(1, 4500, Some(1500)), // popular, profitable → Star
        snap(2, 4000, Some(3200)), // unpopular, low margin → Dog
        snap(3, 6000, Some(1200)), // unpopular, profitable → Puzzle
        snap(4, 3500, Some(2000)), // popular, low margin → Plowhorse
        snap(5, 5000, None),       // revenue-only
        snap(6, 2000, None),       // revenue-only
        snap(7, 1000, Some(400)),  // 3 units → Insufficient
    ];
    let sales = vec![
        sale(1, 120, 4500, Some(1500), 3),
        sale(2, 25, 4000, Some(3200), 5),
        sale(3, 30, 6000, Some(1200), 2),
        sale(4, 110, 3500, Some(2000), 4),
        sale(5, 80, 5000, None, 1),
        sale(6, 40, 2000, None, 6),
        sale(7, 3, 1000, Some(400), 2),
    ];
    let mut baskets: Vec<Vec<ItemKey>> = Vec::new();
    baskets.extend(std::iter::repeat_n(vec![key(3), key(1)], 14)); // puzzle + star co-buys
    baskets.extend(std::iter::repeat_n(vec![key(1)], 80));
    baskets.extend(std::iter::repeat_n(vec![key(4)], 90));
    baskets.extend(std::iter::repeat_n(vec![key(2)], 20));
    baskets.extend(std::iter::repeat_n(vec![key(5), key(6)], 30));
    (snaps, sales, baskets)
}

#[test]
fn full_run_deterministic() {
    let (snaps, sales, baskets) = scenario();
    let config = AnalysisConfig::default();
    let changed = HashSet::new();
    let r1 = run_advisor(&snaps, &sales, &baskets, now(), &config, None, &changed).unwrap();
    let r2 = run_advisor(&snaps, &sales, &baskets, now(), &config, None, &changed).unwrap();
    assert_eq!(
        serde_json::to_string(&r1).unwrap(),
        serde_json::to_string(&r2).unwrap(),
        "same inputs must produce a byte-identical report"
    );
}

#[test]
fn mode_summary_counts_sum_to_total() {
    let (snaps, sales, baskets) = scenario();
    let r = run_advisor(
        &snaps, &sales, &baskets, now(), &AnalysisConfig::default(), None, &HashSet::new(),
    )
    .unwrap();
    let s = r.mode_summary;
    assert_eq!(s.items_total, 7);
    assert_eq!(s.items_cm_tracked + s.items_revenue_only + s.items_insufficient, s.items_total);
    assert_eq!(s.items_cm_tracked, 4);
    assert_eq!(s.items_revenue_only, 2);
    assert_eq!(s.items_insufficient, 1);
    assert_eq!(r.price_suggestions.len(), 7);
}

#[test]
fn only_cm_dogs_get_removal_scenarios() {
    let (snaps, sales, baskets) = scenario();
    let r = run_advisor(
        &snaps, &sales, &baskets, now(), &AnalysisConfig::default(), None, &HashSet::new(),
    )
    .unwrap();
    for sc in &r.removal_scenarios {
        let ps = r.price_suggestions.iter().find(|p| p.key == sc.key).unwrap();
        assert!(matches!(
            ps.classification,
            Classification::Cm { quadrant: CmQuadrant::Dog }
        ));
    }
    assert!(r.removal_scenarios.iter().any(|sc| sc.key == key(2)));
}

#[test]
fn empty_sales_yields_all_insufficient_monitor() {
    let snaps = vec![snap(1, 1000, Some(300)), snap(2, 2000, None)];
    let r = run_advisor(
        &snaps, &[], &[], now(), &AnalysisConfig::default(), None, &HashSet::new(),
    )
    .unwrap();
    assert_eq!(r.mode_summary.items_insufficient, 2);
    for s in &r.price_suggestions {
        assert_eq!(s.action, Action::Monitor);
        assert!(s.suggested_price.is_none());
    }
    assert!(r.bundle_suggestions.is_empty());
    assert!(r.removal_scenarios.is_empty());
}

#[test]
fn no_items_is_an_error() {
    let r = run_advisor(
        &[], &[], &[], now(), &AnalysisConfig::default(), None, &HashSet::new(),
    );
    assert!(matches!(r, Err(EngineError::NoItems)));
}

/// W16: pathological inputs must produce a finite report, not NaN-in-JSON.
#[test]
fn pathological_inputs_produce_finite_report() {
    let cases: Vec<(Vec<ItemSnapshot>, Vec<SaleEvent>, Vec<Vec<ItemKey>>)> = vec![
        // All prices zero.
        (
            vec![snap(1, 0, Some(0)), snap(2, 0, None)],
            vec![sale(1, 50, 0, Some(0), 1), sale(2, 50, 0, None, 1)],
            vec![vec![key(1), key(2)]; 20],
        ),
        // Negative CM everywhere (cost above price).
        (
            vec![snap(1, 100, Some(900)), snap(2, 100, Some(800))],
            vec![sale(1, 60, 100, Some(900), 1), sale(2, 60, 100, Some(800), 1)],
            std::iter::repeat_n(vec![key(1), key(2)], 30).collect(),
        ),
        // Single item, no baskets.
        (vec![snap(1, 1000, Some(300))], vec![sale(1, 100, 1000, Some(300), 0)], vec![]),
        // Item with sales but zero-price sales mixed in.
        (
            vec![snap(1, 1000, Some(300)), snap(2, 1000, Some(300))],
            vec![
                sale(1, 30, 0, Some(300), 1),
                sale(1, 30, 1000, Some(300), 1),
                sale(2, 60, 1000, Some(300), 1),
            ],
            vec![],
        ),
    ];
    for (i, (snaps, sales, baskets)) in cases.iter().enumerate() {
        let r = run_advisor(
            snaps, sales, baskets, now(), &AnalysisConfig::default(), None, &HashSet::new(),
        );
        let report = r.unwrap_or_else(|e| panic!("case {i} failed: {e}"));
        // validate_report ran inside run_advisor; double-check via JSON: no nulls
        // where numbers belong is implied by validate, just ensure it serializes.
        let _ = serde_json::to_string(&report).unwrap();
    }
}

/// Hysteresis is wired through run_advisor's `previous` argument.
#[test]
fn previous_classifications_flow_through() {
    // Borderline popularity: hold the previous quadrant.
    let snaps = vec![
        snap(1, 1000, Some(500)),
        snap(2, 1000, Some(500)),
        snap(3, 1000, Some(500)),
    ];
    let sales = vec![
        sale(1, 238, 1000, Some(500), 0),
        sale(2, 500, 1000, Some(500), 0),
        sale(3, 262, 1000, Some(500), 0),
    ];
    let mut prev = HashMap::new();
    prev.insert(key(1), Classification::Cm { quadrant: CmQuadrant::Puzzle });
    let r = run_advisor(
        &snaps, &sales, &[], now(), &AnalysisConfig::default(), Some(&prev), &HashSet::new(),
    )
    .unwrap();
    let s = r.price_suggestions.iter().find(|s| s.key == key(1)).unwrap();
    assert!(matches!(
        s.classification,
        Classification::Cm { quadrant: CmQuadrant::Puzzle }
    ));
}
