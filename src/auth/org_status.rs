//! Org-suspension enforcement.
//!
//! An org can be suspended (its `is_active` flag cleared) or soft-deleted
//! (`deleted_at` set) — see [`crate::orgs::handlers`]. Once that happens every
//! authenticated request scoped to that org must be rejected, not just future
//! logins: existing JWTs stay valid until they expire, so a login-time check
//! alone would let a suspended org keep operating for hours.
//!
//! The check lives in [`crate::auth::middleware::JwtMiddleware`], which is the
//! single chokepoint every authenticated request already passes through. To
//! avoid a DB round-trip on every request, the resolved flag is cached per org
//! for a short TTL — suspension is rare, so this keeps the steady-state cost at
//! ~nil while bounding how long a just-suspended org keeps serving live
//! requests. The org-mutating handlers call [`OrgStatusCache::invalidate`] so a
//! suspension or reactivation takes effect immediately rather than after the TTL.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sqlx::PgPool;
use uuid::Uuid;

/// How long a resolved org-active flag stays cached before it is re-read.
const TTL: Duration = Duration::from_secs(30);

struct Entry {
    allowed: bool,
    fetched_at: Instant,
}

/// Per-process cache of "may this org serve requests" (active AND not
/// soft-deleted), keyed by org id. Registered as shared `web::Data`, so a
/// single instance is cloned into every worker.
#[derive(Default)]
pub struct OrgStatusCache {
    entries: Mutex<HashMap<Uuid, Entry>>,
}

impl OrgStatusCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `org_id` may serve requests. Reads through to the DB on a cold
    /// or expired entry and caches the result. Lock poisoning is recovered from
    /// rather than propagated — a panicked holder must not wedge auth.
    pub async fn is_allowed(&self, pool: &PgPool, org_id: Uuid) -> Result<bool, sqlx::Error> {
        if let Some(allowed) = self.fresh(org_id) {
            return Ok(allowed);
        }
        let allowed = org_is_allowed(pool, org_id).await?;
        let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        map.insert(
            org_id,
            Entry {
                allowed,
                fetched_at: Instant::now(),
            },
        );
        Ok(allowed)
    }

    fn fresh(&self, org_id: Uuid) -> Option<bool> {
        let map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&org_id)
            .filter(|e| e.fetched_at.elapsed() < TTL)
            .map(|e| e.allowed)
    }

    /// Drop the cached entry for `org_id` so the next request re-reads the DB.
    /// Call after toggling an org's active/deleted state so the change takes
    /// effect immediately instead of waiting out the TTL.
    pub fn invalidate(&self, org_id: Uuid) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&org_id);
    }
}

/// Single source of truth for "may this org serve requests": it exists, is
/// active, and is not soft-deleted. A missing row (hard-deleted, or a token for
/// an org that never existed) resolves to `false`. Used directly at login and
/// read through by [`OrgStatusCache`] per request.
pub async fn org_is_allowed(pool: &PgPool, org_id: Uuid) -> Result<bool, sqlx::Error> {
    let allowed: Option<bool> = sqlx::query_scalar(
        "SELECT (is_active AND deleted_at IS NULL) FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    Ok(allowed.unwrap_or(false))
}
