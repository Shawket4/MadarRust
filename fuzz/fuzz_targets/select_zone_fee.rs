#![no_main]
//! Fuzz `select_zone_fee`: out-of-range past max_dist; otherwise the SMALLEST
//! covering ring's fee wins (or OutOfRange when no ring covers). Never panics.

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use sufrix_rust::delivery::public::{select_zone_fee, FeeOutcome, ZoneRow};
use uuid::Uuid;

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let Ok(n) = u8::arbitrary(&mut u) else { return };
    let n = (n % 16) as usize;

    let mut zones: Vec<ZoneRow> = Vec::with_capacity(n);
    for _ in 0..n {
        let (Ok(fee), Ok(maxd)) = (i32::arbitrary(&mut u), i32::arbitrary(&mut u)) else {
            return;
        };
        zones.push(ZoneRow { id: Uuid::nil(), name: String::new(), fee, max_road_distance_meters: maxd });
    }
    // Precondition: rings ordered by distance ascending (smallest covering wins).
    zones.sort_by_key(|z| z.max_road_distance_meters);

    let Ok(distance) = i32::arbitrary(&mut u) else { return };
    let has_max = bool::arbitrary(&mut u).unwrap_or(false);
    let max_dist = if has_max { i32::arbitrary(&mut u).ok() } else { None };

    let outcome = select_zone_fee(distance, max_dist, &zones);

    if let Some(m) = max_dist {
        if distance > m {
            assert!(matches!(outcome, FeeOutcome::OutOfRange), "expected OutOfRange beyond max_dist");
            return;
        }
    }

    let smallest_covering = zones.iter().find(|z| z.max_road_distance_meters >= distance);
    match (smallest_covering, outcome.fee()) {
        (Some(z), Some(f)) => assert_eq!(z.fee, f, "wrong ring selected"),
        (None, None) => {} // no covering ring → OutOfRange (fee() == None)
        (Some(_), None) => panic!("covering ring exists but no fee returned"),
        (None, Some(_)) => panic!("fee returned with no covering ring"),
    }
});
