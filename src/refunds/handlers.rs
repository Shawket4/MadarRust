use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    delivery::require_branch_access,
    errors::{AppError, AppErrorResponse},
    models::UserRole,
    permissions::checker::check_permission,
    refunds::RefundReason,
    sync::ActingContext,
};

// ── Shapes ────────────────────────────────────────────────────

/// One line of the order a refund is for. Optional detail: an overcharge or a
/// goodwill gesture is an amount with no line behind it.
#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct RefundLineInput {
    pub order_item_id: Uuid,
    /// How many of the line's units this refund is for. Held, cumulatively
    /// across every refund of the order, to what the line sold.
    pub quantity: i32,
    /// The share of the refund's amount attributed to this line, minor units.
    /// Zero is allowed (a reward line sent back for nothing). The lines of a
    /// refund may not add up to more than the refund.
    pub amount: i32,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateRefundRequest {
    /// The settled sale the money goes back against.
    pub order_id: Uuid,
    /// The shift whose drawer the money leaves. Omit for a live request and the
    /// actor's own open shift at the order's branch is used; a replayed offline
    /// refund must name the shift it was issued in, the way a queued sale does.
    #[serde(default)]
    pub shift_id: Option<Uuid>,
    /// Minor units, > 0. Together with every refund already on the order it
    /// may not exceed `orders.total_amount`.
    pub amount: i32,
    /// How the money went back — a name from the org's payment-method
    /// vocabulary. One tender per refund; a split is two refunds.
    pub method: String,
    pub reason: RefundReason,
    /// Free-text explanation. Required when `reason` is `other`.
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub lines: Vec<RefundLineInput>,
    /// When the refund was issued. Omit for live requests — the server stamps
    /// `now()`. An offline till sends the real time; future values are rejected.
    #[serde(default)]
    pub issued_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Client-minted idempotency key. A retried request or a replayed offline
    /// queue carrying the same key gets the original refund back instead of
    /// handing the money out again.
    #[serde(default)]
    pub client_ref: Option<Uuid>,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct Refund {
    pub id: Uuid,
    pub branch_id: Uuid,
    pub order_id: Uuid,
    /// The shift the refund was ISSUED in — the drawer the money left. Not
    /// necessarily the shift the order was sold in.
    pub shift_id: Uuid,
    pub amount: i32,
    pub method: String,
    /// Whether `method` meant cash when the refund was issued. Snapshotted.
    pub is_cash: bool,
    /// One of the [`RefundReason`] spellings.
    pub reason: String,
    #[serde(default)]
    pub note: Option<String>,
    pub issued_by: Uuid,
    pub issued_by_name: String,
    pub issued_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub client_ref: Option<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct RefundLine {
    pub id: Uuid,
    #[serde(skip)]
    #[schema(ignore)]
    pub refund_id: Uuid,
    pub order_item_id: Uuid,
    /// `order_items.item_name`, so a receipt reprint names the dish without a
    /// second lookup.
    pub item_name: String,
    pub quantity: i32,
    pub amount: i32,
    /// Whether the goods came back. Recorded per line; nothing in this module
    /// writes it `true` yet (see the module docs on restock).
    pub restock: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct RefundFull {
    #[serde(flatten)]
    pub refund: Refund,
    pub lines: Vec<RefundLine>,
}

/// "Everything returned" — against one order, or in one shift. Read from
/// `v_order_refund_totals` for an order and summed by `shift_id` for a shift.
#[derive(Debug, Serialize, Deserialize, Clone, Default, sqlx::FromRow, ToSchema)]
pub struct RefundTotals {
    pub refunded_amount: i64,
    /// The cash slice of `refunded_amount` — what left a drawer.
    pub refunded_cash: i64,
    pub refund_count: i64,
}

/// What `POST /refunds` returns: the refund, plus where the order now stands
/// so the till can print "fully refunded" without a second request.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct RefundIssued {
    #[serde(flatten)]
    pub refund: RefundFull,
    /// `orders.status` after this refund — `refunded` only when the cumulative
    /// amount reached the total (the trigger's rule, not this module's).
    pub order_status: String,
    #[serde(flatten)]
    pub totals: RefundTotals,
    /// `total_amount − refunded_amount`: what may still be returned.
    pub refundable_remaining: i64,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct OrderRefunds {
    pub order_id: Uuid,
    pub order_status: String,
    pub total_amount: i32,
    #[serde(flatten)]
    pub totals: RefundTotals,
    pub refundable_remaining: i64,
    pub refunds: Vec<RefundFull>,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ShiftRefunds {
    pub shift_id: Uuid,
    #[serde(flatten)]
    pub totals: RefundTotals,
    pub refunds: Vec<RefundFull>,
}

// ── POST /refunds ─────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/refunds",
    tag = "refunds",
    request_body = CreateRefundRequest,
    responses(
        (status = 201, description = "Refund issued", body = RefundIssued),
        (status = 200, description = "Replay of a refund already on record (same client_ref)", body = RefundIssued),
        AppErrorResponse
    ),
    security(("bearer_jwt" = []))
)]
pub async fn create_refund(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateRefundRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "refunds", "create").await?;
    let order = fetch_refundable_order(pool.get_ref(), body.order_id, claims.org_id()).await?;
    require_branch_access(pool.get_ref(), &claims, order.branch_id).await?;
    create_refund_inner(pool.clone(), body, ActingContext::live(&claims)?).await
}

/// The slice of an order a refund needs to know.
#[derive(sqlx::FromRow)]
struct RefundableOrder {
    id: Uuid,
    branch_id: Uuid,
    status: String,
    total_amount: i32,
}

/// Refund core. LIVE attributes `issued_by` to the JWT principal, resolves an
/// omitted shift to their own open one at the order's branch, and requires
/// the shift to be OPEN (a refund is cash leaving a drawer; a settled drawer
/// cannot lose money after the count). REPLAY attributes it to the queued
/// op's teller, requires the shift to be named, and drops the open and
/// ownership guards the way `create_order_inner` does — the refund happened
/// while the shift was open on the device and is recorded history.
/// Idempotent on `client_ref`.
///
/// Everything that follows the money — the status flip at the ceiling, the
/// loyalty clawback — is the table's trigger; this function writes the row.
pub(crate) async fn create_refund_inner(
    pool: crate::db::Db,
    body: web::Json<CreateRefundRequest>,
    actor: ActingContext,
) -> Result<HttpResponse, AppError> {
    // A retried request or a replayed queue: the original, not a second row.
    // The cumulative bound would in any case refuse a replay on a fully
    // refunded order; this is what stops one on a partially refunded order.
    if let Some(cref) = body.client_ref
        && let Some(existing) =
            fetch_refund_by_client_ref(pool.get_ref(), cref, actor.org_id).await?
    {
        return Ok(HttpResponse::Ok().json(existing));
    }

    if body.amount <= 0 {
        return Err(AppError::BadRequest(
            "A refund must return a positive amount".into(),
        ));
    }
    if body.method.trim().is_empty() {
        return Err(AppError::BadRequest(
            "Say how the money was handed back (method)".into(),
        ));
    }
    let note = body
        .note
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if body.reason == RefundReason::Other && note.is_none() {
        return Err(AppError::BadRequest(
            "A note is required when the refund reason is 'other'".into(),
        ));
    }
    let issued_at = body.issued_at.unwrap_or_else(chrono::Utc::now);
    // Offline refunds carry their real time; reject only a future device clock.
    crate::clock::reject_if_future(issued_at, "issued_at")?;

    // The lines' cross-checks (item on this order, quantity within what was
    // sold, cumulative across refunds) are the trigger's; only what can be
    // said of the request alone is said here, in words rather than a CHECK.
    let mut lines_total: i64 = 0;
    for line in &body.lines {
        if line.quantity <= 0 {
            return Err(AppError::BadRequest(
                "A refund line must name a positive quantity".into(),
            ));
        }
        if line.amount < 0 {
            return Err(AppError::BadRequest(
                "A refund line cannot carry a negative amount".into(),
            ));
        }
        lines_total += i64::from(line.amount);
    }
    if lines_total > i64::from(body.amount) {
        return Err(AppError::BadRequest(format!(
            "The lines add up to {lines_total}, more than the {} being refunded",
            body.amount
        )));
    }

    let order = fetch_refundable_order(pool.get_ref(), body.order_id, Some(actor.org_id)).await?;
    if order.status == "voided" {
        return Err(AppError::BadRequest(
            "This order was voided — a voided sale has no money to return".into(),
        ));
    }

    // The org's vocabulary, and whether the word meant cash TODAY. Snapshotted
    // onto the row so a later flip of the method's flag cannot move a closed
    // shift's drawer.
    let is_cash: bool = sqlx::query_scalar(
        "SELECT is_cash FROM org_payment_methods \
         WHERE org_id = $1 AND name = $2 AND is_active = true",
    )
    .bind(actor.org_id)
    .bind(body.method.trim())
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| {
        AppError::BadRequest(format!(
            "Invalid or inactive payment_method: {}",
            body.method.trim()
        ))
    })?;

    let shift_id = resolve_refund_shift(pool.get_ref(), &body, &order, &actor).await?;

    // Serialize against a concurrent close exactly like a sale or a cash
    // movement does: close_shift snapshots `closing_cash_system` under this
    // same per-shift advisory lock, so a refund either lands before the
    // snapshot — and is subtracted — or is refused, never silently left out
    // of a just-closed drawer.
    let mut tx = pool.get_ref().begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
        .bind(shift_id.to_string())
        .execute(&mut *tx)
        .await?;

    if !actor.replay {
        let still_open: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM shifts WHERE id = $1 AND status = 'open')",
        )
        .bind(shift_id)
        .fetch_one(&mut *tx)
        .await?;
        if !still_open {
            return Err(AppError::BadRequest(
                "Refunds can only be issued in an open shift".into(),
            ));
        }
    }

    // The bound, checked here for a message with words in it. The trigger
    // re-checks it under the same row lock — this SELECT takes it first, so
    // two refunds racing on one order queue here and the second sees the
    // first's row. A zero remainder is a conflict (the order is done), an
    // overshoot is a bad request (a smaller amount would work).
    let remaining: i64 = sqlx::query_scalar(
        "SELECT o.total_amount::bigint \
              - COALESCE((SELECT SUM(r.amount) FROM order_refunds r WHERE r.order_id = o.id), 0) \
         FROM orders o WHERE o.id = $1 FOR UPDATE",
    )
    .bind(order.id)
    .fetch_one(&mut *tx)
    .await?;
    if remaining <= 0 {
        return Err(AppError::Conflict(
            "This order has already been refunded in full".into(),
        ));
    }
    if i64::from(body.amount) > remaining {
        return Err(AppError::BadRequest(format!(
            "Refund of {} exceeds what can still be returned on this order ({remaining} of {})",
            body.amount, order.total_amount
        )));
    }

    let refund_id: Uuid = match sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO order_refunds
            (org_id, branch_id, order_id, shift_id, amount, method, is_cash,
             reason, note, issued_by, issued_at, client_ref)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        RETURNING id
        "#,
    )
    .bind(actor.org_id)
    .bind(order.branch_id)
    .bind(order.id)
    .bind(shift_id)
    .bind(body.amount)
    .bind(body.method.trim())
    .bind(is_cash)
    .bind(body.reason.as_str())
    .bind(note)
    .bind(actor.teller_id)
    .bind(issued_at)
    .bind(body.client_ref)
    .fetch_one(&mut *tx)
    .await
    {
        Ok(id) => id,
        // client_ref race: a concurrent replay of the same refund committed
        // between the lookup above and this insert. Return the original.
        Err(sqlx::Error::Database(db))
            if db.code().as_deref() == Some("23505")
                && db.constraint().is_some_and(|c| c.contains("client_ref")) =>
        {
            drop(tx);
            if let Some(cref) = body.client_ref
                && let Some(existing) =
                    fetch_refund_by_client_ref(pool.get_ref(), cref, actor.org_id).await?
            {
                return Ok(HttpResponse::Ok().json(existing));
            }
            return Err(AppError::Conflict("Duplicate refund".into()));
        }
        Err(e) => return Err(e.into()),
    };

    for line in &body.lines {
        sqlx::query(
            "INSERT INTO order_refund_lines (org_id, refund_id, order_item_id, quantity, amount) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(actor.org_id)
        .bind(refund_id)
        .bind(line.order_item_id)
        .bind(line.quantity)
        .bind(line.amount)
        .execute(&mut *tx)
        .await?;
    }

    // Read back inside the transaction: the status the AFTER INSERT trigger
    // may just have flipped is what the till should print.
    let issued = load_issued(&mut tx, refund_id).await?;
    tx.commit().await?;
    Ok(HttpResponse::Created().json(issued))
}

