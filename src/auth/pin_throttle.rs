//! A growing delay on wrong PINs — never a lock
//! (POS_SIGNIN_OVERHAUL.md §3.4, owner decision 2026-09-16).
//!
//! Account lockout does not apply here. A wrong PIN matches nobody, so there is
//! no account to lock; and the tablet is shared, so any hard lock stops the
//! shop, which is a likelier outcome than a real attack. What does work is
//! making each further guess slower: a teller who mistypes waits a moment, and
//! grinding a million six-digit combinations never gets anywhere.
//!
//! Counted against the PLACE, in two buckets:
//! - the **device**, for a single tablet being ground on;
//! - the **branch**, for someone moving between tablets in one shop.
//!
//! Old tablets send no device id, so they only ever touch the branch bucket;
//! their existing per-account throttling is untouched.
//!
//! This is a separate bucket from the per-address ceiling that was raised for
//! legitimate till traffic — that one is about volume, this one is about
//! failures.

use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;

/// Misses on ONE tablet that cost nothing. Typing a PIN wrong is ordinary.
pub const FREE_ATTEMPTS: i32 = 4;

/// Misses across a whole BRANCH that cost nothing. Higher than a device's
/// allowance because every tablet in the shop shares it: five people each
/// mistyping once must not slow down the sixth.
pub const BRANCH_FREE_ATTEMPTS: i32 = 12;

/// Counters older than this are forgotten: a shop that had a bad morning is not
/// still paying for it after lunch.
pub const DECAY_MINUTES: i64 = 15;

/// The wait after the n-th consecutive failure, in seconds. Deliberately gentle
/// at first and hard later: 5s, 15s, 45s, 120s, then 300s for ever.
pub fn delay_seconds(fails: i32) -> i64 {
    delay_after(fails, FREE_ATTEMPTS)
}

/// The same curve, starting after `free` misses.
pub fn delay_after(fails: i32, free: i32) -> i64 {
    match fails - free {
        i if i <= 0 => 0,
        1 => 5,
        2 => 15,
        3 => 45,
        4 => 120,
        _ => 300,
    }
}

fn buckets(device: Option<&str>, branch: Uuid) -> Vec<(&'static str, String)> {
    let mut v = vec![("branch", branch.to_string())];
    if let Some(d) = device.map(str::trim).filter(|d| !d.is_empty()) {
        v.push(("device", d.to_string()));
    }
    v
}

/// Seconds still to wait, if any. The longest of the buckets wins.
pub async fn remaining(
    pool: &PgPool,
    device: Option<&str>,
    branch: Uuid,
) -> Result<Option<i64>, AppError> {
    let mut worst = 0i64;
    for (kind, key) in buckets(device, branch) {
        let secs: Option<i64> = sqlx::query_scalar(
            "SELECT CEIL(EXTRACT(EPOCH FROM (blocked_until - now())))::bigint
               FROM pin_attempts
              WHERE kind = $1 AND key = $2 AND blocked_until > now()",
        )
        .bind(kind)
        .bind(&key)
        .fetch_optional(pool)
        .await?
        .flatten();
        worst = worst.max(secs.unwrap_or(0));
    }
    Ok((worst > 0).then_some(worst))
}

/// 429 with the remaining wait when one is owed, so the POS can run a countdown
/// instead of guessing.
pub async fn check(pool: &PgPool, device: Option<&str>, branch: Uuid) -> Result<(), AppError> {
    match remaining(pool, device, branch).await? {
        Some(seconds) => Err(AppError::PinThrottled { seconds }),
        None => Ok(()),
    }
}

