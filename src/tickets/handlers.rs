//! Waiter open-ticket endpoints: fire (create), add round, list, get, void, and
//! the cashier settle (materialize → paid order via the shared snapshot engine).

use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::{
    OpenTicketView, extract_claims, fire_round, mint_ticket_ref, open_ticket_view, publish_fired,
    require_branch_access,
};
use crate::errors::{AppError, AppErrorResponse};
use crate::orders::handlers::{CreateOrderRequest, OrderItemInput, create_order_inner};
use crate::permissions::checker::check_permission;
use crate::realtime::event::{BranchEvent, Topic};
use crate::realtime::hub::BranchEventHub;
use crate::shifts::handlers::branch_has_open_shift;
use crate::sync::ActingContext;

// ── Requests ──────────────────────────────────────────────────

#[derive(Deserialize, Serialize, ToSchema)]
pub struct CreateOpenTicketRequest {
    pub branch_id: Uuid,
    #[serde(default)]
    pub table_id: Option<Uuid>,
    #[serde(default)]
    pub customer_name: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub guest_count: Option<i32>,
    /// Client-minted dedup key for the ticket (exactly-once across LAN + cloud).
    #[serde(default)]
    pub idempotency_key: Option<Uuid>,
    /// Per-round dedup key for the first round.
    #[serde(default)]
    pub round_idempotency_key: Option<Uuid>,
    /// Client-priced items (same shape as a POS order line) — recorded verbatim.
    pub items: Vec<OrderItemInput>,
    /// Optional discount the waiter applied at order time (overridable at settle).
    #[serde(default)]
    pub discount_id: Option<Uuid>,
    #[serde(default)]
    pub discount_type: Option<String>,
    #[serde(default)]
    pub discount_value: Option<i32>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct AddRoundRequest {
    #[serde(default)]
    pub idempotency_key: Option<Uuid>,
    pub items: Vec<OrderItemInput>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct SettleOpenTicketRequest {
    pub shift_id: Uuid,
    pub payment_method: String,
    /// Settle-time overrides (else the ticket's own discount / no tip).
    #[serde(default)]
    pub discount_id: Option<Uuid>,
    #[serde(default)]
    pub discount_type: Option<String>,
    #[serde(default)]
    pub discount_value: Option<i32>,
    #[serde(default)]
    pub tip_amount: Option<i32>,
    #[serde(default)]
    pub tip_payment_method: Option<String>,
    #[serde(default)]
    pub amount_tendered: Option<i32>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct VoidOpenTicketRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    pub branch_id: Uuid,
    #[serde(default)]
    pub status: Option<String>,
}

/// Resolve a ticket's branch and enforce the LIVE caller's access to it. The
/// `*_inner` cores stay claims-free (replay authorizes via the embedded actor's
/// org + `op_branch_must_be_in_org`), so each live wrapper runs this first.
async fn require_ticket_branch_access(
    pool: &PgPool,
    claims: &crate::auth::jwt::Claims,
    ticket_id: Uuid,
) -> Result<(), AppError> {
    let branch_id: Option<Uuid> =
        sqlx::query_scalar("SELECT branch_id FROM open_tickets WHERE id = $1")
            .bind(ticket_id)
            .fetch_optional(pool)
            .await?;
    let branch_id = branch_id.ok_or_else(|| AppError::NotFound("Open ticket not found".into()))?;
    require_branch_access(pool, claims, branch_id).await
}

async fn table_label(pool: &PgPool, table_id: Option<Uuid>) -> Result<Option<String>, AppError> {
    match table_id {
        Some(t) => Ok(
            sqlx::query_scalar("SELECT label FROM branch_tables WHERE id = $1")
                .bind(t)
                .fetch_optional(pool)
                .await?,
        ),
        None => Ok(None),
    }
}

// ── Create (fire round 1) ─────────────────────────────────────

#[utoipa::path(post, path = "/open-tickets", tag = "open_tickets", request_body = CreateOpenTicketRequest,
    responses((status = 201, body = OpenTicketView), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn create_open_ticket(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    hub: web::Data<BranchEventHub>,
    body: web::Json<CreateOpenTicketRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "open_tickets", "create").await?;
    require_branch_access(pool.get_ref(), &claims, body.branch_id).await?;
    create_open_ticket_inner(
        pool.clone(),
        body,
        ActingContext::live(&claims)?,
        Some(hub.get_ref()),
    )
    .await
}

/// Fire core. LIVE attributes the ticket to the JWT waiter and requires the
/// branch to be operating (any till open); REPLAY attributes it to the queued
/// op's embedded waiter and skips the branch-open gate (it was validated
/// LAN-first at fire time — the ticket floats free of any till and is settled
/// later). BOTH dedup on the in-body ticket idempotency key.
pub(crate) async fn create_open_ticket_inner(
    pool: web::Data<PgPool>,
    body: web::Json<CreateOpenTicketRequest>,
    actor: ActingContext,
    // The realtime bus, for firing a LIVE ticket to the KDS. `None` on replay (a
    // queued offline fire is historical; cloud consumers re-seed via snapshot).
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    if body.items.is_empty() {
        return Err(AppError::BadRequest(
            "A ticket must fire at least one item".into(),
        ));
    }
    // The branch must be operating (any till open) to fire to the kitchen. Replay
    // is recorded history (the gate was answered LAN-first at fire time) → skip.
    if !actor.replay && !branch_has_open_shift(pool.get_ref(), body.branch_id).await? {
        return Err(AppError::Conflict(
            "No open shift at this branch — open a till first".into(),
        ));
    }

    // Idempotent re-fire: same ticket key → return the existing ticket.
    if let Some(key) = body.idempotency_key {
        let existing: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM open_tickets WHERE idempotency_key = $1")
                .bind(key)
                .fetch_optional(pool.get_ref())
                .await?;
        if let Some(id) = existing {
            let view = open_ticket_view(pool.get_ref(), id).await?;
            return Ok(HttpResponse::Ok().json(view));
        }
    }

    let org_id: Uuid =
        sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
            .bind(body.branch_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    let label = table_label(pool.get_ref(), body.table_id).await?;
    let now = chrono::Utc::now();
    let mut tx = pool.get_ref().begin().await?;
    let ticket_ref = mint_ticket_ref(&mut tx, body.branch_id, now).await?;

    let open_ticket_id: Uuid = sqlx::query_scalar(
        "INSERT INTO open_tickets \
            (org_id, branch_id, table_id, ticket_ref, opened_by, customer_name, notes, guest_count, \
             idempotency_key, discount_id, discount_type, discount_value) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) RETURNING id",
    )
    .bind(org_id)
    .bind(body.branch_id)
    .bind(body.table_id)
    .bind(&ticket_ref)
    .bind(actor.teller_id)
    .bind(&body.customer_name)
    .bind(&body.notes)
    .bind(body.guest_count)
    .bind(body.idempotency_key)
    .bind(body.discount_id)
    .bind(&body.discount_type)
    .bind(body.discount_value)
    .fetch_one(&mut *tx)
    .await?;

    let kt_id = fire_round(
        &mut tx,
        pool.get_ref(),
        org_id,
        body.branch_id,
        open_ticket_id,
        1,
        actor.teller_id,
        body.round_idempotency_key,
        &body.items,
        label.as_deref(),
        Some(ticket_ref.as_str()),
    )
    .await?;
    tx.commit().await?;

    if let Some(hub) = hub {
        publish_fired(
            pool.get_ref(),
            hub,
            body.branch_id,
            open_ticket_id,
            kt_id,
            "ticket.fired",
        )
        .await;
    }
    let view = open_ticket_view(pool.get_ref(), open_ticket_id).await?;
    Ok(HttpResponse::Created().json(view))
}

// ── Add round ─────────────────────────────────────────────────

#[utoipa::path(post, path = "/open-tickets/{id}/rounds", tag = "open_tickets", request_body = AddRoundRequest,
    params(("id" = Uuid, Path, description = "Open ticket ID")),
    responses((status = 200, body = OpenTicketView), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn add_round(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<AddRoundRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "open_tickets", "update").await?;
    require_ticket_branch_access(pool.get_ref(), &claims, *id).await?;
    add_round_inner(
        pool.clone(),
        id.into_inner(),
        body,
        ActingContext::live(&claims)?,
        Some(hub.get_ref()),
    )
    .await
}

/// Add-round core. Fires the next round's client-priced items onto an existing
/// open ticket. Shared by the live route and `/sync/replay` (a queued offline
/// round); dedups on the per-round idempotency key.
pub(crate) async fn add_round_inner(
    pool: web::Data<PgPool>,
    id: Uuid,
    body: web::Json<AddRoundRequest>,
    actor: ActingContext,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    if body.items.is_empty() {
        return Err(AppError::BadRequest(
            "A round must fire at least one item".into(),
        ));
    }

    let ticket: Option<(Uuid, Uuid, String, Option<Uuid>, Option<String>)> = sqlx::query_as(
        "SELECT branch_id, org_id, status::text, table_id, ticket_ref FROM open_tickets WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool.get_ref())
    .await?;
    let Some((branch_id, org_id, status, table_id, ticket_ref)) = ticket else {
        return Err(AppError::NotFound("Open ticket not found".into()));
    };
    // Idempotent round re-fire — checked BEFORE the status gate so a retry of an
    // ALREADY-APPLIED round dedups to 200 even if the ticket has since been settled
    // (its ack may have been lost). Only a genuinely new round falls through to the
    // conflict gate, so the client can safely treat that 409 as a real rejection.
    if let Some(key) = body.idempotency_key {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM open_ticket_rounds WHERE idempotency_key = $1)",
        )
        .bind(key)
        .fetch_one(pool.get_ref())
        .await?;
        if exists {
            let view = open_ticket_view(pool.get_ref(), id).await?;
            return Ok(HttpResponse::Ok().json(view));
        }
    }

    if status == "settled" || status == "voided" {
        return Err(AppError::Conflict(format!(
            "Cannot add a round to a {status} ticket"
        )));
    }

    let label = table_label(pool.get_ref(), table_id).await?;
    let mut tx = pool.get_ref().begin().await?;
    let next_round: i32 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(round_number), 0) + 1 FROM open_ticket_rounds WHERE open_ticket_id = $1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    let kt_id = fire_round(
        &mut tx,
        pool.get_ref(),
        org_id,
        branch_id,
        id,
        next_round,
        actor.teller_id,
        body.idempotency_key,
        &body.items,
        label.as_deref(),
        ticket_ref.as_deref(),
    )
    .await?;
    tx.commit().await?;

    if let Some(hub) = hub {
        publish_fired(
            pool.get_ref(),
            hub,
            branch_id,
            id,
            kt_id,
            "ticket.round_added",
        )
        .await;
    }
    let view = open_ticket_view(pool.get_ref(), id).await?;
    Ok(HttpResponse::Ok().json(view))
}

// ── List / get ────────────────────────────────────────────────

#[utoipa::path(get, path = "/open-tickets", tag = "open_tickets", params(ListQuery),
    responses((status = 200, body = Vec<OpenTicketView>), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn list_open_tickets(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<ListQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "open_tickets", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM open_tickets \
         WHERE branch_id = $1 AND ($2::text IS NULL OR status::text = $2) \
         ORDER BY opened_at DESC LIMIT 500",
    )
    .bind(query.branch_id)
    .bind(query.status.as_deref())
    .fetch_all(pool.get_ref())
    .await?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(v) = open_ticket_view(pool.get_ref(), id).await? {
            out.push(v);
        }
    }
    Ok(HttpResponse::Ok().json(out))
}

