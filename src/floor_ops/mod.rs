//! Table occupancy and the transfer waitlist.
//!
//! Two cooperating pieces, both about the shared state of a room:
//!
//! - **Occupancy arbitration** — at most one live occupant per table. Taking a
//!   table INSERTS a `table_occupancies` row (who, when, from which till, under
//!   what: a ticket or a bare party hold) and giving it back ENDS that row (when,
//!   by whom, why). A table's status is never written; it is DERIVED from the
//!   ledger by `v_table_status`, and that view is the only thing read here.
//!   Every mutation still locks the `branch_tables` row (the per-table mutex)
//!   and checks inside the same transaction; the ledger's partial unique index
//!   catches any path that forgets.
//!
//! - **Transfer waitlist** — "wants to move inside": a queued wish by a table's
//!   occupant for a section or a specific table, resolved through the same
//!   arbitration and auto-fulfilled when the party lands somewhere matching.
//!
//! ## What used to be here
//!
//! Three status walks (`seat` / `free` / `bus`) that wrote `branch_tables.status`
//! — a cache of a fact nobody stored. It could not say who seated a table or
//! when, a parked draft held a table anonymously, and the settle path grew its
//! own copy of the `dirty` UPDATE. The column survives for the transition,
//! projected from the ledger by a trigger, and is dropped once nothing reads it.
//!
//! Server-side *held orders* are gone too: a parked order is a CLIENT-LOCAL
//! draft. Only its claim on a table crosses the wire, as a `party` occupancy
//! owned by the till that parked it.
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

// ── Table occupancy: the ledger ──────────────────────────────────────────────
//
// The invariant: a table has at most ONE live occupant. The `branch_tables`
// row is the mutex -- every mutation locks it first, then reads and writes the
// ledger inside the same transaction. `uq_table_occupancies_live_table` states
// the same thing in the database for anyone who skips the lock.
//
// Two kinds of occupant are written from here. A `ticket` row is a party with
// a bill; a `party` row is a bare hold -- a party that sat down before ordering,
// or a till parking a device-local draft on the table. Bookings are NOT
// written: a booking's hold is derived at read time by the floor (see
// `src/bookings`), so a `booking` row is only ever read here, never inserted.

/// Stable `code`s for the refusals a till must branch on.
///
/// A queued floor op replays with nobody watching, and the POS mapped every
/// 409 to "done" for want of anything machine-readable in the body. These are
/// what it reads instead. A request that finds the floor already in the state
/// it asked for is NOT a refusal -- it is a 200 -- so a code here always means
/// the op did not happen and will not happen by retrying.
pub(crate) mod refusal {
    /// A ticket owns the table.
    pub const TABLE_OCCUPIED: &str = "TABLE_OCCUPIED";
    /// Another till's hold, or a booking, owns the table.
    pub const TABLE_HELD: &str = "TABLE_HELD";
    /// The last party's plates are still on it; someone has to clear it first.
    pub const TABLE_DIRTY: &str = "TABLE_DIRTY";
    /// The party's ticket has settled or been voided since the op was queued.
    pub const TICKET_NOT_LIVE: &str = "TICKET_NOT_LIVE";
    /// The party already has a waiting transfer wish.
    pub const TRANSFER_EXISTS: &str = "TRANSFER_EXISTS";
    pub const TRANSFER_CANCELLED: &str = "TRANSFER_CANCELLED";
    pub const TRANSFER_FULFILLED: &str = "TRANSFER_FULFILLED";
}

pub(crate) fn refused(code: &'static str, reason: impl Into<String>) -> AppError {
    AppError::Refused {
        code,
        reason: reason.into(),
    }
}

/// The person putting a hand on a table, and the till they are at. Stamped on
/// every ledger write (`started_by` / `ended_by` / `cleared_by` and the till
/// beside each), so the row can answer "who did this, and from where".
#[derive(Debug, Clone, Copy)]
pub(crate) struct Hand {
    pub user_id: Uuid,
    pub till_id: Option<Uuid>,
}

