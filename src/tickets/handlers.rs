//! Waiter open-ticket endpoints: fire (create), add round, list, get, void, and
//! the cashier settle (materialize → paid order via the shared snapshot engine).

use actix_web::{HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::{
    OpenTicketView, extract_claims, fire_round, mint_ticket_ref, open_ticket_view, publish_fired,
    publish_table_status, require_branch_access,
};
use crate::errors::{AppError, AppErrorResponse};
use crate::orders::VoidReason;
use crate::orders::handlers::{
    CreateOrderRequest, OrderItemInput, PaymentSplitInput, SettledTicket, create_order_inner,
};
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
    /// The booking this party arrived under: the ticket links to it and the
    /// booking moves to `seated` in the same transaction.
    #[serde(default)]
    pub booking_id: Option<Uuid>,
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
    pub discount_value: Option<rust_decimal::Decimal>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct AddRoundRequest {
    #[serde(default)]
    pub idempotency_key: Option<Uuid>,
    pub items: Vec<OrderItemInput>,
}

/// Whose price a fired round is recorded at: the catalogue's now, or what was
/// charged at the table if this round is being replayed off a till's outbox.
/// One place, so a fire and a later round cannot answer it differently.
fn client_prices(actor: &ActingContext) -> crate::orders::handlers::ClientPrices {
    if actor.replay {
        crate::orders::handlers::ClientPrices::AsCharged
    } else {
        crate::orders::handlers::ClientPrices::Ignore
    }
}

/// The literal a cashier sends as `discount_type` to settle WITHOUT the
/// waiter's discount. Absent means inherit it; anything else overrides it.
pub const DISCOUNT_NONE: &str = "none";

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct SettleOpenTicketRequest {
    pub shift_id: Uuid,
    pub payment_method: String,
    /// Settle-time discount. ABSENT (all three fields) means the waiter's
    /// ticket discount is inherited, as it always was — but the till can now
    /// see that discount on the ticket view. The literal `discount_type:
    /// "none"` settles with no discount at all; any other value (or a
    /// `discount_id`) replaces the waiter's.
    #[serde(default)]
    pub discount_id: Option<Uuid>,
    #[serde(default)]
    pub discount_type: Option<String>,
    #[serde(default)]
    pub discount_value: Option<rust_decimal::Decimal>,
    #[serde(default)]
    pub tip_amount: Option<i32>,
    #[serde(default)]
    pub tip_payment_method: Option<String>,
    #[serde(default)]
    pub amount_tendered: Option<i32>,
    /// What the till handed back. Recorded as the drawer saw it, like a
    /// counter sale's; absent, it is derived from `amount_tendered` and the
    /// server's total.
    #[serde(default)]
    pub change_given: Option<i32>,
    /// When the bill was paid, as the till says. An offline settle replayed
    /// later keeps its real time — it becomes the order's `created_at` and the
    /// ticket's `settled_at`, one instant on both rows. Absent means now; a
    /// future clock is refused.
    #[serde(default)]
    pub settled_at: Option<chrono::DateTime<chrono::Utc>>,
    /// What the till says the bill came to — the figure its drawer collected.
    /// Checked against the server's own total exactly as a counter checkout is
    /// (`create_order_inner`'s drift check); a disagreement is refused, not
    /// recorded. Absent on older builds, which then get no check. The figure
    /// to send is `OpenTicketView::bill.total`, which is priced by the same
    /// engine under the same policy — a till that shows that number cannot
    /// disagree with the books.
    #[serde(default)]
    pub total_amount: Option<i32>,
    /// Split tenders, when the party paid with more than one. Carried to the
    /// order's payment legs like a counter sale's; they must sum to the total.
    #[serde(default)]
    pub payment_splits: Option<Vec<PaymentSplitInput>>,
    /// The member spending a balance on this settle, when rewards are applied.
    #[serde(default)]
    pub loyalty_customer_id: Option<Uuid>,
    /// Rewards covering lines of the ticket. A table-service bill redeems
    /// exactly like a counter one — the cashier scans at settle either way.
    #[serde(default)]
    pub loyalty_redemptions: Vec<crate::orders::handlers::LoyaltyRedemptionInput>,
}

/// Why a bill is torn up. `reason` is typed; `note` is what actually happened,
/// required when the reason is `other`.
///
/// Deserialised leniently, because a void queued offline by an older till
/// arrives here months later with the picker's LABEL (`"Order mistake"`, or
/// `"Order mistake — burnt"`) where the enum now is, and a queued op that fails
/// to parse dead-letters. Those spellings map exactly as migration
/// `20260912020000` mapped the stored rows; an unrecognised string is `other`
/// with the whole text as the note, so nothing the waiter wrote is lost.
#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
#[serde(from = "VoidOpenTicketWire")]
pub struct VoidOpenTicketRequest {
    #[serde(default)]
    pub reason: Option<VoidReason>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Deserialize)]
