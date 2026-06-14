use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    errors::{AppError, AppErrorResponse},
    models::UserRole,
    permissions::checker::check_permission,
};
use utoipa::ToSchema;

// ── Models ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct Shift {
    pub id:                       Uuid,
    pub branch_id:                Uuid,
    pub teller_id:                Uuid,
    pub teller_name:              String,
    pub status:                   String,
    pub opening_cash:             i32,
    pub opening_cash_original:    Option<i32>,
    pub opening_cash_was_edited:  bool,
    pub opening_cash_edit_reason: Option<String>,
    pub closing_cash_declared:    Option<i32>,
    pub closing_cash_system:      Option<i32>,
    pub cash_discrepancy:         Option<i32>,
    pub opened_at:                chrono::DateTime<chrono::Utc>,
    pub closed_at:                Option<chrono::DateTime<chrono::Utc>>,
    pub closed_by:                Option<Uuid>,
    pub force_closed_by:          Option<Uuid>,
    pub force_closed_at:          Option<chrono::DateTime<chrono::Utc>>,
    pub force_close_reason:       Option<String>,
    pub notes:                    Option<String>,
    /// Branch label — only populated by the shifts list (so the "All branches"
    /// view can show which branch each shift belongs to). Other shift endpoints
    /// leave it `null`.
    #[serde(default)]
    #[sqlx(default)]
    pub branch_name:              Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct CashMovement {
    pub id:            Uuid,
    pub shift_id:      Uuid,
    pub amount:        i32,
    pub note:          String,
    pub moved_by:      Uuid,
    pub moved_by_name: String,
    pub created_at:    chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ShiftPreFill {
    pub has_open_shift:         bool,
    pub open_shift:             Option<Shift>,
    pub suggested_opening_cash: i32,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct PaymentSummaryRow {
    pub payment_method: String,
    pub is_cash:        bool,
    pub total:          i64,
    pub order_count:    i64,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct CashMovementSummaryRow {
    pub amount:        i32,
    pub note:          String,
    pub moved_by_name: String,
    pub created_at:    chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ShiftReportResponse {
    pub shift:               Shift,
    pub payment_summary:     Vec<PaymentSummaryRow>,
    pub total_payments:      i64,
    pub voided_amount:       i64,   // informational only — not subtracted from payments
    pub net_payments:        i64,
    pub cash_movements:      Vec<CashMovementSummaryRow>,
    pub cash_movements_in:   i64,
    pub cash_movements_out:  i64,
    /// Net of all cash movements (in - out) as a signed integer
    pub cash_movements_net:  i64,
    pub printed_at:          chrono::DateTime<chrono::Utc>,
}

// ── Request types ─────────────────────────────────────────────

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct OpenShiftRequest {
    pub id:                  Option<Uuid>,
    pub opening_cash:        i32,
    pub opening_cash_edited: Option<bool>,
    pub edit_reason:         Option<String>,
    pub opened_at:           Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CashMovementRequest {
    pub amount: i32,
    pub note:   String,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CloseShiftRequest {
    pub closing_cash_declared: i32,
    pub cash_note:             Option<String>,
    pub closed_at:             Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct ForceCloseRequest {
    pub reason: Option<String>,
}



// ── GET /shifts/branches/:branch_id/current ───────────────────

#[utoipa::path(
    get,
    path = "/shifts/branches/{branch_id}/current",
    tag = "shifts",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    responses((status = 200, description = "Current shift info", body = ShiftPreFill), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_current_shift(
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "shifts", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    let open_shift = sqlx::query_as::<_, Shift>(
        r#"
        SELECT
            s.id, s.branch_id, s.teller_id,
            u.name AS teller_name,
            s.status::text,
            s.opening_cash, s.opening_cash_original,
            s.opening_cash_was_edited, s.opening_cash_edit_reason,
            s.closing_cash_declared, s.closing_cash_system, s.cash_discrepancy,
            s.opened_at, s.closed_at, s.closed_by,
            s.force_closed_by, s.force_closed_at, s.force_close_reason,
            s.notes
        FROM shifts s
        JOIN users u ON u.id = s.teller_id
        WHERE s.branch_id = $1 AND s.status = 'open'
        "#,
    )
    .bind(*branch_id)
    .fetch_optional(pool.get_ref())
    .await?;

    if let Some(shift) = open_shift {
        return Ok(HttpResponse::Ok().json(ShiftPreFill {
            has_open_shift:         true,
            open_shift:             Some(shift),
            suggested_opening_cash: 0,
        }));
    }

    let suggested: Option<i32> = sqlx::query_scalar(
        r#"
        SELECT closing_cash_declared
        FROM shifts
        WHERE branch_id = $1
          AND status IN ('closed', 'force_closed')
          AND closing_cash_declared IS NOT NULL
        ORDER BY closed_at DESC
        LIMIT 1
        "#,
    )
    .bind(*branch_id)
    .fetch_optional(pool.get_ref())
    .await?
    .flatten();

    Ok(HttpResponse::Ok().json(ShiftPreFill {
        has_open_shift:         false,
        open_shift:             None,
        suggested_opening_cash: suggested.unwrap_or(0),
    }))
}

// ── POST /shifts/branches/:branch_id/open ─────────────────────

#[utoipa::path(
    post,
    path = "/shifts/branches/{branch_id}/open",
    tag = "shifts",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    request_body = OpenShiftRequest,
    responses((status = 201, description = "Shift opened", body = Shift), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn open_shift(
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    body:      web::Json<OpenShiftRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "shifts", "create").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    let shift_id = body.id.unwrap_or_else(Uuid::new_v4);

    // Idempotent replay: the same shift id was already persisted → return it.
    if let Some(existing) = sqlx::query_as::<_, Shift>(
        r#"
        SELECT
            s.id, s.branch_id, s.teller_id,
            u.name AS teller_name,
            s.status::text,
            s.opening_cash, s.opening_cash_original,
            s.opening_cash_was_edited, s.opening_cash_edit_reason,
            s.closing_cash_declared, s.closing_cash_system, s.cash_discrepancy,
            s.opened_at, s.closed_at, s.closed_by,
            s.force_closed_by, s.force_closed_at, s.force_close_reason,
            s.notes
        FROM shifts s
        JOIN users u ON u.id = s.teller_id
        WHERE s.id = $1 AND s.branch_id = $2
        "#,
    )
    .bind(shift_id)
    .bind(*branch_id)
    .fetch_optional(pool.get_ref())
    .await?
    {
        return Ok(HttpResponse::Ok().json(existing));
    }

    let was_edited = body.opening_cash_edited.unwrap_or(false);
    if was_edited && body.edit_reason.as_deref().unwrap_or("").trim().is_empty() {
        return Err(AppError::BadRequest(
            "edit_reason is required when opening cash is edited".into(),
        ));
    }

    let opened_at = body.opened_at.unwrap_or_else(chrono::Utc::now);

    // Guards + insert run in one transaction; the unique partial indexes
    // (one open shift per branch AND per teller) are the race-proof backstop —
    // the pre-checks below only exist to return a friendly message.
    let mut tx = pool.get_ref().begin().await?;

    // This teller must not already hold an open shift (here or anywhere).
    let other_open_branch: Option<Uuid> = sqlx::query_scalar(
        "SELECT branch_id FROM shifts WHERE teller_id = $1 AND status = 'open'"
    )
    .bind(claims.user_id())
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(open_branch) = other_open_branch {
        return Err(AppError::Conflict(if open_branch == *branch_id {
            "A shift is already open for this branch".into()
        } else {
            "You already have an open shift at another branch. Close it before opening a new one.".into()
        }));
    }

    // And no one else may already have this branch open.
    let already_open: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM shifts WHERE branch_id = $1 AND status = 'open')"
    )
    .bind(*branch_id)
    .fetch_one(&mut *tx)
    .await?;
    if already_open {
        return Err(AppError::Conflict(
            "A shift is already open for this branch".into(),
        ));
    }

    let shift = sqlx::query_as::<_, Shift>(
        r#"
        INSERT INTO shifts
            (id, branch_id, teller_id, opening_cash, opening_cash_original,
             opening_cash_was_edited, opening_cash_edit_reason, opened_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        RETURNING
            id, branch_id, teller_id,
            (SELECT name FROM users WHERE id = $3) AS teller_name,
            status::text,
            opening_cash, opening_cash_original,
            opening_cash_was_edited, opening_cash_edit_reason,
            closing_cash_declared, closing_cash_system, cash_discrepancy,
            opened_at, closed_at, closed_by,
            force_closed_by, force_closed_at, force_close_reason,
            notes
        "#,
    )
    .bind(shift_id)
    .bind(*branch_id)
    .bind(claims.user_id())
    .bind(body.opening_cash)
    .bind(body.opening_cash)
    .bind(was_edited)
    .bind(&body.edit_reason)
    .bind(opened_at)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match &e {
        // A concurrent open won the race; the partial unique index rejected us.
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
            if db.constraint().is_some_and(|c| c.contains("teller")) {
                AppError::Conflict(
                    "You already have an open shift. Close it before opening a new one.".into(),
                )
            } else {
                AppError::Conflict("A shift is already open for this branch".into())
            }
        }
        _ => AppError::Db(e),
    })?;

    tx.commit().await?;

    Ok(HttpResponse::Created().json(shift))
}

// ── GET /shifts/branches/:branch_id ───────────────────────────

#[utoipa::path(
    get,
    path = "/shifts/branches/{branch_id}",
    tag = "shifts",
    params(("branch_id" = Uuid, Path, description = "Branch ID")),
    responses((status = 200, description = "List shifts", body = Vec<Shift>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_shifts(
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "shifts", "read").await?;

    // nil UUID = every branch in the caller's org ("All branches"); any other
    // UUID is that one branch after the usual access check. The org for the
    // roll-up is the token's org (or X-Org-Id for super admins).
    let (scope_condition, scope_id): (&str, Uuid) = if branch_id.is_nil() {
        let org = claims
            .scope_org(crate::auth::middleware::header_org_id(&req))
            .ok_or_else(|| AppError::Forbidden("No organization in scope".into()))?;
        (
            "s.branch_id IN (SELECT id FROM branches WHERE org_id = $1 AND deleted_at IS NULL)",
            org,
        )
    } else {
        require_branch_access(pool.get_ref(), &claims, *branch_id).await?;
        ("s.branch_id = $1", *branch_id)
    };

    let sql = format!(
        r#"
        SELECT
            s.id, s.branch_id, s.teller_id,
            u.name AS teller_name,
            b.name AS branch_name,
            s.status::text,
            s.opening_cash, s.opening_cash_original,
            s.opening_cash_was_edited, s.opening_cash_edit_reason,
            s.closing_cash_declared, s.closing_cash_system, s.cash_discrepancy,
            s.opened_at, s.closed_at, s.closed_by,
            s.force_closed_by, s.force_closed_at, s.force_close_reason,
            s.notes
        FROM shifts s
        JOIN users u    ON u.id = s.teller_id
        JOIN branches b ON b.id = s.branch_id
        WHERE {scope_condition}
        ORDER BY s.opened_at DESC
        "#
    );
    let shifts = sqlx::query_as::<_, Shift>(&sql)
        .bind(scope_id)
        .fetch_all(pool.get_ref())
        .await?;

    Ok(HttpResponse::Ok().json(shifts))
}

// ── GET /shifts/:shift_id ─────────────────────────────────────

#[utoipa::path(
    get,
    path = "/shifts/{shift_id}",
    tag = "shifts",
    params(("shift_id" = Uuid, Path, description = "Shift ID")),
    responses((status = 200, description = "Get shift", body = Shift), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_shift(
    req:      HttpRequest,
    pool:     web::Data<PgPool>,
    shift_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "shifts", "read").await?;

    let shift = fetch_shift_or_404(pool.get_ref(), *shift_id).await?;
    require_branch_access(pool.get_ref(), &claims, shift.branch_id).await?;

    Ok(HttpResponse::Ok().json(shift))
}

// ── GET /shifts/:shift_id/report ──────────────────────────────
//
// Returns payment breakdown + cash movement totals for any shift
// (open or closed). printed_at is always NOW() so the client can
// use: closed_at if available, otherwise printed_at.

#[utoipa::path(
    get,
    path = "/shifts/{shift_id}/report",
    tag = "shifts",
    params(("shift_id" = Uuid, Path, description = "Shift ID")),
    responses((status = 200, description = "Get shift report", body = ShiftReportResponse), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_shift_report(
    req:      HttpRequest,
    pool:     web::Data<PgPool>,
    shift_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "shifts", "read").await?;

    let shift = fetch_shift_or_404(pool.get_ref(), *shift_id).await?;
    require_branch_access(pool.get_ref(), &claims, shift.branch_id).await?;

    let payment_summary = sqlx::query_as::<_, PaymentSummaryRow>(
        r#"
        WITH all_payments AS (
            SELECT op.method AS payment_method, op.amount, op.order_id, op.is_cash
            FROM order_payments op
            JOIN orders o ON o.id = op.order_id
            WHERE o.shift_id = $1
              AND o.status NOT IN ('voided', 'refunded')
            UNION ALL
            SELECT COALESCE(o.tip_payment_method, o.payment_method) AS payment_method, o.tip_amount AS amount, o.id AS order_id, o.tip_is_cash AS is_cash
            FROM orders o
            WHERE o.shift_id = $1
              AND o.tip_amount IS NOT NULL
              AND o.status NOT IN ('voided', 'refunded')
        )
        SELECT
            ap.payment_method::text,
            COALESCE(ap.is_cash, ap.payment_method = 'cash') AS is_cash,
            COALESCE(SUM(ap.amount), 0)::bigint AS total,
            COUNT(DISTINCT ap.order_id)::bigint AS order_count
        FROM all_payments ap
        GROUP BY ap.payment_method, COALESCE(ap.is_cash, ap.payment_method = 'cash')
        ORDER BY ap.payment_method
        "#,
    )
    .bind(*shift_id)
    .fetch_all(pool.get_ref())
    .await?;

    let total_returns: i64 = sqlx::query_scalar(
        r#"
        SELECT COALESCE(SUM(total_amount), 0)::bigint
        FROM orders
        WHERE shift_id = $1 AND status = 'voided'
        "#,
    )
    .bind(*shift_id)
    .fetch_one(pool.get_ref())
    .await?;

    let cash_movements = sqlx::query_as::<_, CashMovementSummaryRow>(
        r#"
        SELECT
            m.amount,
            m.note,
            u.name AS moved_by_name,
            m.created_at
        FROM shift_cash_movements m
        JOIN users u ON u.id = m.moved_by
        WHERE m.shift_id = $1
        ORDER BY m.created_at ASC
        "#,
    )
    .bind(*shift_id)
    .fetch_all(pool.get_ref())
    .await?;

    let cash_movements_in: i64 = cash_movements.iter()
        .filter(|m| m.amount > 0)
        .map(|m| m.amount as i64)
        .sum();

    let cash_movements_out: i64 = cash_movements.iter()
        .filter(|m| m.amount < 0)
        .map(|m| m.amount.unsigned_abs() as i64)
        .sum();

    let total_payments: i64 = payment_summary.iter().map(|r| r.total).sum();
    // voided orders were never collected — they are informational only,
    // not subtracted. total_payments already excludes voided orders.
    let net_payments = total_payments;

    let cash_movements_net_signed: i64 = cash_movements_in as i64 - cash_movements_out as i64;
    Ok(HttpResponse::Ok().json(ShiftReportResponse {
        shift,
        payment_summary,
        total_payments,
        voided_amount: total_returns,
        net_payments,
        cash_movements,
        cash_movements_in,
        cash_movements_out,
        cash_movements_net: cash_movements_net_signed,
        printed_at: chrono::Utc::now(),
    }))
}

// ── POST /shifts/:shift_id/cash-movements ─────────────────────

#[utoipa::path(
    post,
    path = "/shifts/{shift_id}/cash-movements",
    tag = "shifts",
    params(("shift_id" = Uuid, Path, description = "Shift ID")),
    request_body = CashMovementRequest,
    responses((status = 201, description = "Add cash movement", body = CashMovement), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn add_cash_movement(
    req:      HttpRequest,
    pool:     web::Data<PgPool>,
    shift_id: web::Path<Uuid>,
    body:     web::Json<CashMovementRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "shifts", "update").await?;

    let shift = fetch_shift_or_404(pool.get_ref(), *shift_id).await?;
    require_branch_access(pool.get_ref(), &claims, shift.branch_id).await?;

    if shift.status != "open" {
        return Err(AppError::BadRequest(
            "Cash movements can only be added to an open shift".into(),
        ));
    }
    if body.amount == 0 {
        return Err(AppError::BadRequest("Amount cannot be zero".into()));
    }
    if body.note.trim().is_empty() {
        return Err(AppError::BadRequest(
            "Note is required for cash movements".into(),
        ));
    }

    let movement = sqlx::query_as::<_, CashMovement>(
        r#"
        INSERT INTO shift_cash_movements (shift_id, amount, note, moved_by)
        VALUES ($1, $2, $3, $4)
        RETURNING
            id, shift_id, amount, note, moved_by,
            (SELECT name FROM users WHERE id = $4) AS moved_by_name,
            created_at
        "#,
    )
    .bind(*shift_id)
    .bind(body.amount)
    .bind(&body.note)
    .bind(claims.user_id())
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Created().json(movement))
}

// ── GET /shifts/:shift_id/cash-movements ──────────────────────

#[utoipa::path(
    get,
    path = "/shifts/{shift_id}/cash-movements",
    tag = "shifts",
    params(("shift_id" = Uuid, Path, description = "Shift ID")),
    responses((status = 200, description = "List cash movements", body = Vec<CashMovement>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_cash_movements(
    req:      HttpRequest,
    pool:     web::Data<PgPool>,
    shift_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "shifts", "read").await?;

    let shift = fetch_shift_or_404(pool.get_ref(), *shift_id).await?;
    require_branch_access(pool.get_ref(), &claims, shift.branch_id).await?;

    let movements = sqlx::query_as::<_, CashMovement>(
        r#"
        SELECT
            m.id, m.shift_id, m.amount, m.note, m.moved_by,
            u.name AS moved_by_name,
            m.created_at
        FROM shift_cash_movements m
        JOIN users u ON u.id = m.moved_by
        WHERE m.shift_id = $1
        ORDER BY m.created_at ASC
        "#,
    )
    .bind(*shift_id)
    .fetch_all(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(movements))
}

// ── POST /shifts/:shift_id/close ──────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct CloseShiftResponse {
    pub shift: Shift,
}

#[utoipa::path(
    post,
    path = "/shifts/{shift_id}/close",
    tag = "shifts",
    params(("shift_id" = Uuid, Path, description = "Shift ID")),
    request_body = CloseShiftRequest,
    responses((status = 200, description = "Close shift", body = CloseShiftResponse), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn close_shift(
    req:      HttpRequest,
    pool:     web::Data<PgPool>,
    shift_id: web::Path<Uuid>,
    body:     web::Json<CloseShiftRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "shifts", "update").await?;

    let shift = fetch_shift_or_404(pool.get_ref(), *shift_id).await?;
    require_branch_access(pool.get_ref(), &claims, shift.branch_id).await?;

    // Idempotent: closing an already-closed shift just returns it.
    // (Inventory counting now lives in the standalone stocktake module.)
    if shift.status != "open" {
        return Ok(HttpResponse::Ok().json(CloseShiftResponse { shift }));
    }

    let closed_at = body.closed_at.unwrap_or_else(chrono::Utc::now);
    let mut tx = pool.get_ref().begin().await?;

    // Serialize against concurrent order inserts on this shift: create_order
    // takes the SAME per-shift advisory lock while inserting, so snapshotting
    // cash under this lock counts every committed order (no cash lost to a
    // close/insert race). Re-check open here too (idempotent double-close).
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
        .bind(shift_id.to_string())
        .execute(&mut *tx)
        .await?;
    let still_open: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM shifts WHERE id = $1 AND status = 'open')"
    )
    .bind(*shift_id)
    .fetch_one(&mut *tx)
    .await?;
    if !still_open {
        tx.rollback().await?;
        let current = fetch_shift_or_404(pool.get_ref(), *shift_id).await?;
        return Ok(HttpResponse::Ok().json(CloseShiftResponse { shift: current }));
    }

    let cash_from_orders: i32 = sqlx::query_scalar(
        r#"
        SELECT COALESCE(
            (
                SELECT COALESCE(SUM(op.amount), 0)::int
                FROM order_payments op
                JOIN orders o ON o.id = op.order_id
                WHERE o.shift_id = $1
                  AND COALESCE(op.is_cash, op.method = 'cash') = true
                  AND o.status NOT IN ('voided', 'refunded')
            ) + (
                SELECT COALESCE(SUM(o.tip_amount), 0)::int
                FROM orders o
                WHERE o.shift_id = $1
                  AND COALESCE(o.tip_is_cash, COALESCE(o.tip_payment_method, o.payment_method) = 'cash') = true
                  AND o.status NOT IN ('voided', 'refunded')
            ), 0)
        "#,
    )
    .bind(*shift_id)
    .fetch_one(&mut *tx)
    .await?;

    let cash_movements_total: i32 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount), 0)::int FROM shift_cash_movements WHERE shift_id = $1"
    )
    .bind(*shift_id)
    .fetch_one(&mut *tx)
    .await?;

    let closing_cash_system = shift.opening_cash + cash_from_orders + cash_movements_total;

    let closed_shift = sqlx::query_as::<_, Shift>(
        r#"
        UPDATE shifts SET
            status                = 'closed',
            closing_cash_declared = $2,
            closing_cash_system   = $3,
            closed_at             = $4,
            closed_by             = $5,
            notes                 = COALESCE($6, notes)
        WHERE id = $1
        RETURNING
            id, branch_id, teller_id,
            (SELECT name FROM users WHERE id = teller_id) AS teller_name,
            status::text,
            opening_cash, opening_cash_original,
            opening_cash_was_edited, opening_cash_edit_reason,
            closing_cash_declared, closing_cash_system, cash_discrepancy,
            opened_at, closed_at, closed_by,
            force_closed_by, force_closed_at, force_close_reason,
            notes
        "#,
    )
    .bind(*shift_id)
    .bind(body.closing_cash_declared)
    .bind(closing_cash_system)
    .bind(closed_at)
    .bind(claims.user_id())
    .bind(&body.cash_note)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(HttpResponse::Ok().json(CloseShiftResponse { shift: closed_shift }))
}

