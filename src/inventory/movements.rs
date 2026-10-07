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

use rust_decimal::Decimal;
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
    /// Piastres per unit at movement time, EXACT; `None` ⟺ unknown (never 0).
    /// Written whole as `unit_cost_exact` and rounded into the legacy bigint
    /// `unit_cost` (a gram of milk at 4.568 piastres is not 5).
    pub unit_cost: Option<Decimal>,
    pub reason: Option<&'a str>,
    pub source_type: Option<&'a str>,
    pub source_id: Option<Uuid>,
    pub note: Option<&'a str>,
    pub created_by: Option<Uuid>,
}

/// Decimal places of a PIASTRE kept on a cost per unit (`cost_per_unit` is
/// numeric(20,6)): 1e-8 EGP, far below anything an invoice can express.
pub const COST_DP: u32 = 6;

/// An exact cost per unit from a float the caller holds (a deduction's
/// `cost_per_unit`), kept at [`COST_DP`] instead of rounded to whole piastres.
pub fn exact_cost(piastres: f64) -> Option<Decimal> {
    piastres
        .is_finite()
        .then(|| Decimal::from_f64_retain(piastres))
        .flatten()
        .map(|d| d.round_dp(COST_DP))
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
                 unit_cost, reason, source_type, source_id, note, created_by,
                 unit_cost_exact)
            VALUES ($1, $2, $3::inventory_movement_type, $4,
                    $5, $6, $7, $8, $9, $10, $11)
            RETURNING id, branch_stock_id, balance_after::float8, below_zero
            "#,
    )
    .bind(p.branch_id)
    .bind(p.org_ingredient_id)
    .bind(p.movement_type)
    .bind(p.quantity)
    .bind(p.unit_cost.map(crate::costing::service::round_piastres))
    .bind(p.reason)
    .bind(p.source_type)
    .bind(p.source_id)
    .bind(p.note)
    .bind(p.created_by)
    .bind(p.unit_cost.map(|c| c.round_dp(COST_DP)))
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
