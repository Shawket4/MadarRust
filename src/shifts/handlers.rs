use actix_web::{web, HttpMessage, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    errors::{AppError, AppErrorResponse},
    models::UserRole,
    permissions::checker::check_permission,
    sync::ActingContext,
};
use utoipa::{IntoParams, ToSchema};

/// Default page size when the shifts list is requested *with* pagination params.
const DEFAULT_SHIFTS_PER_PAGE: i64 = 20;
/// Upper bound on a single shifts page, to keep responses bounded.
const MAX_SHIFTS_PER_PAGE: i64 = 200;

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
    /// The till (drawer) this shift is on. Populated by the read/list/open
    /// endpoints; mutation responses that build the row via RETURNING may leave
    /// `till_name` null (same convention as `branch_name`).
    #[serde(default)]
    #[sqlx(default)]
    pub till_id:                  Option<Uuid>,
    #[serde(default)]
    #[sqlx(default)]
    pub till_name:                Option<String>,
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
    /// Client-minted idempotency / reconciliation key, echoed back so an
    /// offline client can map its queued movement to the server row. NULL for
    /// live online movements.
    #[serde(default)]
    pub client_ref:    Option<Uuid>,
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
    /// Authoritative system (expected) cash in the drawer. For a closed shift
    /// this is the snapshot taken at close (`closing_cash_system`); for an open
    /// shift it is computed live via the same formula. Clients should display
    /// this directly instead of re-deriving it from the payment breakdown.
    pub expected_cash:       i64,
    pub printed_at:          chrono::DateTime<chrono::Utc>,
}

/// Paginated envelope for the shifts list. When the request omits `page`/`per_page`,
/// `data` holds every matching shift in one page (back-compat for the dashboard);
/// when they are present, `data` is one bounded page ordered newest-first.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct PaginatedShifts {
    pub data:        Vec<Shift>,
    pub total:       i64,
    pub page:        i64,
    pub per_page:    i64,
    pub total_pages: i64,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListShiftsQuery {
    /// 1-based page number. Omit (along with `per_page`) to fetch every shift.
    pub page:     Option<i64>,
    /// Page size (clamped to [1, 200]). Omit to fetch every shift in one page.
    pub per_page: Option<i64>,
}

// ── Request types ─────────────────────────────────────────────

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct OpenShiftRequest {
    pub id:                  Option<Uuid>,
    /// The till (drawer) this shift opens on. Optional for back-compat: when
    /// omitted the server falls back to the branch's default till. Newer
    /// device-bound clients send their configured till explicitly.
    #[serde(default)]
    pub till_id:             Option<Uuid>,
    pub opening_cash:        i32,
    /// Ignored by the server — the carryover edit is DERIVED from the previous
    /// shift's declared closing. Kept only for API/back-compat with clients.
    pub opening_cash_edited: Option<bool>,
    pub edit_reason:         Option<String>,
    pub opened_at:           Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CashMovementRequest {
    pub amount: i32,
    pub note:   String,
    /// When the movement actually happened. Omit for live (online) movements —
    /// the server stamps `now()`. The POS sends this for movements made OFFLINE
    /// so they keep their real time after syncing. Future values are rejected.
    #[serde(default)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Client-minted idempotency / reconciliation key. The POS sends a stable
    /// UUID per movement so a replayed offline movement dedupes instead of
    /// double-applying. Omit for live online movements.
    #[serde(default)]
    pub client_ref: Option<Uuid>,
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

/// The cash a new shift should open with: the most recent CLOSED/force-closed
/// shift's declared closing **for this till** (the drawer carryover), or `None`
/// when there is no such predecessor. Scoped per **till** (the physical drawer),
/// not per branch or teller: with multiple tills open per branch, a new shift's
/// opening must continue from *that drawer's* last closing, regardless of which
/// teller is on it (handover keeps the float). Single source of truth for both
/// the `suggested_opening_cash` hint and the `open_shift` continuity enforcement —
/// they must never drift apart.
async fn previous_declared_closing<'e, E>(
    executor: E,
    till_id: Uuid,
) -> Result<Option<i32>, sqlx::Error>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_scalar::<_, Option<i32>>(
        r#"
        SELECT closing_cash_declared
        FROM shifts
        WHERE till_id = $1
          AND status IN ('closed', 'force_closed')
          AND closing_cash_declared IS NOT NULL
        ORDER BY closed_at DESC
        LIMIT 1
        "#,
    )
    .bind(till_id)
    .fetch_optional(executor)
    .await
    .map(Option::flatten)
}