/// Which drawer the money leaves. Named by the request, or — live only — the
/// actor's own open shift at the order's branch. The trigger refuses a shift
/// at another branch; this says so before the insert, in words.
async fn resolve_refund_shift(
    pool: &PgPool,
    body: &CreateRefundRequest,
    order: &RefundableOrder,
    actor: &ActingContext,
) -> Result<Uuid, AppError> {
    if let Some(shift_id) = body.shift_id {
        let shift: Option<(Uuid, Uuid, String)> =
            sqlx::query_as("SELECT branch_id, teller_id, status::text FROM shifts WHERE id = $1")
                .bind(shift_id)
                .fetch_optional(pool)
                .await?;
        let Some((shift_branch, shift_teller, shift_status)) = shift else {
            return Err(AppError::NotFound("Shift not found".into()));
        };
        if shift_branch != order.branch_id {
            return Err(AppError::BadRequest(
                "The refund must be issued from a drawer at the branch that made the sale".into(),
            ));
        }
        // A teller refunds out of their OWN drawer — the refund changes that
        // drawer's expected cash, so it must belong to the right person.
        // Replay bypasses this: a different teller may be flushing the device.
        if !actor.replay && actor.role == UserRole::Teller && shift_teller != actor.teller_id {
            return Err(AppError::Forbidden(
                "You can only issue refunds from your own shift".into(),
            ));
        }
        if !actor.replay && shift_status != "open" {
            return Err(AppError::BadRequest(
                "Refunds can only be issued in an open shift".into(),
            ));
        }
        return Ok(shift_id);
    }

    if actor.replay {
        return Err(AppError::BadRequest(
            "A replayed refund must name the shift it was issued in".into(),
        ));
    }

    // `idx_shifts_one_open_per_teller`: at most one row.
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM shifts WHERE teller_id = $1 AND branch_id = $2 AND status = 'open'",
    )
    .bind(actor.teller_id)
    .bind(order.branch_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| {
        AppError::BadRequest(
            "You have no open shift at this branch — open one before issuing a refund".into(),
        )
    })
}

