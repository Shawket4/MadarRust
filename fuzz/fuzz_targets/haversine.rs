#![no_main]
//! Fuzz `haversine_meters`: must never panic. For valid WGS84 coordinates the
//! distance is in [0, πR], symmetric, and zero for identical points. (Near-
//! antipodal points can yield NaN from floating-point error; range/symmetry are
//! only asserted when the result is finite, but the call must still not panic.)

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use sufrix_rust::geo::osrm::{haversine_meters, LatLng};

const MAX_DIST: f64 = std::f64::consts::PI * 6_371_000.0; // half Earth circumference

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let (Ok(a_lat), Ok(a_lng), Ok(b_lat), Ok(b_lng)) = (
        f64::arbitrary(&mut u),
        f64::arbitrary(&mut u),
        f64::arbitrary(&mut u),
        f64::arbitrary(&mut u),
    ) else {
        return;
    };

    // Must not panic on raw (possibly non-finite) input.
    let _ = haversine_meters(LatLng { lat: a_lat, lng: a_lng }, LatLng { lat: b_lat, lng: b_lng });

    if [a_lat, a_lng, b_lat, b_lng].iter().all(|v| v.is_finite()) {
        let a = LatLng { lat: a_lat.clamp(-90.0, 90.0), lng: a_lng.clamp(-180.0, 180.0) };
        let b = LatLng { lat: b_lat.clamp(-90.0, 90.0), lng: b_lng.clamp(-180.0, 180.0) };

        let d = haversine_meters(a, b);
        if d.is_finite() {
            assert!(d >= 0.0 && d <= MAX_DIST + 1.0, "haversine {d} out of [0,{MAX_DIST}]");
            let d2 = haversine_meters(b, a);
            if d2.is_finite() {
                assert!((d - d2).abs() <= 1e-3, "haversine asymmetric {d} vs {d2}");
            }
        }
        // Self-distance is always a well-defined zero.
        let d0 = haversine_meters(a, a);
        assert!(d0.abs() < 1e-3, "haversine self-distance {d0}");
    }
});