/// Does **any** teller currently hold an open shift at this branch? With
/// multi-teller tills, "the branch is operating" is no longer a single-shift
/// fact — this is the gate a waiter fire (which has no shift of its own) checks
/// to confirm the branch is open for business, and the source of truth the LAN
/// shift-open fallback advertises. (Consumed by the waiter/tickets module — WS2.)
#[allow(dead_code)]
pub(crate) async fn branch_has_open_shift<'e, E>(
    executor: E,
    branch_id: Uuid,
) -> Result<bool, sqlx::Error>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM shifts WHERE branch_id = $1 AND status = 'open')",
    )
    .bind(branch_id)
    .fetch_one(executor)
    .await
}

/// Resolve the till a shift should open against: the explicit `till_id` from the
/// request, else the branch's default till. Validates the till is live and
/// belongs to the branch (clean 400/404, not a FK violation). When no till_id is
/// supplied and the branch has NO tills at all (legacy branches / direct inserts),
/// a default "Till 1" drawer is provisioned lazily so the open never gets stuck.
/// If the branch has tills but none is the default, an explicit choice is
/// required. Takes a connection so it can run the read-then-provision steps.
async fn resolve_open_till(
    conn: &mut sqlx::PgConnection,
    branch_id: Uuid,
    requested: Option<Uuid>,
) -> Result<Uuid, AppError> {
    if let Some(till_id) = requested {
        let ok: Option<bool> = sqlx::query_scalar(
            "SELECT is_active FROM tills \
             WHERE id = $1 AND branch_id = $2 AND deleted_at IS NULL",
        )
        .bind(till_id)
        .bind(branch_id)
        .fetch_optional(&mut *conn)
        .await?;
        return match ok {
            None => Err(AppError::NotFound("Till not found for this branch".into())),
            Some(false) => Err(AppError::BadRequest("That till is inactive".into())),
            Some(true) => Ok(till_id),
        };
    }

    // No explicit till → the branch's default drawer.
    if let Some(id) = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM tills WHERE branch_id = $1 AND is_default AND deleted_at IS NULL",
    )
    .bind(branch_id)
    .fetch_optional(&mut *conn)
    .await?
    {
        return Ok(id);
    }

    // No default. If the branch already has other tills, that's a deliberate
    // setup — make the caller pick one rather than guessing the drawer.
    let any_till: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM tills WHERE branch_id = $1 AND deleted_at IS NULL)",
    )
    .bind(branch_id)
    .fetch_one(&mut *conn)
    .await?;
    if any_till {
        return Err(AppError::BadRequest(
            "No till specified and this branch has no default till".into(),
        ));
    }

    // Branch has zero tills → lazily provision the default "Till 1". `ON CONFLICT
    // DO NOTHING` makes a concurrent first-open a no-op; we re-select the winner.
    sqlx::query(
        "INSERT INTO tills (org_id, branch_id, name, is_default, is_active) \
         SELECT b.org_id, b.id, 'Till 1', true, true FROM branches b WHERE b.id = $1 \
         ON CONFLICT DO NOTHING",
    )
    .bind(branch_id)
    .execute(&mut *conn)
    .await?;

    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM tills WHERE branch_id = $1 AND is_default AND deleted_at IS NULL",
    )
    .bind(branch_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| AppError::NotFound("Branch not found".into()))
}

