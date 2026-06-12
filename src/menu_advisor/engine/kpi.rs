//! Per-SKU KPI computation: recency-weighted volume/revenue, effective
//! price, popularity, and cost-optional margin metrics.
//!
//! The cost accounting rule (the old engine got this wrong): effective cost
//! is averaged over cost-KNOWN sales only, and every margin figure derives
//! from `effective_price − effective_cost`. Revenue from cost-unknown sales
//! is never paired with a partial cost sum, so partial cost coverage cannot
//! inflate the contribution margin.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::menu_advisor::dto::{AnalysisConfig, ItemKey};
use super::stats::{ratio_or, recency_weight};
use super::{EngineError, ItemSnapshot, SaleEvent};

#[derive(Debug, Clone)]
pub(crate) struct ItemKpi {
    pub key: ItemKey,
    /// Whether raw unit volume crosses the classification threshold.
    pub sufficient: bool,
    /// Was the item inactive in this window despite having sales?
    pub was_inactive: bool,
    pub current_price: i64,

    // Volume + revenue metrics — always meaningful.
    pub raw_units_sold: f64,
    pub weighted_units_sold: f64,
    pub effective_price: f64,
    pub popularity_share: f64,

    /// `Some` ⟺ cost is known for this item (sale-time samples or a current
    /// recipe rollup). Margin math is impossible by construction when `None`.
    pub cost_metrics: Option<CostMetrics>,
}

#[derive(Debug, Clone)]
pub(crate) struct CostMetrics {
    /// Weighted average unit cost over cost-known sales; falls back to the
    /// snapshot rollup when no sale carried a cost.
    pub effective_cost: f64,
    pub cm_per_unit: f64,
    pub margin_pct: f64,
    pub food_cost_pct: f64,
    /// Did the sale-time cost move >25% inside the window?
    pub cost_volatility_high: bool,
}

