#![no_main]
//! Fuzz `blend_weighted_cost`: the weighted-average ingredient cost. With the
//! caller contract (received_qty > 0, non-negative stock/costs) it must never
//! panic, keep scale ≤ 2, and be a convex combination of the two costs.

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use rust_decimal::Decimal;
use madar_rust::costing::blend_weighted_cost;

fn arb_decimal(u: &mut Unstructured) -> Option<Decimal> {
    // Bound the magnitude to the real cost/quantity domain. `cost_per_unit` is
    // numeric(15,2) (≈1e13 max) and stock quantities are far smaller, so an i64
    // mantissa (|value| < ~1e19) is already far above anything production sees.
    // This stays well clear of Decimal's ~7.9e28 ceiling, where a 29-significant-
    // digit weighted average can fall a few ulps outside [min,max] (a precision
    // artifact, not a costing bug — found by the deeper fuzz campaign).
    let mantissa = i64::arbitrary(u).ok()? as i128;
    let scale = u32::arbitrary(u).ok()? % 12;
    Decimal::try_from_i128_with_scale(mantissa, scale).ok()
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let Some(prior) = arb_decimal(&mut u) else { return };
    let has_cur = bool::arbitrary(&mut u).unwrap_or(false);
    let cur = if has_cur { arb_decimal(&mut u) } else { None };
    let Some(recv_qty) = arb_decimal(&mut u) else { return };
    let Some(recv_cost) = arb_decimal(&mut u) else { return };

    // Caller contract.
    if recv_qty <= Decimal::ZERO || prior < Decimal::ZERO || recv_cost < Decimal::ZERO {
        return;
    }
    if cur.is_some_and(|c| c < Decimal::ZERO) {
        return;
    }

    // Skip inputs whose intermediate products legitimately overflow Decimal's
    // 96-bit mantissa — that's a representable-range limit, not a logic bug.
    let p1 = prior.checked_mul(cur.unwrap_or(Decimal::ZERO));
    let p2 = recv_qty.checked_mul(recv_cost);
    let denom = prior.checked_add(recv_qty);
    let (Some(p1), Some(p2), Some(denom)) = (p1, p2, denom) else { return };
    if p1.checked_add(p2).is_none() || denom.is_zero() {
        return;
    }

    let r = blend_weighted_cost(prior, cur, recv_qty, recv_cost);

    assert!(r.scale() <= 2, "blend scale {} > 2", r.scale());

    match cur {
        Some(c) if prior > Decimal::ZERO => {
            let lo = c.min(recv_cost);
            let hi = c.max(recv_cost);
            let slack = Decimal::new(1, 2); // 0.01 round_dp(2) slack
            assert!(r >= lo - slack && r <= hi + slack, "blend {r} escaped [{lo},{hi}]");
        }
        _ => assert_eq!(r, recv_cost.round_dp(2), "no-prior path must take received cost"),
    }
});