#[utoipa::path(get, path = "/open-tickets/{id}", tag = "open_tickets",
    params(("id" = Uuid, Path, description = "Open ticket ID")),
    responses((status = 200, body = OpenTicketView), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn get_open_ticket(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "open_tickets", "read").await?;
    let view = open_ticket_view(pool.get_ref(), *id)
        .await?
        .ok_or_else(|| AppError::NotFound("Open ticket not found".into()))?;
    require_branch_access(pool.get_ref(), &claims, view.branch_id).await?;
    Ok(HttpResponse::Ok().json(view))
}

// ── Void ──────────────────────────────────────────────────────

#[utoipa::path(post, path = "/open-tickets/{id}/void", tag = "open_tickets", request_body = VoidOpenTicketRequest,
    params(("id" = Uuid, Path, description = "Open ticket ID")),
    responses((status = 200, body = OpenTicketView), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn void_open_ticket(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<VoidOpenTicketRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "open_tickets", "update").await?;
    require_ticket_branch_access(pool.get_ref(), &claims, *id).await?;
    void_open_ticket_inner(pool.clone(), id.into_inner(), body, Some(hub.get_ref())).await
}

/// Void core. Marks the ticket voided and pulls its kitchen tickets off the KDS.
/// Shared by the live route and `/sync/replay` (a queued offline void). No actor
/// attribution (the void carries only a reason), so it takes no `ActingContext`.
pub(crate) async fn void_open_ticket_inner(
    pool: web::Data<PgPool>,
    id: Uuid,
    body: web::Json<VoidOpenTicketRequest>,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    let id = &id;
    let view = open_ticket_view(pool.get_ref(), *id)
        .await?
        .ok_or_else(|| AppError::NotFound("Open ticket not found".into()))?;
    if view.status == "settled" {
        return Err(AppError::Conflict("Cannot void a settled ticket".into()));
    }
    let mut tx = pool.get_ref().begin().await?;
    sqlx::query(
        "UPDATE open_tickets SET status = 'voided', voided_at = now(), void_reason = $2, updated_at = now() \
         WHERE id = $1 AND status <> 'settled'",
    )
    .bind(*id)
    .bind(body.reason.as_deref())
    .execute(&mut *tx)
    .await?;
    // Void the kitchen tickets too so they leave the KDS.
    sqlx::query(
        "UPDATE kitchen_ticket_items SET voided_at = now() \
         FROM kitchen_tickets kt \
         WHERE kitchen_ticket_items.kitchen_ticket_id = kt.id \
           AND kt.source_type = 'open_ticket' AND kt.source_id = $1 \
           AND kitchen_ticket_items.voided_at IS NULL",
    )
    .bind(*id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE kitchen_tickets SET status = 'voided', voided_at = now() \
         WHERE source_type = 'open_ticket' AND source_id = $1 AND status <> 'voided'",
    )
    .bind(*id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let view = open_ticket_view(pool.get_ref(), *id).await?;
    if let Some(hub) = hub
        && let Some(v) = &view
    {
        hub.publish(
            v.branch_id,
            BranchEvent::new(Topic::Tickets, "ticket.voided", v),
        );
    }
    Ok(HttpResponse::Ok().json(view))
}

// ── Move to another table ─────────────────────────────────────

#[derive(Deserialize, Serialize, ToSchema)]
pub struct MoveTicketTableRequest {
    /// The table to move this ticket onto.
    pub table_id: Uuid,
}

/// Switch an open ticket to a different table (the "move table" button). Works
/// for any live ticket — walk-in dine-in or one auto-opened from a booking. The
/// old table is flagged `dirty` (bus it), the new one `seated`; if the ticket
/// came from a booking, the booking's assignment is kept in sync.
#[utoipa::path(patch, path = "/open-tickets/{id}/table", tag = "open_tickets",
    params(("id" = Uuid, Path, description = "Open ticket ID")),
    request_body = MoveTicketTableRequest,
    responses((status = 200, description = "Ticket moved", body = OpenTicketView), AppErrorResponse),
    security(("bearer_jwt" = [])))]