impl Hand {
    /// `user_id`, at whatever till their open shift on `branch_id` is at.
    ///
    /// A waiter's handheld and the dashboard open no shift, so they have no
    /// till -- which is exactly what the ledger's NULL means. Nothing on the
    /// wire names the POS installation yet, so `started_device` is left NULL
    /// rather than invented.
    pub(crate) async fn of<'e, E>(exec: E, user_id: Uuid, branch_id: Uuid) -> Result<Self, AppError>
    where
        E: PgExecutor<'e>,
    {
        let till_id: Option<Uuid> = sqlx::query_scalar(
            "SELECT till_id FROM shifts \
              WHERE teller_id = $1 AND branch_id = $2 AND status = 'open' \
              ORDER BY opened_at DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(branch_id)
        .fetch_optional(exec)
        .await?;
        Ok(Self { user_id, till_id })
    }
}

/// What a new occupancy row is under.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Holder {
    /// A party with a bill. `booking_id` names the booking this ticket seated,
    /// when the waiter fired it with one.
    Ticket { id: Uuid, booking_id: Option<Uuid> },
    /// A bare hold: a party sitting down before ordering, or a till parking a
    /// device-local draft. Owned by the hand that placed it.
    Party,
}

/// Why an occupancy ended. Mirrors `table_occupancies.end_reason`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum EndReason {
    /// The bill was paid; the party is leaving (and the table needs a bus).
    Settled,
    /// The ticket was voided: nobody ate, nothing to bus.
    Voided,
    /// The party moved to another table; a new row opens there.
    Moved,
    /// A bare hold became a ticket; the ticket's row opens on the same table.
    Seated,
    /// A hold let go with no sale: draft discarded, walk-in left.
    Released,
}

impl EndReason {
    fn as_str(self) -> &'static str {
        match self {
            EndReason::Settled => "settled",
            EndReason::Voided => "voided",
            EndReason::Moved => "moved",
            EndReason::Seated => "seated",
            EndReason::Released => "released",
        }
    }
}

/// The live ledger row on a table.
#[derive(Debug, Clone)]
pub(crate) struct Occupancy {
    pub id: Uuid,
    /// `ticket` | `booking` | `party`.
    pub held_by: String,
    pub open_ticket_id: Option<Uuid>,
    pub booking_id: Option<Uuid>,
    pub started_by: Option<Uuid>,
    pub started_till_id: Option<Uuid>,
    pub party_size: Option<i16>,
}

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

/// The table's derived status -- `free` | `held` | `seated` | `dirty` -- or
/// `None` for a table that does not exist. The one sanctioned read of status.
pub(crate) async fn table_status<'e, E>(exec: E, table_id: Uuid) -> Result<Option<String>, AppError>
where
    E: PgExecutor<'e>,
{
    Ok(
        sqlx::query_scalar("SELECT status FROM v_table_status WHERE table_id = $1")
            .bind(table_id)
            .fetch_optional(exec)
            .await?,
    )
}

/// The live row on `table_id`, whatever kind of occupant it is.
pub(crate) async fn live_occupancy(
    tx: &mut Transaction<'_, Postgres>,
    table_id: Uuid,
) -> Result<Option<Occupancy>, AppError> {
    #[allow(clippy::type_complexity)]
    let row: Option<(
        Uuid,
        String,
        Option<Uuid>,
        Option<Uuid>,
        Option<Uuid>,
        Option<Uuid>,
        Option<i16>,
    )> = sqlx::query_as(
        "SELECT id, held_by, open_ticket_id, booking_id, started_by, started_till_id, party_size \
               FROM table_occupancies WHERE table_id = $1 AND ended_at IS NULL",
    )
    .bind(table_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(
        |(id, held_by, open_ticket_id, booking_id, started_by, started_till_id, party_size)| {
            Occupancy {
                id,
                held_by,
                open_ticket_id,
                booking_id,
                started_by,
                started_till_id,
                party_size,
            }
        },
    ))
}

