//! Garbage collection for expired demo orgs.
//!
//! Org/branch foreign keys are NOT `ON DELETE CASCADE`, so an expired demo
//! org's child rows are removed explicitly in dependency order. Each org is
//! purged in its own transaction and failures are isolated (logged, skipped)
//! so one bad org can't wedge the whole sweep — it just lingers and is retried
//! next tick.
//!
//! NOTE: the statement list below covers the tables the demo seed creates plus
//! the entities a dashboard visitor can create (branches, payments, the menu
//! graph, ingredients, team, orders/shifts). If the playground is later opened
//! to features that write OTHER org/branch-scoped tables (deliveries,
//! reservations, stocktakes, purchasing, …), add their deletes here — or give
//! those FKs `ON DELETE CASCADE` in the demo database.

use std::time::Duration;

use sqlx::PgPool;
use uuid::Uuid;

/// Ordered child-row deletes for a single org (`$1` = org id). Parents that
/// cascade to their own children (orders→order_items, menu_items→item_sizes/
/// recipes) are relied upon; everything org/branch-scoped is explicit.
const PURGE_STMTS: &[&str] = &[
    // Branch-scoped transactional data (orders cascade to order_items).
    "DELETE FROM orders WHERE branch_id IN (SELECT id FROM branches WHERE org_id = $1)",
    "DELETE FROM shifts WHERE branch_id IN (SELECT id FROM branches WHERE org_id = $1)",
    // Menu graph (menu_items cascade to item_sizes + menu_item_recipes). Must
    // precede org_ingredients because recipe→ingredient FK is ON DELETE RESTRICT.
    "DELETE FROM menu_items WHERE org_id = $1",
    "DELETE FROM categories WHERE org_id = $1",
    "DELETE FROM addon_items WHERE org_id = $1",
    "DELETE FROM org_ingredients WHERE org_id = $1",
    // People + branches.
    "DELETE FROM user_branch_assignments WHERE branch_id IN (SELECT id FROM branches WHERE org_id = $1)",
    "DELETE FROM branches WHERE org_id = $1",
    "DELETE FROM users WHERE org_id = $1",
    // Finally the org (cascades org_payment_methods, bundles). Guarded so a
    // non-demo org can never be removed here even if mis-called.
    "DELETE FROM organizations WHERE id = $1 AND is_demo",
];

/// Delete one org and all its child rows in a single transaction.
pub async fn purge_org(pool: &PgPool, org_id: Uuid) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    for stmt in PURGE_STMTS {
        sqlx::query(stmt).bind(org_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Find and purge every demo org past its TTL. Returns how many were removed.
/// Per-org failures are logged and skipped (the org is retried next sweep).
pub async fn gc_expired(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let expired: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM organizations \
         WHERE is_demo AND demo_expires_at IS NOT NULL AND demo_expires_at < now()",
    )
    .fetch_all(pool)
    .await?;

    let mut purged = 0u64;
    for id in expired {
        match purge_org(pool, id).await {
            Ok(()) => purged += 1,
            Err(e) => tracing::warn!("demo gc: failed to purge org {id}: {e}"),
        }
    }
    Ok(purged)
}

/// Spawn the background sweeper (once, at startup, when DEMO_MODE is on).
pub fn spawn(pool: PgPool, interval_secs: u64) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(interval_secs.max(30)));
        loop {
            tick.tick().await;
            match gc_expired(&pool).await {
                Ok(n) if n > 0 => tracing::info!("demo gc: purged {n} expired org(s)"),
                Ok(_) => {}
                Err(e) => tracing::warn!("demo gc sweep failed: {e}"),
            }
        }
    });
}
