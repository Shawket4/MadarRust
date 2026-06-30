//! Staff delivery API (JWT, branch-scoped): the queue, status transitions,
//! finalize (snapshot → real sale), and cancel (with optional waste).

use std::time::Duration;

use actix_web::web::Bytes;
use actix_web::{HttpRequest, HttpResponse, web};
use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tokio_stream::wrappers::{BroadcastStream, IntervalStream};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::snapshot::{self, CartSnapshot, FinalizeCtx, SnapshotDeduction};
use super::whatsapp;
use super::{extract_claims, require_branch_access};
use crate::errors::{AppError, AppErrorResponse};
use crate::permissions::checker::check_permission;
use crate::realtime::event::{BranchEvent, Topic};
use crate::realtime::hub::BranchEventHub;

// ── Read model ────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize, ToSchema, sqlx::FromRow)]
pub struct DeliveryOrder {
    pub id: Uuid,
    pub org_id: Uuid,
    pub branch_id: Uuid,
    pub channel: String,
    pub status: String,
    pub delivery_ref: Option<String>,
    pub customer_name: String,
    pub customer_phone: String,
    pub place_name: Option<String>,
    pub floor: Option<String>,
    pub unit_number: Option<String>,
    pub landmark: Option<String>,
    pub address_line: Option<String>,
    pub delivery_notes: Option<String>,
    pub customer_lat: Option<f64>,
    pub customer_lng: Option<f64>,
    pub delivery_zone_id: Option<Uuid>,
    pub road_distance_meters: Option<i32>,
    pub subtotal: i32,
    pub delivery_fee: i32,
    pub total: i32,
    /// Frozen channel discount on the item subtotal (`total == subtotal -
    /// discount_amount + delivery_fee`). `discount_amount` is 0 when none.
    pub discount_id: Option<Uuid>,
    pub discount_type: Option<String>,
    #[serde(default)]
    pub discount_value: i32,
    #[serde(default)]
    pub discount_amount: i32,
    /// Extra prep minutes the teller added on top of the branch base (multiples of 5).
    pub extra_prep_minutes: i32,
    /// The frozen priced line snapshot the POS renders before finalize.
    #[schema(value_type = Object)]
    pub cart: serde_json::Value,
    pub payment_method_hint: Option<String>,
    pub otp_verified: bool,
    pub order_id: Option<Uuid>,
    pub receipt_printed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub confirmed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub preparing_at: Option<chrono::DateTime<chrono::Utc>>,
    pub ready_at: Option<chrono::DateTime<chrono::Utc>>,
    pub out_for_delivery_at: Option<chrono::DateTime<chrono::Utc>>,
    pub delivered_at: Option<chrono::DateTime<chrono::Utc>>,
    pub cancelled_at: Option<chrono::DateTime<chrono::Utc>>,
    pub rejected_at: Option<chrono::DateTime<chrono::Utc>>,
    pub cancel_reason: Option<String>,
    pub cancel_restocked: Option<bool>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

const DO_SELECT: &str = "SELECT id, org_id, branch_id, channel::text, status::text, delivery_ref, \
    customer_name, customer_phone, place_name, floor, unit_number, landmark, address_line, \
    delivery_notes, customer_lat, customer_lng, delivery_zone_id, road_distance_meters, \
    subtotal, delivery_fee, total, discount_id, discount_type::text, discount_value, discount_amount, \
    extra_prep_minutes, cart, payment_method_hint, otp_verified, order_id, \
    receipt_printed_at, confirmed_at, preparing_at, ready_at, out_for_delivery_at, delivered_at, \
    cancelled_at, rejected_at, cancel_reason, cancel_restocked, created_at, updated_at \
    FROM delivery_orders";

pub async fn fetch_delivery_order(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<DeliveryOrder>, AppError> {
    Ok(sqlx::query_as(&format!("{DO_SELECT} WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

pub async fn fetch_delivery_order_by_idem(
    pool: &PgPool,
    key: Uuid,
) -> Result<Option<DeliveryOrder>, AppError> {
    Ok(
        sqlx::query_as(&format!("{DO_SELECT} WHERE idempotency_key = $1"))
            .bind(key)
            .fetch_optional(pool)
            .await?,
    )
}

// ── List / get ────────────────────────────────────────────────

#[derive(Deserialize, IntoParams)]
pub struct ListQuery {
    pub branch_id: Uuid,
    /// Comma-separated statuses to include (default: all).
    pub status: Option<String>,
    pub limit: Option<i64>,
}

#[utoipa::path(
    get, path = "/delivery-orders", tag = "delivery", params(ListQuery),
    responses((status = 200, body = [DeliveryOrder]), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_delivery_orders(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<ListQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "delivery_orders", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    let statuses: Option<Vec<String>> = query.status.as_ref().map(|s| {
        s.split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect()
    });
    let limit = query.limit.unwrap_or(200).clamp(1, 1000);

    let orders: Vec<DeliveryOrder> = sqlx::query_as(&format!(
        "{DO_SELECT} WHERE branch_id = $1 \
         AND ($2::text[] IS NULL OR status::text = ANY($2)) \
         ORDER BY created_at DESC LIMIT $3"
    ))
    .bind(query.branch_id)
    .bind(statuses)
    .bind(limit)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(orders))
}

#[utoipa::path(
    get, path = "/delivery-orders/{id}", tag = "delivery",
    responses((status = 200, body = DeliveryOrder), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_delivery_order(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "delivery_orders", "read").await?;
    let order = fetch_delivery_order(pool.get_ref(), path.into_inner())
        .await?
        .ok_or_else(|| AppError::NotFound("Delivery order not found".into()))?;
    require_branch_access(pool.get_ref(), &claims, order.branch_id).await?;
    Ok(HttpResponse::Ok().json(order))
}

// ── Live stream (SSE) ─────────────────────────────────────────

#[derive(Deserialize, IntoParams)]
pub struct StreamQuery {
    pub branch_id: Uuid,
}

/// Server-Sent Events stream of delivery-order changes for one branch. Auth is
/// the same Bearer + `delivery_orders:read` + branch-access trio as the list
/// endpoint, enforced before the stream opens. The stream is **updates-only**:
/// the client should `GET /delivery-orders` first to seed the list, then connect.
/// On any error/disconnect the client re-GETs and reconnects.
#[utoipa::path(
    get, path = "/delivery-orders/stream", tag = "delivery", params(StreamQuery),
    responses(
        (status = 200, content_type = "text/event-stream",
         description = "DEPRECATED delivery-only view of the unified bus — prefer \
            GET /realtime/stream?topics=delivery. Each event is `event: delivery.created|\
            delivery.updated` + a `data:` DeliveryOrder JSON line. `: ping` every ~20s. On \
            ANY error/close, re-GET /delivery-orders and reconnect."),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn stream_delivery_orders(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    hub: web::Data<BranchEventHub>,
    query: web::Query<StreamQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "delivery_orders", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    let rx = hub.subscribe(query.branch_id);

    // The unified bus carries every topic — keep only Delivery events for this
    // backward-compatible endpoint. A lagged/closed receiver yields `Err`, surfaced
    // as a body error so actix drops the connection; the POS reconnects + re-GETs.
    let events = BroadcastStream::new(rx).filter_map(|res| {
        let out: Option<Result<Bytes, actix_web::Error>> = match res {
            Ok(ev) if ev.topic == Topic::Delivery => Some(Ok(Bytes::from(format!(
                "event: {}\ndata: {}\n\n",
                ev.event_type, ev.data
            )))),
            Ok(_) => None,
            Err(_) => Some(Err(actix_web::error::ErrorInternalServerError(
                "delivery stream lagged",
            ))),
        };
        futures::future::ready(out)
    });

    // Keep-alive comment ticks so idle connections survive proxy timeouts and a
    // dead peer is detected on the next failed write.
    let keepalive = IntervalStream::new(tokio::time::interval(Duration::from_secs(20)))
        .map(|_| Ok::<Bytes, actix_web::Error>(Bytes::from_static(b": ping\n\n")));

    let body = futures::stream::select(events, keepalive);

    Ok(HttpResponse::Ok()
        .content_type("text/event-stream")
        .insert_header(("Cache-Control", "no-cache"))
        // Opt out of the app-wide Compress middleware: a streaming compressor
        // buffers small SSE frames and stalls events until it flushes. Setting
        // Content-Encoding makes Compress skip this response entirely. Also tell
        // nginx not to buffer (X-Accel-Buffering).
        .insert_header((actix_web::http::header::CONTENT_ENCODING, "identity"))
        .insert_header(("X-Accel-Buffering", "no"))
        .streaming(body))
}

// ── Status transitions ────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct StatusInput {
    /// Target line step: "confirmed" | "preparing" | "ready" | "out_for_delivery".
    /// The teller may jump to ANY of these from any non-terminal state (forward or
    /// back); the landed step is stamped and all other step stamps are cleared, and
    /// at most one customer WhatsApp fires (the last newly-crossed step that has one).
    pub status: String,
}

/// The delivery line, in order. `received` is the intake default (not settable
/// via this endpoint); `delivered` is reached only via finalize; `cancelled`/
/// `rejected` via the cancel endpoint.
const STEP_ORDER: [&str; 5] = [
    "received",
    "confirmed",
    "preparing",
    "ready",
    "out_for_delivery",
];

/// Position of a status on the forward line, or `None` for terminal states
/// (delivered/cancelled/rejected) that have left it.
fn step_index(status: &str) -> Option<usize> {
    STEP_ORDER.iter().position(|s| *s == status)
}

/// The customer WhatsApp message a step fires, if any. Single source of truth for
/// the jump notifier: `confirmed` (order accepted) and `out_for_delivery` notify,
/// each carrying the tracking link. Adding a message here for another step is all
/// the jump logic needs to honour it.
fn step_whatsapp_message(status: &str, delivery_ref: &str, order_id: Uuid) -> Option<String> {
    match status {
        "confirmed" => Some(whatsapp::build_order_accepted_message(
            delivery_ref,
            order_id,
        )),
        "out_for_delivery" => Some(whatsapp::build_out_for_delivery_message(
            delivery_ref,
            order_id,
        )),
        _ => None,
    }
}

/// The single WhatsApp message a jump from `prev_idx` → `target_idx` should send:
/// the highest-indexed newly-crossed step (`prev_idx < idx <= target_idx`) that
/// has a template. If the landed step has none, this falls back to the last
/// crossed step that does; a backward / no-op jump crosses nothing new → `None`.
fn jump_whatsapp_message(
    prev_idx: usize,
    target_idx: usize,
    delivery_ref: &str,
    order_id: Uuid,
) -> Option<String> {
    if target_idx <= prev_idx {
        return None;
    }
    STEP_ORDER[prev_idx + 1..=target_idx]
        .iter()
        .rev()
        .find_map(|&step| step_whatsapp_message(step, delivery_ref, order_id))
}

#[utoipa::path(
    post, path = "/delivery-orders/{id}/status", tag = "delivery", request_body = StatusInput,
    responses((status = 200, body = DeliveryOrder), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn set_status(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    hub: web::Data<BranchEventHub>,
    path: web::Path<Uuid>,
    body: web::Json<StatusInput>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "delivery_orders", "update").await?;
    let id = path.into_inner();
    let order = fetch_delivery_order(pool.get_ref(), id)
        .await?
        .ok_or_else(|| AppError::NotFound("Delivery order not found".into()))?;
    require_branch_access(pool.get_ref(), &claims, order.branch_id).await?;

    // Target must be a settable line step (`received` is the intake default;
    // delivered/cancel/reject have their own endpoints).
    let Some(target_idx) = step_index(&body.status).filter(|&i| i > 0) else {
        return Err(AppError::BadRequest(format!(
            "'{}' is not a settable delivery step",
            body.status
        )));
    };
    // The order must still be on the line (not finalized/cancelled/rejected).
    let Some(prev_idx) = step_index(&order.status) else {
        return Err(AppError::Conflict(format!(
            "Order is {}; its status can no longer change",
            order.status
        )));
    };

    // Jump to any step (forward or backward): stamp the landed step and CLEAR
    // every other step stamp, so the recorded position is exactly the landed
    // step. The print-once guard (receipt_printed_at) is preserved, and set the
    // first time the order lands on `confirmed`.
    sqlx::query(
        "UPDATE delivery_orders SET
            status              = $2::delivery_order_status,
            confirmed_at        = CASE WHEN $2 = 'confirmed'        THEN now() ELSE NULL END,
            preparing_at        = CASE WHEN $2 = 'preparing'        THEN now() ELSE NULL END,
            ready_at            = CASE WHEN $2 = 'ready'            THEN now() ELSE NULL END,
            out_for_delivery_at = CASE WHEN $2 = 'out_for_delivery' THEN now() ELSE NULL END,
            receipt_printed_at  = COALESCE(receipt_printed_at, CASE WHEN $2 = 'confirmed' THEN now() ELSE NULL END),
            updated_at          = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(&body.status)
    .execute(pool.get_ref())
    .await?;

    // Send EXACTLY one WhatsApp — see jump_whatsapp_message.
    if let Some(ref dref) = order.delivery_ref
        && let Some(msg) = jump_whatsapp_message(prev_idx, target_idx, dref, id)
    {
        whatsapp::send_message(pool.get_ref().clone(), order.customer_phone.clone(), msg);
    }

    let updated = fetch_delivery_order(pool.get_ref(), id)
        .await?
        .ok_or(AppError::Internal)?;
    hub.publish(
        updated.branch_id,
        BranchEvent::new(Topic::Delivery, "delivery.updated", &updated),
    );
    Ok(HttpResponse::Ok().json(updated))
}

// ── Cancel / reject (with optional waste) ─────────────────────

#[derive(Deserialize, ToSchema)]
pub struct CancelInput {
    pub reason: Option<String>,
    /// true (default): ingredients stay available. false: the food was made and is
    /// wasted — the frozen plan is deducted from stock and logged as `waste`.
    #[serde(default = "default_true")]
    pub restore_inventory: bool,
}

fn default_true() -> bool {
    true
}

#[utoipa::path(
    post, path = "/delivery-orders/{id}/cancel", tag = "delivery", request_body = CancelInput,
    responses((status = 200, body = DeliveryOrder), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn cancel_delivery_order(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    hub: web::Data<BranchEventHub>,
    path: web::Path<Uuid>,
    body: web::Json<CancelInput>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "delivery_orders", "update").await?;
    let id = path.into_inner();
    let order = fetch_delivery_order(pool.get_ref(), id)
        .await?
        .ok_or_else(|| AppError::NotFound("Delivery order not found".into()))?;
    require_branch_access(pool.get_ref(), &claims, order.branch_id).await?;

    if matches!(
        order.status.as_str(),
        "delivered" | "cancelled" | "rejected"
    ) {
        return Err(AppError::Conflict(format!(
            "Order is already {}",
            order.status
        )));
    }

    let mut tx = pool.get_ref().begin().await?;

    // Compare-and-swap: flip the status in ONE guarded UPDATE that only matches a
    // still-non-terminal row (received → rejected; any later state → cancelled).
    // Two concurrent cancels race on this statement — exactly one matches and wins
    // (RETURNING a row); the other matches 0 rows and is rejected below, so the
    // waste is never deducted twice. (Same guarded-flip-then-act pattern as
    // void_order, which is cleaner than a separate FOR UPDATE lock.)
    let won: Option<String> = sqlx::query_scalar(
        "UPDATE delivery_orders
         SET status = CASE WHEN status = 'received' THEN 'rejected' ELSE 'cancelled' END::delivery_order_status,
             rejected_at  = CASE WHEN status =  'received' THEN now() ELSE rejected_at  END,
             cancelled_at = CASE WHEN status <> 'received' THEN now() ELSE cancelled_at END,
             cancel_reason = $2, cancel_restocked = $3, cancelled_by = $4, updated_at = now()
         WHERE id = $1 AND status NOT IN ('delivered','cancelled','rejected')
         RETURNING status::text",
    )
    .bind(id)
    .bind(&body.reason)
    .bind(body.restore_inventory)
    .bind(claims.user_id())
    .fetch_optional(&mut *tx)
    .await?;

    // 0 rows ⟹ a concurrent cancel already terminated the order: bail before any
    // waste is deducted (the winner handled it).
    if won.is_none() {
        tx.rollback().await?;
        let current = fetch_delivery_order(pool.get_ref(), id)
            .await?
            .ok_or(AppError::Internal)?;
        return Err(AppError::Conflict(format!(
            "Order is already {}",
            current.status
        )));
    }

    // restore=false ⟹ the food was made → deduct the frozen plan and log waste.
    // Only the CAS winner reaches here, so waste is deducted exactly once.
    if !body.restore_inventory {
        let deductions: serde_json::Value =
            sqlx::query_scalar("SELECT deductions_snapshot FROM delivery_orders WHERE id = $1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        let deductions: Vec<SnapshotDeduction> =
            serde_json::from_value(deductions).unwrap_or_default();
        snapshot::record_waste(&mut tx, order.branch_id, id, &deductions, claims.user_id()).await?;
    }

    tx.commit().await?;

    let updated = fetch_delivery_order(pool.get_ref(), id)
        .await?
        .ok_or(AppError::Internal)?;
    hub.publish(
        updated.branch_id,
        BranchEvent::new(Topic::Delivery, "delivery.updated", &updated),
    );
    Ok(HttpResponse::Ok().json(updated))
}

// ── Finalize: snapshot → real sale ────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct FinalizeInput {
    pub shift_id: Uuid,
    /// The actual method the customer paid (overrides the hint). Must be an org method.
    pub payment_method: String,
}

#[derive(Serialize, ToSchema)]
pub struct FinalizeResponse {
    pub delivery_order: DeliveryOrder,
    pub order_id: Uuid,
    pub order_ref: Option<String>,
    pub warnings: Vec<String>,
}

#[utoipa::path(
    post, path = "/delivery-orders/{id}/finalize", tag = "delivery", request_body = FinalizeInput,
    responses((status = 200, body = FinalizeResponse), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn finalize_delivery_order(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    hub: web::Data<BranchEventHub>,
    path: web::Path<Uuid>,
    body: web::Json<FinalizeInput>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "delivery_orders", "update").await?;
    let id = path.into_inner();
    let order = fetch_delivery_order(pool.get_ref(), id)
        .await?
        .ok_or_else(|| AppError::NotFound("Delivery order not found".into()))?;
    require_branch_access(pool.get_ref(), &claims, order.branch_id).await?;

    if order.order_id.is_some()
        || matches!(
            order.status.as_str(),
            "delivered" | "cancelled" | "rejected"
        )
    {
        return Err(AppError::Conflict(format!(
            "Cannot finalize from {}",
            order.status
        )));
    }

    // Validate payment method (and snapshot is_cash) against the org's methods.
    let is_cash: Option<bool> = sqlx::query_scalar(
        "SELECT is_cash FROM org_payment_methods WHERE org_id = $1 AND name = $2 AND is_active = true",
    )
    .bind(order.org_id)
    .bind(&body.payment_method)
    .fetch_optional(pool.get_ref())
    .await?;
    let is_cash = is_cash.ok_or_else(|| AppError::BadRequest("Unknown payment method".into()))?;

    // The finalizing teller's shift must be open at this branch (and theirs, if teller).
    let teller_match = if claims.role == crate::models::UserRole::Teller {
        Some(claims.user_id())
    } else {
        None
    };
    let shift_ok: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM shifts WHERE id = $1 AND branch_id = $2 AND status = 'open' \
         AND ($3::uuid IS NULL OR teller_id = $3))",
    )
    .bind(body.shift_id)
    .bind(order.branch_id)
    .bind(teller_match)
    .fetch_one(pool.get_ref())
    .await?;
    if !shift_ok {
        return Err(AppError::BadRequest(
            "Shift is not open, does not belong to this branch, or is not yours.".into(),
        ));
    }

    // Load the frozen snapshot.
    let (cart_json, deductions_json): (serde_json::Value, serde_json::Value) =
        sqlx::query_as("SELECT cart, deductions_snapshot FROM delivery_orders WHERE id = $1")
            .bind(id)
            .fetch_one(pool.get_ref())
            .await?;
    let cart: CartSnapshot = serde_json::from_value(cart_json).map_err(|_| AppError::Internal)?;
    let deductions: Vec<SnapshotDeduction> =
        serde_json::from_value(deductions_json).unwrap_or_default();

    let now = chrono::Utc::now();
    let mut tx = pool.get_ref().begin().await?;

    // Same per-shift advisory lock the POS create path uses (cash TOCTOU).
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
        .bind(body.shift_id.to_string())
        .execute(&mut *tx)
        .await?;
    let still_open: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM shifts WHERE id = $1 AND status = 'open')")
            .bind(body.shift_id)
            .fetch_one(&mut *tx)
            .await?;
    if !still_open {
        return Err(AppError::Conflict(
            "Shift was closed before finalize".into(),
        ));
    }

    // Guard against a concurrent finalize having already linked an order.
    let already: Option<Uuid> =
        sqlx::query_scalar("SELECT order_id FROM delivery_orders WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if already.is_some() {
        return Err(AppError::Conflict("Order is already finalized".into()));
    }

    let ctx = FinalizeCtx {
        branch_id: order.branch_id,
        shift_id: body.shift_id,
        teller_id: claims.user_id(),
        payment_method: &body.payment_method,
        is_cash,
        created_at: now,
        subtotal: order.subtotal,
        tax_amount: 0,
        delivery_fee: order.delivery_fee,
        total_amount: order.total,
        discount_id: order.discount_id,
        discount_type: order.discount_type.as_deref(),
        discount_value: order.discount_value,
        discount_amount: order.discount_amount,
        customer_name: Some(order.customer_name.as_str()),
        notes: order.delivery_notes.as_deref(),
        order_type: "delivery",
        delivery_order_id: Some(id),
    };
    let (created, warnings) =
        snapshot::apply_snapshot(&mut tx, &ctx, &cart.lines, &deductions).await?;

    sqlx::query(
        "UPDATE delivery_orders SET status = 'delivered', delivered_at = now(), order_id = $2, \
         payment_method_hint = $3, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(created.id)
    .bind(&body.payment_method)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    if let Some(ref dref) = order.delivery_ref {
        whatsapp::send_message(
            pool.get_ref().clone(),
            order.customer_phone.clone(),
            whatsapp::build_delivered_message(dref, id),
        );
    }

    let delivery_order = fetch_delivery_order(pool.get_ref(), id)
        .await?
        .ok_or(AppError::Internal)?;
    hub.publish(
        delivery_order.branch_id,
        BranchEvent::new(Topic::Delivery, "delivery.updated", &delivery_order),
    );
    Ok(HttpResponse::Ok().json(FinalizeResponse {
        order_id: created.id,
        order_ref: created.order_ref,
        warnings,
        delivery_order,
    }))
}

// ── Prep time (teller adds to the branch base in 5-min increments) ──

#[derive(Deserialize, ToSchema)]
pub struct PrepTimeInput {
    /// Minutes the teller adds on top of the branch base prep time. Must be a
    /// non-negative multiple of 5.
    pub extra_prep_minutes: i32,
}

#[utoipa::path(
    post, path = "/delivery-orders/{id}/prep-time", tag = "delivery", request_body = PrepTimeInput,
    responses((status = 200, body = DeliveryOrder), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn set_prep_time(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    hub: web::Data<BranchEventHub>,
    path: web::Path<Uuid>,
    body: web::Json<PrepTimeInput>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "delivery_orders", "update").await?;
    let id = path.into_inner();
    let order = fetch_delivery_order(pool.get_ref(), id)
        .await?
        .ok_or_else(|| AppError::NotFound("Delivery order not found".into()))?;
    require_branch_access(pool.get_ref(), &claims, order.branch_id).await?;

    if body.extra_prep_minutes < 0 || body.extra_prep_minutes % 5 != 0 {
        return Err(AppError::BadRequest(
            "Added prep time must be a non-negative multiple of 5 minutes".into(),
        ));
    }

    sqlx::query(
        "UPDATE delivery_orders SET extra_prep_minutes = $2, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(body.extra_prep_minutes)
    .execute(pool.get_ref())
    .await?;

    let updated = fetch_delivery_order(pool.get_ref(), id)
        .await?
        .ok_or(AppError::Internal)?;
    hub.publish(
        updated.branch_id,
        BranchEvent::new(Topic::Delivery, "delivery.updated", &updated),
    );
    Ok(HttpResponse::Ok().json(updated))
}

#[cfg(test)]
mod jump_logic_tests {
    use super::{jump_whatsapp_message, step_index};
    use uuid::Uuid;

    fn i(s: &str) -> usize {
        step_index(s).unwrap()
    }

    // A fixed id stands in for the order id (only used to build the tracking
    // link). When PUBLIC_ORDER_BASE_URL is configured (e.g. via a local .env),
    // messages get a "\nTrack your order: …" suffix appended; `bare` strips it so
    // these assertions match the message's first line regardless of the env.
    const OID: Uuid = Uuid::nil();

    fn bare(msg: Option<String>) -> Option<String> {
        msg.map(|m| m.split('\n').next().unwrap_or("").to_string())
    }

    #[test]
    fn line_steps_are_ordered() {
        assert_eq!(i("received"), 0);
        assert!(i("confirmed") < i("preparing"));
        assert!(i("preparing") < i("ready"));
        assert!(i("ready") < i("out_for_delivery"));
        assert_eq!(step_index("delivered"), None);
        assert_eq!(step_index("cancelled"), None);
    }

    #[test]
    fn landing_on_out_for_delivery_notifies_once() {
        // Skipping preparing+ready still fires the single out_for_delivery msg.
        let msg = jump_whatsapp_message(i("confirmed"), i("out_for_delivery"), "D-1", OID);
        assert_eq!(bare(msg).as_deref(), Some("Your order D-1 is on the way!"));
    }

    #[test]
    fn landing_on_confirmed_sends_the_accepted_message() {
        // Accepting an order (received → confirmed) fires the accepted message.
        let msg = jump_whatsapp_message(i("received"), i("confirmed"), "D-1", OID);
        assert_eq!(
            bare(msg).as_deref(),
            Some("Your order D-1 has been accepted and is being prepared.")
        );
    }

    #[test]
    fn jumping_past_confirmed_to_a_silent_step_still_announces_acceptance() {
        // received → ready crosses confirmed (accepted) then silent preparing/ready;
        // the highest-indexed crossed step WITH a template is confirmed, so the
        // accepted message is the one that fires.
        assert_eq!(
            bare(jump_whatsapp_message(i("received"), i("ready"), "D-1", OID)).as_deref(),
            Some("Your order D-1 has been accepted and is being prepared.")
        );
    }

    #[test]
    fn jumping_past_silent_steps_still_finds_the_last_event() {
        // received → out_for_delivery: out_for_delivery is the highest crossed
        // event step and wins over the earlier confirmed message.
        assert_eq!(
            bare(jump_whatsapp_message(
                i("received"),
                i("out_for_delivery"),
                "D-1",
                OID
            ))
            .as_deref(),
            Some("Your order D-1 is on the way!")
        );
    }

    #[test]
    fn jumping_only_through_silent_steps_sends_nothing() {
        // confirmed → ready crosses only silent steps (preparing/ready).
        assert_eq!(
            jump_whatsapp_message(i("confirmed"), i("ready"), "D-1", OID),
            None
        );
    }

    #[test]
    fn backward_and_noop_jumps_notify_no_one() {
        assert_eq!(
            jump_whatsapp_message(i("out_for_delivery"), i("confirmed"), "D-1", OID),
            None
        );
        assert_eq!(
            jump_whatsapp_message(i("ready"), i("ready"), "D-1", OID),
            None
        );
    }
}