// ── GET /refunds/order/:order_id ──────────────────────────────

#[utoipa::path(
    get,
    path = "/refunds/order/{order_id}",
    tag = "refunds",
    params(("order_id" = Uuid, Path, description = "Order ID")),
    responses((status = 200, description = "Refunds against one order", body = OrderRefunds), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_order_refunds(
    req: HttpRequest,
    pool: crate::db::Db,
    order_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "refunds", "read").await?;
    let order = fetch_refundable_order(pool.get_ref(), *order_id, claims.org_id()).await?;
    require_branch_access(pool.get_ref(), &claims, order.branch_id).await?;

    let mut conn = pool.get_ref().acquire().await?;
    let totals = order_refund_totals(&mut conn, order.id).await?;
    let refunds = fetch_order_refunds_on(&mut conn, order.id).await?;
    Ok(HttpResponse::Ok().json(OrderRefunds {
        order_id: order.id,
        order_status: order.status,
        total_amount: order.total_amount,
        refundable_remaining: i64::from(order.total_amount) - totals.refunded_amount,
        totals,
        refunds,
    }))
}

// ── GET /refunds/shift/:shift_id ──────────────────────────────

#[utoipa::path(
    get,
    path = "/refunds/shift/{shift_id}",
    tag = "refunds",
    params(("shift_id" = Uuid, Path, description = "Shift ID")),
    responses((status = 200, description = "Refunds issued in one shift", body = ShiftRefunds), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_shift_refunds(
    req: HttpRequest,
    pool: crate::db::Db,
    shift_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "refunds", "read").await?;
    let branch_id: Uuid = sqlx::query_scalar("SELECT branch_id FROM shifts WHERE id = $1")
        .bind(*shift_id)
        .fetch_optional(pool.get_ref())
        .await?
        .ok_or_else(|| AppError::NotFound("Shift not found".into()))?;
    require_branch_access(pool.get_ref(), &claims, branch_id).await?;

    let mut conn = pool.get_ref().acquire().await?;
    let totals = shift_refund_totals(&mut conn, *shift_id).await?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM order_refunds WHERE shift_id = $1 ORDER BY issued_at DESC, created_at DESC",
    )
    .bind(*shift_id)
    .fetch_all(&mut *conn)
    .await?;
    let refunds = fetch_refunds_by_ids(&mut conn, &ids).await?;
    Ok(HttpResponse::Ok().json(ShiftRefunds {
        shift_id: *shift_id,
        totals,
        refunds,
    }))
}

