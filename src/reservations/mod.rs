//! The floor plan: sections, table geometry, and live table status.
//!
//! Geometry is dashboard-authored (gated by `floor_plan`); the POS renders it.
//! Table STATUS is not settable here or anywhere else — it is derived from the
//! order sitting on the table, so the room cannot claim a table is free while a
//! ticket is open on it. See `crate::tickets` for the lifecycle that drives it.
//!
//! The booking flow (reservations + waitlist + public self-booking) and its
//! nudge scheduler used to live here. Both are removed: the flow was never
//! used in production -- zero bookings, ever -- and its replacement is being
//! built on the floor/ticket layer instead of bolted beside it.

pub mod floor;
pub mod routes;

#[cfg(test)]
mod tests;

/// Resolve a branch's org id (and confirm it's live).
pub(crate) async fn resolve_branch_org(
    pool: &sqlx::PgPool,
    branch_id: uuid::Uuid,
) -> Result<uuid::Uuid, crate::errors::AppError> {
    sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
        .bind(branch_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| crate::errors::AppError::NotFound("Branch not found".into()))
}
