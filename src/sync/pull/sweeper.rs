//! R-sweep (§10.2): every 10 min, single-instance via an advisory lock.
//! 1. Time-based live-set exits (`kitchen_ticket`, `delivery`, `booking`) have no
//!    row write to fire a trigger, so the sweep emits their `delete`. Each
//!    feed row's own entity is probed; the whole live set is never built.
//! 2. Tombstones older than 30 days are purged and the branch watermark raised,
//!    so a device whose cursor predates the purge is told to resync.
//! 3. Ledger retention (DB audit Phase 6): ledger rows (`LEDGER_TYPES`) older
//!    than [`retention`] — the full snapshot's ledger window plus slack — are
//!    purged the same way, unless a full snapshot could still ask for them (the
//!    till is open or closed inside the retention). The changefeed stops
//!    growing with history; the ledger tables themselves are never touched.
//!    An old feed row whose source changed inside the retention without
//!    re-emitting is re-emitted first (`ledger_rows_refreshed`), so the rows
//!    left old after a pass are only an open till's history.
//! 4. The mirror of (1): live rows with NO feed row are emitted as `upsert`.
//!    A branch backfills itself at creation (migration 20260922020000), but the
//!    sweep is what makes the feed self-healing — any hole from a path that
//!    forgets to emit, a restore, or a hand-written INSERT closes within ten
//!    minutes, and the devices pick the rows up on their next ordinary pull.
//!    Nobody reinstalls a till.
//!
//!    INCREMENTAL (DB audit Phase 6): per branch, over
//!    `sync_live_rows_since(branch, retention start)` (its state plus its recent
//!    ledger, never its history), and only for a branch whose feed head moved
//!    past its `sync_sweep_state.checked_through_seq` watermark since the last
//!    pass, or whose last check is older than
//!    `SYNC_SWEEP_IDLE_RECHECK_MINUTES` (default 360). A hole on an idle
//!    branch therefore closes within that interval instead of ten minutes; a
//!    branch with any feed activity is checked every pass.
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// Advisory lock key for the sweep (arbitrary, stable).
const SWEEP_LOCK: i64 = 0x5359_4e43_5357_4550; // "SYNCSWEP"
const EVERY: Duration = Duration::from_secs(600);
const TIME_BASED: &[&str] = &["kitchen_ticket", "delivery", "booking"];
/// Rows deleted per statement by the ledger purge (§10 batching).
const PURGE_BATCH: i64 = 5000;
/// Default slack on top of the 48 h ledger window: 14 days.
const DEFAULT_SLACK_HOURS: i64 = 14 * 24;
const DEFAULT_IDLE_RECHECK_MINUTES: i64 = 360;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub deletes_emitted: i64,
    /// Live rows that had no feed row at all — a feed hole, now closed.
    pub upserts_emitted: i64,
    pub tombstones_purged: i64,
    /// Ledger feed rows older than the retention, purged (Phase 6).
    pub ledger_rows_purged: i64,
    /// Old ledger feed rows whose source changed inside the retention without
    /// re-emitting (a write that forgot to): re-emitted, so they are current
    /// and leave the old set instead of being re-examined every pass.
    pub ledger_rows_refreshed: i64,
    /// Branches the gap pass examined / skipped as unchanged (Phase 6).
    pub branches_checked: i64,
    pub branches_skipped: i64,
}

fn env_i64(key: &str, default: i64) -> i64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|v: &i64| *v >= 0)
        .unwrap_or(default)
}

/// How long a ledger row stays on the feed: the full snapshot's ledger window
/// (`LEDGER_WINDOW_HOURS`) plus `SYNC_LEDGER_RETENTION_SLACK_HOURS`
/// (default 14 days). The slack is what lets a till that was off for a while
/// catch up incrementally instead of resyncing.
pub fn retention() -> chrono::Duration {
    chrono::Duration::hours(
        madar_sync::LEDGER_WINDOW_HOURS
            + env_i64("SYNC_LEDGER_RETENTION_SLACK_HOURS", DEFAULT_SLACK_HOURS),
    )
}

fn idle_recheck() -> chrono::Duration {
    chrono::Duration::minutes(env_i64(
        "SYNC_SWEEP_IDLE_RECHECK_MINUTES",
        DEFAULT_IDLE_RECHECK_MINUTES,
    ))
}