/// The TICKET sitting on this table, if any -- optionally ignoring one (the
/// ticket being moved, which may already be there). A bare hold is not a
/// ticket and does not count: a ticket may land on one (see [`take_table`]).
pub(crate) async fn occupant_of(
    tx: &mut Transaction<'_, Postgres>,
    table_id: Uuid,
    exclude_ticket: Option<Uuid>,
) -> Result<Option<Uuid>, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT open_ticket_id FROM table_occupancies \
          WHERE table_id = $1 AND ended_at IS NULL AND open_ticket_id IS NOT NULL \
            AND ($2::uuid IS NULL OR open_ticket_id <> $2)",
    )
    .bind(table_id)
    .bind(exclude_ticket)
    .fetch_optional(&mut **tx)
    .await?)
}

/// May a fired round land on `table_id`? Locks the table on the way.
///
/// Yes when nobody has a bill there and nobody has plates there: a free table,
/// or a bare hold -- which is the ordinary case, the party sat down first and
/// this is their first round arriving to start the bill. No for a ticket
/// (that party's bill is already open) and no for `dirty`: nobody has bussed
/// it, and a fire silently clearing that would seat the next party on
/// somebody else's plates. The caller decides what "no" means -- a queued fire
/// drops the table rather than dead-lettering.
pub(crate) async fn ticket_may_claim(
    tx: &mut Transaction<'_, Postgres>,
    table_id: Uuid,
    branch_id: Uuid,
) -> Result<bool, AppError> {
    if !lock_table(tx, table_id, branch_id).await? {
        return Ok(false);
    }
    if occupant_of(tx, table_id, None).await?.is_some() {
        return Ok(false);
    }
    Ok(table_status(&mut **tx, table_id).await?.as_deref() != Some("dirty"))
}

/// What [`take_table`] did.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Taken {
    /// `false` when the holder was already on the table and nothing was
    /// written -- the double-tap, and a replayed op after a lost ack. Callers
    /// publish a status change only when this is `true`.
    pub landed: bool,
}

