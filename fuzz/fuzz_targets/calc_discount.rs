#![no_main]
//! Fuzz `calc_discount`: the result is always within [0, subtotal]; unknown/None
//! types never discount; "fixed" is min(value, subtotal) clamped.
//!
//! PRECONDITION: subtotal >= 0. `calc_discount` ends in `clamp(0, subtotal)`,
//! which panics when subtotal < 0 — a real latent footgun, but cart subtotals
//! are non-negative by construction, so we respect the contract here.

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use sufrix_rust::discounts::handlers::calc_discount;

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let (Ok(kind), Ok(value), Ok(subtotal)) =
        (u8::arbitrary(&mut u), i32::arbitrary(&mut u), i32::arbitrary(&mut u))
    else {
        return;
    };
    if subtotal < 0 {
        return; // out of contract (would panic in clamp)
    }
    let dtype = match kind % 4 {
        0 => Some("percentage"),
        1 => Some("fixed"),
        2 => Some("garbage"),
        _ => None,
    };

    let d = calc_discount(dtype, value, subtotal);

    assert!(d >= 0 && d <= subtotal, "discount {d} outside [0,{subtotal}]");
    if matches!(dtype, Some("garbage") | None) {
        assert_eq!(d, 0, "unknown discount type must be 0");
    }
    if dtype == Some("fixed") {
        assert_eq!(d, value.min(subtotal).clamp(0, subtotal), "fixed discount mismatch");
    }
});