/// The **system (expected) cash** in a shift's drawer:
///   opening float
/// + cash taken in via orders (cash payments + cash tips, excluding voided/refunded)
/// + net manual cash movements (cash in − cash out).
///
/// Single source of truth shared by `close_shift` (snapshotted into
/// `closing_cash_system`) and the live shift report's `expected_cash`, so the
/// teller's pre-close preview can never drift from the value recorded at close.
/// Reads `opening_cash` from the row, so the caller does not pass it in.
async fn compute_system_cash<'e, E>(executor: E, shift_id: Uuid) -> Result<i64, sqlx::Error>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT (
            (SELECT opening_cash FROM shifts WHERE id = $1)
          + COALESCE((
                SELECT SUM(op.amount)
                FROM order_payments op
                JOIN orders o ON o.id = op.order_id
                WHERE o.shift_id = $1
                  AND COALESCE(op.is_cash, op.method = 'cash') = true
                  AND o.status NOT IN ('voided', 'refunded')
            ), 0)
          + COALESCE((
                SELECT SUM(o.tip_amount)
                FROM orders o
                WHERE o.shift_id = $1
                  AND COALESCE(o.tip_is_cash, COALESCE(o.tip_payment_method, o.payment_method) = 'cash') = true
                  AND o.status NOT IN ('voided', 'refunded')
            ), 0)
          + COALESCE((
                SELECT SUM(amount) FROM shift_cash_movements WHERE shift_id = $1
            ), 0)
        )::bigint
        "#,
    )
    .bind(shift_id)
    .fetch_one(executor)
    .await
}



// ── GET /shifts/branches/:branch_id/current ───────────────────

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CurrentShiftQuery {
    /// The device's till (drawer). Narrows the open-shift lookup for managers and
    /// scopes the suggested opening cash to that drawer's carryover. Optional —
    /// omit to fall back to the branch's default till for the suggestion.
    #[serde(default)]
    pub till_id: Option<Uuid>,
}

#[utoipa::path(
    get,
    path = "/shifts/branches/{branch_id}/current",
    tag = "shifts",
    params(
        ("branch_id" = Uuid, Path, description = "Branch ID"),
        CurrentShiftQuery,
    ),
    responses((status = 200, description = "Current shift info", body = ShiftPreFill), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_current_shift(
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query:     web::Query<CurrentShiftQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "shifts", "read").await?;
    require_branch_access(pool.get_ref(), &claims, *branch_id).await?;

    // TELLER-SCOPED: a teller's "current shift" is THEIR OWN open shift at this
    // branch — never another teller's. So after a teller signs in, the device
    // adopts only the shift they actually own (or none → open a new one); a stale
    // or another teller's open shift can never bounce them. Managers (non-tellers)
    // see an open shift for the branch (optionally narrowed to a till); with
    // multiple tills open per branch this returns the most recently opened.
    let teller_filter = if claims.role == UserRole::Teller {
        Some(claims.user_id())
    } else {
        None
    };
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
            s.notes,
            s.till_id, t.name AS till_name
        FROM shifts s
        JOIN users u ON u.id = s.teller_id
        LEFT JOIN tills t ON t.id = s.till_id
        WHERE s.branch_id = $1 AND s.status = 'open'
          AND ($2::uuid IS NULL OR s.teller_id = $2)
          AND ($3::uuid IS NULL OR s.till_id = $3)
        ORDER BY s.opened_at DESC
        LIMIT 1
        "#,
    )
    .bind(*branch_id)
    .bind(teller_filter)
    .bind(query.till_id)
    .fetch_optional(pool.get_ref())
    .await?;

    if let Some(shift) = open_shift {
        return Ok(HttpResponse::Ok().json(ShiftPreFill {
            has_open_shift:         true,
            open_shift:             Some(shift),
            suggested_opening_cash: 0,
        }));
    }

    // Suggest the carryover for THIS drawer: the requested till, else the
    // branch's default till. The opening-cash continuity is per-till, so the hint
    // must be too (a teller handover on a drawer keeps its float).
    let suggest_till_id: Option<Uuid> = match query.till_id {
        Some(t) => Some(t),
        None => sqlx::query_scalar(
            "SELECT id FROM tills WHERE branch_id = $1 AND is_default AND deleted_at IS NULL",
        )
        .bind(*branch_id)
        .fetch_optional(pool.get_ref())
        .await?,
    };
    let suggested = match suggest_till_id {
        Some(t) => previous_declared_closing(pool.get_ref(), t).await?,
        None => None,
    };

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
    open_shift_inner(pool.clone(), branch_id.into_inner(), body, ActingContext::live(&claims)?).await
}

