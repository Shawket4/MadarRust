//! The contract between this repo's tax engine and the till's.
//!
//! The till prices offline, so the arithmetic necessarily exists twice — here
//! and in `madar/rust-core/crates/madar-core/src/tax.rs`. Two copies of a money
//! rule drift; that is not a risk, it is a certainty over enough commits. And
//! since the server now REJECTS an order whose figures disagree with its own, a
//! drift of one piastre is a sale the till cannot make.
//!
//! So the two copies are pinned by `tax_vectors.json`, committed identically to
//! both repos. Each repo has a test that runs its own engine over every vector
//! and compares. Change the maths in one place and that repo goes red against
//! the other's expectations, which is the whole point — the failure arrives at
//! the commit that caused it rather than at a till three weeks later.
//!
//! To change the maths deliberately: change it here, run
//! `MADAR_REGENERATE_TAX_VECTORS=1 cargo test --lib tax::vectors`, copy the
//! regenerated file to the POS repo, and make the same change there.

use std::path::PathBuf;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::tax::engine::{Discount, TaxPolicy, compute, discount_amount};

/// One priced bill: the inputs, and every figure they must produce.
///
/// The discount is stated the way the policy states it — a kind and a value —
/// rather than as the amount it comes to. The amount is an OUTPUT, because
/// deriving it is a rounding point, and a fixture that carried it ready-made
/// let the two engines derive it differently (one in `f64`, one in `Decimal`)
/// while both conformance tests stayed green.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Vector {
    pub subtotal: i64,
    /// `"none"`, `"percentage"` or `"fixed"` — the `discount_type` column's words.
    pub discount_kind: String,
    /// A fraction for `percentage`, minor units for `fixed`, `"0"` for none.
    pub discount_value: String,
    pub tax_rate: String,
    pub tax_inclusive: bool,
    pub service_charge_rate: String,
    pub service_charge_taxable: bool,
    // Expected:
    pub discount: i64,
    pub service_charge: i64,
    pub tax: i64,
    pub total: i64,
    pub net: i64,
}

/// The fixture's words for a discount, as the engine's type. Mirrored in the
/// till's conformance test; an unknown kind is a fixture bug, not a bill.
pub fn discount_from_wire(kind: &str, value: &str) -> Discount {
    let value = value
        .parse::<Decimal>()
        .unwrap_or_else(|e| panic!("discount_value {value:?} is not a decimal: {e}"));
    match kind {
        "none" => Discount::None,
        "percentage" => Discount::Percentage(value),
        "fixed" => Discount::Fixed(value),
        other => panic!("unknown discount_kind {other:?} in tax_vectors.json"),
    }
}

pub fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tax_vectors.json")
}

/// Every combination worth pinning: both modes, both service-charge
/// treatments, the rates a shop plausibly sets (including 14.5%, where f64 and
/// decimal rounding part company), and bills that exercise the rounding
/// boundaries rather than only round numbers — with discounts stated as the
/// policy states them, so the derivation is pinned as well as the tax on it.
pub fn generate() -> Vec<Vector> {
    let rates = ["0", "0.05", "0.10", "0.14", "0.145", "0.20", "0.255", "1"];
    let charges = ["0", "0.10", "0.125"];
    let bills: &[(i64, &str, &str)] = &[
        // Plain bills.
        (0, "none", "0"),
        (1, "none", "0"),
        (7, "none", "0"),
        (100, "none", "0"),
        (333, "none", "0"),
        (999, "none", "0"),
        (1000, "none", "0"),
        (1500, "none", "0"),
        (4999, "none", "0"),
        (5000, "none", "0"),
        (5700, "none", "0"),
        (12_345, "none", "0"),
        (99_999, "none", "0"),
        (1_000_000, "none", "0"),
        // Fixed amounts off, including ones that swallow the bill, and one
        // with a fraction of a piastre — the column is NUMERIC, so it can arrive.
        (5000, "fixed", "1"),
        (5000, "fixed", "500"),
        (5000, "fixed", "250.5"),
        (5000, "fixed", "4999"),
        (5000, "fixed", "5000"),
        (5000, "fixed", "99999"),
        (1, "fixed", "1"),
        // Percentages. Every one of these lands on or near a half-piastre, which
        // is where a derivation in binary floating point parts company with one
        // in decimal — 100 at 14.5% is the case that actually bit.
        (100, "percentage", "0.145"),
        (5, "percentage", "0.10"),
        (25, "percentage", "0.10"),
        (105, "percentage", "0.10"),
        (1000, "percentage", "0.125"),
        (333, "percentage", "0.333"),
        (12_345, "percentage", "0.075"),
        (5000, "percentage", "0.145"),
        (1_000_000, "percentage", "0.145"),
        // An inclusive shop's gross with a discount on it: 5700 is 5000 at 14%.
        (5700, "percentage", "0.10"),
        // Discounts that swallow the bill, or would take more than it.
        (1, "percentage", "0.5"),
        (1, "percentage", "0.145"),
        (0, "percentage", "0.10"),
        (5000, "percentage", "1"),
        (5000, "percentage", "1.5"),
    ];

    let mut out = Vec::new();
    for r in rates {
        for c in charges {
            for &taxable in &[true, false] {
                for &inclusive in &[true, false] {
                    for &(subtotal, kind, value) in bills {
                        let policy = TaxPolicy {
                            tax_rate: r.parse::<Decimal>().unwrap(),
                            tax_inclusive: inclusive,
                            service_charge_rate: c.parse::<Decimal>().unwrap(),
                            service_charge_taxable: taxable,
                        };
                        let discount = discount_amount(subtotal, discount_from_wire(kind, value));
                        let b = compute(subtotal, discount, &policy);
                        out.push(Vector {
                            subtotal,
                            discount_kind: kind.to_string(),
                            discount_value: value.to_string(),
                            tax_rate: r.to_string(),
                            tax_inclusive: inclusive,
                            service_charge_rate: c.to_string(),
                            service_charge_taxable: taxable,
                            discount: b.discount,
                            service_charge: b.service_charge,
                            tax: b.tax,
                            total: b.total,
                            net: b.net,
                        });
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_engine_still_agrees_with_the_shared_vectors() {
        let generated = generate();

        // Deliberate change to the maths: regenerate, then mirror the file
        // into the POS repo and make the same change to its engine.
        if std::env::var("MADAR_REGENERATE_TAX_VECTORS").is_ok() {
            std::fs::write(
                fixture_path(),
                serde_json::to_string_pretty(&generated).unwrap() + "\n",
            )
            .unwrap();
            eprintln!(
                "regenerated {} vectors at {}",
                generated.len(),
                fixture_path().display()
            );
            return;
        }

        let raw = std::fs::read_to_string(fixture_path()).expect(
            "tax_vectors.json is missing — regenerate with \
             MADAR_REGENERATE_TAX_VECTORS=1 cargo test --lib tax::vectors",
        );
        let expected: Vec<Vector> = serde_json::from_str(&raw).unwrap();

        assert_eq!(
            generated.len(),
            expected.len(),
            "the vector set itself changed; regenerate and mirror to the POS repo"
        );
        let mut drift = Vec::new();
        for (got, want) in generated.iter().zip(expected.iter()) {
            if got != want {
                drift.push(format!("  got {got:?}\n  want {want:?}"));
            }
        }
        assert!(
            drift.is_empty(),
            "the tax engine no longer matches the shared vectors — the till \
             computes these bills differently and the server would reject its \
             orders:\n{}",
            drift.join("\n")
        );
    }
}
