//! Reservations & waitlist + the graphical floor plan.
//!
//! Three concerns, one module:
//! - [`floor`]  — sections + table geometry + per-branch reservation settings.
//!   Geometry is dashboard-authored (gated by the `floor_plan` permission); the
//!   POS renders it and only mutates the operational `status`.
//! - [`bookings`] — the unified booking entity. A reservation has a
//!   `reserved_for` time; a waitlist entry has none. One status machine, one set
//!   of host operations (gated by `reservations`). Seating a party auto-opens an
//!   `open_ticket` on the assigned table(s) so it joins the existing dine-in flow.
//! - [`public`] — the unauthenticated self-booking flow, reusing delivery's
//!   phone-OTP + device-trust token + browser geolocation.
//!
//! The [`nudge`] scheduler (spawned once from `main`) drives the flat
//! reservation departure nudge, the no-show warn, table holds, and the
//! OSRM-driven waitlist "head out" nudge. All sends are idempotent via
//! `booking_nudges` and ride the shared WhatsApp gateway.

pub mod bookings;
pub mod floor;
pub mod nudge;
pub mod public;
pub mod routes;

#[cfg(test)]
mod tests;

/// Resolve a branch's org id (and confirm it's live). Shared by floor + booking
/// creation, mirroring the pattern in `tills::handlers::create_till`.
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