/// Open-shift core. LIVE callers come through `open_shift` (JWT-attributed);
/// REPLAY calls this with the queued op's embedded teller. In replay mode the
/// one-open-per-teller / one-open-per-till prechecks and the cash-continuity
/// `edit_reason` requirement are skipped (the open already happened on the device
/// — sequential-only means its predecessor's close replays first), but the unique
/// partial indexes and the idempotent early-return still protect integrity. The
/// shift attaches to a till (the drawer); `body.till_id` else the branch default.
pub(crate) async fn open_shift_inner(
    pool:      web::Data<PgPool>,
    branch_id: Uuid,
    body:      web::Json<OpenShiftRequest>,
    actor:     ActingContext,
) -> Result<HttpResponse, AppError> {
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
            s.notes,
            s.till_id, t.name AS till_name
        FROM shifts s
        JOIN users u ON u.id = s.teller_id
        LEFT JOIN tills t ON t.id = s.till_id
        WHERE s.id = $1 AND s.branch_id = $2
        "#,
    )
    .bind(shift_id)
    .bind(branch_id)
    .fetch_optional(pool.get_ref())
    .await?
    {
        return Ok(HttpResponse::Ok().json(existing));
    }

    let opened_at = body.opened_at.unwrap_or_else(chrono::Utc::now);
    crate::clock::reject_if_future(opened_at, "opened_at")?;

    // Guards + insert run in one transaction; the unique partial indexes
    // (one open shift per till AND per teller) are the race-proof backstop —
    // the pre-checks below only exist to return a friendly message.
    let mut tx = pool.get_ref().begin().await?;

    // Resolve (and validate) the drawer this shift opens on — explicit till_id
    // else the branch's default till. Needed before the per-till continuity read
    // and the insert; applies to replay too (the queued op carries its till).
    let till_id = resolve_open_till(&mut *tx, branch_id, body.till_id).await?;

    // Live only: a fresh open must not collide with an existing open shift. Replay
    // skips these friendly prechecks — its ordering guarantees no real overlap and
    // the unique indexes remain the backstop against a genuine duplicate.
    if !actor.replay {
        // A teller holds at most one open shift (here or anywhere). The branch may
        // now have several open shifts (one per till) — branch-level uniqueness is
        // intentionally gone; the per-till index below is the drawer guard.
        let other_open_branch: Option<Uuid> = sqlx::query_scalar(
            "SELECT branch_id FROM shifts WHERE teller_id = $1 AND status = 'open'"
        )
        .bind(actor.teller_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(open_branch) = other_open_branch {
            return Err(AppError::Conflict(if open_branch == branch_id {
                "You already have an open shift at this branch.".into()
            } else {
                "You already have an open shift at another branch. Close it before opening a new one.".into()
            }));
        }

        // And this till must not already have an open shift under anyone.
        let till_open: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM shifts WHERE till_id = $1 AND status = 'open')"
        )
        .bind(till_id)
        .fetch_one(&mut *tx)
        .await?;
        if till_open {
            return Err(AppError::Conflict(
                "That till already has an open shift.".into(),
            ));
        }
    }

    // ── Cash continuity (V32) ────────────────────────────────────
    // The drawer carries over between shifts: the opening cash must equal THIS
    // till's previous DECLARED closing cash (per-till, so a teller handover on the
    // same drawer keeps the float). We compute that expected carryover server-side
    // and DERIVE `was_edited` from it (the client's `opening_cash_edited` flag is
    // not trusted). A legitimate float change is allowed but must carry a reason
    // and is recorded as an edit. With no prior declared closing (first shift on
    // this till, or a force-closed predecessor that never declared) there is
    // nothing to continue from, so any amount is accepted as the starting float.
    // Read inside the tx for a consistent snapshot.
    let expected_opening = previous_declared_closing(&mut *tx, till_id).await?;

    let was_edited = expected_opening.is_some_and(|exp| exp != body.opening_cash);
    // Live only: a deviating float must carry a reason. Replay records history as
    // it happened — the device may not have known the exact server carryover.
    if !actor.replay && was_edited && body.edit_reason.as_deref().unwrap_or("").trim().is_empty() {
        return Err(AppError::BadRequest(
            "Opening cash differs from the previous shift's declared closing cash; \
             edit_reason is required to override the carryover.".into(),
        ));
    }
    // Only persist a reason when it actually overrides the carryover.
    let edit_reason = if was_edited { body.edit_reason.as_deref() } else { None };

    let insert_result = sqlx::query_as::<_, Shift>(
        r#"
        INSERT INTO shifts
            (id, branch_id, teller_id, opening_cash, opening_cash_original,
             opening_cash_was_edited, opening_cash_edit_reason, opened_at, till_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING
            id, branch_id, teller_id,
            (SELECT name FROM users WHERE id = $3) AS teller_name,
            status::text,
            opening_cash, opening_cash_original,
            opening_cash_was_edited, opening_cash_edit_reason,
            closing_cash_declared, closing_cash_system, cash_discrepancy,
            opened_at, closed_at, closed_by,
            force_closed_by, force_closed_at, force_close_reason,
            notes,
            till_id,
            (SELECT name FROM tills WHERE id = $9) AS till_name
        "#,
    )
    .bind(shift_id)
    .bind(branch_id)
    .bind(actor.teller_id)
    .bind(body.opening_cash)
    .bind(expected_opening)
    .bind(was_edited)
    .bind(edit_reason)
    .bind(opened_at)
    .bind(till_id)
    .fetch_one(&mut *tx)
    .await;

    let shift = match insert_result {
        Ok(s) => s,
        // A concurrent open won the race; the partial unique index rejected us.
        // The pre-insert SELECT above and this INSERT are not atomic, so two
        // concurrent replays of the SAME queued open (e.g. a double-send) can both
        // pass the SELECT and one then trips the unique index. If the row that now
        // exists IS this shift id, the open is idempotently already-applied →
        // return it 200 (mirrors create_order's idempotency). Only a conflict from
        // a DIFFERENT open shift (no row for this id) is a genuine 409 — otherwise
        // a committed open gets wrongly dead-lettered and cascades its order/close.
        Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some("23505") => {
            drop(tx);
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
                    s.notes,
                    s.till_id, t.name AS till_name
                FROM shifts s
                JOIN users u ON u.id = s.teller_id
                LEFT JOIN tills t ON t.id = s.till_id
                WHERE s.id = $1 AND s.branch_id = $2
                "#,
            )
            .bind(shift_id)
            .bind(branch_id)
            .fetch_optional(pool.get_ref())
            .await?
            {
                return Ok(HttpResponse::Ok().json(existing));
            }
            return Err(if db.constraint().is_some_and(|c| c.contains("teller")) {
                AppError::Conflict(
                    "You already have an open shift. Close it before opening a new one.".into(),
                )
            } else {
                // The branch-level open index is gone (multi-teller); a remaining
                // open-shift conflict is the per-till drawer guard.
                AppError::Conflict("That till already has an open shift.".into())
            });
        }
        Err(e) => return Err(AppError::Db(e)),
    };

    tx.commit().await?;

    Ok(HttpResponse::Created().json(shift))
}

