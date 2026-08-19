//! Held-order endpoints: the sync list, park (offline-first upsert), the
//! resume claim/release pair, discard/complete tombstones, table assignment,
//! the atomic cross-entity table swap, and the transfer waitlist.
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
    FloorEvents, HeldOrderParkResponse, HeldOrderView, HeldOrdersSyncResponse, Occupant,
    TransferView, TransfersSyncResponse, autofulfill_transfers, bus_table,
    cancel_waiting_transfers, extract_claims, free_table, held_order_view, lock_table, occupant_of,
    require_branch_access, seat_table, transfer_view,
};
use crate::errors::{AppError, AppErrorResponse};
use crate::permissions::checker::{check_permission, check_permission_for};
use crate::realtime::hub::BranchEventHub;
use crate::sync::ActingContext;

/// Parked carts are small; anything bigger than this is a client bug, not a
/// cart. Guards the opaque jsonb column against abuse.
const MAX_CART_BYTES: usize = 512 * 1024;

// ── Requests ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, Serialize, ToSchema)]
pub struct ParkHeldOrderRequest {
    /// Client-minted id — the held order's identity across parks/resumes/devices.
    pub id: Uuid,
    pub branch_id: Uuid,
    #[serde(default)]
    pub name: String,
    /// Opaque client cart payload, stored and returned verbatim.
    pub cart: serde_json::Value,
    /// Requested table. On conflict the park still succeeds WITHOUT the table
    /// (`table_conflict: true` in the response) — a queued offline park must
    /// never dead-letter over a table race.
    #[serde(default)]
    pub table_id: Option<Uuid>,
    /// The parking device's installation id (also the claim key on resume).
    #[serde(default)]
    pub device_id: Option<String>,
    /// Original creation instant (strip ordering); defaults to now.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct UpdateHeldOrderRequest {
    #[serde(default)]
    pub name: Option<String>,
    /// Optimistic-concurrency fence: reject (409) if the server has moved past
    /// this revision. Omit to last-write-wins.
    #[serde(default)]
    pub base_revision: Option<i64>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct ClaimHeldOrderRequest {
    /// The resuming device — recorded as the claim holder.
    pub device_id: String,
    /// Steal a claim held by another device (that till died mid-edit).
    #[serde(default)]
    pub force: bool,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct ReleaseHeldOrderRequest {
    pub device_id: String,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct DiscardHeldOrderRequest {
    #[serde(default)]
    pub device_id: Option<String>,
    /// Discard even while another device holds the resume claim.
    #[serde(default)]
    pub force: bool,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct CompleteHeldOrderRequest {
    /// The paid order this cart became (linked for the audit trail).
    #[serde(default)]
    pub order_id: Option<Uuid>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct AssignTableRequest {
    /// The table to seat this held order on; `null` releases the current table.
    #[serde(default)]
    pub table_id: Option<Uuid>,
}

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
pub struct UpdateTableStateRequest {
    /// `free` | `held` | `seated` | `dirty`.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub section_id: Option<Uuid>,
    #[serde(default)]
    pub clear_section: bool,
}

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
pub struct ListHeldOrdersQuery {
    pub branch_id: Uuid,
    /// Sync cursor: return everything updated after this instant, INCLUDING
    /// completed/discarded tombstones. Omit for the live board (held+resumed).
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
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

/// Resolve a held order's branch and enforce the LIVE caller's access to it.
async fn require_held_order_branch_access(
    pool: &sqlx::PgPool,
    claims: &crate::auth::jwt::Claims,
    id: Uuid,
) -> Result<Uuid, AppError> {
    let branch_id: Option<Uuid> =
        sqlx::query_scalar("SELECT branch_id FROM held_orders WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    let branch_id = branch_id.ok_or_else(|| AppError::NotFound("Held order not found".into()))?;
    require_branch_access(pool, claims, branch_id).await?;
    Ok(branch_id)
}

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

fn guard_cart(cart: &serde_json::Value) -> Result<(), AppError> {
    let bytes = serde_json::to_string(cart).map(|s| s.len()).unwrap_or(0);
    if bytes > MAX_CART_BYTES {
        return Err(AppError::BadRequest("Cart payload is too large".into()));
    }
    Ok(())
}

/// The permission resource guarding a move of this occupant kind.
fn occupant_move_resource(o: &Occupant) -> &'static str {
    match o {
        Occupant::HeldOrder(_) => "held_orders",
        Occupant::OpenTicket(_) => "open_tickets",
    }
}

/// Move one occupant onto `to_table` (or off any table when `None`) inside the
/// caller's transaction. Handles the held-order unique index by clearing first
/// when asked (`clear_only`), bumps revisions, and records the entity event.
async fn move_occupant(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    occupant: Occupant,
    to_table: Option<Uuid>,
    events: &mut FloorEvents,
) -> Result<(), AppError> {
    match occupant {
        Occupant::HeldOrder(id) => {
            sqlx::query(
                "UPDATE held_orders SET table_id = $2, revision = revision + 1, updated_at = now() \
                 WHERE id = $1",
            )
            .bind(id)
            .bind(to_table)
            .execute(&mut **tx)
            .await?;
            events.held_orders.push(id);
        }
        Occupant::OpenTicket(id) => {
            sqlx::query("UPDATE open_tickets SET table_id = $2, updated_at = now() WHERE id = $1")
                .bind(id)
                .bind(to_table)
                .execute(&mut **tx)
                .await?;
            events.tickets.push(id);
        }
    }
    if let Some(t) = to_table {
        events
            .transfers
            .extend(autofulfill_transfers(tx, occupant.kind(), occupant.id(), t).await?);
    }
    Ok(())
}

// ── List (sync pull) ─────────────────────────────────────────────────────────

#[utoipa::path(get, path = "/held-orders", tag = "held_orders", params(ListHeldOrdersQuery),
    responses((status = 200, body = HeldOrdersSyncResponse), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn list_held_orders(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<ListHeldOrdersQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "held_orders", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    let server_time = Utc::now();
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM held_orders \
         WHERE branch_id = $1 \
           AND (($2::timestamptz IS NULL AND status IN ('held','resumed')) \
                OR ($2 IS NOT NULL AND updated_at > $2)) \
         ORDER BY created_at LIMIT 500",
    )
    .bind(query.branch_id)
    .bind(query.since)
    .fetch_all(pool.get_ref())
    .await?;
    let mut held_orders = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(v) = held_order_view(pool.get_ref(), id).await? {
            held_orders.push(v);
        }
    }
    Ok(HttpResponse::Ok().json(HeldOrdersSyncResponse {
        server_time,
        held_orders,
    }))
}

// ── Park (offline-first upsert) ──────────────────────────────────────────────

#[utoipa::path(post, path = "/held-orders", tag = "held_orders", request_body = ParkHeldOrderRequest,
    responses((status = 200, body = HeldOrderParkResponse), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn park_held_order(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    body: web::Json<ParkHeldOrderRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "held_orders", "create").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    park_held_order_inner(
        pool,
        body,
        ActingContext::live(&claims)?,
        Some(hub.get_ref()),
    )
    .await
}

/// Park core: create, or re-park/update from the device holding the claim.
/// Data always wins over position — a requested table that's taken (or unknown
/// to this branch) is dropped with `table_conflict: true`, never a failure.
pub(crate) async fn park_held_order_inner(
    pool: crate::db::Db,
    body: web::Json<ParkHeldOrderRequest>,
    actor: ActingContext,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    guard_cart(&body.cart)?;
    let org_id = branch_org(pool.get_ref(), body.branch_id).await?;

    let mut events = FloorEvents::default();
    let mut table_conflict = false;
    let mut tx = pool.get_ref().begin().await?;

    #[allow(clippy::type_complexity)]
    let existing: Option<(Uuid, String, Option<Uuid>, Option<String>)> = sqlx::query_as(
        "SELECT branch_id, status, table_id, claimed_by_device FROM held_orders \
         WHERE id = $1 FOR UPDATE",
    )
    .bind(body.id)
    .fetch_optional(&mut *tx)
    .await?;

    // Resolve the requested table under the per-table lock. `exclude` keeps a
    // re-park on the order's OWN table from reading itself as the occupant.
    let desired = body.table_id;
    let resolved_table = match desired {
        Some(t) => {
            if !lock_table(&mut tx, t, body.branch_id).await? {
                table_conflict = true; // stale/foreign layout id — drop it
                None
            } else if occupant_of(&mut tx, t, Some(body.id), None)
                .await?
                .is_some()
            {
                table_conflict = true;
                None
            } else {
                Some(t)
            }
        }
        None => None,
    };

    match existing {
        None => {
            sqlx::query(
                "INSERT INTO held_orders \
                    (id, org_id, branch_id, table_id, name, cart, status, created_by, device_id, \
                     created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, 'held', $7, $8, COALESCE($9, now()))",
            )
            .bind(body.id)
            .bind(org_id)
            .bind(body.branch_id)
            .bind(resolved_table)
            .bind(&body.name)
            .bind(&body.cart)
            .bind(actor.teller_id)
            .bind(&body.device_id)
            .bind(body.created_at)
            .execute(&mut *tx)
            .await?;
        }
        Some((existing_branch, status, old_table, claimed_by)) => {
            if existing_branch != body.branch_id {
                return Err(AppError::Conflict(
                    "Held order belongs to another branch".into(),
                ));
            }
            match status.as_str() {
                "completed" | "discarded" => {
                    return Err(AppError::Conflict(format!(
                        "Held order is already {status}"
                    )));
                }
                "resumed" if claimed_by.is_some() && claimed_by != body.device_id => {
                    return Err(AppError::Conflict(
                        "Held order is being edited on another till".into(),
                    ));
                }
                _ => {}
            }
            sqlx::query(
                "UPDATE held_orders SET name = $2, cart = $3, table_id = $4, device_id = $5, \
                     status = 'held', claimed_by_device = NULL, claimed_at = NULL, \
                     revision = revision + 1, updated_at = now() \
                 WHERE id = $1",
            )
            .bind(body.id)
            .bind(&body.name)
            .bind(&body.cart)
            .bind(resolved_table)
            .bind(&body.device_id)
            .execute(&mut *tx)
            .await?;
            // Freed the old table (moved or unassigned) → bus it.
            if let Some(old) = old_table
                && resolved_table != Some(old)
            {
                free_table(&mut tx, old).await?;
                events.tables.push(old);
            }
        }
    }

    if let Some(t) = resolved_table {
        seat_table(&mut tx, t).await?;
        events.tables.push(t);
        events
            .transfers
            .extend(autofulfill_transfers(&mut tx, "held_order", body.id, t).await?);
    }
    events.held_orders.push(body.id);
    tx.commit().await?;

    if let Some(hub) = hub {
        events.publish(pool.get_ref(), hub, body.branch_id).await;
    }
    let held_order = held_order_view(pool.get_ref(), body.id)
        .await?
        .ok_or(AppError::Internal)?;
    Ok(HttpResponse::Ok().json(HeldOrderParkResponse {
        held_order,
        table_conflict,
    }))
}

// ── Rename (live-only; the POS re-parks to edit) ─────────────────────────────

#[utoipa::path(patch, path = "/held-orders/{id}", tag = "held_orders",
    params(("id" = Uuid, Path, description = "Held order ID")),
    request_body = UpdateHeldOrderRequest,
    responses((status = 200, body = HeldOrderView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn update_held_order(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<UpdateHeldOrderRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "held_orders", "update").await?;
    let branch_id = require_held_order_branch_access(pool.get_ref(), &claims, *id).await?;

    if let Some(name) = &body.name {
        if name.chars().count() > 120 {
            return Err(AppError::BadRequest("Name is too long".into()));
        }
        let updated = sqlx::query(
            "UPDATE held_orders SET name = $2, revision = revision + 1, updated_at = now() \
             WHERE id = $1 AND status IN ('held','resumed') \
               AND ($3::bigint IS NULL OR revision = $3)",
        )
        .bind(*id)
        .bind(name)
        .bind(body.base_revision)
        .execute(pool.get_ref())
        .await?;
        if updated.rows_affected() == 0 {
            return Err(AppError::Conflict(
                "Held order changed on another till (or is no longer live)".into(),
            ));
        }
        let mut events = FloorEvents::default();
        events.held_orders.push(*id);
        events
            .publish(pool.get_ref(), hub.get_ref(), branch_id)
            .await;
    }
    let view = held_order_view(pool.get_ref(), *id)
        .await?
        .ok_or_else(|| AppError::NotFound("Held order not found".into()))?;
    Ok(HttpResponse::Ok().json(view))
}

// ── Claim / release (the cross-till resume lease) ────────────────────────────

#[utoipa::path(post, path = "/held-orders/{id}/claim", tag = "held_orders",
    params(("id" = Uuid, Path, description = "Held order ID")),
    request_body = ClaimHeldOrderRequest,
    responses((status = 200, body = HeldOrderView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn claim_held_order(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<ClaimHeldOrderRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "held_orders", "update").await?;
    require_held_order_branch_access(pool.get_ref(), &claims, *id).await?;
    claim_held_order_inner(pool, *id, body, Some(hub.get_ref())).await
}

/// Claim core: `held → resumed` under this device. Re-claiming your own claim
/// is idempotent; someone else's claim is a 409 unless `force` (till died).
pub(crate) async fn claim_held_order_inner(
    pool: crate::db::Db,
    id: Uuid,
    body: web::Json<ClaimHeldOrderRequest>,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    let mut tx = pool.get_ref().begin().await?;
    let row: Option<(Uuid, String, Option<String>)> = sqlx::query_as(
        "SELECT branch_id, status, claimed_by_device FROM held_orders WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((branch_id, status, claimed_by)) = row else {
        return Err(AppError::NotFound("Held order not found".into()));
    };
    match status.as_str() {
        "completed" | "discarded" => {
            return Err(AppError::Conflict(format!(
                "Held order is already {status}"
            )));
        }
        "resumed" if claimed_by.as_deref() == Some(body.device_id.as_str()) => {
            tx.commit().await?; // idempotent re-claim, nothing changed
            let view = held_order_view(pool.get_ref(), id)
                .await?
                .ok_or(AppError::Internal)?;
            return Ok(HttpResponse::Ok().json(view));
        }
        "resumed" if !body.force => {
            return Err(AppError::Conflict(
                "Held order is being edited on another till".into(),
            ));
        }
        _ => {}
    }
    sqlx::query(
        "UPDATE held_orders SET status = 'resumed', claimed_by_device = $2, claimed_at = now(), \
             revision = revision + 1, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(&body.device_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    if let Some(hub) = hub {
        let mut events = FloorEvents::default();
        events.held_orders.push(id);
        events.publish(pool.get_ref(), hub, branch_id).await;
    }
    let view = held_order_view(pool.get_ref(), id)
        .await?
        .ok_or(AppError::Internal)?;
    Ok(HttpResponse::Ok().json(view))
}

#[utoipa::path(post, path = "/held-orders/{id}/release", tag = "held_orders",
    params(("id" = Uuid, Path, description = "Held order ID")),
    request_body = ReleaseHeldOrderRequest,
    responses((status = 200, body = HeldOrderView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn release_held_order(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<ReleaseHeldOrderRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "held_orders", "update").await?;
    require_held_order_branch_access(pool.get_ref(), &claims, *id).await?;
    release_held_order_inner(pool, *id, body, Some(hub.get_ref())).await
}

/// Release core: give the claim back (`resumed → held`) WITHOUT changing the
/// cart — the "never mind" path out of a resume. Only the claim holder may.
pub(crate) async fn release_held_order_inner(
    pool: crate::db::Db,
    id: Uuid,
    body: web::Json<ReleaseHeldOrderRequest>,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    let mut tx = pool.get_ref().begin().await?;
    let row: Option<(Uuid, String, Option<String>)> = sqlx::query_as(
        "SELECT branch_id, status, claimed_by_device FROM held_orders WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((branch_id, status, claimed_by)) = row else {
        return Err(AppError::NotFound("Held order not found".into()));
    };
    match status.as_str() {
        "held" => {
            tx.commit().await?; // already released — idempotent
            let view = held_order_view(pool.get_ref(), id)
                .await?
                .ok_or(AppError::Internal)?;
            return Ok(HttpResponse::Ok().json(view));
        }
        "completed" | "discarded" => {
            return Err(AppError::Conflict(format!(
                "Held order is already {status}"
            )));
        }
        _ if claimed_by.as_deref() != Some(body.device_id.as_str()) => {
            return Err(AppError::Conflict("Claim is held by another till".into()));
        }
        _ => {}
    }
    sqlx::query(
        "UPDATE held_orders SET status = 'held', claimed_by_device = NULL, claimed_at = NULL, \
             revision = revision + 1, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    if let Some(hub) = hub {
        let mut events = FloorEvents::default();
        events.held_orders.push(id);
        events.publish(pool.get_ref(), hub, branch_id).await;
    }
    let view = held_order_view(pool.get_ref(), id)
        .await?
        .ok_or(AppError::Internal)?;
    Ok(HttpResponse::Ok().json(view))
}

// ── Discard / complete (tombstones; both free the table) ─────────────────────

#[utoipa::path(post, path = "/held-orders/{id}/discard", tag = "held_orders",
    params(("id" = Uuid, Path, description = "Held order ID")),
    request_body = DiscardHeldOrderRequest,
    responses((status = 200, body = HeldOrderView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn discard_held_order(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<DiscardHeldOrderRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "held_orders", "update").await?;
    require_held_order_branch_access(pool.get_ref(), &claims, *id).await?;
    discard_held_order_inner(pool, *id, body, Some(hub.get_ref())).await
}

pub(crate) async fn discard_held_order_inner(
    pool: crate::db::Db,
    id: Uuid,
    body: web::Json<DiscardHeldOrderRequest>,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    terminate_held_order(
        pool,
        id,
        Termination::Discard {
            device_id: body.device_id.clone(),
            force: body.force,
        },
        hub,
    )
    .await
}

#[utoipa::path(post, path = "/held-orders/{id}/complete", tag = "held_orders",
    params(("id" = Uuid, Path, description = "Held order ID")),
    request_body = CompleteHeldOrderRequest,
    responses((status = 200, body = HeldOrderView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn complete_held_order(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<CompleteHeldOrderRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "held_orders", "update").await?;
    require_held_order_branch_access(pool.get_ref(), &claims, *id).await?;
    complete_held_order_inner(pool, *id, body, Some(hub.get_ref())).await
}

pub(crate) async fn complete_held_order_inner(
    pool: crate::db::Db,
    id: Uuid,
    body: web::Json<CompleteHeldOrderRequest>,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    terminate_held_order(
        pool,
        id,
        Termination::Complete {
            order_id: body.order_id,
        },
        hub,
    )
    .await
}

enum Termination {
    Discard {
        device_id: Option<String>,
        force: bool,
    },
    Complete {
        order_id: Option<Uuid>,
    },
}

/// Shared tombstone walk: flip to the terminal status, bus the freed table,
/// cancel the party's waiting transfer wish. Idempotent on its own terminal
/// state; the OTHER terminal state is a conflict (a completed sale can't be
/// discarded, and vice versa).
async fn terminate_held_order(
    pool: crate::db::Db,
    id: Uuid,
    how: Termination,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    let (target, conflict): (&str, &str) = match how {
        Termination::Discard { .. } => ("discarded", "completed"),
        Termination::Complete { .. } => ("completed", "discarded"),
    };
    let mut events = FloorEvents::default();
    let mut tx = pool.get_ref().begin().await?;
    #[allow(clippy::type_complexity)]
    let row: Option<(Uuid, String, Option<Uuid>, Option<String>)> = sqlx::query_as(
        "SELECT branch_id, status, table_id, claimed_by_device FROM held_orders \
         WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((branch_id, status, table_id, claimed_by)) = row else {
        return Err(AppError::NotFound("Held order not found".into()));
    };
    if status == target {
        tx.commit().await?; // replayed terminal op — idempotent
        let view = held_order_view(pool.get_ref(), id)
            .await?
            .ok_or(AppError::Internal)?;
        return Ok(HttpResponse::Ok().json(view));
    }
    if status == conflict {
        return Err(AppError::Conflict(format!(
            "Held order is already {conflict}"
        )));
    }
    if let Termination::Discard { device_id, force } = &how
        && status == "resumed"
        && !force
        && claimed_by.is_some()
        && claimed_by.as_deref() != device_id.as_deref()
    {
        return Err(AppError::Conflict(
            "Held order is being edited on another till".into(),
        ));
    }

    let order_id = match &how {
        Termination::Complete { order_id } => *order_id,
        Termination::Discard { .. } => None,
    };
    sqlx::query(
        "UPDATE held_orders SET status = $2, order_id = COALESCE($3, order_id), table_id = NULL, \
             claimed_by_device = NULL, claimed_at = NULL, revision = revision + 1, \
             updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(target)
    .bind(order_id)
    .execute(&mut *tx)
    .await?;
    if let Some(t) = table_id {
        // A CHECKOUT leaves the table needing a bus; a discard never seated a
        // party in the first place, so it goes straight back to the room.
        match &how {
            Termination::Complete { .. } => bus_table(&mut tx, t).await?,
            Termination::Discard { .. } => free_table(&mut tx, t).await?,
        }
        events.tables.push(t);
    }
    events
        .transfers
        .extend(cancel_waiting_transfers(&mut tx, "held_order", id).await?);
    events.held_orders.push(id);
    tx.commit().await?;

    if let Some(hub) = hub {
        events.publish(pool.get_ref(), hub, branch_id).await;
    }
    let view = held_order_view(pool.get_ref(), id)
        .await?
        .ok_or(AppError::Internal)?;
    Ok(HttpResponse::Ok().json(view))
}

// ── Table assignment (interactive) ───────────────────────────────────────────

#[utoipa::path(post, path = "/held-orders/{id}/table", tag = "held_orders",
    params(("id" = Uuid, Path, description = "Held order ID")),
    request_body = AssignTableRequest,
    responses((status = 200, body = HeldOrderView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn assign_held_order_table(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<AssignTableRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "held_orders", "update").await?;
    require_held_order_branch_access(pool.get_ref(), &claims, *id).await?;
    assign_held_order_table_inner(pool, *id, body, Some(hub.get_ref())).await
}

/// Assign core — the INTERACTIVE path (edit sheet / canvas tap), so unlike the
/// park it FAILS loudly on an occupied table: the teller is looking at the
/// screen and picks another. `table_id: null` releases the table.
pub(crate) async fn assign_held_order_table_inner(
    pool: crate::db::Db,
    id: Uuid,
    body: web::Json<AssignTableRequest>,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    let mut events = FloorEvents::default();
    let mut tx = pool.get_ref().begin().await?;
    let row: Option<(Uuid, String, Option<Uuid>)> = sqlx::query_as(
        "SELECT branch_id, status, table_id FROM held_orders WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((branch_id, status, old_table)) = row else {
        return Err(AppError::NotFound("Held order not found".into()));
    };
    if status != "held" && status != "resumed" {
        return Err(AppError::Conflict(format!(
            "Held order is already {status}"
        )));
    }
    if body.table_id == old_table {
        tx.commit().await?; // no-op (idempotent retry)
        let view = held_order_view(pool.get_ref(), id)
            .await?
            .ok_or(AppError::Internal)?;
        return Ok(HttpResponse::Ok().json(view));
    }
    if let Some(t) = body.table_id {
        if !lock_table(&mut tx, t, branch_id).await? {
            return Err(AppError::BadRequest("Table is not in this branch".into()));
        }
        if occupant_of(&mut tx, t, Some(id), None).await?.is_some() {
            return Err(AppError::Conflict("Table is already occupied".into()));
        }
    }
    move_occupant(&mut tx, Occupant::HeldOrder(id), body.table_id, &mut events).await?;
    if let Some(old) = old_table {
        free_table(&mut tx, old).await?;
        events.tables.push(old);
    }
    if let Some(t) = body.table_id {
        seat_table(&mut tx, t).await?;
        events.tables.push(t);
    }
    tx.commit().await?;

    if let Some(hub) = hub {
        events.publish(pool.get_ref(), hub, branch_id).await;
    }
    let view = held_order_view(pool.get_ref(), id)
        .await?
        .ok_or(AppError::Internal)?;
    Ok(HttpResponse::Ok().json(view))
}

// ── Swap (atomic; covers move-to-empty and cross-entity swaps) ───────────────

#[utoipa::path(post, path = "/floor/tables/swap", tag = "held_orders",
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

/// Swap core: exchange the occupants of two tables in ONE transaction. One
/// empty side degenerates to a move; both empty is a 400. Works across entity
/// kinds (a held order can swap with a waiter ticket); the actor needs the
/// `update` permission of every kind it moves.
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
    let occ_a = occupant_of(&mut tx, body.table_a, None, None).await?;
    let occ_b = occupant_of(&mut tx, body.table_b, None, None).await?;
    if occ_a.is_none() && occ_b.is_none() {
        return Err(AppError::BadRequest("Both tables are empty".into()));
    }
    // The actor must hold `update` on every entity kind being moved.
    let mut resources: Vec<&'static str> = [occ_a, occ_b]
        .iter()
        .flatten()
        .map(occupant_move_resource)
        .collect();
    resources.dedup();
    for resource in resources {
        check_permission_for(
            pool.get_ref(),
            actor.teller_id,
            &actor.role,
            resource,
            "update",
        )
        .await?;
    }

    // Clear both sides first: the held-order one-live-per-table unique index
    // would otherwise reject the transient state where both sit on one table.
    if let Some(o) = occ_a {
        move_occupant(&mut tx, o, None, &mut events).await?;
    }
    if let Some(o) = occ_b {
        move_occupant(&mut tx, o, None, &mut events).await?;
    }
    if let Some(o) = occ_a {
        move_occupant(&mut tx, o, Some(body.table_b), &mut events).await?;
    }
    if let Some(o) = occ_b {
        move_occupant(&mut tx, o, Some(body.table_a), &mut events).await?;
    }
    // A table someone landed on is seated; a side left empty is bused.
    match occ_b {
        Some(_) => seat_table(&mut tx, body.table_a).await?,
        None => free_table(&mut tx, body.table_a).await?,
    }
    match occ_a {
        Some(_) => seat_table(&mut tx, body.table_b).await?,
        None => free_table(&mut tx, body.table_b).await?,
    }
    events.tables.push(body.table_a);
    events.tables.push(body.table_b);
    tx.commit().await?;

    if let Some(hub) = hub {
        events.publish(pool.get_ref(), hub, body.branch_id).await;
    }
    Ok(HttpResponse::Ok().json(serde_json::json!({ "ok": true })))
}

// ── Table state (status + section — the POS's operational edits) ─────────────

#[utoipa::path(patch, path = "/floor/tables/{id}/state", tag = "held_orders",
    params(("id" = Uuid, Path, description = "Table ID")),
    request_body = UpdateTableStateRequest,
    responses((status = 200, description = "Table state updated"), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn update_table_state(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<UpdateTableStateRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    // Host-operation gate (same as the legacy live-status endpoint) — floor
    // staff, not the manager-authoring `floor_plan` permission. Keep in
    // lock-step with the `UpdateTableState` replay op.
    check_permission(pool.get_ref(), &claims, "reservations", "update").await?;
    let branch_id: Option<Uuid> =
        sqlx::query_scalar("SELECT branch_id FROM branch_tables WHERE id = $1")
            .bind(*id)
            .fetch_optional(pool.get_ref())
            .await?;
    let branch_id = branch_id.ok_or_else(|| AppError::NotFound("Table not found".into()))?;
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;
    update_table_state_inner(pool, *id, body, Some(hub.get_ref())).await
}

/// State core. Both edits are idempotent LWW writes (a replayed op re-applies
/// harmlessly); the section move keeps whatever occupant sits on the table —
/// the physical table moved zones, party and all.
pub(crate) async fn update_table_state_inner(
    pool: crate::db::Db,
    id: Uuid,
    body: web::Json<UpdateTableStateRequest>,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    let branch_id: Option<Uuid> =
        sqlx::query_scalar("SELECT branch_id FROM branch_tables WHERE id = $1")
            .bind(id)
            .fetch_optional(pool.get_ref())
            .await?;
    let Some(branch_id) = branch_id else {
        return Err(AppError::NotFound("Table not found".into()));
    };
    if let Some(status) = body.status.as_deref()
        && !matches!(status, "free" | "held" | "seated" | "dirty")
    {
        return Err(AppError::BadRequest(
            "status must be free, held, seated or dirty".into(),
        ));
    }
    if let Some(section) = body.section_id {
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM floor_sections WHERE id = $1 AND branch_id = $2)",
        )
        .bind(section)
        .bind(branch_id)
        .fetch_one(pool.get_ref())
        .await?;
        if !ok {
            return Err(AppError::BadRequest("Section is not in this branch".into()));
        }
    }

    let set_section = body.section_id.is_some() || body.clear_section;
    sqlx::query(
        "UPDATE branch_tables SET \
             status = COALESCE($2, status), \
             section_id = CASE WHEN $3 THEN $4 ELSE section_id END, \
             updated_at = now() \
         WHERE id = $1",
    )
    .bind(id)
    .bind(body.status.as_deref())
    .bind(set_section)
    .bind(body.section_id)
    .execute(pool.get_ref())
    .await?;

    if let Some(hub) = hub {
        let mut events = FloorEvents::default();
        events.tables.push(id);
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

    // The occupant must be live here; its CURRENT table becomes `from_table_id`.
    let from_table: Option<Uuid> = match body.occupant_kind.as_str() {
        "held_order" => sqlx::query_scalar(
            "SELECT table_id FROM held_orders \
             WHERE id = $1 AND branch_id = $2 AND status IN ('held','resumed')",
        )
        .bind(body.occupant_id)
        .bind(body.branch_id)
        .fetch_optional(pool.get_ref())
        .await?
        .ok_or_else(|| AppError::Conflict("The order is no longer live".into()))?,
        "open_ticket" => sqlx::query_scalar(
            "SELECT table_id FROM open_tickets \
             WHERE id = $1 AND branch_id = $2 AND status IN ('open','ready')",
        )
        .bind(body.occupant_id)
        .bind(body.branch_id)
        .fetch_optional(pool.get_ref())
        .await?
        .ok_or_else(|| AppError::Conflict("The ticket is no longer live".into()))?,
        _ => {
            return Err(AppError::BadRequest(
                "occupant_kind must be held_order or open_ticket".into(),
            ));
        }
    };

    let waiting: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM table_transfer_requests \
         WHERE occupant_kind = $1 AND occupant_id = $2 AND status = 'waiting'",
    )
    .bind(&body.occupant_kind)
    .bind(body.occupant_id)
    .fetch_optional(pool.get_ref())
    .await?;
    if waiting.is_some() {
        return Err(AppError::Conflict(
            "This party already has a waiting transfer".into(),
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
            return Err(AppError::Conflict("Transfer is already fulfilled".into()));
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
    let row: Option<(Uuid, String, String, Uuid, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "SELECT branch_id, status, occupant_kind, occupant_id, target_section_id, target_table_id \
         FROM table_transfer_requests WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((branch_id, status, occupant_kind, occupant_id, target_section, target_table)) = row
    else {
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
            return Err(AppError::Conflict("Transfer is already cancelled".into()));
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

    // Resolve the occupant, which must still be live; the actor needs its
    // kind's `update` permission (a waiter can move their ticket, not a
    // teller's parked cart).
    let occupant = match occupant_kind.as_str() {
        "held_order" => {
            let live: Option<Option<Uuid>> = sqlx::query_scalar(
                "SELECT table_id FROM held_orders WHERE id = $1 AND status IN ('held','resumed')",
            )
            .bind(occupant_id)
            .fetch_optional(&mut *tx)
            .await?;
            live.map(|_| Occupant::HeldOrder(occupant_id))
        }
        _ => {
            let live: Option<Option<Uuid>> = sqlx::query_scalar(
                "SELECT table_id FROM open_tickets WHERE id = $1 AND status IN ('open','ready')",
            )
            .bind(occupant_id)
            .fetch_optional(&mut *tx)
            .await?;
            live.map(|_| Occupant::OpenTicket(occupant_id))
        }
    };
    let Some(occupant) = occupant else {
        return Err(AppError::Conflict(
            "The party's order is no longer live".into(),
        ));
    };
    check_permission_for(
        pool.get_ref(),
        actor.teller_id,
        &actor.role,
        occupant_move_resource(&occupant),
        "update",
    )
    .await?;

    if occupant_of(
        &mut tx,
        body.table_id,
        match occupant {
            Occupant::HeldOrder(h) => Some(h),
            _ => None,
        },
        match occupant {
            Occupant::OpenTicket(t) => Some(t),
            _ => None,
        },
    )
    .await?
    .is_some()
    {
        return Err(AppError::Conflict("Table is already occupied".into()));
    }

    // The old table (if any) frees up; the party lands on the new one.
    let old_table: Option<Uuid> = match occupant {
        Occupant::HeldOrder(h) => {
            sqlx::query_scalar("SELECT table_id FROM held_orders WHERE id = $1")
                .bind(h)
                .fetch_one(&mut *tx)
                .await?
        }
        Occupant::OpenTicket(t) => {
            sqlx::query_scalar("SELECT table_id FROM open_tickets WHERE id = $1")
                .bind(t)
                .fetch_one(&mut *tx)
                .await?
        }
    };
    move_occupant(&mut tx, occupant, Some(body.table_id), &mut events).await?;
    if let Some(old) = old_table
        && old != body.table_id
    {
        free_table(&mut tx, old).await?;
        events.tables.push(old);
    }
    seat_table(&mut tx, body.table_id).await?;
    events.tables.push(body.table_id);

    // `autofulfill_transfers` inside `move_occupant` resolves this request when
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
