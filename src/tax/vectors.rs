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

use crate::tax::engine::{TaxPolicy, compute};

/// One priced bill: the inputs, and every figure they must produce.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Vector {
    pub subtotal: i64,
    pub discount: i64,
    pub tax_rate: String,
    pub tax_inclusive: bool,
    pub service_charge_rate: String,
    pub service_charge_taxable: bool,
    // Expected:
    pub service_charge: i64,
    pub tax: i64,
    pub total: i64,
    pub net: i64,
}

pub fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tax_vectors.json")
}

/// Every combination worth pinning: both modes, both service-charge
/// treatments, the rates a shop plausibly sets (including 14.5%, where f64 and
/// decimal rounding part company), and bills that exercise the rounding
/// boundaries rather than only round numbers.
pub fn generate() -> Vec<Vector> {
    let rates = ["0", "0.05", "0.10", "0.14", "0.145", "0.20", "0.255", "1"];
    let charges = ["0", "0.10", "0.125"];
    let bills: &[(i64, i64)] = &[
        (0, 0),
        (1, 0),
        (7, 0),
        (100, 0),
        (333, 0),
        (999, 0),
        (1000, 0),
        (1500, 0),
        (4999, 0),
        (5000, 0),
        (5700, 0),
        (12_345, 0),
        (99_999, 0),
        (1_000_000, 0),
        // Discounts, including ones that swallow the bill.
        (5000, 1),
        (5000, 500),
        (5000, 4999),
        (5000, 5000),
        (5000, 99_999),
        (1, 1),
    ];

    let mut out = Vec::new();
    for r in rates {
        for c in charges {
            for &taxable in &[true, false] {
                for &inclusive in &[true, false] {
                    for &(subtotal, discount) in bills {
                        let policy = TaxPolicy {
                            tax_rate: r.parse::<Decimal>().unwrap(),
                            tax_inclusive: inclusive,
                            service_charge_rate: c.parse::<Decimal>().unwrap(),
                            service_charge_taxable: taxable,
                        };
                        let b = compute(subtotal, discount, &policy);
                        out.push(Vector {
                            subtotal,
                            discount,
                            tax_rate: r.to_string(),
                            tax_inclusive: inclusive,
                            service_charge_rate: c.to_string(),
                            service_charge_taxable: taxable,
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