/// One more miss. Best-effort: a bookkeeping failure must never turn a wrong
/// PIN into a 500.
pub async fn record_failure(pool: &PgPool, device: Option<&str>, branch: Uuid) {
    for (kind, key) in buckets(device, branch) {
        let sql = format!(
            "INSERT INTO pin_attempts (kind, key, fails, last_fail_at)
             VALUES ($1, $2, 1, now())
             ON CONFLICT (kind, key) DO UPDATE SET
                 -- A quiet spell forgets the run and starts again at one.
                 fails = CASE WHEN pin_attempts.last_fail_at < now() - interval '{DECAY_MINUTES} minutes'
                              THEN 1 ELSE pin_attempts.fails + 1 END,
                 last_fail_at = now()
             RETURNING fails"
        );
        let fails: Result<i32, _> = sqlx::query_scalar(&sql)
            .bind(kind)
            .bind(&key)
            .fetch_one(pool)
            .await;
        let Ok(fails) = fails else {
            tracing::warn!(kind, "pin attempt not counted");
            continue;
        };
        let free = if kind == "branch" {
            BRANCH_FREE_ATTEMPTS
        } else {
            FREE_ATTEMPTS
        };
        let wait = delay_after(fails, free);
        if wait > 0 {
            let _ = sqlx::query(
                "UPDATE pin_attempts SET blocked_until = now() + make_interval(secs => $3)
                  WHERE kind = $1 AND key = $2",
            )
            .bind(kind)
            .bind(&key)
            .bind(wait as f64)
            .execute(pool)
            .await;
        }
    }
}

/// A correct PIN clears the run — for the device that typed it and for its
/// branch, because the shop is evidently being used by people who know their
/// PINs.
pub async fn clear(pool: &PgPool, device: Option<&str>, branch: Uuid) {
    for (kind, key) in buckets(device, branch) {
        let _ = sqlx::query("DELETE FROM pin_attempts WHERE kind = $1 AND key = $2")
            .bind(kind)
            .bind(&key)
            .execute(pool)
            .await;
    }
}

/// A CORRECT PIN typed at a branch its holder may not sign in at — the one
/// failure with an identity behind it (§3.4). Counted per person and branch in
/// the owner's review queue (`authz_replay_flags`, reason `pin_wrong_branch`,
/// read by `GET /authz/flags`): one open row per person and branch, its
/// `details.attempts` growing, so a teller who tries twice is one item for the
/// owner, not two. Best-effort, like the rest of this module.
pub async fn record_wrong_branch(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    branch: Uuid,
    device: Option<&str>,
) {
    let details = serde_json::json!({ "attempts": 1, "device_id": device });
    let updated = sqlx::query(
        "UPDATE authz_replay_flags
            SET details = jsonb_set(
                    details || jsonb_build_object('device_id', $4::text),
                    '{attempts}',
                    to_jsonb(COALESCE((details->>'attempts')::int, 1) + 1)),
                occurred_at = now()
          WHERE org_id = $1 AND author_id = $2 AND branch_id = $3
            AND reason = 'pin_wrong_branch' AND reviewed_at IS NULL",
    )
    .bind(org_id)
    .bind(user_id)
    .bind(branch)
    .bind(device)
    .execute(pool)
    .await;
    let inserted = match updated {
        Ok(r) if r.rows_affected() > 0 => return,
        Ok(_) => {
            sqlx::query(
                "INSERT INTO authz_replay_flags
                     (org_id, branch_id, op, author_id, capability, reason, details)
                 VALUES ($1, $3, 'PinSignIn', $2, 'pos:sign_in', 'pin_wrong_branch', $4)",
            )
            .bind(org_id)
            .bind(user_id)
            .bind(branch)
            .bind(details)
            .execute(pool)
            .await
        }
        Err(e) => Err(e),
    };
    if let Err(e) = inserted {
        tracing::error!(error = %e, %user_id, %branch, "could not record a wrong-branch PIN");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_delay_is_free_at_first_then_grows_and_caps() {
        for n in 0..=FREE_ATTEMPTS {
            assert_eq!(delay_seconds(n), 0, "a mistype costs nothing ({n})");
        }
        assert_eq!(delay_seconds(5), 5);
        assert_eq!(delay_seconds(6), 15);
        assert_eq!(delay_seconds(7), 45);
        assert_eq!(delay_seconds(8), 120);
        // Capped: a long lock on a shared counter tablet is a denial of service
        // on the shop, which is the likelier harm.
        assert_eq!(delay_seconds(9), 300);
        assert_eq!(delay_seconds(500), 300);
    }

    #[test]
    fn the_branch_bucket_is_more_forgiving_than_a_tablet() {
        assert_eq!(delay_after(BRANCH_FREE_ATTEMPTS, BRANCH_FREE_ATTEMPTS), 0);
        assert_eq!(
            delay_after(BRANCH_FREE_ATTEMPTS + 1, BRANCH_FREE_ATTEMPTS),
            5
        );
        assert!(BRANCH_FREE_ATTEMPTS > FREE_ATTEMPTS);
    }
}