struct VoidOpenTicketWire {
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

impl From<VoidOpenTicketWire> for VoidOpenTicketRequest {
    fn from(w: VoidOpenTicketWire) -> Self {
        let clean = |s: Option<String>| s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let note = clean(w.note);
        let Some(raw) = clean(w.reason) else {
            return Self { reason: None, note };
        };
        if let Some(reason) = VoidReason::parse(&raw) {
            return Self {
                reason: Some(reason),
                note,
            };
        }
        // The legacy picker: `<label>` or `<label> — <note>` (em dash, spaced).
        let (label, tail) = match raw.split_once(" — ") {
            Some((l, n)) => (l, clean(Some(n.to_string()))),
            None => (raw.as_str(), None),
        };
        match VoidReason::from_legacy_label(label) {
            Some(reason) => Self {
                reason: Some(reason),
                note: note.or(tail),
            },
            None => Self {
                reason: Some(VoidReason::Other),
                note: Some(note.unwrap_or(raw)),
            },
        }
    }
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
    pool: crate::db::Db,
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
    pool: crate::db::Db,
    body: web::Json<CreateOpenTicketRequest>,
    actor: ActingContext,
    // The realtime bus, for firing a LIVE ticket to the KDS. `None` on replay (a
    // queued offline fire is historical; cloud consumers re-seed via snapshot).
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    // A TICKET IS A BILL. It exists because somebody ordered something.
    //
    // Seating a party is not this. It is a fact about the room — that table is
    // taken — and it now travels on its own, through `hold_table` /
    // `release_table` in `floor_ops`. Nothing about a party who has sat down
    // and not yet ordered belongs in a bill: an empty ticket had no money, no
    // kitchen work and no reason to be in a report, and a party who changed
    // their mind and left needed it VOIDED, as though a sale had been undone.
    //
    // So the tab starts with the first round, carrying the table the party is
    // already sitting at.
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

    // A percentage is a fraction here, like the tax rate. Checked at the FIRE
    // rather than only at the settle: a waiter's discount that the till will
    // refuse hours later, with the party waiting to pay, is a bad place to
    // discover a stale client.
    if body.discount_type.as_deref() == Some("percentage")
        && body.discount_value.unwrap_or(rust_decimal::Decimal::ZERO) > rust_decimal::Decimal::ONE
    {
        return Err(AppError::BadRequest(
            "discount_value for a percentage is a fraction between 0 and 1 (0.14 = 14%)".into(),
        ));
    }

