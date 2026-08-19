//! Held orders (teller parked carts) + table occupancy + the transfer waitlist.
//!
//! The POS parks a cart server-side so it can OWN a floor table and be visible
//! (and resumable) on every till in the branch. The cart payload is CLIENT-
//! authored and opaque — the server brokers identity, occupancy, claims, and
//! sync, and never prices a line. Three cooperating pieces:
//!
//! - **Held orders** — client-minted ids, `revision` conflict fence, soft
//!   terminal states (`completed`/`discarded` are tombstones for the `since`
//!   pull), and a resume **claim** (`claimed_by_device`) so two tills can't
//!   edit one cart. Parking is offline-first: a queued park must NEVER be lost
//!   to a table race, so a conflicting table assignment is DROPPED (flagged in
//!   the response) rather than failing the op.
//! - **Occupancy arbitration** — at most one live occupant per table across
//!   held orders AND open tickets. Every mutation locks the `branch_tables`
//!   row (the per-table mutex), checks both entities, and choreographs the
//!   status walk (`seated` on claim, `dirty` on free) exactly like the
//!   open-ticket move path.
//! - **Transfer waitlist** — "wants to move inside": a queued wish by a table's
//!   occupant (or a no-table order) for a section or a specific table,
//!   fulfilled through the same arbitration.
//!
//! Everything mutating is split live-route / `*_inner` so `/sync/replay` can
//! flush a till's offline backlog through the same cores (see `src/sync`).

pub mod handlers;
pub mod routes;

#[cfg(test)]
mod tests;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgExecutor, Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::errors::AppError;
use crate::realtime::event::{BranchEvent, Topic};
use crate::realtime::hub::BranchEventHub;

pub(crate) use crate::delivery::require_branch_access;
pub(crate) use crate::orgs::handlers::extract_claims;

// ── Read models ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HeldOrderView {
    pub id: Uuid,
    pub branch_id: Uuid,
    pub table_id: Option<Uuid>,
    /// Resolved display label of the assigned table (for lists/strips).
    pub table_label: Option<String>,
    pub name: String,
    /// The opaque client cart payload, returned verbatim.
    pub cart: serde_json::Value,
    /// `held` | `resumed` | `completed` | `discarded`.
    pub status: String,
    pub created_by: Option<Uuid>,
    pub device_id: Option<String>,
    /// Set while `resumed` — the device editing the cart.
    pub claimed_by_device: Option<String>,
    pub order_id: Option<Uuid>,
    pub revision: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Park/upsert result: the stored order plus whether a requested table
/// assignment was DROPPED because the table was taken (offline-first parks
/// keep the cart and lose the race, never the other way around).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HeldOrderParkResponse {
    pub held_order: HeldOrderView,
    pub table_conflict: bool,
}

/// The `GET /held-orders` sync payload. With `since`, tombstones are included
/// so devices retire local copies; `server_time` is the client's next cursor.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HeldOrdersSyncResponse {
    pub server_time: DateTime<Utc>,
    pub held_orders: Vec<HeldOrderView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TransferView {
    pub id: Uuid,
    pub branch_id: Uuid,
    /// `held_order` | `open_ticket`.
    pub occupant_kind: String,
    pub occupant_id: Uuid,
    /// Display label for the queue: the held order's name / the ticket's ref.
    pub occupant_label: Option<String>,
    pub from_table_id: Option<Uuid>,
    pub target_section_id: Option<Uuid>,
    pub target_table_id: Option<Uuid>,
    pub note: Option<String>,
    /// `waiting` | `fulfilled` | `cancelled`.
    pub status: String,
    pub requested_by: Option<Uuid>,
    pub fulfilled_table_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TransfersSyncResponse {
    pub server_time: DateTime<Utc>,
    pub transfers: Vec<TransferView>,
}

pub(crate) async fn held_order_view<'e, E>(
    executor: E,
    id: Uuid,
) -> Result<Option<HeldOrderView>, AppError>
where
    E: PgExecutor<'e>,
{
    #[allow(clippy::type_complexity)]
    let row: Option<(
        Uuid,
        Uuid,
        Option<Uuid>,
        Option<String>,
        String,
        serde_json::Value,
        String,
        Option<Uuid>,
        Option<String>,
        Option<String>,
        Option<Uuid>,
        i64,
        DateTime<Utc>,
        DateTime<Utc>,
    )> = sqlx::query_as(
        "SELECT h.id, h.branch_id, h.table_id, bt.label, h.name, h.cart, h.status, \
                h.created_by, h.device_id, h.claimed_by_device, h.order_id, h.revision, \
                h.created_at, h.updated_at \
         FROM held_orders h LEFT JOIN branch_tables bt ON bt.id = h.table_id \
         WHERE h.id = $1",
    )
    .bind(id)
    .fetch_optional(executor)
    .await?;
    Ok(row.map(
        |(
            id,
            branch_id,
            table_id,
            table_label,
            name,
            cart,
            status,
            created_by,
            device_id,
            claimed_by_device,
            order_id,
            revision,
            created_at,
            updated_at,
        )| HeldOrderView {
            id,
            branch_id,
            table_id,
            table_label,
            name,
            cart,
            status,
            created_by,
            device_id,
            claimed_by_device,
            order_id,
            revision,
            created_at,
            updated_at,
        },
    ))
}