/// Take a table: the seating primitive, and the ONLY insert into the ledger.
///
/// The caller holds the table lock. Against what is already there:
///
/// * nothing, or a cleared table       -> a new row;
/// * this same ticket                  -> no write (`landed: false`);
/// * this same hand's bare hold        -> no write, when re-holding;
/// * a bare hold or a booking's hold   -> ended `seated` and a ticket row
///   opens, carrying the booking it seated -- the party was waiting for
///   exactly this bill;
/// * another ticket                    -> refused `TABLE_OCCUPIED`;
/// * another hand's hold or a booking  -> refused `TABLE_HELD`, when holding;
/// * plates still on it (`dirty`)      -> refused `TABLE_DIRTY`, whoever asks.
///
/// "Same hand" is the same user or the same till: a draft parked by the
/// morning teller is re-held by the afternoon one on the same till after a
/// shift handover, and that is one hold, not a fight over the table.
pub(crate) async fn take_table(
    tx: &mut Transaction<'_, Postgres>,
    table_id: Uuid,
    holder: Holder,
    party_size: Option<i16>,
    by: &Hand,
) -> Result<Taken, AppError> {
    let live = live_occupancy(tx, table_id).await?;
    let mut booking_id = match holder {
        Holder::Ticket { booking_id, .. } => booking_id,
        Holder::Party => None,
    };
    // A ticket taking over a bare party hold keeps the party's clock: they sat
    // down when the hold says, not when their first round went in.
    let mut seated_from: Option<Uuid> = None;
    let mut party_size = party_size;
    match (&live, holder) {
        (Some(o), Holder::Ticket { id, .. }) if o.open_ticket_id == Some(id) => {
            return Ok(Taken { landed: false });
        }
        (Some(o), _) if o.held_by == "ticket" => {
            return Err(refused(
                refusal::TABLE_OCCUPIED,
                "Someone is seated at this table",
            ));
        }
        (Some(o), Holder::Party) => {
            let same_hand = o.started_by == Some(by.user_id)
                || (by.till_id.is_some() && o.started_till_id == by.till_id);
            if o.held_by == "party" && same_hand {
                return Ok(Taken { landed: false });
            }
            return Err(refused(
                refusal::TABLE_HELD,
                if o.held_by == "booking" {
                    "This table is held for a booking"
                } else {
                    "Another till is holding this table"
                },
            ));
        }
        (Some(o), Holder::Ticket { .. }) => {
            booking_id = booking_id.or(o.booking_id);
            if o.held_by == "party" {
                seated_from = Some(o.id);
            }
            // The covers the host counted at the door stay with the party
            // when their first round opens the bill without a guest count.
            party_size = party_size.or(o.party_size);
            end_occupancy_row(&mut **tx, o.id, EndReason::Seated, false, by).await?;
        }
        (None, _) => {
            if table_status(&mut **tx, table_id).await?.as_deref() == Some("dirty") {
                return Err(refused(
                    refusal::TABLE_DIRTY,
                    "Table has not been cleared since the last party",
                ));
            }
        }
    }
    let (held_by, open_ticket_id) = match holder {
        Holder::Ticket { id, .. } => ("ticket", Some(id)),
        Holder::Party => ("party", None),
    };
    sqlx::query(
        "INSERT INTO table_occupancies \
            (org_id, branch_id, table_id, held_by, open_ticket_id, booking_id, party_size, \
             started_by, started_till_id, seated_at) \
         SELECT bt.org_id, bt.branch_id, bt.id, $2, $3, $4, $5, $6, $7, \
                CASE WHEN $3::uuid IS NOT NULL THEN COALESCE( \
                    (SELECT ot.seated_at FROM open_tickets ot WHERE ot.id = $3), \
                    (SELECT COALESCE(p.seated_at, p.started_at) FROM table_occupancies p WHERE p.id = $8), \
                    (SELECT ot.opened_at FROM open_tickets ot WHERE ot.id = $3)) END \
           FROM branch_tables bt WHERE bt.id = $1",
    )
    .bind(table_id)
    .bind(held_by)
    .bind(open_ticket_id)
    .bind(booking_id)
    .bind(party_size)
    .bind(by.user_id)
    .bind(by.till_id)
    .bind(seated_from)
    .execute(&mut **tx)
    .await?;
    if let Some(ticket_id) = open_ticket_id {
        // The bill carries the same instant, so a settle can copy it onto the
        // sale and a later move inherits it.
        sqlx::query(
            "UPDATE open_tickets t SET seated_at = o.seated_at \
               FROM table_occupancies o \
              WHERE t.id = $1 AND t.seated_at IS NULL \
                AND o.open_ticket_id = t.id AND o.ended_at IS NULL",
        )
        .bind(ticket_id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(Taken { landed: true })
}

/// Stamp the live party hold on `table_id` with when the party sat, by the
/// till's clock. Clamped: never in the future, never more than 12 hours back,
/// never before the table's previous occupancy ended (or was cleared). Only
/// the clock is written; status is the ledger's, not this.
pub(crate) async fn stamp_party_seated_at(
    tx: &mut Transaction<'_, Postgres>,
    table_id: Uuid,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE table_occupancies o \
            SET seated_at = LEAST(o.started_at, GREATEST($2, o.started_at - interval '12 hours', \
                    COALESCE((SELECT MAX(COALESCE(p.cleared_at, p.ended_at)) FROM table_occupancies p \
                               WHERE p.table_id = o.table_id AND p.id <> o.id AND p.ended_at IS NOT NULL), \
                             '-infinity'::timestamptz))) \
          WHERE o.table_id = $1 AND o.ended_at IS NULL AND o.held_by = 'party'",
    )
    .bind(table_id)
    .bind(at)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// End one row: when, by whom, why, and whether the party left plates behind.
async fn end_occupancy_row<'e, E>(
    exec: E,
    occupancy_id: Uuid,
    reason: EndReason,
    needs_bussing: bool,
    by: &Hand,
) -> Result<(), AppError>
where
    E: PgExecutor<'e>,
{
    sqlx::query(
        "UPDATE table_occupancies \
            SET ended_at = now(), ended_by = $2, ended_till_id = $3, end_reason = $4, \
                needs_bussing = $5, updated_at = now() \
          WHERE id = $1 AND ended_at IS NULL",
    )
    .bind(occupancy_id)
    .bind(by.user_id)
    .bind(by.till_id)
    .bind(reason.as_str())
    .bind(needs_bussing)
    .execute(exec)
    .await?;
    Ok(())
}

/// End a ticket's live occupancy, wherever it is sitting. Returns the table it
/// left, `None` when it was not on one (a table-less order, or a lost-ack
/// retry of the same end). `needs_bussing` is the checkout fork: a party that
/// paid leaves the table `dirty` until a person clears it; a void or a move
/// leaves nothing behind and the table reads `free` at once.
///
/// Generic over the executor because the settle path ends the row after the
/// order's own transaction has committed (best-effort, as its booking link
/// is), while every other caller ends it under the table lock.
pub(crate) async fn end_ticket_occupancy<'e, E>(
    exec: E,
    ticket_id: Uuid,
    reason: EndReason,
    needs_bussing: bool,
    by: &Hand,
) -> Result<Option<Uuid>, AppError>
where
    E: PgExecutor<'e>,
{
    Ok(sqlx::query_scalar(
        "UPDATE table_occupancies \
            SET ended_at = now(), ended_by = $2, ended_till_id = $3, end_reason = $4, \
                needs_bussing = $5, updated_at = now() \
          WHERE open_ticket_id = $1 AND ended_at IS NULL \
          RETURNING table_id",
    )
    .bind(ticket_id)
    .bind(by.user_id)
    .bind(by.till_id)
    .bind(reason.as_str())
    .bind(needs_bussing)
    .fetch_optional(exec)
    .await?)
}