// ── POST /shifts/:shift_id/force-close ────────────────────────

#[utoipa::path(
    post,
    path = "/shifts/{shift_id}/force-close",
    tag = "shifts",
    params(("shift_id" = Uuid, Path, description = "Shift ID")),
    request_body = ForceCloseRequest,
    responses((status = 200, description = "Force close shift", body = Shift), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn force_close_shift(
    req:      HttpRequest,
    pool:     web::Data<PgPool>,
    shift_id: web::Path<Uuid>,
    body:     web::Json<ForceCloseRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "shifts", "update").await?;

    let shift = fetch_shift_or_404(pool.get_ref(), *shift_id).await?;
    require_branch_access(pool.get_ref(), &claims, shift.branch_id).await?;

    if claims.role == UserRole::Teller {
        return Err(AppError::Forbidden(
            "Only managers can force close a shift".into(),
        ));
    }

    if shift.status != "open" {
        return Err(AppError::BadRequest("Shift is not open".into()));
    }

    let closed = sqlx::query_as::<_, Shift>(
        r#"
        UPDATE shifts SET
            status             = 'force_closed',
            closed_at          = NOW(),
            closed_by          = $2,
            force_closed_by    = $2,
            force_closed_at    = NOW(),
            force_close_reason = $3
        WHERE id = $1
        RETURNING
            id, branch_id, teller_id,
            (SELECT name FROM users WHERE id = teller_id) AS teller_name,
            status::text,
            opening_cash, opening_cash_original,
            opening_cash_was_edited, opening_cash_edit_reason,
            closing_cash_declared, closing_cash_system, cash_discrepancy,
            opened_at, closed_at, closed_by,
            force_closed_by, force_closed_at, force_close_reason,
            notes
        "#,
    )
    .bind(*shift_id)
    .bind(claims.user_id())
    .bind(&body.reason)
    .fetch_one(pool.get_ref())
    .await?;

    Ok(HttpResponse::Ok().json(closed))
}

