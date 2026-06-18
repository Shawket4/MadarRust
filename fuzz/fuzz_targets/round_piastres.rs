#![no_main]
//! Fuzz `round_piastres`: must never panic, and rounding is symmetric under
//! negation (MidpointAwayFromZero), except where the saturating
//! `to_i64().unwrap_or(0)` clamps an out-of-range magnitude to 0.

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use rust_decimal::Decimal;
use sufrix_rust::costing::round_piastres;

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let (Ok(mantissa), Ok(scale_raw)) = (i128::arbitrary(&mut u), u32::arbitrary(&mut u)) else {
        return;
    };
    // Decimal supports scale 0..=28; try_* avoids the constructor panicking on
    // out-of-range mantissa so we only ever exercise round_piastres itself.
    let Ok(d) = Decimal::try_from_i128_with_scale(mantissa, scale_raw % 29) else {
        return;
    };

    let r = round_piastres(d);

    if let Some(neg) = d.checked_mul(Decimal::NEGATIVE_ONE) {
        let rn = round_piastres(neg);
        // Exact symmetry when neither side saturated to 0.
        if r != 0 && rn != 0 {
            assert_eq!(r, -rn, "round_piastres asymmetric for {d}");
        }
    }
});
