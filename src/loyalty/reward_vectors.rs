//! The contract between the server's reward pricing and the till's.
//!
//! A reward changes what is owed, and the till must show — and collect — the
//! same figure the server records. The rule is: each reward covers whole units
//! of its line at the price the line was charged per unit (modifiers included,
//! capped at the line); the covered amount comes off the subtotal FIRST; the
//! discount is then resolved against the reduced subtotal; service charge and
//! tax follow through the shared engine.
//!
//! Pinned by `loyalty_reward_vectors.json`, committed identically to
//! `madar/rust-core/crates/madar-core`. To change the maths deliberately:
//! change it here, run
//! `MADAR_REGENERATE_REWARD_VECTORS=1 cargo test --lib loyalty::reward_vectors`,
//! copy the file to the POS repo, and make the same change there.

use std::path::PathBuf;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::tax::engine::{Breakdown, TaxPolicy, compute, discount_amount};
use crate::tax::vectors::discount_from_wire;

/// One line of a reward bill: charged per unit (modifiers included), how many
/// units, how many of them a reward covers.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Line {
    pub per_unit: i64,
    pub qty: i64,
    pub reward_units: i64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Vector {
    pub lines: Vec<Line>,
    pub discount_kind: String,
    pub discount_value: String,
    pub tax_rate: String,
    pub tax_inclusive: bool,
    pub service_charge_rate: String,
    // Expected:
    pub covered: i64,
    pub subtotal: i64,
    pub discount: i64,
    pub service_charge: i64,
    pub tax: i64,
    pub total: i64,
}

/// The server's reward arithmetic, in the order `create_order_inner` applies
/// it. Returns (covered, breakdown over the reduced subtotal).
pub fn price(lines: &[Line], kind: &str, value: &str, policy: &TaxPolicy) -> (i64, Breakdown) {
    let mut subtotal = 0i64;
    let mut covered = 0i64;
    for l in lines {
        let line = l.per_unit * l.qty;
        let c = super::redeem::covered_minor(l.per_unit, line, l.reward_units);
        covered += c;
        subtotal += line - c;
    }
    let discount = discount_amount(subtotal, discount_from_wire(kind, value));
    (covered, compute(subtotal, discount, policy))
}

pub fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("loyalty_reward_vectors.json")
}

pub fn generate() -> Vec<Vector> {
    let baskets: Vec<Vec<Line>> = vec![
        vec![Line {
            per_unit: 5_000,
            qty: 1,
            reward_units: 1,
        }],
        vec![
            Line {
                per_unit: 5_000,
                qty: 1,
                reward_units: 1,
            },
            Line {
                per_unit: 9_000,
                qty: 1,
                reward_units: 0,
            },
        ],
        // Oat milk and a large: the reward is the drink as chosen.
        vec![
            Line {
                per_unit: 6_750,
                qty: 3,
                reward_units: 2,
            },
            Line {
                per_unit: 1_999,
                qty: 2,
                reward_units: 0,
            },
        ],
        vec![
            Line {
                per_unit: 4_550,
                qty: 2,
                reward_units: 1,
            },
            Line {
                per_unit: 3_333,
                qty: 1,
                reward_units: 1,
            },
            Line {
                per_unit: 12_345,
                qty: 1,
                reward_units: 0,
            },
        ],
        // Everything free.
        vec![Line {
            per_unit: 5_000,
            qty: 2,
            reward_units: 2,
        }],
        // Units beyond the line are capped at it.
        vec![
            Line {
                per_unit: 2_500,
                qty: 1,
                reward_units: 3,
            },
            Line {
                per_unit: 1_005,
                qty: 7,
                reward_units: 0,
            },
        ],
    ];
    let discounts = [
        ("none", "0"),
        ("percentage", "0.10"),
        ("percentage", "0.145"),
        ("fixed", "1500"),
        ("fixed", "99999"),
    ];
    let taxes = [
        ("0", true),
        ("0.14", false),
        ("0.14", true),
        ("0.145", false),
    ];
    let charges = ["0", "0.12"];
    let mut out = Vec::new();
    for lines in &baskets {
        for (kind, value) in discounts {
            for (rate, inclusive) in taxes {
                for charge in charges {
                    let policy = TaxPolicy {
                        tax_rate: rate.parse::<Decimal>().unwrap(),
                        tax_inclusive: inclusive,
                        service_charge_rate: charge.parse::<Decimal>().unwrap(),
                        service_charge_taxable: true,
                    };
                    let (covered, b) = price(lines, kind, value, &policy);
                    out.push(Vector {
                        lines: lines.clone(),
                        discount_kind: kind.into(),
                        discount_value: value.into(),
                        tax_rate: rate.into(),
                        tax_inclusive: inclusive,
                        service_charge_rate: charge.into(),
                        covered,
                        subtotal: b.subtotal,
                        discount: b.discount,
                        service_charge: b.service_charge,
                        tax: b.tax,
                        total: b.total,
                    });
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
    fn reward_pricing_still_agrees_with_the_shared_vectors() {
        let generated = generate();
        if std::env::var("MADAR_REGENERATE_REWARD_VECTORS").is_ok() {
            std::fs::write(
                fixture_path(),
                serde_json::to_string_pretty(&generated).unwrap() + "\n",
            )
            .unwrap();
            return;
        }
        let raw = std::fs::read_to_string(fixture_path())
            .expect("loyalty_reward_vectors.json is missing — regenerate it");
        let expected: Vec<Vector> = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            generated, expected,
            "reward maths drifted from the shared vectors"
        );
    }
}