pub(crate) async fn transfer_view<'e, E>(
    executor: E,
    id: Uuid,
) -> Result<Option<TransferView>, AppError>
where
    E: PgExecutor<'e>,
{
    #[allow(clippy::type_complexity)]
    let row: Option<(
        Uuid,
        Uuid,
        String,
        Uuid,
        Option<String>,
        Option<Uuid>,
        Option<Uuid>,
        Option<Uuid>,
        Option<String>,
        String,
        Option<Uuid>,
        Option<Uuid>,
        DateTime<Utc>,
        Option<DateTime<Utc>>,
        DateTime<Utc>,
    )> = sqlx::query_as(
        "SELECT t.id, t.branch_id, t.occupant_kind, t.occupant_id, \
                CASE t.occupant_kind \
                    WHEN 'held_order'  THEN (SELECT NULLIF(h.name, '') FROM held_orders h WHERE h.id = t.occupant_id) \
                    WHEN 'open_ticket' THEN (SELECT ot.ticket_ref FROM open_tickets ot WHERE ot.id = t.occupant_id) \
                END, \
                t.from_table_id, t.target_section_id, t.target_table_id, t.note, t.status, \
                t.requested_by, t.fulfilled_table_id, t.created_at, t.resolved_at, t.updated_at \
         FROM table_transfer_requests t WHERE t.id = $1",
    )
    .bind(id)
    .fetch_optional(executor)
    .await?;
    Ok(row.map(
        |(
            id,
            branch_id,
            occupant_kind,
            occupant_id,
            occupant_label,
            from_table_id,
            target_section_id,
            target_table_id,
            note,
            status,
            requested_by,
            fulfilled_table_id,
            created_at,
            resolved_at,
            updated_at,
        )| TransferView {
            id,
            branch_id,
            occupant_kind,
            occupant_id,
            occupant_label,
            from_table_id,
            target_section_id,
            target_table_id,
            note,
            status,
            requested_by,
            fulfilled_table_id,
            created_at,
            resolved_at,
            updated_at,
        },
    ))
}

// ── Table occupancy arbitration ──────────────────────────────────────────────
//
// The invariant: a table has at most ONE live occupant across held orders
// (status held/resumed) and open tickets (status open/ready). The
// `branch_tables` row is the mutex — every mutation locks it first, then
// checks both entities inside the same transaction. The partial unique index
// on held_orders backstops the held-order half against races this code misses.

/// A table's live occupant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Occupant {
    HeldOrder(Uuid),
    OpenTicket(Uuid),
}

impl Occupant {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Occupant::HeldOrder(_) => "held_order",
            Occupant::OpenTicket(_) => "open_ticket",
        }
    }
    pub(crate) fn id(&self) -> Uuid {
        match self {
            Occupant::HeldOrder(id) | Occupant::OpenTicket(id) => *id,
        }
    }
}

