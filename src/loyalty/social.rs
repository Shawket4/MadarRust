//! "N+ people already have this card": the join page's social proof.
//!
//! Shown only for a programme with MORE THAN 100 live members (owner,
//! 2026-10-23), and never as an exact figure: the count is floored to a "nice"
//! step, so a public page cannot be probed for how many members a shop has, or
//! for when one joins or leaves.
//!
//! **The rule** ([`display_floor`]): the largest of 1, 1.5, 2, 2.5, 3, 4, 5,
//! 7.5 × 10^k that is not above the count; nothing at 100 or fewer. So 101-149
//! show 100, 150-199 show 150, 200-249 show 200, ..., 750-999 show 750, 1000
//! shows 1000, 1500, 2000, 2500, 3000, 4000, 5000, 7500, 10000, ...
//!
//! **Cost**: one indexed count per org per [`TTL`] at most, never per page
//! view. The count is the programme's own definition of a member
//! (`loyalty_memberships`, `deleted_at IS NULL`: leavers, merged-away and
//! erased cards all carry `deleted_at`) and is served by the partial index
//! `idx_loyalty_customers_org (org_id) WHERE deleted_at IS NULL`. Cached like
//! the wallet's per-org strips and brand: a moka entry per org with a TTL, no
//! write-side invalidation (a floored figure moving a few minutes late is
//! invisible), concurrent misses for one org coalesced into one query.

use std::sync::LazyLock;
use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;

/// Members at or below which nothing is shown.
pub const THRESHOLD: i64 = 100;

/// How long a shop's figure is reused.
pub const TTL: Duration = Duration::from_secs(10 * 60);

/// The figure shown for `count` live members, or `None` (show nothing).
pub fn display_floor(count: i64) -> Option<i64> {
    if count <= THRESHOLD {
        return None;
    }
    // Steps in tenths: 1, 1.5, 2, 2.5, 3, 4, 5, 7.5 (x 10^k).
    const STEPS: [i64; 8] = [10, 15, 20, 25, 30, 40, 50, 75];
    let mut scale: i64 = 10; // step 10 (tenths) x 10 = 100
    let mut best = THRESHOLD;
    loop {
        for s in STEPS {
            match s.checked_mul(scale) {
                Some(v) if v <= count => best = v,
                _ => return Some(best),
            }
        }
        match scale.checked_mul(10) {
            Some(next) => scale = next,
            None => return Some(best),
        }
    }
}

static CACHE: LazyLock<moka::future::Cache<Uuid, Option<i64>>> = LazyLock::new(|| {
    moka::future::Cache::builder()
        .max_capacity(20_000)
        .time_to_live(TTL)
        .build()
});

async fn live_members(pool: &PgPool, org_id: Uuid) -> Result<i64, AppError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM loyalty_memberships
            WHERE org_id = $1 AND deleted_at IS NULL"#,
        org_id
    )
    .fetch_one(pool)
    .await?)
}

/// The join page's figure for this org: [`display_floor`] of its live members,
/// cached per org for [`TTL`]. A failed count is "show nothing" (a decoration
/// must never fail the page) and is not cached.
pub async fn member_count(pool: &PgPool, org_id: Uuid) -> Option<i64> {
    if let Some(hit) = CACHE.get(&org_id).await {
        return hit;
    }
    match live_members(pool, org_id).await {
        Ok(n) => {
            let shown = display_floor(n);
            CACHE.insert(org_id, shown).await;
            shown
        }
        Err(e) => {
            tracing::warn!(%org_id, error = %e, "loyalty: member count for the join page failed");
            None
        }
    }
}

/// Forget an org's cached figure (tests, and anything that must show a change
/// at once).
pub async fn forget(org_id: Uuid) {
    CACHE.invalidate(&org_id).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_at_or_below_a_hundred_then_nice_steps_rounded_down() {
        for n in [i64::MIN, -1, 0, 1, 99, 100] {
            assert_eq!(display_floor(n), None, "{n}");
        }
        let cases = [
            (101, 100), (149, 100), (150, 150), (199, 150), (200, 200), (249, 200),
            (250, 250), (299, 250), (300, 300), (399, 300), (400, 400), (499, 400),
            (500, 500), (749, 500), (750, 750), (999, 750), (1000, 1000), (1499, 1000),
            (1500, 1500), (2000, 2000), (2600, 2500), (3999, 3000), (4000, 4000),
            (7499, 5000), (7500, 7500), (10_000, 10_000), (123_456, 100_000),
        ];
        for (n, want) in cases {
            assert_eq!(display_floor(n), Some(want), "{n}");
        }
        // Never above the truth, and never the exact count past the first step.
        for n in 101..20_000 {
            let s = display_floor(n).unwrap();
            assert!(s <= n && s >= 100, "{n} -> {s}");
        }
        assert!(display_floor(i64::MAX).is_some(), "no overflow");
    }
}