/// `SYNC_SWEEP_ENABLED` is falsy: `0`, `false`, `no` or `off`, exactly like
/// every other background switch. Unset (or anything else) keeps it on.
pub fn disabled_by(value: Option<&str>) -> bool {
    value.is_some_and(|v| matches!(v, "0" | "false" | "no" | "off"))
}

pub fn spawn(pool: PgPool) {
    if disabled_by(std::env::var("SYNC_SWEEP_ENABLED").ok().as_deref()) {
        tracing::info!("sync changefeed sweep disabled (SYNC_SWEEP_ENABLED)");
        return;
    }
    crate::db::spawn(async move {
        let mut tick = tokio::time::interval(EVERY);
        loop {
            tick.tick().await;
            match sweep_once(&pool).await {
                Ok(r) if has_work(&r) => tracing::info!(?r, "sync sweep"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "sync sweep failed"),
            }
        }
    });
}

fn has_work(r: &SweepReport) -> bool {
    r.deletes_emitted
        + r.upserts_emitted
        + r.tombstones_purged
        + r.ledger_rows_purged
        + r.ledger_rows_refreshed
        > 0
}

/// One sweep pass. Returns zeroes when another instance holds the lock.
pub async fn sweep_once(pool: &PgPool) -> Result<SweepReport, sqlx::Error> {
    sweep_once_at(pool, Utc::now()).await
}

/// [`sweep_once`] as if it were `now` (tests age the feed this way).
pub async fn sweep_once_at(pool: &PgPool, now: DateTime<Utc>) -> Result<SweepReport, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let locked = sqlx::query_scalar!(r#"SELECT pg_try_advisory_lock($1) AS "locked!""#, SWEEP_LOCK)
        .fetch_one(&mut *conn)
        .await?;
    if !locked {
        return Ok(SweepReport::default());
    }
    // These statements are short probes over indexes whose cost estimates can
    // cross `jit_above_cost`; JIT compilation then costs more than the query
    // (measured: ~0.5 s per pass on the prod copy). Session-level, reset below.
    let _ = sqlx::query!("SET jit = off").execute(&mut *conn).await;
    let result = sweep_locked(&mut conn, now).await;
    let _ = sqlx::query!("RESET jit").execute(&mut *conn).await;
    let _ = sqlx::query_scalar!(r#"SELECT pg_advisory_unlock($1) AS "unlocked!""#, SWEEP_LOCK)
        .fetch_one(&mut *conn)
        .await;
    result
}

async fn raise_watermark(
    conn: &mut sqlx::PgConnection,
    branch: Uuid,
    max_seq: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO sync_feed_watermarks (branch_id, purged_through_seq, updated_at) VALUES ($1, $2, now()) \
         ON CONFLICT (branch_id) DO UPDATE \
            SET purged_through_seq = GREATEST(sync_feed_watermarks.purged_through_seq, EXCLUDED.purged_through_seq), \
                updated_at = now()",
        branch,
        max_seq
    )
    .execute(&mut *conn)
    .await?;
    // A cursor below the new watermark must now resync: every instance drops
    // the branch's cached idle answer (`fastpath`), at commit. No device is
    // nudged (that is the `sync_changes` channel).
    sqlx::query!(
        "SELECT pg_notify($1, $2)",
        crate::fastpath::listener::PURGED,
        branch.to_string()
    )
    .execute(&mut *conn)
    .await?;
    crate::fastpath::on_branch_changed(branch);
    Ok(())
}