/// Lock `table_id` (the per-table occupancy mutex) and confirm it belongs to
/// `branch_id`. `false` = no such table in this branch (a stale or foreign
/// layout id) — callers decide whether that's a drop (offline park) or a 400.
pub(crate) async fn lock_table(
    tx: &mut Transaction<'_, Postgres>,
    table_id: Uuid,
    branch_id: Uuid,
) -> Result<bool, AppError> {
    let found: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM branch_tables WHERE id = $1 AND branch_id = $2 FOR UPDATE",
    )
    .bind(table_id)
    .bind(branch_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(found.is_some())
}

/// The table's live occupant, if any — optionally ignoring one held order /
/// one ticket (the entity being moved, which may already sit there).
pub(crate) async fn occupant_of(
    tx: &mut Transaction<'_, Postgres>,
    table_id: Uuid,
    exclude_held: Option<Uuid>,
    exclude_ticket: Option<Uuid>,
) -> Result<Option<Occupant>, AppError> {
    let row: Option<(String, Uuid)> = sqlx::query_as(
        "SELECT kind, id FROM ( \
             SELECT 'held_order' AS kind, id, updated_at FROM held_orders \
              WHERE table_id = $1 AND status IN ('held','resumed') \
                AND ($2::uuid IS NULL OR id <> $2) \
             UNION ALL \
             SELECT 'open_ticket' AS kind, id, updated_at FROM open_tickets \
              WHERE table_id = $1 AND status IN ('open','ready') \
                AND ($3::uuid IS NULL OR id <> $3) \
         ) occ ORDER BY updated_at LIMIT 1",
    )
    .bind(table_id)
    .bind(exclude_held)
    .bind(exclude_ticket)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|(kind, id)| match kind.as_str() {
        "held_order" => Occupant::HeldOrder(id),
        _ => Occupant::OpenTicket(id),
    }))
}