pub async fn move_ticket_table(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<MoveTicketTableRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "open_tickets", "update").await?;
    require_ticket_branch_access(pool.get_ref(), &claims, *id).await?;

    let row: Option<(Uuid, Option<Uuid>, String, Option<Uuid>)> = sqlx::query_as(
        "SELECT branch_id, table_id, status::text, booking_id FROM open_tickets WHERE id = $1",
    )
    .bind(*id)
    .fetch_optional(pool.get_ref())
    .await?;
    let (branch_id, old_table, status, booking_id) =
        row.ok_or_else(|| AppError::NotFound("Open ticket not found".into()))?;
    if status == "settled" || status == "voided" {
        return Err(AppError::Conflict(
            "Cannot move a settled or voided ticket".into(),
        ));
    }
    let target_ok: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM branch_tables WHERE id = $1 AND branch_id = $2)",
    )
    .bind(body.table_id)
    .bind(branch_id)
    .fetch_one(pool.get_ref())
    .await?;
    if !target_ok {
        return Err(AppError::BadRequest(
            "Target table is not in this branch".into(),
        ));
    }

    let mut tx = pool.get_ref().begin().await?;
    sqlx::query("UPDATE open_tickets SET table_id = $2, updated_at = now() WHERE id = $1")
        .bind(*id)
        .bind(body.table_id)
        .execute(&mut *tx)
        .await?;
    if let Some(old) = old_table
        && old != body.table_id
    {
        sqlx::query("UPDATE branch_tables SET status = 'dirty', updated_at = now() WHERE id = $1")
            .bind(old)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("UPDATE branch_tables SET status = 'seated', updated_at = now() WHERE id = $1")
        .bind(body.table_id)
        .execute(&mut *tx)
        .await?;
    if let Some(bid) = booking_id {
        sqlx::query("DELETE FROM booking_tables WHERE booking_id = $1")
            .bind(bid)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO booking_tables (booking_id, table_id) VALUES ($1, $2)")
            .bind(bid)
            .bind(body.table_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    let view = open_ticket_view(pool.get_ref(), *id).await?;
    if let Some(v) = &view {
        hub.publish(
            v.branch_id,
            BranchEvent::new(Topic::Tickets, "ticket.moved", v),
        );
        // The floor changed too — let the reservations/floor view refresh.
        hub.publish(
            v.branch_id,
            BranchEvent::new(
                Topic::Reservations,
                "table.status_changed",
                &serde_json::json!({ "branch_id": branch_id, "table_id": body.table_id }),
            ),
        );
    }
    Ok(HttpResponse::Ok().json(view))
}

// ── Settle (materialize → paid order) ─────────────────────────

#[utoipa::path(post, path = "/open-tickets/{id}/settle", tag = "open_tickets", request_body = SettleOpenTicketRequest,
    params(("id" = Uuid, Path, description = "Open ticket ID")),
    responses((status = 200, description = "Settled; returns the created order", body = crate::orders::handlers::Order),
        AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn settle_open_ticket(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<SettleOpenTicketRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "open_tickets", "update").await?;
    check_permission(pool.get_ref(), &claims, "orders", "create").await?;
    check_permission(pool.get_ref(), &claims, "payments", "create").await?;
    require_ticket_branch_access(pool.get_ref(), &claims, *id).await?;
    settle_open_ticket_inner(
        pool.clone(),
        id.into_inner(),
        body,
        ActingContext::live(&claims)?,
        Some(hub.get_ref()),
    )
    .await
}

/// Settle core. Materializes the ticket's stored client-priced lines into one
/// paid order via `create_order_inner` (with the ticket id as the order
/// idempotency key → a retried/concurrent/replayed settle dedups to one order),
/// landing it in the SETTLING cashier's open shift. Shared by the live route and
/// `/sync/replay` (a queued offline settle).
#[allow(clippy::type_complexity)]
pub(crate) async fn settle_open_ticket_inner(
    pool: web::Data<PgPool>,
    id: Uuid,
    body: web::Json<SettleOpenTicketRequest>,
    actor: ActingContext,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    let id = &id;
    let row: Option<(
        Uuid,
        Uuid,
        String,
        Option<Uuid>,
        Option<String>,
        Option<String>,
        Option<Uuid>,
        Option<String>,
        Option<i32>,
    )> = sqlx::query_as(
        "SELECT branch_id, org_id, status::text, order_id, customer_name, notes, \
                    discount_id, discount_type, discount_value FROM open_tickets WHERE id = $1",
    )
    .bind(*id)
    .fetch_optional(pool.get_ref())
    .await?;
    let Some((
        branch_id,
        org_id,
        status,
        order_id,
        customer_name,
        notes,
        t_disc_id,
        t_disc_type,
        t_disc_value,
    )) = row
    else {
        return Err(AppError::NotFound("Open ticket not found".into()));
    };
    if status == "voided" {
        return Err(AppError::Conflict("Cannot settle a voided ticket".into()));
    }
    if status == "settled" || order_id.is_some() {
        // Already settled. A REPLAYED (lost-ack) settle is idempotent — return the
        // existing paid order (the order idempotency key is the ticket id). A LIVE
        // double-settle (two cashiers racing the same ticket) is a clean conflict.
        if actor.replay
            && let Some(order) =
                crate::orders::handlers::fetch_order_by_idempotency_key(pool.get_ref(), *id, org_id)
                    .await?
        {
            return Ok(HttpResponse::Ok().json(order));
        }
        return Err(AppError::Conflict("Ticket is already settled".into()));
    }

    // Replay the stored client-priced items back through the POS create-order path.
    let line_rows: Vec<(serde_json::Value,)> = sqlx::query_as(
        "SELECT oti.line FROM open_ticket_items oti \
         JOIN open_ticket_rounds r ON r.id = oti.round_id \
         WHERE oti.open_ticket_id = $1 AND oti.voided_at IS NULL \
         ORDER BY r.round_number, oti.created_at",
    )
    .bind(*id)
    .fetch_all(pool.get_ref())
    .await?;
    if line_rows.is_empty() {
        return Err(AppError::BadRequest(
            "Nothing to settle — the ticket has no live items".into(),
        ));
    }
    let mut items: Vec<OrderItemInput> = Vec::with_capacity(line_rows.len());
    for (line_json,) in line_rows {
        let stored: super::StoredTicketLine =
            serde_json::from_value(line_json).map_err(|_| AppError::Internal)?;
        let input: OrderItemInput =
            serde_json::from_value(stored.input).map_err(|_| AppError::Internal)?;
        items.push(input);
    }

    // Discount: the cashier's settle override wins, else the waiter's ticket discount.
    let discount_id = body.discount_id.or(t_disc_id);
    let discount_type = body.discount_type.clone().or(t_disc_type);
    let discount_value = body.discount_value.or(t_disc_value);

    // Build a POS order request. The TICKET ID is the order idempotency key, so a
    // retried/concurrent settle dedups to one paid order. `create_order_inner`
    // enforces the cashier's open shift, validates the payment method, computes
    // deductions/inventory/tax, and lands the sale in the cashier's drawer.
    let request = CreateOrderRequest {
        branch_id,
        shift_id: body.shift_id,
        payment_method: body.payment_method.clone(),
        customer_name,
        notes,
        discount_type,
        discount_value,
        discount_id,
        amount_tendered: body.amount_tendered,
        tip_amount: body.tip_amount,
        tip_payment_method: body.tip_payment_method.clone(),
        payment_splits: None,
        items,
        created_at: Some(chrono::Utc::now()),
        subtotal: None,
        discount_amount: None,
        tax_amount: None,
        total_amount: None,
        change_given: None,
        idempotency_key: Some(*id),
        order_number: None,
        order_ref: None,
    };

    // The settling cashier (`actor`) owns the materialized order + drawer. Capture
    // the id before `create_order_inner` consumes the context.
    let settled_by = actor.teller_id;
    // hub = None → don't re-fire the kitchen (the items already fired at order time).
    let _ = create_order_inner(pool.clone(), web::Json(request), actor, None).await?;

    let created =
        crate::orders::handlers::fetch_order_by_idempotency_key(pool.get_ref(), *id, org_id)
            .await?
            .ok_or(AppError::Internal)?;

    // Link ticket → order (idempotent; a concurrent settle that lost the race is a no-op).
    sqlx::query(
        "UPDATE open_tickets SET status = 'settled', settled_at = now(), order_id = $2, \
             settled_by = $3, settled_shift_id = $4, updated_at = now() \
         WHERE id = $1 AND status <> 'settled'",
    )
    .bind(*id)
    .bind(created.id)
    .bind(settled_by)
    .bind(body.shift_id)
    .execute(pool.get_ref())
    .await?;

    if let Some(hub) = hub
        && let Ok(Some(view)) = open_ticket_view(pool.get_ref(), *id).await
    {
        hub.publish(
            branch_id,
            BranchEvent::new(Topic::Tickets, "ticket.settled", &view),
        );
    }
    Ok(HttpResponse::Ok().json(created))
}
