//! Floor endpoints: the atomic two-table swap, clearing a bussed table, a
//! till's hold/release of a table for its own parked draft, and the transfer
//! waitlist. Every occupancy change is a ledger write through the primitives
//! in the parent module; nothing here writes a status.
//!
//! Every mutation is split live-route / `*_inner` so `/sync/replay` can flush
//! a till's offline backlog through the same core (same idempotency, same
//! occupancy arbitration). Live wrappers do claims + permission + branch
//! checks; the cores stay claims-free and act for an [`ActingContext`].

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::{
    FloorEvents, Hand, Holder, TransferView, TransfersSyncResponse, clear_bussing, extract_claims,
    live_occupancy, lock_table, occupant_of, refusal, refused, release_party_hold, relocate_ticket,
    require_branch_access, take_table, transfer_view,
};
use crate::errors::{AppError, AppErrorResponse};
use crate::permissions::checker::{check_permission, check_permission_for};
use crate::realtime::hub::BranchEventHub;
use crate::sync::ActingContext;

// ── Requests ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, Serialize, ToSchema)]
pub struct SwapTablesRequest {
    pub branch_id: Uuid,
    pub table_a: Uuid,
    pub table_b: Uuid,
}

/// Operational table-state edit from the POS: the layout (geometry/shape) is
/// dashboard-authored, but STATE — status walks (bussing a dirty table) and
/// which zone the physical table currently sits in — belongs to the floor
/// staff. Both fields optional; `clear_section` moves the table out of every
/// section (`section_id` wins when both are sent).
#[derive(Deserialize, Serialize, ToSchema)]
pub struct CreateFloorTransferRequest {
    /// Client-minted id (offline-first identity; retries dedup on it).
    pub id: Uuid,
    pub branch_id: Uuid,
    /// `held_order` | `open_ticket`.
    pub occupant_kind: String,
    pub occupant_id: Uuid,
    /// The wish: any table in this section…
    #[serde(default)]
    pub target_section_id: Option<Uuid>,
    /// …or exactly this table. At least one of the two is required.
    #[serde(default)]
    pub target_table_id: Option<Uuid>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct FulfillTransferRequest {
    /// The table the party actually moves to (must satisfy the wish).
    pub table_id: Uuid,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListTransfersQuery {
    pub branch_id: Uuid,
    /// Sync cursor (as on /held-orders). Omit for the waiting queue only.
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
}

// ── Shared lookups ───────────────────────────────────────────────────────────

async fn require_transfer_branch_access(
    pool: &sqlx::PgPool,
    claims: &crate::auth::jwt::Claims,
    id: Uuid,
) -> Result<Uuid, AppError> {
    let branch_id: Option<Uuid> =
        sqlx::query_scalar("SELECT branch_id FROM table_transfer_requests WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let branch_id =
        branch_id.ok_or_else(|| AppError::NotFound("Transfer request not found".into()))?;
    require_branch_access(pool, claims, branch_id).await?;
    Ok(branch_id)
}

/// The live branch org (also confirms the branch exists and isn't deleted).
async fn branch_org(pool: &sqlx::PgPool, branch_id: Uuid) -> Result<Uuid, AppError> {
    sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
        .bind(branch_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::NotFound("Branch not found".into()))
}

/// Move one ticket onto `to_table` (or off any table when `None`) inside the
/// caller's transaction, collecting the events it caused.
async fn move_ticket(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ticket_id: Uuid,
    to_table: Option<Uuid>,
    by: &Hand,
    events: &mut FloorEvents,
) -> Result<(), AppError> {
    let fulfilled = relocate_ticket(tx, ticket_id, to_table, by).await?;
    events.tickets.push(ticket_id);
    events.transfers.extend(fulfilled);
    Ok(())
}

// ── Swap (atomic; covers move-to-empty and cross-entity swaps) ───────────────

#[utoipa::path(post, path = "/floor/tables/swap", tag = "floor",
    request_body = SwapTablesRequest,
    responses((status = 200, description = "Occupants swapped/moved"), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn swap_tables(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    body: web::Json<SwapTablesRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    // Per-occupant permissions are enforced in the core (it knows what sits on
    // each table); here only the branch boundary.
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    swap_tables_inner(
        pool,
        body,
        ActingContext::live(&claims)?,
        Some(hub.get_ref()),
    )
    .await
}

/// Swap core: exchange the tickets on two tables in ONE transaction. One
/// empty side degenerates to a move; both empty is a 400. A bare hold on the
/// "empty" side is not an occupant to swap -- the arriving ticket takes it
/// over, as a fire would.
pub(crate) async fn swap_tables_inner(
    pool: crate::db::Db,
    body: web::Json<SwapTablesRequest>,
    actor: ActingContext,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    if body.table_a == body.table_b {
        return Err(AppError::BadRequest("Pick two different tables".into()));
    }
    let mut events = FloorEvents::default();
    let mut tx = pool.get_ref().begin().await?;

    // Deadlock-proof: always take the two per-table locks in uuid order.
    let (first, second) = if body.table_a < body.table_b {
        (body.table_a, body.table_b)
    } else {
        (body.table_b, body.table_a)
    };
    for t in [first, second] {
        if !lock_table(&mut tx, t, body.branch_id).await? {
            return Err(AppError::BadRequest("Table is not in this branch".into()));
        }
    }
    let occ_a = occupant_of(&mut tx, body.table_a, None).await?;
    let occ_b = occupant_of(&mut tx, body.table_b, None).await?;
    if occ_a.is_none() && occ_b.is_none() {
        return Err(AppError::BadRequest("Both tables are empty".into()));
    }
    check_permission_for(
        pool.get_ref(),
        actor.teller_id,
        &actor.role,
        "open_tickets",
        "update",
    )
    .await?;

    let hand = Hand::of(&mut *tx, actor.teller_id, body.branch_id).await?;
    // Clear both sides before landing either: the live-per-table and
    // live-per-ticket indexes would refuse the second landing otherwise, and a
    // concurrent read never sees two tickets on one table.
    if let Some(t) = occ_a {
        move_ticket(&mut tx, t, None, &hand, &mut events).await?;
    }
    if let Some(t) = occ_b {
        move_ticket(&mut tx, t, None, &hand, &mut events).await?;
    }
    if let Some(t) = occ_a {
        move_ticket(&mut tx, t, Some(body.table_b), &hand, &mut events).await?;
    }
    if let Some(t) = occ_b {
        move_ticket(&mut tx, t, Some(body.table_a), &hand, &mut events).await?;
    }
    events.tables.push(body.table_a);
    events.tables.push(body.table_b);
    tx.commit().await?;

    if let Some(hub) = hub {
        events.publish(pool.get_ref(), hub, body.branch_id).await;
    }
    Ok(HttpResponse::Ok().json(serde_json::json!({ "ok": true })))
}

// ── Clearing a bussed table ─────────────────────────────────────────────────

#[derive(Debug, Deserialize, ToSchema)]
pub struct ClearTableRequest {
    pub branch_id: Uuid,
}

/// Mark a bussed table ready for the next party.
///
/// The ONE human act the ledger cannot derive. Everything else about a
/// table's status follows from its rows: seated while one is live, dirty
/// after a checkout ended it. But no server can see that the plates have been
/// cleared, so a person says so, and the row records who.
///
/// Deliberately not a set-status endpoint. Its predecessor took any status and
/// wrote it with no lock and no occupancy check, so it could declare a table
/// free while a ticket was open on it. This performs exactly one transition,
/// `dirty` -> `free`, and refuses anything else.
#[utoipa::path(
    post, path = "/floor/tables/{id}/clear", tag = "floor",
    params(("id" = Uuid, Path, description = "Table ID")),
    request_body = ClearTableRequest,
    responses((status = 200, description = "Table is ready"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn clear_table(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<ClearTableRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "open_tickets", "update").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    clear_table_inner(
        pool,
        *id,
        Some(body.branch_id),
        ActingContext::live(&claims)?,
        Some(hub.get_ref()),
    )
    .await
}

/// Clear core: the `dirty` -> `free` transition, shared by the live route and
/// `/sync/replay`.
///
/// `branch_id` is what the LIVE caller asserted, and the table must be in it.
/// Replay passes `None` and the branch is read off the table instead: the POS
/// queues this op with an empty `request` body (it has always sent `{}`), so
/// there is no branch on the wire — and shipped builds cannot be asked to start
/// sending one. That lookup is safe without a branch check of its own: `pool` is
/// tenant-scoped, so a table outside the caller's org is not visible and this
/// 404s (and `op_branch_must_be_in_org` has already rejected a cross-org table
/// outright), while the replay actor is always a teller/waiter/kitchen user —
/// all org-scoped rather than branch-scoped.
pub(crate) async fn clear_table_inner(
    pool: crate::db::Db,
    table_id: Uuid,
    branch_id: Option<Uuid>,
    actor: ActingContext,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    let branch_id = match branch_id {
        Some(b) => b,
        None => sqlx::query_scalar("SELECT branch_id FROM branch_tables WHERE id = $1")
            .bind(table_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound("Table not found".into()))?,
    };
    check_permission_for(
        pool.get_ref(),
        actor.teller_id,
        &actor.role,
        "open_tickets",
        "update",
    )
    .await?;

    let mut tx = pool.get_ref().begin().await?;
    if !lock_table(&mut tx, table_id, branch_id).await? {
        return Err(AppError::NotFound("Table not found".into()));
    }
    // A table someone is sitting at is not "bussed".
    if let Some(live) = live_occupancy(&mut tx, table_id).await? {
        return Err(match live.held_by.as_str() {
            "ticket" => refused(refusal::TABLE_OCCUPIED, "Someone is seated at this table"),
            _ => refused(
                refusal::TABLE_HELD,
                "This table is held, not waiting to be cleared",
            ),
        });
    }
    let hand = Hand::of(&mut *tx, actor.teller_id, branch_id).await?;
    // Nothing to clear is the common double-tap (and a replayed op after a
    // lost ack): `free` -> `free` is a yes, not a 409.
    if !clear_bussing(&mut tx, table_id, &hand).await? {
        tx.commit().await?;
        return Ok(HttpResponse::Ok().json(serde_json::json!({ "ok": true })));
    }
    tx.commit().await?;

    if let Some(hub) = hub {
        let mut events = FloorEvents::default();
        events.tables.push(table_id);
        events.publish(pool.get_ref(), hub, branch_id).await;
    }
    Ok(HttpResponse::Ok().json(serde_json::json!({ "ok": true })))
}

// ── A till's own hold on a table ─────────────────────────────────────────────

#[derive(Debug, Deserialize, ToSchema)]
pub struct HoldTableRequest {
    pub branch_id: Uuid,
    /// When the party actually sat down, by the till's clock. An offline seat
    /// replays later than it happened; this keeps every device's table clock
    /// on the seating. Clamped server-side to the last 12 hours, never in the
    /// future, and never before the table's previous party left. Recorded only
    /// -- it moves no status.
    #[serde(default)]
    pub seated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct ReleaseTableRequest {
    pub branch_id: Uuid,
    /// The party ATE here and has paid: the table needs bussing before anyone
    /// else sits, so it lands `dirty` rather than `free`. The same fork the
    /// till makes locally when a parked order checks out versus is discarded --
    /// a discard means nobody ever sat, and the table goes straight back to the
    /// room. Without this the dashboard would show a table with dirty plates on
    /// it as ready for the next party.
    #[serde(default)]
    pub bus: bool,
}

/// Take a table for a party with no bill yet.
///
/// Occupancy travels on its own here, carrying nothing about what is on the
/// table -- but always who took it: the hold is a `party` row in the ledger
/// owned by the hand that placed it, so there is no such thing as a table held
/// by nobody. Two things use it:
///
///   * A PARTY SITTING DOWN. They have ordered nothing yet, so there is no
///     bill — a ticket starts with their first round and claims this table on
///     the way in. Seating used to open an empty ticket instead, which put a
///     zero-value bill in every report and made a party who changed their mind
///     and left something you had to VOID.
///   * A PARKED CART. Device-local by design: the order, its lines and its
///     money never leave the till. But the table is not the till's private
///     business, and while it stayed local the dashboard's floor and every
///     other terminal were told a table with somebody's order waiting on it was
///     free.
///
/// In both cases the server learns that the table is taken, by whom and from
/// which till, and nothing whatever about what is on it.
///
/// Like `clear_table`, and for the reason written there, this is not a
/// set-status endpoint: exactly one transition, `free` -> `seated`, refused
/// from anything else with a `code` the till can act on. A table a ticket is
/// already on stays the ticket's; a table another till holds stays theirs.
#[utoipa::path(
    post, path = "/floor/tables/{id}/hold", tag = "floor",
    params(("id" = Uuid, Path, description = "Table ID")),
    request_body = HoldTableRequest,
    responses((status = 200, description = "Table is held"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn hold_table(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<HoldTableRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "open_tickets", "update").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    hold_table_inner(
        pool,
        *id,
        Some(body.branch_id),
        body.seated_at,
        ActingContext::live(&claims)?,
        Some(hub.get_ref()),
    )
    .await
}

/// The replay-safe half. See `clear_table_inner` for why this split exists:
/// a queued offline op flushes through exactly this code.
pub(crate) async fn hold_table_inner(
    pool: crate::db::Db,
    table_id: Uuid,
    branch_id: Option<Uuid>,
    seated_at: Option<DateTime<Utc>>,
    actor: ActingContext,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    let branch_id = match branch_id {
        Some(b) => b,
        None => sqlx::query_scalar("SELECT branch_id FROM branch_tables WHERE id = $1")
            .bind(table_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound("Table not found".into()))?,
    };

    let mut tx = pool.get_ref().begin().await?;
    if !lock_table(&mut tx, table_id, branch_id).await? {
        return Err(AppError::NotFound("Table not found".into()));
    }
    let hand = Hand::of(&mut *tx, actor.teller_id, branch_id).await?;
    // Already ours is the common double-tap and the replayed op after a
    // reconnect; saying yes twice is correct. Anything else that is not a free
    // table is a coded refusal -- see `take_table`.
    let taken = take_table(&mut tx, table_id, Holder::Party, None, &hand).await?;
    if taken.landed
        && let Some(at) = seated_at
    {
        super::stamp_party_seated_at(&mut tx, table_id, at).await?;
    }
    tx.commit().await?;

    if let Some(hub) = hub
        && taken.landed
    {
        let mut events = FloorEvents::default();
        events.tables.push(table_id);
        events.publish(pool.get_ref(), hub, branch_id).await;
    }
    Ok(HttpResponse::Ok().json(serde_json::json!({ "ok": true })))
}

/// Give back a table a till was holding for its own parked order.
///
/// The counterpart to `hold_table`: the hold moved to another table, was
/// checked out, or was discarded. Ends the `party` row -- leaving the table
/// `free`, or `dirty` when `bus` says the party ate -- and never touches a
/// ticket's: if one has landed since, the ticket owns the table and this is a
/// no-op rather than a way to free an occupied table.
///
/// Not owner-gated on purpose. The draft is device-local and outlives a shift
/// handover, so the teller who checks it out is often not the one who parked
/// it; the ledger records who released it instead of refusing them.
#[utoipa::path(
    post, path = "/floor/tables/{id}/release", tag = "floor",
    params(("id" = Uuid, Path, description = "Table ID")),
    request_body = ReleaseTableRequest,
    responses((status = 200, description = "Table is free"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn release_table(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<ReleaseTableRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "open_tickets", "update").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    release_table_inner(
        pool,
        *id,
        Some(body.branch_id),
        body.bus,
        ActingContext::live(&claims)?,
        Some(hub.get_ref()),
    )
    .await
}

/// The replay-safe half.
pub(crate) async fn release_table_inner(
    pool: crate::db::Db,
    table_id: Uuid,
    branch_id: Option<Uuid>,
    bus: bool,
    actor: ActingContext,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    let branch_id = match branch_id {
        Some(b) => b,
        None => sqlx::query_scalar("SELECT branch_id FROM branch_tables WHERE id = $1")
            .bind(table_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound("Table not found".into()))?,
    };

    let mut tx = pool.get_ref().begin().await?;
    if !lock_table(&mut tx, table_id, branch_id).await? {
        return Err(AppError::NotFound("Table not found".into()));
    }
    // Only a `party` row is ours to end. A ticket that landed in the meantime
    // owns the table and freeing it would strand the bill; a table with no
    // live row is already released (and `dirty` stays dirty -- only a person
    // clears that). Neither is an error: the hold is gone either way, which
    // is all the caller was telling us.
    let hand = Hand::of(&mut *tx, actor.teller_id, branch_id).await?;
    let released = release_party_hold(&mut tx, table_id, bus, &hand).await?;
    tx.commit().await?;

    if let Some(hub) = hub
        && released
    {
        let mut events = FloorEvents::default();
        events.tables.push(table_id);
        events.publish(pool.get_ref(), hub, branch_id).await;
    }
    Ok(HttpResponse::Ok().json(serde_json::json!({ "ok": true })))
}

// ── Transfer waitlist ────────────────────────────────────────────────────────

#[utoipa::path(get, path = "/floor/transfers", tag = "floor_transfers", params(ListTransfersQuery),
    responses((status = 200, body = TransfersSyncResponse), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_floor_transfers(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<ListTransfersQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "table_transfers", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    let server_time = Utc::now();
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM table_transfer_requests \
         WHERE branch_id = $1 \
           AND (($2::timestamptz IS NULL AND status = 'waiting') \
                OR ($2 IS NOT NULL AND updated_at > $2)) \
         ORDER BY created_at LIMIT 500",
    )
    .bind(query.branch_id)
    .bind(query.since)
    .fetch_all(pool.get_ref())
    .await?;
    let mut transfers = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(v) = transfer_view(pool.get_ref(), id).await? {
            transfers.push(v);
        }
    }
    Ok(HttpResponse::Ok().json(TransfersSyncResponse {
        server_time,
        transfers,
    }))
}

#[utoipa::path(post, path = "/floor/transfers", tag = "floor_transfers",
    request_body = CreateFloorTransferRequest,
    responses((status = 200, body = TransferView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn create_floor_transfer(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    body: web::Json<CreateFloorTransferRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "table_transfers", "create").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    create_transfer_inner(
        pool,
        body,
        ActingContext::live(&claims)?,
        Some(hub.get_ref()),
    )
    .await
}

/// Create core. The occupant must be LIVE in this branch (a parked/resumed held
/// order or an open/ready ticket — a no-table "outside" order queues too, with
/// `from_table_id: null`). One waiting wish per party; a retried create with
/// the same id dedups to the stored row.
pub(crate) async fn create_transfer_inner(
    pool: crate::db::Db,
    body: web::Json<CreateFloorTransferRequest>,
    actor: ActingContext,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    // Idempotent retry: the id already landed → return it as stored.
    if let Some(existing) = transfer_view(pool.get_ref(), body.id).await? {
        return Ok(HttpResponse::Ok().json(existing));
    }
    if body.target_section_id.is_none() && body.target_table_id.is_none() {
        return Err(AppError::BadRequest(
            "A transfer needs a target section or table".into(),
        ));
    }
    if let Some(note) = &body.note
        && note.chars().count() > 500
    {
        return Err(AppError::BadRequest("Note is too long".into()));
    }
    let org_id = branch_org(pool.get_ref(), body.branch_id).await?;

    // Validate the wish against this branch's layout.
    if let Some(s) = body.target_section_id {
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM floor_sections WHERE id = $1 AND branch_id = $2)",
        )
        .bind(s)
        .bind(body.branch_id)
        .fetch_one(pool.get_ref())
        .await?;
        if !ok {
            return Err(AppError::BadRequest(
                "Target section is not in this branch".into(),
            ));
        }
    }
    if let Some(t) = body.target_table_id {
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM branch_tables WHERE id = $1 AND branch_id = $2)",
        )
        .bind(t)
        .bind(body.branch_id)
        .fetch_one(pool.get_ref())
        .await?;
        if !ok {
            return Err(AppError::BadRequest(
                "Target table is not in this branch".into(),
            ));
        }
    }

    // The occupant must be live here; its CURRENT table (per the ledger)
    // becomes `from_table_id`.
    let from_table: Option<Uuid> = sqlx::query_scalar(
        "SELECT (SELECT o.table_id FROM table_occupancies o \
                  WHERE o.open_ticket_id = t.id AND o.ended_at IS NULL) \
           FROM open_tickets t \
          WHERE t.id = $1 AND t.branch_id = $2 AND t.status = 'open'",
    )
    .bind(body.occupant_id)
    .bind(body.branch_id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Order not found".into()))?;

    let waiting: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM table_transfer_requests \
         WHERE occupant_kind = $1 AND occupant_id = $2 AND status = 'waiting'",
    )
    .bind(&body.occupant_kind)
    .bind(body.occupant_id)
    .fetch_optional(pool.get_ref())
    .await?;
    if waiting.is_some() {
        return Err(refused(
            refusal::TRANSFER_EXISTS,
            "This party already has a waiting transfer",
        ));
    }

    sqlx::query(
        "INSERT INTO table_transfer_requests \
            (id, org_id, branch_id, occupant_kind, occupant_id, from_table_id, \
             target_section_id, target_table_id, note, requested_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(body.id)
    .bind(org_id)
    .bind(body.branch_id)
    .bind(&body.occupant_kind)
    .bind(body.occupant_id)
    .bind(from_table)
    .bind(body.target_section_id)
    .bind(body.target_table_id)
    .bind(&body.note)
    .bind(actor.teller_id)
    .execute(pool.get_ref())
    .await?;

    if let Some(hub) = hub {
        let mut events = FloorEvents::default();
        events.transfers.push(body.id);
        events.publish(pool.get_ref(), hub, body.branch_id).await;
    }
    let view = transfer_view(pool.get_ref(), body.id)
        .await?
        .ok_or(AppError::Internal)?;
    Ok(HttpResponse::Ok().json(view))
}

#[utoipa::path(post, path = "/floor/transfers/{id}/cancel", tag = "floor_transfers",
    params(("id" = Uuid, Path, description = "Transfer request ID")),
    responses((status = 200, body = TransferView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn cancel_transfer(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "table_transfers", "update").await?;
    require_transfer_branch_access(pool.get_ref(), &claims, *id).await?;
    cancel_transfer_inner(pool, *id, Some(hub.get_ref())).await
}

pub(crate) async fn cancel_transfer_inner(
    pool: crate::db::Db,
    id: Uuid,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    let row: Option<(Uuid, String)> =
        sqlx::query_as("SELECT branch_id, status FROM table_transfer_requests WHERE id = $1")
            .bind(id)
            .fetch_optional(pool.get_ref())
            .await?;
    let Some((branch_id, status)) = row else {
        return Err(AppError::NotFound("Transfer request not found".into()));
    };
    match status.as_str() {
        "cancelled" => {} // idempotent
        "fulfilled" => {
            return Err(refused(
                refusal::TRANSFER_FULFILLED,
                "Transfer is already fulfilled",
            ));
        }
        _ => {
            sqlx::query(
                "UPDATE table_transfer_requests \
                 SET status = 'cancelled', resolved_at = now(), updated_at = now() \
                 WHERE id = $1 AND status = 'waiting'",
            )
            .bind(id)
            .execute(pool.get_ref())
            .await?;
            if let Some(hub) = hub {
                let mut events = FloorEvents::default();
                events.transfers.push(id);
                events.publish(pool.get_ref(), hub, branch_id).await;
            }
        }
    }
    let view = transfer_view(pool.get_ref(), id)
        .await?
        .ok_or(AppError::Internal)?;
    Ok(HttpResponse::Ok().json(view))
}

#[utoipa::path(post, path = "/floor/transfers/{id}/fulfill", tag = "floor_transfers",
    params(("id" = Uuid, Path, description = "Transfer request ID")),
    request_body = FulfillTransferRequest,
    responses((status = 200, body = TransferView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn fulfill_transfer(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<FulfillTransferRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "table_transfers", "update").await?;
    require_transfer_branch_access(pool.get_ref(), &claims, *id).await?;
    fulfill_transfer_inner(
        pool,
        *id,
        body,
        ActingContext::live(&claims)?,
        Some(hub.get_ref()),
    )
    .await
}

/// Fulfill core: seat the waiting party on `table_id` — which must satisfy the
/// wish (the exact wished table, or any table in the wished section) and be
/// free — through the same arbitration as every other move.
pub(crate) async fn fulfill_transfer_inner(
    pool: crate::db::Db,
    id: Uuid,
    body: web::Json<FulfillTransferRequest>,
    actor: ActingContext,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    let mut events = FloorEvents::default();
    let mut tx = pool.get_ref().begin().await?;
    #[allow(clippy::type_complexity)]
    let row: Option<(Uuid, String, Uuid, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "SELECT branch_id, status, occupant_id, target_section_id, target_table_id \
         FROM table_transfer_requests WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((branch_id, status, occupant_id, target_section, target_table)) = row else {
        return Err(AppError::NotFound("Transfer request not found".into()));
    };
    match status.as_str() {
        "fulfilled" => {
            tx.commit().await?; // replayed fulfill — idempotent
            let view = transfer_view(pool.get_ref(), id)
                .await?
                .ok_or(AppError::Internal)?;
            return Ok(HttpResponse::Ok().json(view));
        }
        "cancelled" => {
            return Err(refused(
                refusal::TRANSFER_CANCELLED,
                "Transfer is already cancelled",
            ));
        }
        _ => {}
    }

    // The chosen table must satisfy the wish.
    if let Some(t) = target_table
        && t != body.table_id
    {
        return Err(AppError::BadRequest(
            "The party asked for a different table".into(),
        ));
    }
    if !lock_table(&mut tx, body.table_id, branch_id).await? {
        return Err(AppError::BadRequest("Table is not in this branch".into()));
    }
    if target_table.is_none()
        && let Some(section) = target_section
    {
        let in_section: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM branch_tables WHERE id = $1 AND section_id = $2)",
        )
        .bind(body.table_id)
        .bind(section)
        .fetch_one(&mut *tx)
        .await?;
        if !in_section {
            return Err(AppError::BadRequest(
                "Table is not in the section the party asked for".into(),
            ));
        }
    }

    // The ticket must still be live.
    let live: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM open_tickets WHERE id = $1 AND status = 'open'")
            .bind(occupant_id)
            .fetch_optional(&mut *tx)
            .await?;
    if live.is_none() {
        return Err(refused(
            refusal::TICKET_NOT_LIVE,
            "The party's order is no longer live",
        ));
    }
    check_permission_for(
        pool.get_ref(),
        actor.teller_id,
        &actor.role,
        "open_tickets",
        "update",
    )
    .await?;

    if occupant_of(&mut tx, body.table_id, Some(occupant_id))
        .await?
        .is_some()
    {
        return Err(refused(
            refusal::TABLE_OCCUPIED,
            "Table is already occupied",
        ));
    }

    // The old table (if any) frees up; the party lands on the new one.
    let old_table: Option<Uuid> = sqlx::query_scalar(
        "SELECT table_id FROM table_occupancies WHERE open_ticket_id = $1 AND ended_at IS NULL",
    )
    .bind(occupant_id)
    .fetch_optional(&mut *tx)
    .await?;
    let hand = Hand::of(&mut *tx, actor.teller_id, branch_id).await?;
    move_ticket(
        &mut tx,
        occupant_id,
        Some(body.table_id),
        &hand,
        &mut events,
    )
    .await?;
    if let Some(old) = old_table
        && old != body.table_id
    {
        events.tables.push(old);
    }
    events.tables.push(body.table_id);

    // `autofulfill_transfers` inside `relocate_ticket` resolves this request when
    // the wish matches; a section wish landing on a table WITHOUT a section
    // (edge: table moved out of the section since) still needs the explicit stamp.
    sqlx::query(
        "UPDATE table_transfer_requests \
         SET status = 'fulfilled', fulfilled_table_id = $2, resolved_at = now(), updated_at = now() \
         WHERE id = $1 AND status = 'waiting'",
    )
    .bind(id)
    .bind(body.table_id)
    .execute(&mut *tx)
    .await?;
    events.transfers.push(id);
    tx.commit().await?;

    if let Some(hub) = hub {
        events.publish(pool.get_ref(), hub, branch_id).await;
    }
    let view = transfer_view(pool.get_ref(), id)
        .await?
        .ok_or(AppError::Internal)?;
    Ok(HttpResponse::Ok().json(view))
}

// ── a table's own history ────────────────────────────────────────────────────

/// One sitting at a table: the bill that was opened on it and what it came to.
#[derive(Debug, Serialize, ToSchema)]
pub struct TableSitting {
    pub open_ticket_id: Uuid,
    pub ticket_ref: Option<String>,
    /// When the party's bill was opened — the closest thing the server has to
    /// when they sat down.
    pub opened_at: DateTime<Utc>,
    /// When the party sat down: the seat hold's stamp when they were seated
    /// before ordering, else the bill's opening.
    pub seated_at: DateTime<Utc>,
    /// When the bill was settled or voided; `None` while it is still open.
    pub closed_at: Option<DateTime<Utc>>,
    /// Minutes from `seated_at` to the close, or to now while still open.
    pub minutes: i64,
    pub status: String,
    pub customer_name: Option<String>,
    pub guest_count: Option<i32>,
    /// The settled sale, when the bill became one.
    pub order_id: Option<Uuid>,
    pub order_number: Option<i32>,
    /// What the sale came to, in minor units. `None` for an unsettled or
    /// voided bill — a table's takings only count money that was taken.
    pub total_amount: Option<i32>,
}

/// What a table has done over the window asked for.
#[derive(Debug, Serialize, ToSchema)]
pub struct TableHistory {
    pub table_id: Uuid,
    pub label: String,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    /// Bills opened on this table in the window, newest first.
    pub sittings: Vec<TableSitting>,
    /// Settled bills only.
    pub covers: i64,
    pub settled_count: i64,
    pub total_minor: i64,
    /// Mean spend per settled bill, minor units.
    pub average_bill_minor: i64,
    /// Mean minutes a party occupied the table, over settled bills — the
    /// number that says whether a table turns.
    pub average_minutes: i64,
    /// Settled bills per day over the window, x100 so the wire stays integer.
    pub turns_per_day_x100: i64,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct TableHistoryQuery {
    /// Inclusive lower bound; defaults to 30 days back.
    pub from: Option<DateTime<Utc>>,
    /// Exclusive upper bound; defaults to now.
    pub to: Option<DateTime<Utc>>,
}

/// A table's history and what it earns.
///
/// The link was always there and nothing ever read it: a settled bill carries
/// `orders.open_ticket_id`, and the ticket carries `table_id`. So a table's
/// takings are one join away, and until now a shop could see a room full of
/// tables and not answer "which of these actually earns".
///
/// Covers and money count SETTLED bills only. An open bill is still running
/// and a voided one took nothing — folding either into the averages would
/// flatter a table that lost money.
#[utoipa::path(
    get,
    path = "/floor/tables/{id}/history",
    params(("id" = Uuid, Path, description = "Table id"), TableHistoryQuery),
    responses((status = 200, body = TableHistory)),
    tag = "floor"
)]
pub async fn table_history(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    q: web::Query<TableHistoryQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "open_tickets", "read").await?;

    let to = q.to.unwrap_or_else(Utc::now);
    let from = q.from.unwrap_or_else(|| to - chrono::Duration::days(30));
    if from >= to {
        return Err(AppError::BadRequest(
            "`from` must be before `to`".to_string(),
        ));
    }

    let table: Option<(Uuid, String, Uuid)> =
        sqlx::query_as("SELECT id, label, branch_id FROM branch_tables WHERE id = $1")
            .bind(*id)
            .fetch_optional(pool.get_ref())
            .await?;
    let Some((table_id, label, branch_id)) = table else {
        return Err(AppError::NotFound("table not found".to_string()));
    };
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;

    let rows: Vec<(
        Uuid,
        Option<String>,
        DateTime<Utc>,
        Option<DateTime<Utc>>,
        String,
        Option<String>,
        Option<i32>,
        Option<Uuid>,
        Option<i32>,
        Option<i32>,
        DateTime<Utc>,
    )> = sqlx::query_as(
        "SELECT t.id, t.ticket_ref, t.opened_at, COALESCE(t.settled_at, t.voided_at), \
                t.status::text, \
                t.customer_name, t.guest_count, o.id, o.order_number, \
                CASE WHEN o.voided_at IS NULL THEN o.total_amount ELSE NULL END, \
                LEAST(COALESCE(o.seated_at, t.seated_at, t.opened_at), t.opened_at) \
           FROM open_tickets t \
           LEFT JOIN orders o ON o.open_ticket_id = t.id \
          WHERE t.table_id = $1 AND t.opened_at >= $2 AND t.opened_at < $3 \
          ORDER BY t.opened_at DESC",
    )
    .bind(table_id)
    .bind(from)
    .bind(to)
    .fetch_all(pool.get_ref())
    .await?;

    let now = Utc::now();
    let sittings: Vec<TableSitting> = rows
        .into_iter()
        .map(|r| {
            let closed = r.3;
            let minutes = (closed.unwrap_or(now) - r.10).num_minutes().max(0);
            TableSitting {
                open_ticket_id: r.0,
                ticket_ref: r.1,
                opened_at: r.2,
                seated_at: r.10,
                closed_at: closed,
                minutes,
                status: r.4,
                customer_name: r.5,
                guest_count: r.6,
                order_id: r.7,
                order_number: r.8,
                total_amount: r.9,
            }
        })
        .collect();

    // Settled only: an open bill has not finished and a voided one took
    // nothing, so neither belongs in an average that answers "what does this
    // table earn".
    let settled: Vec<&TableSitting> = sittings
        .iter()
        .filter(|s| s.total_amount.is_some())
        .collect();
    let settled_count = settled.len() as i64;
    let total_minor: i64 = settled
        .iter()
        .map(|s| i64::from(s.total_amount.unwrap_or(0)))
        .sum();
    let covers: i64 = settled
        .iter()
        .map(|s| i64::from(s.guest_count.unwrap_or(0)))
        .sum();
    let minutes_sum: i64 = settled.iter().map(|s| s.minutes).sum();
    let days = ((to - from).num_minutes() as f64 / (24.0 * 60.0)).max(1.0 / 24.0);

    Ok(HttpResponse::Ok().json(TableHistory {
        table_id,
        label,
        from,
        to,
        covers,
        settled_count,
        total_minor,
        average_bill_minor: if settled_count == 0 {
            0
        } else {
            total_minor / settled_count
        },
        average_minutes: if settled_count == 0 {
            0
        } else {
            minutes_sum / settled_count
        },
        turns_per_day_x100: ((settled_count as f64 / days) * 100.0).round() as i64,
        sittings,
    }))
}
