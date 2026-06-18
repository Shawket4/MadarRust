#![no_main]
//! Fuzz `summarize_line_costs`: must never panic (incl. quantity 0/negative, NaN
//! costs). `cost_missing` ⇒ no line_cost; bundle lines have no unit_cost; empty /
//! uncosted-addon / any-missing-entry ⇒ cost_missing.

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use sufrix_rust::orders::cost_math::{summarize_line_costs, InventoryDeduction};

const SOURCES: &[&str] =
    &["drink_recipe", "addon", "addon_swap:x", "optional", "bundle_component:y"];

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let Ok(n) = u8::arbitrary(&mut u) else { return };
    let n = (n % 12) as usize;

    let mut deds: Vec<InventoryDeduction> = Vec::with_capacity(n);
    for _ in 0..n {
        let Ok(si) = u8::arbitrary(&mut u) else { return };
        let Ok(qty) = f64::arbitrary(&mut u) else { return };
        let cost = if bool::arbitrary(&mut u).unwrap_or(false) {
            f64::arbitrary(&mut u).ok()
        } else {
            None
        };
        let line_cost = if bool::arbitrary(&mut u).unwrap_or(false) {
            i64::arbitrary(&mut u).ok()
        } else {
            None
        };
        deds.push(InventoryDeduction {
            org_ingredient_id: None,
            ingredient_name: String::new(),
            unit: String::new(),
            quantity: qty,
            source: SOURCES[si as usize % SOURCES.len()].to_string(),
            category: String::new(),
            addon_item_id: None,
            optional_field_id: None,
            component_item_id: None,
            cost_per_unit: cost,
            line_cost,
        });
    }
    let (Ok(quantity), Ok(is_bundle), Ok(uncosted)) =
        (i32::arbitrary(&mut u), bool::arbitrary(&mut u), bool::arbitrary(&mut u))
    else {
        return;
    };

    let s = summarize_line_costs(&deds, quantity, is_bundle, uncosted);

    if s.cost_missing {
        assert!(s.line_cost.is_none(), "cost_missing but line_cost set");
    }
    if is_bundle {
        assert!(s.unit_cost.is_none(), "bundle line has unit_cost");
    }
    if deds.is_empty() || uncosted || deds.iter().any(|d| d.line_cost.is_none()) {
        assert!(s.cost_missing, "expected cost_missing");
    }
});
