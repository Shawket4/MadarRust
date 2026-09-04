//! Table occupancy and the transfer waitlist.
//!
//! Two cooperating pieces, both about the shared state of a room:
//!
//! - **Occupancy arbitration** — at most one live occupant per table, and an
//!   occupant is always an OPEN TICKET. Every mutation locks the `branch_tables`
//!   row (the per-table mutex), checks inside the same transaction, and walks
//!   the status (`seated` on landing, `free` when nobody vacated, `dirty` after
//!   a checkout). Those three walks are the ONLY writers of
//!   `branch_tables.status`; there is no endpoint that sets it.
//!
//! - **Transfer waitlist** — "wants to move inside": a queued wish by a table's
//!   occupant for a section or a specific table, resolved through the same
//!   arbitration and auto-fulfilled when the party lands somewhere matching.
//!
//! ## What used to be here
//!
//! Server-side *held orders*: parked carts that owned a table and could be
//! claimed by another till. They are gone. A parked order is now a CLIENT-LOCAL
//! draft — the terminal's own way to juggle several orders at once — so parking,
//! naming, resuming and discarding one never touch the network and never need a
//! claim lease, a revision fence, or an offline replay op.
//!
//! An order reaches the floor only by becoming a ticket. That collapsed the
//! occupant model from two kinds to one, and with it a whole class of question
//! ("which kind is sitting here, and may this actor move that kind?").
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
pub struct TransferView {
    pub id: Uuid,
    pub branch_id: Uuid,
    /// Always `open_ticket`. Kept on the wire so the rebuilt booking flow
    /// can queue into the same waitlist without a schema change.
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
                (SELECT ot.ticket_ref FROM open_tickets ot WHERE ot.id = t.occupant_id), \
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
// The invariant: a table has at most ONE live occupant, and an occupant is
// always an OPEN TICKET. The `branch_tables` row is the mutex -- every mutation
// locks it first, then checks inside the same transaction.
//
// It used to be two kinds, because a parked cart could own a table too. Parked
// orders are now client-local drafts with no server identity, so an order only
// reaches the floor by becoming a ticket. That halves this file's arbitration
// and removes a whole class of question ("which kind is sitting here?").

/// Lock `table_id` (the per-table occupancy mutex) and confirm it belongs to
/// `branch_id`. `false` = no such table in this branch (a stale or foreign
/// layout id).
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

/// The ticket sitting on this table, if any — optionally ignoring one (the
/// ticket being moved, which may already be there).
pub(crate) async fn occupant_of(
    tx: &mut Transaction<'_, Postgres>,
    table_id: Uuid,
    exclude_ticket: Option<Uuid>,
) -> Result<Option<Uuid>, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM open_tickets \
          WHERE table_id = $1 AND status IN ('open','ready') \
            AND ($2::uuid IS NULL OR id <> $2) \
          ORDER BY updated_at LIMIT 1",
    )
    .bind(table_id)
    .bind(exclude_ticket)
    .fetch_optional(&mut **tx)
    .await?)
}

/// Flag a table `seated` (an occupant landed on it).
/// The three status walks, and the ONLY places `branch_tables.status` is
/// written.
///
/// Generic over the executor so the same walk serves a transactional caller
/// (which locks the table row first) and the non-transactional settle path.
/// They used to be transaction-only, which is why the settle path grew its own
/// inline copy of the `dirty` UPDATE -- two writers of one invariant, drifting
/// apart by construction.
///
/// Status is never set directly by a request. There is no endpoint for it: a
/// table is occupied because a ticket sits on it, and free because none does.
pub(crate) async fn seat_table<'e, E>(exec: E, table_id: Uuid) -> Result<(), AppError>
where
    E: PgExecutor<'e>,
{
    set_status(exec, table_id, "seated").await
}

/// Hand a table straight back to the room, AVAILABLE immediately. The right
/// walk when nobody physically vacated it: the order moved to another table,
/// was unassigned, swapped, or voided before it was ever served. A checkout
/// does NOT come through here -- see [`bus_table`].
pub(crate) async fn free_table<'e, E>(exec: E, table_id: Uuid) -> Result<(), AppError>
where
    E: PgExecutor<'e>,
{
    set_status(exec, table_id, "free").await
}

/// A party CHECKED OUT and left the table behind: it needs bussing before the
/// next party can sit, so it lands in `dirty`, not `free`. Clearing it is an
/// explicit human act -- the POS asks the teller immediately after the sale and
/// keeps a one-tap clear on the tables screen until someone says it is ready.
/// Never auto-free on settle: a table that reads available while it still holds
/// the last party's plates is worse than one extra tap.
pub(crate) async fn bus_table<'e, E>(exec: E, table_id: Uuid) -> Result<(), AppError>
where
    E: PgExecutor<'e>,
{
    set_status(exec, table_id, "dirty").await
}

async fn set_status<'e, E>(exec: E, table_id: Uuid, status: &str) -> Result<(), AppError>
where
    E: PgExecutor<'e>,
{
    sqlx::query("UPDATE branch_tables SET status = $2, updated_at = now() WHERE id = $1")
        .bind(table_id)
        .bind(status)
        .execute(exec)
        .await?;
    Ok(())
}

/// Cancel the occupant's waiting transfer wish (its order left the floor —
/// settled, voided, completed, or discarded). Returns the ids cancelled so the
/// caller can publish `transfer.changed` after commit.
pub(crate) async fn cancel_waiting_transfers(
    tx: &mut Transaction<'_, Postgres>,
    occupant_id: Uuid,
) -> Result<Vec<Uuid>, AppError> {
    Ok(sqlx::query_scalar(
        "UPDATE table_transfer_requests \
         SET status = 'cancelled', resolved_at = now(), updated_at = now() \
         WHERE occupant_id = $1 AND status = 'waiting' \
         RETURNING id",
    )
    .bind(occupant_id)
    .fetch_all(&mut **tx)
    .await?)
}

/// If the occupant just landed on a table its waiting transfer was wishing for
/// (the exact table, or any table in the wished section), resolve the wish.
/// Returns the fulfilled ids for post-commit publishing.
pub(crate) async fn autofulfill_transfers(
    tx: &mut Transaction<'_, Postgres>,
    occupant_id: Uuid,
    landed_table: Uuid,
) -> Result<Vec<Uuid>, AppError> {
    Ok(sqlx::query_scalar(
        "UPDATE table_transfer_requests \
         SET status = 'fulfilled', fulfilled_table_id = $2, resolved_at = now(), updated_at = now() \
         WHERE occupant_id = $1 AND status = 'waiting' \
           AND (target_table_id = $2 \
                OR (target_table_id IS NULL AND target_section_id = \
                    (SELECT section_id FROM branch_tables WHERE id = $2))) \
         RETURNING id",
    )
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
    pub transfers: Vec<Uuid>,
    pub tables: Vec<Uuid>,
    pub tickets: Vec<Uuid>,
}

impl FloorEvents {
    pub(crate) async fn publish(self, pool: &sqlx::PgPool, hub: &BranchEventHub, branch_id: Uuid) {
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
