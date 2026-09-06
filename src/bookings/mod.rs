//! Table bookings, rebuilt on the floor/ticket layer.
//!
//! A booking is a FUTURE CLAIM on capacity. It never writes `branch_tables.status`;
//! the floor stays the live truth. What a booking does own is a set of table
//! claims (`booking_tables`) for its time window, guarded by a DB exclusion
//! constraint so two active bookings can never hold one table at once.
//!
//! Lifecycle: `confirmed` → `seated` (the POS parks the party; the fired ticket
//! carries `booking_id`) → `completed` (the ticket settles, or the sweep closes
//! it after its window). Side exits: `no_show` (host tap or the sweep after the
//! grace) and `cancelled` (guest link, host, or system).
//!
//! "Held" is DERIVED, never scheduled: `GET /floor/tables` returns the table's
//! `next_booking` with `held_from = starts_at - hold_minutes`; the dashboard and
//! the POS compare it with their clock. No cron is needed for the floor to be
//! right — the only periodic work (`jobs`) is reminders, the "party arriving"
//! nudge, and the no-show / completion roll-overs.
//!
//! Capacity is SEATS-BASED with automatic best-fit table assignment at
//! confirmation (one table first, then two tables in the same section), so the
//! floor can show the held table without a host touching every booking. Hosts
//! can reassign; a booking that fits nowhere can still be forced by a host and
//! shows up as "needs a table".

pub mod availability;
pub mod handlers;
pub mod jobs;
pub mod model;
pub mod public;
pub mod routes;
pub mod settings;
pub mod whatsapp;

#[cfg(test)]
mod tests;

use chrono_tz::Tz;
use sqlx::PgExecutor;
use uuid::Uuid;

use crate::errors::AppError;
use crate::realtime::event::{BranchEvent, Topic};
use crate::realtime::hub::BranchEventHub;

pub use model::{BookingView, booking_view};
pub use settings::{BookingSettings, load_settings};

/// The branch's effective IANA timezone (branch override, else the org's).
pub(crate) async fn branch_tz<'e, E>(exec: E, branch_id: Uuid) -> Result<Tz, AppError>
where
    E: PgExecutor<'e>,
{
    let name: Option<String> = sqlx::query_scalar(
        "SELECT COALESCE(b.timezone, o.timezone)::text FROM branches b \
         JOIN organizations o ON o.id = b.org_id WHERE b.id = $1 AND b.deleted_at IS NULL",
    )
    .bind(branch_id)
    .fetch_optional(exec)
    .await?;
    let name = name.ok_or_else(|| AppError::NotFound("Branch not found".into()))?;
    Ok(name.parse().unwrap_or(chrono_tz::Africa::Cairo))
}

/// Publish a booking event on the `bookings` topic (post-commit). The payload is
/// the full view so a POS or dashboard can update its list without a re-pull,
/// though both re-pull anyway (the event is the nudge, the list is the truth).
pub(crate) async fn publish_booking(
    pool: &sqlx::PgPool,
    hub: &BranchEventHub,
    event_type: &str,
    booking_id: Uuid,
) {
    if let Ok(Some(view)) = booking_view(pool, booking_id).await {
        hub.publish(
            view.branch_id,
            BranchEvent::new(Topic::Bookings, event_type, &view),
        );
    }
}
