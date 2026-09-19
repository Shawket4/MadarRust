//! The staff drinks pool's arithmetic — the server's copy.
//!
//! This is the SECOND copy of a rule that must not disagree with the till. The
//! first lives in `madar/rust-core/crates/madar-core/src/staff_pool.rs`, which
//! carries the full design note; read it before changing anything here. The two
//! are pinned together by `staff_pool_vectors.json`, a fixture committed to
//! BOTH repos and executed by a test on each side, exactly as `tax_vectors.json`
//! pins the bill engine.
//!
//! The till decides offline and the server re-decides at replay. When they
//! disagree the SERVER's answer is the one recorded — but a disagreement never
//! rejects a staff drink that already happened. The drink lands, and the
//! difference is marked (`overspent`) and flagged for the owner, per the locked
//! accept-and-flag rule for money ops (PERMISSIONS_ARCHITECTURE §4.4.5).

use serde::{Deserialize, Serialize};

/// What the org (or a branch overriding it) allows.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct StaffPoolSettings {
    pub enabled: bool,
    pub daily_allowance: i32,
    /// EMPTY = nothing counts = the pool is off.
    pub eligible_item_ids: Vec<String>,
}

/// A branch's pool as it stands on one business day.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct StaffPoolDay {
    pub business_date: String,
    pub allowance: i32,
    pub used: i32,
    pub remaining: i32,
    pub over: i32,
}

/// Why a staff drink was refused. An overspend is never one of these.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StaffDrinkRefusal {
    PoolOff,
    NoEligibleItems,
    ItemNotEligible,
    NoteRequired,
}

impl StaffDrinkRefusal {
    /// The stable token recorded on a flag and read by reports.
    pub fn token(self) -> &'static str {
        match self {
            Self::PoolOff => "pool_off",
            Self::NoEligibleItems => "no_eligible_items",
            Self::ItemNotEligible => "item_not_eligible",
            Self::NoteRequired => "note_required",
        }
    }

    /// The refusal as the API states it.
    pub fn message(self) -> &'static str {
        match self {
            Self::PoolOff => "The staff pool is switched off for this branch",
            Self::NoEligibleItems => "No drinks are set for the staff pool yet",
            Self::ItemNotEligible => "This item is not on the staff pool list",
            Self::NoteRequired => "A staff drink needs a note saying who it is for",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct StaffDrinkDecision {
    pub allowed: bool,
    pub refusal: Option<StaffDrinkRefusal>,
    pub overspent: bool,
    pub pool: StaffPoolDay,
}

/// A note is a note when it has a non-whitespace character in it.
pub fn note_is_given(note: &str) -> bool {
    !note.trim().is_empty()
}

/// `remaining` never goes below zero and `over` never above it.
pub fn pool_state(business_date: &str, allowance: i32, used: i32) -> StaffPoolDay {
    let allowance = allowance.max(0);
    let used = used.max(0);
    StaffPoolDay {
        business_date: business_date.to_string(),
        allowance,
        used,
        remaining: (allowance - used).max(0),
        over: (used - allowance).max(0),
    }
}

/// THE decision. Byte-for-byte the rule the till runs; see the module note.
pub fn decide(
    settings: &StaffPoolSettings,
    business_date: &str,
    item_id: &str,
    note: &str,
    used: i32,
) -> StaffDrinkDecision {
    let refuse = |r: StaffDrinkRefusal| StaffDrinkDecision {
        allowed: false,
        refusal: Some(r),
        overspent: false,
        pool: pool_state(business_date, settings.daily_allowance, used),
    };

    if !settings.enabled {
        return refuse(StaffDrinkRefusal::PoolOff);
    }
    if settings.eligible_item_ids.is_empty() {
        return refuse(StaffDrinkRefusal::NoEligibleItems);
    }
    if !settings.eligible_item_ids.iter().any(|i| i == item_id) {
        return refuse(StaffDrinkRefusal::ItemNotEligible);
    }
    if !note_is_given(note) {
        return refuse(StaffDrinkRefusal::NoteRequired);
    }

    let after = used.max(0) + 1;
    StaffDrinkDecision {
        allowed: true,
        refusal: None,
        overspent: after > settings.daily_allowance.max(0),
        pool: pool_state(business_date, settings.daily_allowance, after),
    }
}

/// The branch-local business date of an instant — the same boundary
/// `service_day_bounds` draws, in the branch's effective timezone.
pub fn business_date_of(
    tz: chrono_tz::Tz,
    at: chrono::DateTime<chrono::Utc>,
) -> chrono::NaiveDate {
    use chrono::TimeZone as _;
    tz.from_utc_datetime(&at.naive_utc()).date_naive()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(enabled: bool, allowance: i32, items: &[&str]) -> StaffPoolSettings {
        StaffPoolSettings {
            enabled,
            daily_allowance: allowance,
            eligible_item_ids: items.iter().map(|s| s.to_string()).collect(),
        }
    }

    const D: &str = "2026-09-19";

    #[test]
    fn an_overspend_lands_and_is_marked_never_refused() {
        let d = decide(&settings(true, 5, &["latte"]), D, "latte", "n", 5);
        assert!(d.allowed);
        assert!(d.overspent);
        assert_eq!(d.pool.over, 1);
        assert_eq!(d.pool.remaining, 0);
    }

    #[test]
    fn a_note_is_required_and_whitespace_is_not_a_note() {
        let s = settings(true, 5, &["latte"]);
        assert_eq!(decide(&s, D, "latte", "  ", 0).refusal, Some(StaffDrinkRefusal::NoteRequired));
        assert!(decide(&s, D, "latte", "for Sara", 0).allowed);
    }

    #[test]
    fn an_empty_eligible_list_is_a_pool_that_is_off() {
        assert_eq!(
            decide(&settings(true, 5, &[]), D, "latte", "n", 0).refusal,
            Some(StaffDrinkRefusal::NoEligibleItems)
        );
    }

    #[test]
    fn the_business_day_turns_over_at_branch_midnight_not_utc() {
        use chrono::TimeZone as _;
        let cairo = chrono_tz::Africa::Cairo;
        let late = chrono::Utc.with_ymd_and_hms(2026, 9, 19, 22, 30, 0).unwrap();
        assert_eq!(business_date_of(cairo, late).to_string(), "2026-09-20");
        let before = chrono::Utc.with_ymd_and_hms(2026, 9, 19, 20, 59, 59).unwrap();
        assert_eq!(business_date_of(cairo, before).to_string(), "2026-09-19");
    }

    /// The boundary this pool resets on is the one the Z report already draws.
    /// If `service_day_bounds` ever moves, this fails and the pool moves with it.
    #[test]
    fn the_reset_boundary_is_the_same_one_service_day_bounds_uses() {
        use chrono::NaiveDate;
        let tz = chrono_tz::Africa::Cairo;
        for d in 18..=21 {
            let date = NaiveDate::from_ymd_opt(2026, 9, d).unwrap();
            let (start, end) = crate::bookings::handlers::service_day_bounds(tz, date);
            // Every instant inside the Z report's day is the pool's same day…
            assert_eq!(business_date_of(tz, start), date);
            assert_eq!(business_date_of(tz, end - chrono::Duration::seconds(1)), date);
            // …and the instant the Z report's day ends is already the next one.
            assert_eq!(business_date_of(tz, end), date.succ_opt().unwrap());
        }
    }
}
