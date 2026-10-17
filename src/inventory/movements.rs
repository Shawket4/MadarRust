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

/// Post many movements in ONE statement, in the order given, and report each
/// one back in that order. Same rows, same triggers, same balances as calling
/// [`record_movement`] once per entry: the `BEFORE INSERT` triggers still run
/// per row, in this order (a later row's trigger sees the earlier rows'
/// `branch_stock` upserts, so two entries for one ingredient chain exactly as
/// two statements did, and the row locks are taken in the same order). One
/// round trip instead of one per entry: a sale posts one movement per recipe
/// line, so this was the largest statement count on the order path.
///
/// `org_id`, when the caller knows it (every sale does), is written on the
/// rows instead of being looked up per row by `fill_org_id`; the composite FK
/// `(org_id, branch_id) → branches` holds it to the branch's org either way.
pub async fn record_movements<'e, E>(
    executor: E,
    org_id: Option<Uuid>,
    entries: &[MovementParams<'_>],
) -> Result<Vec<PostedMovement>, AppError>
where
    E: PgExecutor<'e>,
{
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    // The ids are minted here (the column default is the same v4 uuid) so the
    // rows come back matched by id, not by RETURNING order.
    let ids: Vec<Uuid> = entries.iter().map(|_| Uuid::new_v4()).collect();
    let branch: Vec<Uuid> = entries.iter().map(|p| p.branch_id).collect();
    let ingredient: Vec<Uuid> = entries.iter().map(|p| p.org_ingredient_id).collect();
    let kind: Vec<String> = entries.iter().map(|p| p.movement_type.to_string()).collect();
    let quantity: Vec<f64> = entries.iter().map(|p| p.quantity).collect();
    let unit_cost: Vec<Option<i64>> = entries
        .iter()
        .map(|p| p.unit_cost.map(crate::costing::service::round_piastres))
        .collect();
    let reason: Vec<Option<String>> = entries.iter().map(|p| p.reason.map(str::to_string)).collect();
    let source_type: Vec<Option<String>> =
        entries.iter().map(|p| p.source_type.map(str::to_string)).collect();
    let source_id: Vec<Option<Uuid>> = entries.iter().map(|p| p.source_id).collect();
    let note: Vec<Option<String>> = entries.iter().map(|p| p.note.map(str::to_string)).collect();
    let created_by: Vec<Option<Uuid>> = entries.iter().map(|p| p.created_by).collect();
    let exact: Vec<Option<Decimal>> =
        entries.iter().map(|p| p.unit_cost.map(|c| c.round_dp(COST_DP))).collect();
    let rows = sqlx::query!(
        r#"
        INSERT INTO inventory_movements
            (id, org_id, branch_id, org_ingredient_id, type, quantity,
             unit_cost, reason, source_type, source_id, note, created_by,
             unit_cost_exact)
        SELECT u.id, $1, u.branch_id, u.ingredient, u.kind::inventory_movement_type, u.quantity,
               u.unit_cost, u.reason, u.source_type, u.source_id, u.note, u.created_by,
               u.exact
          FROM UNNEST($2::uuid[], $3::uuid[], $4::uuid[], $5::text[], $6::float8[],
                      $7::int8[], $8::text[], $9::text[], $10::uuid[], $11::text[],
                      $12::uuid[], $13::numeric[])
               WITH ORDINALITY AS u(id, branch_id, ingredient, kind, quantity,
                                    unit_cost, reason, source_type, source_id, note,
                                    created_by, exact, ord)
         ORDER BY u.ord
        RETURNING id, branch_stock_id AS "branch_stock_id!", balance_after::float8 AS "balance_after!",
                  below_zero
        "#,
        org_id as Option<Uuid>,
        &ids,
        &branch,
        &ingredient,
        &kind,
        &quantity,
        &unit_cost as &[Option<i64>],
        &reason as &[Option<String>],
        &source_type as &[Option<String>],
        &source_id as &[Option<Uuid>],
        &note as &[Option<String>],
        &created_by as &[Option<Uuid>],
        &exact as &[Option<Decimal>],
    )
    .fetch_all(executor)
    .await?;
    let mut by_id: std::collections::HashMap<Uuid, PostedMovement> = rows
        .into_iter()
        .map(|r| {
            (
                r.id,
                PostedMovement {
                    id: r.id,
                    branch_stock_id: r.branch_stock_id,
                    balance_after: r.balance_after,
                    below_zero: r.below_zero,
                },
            )
        })
        .collect();
    ids.iter()
        .map(|id| {
            by_id.remove(id).ok_or_else(|| {
                tracing::error!(movement = %id, "inventory movement was not posted");
                AppError::Internal
            })
        })
        .collect()
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