/// Flag a table `seated` (an occupant landed on it).
pub(crate) async fn seat_table(
    tx: &mut Transaction<'_, Postgres>,
    table_id: Uuid,
) -> Result<(), AppError> {
    sqlx::query("UPDATE branch_tables SET status = 'seated', updated_at = now() WHERE id = $1")
        .bind(table_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Hand a table straight back to the room, AVAILABLE immediately. This is the
/// right walk when no party physically vacated the table: the order moved to
/// another table, was unassigned, swapped, or discarded before it was ever
/// served. A checkout does NOT come through here — see [`bus_table`].
pub(crate) async fn free_table(
    tx: &mut Transaction<'_, Postgres>,
    table_id: Uuid,
) -> Result<(), AppError> {
    sqlx::query("UPDATE branch_tables SET status = 'free', updated_at = now() WHERE id = $1")
        .bind(table_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// A party CHECKED OUT and left the table behind: it needs bussing before the
/// next party can sit, so it lands in `dirty`, not `free`. Clearing it is an
/// explicit human act — the POS prompts the teller right after the sale and
/// keeps the table on the tables screen with a one-tap clear until someone
/// says it is ready. Never auto-free on settle: a table that reads available
/// while it still holds the last party's plates is worse than one extra tap.
pub(crate) async fn bus_table(
    tx: &mut Transaction<'_, Postgres>,
    table_id: Uuid,
) -> Result<(), AppError> {
    sqlx::query("UPDATE branch_tables SET status = 'dirty', updated_at = now() WHERE id = $1")
        .bind(table_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Cancel the occupant's waiting transfer wish (its order left the floor —
/// settled, voided, completed, or discarded). Returns the ids cancelled so the
/// caller can publish `transfer.changed` after commit.
pub(crate) async fn cancel_waiting_transfers(
    tx: &mut Transaction<'_, Postgres>,
    occupant_kind: &str,
    occupant_id: Uuid,
) -> Result<Vec<Uuid>, AppError> {
    Ok(sqlx::query_scalar(
        "UPDATE table_transfer_requests \
         SET status = 'cancelled', resolved_at = now(), updated_at = now() \
         WHERE occupant_kind = $1 AND occupant_id = $2 AND status = 'waiting' \
         RETURNING id",
    )
    .bind(occupant_kind)
    .bind(occupant_id)
    .fetch_all(&mut **tx)
    .await?)
}

/// If the occupant just landed on a table its waiting transfer was wishing for
/// (the exact table, or any table in the wished section), resolve the wish.
/// Returns the fulfilled ids for post-commit publishing.
pub(crate) async fn autofulfill_transfers(
    tx: &mut Transaction<'_, Postgres>,
    occupant_kind: &str,
    occupant_id: Uuid,
    landed_table: Uuid,
) -> Result<Vec<Uuid>, AppError> {
    Ok(sqlx::query_scalar(
        "UPDATE table_transfer_requests \
         SET status = 'fulfilled', fulfilled_table_id = $3, resolved_at = now(), updated_at = now() \
         WHERE occupant_kind = $1 AND occupant_id = $2 AND status = 'waiting' \
           AND (target_table_id = $3 \
                OR (target_table_id IS NULL AND target_section_id = \
                    (SELECT section_id FROM branch_tables WHERE id = $3))) \
         RETURNING id",
    )
    .bind(occupant_kind)
    .bind(occupant_id)
    .bind(landed_table)
    .fetch_all(&mut **tx)
    .await?)
}

// ── Post-commit event publishing ─────────────────────────────────────────────
//
// Everything rides `Topic::Floor` with LEAN invalidation payloads (no cart
// contents on the bus) — devices re-pull through the sync endpoints, which are
// the single source of truth either way.

/// Mutations collect the events they caused and publish them after commit.
#[derive(Default)]
pub(crate) struct FloorEvents {
    pub held_orders: Vec<Uuid>,
    pub transfers: Vec<Uuid>,
    pub tables: Vec<Uuid>,
    pub tickets: Vec<Uuid>,
}

impl FloorEvents {
    pub(crate) async fn publish(self, pool: &sqlx::PgPool, hub: &BranchEventHub, branch_id: Uuid) {
        for id in self.held_orders {
            #[allow(clippy::type_complexity)]
            let row: Option<(String, Option<Uuid>, i64)> =
                sqlx::query_as("SELECT status, table_id, revision FROM held_orders WHERE id = $1")
                    .bind(id)
                    .fetch_optional(pool)
                    .await
                    .ok()
                    .flatten();
            if let Some((status, table_id, revision)) = row {
                hub.publish(
                    branch_id,
                    BranchEvent::new(
                        Topic::Floor,
                        "held_order.changed",
                        &serde_json::json!({
                            "branch_id": branch_id, "id": id, "status": status,
                            "table_id": table_id, "revision": revision,
                        }),
                    ),
                );
            }
        }
        for id in self.transfers {
            let status: Option<String> =
                sqlx::query_scalar("SELECT status FROM table_transfer_requests WHERE id = $1")
                    .bind(id)
                    .fetch_optional(pool)
                    .await
                    .ok()
                    .flatten();
            if let Some(status) = status {
                hub.publish(
                    branch_id,
                    BranchEvent::new(
                        Topic::Floor,
                        "transfer.changed",
                        &serde_json::json!({ "branch_id": branch_id, "id": id, "status": status }),
                    ),
                );
            }
        }
        for id in self.tickets {
            hub.publish(
                branch_id,
                BranchEvent::new(
                    Topic::Floor,
                    "ticket.table_changed",
                    &serde_json::json!({ "branch_id": branch_id, "id": id }),
                ),
            );
        }
        for id in self.tables {
            let status: Option<String> =
                sqlx::query_scalar("SELECT status FROM branch_tables WHERE id = $1")
                    .bind(id)
                    .fetch_optional(pool)
                    .await
                    .ok()
                    .flatten();
            if let Some(status) = status {
                hub.publish(
                    branch_id,
                    BranchEvent::new(
                        Topic::Floor,
                        "table.status_changed",
                        &serde_json::json!({ "branch_id": branch_id, "table_id": id, "status": status }),
                    ),
                );
            }
        }
    }
}
