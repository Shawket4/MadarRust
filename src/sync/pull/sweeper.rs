//! R-sweep (§10.2): every 10 min, single-instance via an advisory lock.
//! 1. Time-based live-set exits (`kitchen_ticket`, `delivery`, `booking`) have no
//!    row write to fire a trigger, so the sweep emits their `delete`.
//! 2. Tombstones older than 30 days are purged and the branch watermark raised,
//!    so a device whose cursor predates the purge is told to resync.
//! 3. The mirror of (1): live rows with NO feed row are emitted as `upsert`.
//!    A branch backfills itself at creation (migration 20260922020000), but the
//!    sweep is what makes the feed self-healing — any hole from a path that
//!    forgets to emit, a restore, or a hand-written INSERT closes within ten
//!    minutes, and the devices pick the rows up on their next ordinary pull.
//!    Nobody reinstalls a till.
use std::time::Duration;

use sqlx::PgPool;

/// Advisory lock key for the sweep (arbitrary, stable).
const SWEEP_LOCK: i64 = 0x5359_4e43_5357_4550; // "SYNCSWEP"
const EVERY: Duration = Duration::from_secs(600);
const TIME_BASED: &[&str] = &["kitchen_ticket", "delivery", "booking"];

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub deletes_emitted: i64,
    /// Live rows that had no feed row at all — a feed hole, now closed.
    pub upserts_emitted: i64,
    pub tombstones_purged: i64,
}

pub fn spawn(pool: PgPool) {
    if std::env::var("SYNC_SWEEP_ENABLED")
        .map(|v| v == "false")
        .unwrap_or(false)
    {
        tracing::info!("sync changefeed sweep disabled (SYNC_SWEEP_ENABLED)");
        return;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(EVERY);
        loop {
            tick.tick().await;
            match sweep_once(&pool).await {
                Ok(r) if r != SweepReport::default() => tracing::info!(?r, "sync sweep"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "sync sweep failed"),
            }
        }
    });
}

/// One sweep pass. Returns zeroes when another instance holds the lock.
pub async fn sweep_once(pool: &PgPool) -> Result<SweepReport, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(SWEEP_LOCK)
        .fetch_one(&mut *conn)
        .await?;
    if !locked {
        return Ok(SweepReport::default());
    }
    let result = sweep_locked(&mut conn).await;
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(SWEEP_LOCK)
        .execute(&mut *conn)
        .await;
    result
}

async fn sweep_locked(conn: &mut sqlx::PgConnection) -> Result<SweepReport, sqlx::Error> {
    let types: Vec<String> = TIME_BASED.iter().map(|t| t.to_string()).collect();
    let deletes_emitted: i64 = sqlx::query_scalar(
        "WITH live AS (SELECT branch_id, type, entity_id FROM sync_live_rows() WHERE type = ANY($1)), \
              gone AS ( \
                SELECT c.branch_id, c.type, c.entity_id FROM sync_changes c \
                 WHERE c.op = 'upsert' AND c.type = ANY($1) \
                   AND NOT EXISTS (SELECT 1 FROM live l WHERE l.branch_id = c.branch_id AND l.type = c.type \
                                                         AND l.entity_id = c.entity_id)) \
         SELECT count(*) FROM (SELECT sync_emit(branch_id, type, entity_id, 'delete') FROM gone) x",
    )
    .bind(&types)
    .fetch_one(&mut *conn)
    .await?;

    // The mirror of the delete pass, over EVERY type: anything live that the
    // feed never heard of. This is what heals a branch whose rows were never
    // emitted — the org fan-out only ever reached the branches that existed at
    // write time, so a branch opened later had nothing for its org's payment
    // methods, menu, discounts or staff, and its devices read a perfectly
    // consistent empty snapshot.
    let upserts_emitted: i64 = sqlx::query_scalar(
        "WITH gap AS ( \
            SELECT l.branch_id, l.type, l.entity_id FROM sync_live_rows() l \
             WHERE NOT EXISTS (SELECT 1 FROM sync_changes c \
                                WHERE c.branch_id = l.branch_id AND c.type = l.type \
                                  AND c.entity_id = l.entity_id)) \
         SELECT count(*) FROM (SELECT sync_emit(branch_id, type, entity_id, 'upsert') FROM gap) x",
    )
    .fetch_one(&mut *conn)
    .await?;

    let purged: Vec<(uuid::Uuid, i64, i64)> = sqlx::query_as(
        "WITH dead AS ( \
            DELETE FROM sync_changes WHERE op = 'delete' AND changed_at < now() - interval '30 days' \
            RETURNING branch_id, seq) \
         SELECT branch_id, max(seq), count(*) FROM dead GROUP BY branch_id",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut tombstones_purged = 0;
    for (branch, max_seq, n) in purged {
        tombstones_purged += n;
        sqlx::query(
            "INSERT INTO sync_feed_watermarks (branch_id, purged_through_seq, updated_at) VALUES ($1, $2, now()) \
             ON CONFLICT (branch_id) DO UPDATE \
                SET purged_through_seq = GREATEST(sync_feed_watermarks.purged_through_seq, EXCLUDED.purged_through_seq), \
                    updated_at = now()",
        )
        .bind(branch)
        .bind(max_seq)
        .execute(&mut *conn)
        .await?;
    }
    Ok(SweepReport {
        deletes_emitted,
        upserts_emitted,
        tombstones_purged,
    })
}