// ── GET /refunds/:id ──────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/refunds/{id}",
    tag = "refunds",
    params(("id" = Uuid, Path, description = "Refund ID")),
    responses((status = 200, description = "One refund", body = RefundFull), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_refund(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "refunds", "read").await?;
    let mut conn = pool.get_ref().acquire().await?;
    let refund = fetch_refunds_by_ids(&mut conn, &[*id])
        .await?
        .pop()
        .ok_or_else(|| AppError::NotFound("Refund not found".into()))?;
    require_branch_access(pool.get_ref(), &claims, refund.refund.branch_id).await?;
    Ok(HttpResponse::Ok().json(refund))
}

// ── Figures other modules fold in ─────────────────────────────

/// The refunds against an order, oldest first — for the order detail view, so
/// a receipt reprint and the dashboard show them next to the sale.
pub async fn fetch_order_refunds(
    pool: &PgPool,
    order_id: Uuid,
) -> Result<Vec<RefundFull>, AppError> {
    let mut conn = pool.acquire().await?;
    fetch_order_refunds_on(&mut conn, order_id).await
}

/// Cash returned to customers out of THIS shift's drawer — keyed on the shift
/// the refund was issued in, not the shift the order was sold in. The drawer
/// maths (`compute_system_cash`) subtracts it; the snapshotted `is_cash` is
/// what makes the figure stable after a payment method's flag is flipped.
pub async fn shift_cash_refunds<'e, E>(executor: E, shift_id: Uuid) -> Result<i64, sqlx::Error>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(SUM(amount), 0)::bigint FROM order_refunds \
         WHERE shift_id = $1 AND is_cash",
    )
    .bind(shift_id)
    .fetch_one(executor)
    .await
}