pub(crate) fn compute_item_kpis(
    snapshots: &[ItemSnapshot],
    sales: &[SaleEvent],
    now: DateTime<Utc>,
    config: &AnalysisConfig,
) -> Result<HashMap<ItemKey, ItemKpi>, EngineError> {
    if snapshots.is_empty() {
        return Err(EngineError::NoItems);
    }

    #[derive(Default)]
    struct Acc {
        raw_units: f64,
        w_units: f64,
        w_revenue: f64,
        // Cost-known sales only.
        w_cost: f64,
        w_cost_units: f64,
        cost_min: Option<f64>,
        cost_max: Option<f64>,
    }
    let mut accs: HashMap<ItemKey, Acc> = HashMap::new();

    for sale in sales {
        let age_days = ((now - sale.sold_at).num_seconds() as f64 / 86_400.0).max(0.0);
        let w = recency_weight(age_days, config.recency_half_life_days);
        let qty = sale.quantity_sold as f64;

        let acc = accs.entry(sale.key.clone()).or_default();
        acc.raw_units += qty;
        acc.w_units += w * qty;
        acc.w_revenue += w * qty * sale.unit_price_paid as f64;

        if let Some(uc) = sale.unit_cost_at_sale {
            let c = uc as f64;
            acc.w_cost += w * qty * c;
            acc.w_cost_units += w * qty;
            acc.cost_min = Some(acc.cost_min.map_or(c, |m| m.min(c)));
            acc.cost_max = Some(acc.cost_max.map_or(c, |m| m.max(c)));
        }
    }

    // Popularity denominator: active, non-bundle-only SKUs. Summed in
    // snapshot order (NOT HashMap order) so float addition is deterministic.
    let total_w_units: f64 = snapshots
        .iter()
        .filter(|s| s.is_active && !s.bundle_only)
        .filter_map(|s| accs.get(&s.key).map(|a| a.w_units))
        .sum();

    let mut result: HashMap<ItemKey, ItemKpi> = HashMap::new();

    for snap in snapshots {
        let acc = accs.get(&snap.key);
        let raw_units = acc.map_or(0.0, |a| a.raw_units);
        let sufficient = raw_units >= config.min_units_for_classification;
        let (w_units, w_revenue) = acc.map_or((0.0, 0.0), |a| (a.w_units, a.w_revenue));

        let effective_price = if w_units > 0.0 {
            ratio_or(w_revenue, w_units, snap.current_price as f64)
        } else {
            snap.current_price as f64
        };

        let popularity_share = if snap.is_active && !snap.bundle_only {
            ratio_or(w_units, total_w_units, 0.0)
        } else {
            0.0
        };

        // Effective cost: sale-time samples first, snapshot rollup second.
        let effective_cost = match acc {
            Some(a) if a.w_cost_units > 0.0 => Some(ratio_or(
                a.w_cost,
                a.w_cost_units,
                snap.cost_per_serving.map_or(0.0, |c| c as f64),
            )),
            _ => snap.cost_per_serving.map(|c| c as f64),
        };

        let cost_metrics = effective_cost.map(|eff_cost| {
            let cm_per_unit = effective_price - eff_cost;
            let cost_volatility_high = acc
                .and_then(|a| a.cost_min.zip(a.cost_max))
                .is_some_and(|(lo, hi)| lo > 0.0 && (hi - lo) / lo > 0.25);
            CostMetrics {
                effective_cost: eff_cost,
                cm_per_unit,
                margin_pct: ratio_or(cm_per_unit, effective_price, 0.0),
                food_cost_pct: ratio_or(eff_cost, effective_price, 1.0),
                cost_volatility_high,
            }
        });

        result.insert(
            snap.key.clone(),
            ItemKpi {
                key: snap.key.clone(),
                sufficient,
                was_inactive: !snap.is_active && raw_units > 0.0,
                current_price: snap.current_price,
                raw_units_sold: raw_units,
                weighted_units_sold: w_units,
                effective_price,
                popularity_share,
                cost_metrics,
            },
        );
    }

    Ok(result)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use chrono::{Duration, TimeZone};

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

    fn now() -> DateTime<Utc> {
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

    #[test]
    fn no_items_errors() {
        let r = compute_item_kpis(&[], &[], now(), &AnalysisConfig::default());
        assert!(matches!(r, Err(EngineError::NoItems)));
    }

    #[test]
    fn effective_price_is_weighted_paid_price() {
        let snaps = vec![snap(1, 1000, None)];
        // Same-day sales: weights equal, so plain average of paid prices.
        let sales = vec![sale(1, 1, 800, None, 0), sale(1, 1, 1200, None, 0)];
        let kpis = compute_item_kpis(&snaps, &sales, now(), &AnalysisConfig::default()).unwrap();
        let k = &kpis[&key(1)];
        assert!((k.effective_price - 1000.0).abs() < 1e-9);
        assert_eq!(k.raw_units_sold, 2.0);
    }

    /// W1 regression: one costed + one uncosted sale must yield
    /// cm = price − mean(costed costs), NOT price − (cost_sum / all_units).
    #[test]
    fn partial_cost_coverage_does_not_inflate_cm() {
        let snaps = vec![snap(1, 1000, Some(400))];
        let sales = vec![
            sale(1, 1, 1000, Some(400), 0),
            sale(1, 1, 1000, None, 0), // cost unknown for this line
        ];
        let kpis = compute_item_kpis(&snaps, &sales, now(), &AnalysisConfig::default()).unwrap();
        let cm = kpis[&key(1)].cost_metrics.as_ref().unwrap();
        // effective_cost = 400 (from the one costed sale), NOT 200 — the old
        // engine divided the partial cost sum by ALL units, inflating CM.
        assert!((cm.effective_cost - 400.0).abs() < 1e-9);
        assert!((cm.cm_per_unit - 600.0).abs() < 1e-9);
        assert!((cm.margin_pct - 0.6).abs() < 1e-9);
    }

    #[test]
    fn static_cost_without_samples_uses_list_metrics() {
        let snaps = vec![snap(1, 1000, Some(300))];
        let kpis = compute_item_kpis(&snaps, &[], now(), &AnalysisConfig::default()).unwrap();
        let k = &kpis[&key(1)];
        let cm = k.cost_metrics.as_ref().unwrap();
        assert!((cm.effective_cost - 300.0).abs() < 1e-9);
        assert!((cm.cm_per_unit - 700.0).abs() < 1e-9);
        assert!((cm.margin_pct - 0.7).abs() < 1e-9);
        assert!(!k.sufficient);
    }

    #[test]
    fn no_cost_anywhere_means_no_cost_metrics() {
        let snaps = vec![snap(1, 1000, None)];
        let sales = vec![sale(1, 5, 1000, None, 1)];
        let kpis = compute_item_kpis(&snaps, &sales, now(), &AnalysisConfig::default()).unwrap();
        assert!(kpis[&key(1)].cost_metrics.is_none());
    }

    #[test]
    fn bundle_only_items_excluded_from_popularity_denominator() {
        let mut bundle_snap = snap(2, 500, None);
        bundle_snap.bundle_only = true;
        let snaps = vec![snap(1, 1000, None), bundle_snap];
        let sales = vec![sale(1, 10, 1000, None, 0)];
        let kpis = compute_item_kpis(&snaps, &sales, now(), &AnalysisConfig::default()).unwrap();
        assert!((kpis[&key(1)].popularity_share - 1.0).abs() < 1e-9);
        assert_eq!(kpis[&key(2)].popularity_share, 0.0);
    }

    #[test]
    fn cost_volatility_flag_set_above_25pct() {
        let snaps = vec![snap(1, 1000, Some(400))];
        let sales = vec![
            sale(1, 1, 1000, Some(400), 10),
            sale(1, 1, 1000, Some(520), 1), // +30% swing
        ];
        let kpis = compute_item_kpis(&snaps, &sales, now(), &AnalysisConfig::default()).unwrap();
        assert!(kpis[&key(1)].cost_metrics.as_ref().unwrap().cost_volatility_high);
    }
}