// ── DELETE /shifts/:shift_id ──────────────────────────────────

#[utoipa::path(
    delete,
    path = "/shifts/{shift_id}",
    tag = "shifts",
    params(("shift_id" = Uuid, Path, description = "Shift ID")),
    responses((status = 204, description = "Shift deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_shift(
    req:      HttpRequest,
    pool:     web::Data<PgPool>,
    shift_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    
    // Only OrgAdmin and SuperAdmin can delete shifts
    use crate::models::UserRole;
    if claims.role != UserRole::OrgAdmin && claims.role != UserRole::SuperAdmin {
        return Err(AppError::Forbidden("Only organization administrators can delete shifts".into()));
    }

    // Verify shift exists and belongs to this organization
    let shift = fetch_shift_or_404(pool.get_ref(), *shift_id).await?;
    require_branch_access(pool.get_ref(), &claims, shift.branch_id).await?;

    let mut tx = pool.get_ref().begin().await?;

    // 1. Delete orders belonging to the shift (cascades to order items, payments, etc.)
    sqlx::query("DELETE FROM orders WHERE shift_id = $1")
        .bind(*shift_id)
        .execute(&mut *tx)
        .await?;

    // 2. Delete the shift itself (cascades to cash movements and inventory counts)
    sqlx::query("DELETE FROM shifts WHERE id = $1")
        .bind(*shift_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(HttpResponse::NoContent().finish())
}

// ── Helpers ───────────────────────────────────────────────────

fn extract_claims(req: &HttpRequest) -> Result<Claims, AppError> {
    req.extensions()
        .get::<Claims>()
        .cloned()
        .ok_or_else(|| AppError::Unauthorized("Missing claims".into()))
}

async fn fetch_shift_or_404(pool: &PgPool, shift_id: Uuid) -> Result<Shift, AppError> {
    sqlx::query_as::<_, Shift>(
        r#"
        SELECT
            s.id, s.branch_id, s.teller_id,
            u.name AS teller_name,
            s.status::text,
            s.opening_cash, s.opening_cash_original,
            s.opening_cash_was_edited, s.opening_cash_edit_reason,
            s.closing_cash_declared, s.closing_cash_system, s.cash_discrepancy,
            s.opened_at, s.closed_at, s.closed_by,
            s.force_closed_by, s.force_closed_at, s.force_close_reason,
            s.notes
        FROM shifts s
        JOIN users u ON u.id = s.teller_id
        WHERE s.id = $1
        "#,
    )
    .bind(shift_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Shift not found".into()))
}

async fn require_branch_access(
    pool:      &PgPool,
    claims:    &Claims,
    branch_id: Uuid,
) -> Result<(), AppError> {
    if claims.role == UserRole::SuperAdmin { return Ok(()); }

    let branch_org: Option<Uuid> = sqlx::query_scalar(
        "SELECT org_id FROM branches WHERE id = $1 AND deleted_at IS NULL"
    )
    .bind(branch_id)
    .fetch_optional(pool)
    .await?
    .flatten();

    let branch_org = branch_org
        .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    if claims.org_id() != Some(branch_org) {
        return Err(AppError::Forbidden("Branch belongs to a different org".into()));
    }

    if claims.role == UserRole::OrgAdmin { return Ok(()); }

    let assigned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM user_branch_assignments WHERE user_id = $1 AND branch_id = $2)"
    )
    .bind(claims.user_id())
    .bind(branch_id)
    .fetch_one(pool)
    .await?;

    if !assigned {
        return Err(AppError::Forbidden("Not assigned to this branch".into()));
    }

    // A teller is bound to the branch they authenticated for. A token minted
    // for one branch must not drive another, even when the teller is assigned
    // to both — this is what stops a device picking up a different branch's
    // shift. (Tokens always carry a branch for tellers; the `None` guard keeps
    // legacy/unit-test tokens from being rejected outright.)
    if claims.role == UserRole::Teller {
        if let Some(token_branch) = claims.branch_id()
            && token_branch != branch_id {
            return Err(AppError::Forbidden(
                "This device is signed in to a different branch. Sign in to this branch to continue.".into(),
            ));
        }
    }

    Ok(())
}