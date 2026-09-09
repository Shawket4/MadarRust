//! Bringing cards already in a wallet up to date with the shop.
//!
//! A pass is a copy. It carries the branches you can use it at, baked in at the
//! moment it was issued, and the wallets only fetch a new one when we tell them
//! to. We told them on a balance change — awarding points, a redemption, a
//! manual adjustment — and nowhere else.
//!
//! So a shop opening a branch, or filling in coordinates for one it opened last
//! month, changed nothing for anybody already carrying a card. Their pass kept
//! the old list until they next bought something, and a customer who did not
//! come back kept it forever. The lock-screen prompt at the new branch never
//! appeared, which is the one thing the feature exists for.
//!
//! ## Why a sweep rather than a push on save
//!
//! Saving a branch would have to notify every member of the organisation at
//! once — thousands of pushes fired from inside an HTTP handler, with no way to
//! resume if the process restarts halfway. A sweep instead compares each
//! member's `pass_updated_at` against their org's most recent branch change and
//! works through the stale ones in batches. Several edits in an afternoon
//! coalesce into one refresh per customer, it picks up where it left off, and
//! it cannot stampede.
//!
//! Runs on the OWNER pool, which bypasses RLS — the sanctioned path for
//! cross-tenant background work. Every query is keyed by `org_id` regardless.

use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

use super::notices;
use crate::errors::AppError;

/// How many cards one tick refreshes.
///
/// Each is a push to Apple and a write to Google, so the cap is about being a
/// good citizen of both rather than about our own load. A backlog drains over
/// several ticks, which is fine: nobody is waiting on this.
const BATCH: i64 = 200;

const DEFAULT_TICK_SECS: u64 = 300;

pub fn spawn(pool: PgPool) {
    let disabled = std::env::var("LOYALTY_PASS_REFRESH_ENABLED")
        .map(|v| matches!(v.as_str(), "0" | "false" | "no" | "off"))
        .unwrap_or(false);
    if disabled {
        tracing::info!("Loyalty pass refresh disabled (LOYALTY_PASS_REFRESH_ENABLED)");
        return;
    }
    let secs = std::env::var("LOYALTY_PASS_REFRESH_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_TICK_SECS)
        .max(30);

    tracing::info!("Loyalty pass refresh started ({secs}s tick)");
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(secs));
        loop {
            ticker.tick().await;
            crate::observability::report::guarded_tick("loyalty_pass_refresh", || run_tick(&pool))
                .await;
        }
    });
}

async fn run_tick(pool: &PgPool) -> Result<(), AppError> {
    // A message we put on a card that the card never came back for. Riding this
    // tick rather than owning one: both are about passes that have not caught
    // up, and one sweep is one thing to reason about.
    notices::sweep_undelivered(pool).await?;

    let stale = stale_passes(pool, BATCH).await?;
    if stale.is_empty() {
        return Ok(());
    }
    tracing::info!(count = stale.len(), "loyalty: refreshing stale passes");

    for id in stale {
        // One card's failure is not the sweep's. `refresh_pass` stamps
        // `pass_updated_at` on success, so a card that failed is simply picked
        // up again on the next tick rather than skipped forever.
        if let Err(e) = super::refresh_pass(pool, id).await {
            tracing::warn!(customer_id = %id, error = %e, "loyalty: pass refresh failed");
        }
    }
    Ok(())
}

/// Members whose card predates their shop's most recent branch change, and who
/// are actually carrying one.
///
/// A member who never added a pass has nothing to refresh, and pushing to them
/// is a round trip that always finds no devices.
pub async fn stale_passes(pool: &PgPool, limit: i64) -> Result<Vec<Uuid>, AppError> {
    let stale: Vec<Uuid> = sqlx::query_scalar(
        "SELECT c.id \
           FROM loyalty_customers c \
           JOIN ( \
               SELECT org_id, MAX(updated_at) AS changed \
                 FROM branches WHERE deleted_at IS NULL GROUP BY org_id \
           ) b ON b.org_id = c.org_id \
          WHERE (c.pass_updated_at IS NULL OR c.pass_updated_at < b.changed) \
            AND ( \
                c.google_object_id IS NOT NULL \
                OR EXISTS (SELECT 1 FROM loyalty_pass_devices d WHERE d.customer_id = c.id) \
            ) \
          ORDER BY c.pass_updated_at NULLS FIRST \
          LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(stale)
}
