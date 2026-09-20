//! How expensive password hashing is, and the one place that is decided.
//!
//! bcrypt at [`bcrypt::DEFAULT_COST`] takes roughly 450 ms per hash by design —
//! that slowness is the whole point, and production must keep paying it. The
//! test suite pays it too, and it dominated everything else: an empty
//! `#[sqlx::test]` costs 0.23 s, while an orders test that creates a handful of
//! users through the real endpoints costs 6.5 s. Measured 2026-09-20; hashing,
//! not fixtures and not `CREATE DATABASE`, is where the suite's time went.
//!
//! So the cost is a function rather than a constant. It is weakened ONLY when
//! both of these hold:
//!
//!  1. the build has `debug_assertions` — a release build, which is what runs
//!     in production, compiles the weak branch out entirely; and
//!  2. `MADAR_FAST_TEST_HASHING` is set in the environment.
//!
//! Neither alone is enough. No configuration of a production binary can reach
//! the cheap cost, because the code for it is not in that binary.

/// The bcrypt cost to hash a new password with.
pub fn bcrypt_cost() -> u32 {
    #[cfg(debug_assertions)]
    {
        if std::env::var_os("MADAR_FAST_TEST_HASHING").is_some() {
            // 4 is bcrypt's minimum. Verification reads the cost out of the
            // stored hash, so a hash written at 4 still verifies normally and
            // no other code has to know this happened.
            return 4;
        }
    }
    bcrypt::DEFAULT_COST
}

#[cfg(test)]
mod tests {
    /// With the variable unset we are at the real cost, whatever the build.
    #[test]
    fn the_default_is_always_the_real_cost() {
        assert_eq!(super::bcrypt_cost(), bcrypt::DEFAULT_COST);
    }
}
