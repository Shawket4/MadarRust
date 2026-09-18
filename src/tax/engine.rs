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

/// How a bill is discounted, as the policy states it — before it is an amount.
///
/// `compute` takes the amount, because discount policy is not its business.
/// But TURNING a rate into an amount is the other place a bill rounds, and it
/// went unpinned: `tax_vectors.json` carried the amount ready-made, so the till
/// could derive it in `f64` while the server derived it in `Decimal` and the
/// conformance test on each side stayed green. At 14.5% of 1.00 the two
/// answers differ — `100.0 * 0.145` is 14.499999999999998 and rounds to 14,
/// while the decimal 14.5 rounds to 15 — and the server refuses the till's
/// order over that piastre. So the derivation lives here, next to the rounding
/// it has to match, and the fixture states the rate rather than the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discount {
    None,
    /// A fraction of the subtotal, like every rate in this system: `0.10` is
    /// 10% off. Anything above `1` takes the whole bill and no more.
    Percentage(Decimal),
    /// An amount off, in minor units. Never more than the bill.
    Fixed(Decimal),
}

/// The amount a discount takes off `subtotal`, in minor units.
///
/// Rounded half-away-from-zero like everything else on the bill, and clamped to
/// `[0, subtotal]`: a discount can neither inflate a bill nor drive it below
/// nothing. `compute` clamps again, but this is the figure the receipt prints
/// on its discount line, so it has to be right on its own.
pub fn discount_amount(subtotal: Minor, discount: Discount) -> Minor {
    let raw = match discount {
        Discount::None => 0,
        Discount::Percentage(rate) => round_minor(minor(subtotal) * rate),
        Discount::Fixed(amount) => round_minor(amount),
    };
    raw.clamp(0, subtotal.max(0))
}

/// Where a sale happens, which decides whether it carries a service charge.
///
/// The service charge is DINE-IN ONLY (owner ruling 2): a party that sat at a
/// table pays it, anything carried out, delivered or ordered online does not.
/// The rule used to live in two `if` statements on the server and nowhere on
/// the till, so the till added the charge to a takeaway the server then
/// refused. It lives here now, in the pinned engine, and `tax_vectors.json`
/// carries a case per channel so the two copies cannot part company on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaleChannel {
    /// A table's bill (an open ticket settled at the till).
    DineIn,
    /// Rung straight through the till: counter, takeaway, a parked cart.
    Takeaway,
    /// A delivery order finalized at the branch.
    Delivery,
    /// The online storefront.
    Online,
}

impl SaleChannel {
    /// The `orders.order_type` word for the channel (`online` has none of its
    /// own on the order and books as `delivery`; the word here is the vector's).
    pub fn as_str(self) -> &'static str {
        match self {
            SaleChannel::DineIn => "dine_in",
            SaleChannel::Takeaway => "takeaway",
            SaleChannel::Delivery => "delivery",
            SaleChannel::Online => "online",
        }
    }

    /// The channel for a wire word. Unknown words are `None`, never a guess.
    pub fn from_wire(word: &str) -> Option<Self> {
        match word {
            "dine_in" => Some(SaleChannel::DineIn),
            "takeaway" => Some(SaleChannel::Takeaway),
            "delivery" => Some(SaleChannel::Delivery),
            "online" => Some(SaleChannel::Online),
            _ => None,
        }
    }

    /// Whether a sale on this channel may carry a service charge at all.
    pub fn carries_service_charge(self) -> bool {
        matches!(self, SaleChannel::DineIn)
    }
}

impl TaxPolicy {
    /// The policy one sale is priced under: this policy, with the service
    /// charge taken off when the channel carries none or when someone holding
    /// `orders:waive_service` removed it from the bill.
    ///
    /// The zero is the RATE, before the engine runs, never a charge computed
    /// and then dropped: in inclusive mode a taxable charge enters the gross,
    /// so dropping it afterwards would leave the tax wrong.
    pub fn for_sale(self, channel: SaleChannel, service_waived: bool) -> TaxPolicy {
        if channel.carries_service_charge() && !service_waived {
            self
        } else {
            TaxPolicy {
                service_charge_rate: Decimal::ZERO,
                ..self
            }
        }
    }
}