    let org_id: Uuid =
        sqlx::query_scalar("SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL")
            .bind(body.branch_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    let now = chrono::Utc::now();
    let mut tx = pool.get_ref().begin().await?;
    let ticket_ref = mint_ticket_ref(&mut tx, body.branch_id, now).await?;

    // Table arbitration. An occupied — or unknown, or unbussed — table is
    // DROPPED rather than failing the fire: a queued offline round must never
    // dead-letter over a table race, so the ticket floats table-less and the
    // waiter reassigns it from the canvas.
    //
    // `seated` with no ticket on it is CLAIMABLE, and that is the ordinary
    // path: the party sat down first, which held the table, and this is their
    // first round arriving to start the bill. `dirty` is not — nobody has
    // bussed it, and a fire silently clearing that would send the next party to
    // somebody else's plates.
    let claimable = match body.table_id {
        Some(t) => crate::floor_ops::ticket_may_claim(&mut tx, t, body.branch_id).await?,
        None => false,
    };
    let table_id = if claimable { body.table_id } else { None };
    let label = table_label(pool.get_ref(), table_id).await?;

    let open_ticket_id: Uuid = sqlx::query_scalar(
        "INSERT INTO open_tickets \
            (org_id, branch_id, table_id, ticket_ref, opened_by, customer_name, notes, guest_count, \
             idempotency_key, discount_id, discount_type, discount_value, booking_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) RETURNING id",
    )
    .bind(org_id)
    .bind(body.branch_id)
    .bind(table_id)
    .bind(&ticket_ref)
    .bind(actor.teller_id)
    .bind(&body.customer_name)
    .bind(&body.notes)
    .bind(body.guest_count)
    .bind(body.idempotency_key)
    .bind(body.discount_id)
    .bind(&body.discount_type)
    .bind(body.discount_value)
    .bind(body.booking_id)
    .fetch_one(&mut *tx)
    .await?;

    if let Some(t) = table_id {
        let hand = crate::floor_ops::Hand::of(&mut *tx, actor.teller_id, body.branch_id).await?;
        crate::floor_ops::take_table(
            &mut tx,
            t,
            crate::floor_ops::Holder::Ticket {
                id: open_ticket_id,
                booking_id: body.booking_id,
            },
            crate::floor_ops::party_size(body.guest_count),
            &hand,
        )
        .await?;
    }
    // A booked party sat down: the booking becomes `seated` with this ticket.
    if let Some(b) = body.booking_id {
        crate::bookings::handlers::link_ticket(&mut tx, b, body.branch_id, open_ticket_id).await?;
    }
    let kt_id = Some(
        fire_round(
            &mut tx,
            pool.get_ref(),
            org_id,
            body.branch_id,
            open_ticket_id,
            actor.teller_id,
            body.round_idempotency_key,
            &body.items,
            label.as_deref(),
            Some(ticket_ref.as_str()),
            client_prices(&actor),
        )
        .await?,
    );
    tx.commit().await?;

    if let Some(hub) = hub {
        if let Some(kt_id) = kt_id {
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
        if let Some(t) = table_id {
            publish_table_status(pool.get_ref(), hub, body.branch_id, t).await;
        }
        if let Some(b) = body.booking_id {
            crate::bookings::publish_booking(pool.get_ref(), hub, "booking.changed", b).await;
        }
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
    pool: crate::db::Db,
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
    pool: crate::db::Db,
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

    if status != "open" {
        return Err(AppError::Conflict(format!(
            "Cannot add a round to a {status} ticket"
        )));
    }

    let label = table_label(pool.get_ref(), table_id).await?;
    let mut tx = pool.get_ref().begin().await?;
    // The round number is allocated by the database under the ticket's row
    // lock (see `fire_round`) — never computed here from MAX + 1.
    let kt_id = fire_round(
        &mut tx,
        pool.get_ref(),
        org_id,
        branch_id,
        id,
        actor.teller_id,
        body.idempotency_key,
        &body.items,
        label.as_deref(),
        ticket_ref.as_deref(),
        client_prices(&actor),
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
    pool: crate::db::Db,
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
    pool: crate::db::Db,
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
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<VoidOpenTicketRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    // Tearing up a bill is the ticket's void rung, `open_tickets:delete`, kept
    // apart from adding to it. `sync::ReplayOp::required_permissions` asks the
    // same of a queued void; keep the two in step.
    check_permission(pool.get_ref(), &claims, "open_tickets", "delete").await?;
    require_ticket_branch_access(pool.get_ref(), &claims, *id).await?;
    void_open_ticket_inner(
        pool.clone(),
        id.into_inner(),
        body,
        ActingContext::live(&claims)?,
        Some(hub.get_ref()),
    )
    .await
}

/// Void core. A void is an EVENT — who, when, a categorised reason and a note
/// — recorded on the bill exactly as an order void is, and it pulls the
/// ticket's kitchen copies off the KDS (closed `voided`). Shared by the live
/// route and `/sync/replay` (a queued offline void), attributed to `actor`.
///
/// LIVE requires a reason, and a note when the reason is `other`. REPLAY is
/// recorded history: a void an older till queued without a reason is applied
/// with none, which is the truth about it, rather than dead-lettered over a
/// field it could not have known to send.
///
/// Idempotent: voiding a voided ticket returns it unchanged (a lost-ack retry
/// must not re-stamp `voided_at`). A settled bill cannot be voided here — that
/// is a void or refund on its ORDER.
pub(crate) async fn void_open_ticket_inner(
    pool: crate::db::Db,
    id: Uuid,
    body: web::Json<VoidOpenTicketRequest>,
    actor: ActingContext,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    let id = &id;
    let view = open_ticket_view(pool.get_ref(), *id)
        .await?
        .ok_or_else(|| AppError::NotFound("Open ticket not found".into()))?;
    match view.status.as_str() {
        "settled" => return Err(AppError::Conflict("Cannot void a settled ticket".into())),
        "voided" => return Ok(HttpResponse::Ok().json(view)),
        _ => {}
    }
    if !actor.replay {
        if body.reason.is_none() {
            return Err(AppError::BadRequest("A void needs a reason".into()));
        }
        if body.reason == Some(VoidReason::Other) && body.note.is_none() {
            return Err(AppError::BadRequest(
                "A note is required when the void reason is 'other'".into(),
            ));
        }
    }

    let mut tx = pool.get_ref().begin().await?;
    let voided = sqlx::query(
        "UPDATE open_tickets SET status = 'voided', voided_at = now(), voided_by = $2, \
             void_reason = $3::void_reason, void_note = $4, updated_at = now() \
         WHERE id = $1 AND status = 'open'",
    )
    .bind(*id)
    .bind(actor.teller_id)
    .bind(body.reason.map(VoidReason::as_str))
    .bind(body.note.as_deref())
    .execute(&mut *tx)
    .await?;
    if voided.rows_affected() == 0 {
        // Settled or voided between the read above and this write. Report it
        // the way the read would have: idempotent for a void, a conflict for a
        // settle.
        tx.rollback().await?;
        let current = open_ticket_view(pool.get_ref(), *id).await?;
        return match current.as_ref().map(|v| v.status.as_str()) {
            Some("voided") => Ok(HttpResponse::Ok().json(current)),
            _ => Err(AppError::Conflict("Cannot void a settled ticket".into())),
        };
    }
    // Every round's kitchen ticket is voided with the bill and leaves the KDS.
    let closed_kitchen = crate::kitchen::close_kitchen_tickets(
        &mut tx,
        crate::kitchen::KitchenSourceRef::OpenTicket(*id),
        crate::kitchen::CloseReason::Voided,
        Some(actor.teller_id),
    )
    .await?;
    // The party left the floor: end its occupancy and drop its transfer wish.
    //
    // Nothing to bus. A void is not a checkout -- nobody ate and left plates
    // behind, so the table is genuinely ready for the next party. Only a
    // settle buses (see `settle_open_ticket_inner`).
    let hand = crate::floor_ops::Hand::of(&mut *tx, actor.teller_id, view.branch_id).await?;
    let freed_table = crate::floor_ops::end_ticket_occupancy(
        &mut *tx,
        *id,
        crate::floor_ops::EndReason::Voided,
        false,
        &hand,
    )
    .await?;
    let cancelled = crate::floor_ops::cancel_waiting_transfers(&mut tx, *id).await?;
    // A booked party whose only bill was torn up did not eat under their
    // booking: it is `cancelled`, by the system, in this same transaction.
    // The rule (and the "unless a sibling bill is still open or paid" clause)
    // lives with the bookings, in the one query the nightly sweep also runs.
    let cancelled_bookings =
        crate::bookings::handlers::cancel_for_voided_tickets(&mut *tx, Some(*id)).await?;
    tx.commit().await?;
    let view = open_ticket_view(pool.get_ref(), *id).await?;
    if let Some(hub) = hub
        && let Some(v) = &view
    {
        hub.publish(
            v.branch_id,
            BranchEvent::new(Topic::Tickets, "ticket.voided", v),
        );
        for kt in closed_kitchen {
            crate::kitchen::publish_kitchen(pool.get_ref(), hub, v.branch_id, "kitchen.voided", kt)
                .await;
        }
        if let Some(t) = freed_table {
            publish_table_status(pool.get_ref(), hub, v.branch_id, t).await;
        }
        for b in cancelled_bookings {
            crate::bookings::publish_booking(pool.get_ref(), hub, "booking.changed", b).await;
        }
        for tid in cancelled {
            hub.publish(
                v.branch_id,
                BranchEvent::new(
                    Topic::Floor,
                    "transfer.changed",
                    &serde_json::json!({ "branch_id": v.branch_id, "id": tid, "status": "cancelled" }),
                ),
            );
        }
    }
    Ok(HttpResponse::Ok().json(view))
}

// ── Void one line ─────────────────────────────────────────────

/// Take ONE line off an open bill. Reuses the ticket void's request shape —
/// the same reason enum and note, so a report counts a sent-back plate beside
/// a torn-up bill without translating between two vocabularies.
pub type VoidTicketLineRequest = VoidOpenTicketRequest;

#[utoipa::path(post, path = "/open-tickets/{id}/items/{item_id}/void", tag = "open_tickets",
    request_body = VoidOpenTicketRequest,
    params(
        ("id" = Uuid, Path, description = "Open ticket ID"),
        ("item_id" = Uuid, Path, description = "Bill line ID"),
    ),
    responses((status = 200, body = OpenTicketView), AppErrorResponse), security(("bearer_jwt" = [])))]
pub async fn void_ticket_line(
    req: HttpRequest,
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    path: web::Path<(Uuid, Uuid)>,
    body: web::Json<VoidOpenTicketRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    // The same rung as tearing the whole bill up: both take money off a bill
    // nobody has paid yet. The seeder has said so since the columns landed.
    check_permission(pool.get_ref(), &claims, "open_tickets", "delete").await?;
    let (id, item_id) = path.into_inner();
    require_ticket_branch_access(pool.get_ref(), &claims, id).await?;
    void_ticket_line_inner(
        pool.clone(),
        id,
        item_id,
        body,
        ActingContext::live(&claims)?,
        Some(hub.get_ref()),
    )
    .await
}

/// Line-void core. "Take the calamari off" — the party changed their mind, or
/// it came back. The line leaves the bill, the running subtotal drops by
/// exactly what it added, and the plate comes off the board so the kitchen
/// stops making it.
///
/// Shared by the live route and `/sync/replay`, attributed to `actor`, and
/// idempotent: voiding a voided line returns the bill unchanged rather than
/// re-stamping it or subtracting the money twice. That second part is the one
/// that matters — a retried drain that took the line off the subtotal again
/// would leave the bill short by the price of a plate, and nothing downstream
/// would notice until someone counted the drawer.
///
/// LIVE requires a reason, and a note when the reason is `other`; REPLAY is
/// recorded history and takes what the till queued, exactly as a ticket void
/// does.
pub(crate) async fn void_ticket_line_inner(
    pool: crate::db::Db,
    id: Uuid,
    item_id: Uuid,
    body: web::Json<VoidOpenTicketRequest>,
    actor: ActingContext,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    let view = open_ticket_view(pool.get_ref(), id)
        .await?
        .ok_or_else(|| AppError::NotFound("Open ticket not found".into()))?;
    // A settled bill's lines are an ORDER's now; a voided bill has no lines
    // left to take anything off.
    match view.status.as_str() {
        "settled" => {
            return Err(AppError::Conflict(
                "That bill is settled — a paid line is a refund, not a void".into(),
            ));
        }
        "voided" => return Err(AppError::Conflict("That bill was voided".into())),
        _ => {}
    }
    if !actor.replay {
        if body.reason.is_none() {
            return Err(AppError::BadRequest("A void needs a reason".into()));
        }
        if body.reason == Some(VoidReason::Other) && body.note.is_none() {
            return Err(AppError::BadRequest(
                "A note is required when the void reason is 'other'".into(),
            ));
        }
    }

    let mut tx = pool.get_ref().begin().await?;
    // Lock the line and read what it is worth IN THE SAME STATEMENT that finds
    // it still live. Two tills voiding the same line race here otherwise, and
    // the loser would subtract a second time.
    let line: Option<(i32,)> = sqlx::query_as(
        "SELECT line_total FROM open_ticket_items \
         WHERE id = $1 AND open_ticket_id = $2 AND voided_at IS NULL FOR UPDATE",
    )
    .bind(item_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((line_total,)) = line else {
        tx.rollback().await?;
        // Either it is not this bill's line, or it is already void. The second
        // is a lost-ack retry and must read as success.
        let known: Option<bool> = sqlx::query_scalar(
            "SELECT voided_at IS NOT NULL FROM open_ticket_items \
             WHERE id = $1 AND open_ticket_id = $2",
        )
        .bind(item_id)
        .bind(id)
        .fetch_optional(pool.get_ref())
        .await?;
        return match known {
            Some(true) => Ok(HttpResponse::Ok().json(open_ticket_view(pool.get_ref(), id).await?)),
            _ => Err(AppError::NotFound("That line is not on this bill".into())),
        };
    };

    sqlx::query(
        "UPDATE open_ticket_items \
            SET voided_at = now(), voided_by = $2, void_reason = $3::void_reason, void_note = $4 \
          WHERE id = $1",
    )
    .bind(item_id)
    .bind(actor.teller_id)
    .bind(body.reason.map(VoidReason::as_str))
    .bind(body.note.as_deref())
    .execute(&mut *tx)
    .await?;

    // The bill's running subtotal is a column, not a sum over the lines, so it
    // has to be told. Exactly what this line added, never a recomputation:
    // re-summing would silently re-price a bill under today's menu.
    sqlx::query(
        "UPDATE open_tickets SET subtotal = subtotal - $2, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(line_total)
    .execute(&mut *tx)
    .await?;

    // And off the board, so nobody cooks it. Only lines still live: a plate
    // the kitchen already bumped is made, and taking a finished line off a
    // screen tells the cook nothing they can act on.
    //
    // A round fired before the link column existed has no kitchen row to find
    // (see the migration). The money still comes off the bill — the honest
    // half — and the board keeps a plate someone has to call off by voice,
    // which is what happened before this feature existed anyway.
    let kitchen_ids: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE kitchen_ticket_items SET voided_at = now() \
          WHERE open_ticket_item_id = $1 AND voided_at IS NULL AND bumped_at IS NULL \
        RETURNING kitchen_ticket_id",
    )
    .bind(item_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let view = open_ticket_view(pool.get_ref(), id).await?;
    if let Some(hub) = hub
        && let Some(v) = &view
    {
        hub.publish(
            v.branch_id,
            BranchEvent::new(Topic::Tickets, "ticket.changed", v),
        );
        for kt in kitchen_ids {
            crate::kitchen::publish_kitchen(
                pool.get_ref(),
                hub,
                v.branch_id,
                "kitchen.changed",
                kt,
            )
            .await;
        }
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
    pool: crate::db::Db,
    hub: web::Data<BranchEventHub>,
    id: web::Path<Uuid>,
    body: web::Json<MoveTicketTableRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "open_tickets", "update").await?;
    require_ticket_branch_access(pool.get_ref(), &claims, *id).await?;

    let row: Option<(Uuid, String)> =
        sqlx::query_as("SELECT branch_id, status::text FROM open_tickets WHERE id = $1")
            .bind(*id)
            .fetch_optional(pool.get_ref())
            .await?;
    let (branch_id, status) =
        row.ok_or_else(|| AppError::NotFound("Open ticket not found".into()))?;
    if status != "open" {
        return Err(AppError::Conflict(
            "Cannot move a settled or voided ticket".into(),
        ));
    }
    let mut tx = pool.get_ref().begin().await?;
    // Shared arbitration: lock the target (the per-table mutex) and reject an
    // occupied one — this is the INTERACTIVE path, so unlike a queued fire it
    // fails loudly and the waiter picks another table (or uses the swap op).
    if !crate::floor_ops::lock_table(&mut tx, body.table_id, branch_id).await? {
        return Err(AppError::BadRequest(
            "Target table is not in this branch".into(),
        ));
    }
    if crate::floor_ops::occupant_of(&mut tx, body.table_id, Some(*id))
        .await?
        .is_some()
    {
        return Err(AppError::Conflict("Table is already occupied".into()));
    }
    // The old table's row ends `moved`, a row opens on the new one, and the
    // move may be exactly what this party's transfer wish asked for.
    let old_table: Option<Uuid> = sqlx::query_scalar(
        "SELECT table_id FROM table_occupancies WHERE open_ticket_id = $1 AND ended_at IS NULL",
    )
    .bind(*id)
    .fetch_optional(&mut *tx)
    .await?;
    let hand = crate::floor_ops::Hand::of(&mut *tx, claims.user_id(), branch_id).await?;
    let fulfilled =
        crate::floor_ops::relocate_ticket(&mut tx, *id, Some(body.table_id), &hand).await?;
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
                Topic::Floor,
                "table.status_changed",
                &serde_json::json!({ "branch_id": branch_id, "table_id": body.table_id }),
            ),
        );
        publish_table_status(pool.get_ref(), hub.get_ref(), branch_id, body.table_id).await;
        if let Some(old) = old_table
            && old != body.table_id
        {
            publish_table_status(pool.get_ref(), hub.get_ref(), branch_id, old).await;
        }
        for tid in fulfilled {
            hub.publish(
                branch_id,
                BranchEvent::new(
                    Topic::Floor,
                    "transfer.changed",
                    &serde_json::json!({ "branch_id": branch_id, "id": tid, "status": "fulfilled" }),
                ),
            );
        }
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
    pool: crate::db::Db,
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
/// paid `dine_in` order via `create_order_inner`, which writes BOTH sides of
/// the ticket↔order link in the order's own transaction (see `SettledTicket`),
/// landing it in the SETTLING cashier's open shift. The ticket id doubles as
/// the order idempotency key so a retried/concurrent/replayed settle dedups to
/// one order; the LINK is the explicit `orders.open_ticket_id`, not that
/// convention. Shared by the live route and `/sync/replay` (a queued offline
/// settle).
pub(crate) async fn settle_open_ticket_inner(
    pool: crate::db::Db,
    id: Uuid,
    body: web::Json<SettleOpenTicketRequest>,
    actor: ActingContext,
    hub: Option<&BranchEventHub>,
) -> Result<HttpResponse, AppError> {
    let id = &id;
    #[allow(clippy::type_complexity)]
    let row: Option<(
        Uuid,
        Uuid,
        String,
        Option<Uuid>,
        Option<String>,
        Option<String>,
        Option<Uuid>,
        Option<String>,
        Option<rust_decimal::Decimal>,
        Uuid,
    )> = sqlx::query_as(
        "SELECT branch_id, org_id, status::text, order_id, customer_name, notes, \
                    discount_id, discount_type, discount_value, opened_by \
             FROM open_tickets WHERE id = $1",
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
        opened_by,
    )) = row
    else {
        return Err(AppError::NotFound("Open ticket not found".into()));
    };
    if status == "voided" {
        return Err(AppError::Conflict("Cannot settle a voided ticket".into()));
    }
    if status == "settled" || order_id.is_some() {
        // Already settled. A REPLAYED (lost-ack) settle is idempotent — return the
        // existing paid order, found by the link. A LIVE double-settle (two
        // cashiers racing the same ticket) is a clean conflict.
        if actor.replay
            && let Some(order) =
                crate::orders::handlers::fetch_order_by_open_ticket(pool.get_ref(), *id, org_id)
                    .await?
        {
            return Ok(HttpResponse::Ok().json(order));
        }
        return Err(AppError::Conflict("Ticket is already settled".into()));
    }

    // Replay the stored client-priced items back through the POS create-order
    // path. The id comes along so a reward can name a LINE rather than guess a
    // position: this ORDER BY is what decides the positions, and it lives here,
    // not on the till.
    let line_rows: Vec<(Uuid, serde_json::Value)> = sqlx::query_as(
        "SELECT oti.id, oti.line FROM open_ticket_items oti \
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
    let mut line_positions: std::collections::HashMap<Uuid, usize> =
        std::collections::HashMap::with_capacity(line_rows.len());
    for (line_id, line_json) in line_rows {
        line_positions.insert(line_id, items.len());
        let stored: super::StoredTicketLine =
            serde_json::from_value(line_json).map_err(|_| AppError::Internal)?;
        let input: OrderItemInput =
            serde_json::from_value(stored.input).map_err(|_| AppError::Internal)?;
        items.push(input);
    }

    // Translate each reward's LINE into the position that line ended up at.
    // A settle must name lines by id: the till cannot see the order the rounds
    // flatten into, and a guessed index takes the wrong item off the bill.
    let mut redemptions = body.loyalty_redemptions.clone();
    for r in &mut redemptions {
        let Some(line_id) = r.ticket_line_id else {
            return Err(AppError::BadRequest(
                "A reward on a ticket must name the line it covers".into(),
            ));
        };
        r.item_index = Some(*line_positions.get(&line_id).ok_or_else(|| {
            // A voided line, or one from another ticket. Either way the reward
            // has nothing to cover, and guessing would give something away.
            AppError::BadRequest("That line is not on this ticket".into())
        })?);
    }

    // The discount, explicitly. The cashier says nothing → the waiter's ticket
    // discount is inherited (and the ticket view shows it, so "nothing" is a
    // choice). `discount_type: "none"` → no discount, whatever the waiter set.
    // Anything else the cashier sends replaces the waiter's outright — never a
    // field-by-field merge, which is how a settle used to end up with the
    // waiter's `discount_type` under the cashier's `discount_value`.
    let cashier_spoke =
        body.discount_id.is_some() || body.discount_type.is_some() || body.discount_value.is_some();
    let (discount_id, discount_type, discount_value) =
        if body.discount_type.as_deref() == Some(DISCOUNT_NONE) {
            (None, None, None)
        } else if cashier_spoke {
            (
                body.discount_id,
                body.discount_type.clone(),
                body.discount_value,
            )
        } else {
            (t_disc_id, t_disc_type, t_disc_value)
        };

    // Build a POS order request. The TICKET ID is the order idempotency key, so a
    // retried/concurrent settle dedups to one paid order. `create_order_inner`
    // enforces the cashier's open shift, validates the payment method, computes
    // deductions/inventory/tax, refuses a total the till disagrees with, and
    // lands the sale in the cashier's drawer.
    let request = CreateOrderRequest {
        branch_id,
        loyalty_customer_id: body.loyalty_customer_id,
        loyalty_redemptions: redemptions,
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
        // Split tenders ride to the order's payment legs, as a counter sale's
        // do; the legs are what the drawer maths sums, so dropping them here
        // used to book a half-cash bill as all cash.
        payment_splits: body.payment_splits.clone(),
        items,
        // The till's clock, or now. `create_order_inner` refuses a future one
        // and stamps the same instant on the ticket's `settled_at`.
        created_at: Some(body.settled_at.unwrap_or_else(chrono::Utc::now)),
        subtotal: None,
        discount_amount: None,
        tax_amount: None,
        // The till's figure, through the same drift check a counter checkout
        // gets. A drawer that collected the ticket SUBTOTAL for a bill the
        // server prices with tax on top is refused here, not discovered at
        // shift close.
        total_amount: body.total_amount,
        change_given: body.change_given,
        idempotency_key: Some(*id),
        order_number: None,
        order_ref: None,
    };

    // hub = None → don't re-fire the kitchen (the items already fired at order
    // time). The ticket rides along so the order is stamped `dine_in`, carries
    // the WAITER (opened_by) the dashboard segments by, links both ways, ends
    // the party's table, drops their transfer wish and completes their booking
    // — all in the order's own transaction. What the floor did comes back in
    // `ticket.floor`; on the idempotency shortcut (a concurrent settle already
    // committed this order) it stays empty, because that settle did the work.
    let mut ticket = SettledTicket {
        open_ticket_id: *id,
        waiter_id: opened_by,
        floor: Default::default(),
    };
    create_order_inner(
        pool.clone(),
        web::Json(request),
        actor,
        None,
        Some(&mut ticket),
    )
    .await?;
    let floor = ticket.floor;

    let created = crate::orders::handlers::fetch_order_by_open_ticket(pool.get_ref(), *id, org_id)
        .await?
        .ok_or(AppError::Internal)?;

    if let Some(hub) = hub {
        if let Ok(Some(view)) = open_ticket_view(pool.get_ref(), *id).await {
            hub.publish(
                branch_id,
                BranchEvent::new(Topic::Tickets, "ticket.settled", &view),
            );
        }
        // The party CHECKED OUT, so the table reads `dirty` — it still holds
        // their plates. A human clears it: the POS prompts the teller right
        // after the sale, and the tables screen keeps a one-tap clear.
        if let Some(t) = floor.freed_table {
            publish_table_status(pool.get_ref(), hub, branch_id, t).await;
        }
        if let Some(b) = floor.completed_booking {
            crate::bookings::publish_booking(pool.get_ref(), hub, "booking.changed", b).await;
        }
        for tid in floor.cancelled_transfers {
            hub.publish(
                branch_id,
                BranchEvent::new(
                    Topic::Floor,
                    "transfer.changed",
                    &serde_json::json!({ "branch_id": branch_id, "id": tid, "status": "cancelled" }),
                ),
            );
        }
    }
    Ok(HttpResponse::Ok().json(created))
}