// ── GET /shifts/branches/:branch_id ───────────────────────────

#[utoipa::path(
    get,
    path = "/shifts/branches/{branch_id}",
    tag = "shifts",
    params(
        ("branch_id" = Uuid, Path, description = "Branch ID (nil UUID = all branches in org)"),
        ListShiftsQuery,
    ),
    responses((status = 200, description = "List shifts (newest first)", body = PaginatedShifts), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_shifts(
    req:       HttpRequest,
    pool:      web::Data<PgPool>,
    branch_id: web::Path<Uuid>,
    query:     web::Query<ListShiftsQuery>,
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

    // Pagination is opt-in: a request with neither `page` nor `per_page` returns
    // every matching shift in one page (the dashboard relies on this for export).
    // Supplying either one switches on bounded, newest-first paging.
    let paginate = query.page.is_some() || query.per_page.is_some();

    let total: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM shifts s WHERE {scope_condition}"
    ))
    .bind(scope_id)
    .fetch_one(pool.get_ref())
    .await?;

    let (page, per_page) = if paginate {
        let per_page = query
            .per_page
            .unwrap_or(DEFAULT_SHIFTS_PER_PAGE)
            .clamp(1, MAX_SHIFTS_PER_PAGE);
        (query.page.unwrap_or(1).max(1), per_page)
    } else {
        // One page holding everything; per_page mirrors the row count (>=1).
        (1, total.max(1))
    };
    let total_pages = if per_page > 0 { (total + per_page - 1) / per_page } else { 0 };
    let offset = (page - 1) * per_page;

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
            s.notes,
            s.till_id, t.name AS till_name
        FROM shifts s
        JOIN users u    ON u.id = s.teller_id
        JOIN branches b ON b.id = s.branch_id
        LEFT JOIN tills t ON t.id = s.till_id
        WHERE {scope_condition}
        ORDER BY s.opened_at DESC
        LIMIT $2 OFFSET $3
        "#
    );
    let data = sqlx::query_as::<_, Shift>(&sql)
        .bind(scope_id)
        .bind(per_page)
        .bind(offset)
        .fetch_all(pool.get_ref())
        .await?;

    Ok(HttpResponse::Ok().json(PaginatedShifts {
        data,
        total,
        page,
        per_page,
        total_pages,
    }))
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

    // Authoritative expected cash: a closed shift uses its snapshot; an open
    // shift is computed live with the SAME formula close_shift will use, so the
    // teller's preview matches what gets recorded at close.
    let expected_cash: i64 = match shift.closing_cash_system {
        Some(v) => v as i64,
        None => compute_system_cash(pool.get_ref(), *shift_id).await?,
    };

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
        expected_cash,
        printed_at: chrono::Utc::now(),
    }))
}

