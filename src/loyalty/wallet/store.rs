//! Built passes, kept so a tap serves bytes instead of making them.
//!
//! See `migrations/*_loyalty_pass_cache.sql` for why this is a table and not
//! the asset store, and for the two independent guards against serving a pass
//! that is silently wrong.
//!
//! **Everything here is best-effort.** A cache read that fails is a miss, a
//! cache write that fails is a log line. The only thing this module is allowed
//! to do to a customer is make their card arrive faster.

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;
use crate::loyalty::model::MemberRow;

/// How long stored bytes are served for.
///
/// One hour, deliberately the same hour as `apple::STRIP_CACHE` and
/// `apple::BRAND_CACHE`: the stored pass can then never be staler than the
/// images that were baked into it, so a single number covers every org-level
/// input — palette, logo, card photograph, branch list — without a trigger or
/// a notification anywhere in the codebase.
const TTL: chrono::Duration = chrono::Duration::hours(1);

/// Everything member-specific the pass DRAWS, hashed.
///
/// Not the whole row: `pass_updated_at` and the wallet ids change without
/// changing a pixel, and hashing them would throw the cache away on its own
/// bookkeeping. What is here is what a customer would see change.
fn fingerprint(member: &MemberRow) -> String {
    let mut h = Sha256::new();
    h.update(member.points_balance.to_le_bytes());
    h.update(member.visits_balance.to_le_bytes());
    h.update(member.lifetime_points.to_le_bytes());
    h.update(member.lifetime_visits.to_le_bytes());
    h.update(member.name.as_bytes());
    h.update([0]);
    h.update(member.locale.as_bytes());
    h.update([0]);
    h.update(member.pass_notice.as_deref().unwrap_or("").as_bytes());
    h.update([0]);
    h.update(member.member_token.as_bytes());
    format!("{:x}", h.finalize())
}

/// Stored bytes for this member, if they are still the right ones.
///
/// The fingerprint comparison costs nothing: the caller already holds the
/// member row, so this is one indexed primary-key lookup and a string compare.
pub async fn load(pool: &PgPool, member: &MemberRow) -> Option<Vec<u8>> {
    // Caching is bypassed under test for the reason in `crate::cache`: one
    // process runs many isolated databases. Here the table is per-database so
    // there is no leak, but the tests assert on a FRESHLY BUILT pass, and a
    // test that mutated a balance and read the pass back would otherwise be
    // asserting against whatever the previous assertion stored.
    if cfg!(test) {
        return None;
    }
    let row: Option<(Vec<u8>, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT bytes, fingerprint, built_at FROM loyalty_pass_cache \
          WHERE customer_id = $1 AND kind = 'apple'",
    )
    .bind(member.id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    let (bytes, stored, built_at) = row?;
    if stored != fingerprint(member) {
        return None;
    }
    if chrono::Utc::now() - built_at > TTL {
        return None;
    }
    Some(bytes)
}

/// Keep these bytes for the next tap. Failure is a log line, never an error.
pub async fn save(pool: &PgPool, member: &MemberRow, bytes: &[u8]) {
    if cfg!(test) {
        return;
    }
    let res = sqlx::query(
        "INSERT INTO loyalty_pass_cache (customer_id, org_id, kind, bytes, fingerprint, built_at) \
         VALUES ($1, $2, 'apple', $3, $4, now()) \
         ON CONFLICT (customer_id) DO UPDATE \
            SET bytes = EXCLUDED.bytes, fingerprint = EXCLUDED.fingerprint, \
                built_at = EXCLUDED.built_at, org_id = EXCLUDED.org_id",
    )
    .bind(member.id)
    .bind(member.org_id)
    .bind(bytes)
    .bind(fingerprint(member))
    .execute(pool)
    .await;
    if let Err(e) = res {
        tracing::warn!(
            customer_id = %member.id, error = %e,
            "loyalty: could not store the built pass; the next tap will rebuild it"
        );
    }
}

/// Throw away one member's stored pass.
///
/// Called from `apple::notify_devices`, which is the one place in the codebase
/// that means "this member's pass is no longer what we last built". Putting it
/// THERE rather than at each of the places that changes a balance is the whole
/// reason there is no second trigger to keep in step: any future caller that
/// remembers to tell the devices automatically drops the stale bytes too, and
/// one that forgets was already broken for a different reason.
pub async fn invalidate(pool: &PgPool, customer_id: Uuid) {
    let _ = sqlx::query("DELETE FROM loyalty_pass_cache WHERE customer_id = $1")
        .bind(customer_id)
        .execute(pool)
        .await;
}

/// Throw away every stored pass of one shop.
///
/// For the edits an owner makes and then immediately checks on their own card:
/// saving the loyalty settings. Org-level BRANDING is left to the TTL instead,
/// because the images inside the pass are themselves cached for that same hour
/// — purging here would hand back a pass rebuilt around stale strips, which is
/// slower AND no fresher.
pub async fn purge_org(pool: &PgPool, org_id: Uuid) {
    let _ = sqlx::query("DELETE FROM loyalty_pass_cache WHERE org_id = $1")
        .bind(org_id)
        .execute(pool)
        .await;
}

/// Drop everything past its TTL.
///
/// Without this the table grows to one row per member who ever tapped, at a few
/// hundred KB each, on a box where Postgres shares the disk and the vCPU with
/// everything else. With it the resident set is only members active in the last
/// hour, which is the only set the cache can serve anyway.
pub async fn sweep(pool: &PgPool) -> Result<u64, AppError> {
    let done =
        sqlx::query("DELETE FROM loyalty_pass_cache WHERE built_at < now() - interval '1 hour'")
            .execute(pool)
            .await?;
    Ok(done.rows_affected())
}
