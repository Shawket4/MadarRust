//! What a bill adds up to, and the only place that decides it.
//!
//! This exists because the number was previously decided in three places that
//! disagreed. The dashboard called `tax_rate` a percentage and rendered `14%`
//! from a column the backend validated as a fraction `0..=1`, so an admin
//! typing what the label asked for got a 400 and an admin typing `0.14` saw
//! "0.14%" and corrected it back. Online orders skipped tax entirely
//! (`tax_amount: 0`, hard-coded at delivery finalize). And the till's figure
//! was recorded verbatim, so whatever it sent became the books.
//!
//! ## The rules
//!
//! * **Exclusive** — the menu price is net; tax is added at checkout.
//! * **Inclusive** — the menu price is what the customer pays and already
//!   contains the tax; the receipt breaks it out backwards. The two are
//!   consistent by construction: an exclusive bill of 5000 at 14% and an
//!   inclusive bill of 5700 at 14% are the same bill, and this returns the same
//!   `net`, `tax` and `total` for both.
//! * **Service charge** is a percentage of the same base as tax, and is itself
//!   taxable or not per policy. When it is taxable it enters the tax base;
//!   when it is not, it rides alongside untaxed.
//!
//! ## Why `Decimal` and not `f64`
//!
//! CLAUDE.md requires it for currency, and the old `(taxable as f64 * rate)`
//! was a real hazard rather than a theoretical one: `0.07` and `0.145` are not
//! representable in binary floating point, so a rate that is exactly half a
//! piastre lands on whichever side the representation error falls — and the
//! backend and the till, rounding the same bill from different code, could
//! land on different sides. That is a one-piastre mismatch, and with the
//! server now REJECTING mismatches it would be a refused sale.
//!
//! Rounding is half-away-from-zero, matching what `f64::round()` did before,
//! so historical orders re-price to the figures already in the books.
//!
//! ## The two copies
//!
//! The till prices offline, so this logic necessarily exists twice: here and in
//! `madar/rust-core/crates/madar-core/src/tax.rs`. They are pinned together by
//! `tax_vectors.json`, a fixture committed to BOTH repos and executed by a test
//! in each. Change the maths in one and that repo's test fails against the
//! other's expectations. Never edit one copy alone.

use rust_decimal::Decimal;
use rust_decimal::prelude::*;

/// Minor units (piastres). Signed because a discount can exceed a line and the
/// clamping has to be visible rather than wrapping.
pub type Minor = i64;

/// How one organisation (or one branch overriding it) taxes a bill.
///
/// Resolved from the branch first and the org second — see `policy::resolve`.
/// It is carried on the order once applied, because a rate that changes next
/// month must not restate last month's books.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaxPolicy {
    /// Fraction, not percentage: `0.14` is 14%. Guarded at every boundary.
    pub tax_rate: Decimal,
    /// Whether menu prices already contain the tax.
    pub tax_inclusive: bool,
    /// Fraction of the bill added as a service charge. `0` disables it.
    pub service_charge_rate: Decimal,
    /// Whether the service charge is itself taxed.
    pub service_charge_taxable: bool,
}

impl Default for TaxPolicy {
    /// Tax-free and charge-free. Deliberately NOT Egypt's 14%: a policy that
    /// failed to load must not invent a tax the shop never configured, which
    /// is what the old `.unwrap_or(0.14)` did on any parse failure.
    fn default() -> Self {
        Self {
            tax_rate: Decimal::ZERO,
            tax_inclusive: false,
            service_charge_rate: Decimal::ZERO,
            service_charge_taxable: true,
        }
    }
}

impl TaxPolicy {
    /// A policy is only usable if both rates are real fractions. Anything else
    /// is the percent/fraction confusion arriving from somewhere new, and it
    /// should stop here rather than multiply a bill by fourteen.
    pub fn is_sane(&self) -> bool {
        let ok = |r: Decimal| r >= Decimal::ZERO && r <= Decimal::ONE;
        ok(self.tax_rate) && ok(self.service_charge_rate)
    }
}

/// Every figure a bill needs, in minor units.
///
/// `subtotal` is what the customer sees listed; `net` is what the books count
/// as revenue. In exclusive mode they are equal. In inclusive mode `subtotal`
/// is bigger, because it contains the tax.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Breakdown {
    /// Line total as charged, before discount. Gross when tax-inclusive.
    pub subtotal: Minor,
    pub discount: Minor,
    /// Added to the bill; `0` when the policy has no service charge.
    pub service_charge: Minor,
    /// Contained within the total when inclusive, added on top when exclusive.
    pub tax: Minor,
    /// What the customer pays.
    pub total: Minor,
    /// Revenue excluding tax — `total - tax`. What reports should sum, and the
    /// reason they can no longer get away with `total - tax - delivery_fee`
    /// once a service charge exists.
    pub net: Minor,
}