/// Everything returned in one shift, cash and otherwise — for the shift
/// report / Z-report alongside the cash figure above.
pub async fn shift_refund_totals(
    conn: &mut PgConnection,
    shift_id: Uuid,
) -> Result<RefundTotals, AppError> {
    Ok(sqlx::query_as::<_, RefundTotals>(
        "SELECT COALESCE(SUM(amount), 0)::bigint                        AS refunded_amount, \
                COALESCE(SUM(amount) FILTER (WHERE is_cash), 0)::bigint AS refunded_cash, \
                COUNT(*)::bigint                                        AS refund_count \
         FROM order_refunds WHERE shift_id = $1",
    )
    .bind(shift_id)
    .fetch_one(&mut *conn)
    .await?)
}

/// Everything returned against one order, from `v_order_refund_totals`. All
/// zeros for an order nothing has been refunded on.
pub async fn order_refund_totals(
    conn: &mut PgConnection,
    order_id: Uuid,
) -> Result<RefundTotals, AppError> {
    Ok(sqlx::query_as::<_, RefundTotals>(
        "SELECT refunded_amount, COALESCE(refunded_cash, 0) AS refunded_cash, refund_count \
         FROM v_order_refund_totals WHERE order_id = $1",
    )
    .bind(order_id)
    .fetch_optional(&mut *conn)
    .await?
    .unwrap_or_default())
}

// ── Helpers ───────────────────────────────────────────────────

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

