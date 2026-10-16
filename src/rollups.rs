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

/// The sweep loop. Spawned once in `main` on the owner pool.
pub fn spawn(pool: PgPool) {
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