// ── No bill may be negative ──────────────────────────────────────────────
//
// THE RULE (owner, 2026-09-18), and the reason it is split in two.
//
// A DISCOUNT IS CAPPED, NEVER REFUSED. `discount_amount` above clamps a
// percentage to `[0, 1]` of its base and a fixed amount to `[0, base]`. A
// discount that overshoots is a person saying "make it free" in a clumsy way —
// it has one obvious correct reading, and refusing the sale over it would
// leave a customer standing at the counter. So it is capped, silently and
// identically on both sides of the wire.
//
// EVERY OTHER NEGATIVE IS REFUSED, NEVER CAPPED. A negative line, subtotal,
// service charge, tax, total or tender has NO correct reading: it is a stale
// build, a bad modifier price, or a forged payload. Capping it to zero would
// write a sale into the books that nobody made, and the books would balance
// against nothing. So the sale is refused — at the till before it can be
// queued, and at the server on the live route and at replay.
//
// Refusing at replay does not contradict the accept-and-flag rule
// (TILLS_CONTRACT §4.4.5). That rule answers "MAY this actor do this?" and
// says a sale whose money already moved is recorded even when the actor's
// grant was missing. This answers "IS this a sale at all?". A negative total
// is not a sale that happened, it is corrupt input, and it is refused for the
// same reason `quantity <= 0` already is. The op dead-letters on the till and
// surfaces in the stuck list for the owner rather than retrying for ever.

/// A money figure that may never be negative, named for the message the teller
/// reads and the field the owner looks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NegativePart {
    /// One line of the bill (unit price + modifiers, × quantity).
    Line,
    /// The sum of the lines.
    Subtotal,
    /// The amount taken off. Capped upstream, so this only fires on a figure
    /// that never went through `discount_amount`.
    Discount,
    ServiceCharge,
    Tax,
    /// What the customer pays.
    Total,
    /// Cash handed over, a split leg, or the change given back.
    Tender,
}

impl NegativePart {
    /// A stable word for logs, wire messages and tests. Never translated —
    /// the human-facing sentence is composed by the caller in its own language.
    pub fn as_str(self) -> &'static str {
        match self {
            NegativePart::Line => "line",
            NegativePart::Subtotal => "subtotal",
            NegativePart::Discount => "discount",
            NegativePart::ServiceCharge => "service_charge",
            NegativePart::Tax => "tax",
            NegativePart::Total => "total",
            NegativePart::Tender => "tender",
        }
    }
}

/// The first negative component of a priced bill, if any. `None` means every
/// figure on it is zero or better and the sale may be recorded.
///
/// `compute` already floors the base it taxes, so this can only fire on a
/// `subtotal` that arrived negative — which is exactly the case worth
/// catching, because that subtotal is what the books store.
pub fn negative_part(b: &Breakdown) -> Option<NegativePart> {
    if b.subtotal < 0 {
        return Some(NegativePart::Subtotal);
    }
    if b.discount < 0 {
        return Some(NegativePart::Discount);
    }
    if b.service_charge < 0 {
        return Some(NegativePart::ServiceCharge);
    }
    if b.tax < 0 {
        return Some(NegativePart::Tax);
    }
    if b.total < 0 {
        return Some(NegativePart::Total);
    }
    None
}

