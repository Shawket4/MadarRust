#![no_main]
//! Fuzz `units::convert`: must never panic on any qty/unit combination. Identity
//! conversion (same unit) returns the input within the 3-dp rounding floor.

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use madar_rust::units::convert;

const UNITS: &[&str] = &[
    "g", "kg", "mg", "ml", "l", "cl", "pcs", "piece", "unit", "G", "KG", " g ", "", "bogus",
];

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let (Ok(qty), Ok(i), Ok(j)) =
        (f64::arbitrary(&mut u), usize::arbitrary(&mut u), usize::arbitrary(&mut u))
    else {
        return;
    };
    let from = UNITS[i % UNITS.len()];
    let to = UNITS[j % UNITS.len()];

    // Primary property: convert must not panic for ANY input.
    let _ = convert(qty, from, to);

    // Identity conversion is well-defined for a valid unit: result ≈ qty.
    if qty.is_finite() && qty.abs() < 1e12 {
        if let Ok(out) = convert(qty, from, from) {
            assert!((out - qty).abs() <= 0.001, "identity {qty} {from} -> {out}");
        }
    }
});
