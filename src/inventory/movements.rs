//! Append-only stock-movement ledger writer.
//!
//! Every site that changes `branch_inventory.current_stock` should also record
//! an `inventory_movements` row via [`record_movement`] so consumption,
//! adjustments, transfers, waste, purchases and stock counts share one audit
//! trail (read back via `GET /inventory/branches/{id}/movements` and the
//! valuation / consumption / variance reports).

use sqlx::PgExecutor;
use uuid::Uuid;

use crate::errors::AppError;

/// One ledger entry. `quantity` is the SIGNED delta applied to current_stock
/// (consumption negative, replenishment positive). `balance_after` is the
/// resulting stock level; `below_zero` flags a movement that drove it negative.
pub struct MovementParams<'a> {
    pub branch_id: Uuid,
    pub org_ingredient_id: Uuid,
    pub branch_inventory_id: Option<Uuid>,
    /// An `inventory_movement_type` enum value, e.g. "sale", "purchase_in".
    pub movement_type: &'a str,
    pub quantity: f64,
    pub balance_after: Option<f64>,
    /// Piastres per unit at movement time; `None` ⟺ unknown (never 0).
    pub unit_cost: Option<i64>,
    pub reason: Option<&'a str>,
    pub below_zero: bool,
    pub source_type: Option<&'a str>,
    pub source_id: Option<Uuid>,
    pub note: Option<&'a str>,
    pub created_by: Option<Uuid>,
}

/// Insert one movement row and return its id. Pass `&mut *tx` to enrol it in
/// the caller's transaction (so the ledger entry commits atomically with the
/// stock change). Callers that don't need the id can just `?` and ignore it.
pub async fn record_movement<'e, E>(executor: E, p: MovementParams<'_>) -> Result<Uuid, AppError>
where
    E: PgExecutor<'e>,
{
    let id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO inventory_movements
            (branch_id, org_ingredient_id, branch_inventory_id, type, quantity,
             balance_after, unit_cost, reason, below_zero, source_type, source_id, note, created_by)
        VALUES ($1, $2, $3, $4::inventory_movement_type, $5,
                $6, $7, $8, $9, $10, $11, $12, $13)
        RETURNING id
        "#,
    )
    .bind(p.branch_id)
    .bind(p.org_ingredient_id)
    .bind(p.branch_inventory_id)
    .bind(p.movement_type)
    .bind(p.quantity)
    .bind(p.balance_after)
    .bind(p.unit_cost)
    .bind(p.reason)
    .bind(p.below_zero)
    .bind(p.source_type)
    .bind(p.source_id)
    .bind(p.note)
    .bind(p.created_by)
    .fetch_one(executor)
    .await?;
    Ok(id)
}