/// Let go of the bare hold on `table_id`, if there is one (`released`). A
/// ticket's row is never touched from here -- a ticket owns its table until
/// its bill ends. Returns whether a hold was there to release.
pub(crate) async fn release_party_hold(
    tx: &mut Transaction<'_, Postgres>,
    table_id: Uuid,
    needs_bussing: bool,
    by: &Hand,
) -> Result<bool, AppError> {
    let ended = sqlx::query(
        "UPDATE table_occupancies \
            SET ended_at = now(), ended_by = $2, ended_till_id = $3, end_reason = $4, \
                needs_bussing = $5, updated_at = now() \
          WHERE table_id = $1 AND ended_at IS NULL AND held_by = 'party'",
    )
    .bind(table_id)
    .bind(by.user_id)
    .bind(by.till_id)
    .bind(EndReason::Released.as_str())
    .bind(needs_bussing)
    .execute(&mut **tx)
    .await?;
    Ok(ended.rows_affected() > 0)
}

/// "The plates are gone": record the clearing on the row that left them. The
/// one transition no server can observe, so it is a person's word, and the
/// ledger keeps whose. Returns whether there was anything to clear.
pub(crate) async fn clear_bussing(
    tx: &mut Transaction<'_, Postgres>,
    table_id: Uuid,
    by: &Hand,
) -> Result<bool, AppError> {
    let cleared = sqlx::query(
        "UPDATE table_occupancies o \
            SET cleared_at = now(), cleared_by = $2, updated_at = now() \
          WHERE o.id = (SELECT id FROM table_occupancies \
                         WHERE table_id = $1 ORDER BY started_at DESC, id DESC LIMIT 1) \
            AND o.ended_at IS NOT NULL AND o.needs_bussing AND o.cleared_at IS NULL",
    )
    .bind(table_id)
    .bind(by.user_id)
    .execute(&mut **tx)
    .await?;
    Ok(cleared.rows_affected() > 0)
}