async fn sweep_locked(
    conn: &mut sqlx::PgConnection,
    now: DateTime<Utc>,
) -> Result<SweepReport, sqlx::Error> {
    let mut report = SweepReport::default();
    let types: Vec<String> = TIME_BASED.iter().map(|t| t.to_string()).collect();

    // 1. Time-based exits: a feed upsert whose entity no longer projects.
    report.deletes_emitted = sqlx::query_scalar!(
        r#"WITH gone AS (
              SELECT c.branch_id, c.type, c.entity_id FROM sync_changes c
               WHERE c.op = 'upsert' AND c.type = ANY($1)
                 AND NOT CASE c.type
                       WHEN 'kitchen_ticket' THEN EXISTS (SELECT 1 FROM kitchen_tickets x
                            WHERE x.id = c.entity_id AND x.branch_id = c.branch_id AND sync_live_kitchen_ticket(x))
                       WHEN 'delivery' THEN EXISTS (SELECT 1 FROM delivery_orders x
                            WHERE x.id = c.entity_id AND x.branch_id = c.branch_id AND sync_live_delivery(x))
                       WHEN 'booking' THEN EXISTS (SELECT 1 FROM bookings x
                            WHERE x.id = c.entity_id AND x.branch_id = c.branch_id AND sync_live_booking(x))
                       ELSE true END)
           SELECT count(*) AS "n!" FROM (SELECT sync_emit(branch_id, type, entity_id, 'delete') FROM gone) x"#,
        &types
    )
    .fetch_one(&mut *conn)
    .await?;

    // 2. Old tombstones.
    let purged = sqlx::query!(
        r#"WITH dead AS (
              DELETE FROM sync_changes WHERE op = 'delete' AND changed_at < $1::timestamptz - interval '30 days'
              RETURNING branch_id, seq)
           SELECT branch_id AS "branch_id!", max(seq) AS "max_seq!", count(*) AS "n!" FROM dead GROUP BY branch_id"#,
        now
    )
    .fetch_all(&mut *conn)
    .await?;
    for p in purged {
        report.tombstones_purged += p.n;
        raise_watermark(conn, p.branch_id, p.max_seq).await?;
    }

    // 3. Ledger retention, in batches over every branch at once.
    let cutoff = now - retention();
    let ledger: Vec<String> = madar_sync::LEDGER_TYPES.iter().map(|t| t.to_string()).collect();
    report.ledger_rows_refreshed = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM (
              SELECT sync_emit(c.branch_id, c.type, c.entity_id, 'upsert')
                FROM sync_changes c
               WHERE c.type = ANY($1) AND c.op = 'upsert' AND c.changed_at < $2
                 AND CASE c.type
                       WHEN 'till' THEN EXISTS (SELECT 1 FROM tills t WHERE t.id = c.entity_id AND t.updated_at >= $2)
                       WHEN 'cash_movement' THEN EXISTS (SELECT 1 FROM till_cash_movements m WHERE m.id = c.entity_id AND m.created_at >= $2)
                       WHEN 'order' THEN EXISTS (SELECT 1 FROM orders o WHERE o.id = c.entity_id AND o.updated_at >= $2)
                       WHEN 'refund' THEN EXISTS (SELECT 1 FROM order_refunds r WHERE r.id = c.entity_id AND r.created_at >= $2)
                       ELSE false END) x"#,
        &ledger,
        cutoff
    )
    .fetch_one(&mut *conn)
    .await?;
    report.ledger_rows_purged = purge_ledger(conn, &ledger, cutoff).await?;

    // 4. The incremental gap pass.
    let recheck_before = now - idle_recheck();
    // Skip a branch whose head has not passed its watermark and whose last
    // check is recent; decided in SQL (`checked_at` may be -infinity, which
    // has no chrono value).
    let due = sqlx::query!(
        r#"SELECT b.id AS "branch_id!",
                  (COALESCE((SELECT max(c.seq) FROM sync_changes c WHERE c.branch_id = b.id), 0)
                       <= COALESCE(s.checked_through_seq, -1)
                   AND COALESCE(s.checked_at > $1, false)) AS "skip!"
             FROM branches b LEFT JOIN sync_sweep_state s ON s.branch_id = b.id
            WHERE b.deleted_at IS NULL"#,
        recheck_before
    )
    .fetch_all(&mut *conn)
    .await?;
    for b in due {
        if b.skip {
            report.branches_skipped += 1;
            continue;
        }
        report.branches_checked += 1;
        report.upserts_emitted += sqlx::query_scalar!(
            r#"WITH gap AS (
                  SELECT l.branch_id, l.type, l.entity_id FROM sync_live_rows_since($1, $2) l
                   WHERE NOT EXISTS (SELECT 1 FROM sync_changes c
                                      WHERE c.branch_id = l.branch_id AND c.type = l.type
                                        AND c.entity_id = l.entity_id))
               SELECT count(*) AS "n!" FROM (SELECT sync_emit(branch_id, type, entity_id, 'upsert') FROM gap) x"#,
            b.branch_id,
            cutoff
        )
        .fetch_one(&mut *conn)
        .await?;
        // The watermark is the head AFTER this branch's own emits, so the
        // next pass does not re-check a branch only because the sweep wrote.
        sqlx::query!(
            "INSERT INTO sync_sweep_state (branch_id, checked_through_seq, checked_at)
             VALUES ($1, COALESCE((SELECT max(seq) FROM sync_changes WHERE branch_id = $1), 0), $2)
             ON CONFLICT (branch_id) DO UPDATE
                SET checked_through_seq = EXCLUDED.checked_through_seq, checked_at = EXCLUDED.checked_at",
            b.branch_id,
            now
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(report)
}

