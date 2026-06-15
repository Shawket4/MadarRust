//! Staff delivery API (JWT, branch-scoped): the queue, status transitions,
//! finalize (snapshot → real sale), and cancel (with optional waste).

use std::time::Duration;

use actix_web::web::Bytes;
use actix_web::{web, HttpRequest, HttpResponse};
use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tokio_stream::wrappers::{BroadcastStream, IntervalStream};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::hub::{DeliveryEvent, DeliveryHub};
use super::snapshot::{self, CartSnapshot, FinalizeCtx, SnapshotDeduction};
use super::whatsapp;
use super::{extract_claims, require_branch_access};
use crate::errors::{AppError, AppErrorResponse};
use crate::permissions::checker::check_permission;

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

pub async fn fetch_delivery_order(pool: &PgPool, id: Uuid) -> Result<Option<DeliveryOrder>, AppError> {
    Ok(sqlx::query_as(&format!("{DO_SELECT} WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

pub async fn fetch_delivery_order_by_idem(
    pool: &PgPool,
    key: Uuid,
) -> Result<Option<DeliveryOrder>, AppError> {
    Ok(sqlx::query_as(&format!("{DO_SELECT} WHERE idempotency_key = $1"))
        .bind(key)
        .fetch_optional(pool)
        .await?)
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
        s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()
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
        (status = 200, content_type = "text/event-stream", body = DeliveryEvent,
         description = "SSE stream. Each event is `event: created|updated` followed by a \
            `data:` line carrying a DeliveryOrder JSON object (identical to a \
            GET /delivery-orders item). `: ping` comment lines arrive ~every 20s as \
            keep-alive. On ANY stream error or close, re-GET /delivery-orders and reconnect."),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn stream_delivery_orders(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    hub: web::Data<DeliveryHub>,
    query: web::Query<StreamQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "delivery_orders", "read").await?;
    require_branch_access(pool.get_ref(), &claims, query.branch_id).await?;

    let rx = hub.subscribe(query.branch_id);

    // Broadcast events → SSE frames. A lagged/closed receiver yields `Err`, which
    // we turn into a body error so actix drops the connection; the POS then
    // reconnects and re-GETs to catch up on anything it missed during the lag.
    let events = BroadcastStream::new(rx).map(|res| match res {
        Ok(ev) => {
            let json = serde_json::to_string(&ev.order).unwrap_or_else(|_| "{}".into());
            Ok::<Bytes, actix_web::Error>(Bytes::from(format!(
                "event: {}\ndata: {}\n\n",
                ev.event_type, json
            )))
        }
        Err(_) => Err(actix_web::error::ErrorInternalServerError("delivery stream lagged")),
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
    /// "confirmed" | "preparing" | "ready" | "out_for_delivery"
    pub status: String,
}

/// Single forward step. Delivered is reached only via finalize; cancelled/rejected
/// via the cancel endpoint.
fn valid_forward(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("received", "confirmed")
            | ("confirmed", "preparing")
            | ("preparing", "ready")
            | ("ready", "out_for_delivery")
    )
}

#[utoipa::path(
    post, path = "/delivery-orders/{id}/status", tag = "delivery", request_body = StatusInput,
    responses((status = 200, body = DeliveryOrder), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn set_status(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    hub: web::Data<DeliveryHub>,
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

    if !valid_forward(&order.status, &body.status) {
        return Err(AppError::Conflict(format!(
            "Cannot move from {} to {}",
            order.status, body.status
        )));
    }

    // confirmed = the accept/print moment → stamp receipt_printed_at (print-once).
    let ts_col = match body.status.as_str() {
        "confirmed" => "confirmed_at",
        "preparing" => "preparing_at",
        "ready" => "ready_at",
        "out_for_delivery" => "out_for_delivery_at",
        _ => unreachable!(),
    };
    let receipt_clause = if body.status == "confirmed" {
        ", receipt_printed_at = COALESCE(receipt_printed_at, now())"
    } else {
        ""
    };

    sqlx::query(&format!(
        "UPDATE delivery_orders SET status = $2::delivery_order_status, {ts_col} = now(), updated_at = now(){receipt_clause} WHERE id = $1"
    ))
    .bind(id)
    .bind(&body.status)
    .execute(pool.get_ref())
    .await?;

    if body.status == "out_for_delivery"
        && let Some(ref dref) = order.delivery_ref
    {
        whatsapp::send_message(order.customer_phone.clone(), whatsapp::build_out_for_delivery_message(dref));
    }

    let updated = fetch_delivery_order(pool.get_ref(), id).await?.ok_or(AppError::Internal)?;
    hub.publish(
        updated.branch_id,
        DeliveryEvent { event_type: "updated".into(), order: updated.clone() },
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
    hub: web::Data<DeliveryHub>,
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

    if matches!(order.status.as_str(), "delivered" | "cancelled" | "rejected") {
        return Err(AppError::Conflict(format!("Order is already {}", order.status)));
    }

    // received → rejected; any later non-terminal state → cancelled.
    let (new_status, ts_col) = if order.status == "received" {
        ("rejected", "rejected_at")
    } else {
        ("cancelled", "cancelled_at")
    };

    let mut tx = pool.get_ref().begin().await?;

    // restore=false ⟹ the food was made → deduct the frozen plan and log waste.
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

    sqlx::query(&format!(
        "UPDATE delivery_orders SET status = $2::delivery_order_status, {ts_col} = now(), \
         cancel_reason = $3, cancel_restocked = $4, cancelled_by = $5, updated_at = now() WHERE id = $1"
    ))
    .bind(id)
    .bind(new_status)
    .bind(&body.reason)
    .bind(body.restore_inventory)
    .bind(claims.user_id())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let updated = fetch_delivery_order(pool.get_ref(), id).await?.ok_or(AppError::Internal)?;
    hub.publish(
        updated.branch_id,
        DeliveryEvent { event_type: "updated".into(), order: updated.clone() },
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
    hub: web::Data<DeliveryHub>,
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

    if order.order_id.is_some() || matches!(order.status.as_str(), "delivered" | "cancelled" | "rejected") {
        return Err(AppError::Conflict(format!("Cannot finalize from {}", order.status)));
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
    let (cart_json, deductions_json): (serde_json::Value, serde_json::Value) = sqlx::query_as(
        "SELECT cart, deductions_snapshot FROM delivery_orders WHERE id = $1",
    )
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
        return Err(AppError::Conflict("Shift was closed before finalize".into()));
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
        delivery_order_id: id,
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
        whatsapp::send_message(order.customer_phone.clone(), whatsapp::build_delivered_message(dref));
    }

    let delivery_order = fetch_delivery_order(pool.get_ref(), id).await?.ok_or(AppError::Internal)?;
    hub.publish(
        delivery_order.branch_id,
        DeliveryEvent { event_type: "updated".into(), order: delivery_order.clone() },
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
    hub: web::Data<DeliveryHub>,
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

    sqlx::query("UPDATE delivery_orders SET extra_prep_minutes = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(body.extra_prep_minutes)
        .execute(pool.get_ref())
        .await?;

    let updated = fetch_delivery_order(pool.get_ref(), id).await?.ok_or(AppError::Internal)?;
    hub.publish(
        updated.branch_id,
        DeliveryEvent { event_type: "updated".into(), order: updated.clone() },
    );
    Ok(HttpResponse::Ok().json(updated))
}