/// The order, org-scoped through its branch so a caller that guessed an id
/// from another tenant sees a 404 and nothing else. `None` for the org means
/// a super admin (cross-tenant by design).
async fn fetch_refundable_order(
    pool: &PgPool,
    order_id: Uuid,
    org_id: Option<Uuid>,
) -> Result<RefundableOrder, AppError> {
    sqlx::query_as::<_, RefundableOrder>(
        "SELECT o.id, o.branch_id, o.status::text AS status, o.total_amount \
         FROM orders o JOIN branches b ON b.id = o.branch_id \
         WHERE o.id = $1 AND ($2::uuid IS NULL OR b.org_id = $2)",
    )
    .bind(order_id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Order not found".into()))
}

async fn fetch_refund_by_client_ref(
    pool: &PgPool,
    client_ref: Uuid,
    org_id: Uuid,
) -> Result<Option<RefundIssued>, AppError> {
    // Org-scoped so a cross-org client_ref collision cannot echo another
    // tenant's refund (mirrors fetch_order_by_idempotency_key).
    let id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM order_refunds WHERE client_ref = $1 AND org_id = $2")
            .bind(client_ref)
            .bind(org_id)
            .fetch_optional(pool)
            .await?;
    let Some(id) = id else {
        return Ok(None);
    };
    let mut conn = pool.acquire().await?;
    Ok(Some(load_issued(&mut conn, id).await?))
}

/// The POST response for a refund on record: the row, its lines, and where
/// its order stands now.
async fn load_issued(conn: &mut PgConnection, refund_id: Uuid) -> Result<RefundIssued, AppError> {
    let refund = fetch_refunds_by_ids(conn, &[refund_id])
        .await?
        .pop()
        .ok_or_else(|| AppError::NotFound("Refund not found".into()))?;
    let (order_status, total_amount): (String, i32) =
        sqlx::query_as("SELECT status::text, total_amount FROM orders WHERE id = $1")
            .bind(refund.refund.order_id)
            .fetch_one(&mut *conn)
            .await?;
    let totals = order_refund_totals(conn, refund.refund.order_id).await?;
    Ok(RefundIssued {
        refund,
        order_status,
        refundable_remaining: i64::from(total_amount) - totals.refunded_amount,
        totals,
    })
}

async fn fetch_order_refunds_on(
    conn: &mut PgConnection,
    order_id: Uuid,
) -> Result<Vec<RefundFull>, AppError> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM order_refunds WHERE order_id = $1 ORDER BY issued_at, created_at",
    )
    .bind(order_id)
    .fetch_all(&mut *conn)
    .await?;
    fetch_refunds_by_ids(conn, &ids).await
}

/// Refunds with their lines, in the order the ids were given (the callers
/// have already sorted). One query per table, not one per refund.
async fn fetch_refunds_by_ids(
    conn: &mut PgConnection,
    ids: &[Uuid],
) -> Result<Vec<RefundFull>, AppError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let refunds = sqlx::query_as::<_, Refund>(
        r#"
        SELECT r.id, r.branch_id, r.order_id, r.shift_id, r.amount, r.method, r.is_cash,
               r.reason, r.note, r.issued_by,
               (SELECT name FROM users WHERE id = r.issued_by) AS issued_by_name,
               r.issued_at, r.client_ref, r.created_at
        FROM order_refunds r
        WHERE r.id = ANY($1)
        "#,
    )
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    let lines = sqlx::query_as::<_, RefundLine>(
        r#"
        SELECT l.id, l.refund_id, l.order_item_id, i.item_name, l.quantity, l.amount, l.restock
        FROM order_refund_lines l
        JOIN order_items i ON i.id = l.order_item_id
        WHERE l.refund_id = ANY($1)
        ORDER BY l.id
        "#,
    )
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;

    let mut by_id: std::collections::HashMap<Uuid, RefundFull> = refunds
        .into_iter()
        .map(|r| {
            (
                r.id,
                RefundFull {
                    refund: r,
                    lines: Vec::new(),
                },
            )
        })
        .collect();
    for line in lines {
        if let Some(full) = by_id.get_mut(&line.refund_id) {
            full.lines.push(line);
        }
    }
    Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
}
