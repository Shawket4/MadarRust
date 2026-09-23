//! The staff drinks pool's arithmetic.
//!
//! The rule lives in madar-shared (`madar_money::staff_pool`), the one copy the
//! till (deciding offline) and the server (re-deciding at replay) both run,
//! pinned by its `staff_pool_vectors.json`. When they disagree the SERVER's
//! answer is the one recorded — but a disagreement never rejects a staff drink
//! that already happened: it lands, marked `overspent` and flagged for the
//! owner (PERMISSIONS_ARCHITECTURE §4.4.5).

pub use madar_money::staff_pool::{
    StaffDrinkDecision, StaffDrinkRefusal, StaffPoolDay, StaffPoolSettings, business_date_of,
    decide, note_is_given, pool_state,
};

#[cfg(test)]
mod tests {
    use super::*;

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
            assert_eq!(
                business_date_of(tz, end - chrono::Duration::seconds(1)),
                date
            );
            // …and the instant the Z report's day ends is already the next one.
            assert_eq!(business_date_of(tz, end), date.succ_opt().unwrap());
        }
    }
}
