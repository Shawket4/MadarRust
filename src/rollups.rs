//! The `sales_daily_*` rollups (DB_ARCHITECTURE_AUDIT_2026-10-16 §6.3, Phase
//! 1.6): one row set per (branch, business day), rebuilt from the ledger by
//! the SQL function `sales_daily_recompute` (DELETE + INSERT in one
//! transaction, so a rerun gives the same rows).
//!
//! NOT READ YET. Reports keep computing live; Phase 2 points closed days at
//! these after a per-org, per-day reconciliation against the live figures.
//!
//! Kept fresh two ways, both best-effort and never in a request's way:
//! * after a till closes, every business day its sales fell on
//!   ([`after_till_close`], after the COMMIT, on the caller's RLS pool);
//! * a sweep (owner pool, every `MADAR_ROLLUP_SWEEP_SECS`, default 6 h, `0`
//!   = off) recomputes D-1 and D-2 of every branch that sold in the last days,
//!   which catches late replays and refunds.

use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

/// Recompute the business days of one till that just closed. Awaited after the
/// close's COMMIT (a few indexed statements per day); a failure is logged and
/// never fails the close — the sweep repairs it.
pub async fn after_till_close(pool: &PgPool, till_id: Uuid) {
    if let Err(e) = recompute_till(pool, till_id).await {
        tracing::warn!(target: "madar.rollups", %till_id, error = %e, "sales_daily recompute after till close failed");
    }
    if let Err(e) = settle_till(pool, till_id).await {
        tracing::warn!(target: "madar.rollups", %till_id, error = %e, "report rollups after till close failed");
    }
}

/// The report rollups of a till's branch, brought up to date at its close
/// (coverage advanced, its dirty days recomputed; the worker takes any rest).
/// Returns how many days were recomputed.
pub async fn settle_till(pool: &PgPool, till_id: Uuid) -> Result<i32, sqlx::Error> {
    let n = sqlx::query_scalar!(
        r#"SELECT sales_daily_settle_till($1) AS "n!""#,
        till_id
    )
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// Recompute every business day of `till_id`; returns how many days.
pub async fn recompute_till(pool: &PgPool, till_id: Uuid) -> Result<i32, sqlx::Error> {
    let n = sqlx::query_scalar!(
        r#"SELECT sales_daily_recompute_till($1) AS "n!""#,
        till_id
    )
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// D-1 and D-2 (each branch's own local date) of every branch with a sale or a
/// refund in the last four days. Returns how many (branch, day) pairs.
pub async fn sweep(pool: &PgPool) -> Result<i64, sqlx::Error> {
    let n = sqlx::query_scalar!(
        r#"WITH b AS (
               SELECT DISTINCT branch_id FROM orders WHERE created_at > now() - interval '4 days'
               UNION
               SELECT DISTINCT branch_id FROM order_refunds WHERE issued_at > now() - interval '4 days'
           ), d AS (
               SELECT b.branch_id,
                      ((now() AT TIME ZONE effective_timezone(b.branch_id))::date - k) AS day
                 FROM b CROSS JOIN generate_series(1, 2) k
           )
           SELECT count(sales_daily_recompute(d.branch_id, d.day)) AS "n!" FROM d"#
    )
    .fetch_one(pool)
    .await?;
    Ok(n)
}

// ── the report rollups (`sales_daily_sold*`, Phase 2.6, read by reports::sales) ──

/// What one report-rollup pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReportTick {
    /// Closed days newly brought into coverage (each enters dirty), plus
    /// D-1 / D-2 marked for their catch-up recompute.
    pub days_covered: i64,
    /// Dirty days recomputed.
    pub days_recomputed: i64,
}

/// Extend every live branch's coverage over the days that closed more than
/// `lag` ago (never less than 30 minutes: the order write path relies on it,
/// see migration 20261018310000), mark D-1 and D-2 for their catch-up
/// recompute, then recompute dirty days (at most `max_days`, in batches of 50,
/// one short transaction per batch). Idempotent: a second run right after
/// does nothing.
pub async fn report_tick(
    pool: &PgPool,
    lag: Duration,
    max_days: i64,
) -> Result<ReportTick, sqlx::Error> {
    let lag_secs = lag.as_secs_f64();
    let days_covered = sqlx::query_scalar!(
        r#"SELECT COALESCE(SUM(sales_daily_advance(b.id, make_interval(secs => $1))), 0)::bigint AS "n!"
             FROM branches b WHERE b.deleted_at IS NULL"#,
        lag_secs
    )
    .fetch_one(pool)
    .await?;
    let mut days_recomputed = 0i64;
    while days_recomputed < max_days {
        let n = sqlx::query_scalar!(r#"SELECT sales_daily_clean(50) AS "n!""#)
            .fetch_one(pool)
            .await?;
        if n == 0 {
            break;
        }
        days_recomputed += i64::from(n);
    }
    Ok(ReportTick {
        days_covered,
        days_recomputed,
    })
}

/// The sweep loop. Spawned once in `main` on the owner pool.
pub fn spawn(pool: PgPool) {
    spawn_report_worker(pool.clone());
    let secs: u64 = std::env::var("MADAR_ROLLUP_SWEEP_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6 * 3600);
    if secs == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(secs));
        // Not at boot: a deploy must not add work to its first minutes.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            match sweep(&pool).await {
                Ok(n) => tracing::info!(target: "madar.rollups", days = n, "sales_daily sweep"),
                Err(e) => tracing::warn!(target: "madar.rollups", error = %e, "sales_daily sweep failed"),
            }
        }
    });
}

/// The report-rollup worker: every `MADAR_ROLLUP_REPORT_SECS` (default 300,
/// `0` = off) one [`report_tick`] with a one-hour lag. Not at boot. While it
/// is off (or behind), reports stay exact: an uncovered or dirty day is read
/// live.
fn spawn_report_worker(pool: PgPool) {
    let secs: u64 = std::env::var("MADAR_ROLLUP_REPORT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    if secs == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(secs));
        ticker.tick().await;
        loop {
            ticker.tick().await;
            match report_tick(&pool, Duration::from_secs(3600), 5000).await {
                Ok(t) if t == ReportTick::default() => {}
                Ok(t) => tracing::info!(target: "madar.rollups", covered = t.days_covered, recomputed = t.days_recomputed, "report rollups"),
                Err(e) => tracing::warn!(target: "madar.rollups", error = %e, "report rollups failed"),
            }
        }
    });
}
