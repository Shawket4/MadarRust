//! The nightly re-point (CUSTOMERS_UNIFICATION_DESIGN.md §2.5).
//!
//! A merge moves every reference it can see, in its own transaction. What it
//! cannot see arrives later: a till that was offline through the merge flushes
//! a bill naming the duplicate, an import lands with old ids. Those rows are
//! still CORRECT — `customers_resolve` walks the chain on every read — but each
//! one keeps the chain in use, and a chain that is never retired only grows.
//!
//! So, once a merge is 30 days old (long past any till's offline window),
//! whatever still names the duplicate is moved to the survivor. Bounded per
//! tick, idempotent, and safe to race with a live write: every statement is
//! "rows that still name the duplicate", so running it twice moves nothing the
//! second time, and a row that appears mid-tick is simply next tick's work.

use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;

const DEFAULT_TICK_SECS: u64 = 24 * 60 * 60;
/// Merged customers handled per tick.
const CUSTOMERS_PER_TICK: i64 = 200;
/// Rows moved per table per customer per tick.
const ROWS_PER_TABLE: i64 = 500;
/// A merge younger than this is left to the chain: a till may still be offline.
pub const SETTLE_DAYS: i32 = 30;

/// Every table is keyed by a customer id that is unique across orgs, so the id alone scopes the move.
const TABLES: &[&str] = &["orders", "delivery_orders", "bookings", "open_tickets"];

/// Spawn the sweep. No-op when `CUSTOMERS_REPOINT_SWEEP_ENABLED` is falsy.
pub fn spawn(pool: PgPool) {
    let disabled = std::env::var("CUSTOMERS_REPOINT_SWEEP_ENABLED")
        .map(|v| matches!(v.as_str(), "0" | "false" | "no" | "off"))
        .unwrap_or(false);
    if disabled {
        tracing::info!("Customer re-point sweep disabled (CUSTOMERS_REPOINT_SWEEP_ENABLED)");
        return;
    }
    let secs = std::env::var("CUSTOMERS_REPOINT_SWEEP_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_TICK_SECS)
        .max(60);
    tracing::info!("Customer re-point sweep started ({secs}s tick)");
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(secs));
        loop {
            ticker.tick().await;
            crate::observability::report::guarded_tick("customers_repoint", || async {
                run_tick(&pool).await.map(|_| ())
            })
            .await;
        }
    });
}

/// What one tick moved.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Repointed {
    pub customers: usize,
    pub rows: u64,
}

/// One bounded pass. Public so the suite (and an operator) can run it.
pub async fn run_tick(pool: &PgPool) -> Result<Repointed, AppError> {
    // Settled merges that something still names. Oldest first, so a backlog
    // larger than one tick drains in order instead of starving its tail.
    let stale: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT c.id, c.org_id FROM customers c
          WHERE c.merged_into IS NOT NULL
            AND c.merged_at < now() - make_interval(days => $1)
            AND (EXISTS (SELECT 1 FROM orders x WHERE x.customer_id = c.id)
              OR EXISTS (SELECT 1 FROM delivery_orders x WHERE x.customer_id = c.id)
              OR EXISTS (SELECT 1 FROM bookings x WHERE x.customer_id = c.id)
              OR EXISTS (SELECT 1 FROM open_tickets x WHERE x.customer_id = c.id)
              OR EXISTS (SELECT 1 FROM customer_addresses x WHERE x.customer_id = c.id))
          ORDER BY c.merged_at, c.id LIMIT $2",
    )
    .bind(SETTLE_DAYS)
    .bind(CUSTOMERS_PER_TICK)
    .fetch_all(pool)
    .await?;

    let mut out = Repointed::default();
    for (from, org) in stale {
        let mut tx = pool.begin().await?;
        // The survivor at the END of the chain, so one move is enough however
        // many merges deep the duplicate is. A broken chain moves nothing.
        let into: Option<Uuid> = sqlx::query_scalar("SELECT customers_resolve($1, $2)")
            .bind(org)
            .bind(from)
            .fetch_one(&mut *tx)
            .await?;
        let Some(into) = into.filter(|i| *i != from) else {
            continue;
        };
        let mut moved = 0;
        for table in TABLES {
            moved += sqlx::query(&format!(
                "UPDATE {table} SET customer_id = $2
                  WHERE id IN (SELECT id FROM {table} WHERE customer_id = $1 LIMIT $3)"
            ))
            .bind(from)
            .bind(into)
            .bind(ROWS_PER_TABLE)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        let addresses: i64 =
            sqlx::query_scalar("SELECT count(*) FROM customer_addresses WHERE customer_id = $1")
                .bind(from)
                .fetch_one(&mut *tx)
                .await?;
        if addresses > 0 {
            // The same fold a merge uses: a duplicate address is absorbed.
            super::handlers::repoint_addresses(&mut tx, org, from, into).await?;
            moved += addresses as u64;
        }
        tx.commit().await?;
        if moved > 0 {
            out.customers += 1;
            out.rows += moved;
        }
    }
    if out.rows > 0 {
        tracing::info!(
            customers = out.customers,
            rows = out.rows,
            "customers re-point: moved stale references to their survivors"
        );
    }
    Ok(out)
}