/// Look up a cash movement by its client-minted `client_ref` (idempotency /
/// reconciliation key). Lets a replayed offline movement return the original
/// row instead of double-applying.
async fn fetch_cash_movement_by_client_ref(
    pool:       &PgPool,
    client_ref: Uuid,
) -> Result<Option<CashMovement>, AppError> {
    let m = sqlx::query_as::<_, CashMovement>(
        r#"
        SELECT
            m.id, m.shift_id, m.amount, m.note, m.moved_by,
            (SELECT name FROM users WHERE id = m.moved_by) AS moved_by_name,
            m.created_at, m.client_ref
        FROM shift_cash_movements m
        WHERE m.client_ref = $1
        "#,
    )
    .bind(client_ref)
    .fetch_optional(pool)
    .await?;
    Ok(m)
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
    add_cash_movement_inner(pool.clone(), shift_id.into_inner(), body, ActingContext::live(&claims)?).await
}

/// Cash-movement core. LIVE attributes `moved_by` to the JWT teller and enforces
/// own-drawer; REPLAY attributes it to the queued op's teller and skips the
/// drawer-owner guard (a different teller may be flushing the device). Idempotent
/// on `client_ref`; still requires the shift to be open (a movement on a closed
/// shift would corrupt its already-settled cash reconciliation).
pub(crate) async fn add_cash_movement_inner(
    pool:     web::Data<PgPool>,
    shift_id: Uuid,
    body:     web::Json<CashMovementRequest>,
    actor:    ActingContext,
) -> Result<HttpResponse, AppError> {
    let shift = fetch_shift_or_404(pool.get_ref(), shift_id).await?;

    // A teller may only move cash within their OWN drawer — a movement changes
    // the expected-cash reconciliation, so it must belong to the right person.
    // Replay bypasses this: it's recorded history, attributed to its own teller.
    if !actor.replay && actor.role == UserRole::Teller && shift.teller_id != actor.teller_id {
        return Err(AppError::Forbidden(
            "You can only add cash movements to your own shift".into(),
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
    if let Some(ts) = body.created_at {
        crate::clock::reject_if_future(ts, "created_at")?;
    }

    // Idempotent replay: a queued offline movement that already landed returns
    // the original instead of double-applying (which corrupts expected_cash).
    if let Some(cref) = body.client_ref
        && let Some(existing) = fetch_cash_movement_by_client_ref(pool.get_ref(), cref).await?
    {
        return Ok(HttpResponse::Ok().json(existing));
    }

    // Serialize against a concurrent close exactly like create_order does:
    // close_shift snapshots `closing_cash_system` under this same per-shift
    // advisory lock, so taking it here (and re-checking `open` inside the lock)
    // guarantees a movement either lands before the snapshot — and is counted —
    // or is rejected, never silently excluded from a just-closed shift.
    let mut tx = pool.get_ref().begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
        .bind(shift_id.to_string())
        .execute(&mut *tx)
        .await?;

    let still_open: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM shifts WHERE id = $1 AND status = 'open')"
    )
    .bind(shift_id)
    .fetch_one(&mut *tx)
    .await?;
    if !still_open {
        return Err(AppError::BadRequest(
            "Cash movements can only be added to an open shift".into(),
        ));
    }

    let movement = match sqlx::query_as::<_, CashMovement>(
        r#"
        INSERT INTO shift_cash_movements (shift_id, amount, note, moved_by, created_at, client_ref)
        VALUES ($1, $2, $3, $4, COALESCE($5, now()), $6)
        RETURNING
            id, shift_id, amount, note, moved_by,
            (SELECT name FROM users WHERE id = $4) AS moved_by_name,
            created_at, client_ref
        "#,
    )
    .bind(shift_id)
    .bind(body.amount)
    .bind(&body.note)
    .bind(actor.teller_id)
    .bind(body.created_at)
    .bind(body.client_ref)
    .fetch_one(&mut *tx)
    .await
    {
        Ok(m) => m,
        // client_ref race: a concurrent replay of the same offline movement
        // already committed. Return the original instead of a raw 500.
        Err(sqlx::Error::Database(db))
            if db.code().as_deref() == Some("23505")
                && db.constraint().is_some_and(|c| c.contains("client_ref")) =>
        {
            drop(tx);
            if let Some(cref) = body.client_ref
                && let Some(existing) =
                    fetch_cash_movement_by_client_ref(pool.get_ref(), cref).await?
            {
                return Ok(HttpResponse::Ok().json(existing));
            }
            return Err(AppError::Conflict("Duplicate cash movement".into()));
        }
        Err(e) => return Err(e.into()),
    };

    tx.commit().await?;

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
            m.created_at, m.client_ref
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
    close_shift_inner(pool.clone(), shift_id.into_inner(), body, ActingContext::live(&claims)?).await
}

/// Close-shift core. LIVE attributes `closed_by` to the JWT teller and enforces
/// own-shift; REPLAY attributes it to the queued op's teller and skips the
/// own-shift guard (the device flushing the backlog may be a different teller).
/// Idempotent: an already-closed shift just returns it.
pub(crate) async fn close_shift_inner(
    pool:     web::Data<PgPool>,
    shift_id: Uuid,
    body:     web::Json<CloseShiftRequest>,
    actor:    ActingContext,
) -> Result<HttpResponse, AppError> {
    let shift = fetch_shift_or_404(pool.get_ref(), shift_id).await?;

    // A teller may only close their OWN shift — closing settles cash, so it must
    // be attributed to the right person. Managers/admins can close any shift in
    // their branch scope (force-close remains the path for an absent teller).
    // Replay bypasses this: it's recorded history, attributed to its own teller.
    if !actor.replay && actor.role == UserRole::Teller && shift.teller_id != actor.teller_id {
        return Err(AppError::Forbidden(
            "You can only close your own shift".into(),
        ));
    }

    // Idempotent: closing an already-closed shift just returns it.
    // (Inventory counting now lives in the standalone stocktake module.)
    if shift.status != "open" {
        return Ok(HttpResponse::Ok().json(CloseShiftResponse { shift }));
    }

    let closed_at = body.closed_at.unwrap_or_else(chrono::Utc::now);
    crate::clock::reject_if_future(closed_at, "closed_at")?;
    if closed_at < shift.opened_at {
        return Err(AppError::BadRequest(
            "closed_at cannot be before the shift was opened".into(),
        ));
    }
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
    .bind(shift_id)
    .fetch_one(&mut *tx)
    .await?;
    if !still_open {
        tx.rollback().await?;
        let current = fetch_shift_or_404(pool.get_ref(), shift_id).await?;
        return Ok(HttpResponse::Ok().json(CloseShiftResponse { shift: current }));
    }

    // Snapshot the expected drawer cash under the advisory lock (counts every
    // committed order). Same formula as the live shift report — see
    // [compute_system_cash].
    let closing_cash_system = cash_to_i32(compute_system_cash(&mut *tx, shift_id).await?)?;

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
    .bind(shift_id)
    .bind(body.closing_cash_declared)
    .bind(closing_cash_system)
    .bind(closed_at)
    .bind(actor.teller_id)
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

    // Idempotent replay: a force-close re-sent on reconnect (or a shift already
    // closed/force-closed) returns the existing terminal shift instead of a 400,
    // so a retried request is safe. Mirrors close_shift's early-return.
    if shift.status != "open" {
        return Ok(HttpResponse::Ok().json(shift));
    }

    // Snapshot the expected drawer cash under the same per-shift advisory lock
    // create_order / close_shift use, so a force-close freezes `closing_cash_system`
    // exactly like a normal close. No declared count is collected (the teller is
    // absent), but the system figure must still be an immutable audit record —
    // otherwise a force-closed shift's expected cash keeps being re-derived live
    // and a later void could silently rewrite it.
    let mut tx = pool.get_ref().begin().await?;
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
        return Ok(HttpResponse::Ok().json(current));
    }

    let closing_cash_system = cash_to_i32(compute_system_cash(&mut *tx, *shift_id).await?)?;

    let closed = sqlx::query_as::<_, Shift>(
        r#"
        UPDATE shifts SET
            status              = 'force_closed',
            closing_cash_system = $4,
            closed_at           = NOW(),
            closed_by           = $2,
            force_closed_by     = $2,
            force_closed_at     = NOW(),
            force_close_reason  = $3
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
    .bind(closing_cash_system)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

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

    // Delete is a cleanup tool for empty / erroneous shifts only — it must never
    // be able to wipe live or settled financial history:
    //   • An OPEN shift may have a teller actively ringing up orders; deleting it
    //     would destroy in-progress sales and break the one-open-per-branch
    //     invariant mid-service. Force-close it first (audited), then delete.
    //   • A shift with non-voided orders holds real revenue that belongs to the
    //     financial record; those sales must not vanish silently.
    if shift.status == "open" {
        return Err(AppError::Conflict(
            "Cannot delete an open shift — force-close it first.".into(),
        ));
    }
    let has_real_orders: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM orders WHERE shift_id = $1 AND status <> 'voided')"
    )
    .bind(*shift_id)
    .fetch_one(pool.get_ref())
    .await?;
    if has_real_orders {
        return Err(AppError::Conflict(
            "Cannot delete a shift that has recorded (non-voided) orders — its \
             sales are part of the financial record.".into(),
        ));
    }

    let mut tx = pool.get_ref().begin().await?;

    // Only voided orders (no financial value) remain — remove them so the shift's
    // FK references clear, then delete the shift (cascades to cash movements).
    sqlx::query("DELETE FROM orders WHERE shift_id = $1")
        .bind(*shift_id)
        .execute(&mut *tx)
        .await?;

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

