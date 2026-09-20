//! The staff pool rule, executed from the fixture both repos share.
//!
//! `tests/fixtures/staff_pool_vectors.json` is committed here AND in
//! `madar/rust-core/crates/madar-core/tests/fixtures/`, and the POS runs the
//! identical assertions in `tests/staff_pool_vectors.rs` against its copy of
//! the engine. That is what stops the till and the server from drifting apart
//! on what a staff drink costs the pool — the same arrangement
//! `tax_vectors.json` gives the bill. Change the rule, change the fixture, and
//! let both sides fail until they agree.

use madar_rust::staff_pool::engine::{self, StaffPoolSettings};
use serde_json::Value;

fn vectors() -> Value {
    let raw = include_str!("fixtures/staff_pool_vectors.json");
    serde_json::from_str(raw).expect("staff_pool_vectors.json is valid JSON")
}

fn settings_of(v: &Value) -> StaffPoolSettings {
    StaffPoolSettings {
        enabled: v["enabled"].as_bool().unwrap(),
        daily_allowance: v["daily_allowance"].as_i64().unwrap() as i32,
        eligible_item_ids: v["eligible_item_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i.as_str().unwrap().to_string())
            .collect(),
    }
}

#[test]
fn the_shared_vectors_decide_exactly_as_this_side_does() {
    let doc = vectors();
    let cases = doc["cases"].as_array().expect("cases");
    assert!(!cases.is_empty(), "the fixture must actually contain cases");

    for c in cases {
        let name = c["name"].as_str().unwrap();
        let d = engine::decide(
            &settings_of(&c["settings"]),
            c["business_date"].as_str().unwrap(),
            c["item_id"].as_str().unwrap(),
            c["note"].as_str().unwrap(),
            c["used"].as_i64().unwrap() as i32,
        );
        let want = &c["expect"];

        assert_eq!(d.allowed, want["allowed"].as_bool().unwrap(), "[{name}] allowed");
        assert_eq!(d.refusal.map(|r| r.token()), want["refusal"].as_str(), "[{name}] refusal");
        assert_eq!(d.overspent, want["overspent"].as_bool().unwrap(), "[{name}] overspent");

        let p = &want["pool"];
        assert_eq!(d.pool.business_date, p["business_date"].as_str().unwrap(), "[{name}] date");
        assert_eq!(d.pool.allowance, p["allowance"].as_i64().unwrap() as i32, "[{name}] allowance");
        assert_eq!(d.pool.used, p["used"].as_i64().unwrap() as i32, "[{name}] used");
        assert_eq!(d.pool.remaining, p["remaining"].as_i64().unwrap() as i32, "[{name}] remaining");
        assert_eq!(d.pool.over, p["over"].as_i64().unwrap() as i32, "[{name}] over");
    }
}

#[test]
fn the_shared_vectors_place_the_business_day_exactly_as_this_side_does() {
    let doc = vectors();
    let cases = doc["business_date_cases"].as_array().expect("business_date_cases");
    assert!(!cases.is_empty());

    for c in cases {
        let name = c["name"].as_str().unwrap();
        let tz: chrono_tz::Tz = c["tz"].as_str().unwrap().parse().expect("a real timezone");
        let at = chrono::DateTime::parse_from_rfc3339(c["at"].as_str().unwrap())
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            engine::business_date_of(tz, at).to_string(),
            c["expect"].as_str().unwrap(),
            "[{name}]"
        );
    }
}

/// An overspend must never turn into a refusal: the drink was already made.
#[test]
fn no_vector_ever_refuses_a_drink_for_being_over_the_allowance() {
    for c in vectors()["cases"].as_array().unwrap() {
        let e = &c["expect"];
        if e["overspent"].as_bool().unwrap() {
            assert!(
                e["allowed"].as_bool().unwrap(),
                "[{}] an overspend must land, never be refused",
                c["name"].as_str().unwrap()
            );
        }
    }
}

/// The fixture in this repo and the one the POS ships must be the same bytes.
/// A vectors file that has drifted pins nothing.
#[test]
fn both_repos_carry_the_same_fixture() {
    let here = include_str!("fixtures/staff_pool_vectors.json");
    let pos = std::path::Path::new("../madar/rust-core/crates/madar-core/tests/fixtures/staff_pool_vectors.json");
    // The POS checkout is not always beside this one (CI clones this repo
    // alone), so this is a check when it is there and a silent pass when not.
    if let Ok(theirs) = std::fs::read_to_string(pos) {
        assert_eq!(
            here, theirs,
            "staff_pool_vectors.json has drifted between the two repos — copy one over the other"
        );
    }
}
