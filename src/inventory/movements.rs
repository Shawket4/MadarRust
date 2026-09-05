//! The stock ledger — the ONLY way `branch_stock.on_hand` changes.
//!
//! `inventory_movements` is append-only. A `BEFORE INSERT` trigger
//! (`inventory_movements_apply`, migration 20260906000000) upserts the
//! `branch_stock` balance row — creating it on an ingredient's first activity
//! at a branch — and stamps the resulting `balance_after` / `below_zero` on the
//! movement. A guard trigger rejects every other write to `on_hand`, so a
//! handler cannot drift the balance away from the ledger even by mistake.
//!
//! Callers therefore never touch `branch_stock` for quantities: they post a
//! movement and read the balance back from [`PostedMovement`]. Stock may go
//! negative (a sale on an ingredient that was never counted, or oversold); the
//! movement is flagged `below_zero` and the caller decides whether to warn.

use sqlx::PgExecutor;
use uuid::Uuid;

use crate::errors::AppError;

/// One ledger entry to post. `quantity` is the SIGNED delta (consumption
/// negative, replenishment positive) in the ingredient's base stock unit.
pub struct MovementParams<'a> {
    pub branch_id: Uuid,
    pub org_ingredient_id: Uuid,
    /// An `inventory_movement_type` enum value, e.g. "sale", "purchase_in".
    pub movement_type: &'a str,
    pub quantity: f64,
    /// Piastres per unit at movement time; `None` ⟺ unknown (never 0).
    pub unit_cost: Option<i64>,
    pub reason: Option<&'a str>,
    pub source_type: Option<&'a str>,
    pub source_id: Option<Uuid>,
    pub note: Option<&'a str>,
    pub created_by: Option<Uuid>,
}

/// What the ledger reports back once the trigger has applied the movement.
#[derive(Debug, Clone, Copy)]
pub struct PostedMovement {
    pub id: Uuid,
    pub branch_stock_id: Uuid,
    /// Balance after this movement, in the base stock unit.
    pub balance_after: f64,
    pub below_zero: bool,
}

/// Post one movement. Pass `&mut *tx` to enrol it in the caller's transaction
/// so the ledger entry and the balance change commit atomically.
pub async fn record_movement<'e, E>(
    executor: E,
    p: MovementParams<'_>,
) -> Result<PostedMovement, AppError>
where
    E: PgExecutor<'e>,
{
    let (id, branch_stock_id, balance_after, below_zero): (Uuid, Uuid, f64, bool) = sqlx::query_as(
        r#"
            INSERT INTO inventory_movements
                (branch_id, org_ingredient_id, type, quantity,
                 unit_cost, reason, source_type, source_id, note, created_by)
            VALUES ($1, $2, $3::inventory_movement_type, $4,
                    $5, $6, $7, $8, $9, $10)
            RETURNING id, branch_stock_id, balance_after::float8, below_zero
            "#,
    )
    .bind(p.branch_id)
    .bind(p.org_ingredient_id)
    .bind(p.movement_type)
    .bind(p.quantity)
    .bind(p.unit_cost)
    .bind(p.reason)
    .bind(p.source_type)
    .bind(p.source_id)
    .bind(p.note)
    .bind(p.created_by)
    .fetch_one(executor)
    .await?;
    Ok(PostedMovement {
        id,
        branch_stock_id,
        balance_after,
        below_zero,
    })
}

/// Current on-hand for one ingredient at a branch, locked `FOR UPDATE` so the
/// caller can validate ("only 4 on hand") and post the movement without a
/// concurrent movement slipping in between. `None` means the ingredient has no
/// activity at this branch yet — treat as zero on hand.
pub async fn lock_on_hand<'e, E>(
    executor: E,
    branch_id: Uuid,
    org_ingredient_id: Uuid,
) -> Result<Option<f64>, AppError>
where
    E: PgExecutor<'e>,
{
    let row: Option<f64> = sqlx::query_scalar(
        "SELECT on_hand::float8 FROM branch_stock \
         WHERE branch_id = $1 AND org_ingredient_id = $2 FOR UPDATE",
    )
    .bind(branch_id)
    .bind(org_ingredient_id)
    .fetch_optional(executor)
    .await?;
    Ok(row)
}