/// Half-away-from-zero to the whole minor unit, matching the `f64::round()`
/// this replaces so existing orders re-price to what is already recorded.
fn round_minor(d: Decimal) -> Minor {
    d.round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero)
        .to_i64()
        .unwrap_or(0)
}

fn minor(v: Minor) -> Decimal {
    Decimal::from(v)
}

/// Price a bill.
///
/// `subtotal` and `discount` arrive already computed — line maths and discount
/// policy are not this function's business, and `discount` is clamped here only
/// because a bill may never go negative.
pub fn compute(subtotal: Minor, discount: Minor, policy: &TaxPolicy) -> Breakdown {
    // A bill cannot be discounted below nothing. The clamp lives here as well
    // as at the call sites because this is the function the books trust.
    let discount = discount.clamp(0, subtotal.max(0));
    let base = (subtotal - discount).max(0);

    let one = Decimal::ONE;
    let service_charge = round_minor(minor(base) * policy.service_charge_rate);

    let (tax, total) = if policy.tax_inclusive {
        // The base already contains tax. Divide it back out rather than
        // multiplying: `gross - gross/(1+rate)` is the only form that makes an
        // inclusive bill agree with the exclusive bill it represents.
        let taxed_gross = if policy.service_charge_taxable {
            base + service_charge
        } else {
            base
        };
        let divisor = one + policy.tax_rate;
        // A rate of exactly -100% would divide by zero; `is_sane` excludes it,
        // and this stays defensive because the books are downstream.
        let net_of_tax = if divisor.is_zero() {
            taxed_gross
        } else {
            round_minor(minor(taxed_gross) / divisor)
        };
        let tax = taxed_gross - net_of_tax;
        (tax, base + service_charge)
    } else {
        let tax_base = if policy.service_charge_taxable {
            base + service_charge
        } else {
            base
        };
        let tax = round_minor(minor(tax_base) * policy.tax_rate);
        (tax, base + service_charge + tax)
    };

    Breakdown {
        subtotal,
        discount,
        service_charge,
        tax,
        total,
        net: total - tax,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn exclusive(rate: Decimal) -> TaxPolicy {
        TaxPolicy {
            tax_rate: rate,
            ..TaxPolicy::default()
        }
    }

    fn inclusive(rate: Decimal) -> TaxPolicy {
        TaxPolicy {
            tax_rate: rate,
            tax_inclusive: true,
            ..TaxPolicy::default()
        }
    }

    #[test]
    fn exclusive_adds_tax_on_top() {
        let b = compute(5000, 0, &exclusive(dec!(0.14)));
        assert_eq!(b.tax, 700);
        assert_eq!(b.total, 5700);
        assert_eq!(b.net, 5000);
    }

    #[test]
    fn inclusive_takes_the_same_tax_back_out() {
        // The whole point of the two modes: 5000 net at 14% and 5700 gross at
        // 14% are ONE bill described two ways, and must agree on every figure
        // the books read.
        let ex = compute(5000, 0, &exclusive(dec!(0.14)));
        let inc = compute(5700, 0, &inclusive(dec!(0.14)));
        assert_eq!(inc.tax, ex.tax);
        assert_eq!(inc.total, ex.total);
        assert_eq!(inc.net, ex.net);
        // Only the listed subtotal differs, which is exactly what the customer
        // sees differ on the menu.
        assert_eq!(inc.subtotal, 5700);
        assert_eq!(ex.subtotal, 5000);
    }

    #[test]
    fn a_zero_rate_is_a_bill_with_no_tax_line() {
        for policy in [exclusive(Decimal::ZERO), inclusive(Decimal::ZERO)] {
            let b = compute(4321, 0, &policy);
            assert_eq!(b.tax, 0);
            assert_eq!(b.total, 4321);
            assert_eq!(b.net, 4321);
        }
    }

    #[test]
    fn service_charge_is_taxed_when_the_policy_says_so() {
        let p = TaxPolicy {
            tax_rate: dec!(0.14),
            service_charge_rate: dec!(0.12),
            service_charge_taxable: true,
            tax_inclusive: false,
        };
        let b = compute(5000, 0, &p);
        assert_eq!(b.service_charge, 600);
        assert_eq!(b.tax, 784, "tax is charged on 5600, not 5000");
        assert_eq!(b.total, 6384);
    }

    #[test]
    fn an_untaxed_service_charge_rides_alongside() {
        let p = TaxPolicy {
            tax_rate: dec!(0.14),
            service_charge_rate: dec!(0.12),
            service_charge_taxable: false,
            tax_inclusive: false,
        };
        let b = compute(5000, 0, &p);
        assert_eq!(b.service_charge, 600);
        assert_eq!(b.tax, 700, "tax is charged on 5000 only");
        assert_eq!(b.total, 6300);
    }

    #[test]
    fn discount_is_taxed_after_not_before() {
        let b = compute(5000, 1000, &exclusive(dec!(0.14)));
        assert_eq!(b.tax, 560, "14% of 4000, not of 5000");
        assert_eq!(b.total, 4560);
    }

    #[test]
    fn a_discount_cannot_drive_a_bill_negative() {
        let b = compute(1000, 99_999, &exclusive(dec!(0.14)));
        assert_eq!(b.discount, 1000);
        assert_eq!(b.total, 0);
        assert_eq!(b.tax, 0);
        assert_eq!(b.net, 0);
    }

    #[test]
    fn the_default_policy_invents_no_tax() {
        // The old code fell back to 0.14 whenever it failed to read a rate,
        // which silently charged Egyptian VAT to a shop that never set one.
        let b = compute(5000, 0, &TaxPolicy::default());
        assert_eq!(b.tax, 0);
        assert_eq!(b.total, 5000);
    }

    #[test]
    fn a_percentage_masquerading_as_a_fraction_is_not_sane() {
        // `14` instead of `0.14` is the bug this whole module exists because
        // of. It must never reach `compute`.
        assert!(
            !TaxPolicy {
                tax_rate: dec!(14),
                ..TaxPolicy::default()
            }
            .is_sane()
        );
        assert!(
            !TaxPolicy {
                service_charge_rate: dec!(12),
                ..TaxPolicy::default()
            }
            .is_sane()
        );
        assert!(
            TaxPolicy {
                tax_rate: dec!(0.14),
                ..TaxPolicy::default()
            }
            .is_sane()
        );
        assert!(
            TaxPolicy {
                tax_rate: Decimal::ONE,
                ..TaxPolicy::default()
            }
            .is_sane()
        );
    }

    #[test]
    fn rates_that_are_not_representable_in_binary_still_land_on_the_half() {
        // 14.5% of 1.00 is exactly 14.5 piastres, which rounds to 15. In f64
        // the product is 14.499999999999998, so `round()` gives 14 — the
        // customer is charged a piastre less than the rate says.
        //
        // This is not a curiosity. Two implementations of this bill (the
        // server's and the till's) that round from different code can land on
        // different sides of that half, and with the server now REJECTING
        // mismatches a one-piastre disagreement is a refused sale.
        let b = compute(100, 0, &exclusive(dec!(0.145)));
        assert_eq!(b.tax, 15);
        assert_eq!(
            (100.0_f64 * 0.145).round() as i64,
            14,
            "the f64 maths this replaces really did lose the piastre"
        );
    }

    #[test]
    fn net_plus_tax_is_always_the_total() {
        // The invariant every report depends on, across the whole policy space.
        for &incl in &[true, false] {
            for &sc_taxable in &[true, false] {
                for rate in [dec!(0), dec!(0.05), dec!(0.14), dec!(0.255), Decimal::ONE] {
                    for sc in [dec!(0), dec!(0.10), dec!(0.125)] {
                        for sub in [0, 1, 7, 999, 12_345, 1_000_000] {
                            for disc in [0, 1, 500] {
                                let p = TaxPolicy {
                                    tax_rate: rate,
                                    tax_inclusive: incl,
                                    service_charge_rate: sc,
                                    service_charge_taxable: sc_taxable,
                                };
                                let b = compute(sub, disc, &p);
                                assert_eq!(b.net + b.tax, b.total, "{b:?} / {p:?}");
                                assert!(b.total >= 0, "{b:?} / {p:?}");
                                assert!(b.tax >= 0, "{b:?} / {p:?}");
                                assert!(b.service_charge >= 0, "{b:?} / {p:?}");
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn inclusive_and_exclusive_agree_wherever_the_gross_is_exact() {
        // Sweep the rates a shop would actually set. Where an exclusive bill
        // has a whole-piastre gross, describing that gross as inclusive must
        // reproduce the identical bill — otherwise the two modes are two
        // different tax systems wearing one name.
        for rate in [dec!(0), dec!(0.05), dec!(0.10), dec!(0.14), dec!(0.20)] {
            for net in [0, 100, 1000, 5000, 12_500, 250_000] {
                let ex = compute(net, 0, &exclusive(rate));
                let inc = compute(ex.total, 0, &inclusive(rate));
                assert_eq!(inc.tax, ex.tax, "rate {rate} net {net}");
                assert_eq!(inc.net, ex.net, "rate {rate} net {net}");
                assert_eq!(inc.total, ex.total, "rate {rate} net {net}");
            }
        }
    }
}
