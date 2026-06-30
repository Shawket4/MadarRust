//! Pure line-cost rollup math, factored out of order creation so it can be
//! unit-tested and fuzzed in isolation (no DB, no IO).
//!
//! `InventoryDeduction` here is the *enriched* deduction (carrying resolved
//! `cost_per_unit` / `line_cost`), distinct from the raw resolver output in
//! [`crate::orders::component_resolve`]. It is serialized verbatim into
//! `order_items.deductions_snapshot`, so its field names / serde attributes are
//! part of the stored snapshot contract — do not rename without a migration.

use serde::Serialize;
use uuid::Uuid;

#[derive(Serialize, Clone)]
pub struct InventoryDeduction {
    pub org_ingredient_id: Option<Uuid>,
    pub ingredient_name: String,
    pub unit: String,
    pub quantity: f64,
    pub source: String, // "drink_recipe" | "addon" | "addon_swap:<name>" | "optional" | "bundle_component:<name>"
    pub category: String,
    /// Additive-addon attribution (None for recipe/swap/optional entries).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub addon_item_id: Option<Uuid>,
    /// Optional-field attribution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optional_field_id: Option<Uuid>,
    /// Bundle-component attribution (which component this entry belongs to).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component_item_id: Option<Uuid>,
    /// Piastre cost per ingredient unit at sale time. None ⟺ unknown.
    pub cost_per_unit: Option<f64>,
    /// quantity × cost_per_unit in piastres, rounded. None ⟺ unknown.
    pub line_cost: Option<i64>,
}

pub struct LineCostSummary {
    pub line_cost: Option<i64>,
    pub unit_cost: Option<i64>,
    pub cost_missing: bool,
}

/// Roll a line's enriched deductions up into the stored cost columns.
///
/// * `line_cost` — full COGS in piastres; `None` when anything is unknown.
/// * `unit_cost` — recipe-scope (drink_recipe + addon_swap) cost ÷ quantity;
///   `None` for bundle lines and whenever recipe cost is unknown.
/// * `cost_missing` — any unresolved entry, a menu line with no recipe at
///   all, or an additive addon with no ingredient rows.
pub fn summarize_line_costs(
    deductions: &[InventoryDeduction],
    quantity: i32,
    is_bundle_line: bool,
    has_uncosted_addon: bool,
) -> LineCostSummary {
    let mut cost_missing = deductions.iter().any(|d| d.line_cost.is_none())
        || deductions.is_empty()
        || has_uncosted_addon;

    // Ingredient costs are stored in piastres; no currency conversion here.
    let total_cost: f64 = deductions
        .iter()
        .filter_map(|d| d.cost_per_unit.map(|c| c * d.quantity))
        .sum();
    let line_cost = if cost_missing {
        None
    } else {
        Some(total_cost.round() as i64)
    };

    let recipe_scope =
        |d: &&InventoryDeduction| d.source == "drink_recipe" || d.source.starts_with("addon_swap:");
    let unit_cost = if is_bundle_line {
        None
    } else {
        let entries: Vec<&InventoryDeduction> = deductions.iter().filter(recipe_scope).collect();
        if entries.is_empty() || entries.iter().any(|d| d.cost_per_unit.is_none()) {
            None
        } else {
            let cost: f64 = entries
                .iter()
                .map(|d| d.cost_per_unit.unwrap() * d.quantity)
                .sum();
            Some((cost / quantity.max(1) as f64).round() as i64)
        }
    };

    if is_bundle_line && deductions.is_empty() {
        cost_missing = true;
    }

    LineCostSummary {
        line_cost,
        unit_cost,
        cost_missing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ded(
        source: &str,
        quantity: f64,
        cost_per_unit: Option<f64>,
        line_cost: Option<i64>,
    ) -> InventoryDeduction {
        InventoryDeduction {
            org_ingredient_id: None,
            ingredient_name: String::new(),
            unit: String::new(),
            quantity,
            source: source.to_string(),
            category: String::new(),
            addon_item_id: None,
            optional_field_id: None,
            component_item_id: None,
            cost_per_unit,
            line_cost,
        }
    }

    #[test]
    fn empty_line_is_cost_missing() {
        let s = summarize_line_costs(&[], 1, false, false);
        assert!(s.cost_missing);
        assert_eq!(s.line_cost, None);
        assert_eq!(s.unit_cost, None);
    }

    #[test]
    fn fully_costed_recipe_line_rolls_up() {
        let d = ded("drink_recipe", 2.0, Some(50.0), Some(100));
        let s = summarize_line_costs(std::slice::from_ref(&d), 1, false, false);
        assert!(!s.cost_missing);
        assert_eq!(s.line_cost, Some(100)); // 50 × 2
        assert_eq!(s.unit_cost, Some(100)); // recipe cost ÷ qty(1)
    }

    #[test]
    fn unit_cost_divides_by_quantity() {
        let d = ded("drink_recipe", 4.0, Some(50.0), Some(200));
        let s = summarize_line_costs(std::slice::from_ref(&d), 2, false, false);
        assert_eq!(s.line_cost, Some(200)); // full COGS, not divided
        assert_eq!(s.unit_cost, Some(100)); // 200 ÷ 2
    }

    #[test]
    fn any_unknown_cost_marks_missing() {
        let d = ded("drink_recipe", 2.0, None, None);
        let s = summarize_line_costs(std::slice::from_ref(&d), 1, false, false);
        assert!(s.cost_missing);
        assert_eq!(s.line_cost, None);
    }

    #[test]
    fn uncosted_addon_flag_marks_missing() {
        let d = ded("drink_recipe", 2.0, Some(50.0), Some(100));
        let s = summarize_line_costs(std::slice::from_ref(&d), 1, false, true);
        assert!(s.cost_missing);
        assert_eq!(s.line_cost, None);
    }

    #[test]
    fn bundle_line_has_no_unit_cost() {
        let d = ded("drink_recipe", 2.0, Some(50.0), Some(100));
        let s = summarize_line_costs(std::slice::from_ref(&d), 1, true, false);
        assert_eq!(s.unit_cost, None);
        assert_eq!(s.line_cost, Some(100));
        // A non-empty, fully-costed bundle line is NOT cost_missing — the late
        // `is_bundle_line && deductions.is_empty()` guard must require BOTH.
        assert!(!s.cost_missing);
    }

    #[test]
    fn empty_bundle_line_is_cost_missing() {
        let s = summarize_line_costs(&[], 1, true, false);
        assert!(s.cost_missing);
    }

    #[test]
    fn addons_excluded_from_unit_cost_but_in_line_cost() {
        // unit_cost is recipe-scope only (drink_recipe + addon_swap); a plain
        // additive addon contributes to line_cost but not unit_cost.
        let recipe = ded("drink_recipe", 1.0, Some(50.0), Some(50));
        let addon = ded("addon", 1.0, Some(30.0), Some(30));
        let s = summarize_line_costs(&[recipe, addon], 1, false, false);
        assert_eq!(s.line_cost, Some(80)); // 50 + 30
        assert_eq!(s.unit_cost, Some(50)); // recipe scope only
    }
}