/// Purge ledger feed rows older than `cutoff`, in batches, raising each
/// branch's watermark past what it lost. A row
/// is KEPT when a full snapshot could still ask for it: its till is open or
/// closed after `cutoff`, or its source row changed after `cutoff`. Every
/// condition sits on the DELETE itself, so a row an emitter refreshes while
/// the purge runs is re-checked on its new version and survives.
async fn purge_ledger(
    conn: &mut sqlx::PgConnection,
    ledger: &[String],
    cutoff: DateTime<Utc>,
) -> Result<i64, sqlx::Error> {
    let mut total = 0;
    loop {
        // The keep test is on the batch's selection (so a batch is never
        // filled with rows that stay) AND on the DELETE itself (so a row an
        // emitter refreshes meanwhile is re-checked on its new version).
        let done = sqlx::query!(
            r#"WITH dead AS (
                  DELETE FROM sync_changes c
                   WHERE c.type = ANY($1) AND c.changed_at < $2
                     AND (c.branch_id, c.type, c.entity_id) IN (
                           SELECT c2.branch_id, c2.type, c2.entity_id FROM sync_changes c2
                            WHERE c2.type = ANY($1) AND c2.changed_at < $2
                              AND NOT CASE c2.type
                       WHEN 'till' THEN EXISTS (SELECT 1 FROM tills t WHERE t.id = c2.entity_id
                            AND (t.status = 'open' OR t.closed_at >= $2 OR t.updated_at >= $2))
                       WHEN 'cash_movement' THEN EXISTS (SELECT 1 FROM till_cash_movements m JOIN tills t ON t.id = m.till_id
                            WHERE m.id = c2.entity_id AND (t.status = 'open' OR t.closed_at >= $2 OR m.created_at >= $2))
                       WHEN 'order' THEN EXISTS (SELECT 1 FROM orders o WHERE o.id = c2.entity_id
                            AND (o.updated_at >= $2 OR EXISTS (SELECT 1 FROM tills t WHERE t.id = o.till_id
                                                                AND (t.status = 'open' OR t.closed_at >= $2))))
                       WHEN 'refund' THEN EXISTS (SELECT 1 FROM order_refunds r WHERE r.id = c2.entity_id
                            AND (r.created_at >= $2 OR EXISTS (SELECT 1 FROM tills t WHERE t.id = r.till_id
                                                                AND (t.status = 'open' OR t.closed_at >= $2))))
                       ELSE false END
                            ORDER BY c2.seq LIMIT $3)
                     AND NOT CASE c.type
                       WHEN 'till' THEN EXISTS (SELECT 1 FROM tills t WHERE t.id = c.entity_id
                            AND (t.status = 'open' OR t.closed_at >= $2 OR t.updated_at >= $2))
                       WHEN 'cash_movement' THEN EXISTS (SELECT 1 FROM till_cash_movements m JOIN tills t ON t.id = m.till_id
                            WHERE m.id = c.entity_id AND (t.status = 'open' OR t.closed_at >= $2 OR m.created_at >= $2))
                       WHEN 'order' THEN EXISTS (SELECT 1 FROM orders o WHERE o.id = c.entity_id
                            AND (o.updated_at >= $2 OR EXISTS (SELECT 1 FROM tills t WHERE t.id = o.till_id
                                                                AND (t.status = 'open' OR t.closed_at >= $2))))
                       WHEN 'refund' THEN EXISTS (SELECT 1 FROM order_refunds r WHERE r.id = c.entity_id
                            AND (r.created_at >= $2 OR EXISTS (SELECT 1 FROM tills t WHERE t.id = r.till_id
                                                                AND (t.status = 'open' OR t.closed_at >= $2))))
                       ELSE false END
                  RETURNING c.branch_id, c.seq)
               SELECT branch_id AS "branch_id!", count(*) AS "n!", max(seq) AS "max_seq!"
                 FROM dead GROUP BY branch_id"#,
            ledger,
            cutoff,
            PURGE_BATCH
        )
        .fetch_all(&mut *conn)
        .await?;
        let mut n = 0;
        for b in &done {
            raise_watermark(conn, b.branch_id, b.max_seq).await?;
            n += b.n;
        }
        total += n;
        if n < PURGE_BATCH {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(total)
}