/// Move a live ticket onto `to_table` (or off any table, `None`): its current
/// row ends `moved`, `open_tickets.table_id` follows, a row opens on the new
/// table through [`take_table`], and any transfer wish the landing satisfied
/// resolves. Returns the fulfilled wish ids for post-commit publishing.
///
/// The caller holds the lock on `to_table`. Already sitting there is a no-op,
/// so a replayed move cannot end and reopen its own row.
pub(crate) async fn relocate_ticket(
    tx: &mut Transaction<'_, Postgres>,
    ticket_id: Uuid,
    to_table: Option<Uuid>,
    by: &Hand,
) -> Result<Vec<Uuid>, AppError> {
    let row: Option<(Option<Uuid>, Option<Uuid>, Option<i32>)> = sqlx::query_as(
        "SELECT (SELECT table_id FROM table_occupancies \
                  WHERE open_ticket_id = t.id AND ended_at IS NULL), \
                t.booking_id, t.guest_count \
           FROM open_tickets t WHERE t.id = $1",
    )
    .bind(ticket_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((current, booking_id, guest_count)) = row else {
        return Err(AppError::NotFound("Open ticket not found".into()));
    };
    if current.is_some() && current != to_table {
        end_ticket_occupancy(&mut **tx, ticket_id, EndReason::Moved, false, by).await?;
    }
    sqlx::query("UPDATE open_tickets SET table_id = $2, updated_at = now() WHERE id = $1")
        .bind(ticket_id)
        .bind(to_table)
        .execute(&mut **tx)
        .await?;
    let Some(t) = to_table else {
        return Ok(Vec::new());
    };
    take_table(
        tx,
        t,
        Holder::Ticket {
            id: ticket_id,
            booking_id,
        },
        party_size(guest_count),
        by,
    )
    .await?;
    autofulfill_transfers(tx, ticket_id, t).await
}

/// What a swap can pick up off a table and carry to another.
#[derive(Debug, Clone)]
pub(crate) enum Movable {
    /// A party with a bill.
    Ticket(Uuid),
    /// A party sitting with no bill yet (or a till's parked draft).
    Party(Occupancy),
}

/// The occupant a move/swap would carry off `table_id`. A booking's claim is
/// not a party at the table, so there is nothing to carry: it stays, and a
/// bill landing on it seats the booking (`take_table`), as tills before this
/// change relied on.
pub(crate) async fn movable_on(
    tx: &mut Transaction<'_, Postgres>,
    table_id: Uuid,
) -> Result<Option<Movable>, AppError> {
    match live_occupancy(tx, table_id).await? {
        None => Ok(None),
        Some(o) if o.held_by == "ticket" => Ok(o.open_ticket_id.map(Movable::Ticket)),
        Some(o) if o.held_by == "party" => Ok(Some(Movable::Party(o))),
        Some(_) => Ok(None),
    }
}

/// A bare party hold leaves its table: its row ends `moved`, nothing to bus.
pub(crate) async fn end_party_hold_moved(
    tx: &mut Transaction<'_, Postgres>,
    hold: &Occupancy,
    by: &Hand,
) -> Result<(), AppError> {
    end_occupancy_row(&mut **tx, hold.id, EndReason::Moved, false, by).await
}

/// Open `hold` again on `to_table`: same owner (so the till that placed it
/// can still release it), same covers, same seating clock. The caller holds
/// the lock on `to_table` and has already emptied it.
pub(crate) async fn land_party_hold(
    tx: &mut Transaction<'_, Postgres>,
    hold: &Occupancy,
    to_table: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO table_occupancies \
            (org_id, branch_id, table_id, held_by, party_size, started_by, started_till_id, \
             started_device, seated_at) \
         SELECT bt.org_id, bt.branch_id, bt.id, 'party', p.party_size, p.started_by, \
                p.started_till_id, p.started_device, COALESCE(p.seated_at, p.started_at) \
           FROM branch_tables bt, table_occupancies p \
          WHERE bt.id = $1 AND p.id = $2",
    )
    .bind(to_table)
    .bind(hold.id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// A ticket's `guest_count` as the ledger's `party_size`: a smallint that
/// must be positive, so anything else is simply not recorded.
pub(crate) fn party_size(guest_count: Option<i32>) -> Option<i16> {
    guest_count
        .filter(|n| *n > 0)
        .and_then(|n| i16::try_from(n).ok())
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
            let status = table_status(pool, id).await.ok().flatten();
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