/// The tax and service charge a refund takes back, as `(tax, service_charge)`.
///
/// A refund records an AMOUNT; the books also need to know how much of that
/// amount was tax and service, or a partially refunded order keeps all its tax
/// in the tax figure. The split is pro rata of the order's own figures, and it
/// is computed CUMULATIVELY — what the refunds so far plus this one should have
/// taken back, minus what the earlier ones did take — so the rounding of every
/// refund on an order adds up exactly, and a refund of the whole order takes
/// back exactly its tax and service charge, never a piastre either side.
///
/// `refunded_before` is the sum of the order's earlier refunds. Amounts are
/// clamped to the order, so an over-refund cannot take back more tax than the
/// order carried.
pub fn refund_split(
    order_total: Minor,
    order_tax: Minor,
    order_service_charge: Minor,
    refunded_before: Minor,
    amount: Minor,
) -> (Minor, Minor) {
    if order_total <= 0 {
        return (0, 0);
    }
    let before = refunded_before.clamp(0, order_total);
    let after = (refunded_before.max(0) + amount.max(0)).clamp(0, order_total);
    let share = |figure: Minor, upto: Minor| -> Minor {
        round_minor(minor(figure.max(0)) * minor(upto) / minor(order_total))
    };
    (
        share(order_tax, after) - share(order_tax, before),
        share(order_service_charge, after) - share(order_service_charge, before),
    )
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

    #[test]
    fn a_percentage_discount_rounds_the_decimal_not_the_binary_float() {
        // 14.5% off 1.00 is exactly 14.5 piastres, which rounds to 15. The
        // till used to compute this as `subtotal as f64 * 0.145`, which is
        // 14.499999999999998 and rounds to 14 — one piastre more for the
        // customer than the server charged, and a refused order.
        assert_eq!(discount_amount(100, Discount::Percentage(dec!(0.145))), 15);
        assert_eq!(
            (100.0_f64 * 0.145).round() as i64,
            14,
            "the f64 derivation this replaces really did lose the piastre"
        );
        // The classic halves, for good measure.
        assert_eq!(discount_amount(5, Discount::Percentage(dec!(0.10))), 1);
        assert_eq!(discount_amount(25, Discount::Percentage(dec!(0.10))), 3);
        assert_eq!(discount_amount(105, Discount::Percentage(dec!(0.10))), 11);
    }

    #[test]
    fn a_half_percent_is_expressible() {
        // The reason the value is a fraction: an integer percentage could not
        // say 12.5% at all.
        assert_eq!(
            discount_amount(1000, Discount::Percentage(dec!(0.125))),
            125
        );
    }

    #[test]
    fn a_discount_takes_at_most_the_whole_bill() {
        assert_eq!(discount_amount(5000, Discount::Percentage(dec!(1))), 5000);
        assert_eq!(discount_amount(5000, Discount::Percentage(dec!(1.5))), 5000);
        assert_eq!(discount_amount(5000, Discount::Fixed(dec!(99_999))), 5000);
        assert_eq!(discount_amount(1, Discount::Fixed(dec!(1))), 1);
        // Half a piastre off a one-piastre bill swallows it.
        assert_eq!(discount_amount(1, Discount::Percentage(dec!(0.5))), 1);
    }

    #[test]
    fn a_negative_discount_is_no_discount() {
        // A discount that ADDS to the bill is the sign flipped somewhere
        // upstream, and the books must not inherit it.
        assert_eq!(discount_amount(1000, Discount::Percentage(dec!(-0.10))), 0);
        assert_eq!(discount_amount(1000, Discount::Fixed(dec!(-50))), 0);
        assert_eq!(discount_amount(-7, Discount::Fixed(dec!(50))), 0);
        assert_eq!(discount_amount(1000, Discount::None), 0);
    }

    #[test]
    fn a_fixed_amount_with_a_fraction_rounds_like_the_rest_of_the_bill() {
        // The column is NUMERIC, so a fraction of a piastre can arrive.
        assert_eq!(discount_amount(5000, Discount::Fixed(dec!(250.5))), 251);
        assert_eq!(discount_amount(5000, Discount::Fixed(dec!(250.4))), 250);
    }

    #[test]
    fn only_a_dine_in_sale_carries_the_service_charge() {
        let p = TaxPolicy {
            tax_rate: dec!(0.14),
            tax_inclusive: false,
            service_charge_rate: dec!(0.12),
            service_charge_taxable: true,
        };
        assert_eq!(
            compute(10000, 0, &p.for_sale(SaleChannel::DineIn, false)).service_charge,
            1200
        );
        for ch in [
            SaleChannel::Takeaway,
            SaleChannel::Delivery,
            SaleChannel::Online,
        ] {
            let b = compute(10000, 0, &p.for_sale(ch, false));
            assert_eq!(
                (b.service_charge, b.tax, b.total),
                (0, 1400, 11400),
                "{ch:?}"
            );
        }
        let waived = compute(10000, 0, &p.for_sale(SaleChannel::DineIn, true));
        assert_eq!((waived.service_charge, waived.total), (0, 11400));
        for ch in [
            SaleChannel::DineIn,
            SaleChannel::Takeaway,
            SaleChannel::Delivery,
            SaleChannel::Online,
        ] {
            assert_eq!(SaleChannel::from_wire(ch.as_str()), Some(ch));
        }
        assert_eq!(SaleChannel::from_wire("counter"), None);
    }

    #[test]
    fn refunds_take_back_tax_and_service_pro_rata_and_add_up_exactly() {
        // Shared with madar-core `tax::tests` — the same table on both sides.
        let cases: &[(Minor, Minor, Minor, Minor, Minor, (Minor, Minor))] = &[
            (11400, 1400, 0, 0, 5700, (700, 0)),
            (11400, 1400, 0, 5700, 5700, (700, 0)),
            (12768, 1568, 1200, 0, 1000, (123, 94)),
            (12768, 1568, 1200, 1000, 11768, (1445, 1106)),
            (12768, 1568, 1200, 0, 12768, (1568, 1200)),
            (12768, 1568, 1200, 12000, 5000, (94, 72)),
            (333, 41, 0, 0, 1, (0, 0)),
            (333, 41, 0, 1, 1, (0, 0)),
            (333, 41, 0, 2, 331, (41, 0)),
            (0, 0, 0, 0, 100, (0, 0)),
        ];
        for &(total, tax, sc, before, amount, want) in cases {
            assert_eq!(
                refund_split(total, tax, sc, before, amount),
                want,
                "{total} {tax} {sc} {before} {amount}"
            );
        }
        // Any sequence of refunds that empties the order takes back all of it.
        let (total, tax, sc) = (98765, 12129, 8888);
        let mut before = 0;
        let (mut t, mut s) = (0, 0);
        for amount in [1, 333, 4999, 12345, 40000, 41087] {
            let (dt, ds) = refund_split(total, tax, sc, before, amount);
            t += dt;
            s += ds;
            before += amount;
        }
        assert_eq!((before, t, s), (total, tax, sc));
    }
}
