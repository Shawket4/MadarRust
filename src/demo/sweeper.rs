//! Garbage collection for expired demo orgs.
//!
//! Org/branch foreign keys into ledgers RESTRICT (DB Phase 5.7), so an expired demo
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
    // Ledgers that used to go with the org/branch by cascade. Since DB Phase
    // 5.7 those FKs RESTRICT (a real shop's ledger never disappears as a side
    // effect), so a DEMO org's are removed explicitly, children first.
    "DELETE FROM loyalty_transactions WHERE org_id = $1",
    "DELETE FROM order_refund_lines WHERE org_id = $1",
    "DELETE FROM order_refunds WHERE org_id = $1",
    "DELETE FROM kitchen_tickets WHERE org_id = $1",
    "DELETE FROM delivery_orders WHERE org_id = $1",
    "DELETE FROM inventory_movements WHERE org_id = $1",
    // Branch-scoped transactional data (orders cascade to order_items).
    "DELETE FROM orders WHERE branch_id IN (SELECT id FROM branches WHERE org_id = $1)",
    "DELETE FROM table_occupancies WHERE org_id = $1",
    "DELETE FROM open_tickets WHERE org_id = $1",
    "DELETE FROM tills WHERE branch_id IN (SELECT id FROM branches WHERE org_id = $1)",
    // Unified menu model (menu unification): recipe_lines and
    // menu_price_overrides have NO FK to their polymorphic owners/targets, so
    // they are removed explicitly. recipe_lines MUST precede org_ingredients
    // (ingredient FK is ON DELETE RESTRICT); one delete by ingredient covers
    // both owner kinds (item sizes + modifier options).
    "DELETE FROM recipe_lines WHERE ingredient_id IN (SELECT id FROM org_ingredients WHERE org_id = $1)",
    "DELETE FROM menu_price_overrides WHERE \
         target_id IN (SELECT s.id FROM menu_item_sizes s JOIN menu_items m ON m.id = s.menu_item_id WHERE m.org_id = $1) \
      OR target_id IN (SELECT o.id FROM modifier_options o JOIN modifier_groups g ON g.id = o.group_id WHERE g.org_id = $1)",
    // Menu graph (menu_items cascade to menu_item_sizes + attachment rows;
    // modifier_groups/options + catalog_revision cascade with the org row).
    "DELETE FROM menu_items WHERE org_id = $1",
    "DELETE FROM categories WHERE org_id = $1",
    "DELETE FROM org_ingredients WHERE org_id = $1",
    // People + branches.
    "DELETE FROM user_branch_assignments WHERE branch_id IN (SELECT id FROM branches WHERE org_id = $1)",
    // The org's access model, explicitly and while the org row still exists:
    // its triggers bump the org's authz epoch and log a grant event, which
    // fails once the org row is gone (FK), so leaving these to the org's
    // cascade made every purge of a demo org with users fail.
    "DELETE FROM role_assignment_branches WHERE org_id = $1",
    "DELETE FROM role_assignments WHERE org_id = $1",
    "DELETE FROM user_overrides WHERE org_id = $1",
    "DELETE FROM org_role_grants WHERE org_id = $1",
    "DELETE FROM org_capability_policy WHERE org_id = $1",
    "DELETE FROM org_roles WHERE org_id = $1",
    "DELETE FROM branches WHERE org_id = $1",
    "DELETE FROM users WHERE org_id = $1",
    // Finally the org (cascades org_payment_methods, modifier_groups →
    // modifier_options, catalog_revision). Guarded so a non-demo org can never
    // be removed here even if mis-called.
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
    crate::db::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(interval_secs.max(30)));
        loop {
            tick.tick().await;
            // Guarded at the job boundary — see `observability::report::guarded_tick`.
            crate::observability::report::guarded_tick("demo_gc", || async {
                let purged = gc_expired(&pool).await?;
                if purged > 0 {
                    tracing::info!("demo gc: purged {purged} expired org(s)");
                }
                Ok::<(), crate::errors::AppError>(())
            })
            .await;
        }
    });
}