/// Rejects a client-supplied timestamp that sits too far in the FUTURE. POS
/// devices can be offline for a while, so a PAST timestamp is legitimate (an
/// action that happened earlier and is only now syncing) and is accepted as-is.
/// A future timestamp, though, means a skewed device clock or a spoof — left
/// unchecked it would file the shift into a future business day or reorder the
/// cash-continuity carryover (which keys on `closed_at`). We clamp only the
/// future side, with a few minutes of skew tolerance.
/// Narrows the i64 system-cash total to the i32 the cash columns store, turning
/// what was a silent wrap-around (`as i32`) into a clear error. The columns hold
/// piastres, so i32 caps a single shift at ~21.4M EGP; exceeding that is a data
/// anomaly we surface rather than corrupt the closing snapshot.
fn cash_to_i32(v: i64) -> Result<i32, AppError> {
    i32::try_from(v).map_err(|_| AppError::Internal)
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
            s.notes,
            s.till_id, t.name AS till_name
        FROM shifts s
        JOIN users u ON u.id = s.teller_id
        LEFT JOIN tills t ON t.id = s.till_id
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

    // D13: tellers are ORG-scoped, not branch-scoped — any active teller in the
    // branch's org may operate here (the org check above is the boundary). The
    // device still stamps its own branch on the records it creates.
    if claims.role == UserRole::Teller { return Ok(()); }

    // Branch managers stay branch-scoped via their explicit assignments.
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

    Ok(())
}