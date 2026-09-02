//! Staff requests — every "may I?" an employee asks, and the manager's answer.
//!
//! One table, one status machine, one inbox. See the `staff_requests` migration
//! for why the five kinds collapse into a single shape: each is an EXCUSED WINDOW
//! inside a day, open at the start (`late_arrival`), open at the end
//! (`early_departure`), closed at both (`excuse`), or covering whole days
//! (`leave`, `mission`).
//!
//! ## Why this matters to payroll
//!
//! An approved request removes a penalty AT ITS SOURCE rather than generating one
//! and cancelling it. [`day_adjustments`] resolves a day's approved requests into
//! the shape `attendance::derive` consumes, so an employee with permission to
//! arrive at 10:00 is never late in the first place — there is no penalty row to
//! waive, no correction to make, and nothing to argue about later.
//!
//! ## Leave balances
//!
//! `leave_balances.used_days` is maintained by the approval path, not derived on
//! read: an entitlement can change mid-year, so recomputing "days used" from the
//! request list would silently rewrite history every time HR edits a quota.
//! Approving spends; cancelling an approved request refunds. Both happen inside
//! the same transaction as the status change.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    errors::{AppError, AppErrorResponse},
    orgs::handlers::extract_claims,
    permissions::checker::check_permission,
    staff::{attendance::DayAdjustments, require_user_in_org, scope_org, validate_decision},
};

/// The six things an employee can ask for.
///
/// The first five are EXCUSED WINDOWS — "forgive this part of my day". The
/// sixth, `correction`, is not: it proposes an edit to a punch that the clock
/// got wrong, and on approval it is written to the attendance record and
/// repriced. See [`apply_correction`].
pub const KINDS: [&str; 6] = [
    "leave",
    "late_arrival",
    "early_departure",
    "excuse",
    "mission",
    "correction",
];

// ── Models ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct StaffRequest {
    pub id: Uuid,
    pub org_id: Uuid,
    pub user_id: Uuid,
    #[sqlx(default)]
    pub user_name: Option<String>,
    /// `leave` | `late_arrival` | `early_departure` | `excuse` | `mission`.
    pub kind: String,
    pub on_date: NaiveDate,
    /// Set for `leave` and `mission`; the span's last day.
    pub end_date: Option<NaiveDate>,
    /// Start of the excused window. `None` = open to the shift's start.
    pub from_time: Option<NaiveTime>,
    /// End of the excused window. `None` = open to the shift's end.
    pub to_time: Option<NaiveTime>,
    pub leave_type_id: Option<Uuid>,
    #[sqlx(default)]
    pub leave_type_name: Option<String>,
    pub is_half_day: bool,
    pub title: Option<String>,
    pub location: Option<String>,
    /// The record a `correction` proposes to fix. `None` for every other kind.
    pub attendance_record_id: Option<Uuid>,
    pub reason: Option<String>,
    pub status: String,
    /// Whether the excused time is paid. `None` until decided.
    pub is_paid: Option<bool>,
    pub decided_by: Option<Uuid>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decision_note: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const REQUEST_SELECT: &str = r#"
    SELECT r.id, r.org_id, r.user_id, u.name AS user_name, r.kind, r.on_date,
           r.end_date, r.from_time, r.to_time, r.leave_type_id,
           t.name AS leave_type_name, r.is_half_day, r.title, r.location,
           r.attendance_record_id, r.reason, r.status, r.is_paid, r.decided_by, r.decided_at,
           r.decision_note, r.created_at, r.updated_at
      FROM staff_requests r
      JOIN users u ON u.id = r.user_id
      LEFT JOIN leave_types t ON t.id = r.leave_type_id
"#;

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct LeaveType {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub is_paid: bool,
    pub annual_quota_days: Option<Decimal>,
    pub requires_approval: bool,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const LEAVE_TYPE_COLS: &str = "id, org_id, name, is_paid, annual_quota_days, \
     requires_approval, is_active, created_at, updated_at";

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct LeaveBalance {
    pub id: Uuid,
    pub org_id: Uuid,
    pub user_id: Uuid,
    pub leave_type_id: Uuid,
    #[sqlx(default)]
    pub leave_type_name: Option<String>,
    pub year: i32,
    pub entitled_days: Decimal,
    pub used_days: Decimal,
    pub carried_over_days: Decimal,
    /// `entitled + carried_over − used`. Computed, not stored.
    #[sqlx(default)]
    pub remaining_days: Decimal,
}

const BALANCE_SELECT: &str = r#"
    SELECT b.id, b.org_id, b.user_id, b.leave_type_id, t.name AS leave_type_name,
           b.year, b.entitled_days, b.used_days, b.carried_over_days,
           (b.entitled_days + b.carried_over_days - b.used_days) AS remaining_days
      FROM leave_balances b
      JOIN leave_types t ON t.id = b.leave_type_id
"#;

// ── Requests (the wire) ───────────────────────────────────────

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateStaffRequest {
    /// Admin-only. Omitted on `/staff/me/*`, where it is always the caller.
    #[serde(default)]
    pub user_id: Option<Uuid>,
    /// One of `leave`, `late_arrival`, `early_departure`, `excuse`, `mission`,
    /// `correction`.
    pub kind: String,
    pub on_date: NaiveDate,
    #[serde(default)]
    pub end_date: Option<NaiveDate>,
    #[serde(default)]
    pub from_time: Option<NaiveTime>,
    #[serde(default)]
    pub to_time: Option<NaiveTime>,
    #[serde(default)]
    pub leave_type_id: Option<Uuid>,
    #[serde(default)]
    pub is_half_day: Option<bool>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    /// `correction` only — the record whose punch is wrong.
    #[serde(default)]
    pub attendance_record_id: Option<Uuid>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct RequestDecision {
    /// `approved` | `rejected` | `cancelled`.
    pub status: String,
    #[serde(default)]
    pub note: Option<String>,
    /// Whether the excused time is paid. Applies to `excuse` and
    /// `early_departure`; omitted falls back to the org's
    /// `excused_time_paid_default`.
    #[serde(default)]
    pub is_paid: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpsertLeaveTypeRequest {
    pub name: String,
    #[serde(default)]
    pub is_paid: Option<bool>,
    #[serde(default)]
    pub annual_quota_days: Option<Decimal>,
    #[serde(default)]
    pub requires_approval: Option<bool>,
    #[serde(default)]
    pub is_active: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct PutBalanceRequest {
    pub user_id: Uuid,
    pub leave_type_id: Uuid,
    pub year: i32,
    pub entitled_days: Decimal,
    #[serde(default)]
    pub carried_over_days: Option<Decimal>,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct RequestListQuery {
    #[serde(default)]
    pub user_id: Option<Uuid>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub from: Option<NaiveDate>,
    #[serde(default)]
    pub to: Option<NaiveDate>,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct BalanceQuery {
    #[serde(default)]
    pub user_id: Option<Uuid>,
    /// Defaults to the current calendar year.
    #[serde(default)]
    pub year: Option<i32>,
}

fn validate_status_filter(status: Option<&str>) -> Result<(), AppError> {
    match status {
        None | Some("pending") | Some("approved") | Some("rejected") | Some("cancelled") => Ok(()),
        Some(other) => Err(AppError::BadRequest(format!(
            "Unknown status '{other}' — expected pending, approved, rejected, or cancelled"
        ))),
    }
}

fn validate_kind(kind: &str) -> Result<(), AppError> {
    if KINDS.contains(&kind) {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "Unknown request kind '{kind}' — expected one of {}",
            KINDS.join(", ")
        )))
    }
}

/// A local wall-clock time on a date, resolved to an instant in `timezone`.
///
/// Resolved by POSTGRES rather than chrono-tz, for the same reason the rest of
/// this module does it: the tz database owns DST, and a correction filed on the
/// morning a clock changes must land on the instant the branch actually meant.
async fn local_instant(
    pool: &PgPool,
    date: NaiveDate,
    time: NaiveTime,
    timezone: &str,
) -> Result<DateTime<Utc>, AppError> {
    Ok(
        sqlx::query_scalar::<_, DateTime<Utc>>("SELECT ($1::date + $2::time) AT TIME ZONE $3")
            .bind(date)
            .bind(time)
            .bind(timezone)
            .fetch_one(pool)
            .await?,
    )
}

fn clean(value: Option<&String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Days a leave request consumes: inclusive day count, or half a day when the
/// request is a single-day half.
pub(crate) fn leave_days(start: NaiveDate, end: NaiveDate, is_half_day: bool) -> Decimal {
    if is_half_day {
        return dec!(0.5);
    }
    Decimal::from((end - start).num_days().max(0) + 1)
}

/// Reject a request whose shape its kind does not allow, with a message that says
/// what was missing. The database CHECK enforces the same rules as a backstop;
/// this exists so the API answers "a late arrival needs a time" rather than
/// surfacing a constraint name.
fn validate_shape(body: &CreateStaffRequest) -> Result<(), AppError> {
    let bad = |m: &str| Err(AppError::BadRequest(m.to_string()));
    match body.kind.as_str() {
        "leave" => {
            if body.leave_type_id.is_none() {
                return bad("A leave request needs a leave type");
            }
            let end = body.end_date.unwrap_or(body.on_date);
            if end < body.on_date {
                return bad("End date is before start date");
            }
            if body.is_half_day.unwrap_or(false) && end != body.on_date {
                return bad("A half day must start and end on the same date");
            }
        }
        "late_arrival" => {
            if body.to_time.is_none() {
                return bad("A late arrival needs the time you expect to arrive");
            }
        }
        "early_departure" => {
            if body.from_time.is_none() {
                return bad("An early departure needs the time you expect to leave");
            }
        }
        "excuse" => match (body.from_time, body.to_time) {
            (Some(from), Some(to)) if to > from => {}
            (Some(_), Some(_)) => return bad("The window must end after it starts"),
            _ => return bad("A permission needs a start and an end time"),
        },
        "mission" => {
            if clean(body.title.as_ref()).is_none() {
                return bad("A mission needs a title");
            }
            if body.end_date.unwrap_or(body.on_date) < body.on_date {
                return bad("End date is before start date");
            }
        }
        "correction" => {
            if body.attendance_record_id.is_none() {
                return bad("A correction needs the attendance record it fixes");
            }
            match (body.from_time, body.to_time) {
                (None, None) => return bad("A correction needs a proposed time"),
                (Some(from), Some(to)) if to <= from => {
                    return bad("The check-out must be after the check-in");
                }
                _ => {}
            }
        }
        other => return validate_kind(other),
    }
    Ok(())
}

// ── The classifier's input ────────────────────────────────────

/// Resolve one employee's approved requests for one business date into the
/// adjustments the attendance math consumes.
///
/// Times are stored as the branch's LOCAL wall clock (an employee agreeing to
/// arrive "by 10:00" means 10:00 where they work), so they are converted here
/// with the same `(date + time) AT TIME ZONE` treatment the rest of the module
/// uses — the tz database owns DST, not us.
pub(crate) async fn day_adjustments(
    pool: &PgPool,
    user_id: Uuid,
    business_date: NaiveDate,
    timezone: &str,
    excused_time_paid_default: bool,
) -> Result<DayAdjustments, AppError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        kind: String,
        from_at: Option<DateTime<Utc>>,
        to_at: Option<DateTime<Utc>>,
        is_paid: Option<bool>,
    }

    let rows: Vec<Row> = sqlx::query_as(
        r#"
        SELECT r.kind,
               ($2::date + r.from_time) AT TIME ZONE $3 AS from_at,
               ($2::date + r.to_time)   AT TIME ZONE $3 AS to_at,
               r.is_paid
          FROM staff_requests r
         WHERE r.user_id = $1
           AND r.status = 'approved'
           AND r.on_date <= $2
           AND COALESCE(r.end_date, r.on_date) >= $2
           -- Corrections are excluded on purpose: they REWRITE the punch on
           -- approval rather than forgive a window. Letting one through here
           -- would forgive the very lateness it just recorded.
           AND r.kind <> 'correction'
        "#,
    )
    .bind(user_id)
    .bind(business_date)
    .bind(timezone)
    .fetch_all(pool)
    .await?;

    let mut adj = DayAdjustments {
        excused_time_paid: excused_time_paid_default,
        ..Default::default()
    };
    for row in rows {
        match row.kind.as_str() {
            // Latest wins: two approvals for one morning cannot both hold, and the
            // more generous one is the one the employee was last told about.
            "late_arrival" => {
                if let Some(to) = row.to_at
                    && adj.excused_until.is_none_or(|current| to > current)
                {
                    adj.excused_until = Some(to);
                }
            }
            "early_departure" => {
                if let Some(from) = row.from_at
                    && adj.excused_from.is_none_or(|current| from < current)
                {
                    adj.excused_from = Some(from);
                }
                if let Some(paid) = row.is_paid {
                    adj.excused_time_paid = paid;
                }
            }
            "excuse" => {
                if let (Some(from), Some(to)) = (row.from_at, row.to_at) {
                    adj.excused_windows.push((from, to));
                }
                if let Some(paid) = row.is_paid {
                    adj.excused_time_paid = paid;
                }
            }
            "leave" | "mission" => adj.on_leave = true,
            _ => {}
        }
    }
    Ok(adj)
}

// ── Requests CRUD ─────────────────────────────────────────────

async fn insert_request(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    body: &CreateStaffRequest,
) -> Result<StaffRequest, AppError> {
    validate_kind(&body.kind)?;
    validate_shape(body)?;

    let is_leave = body.kind == "leave";
    let end_date = match body.kind.as_str() {
        "leave" | "mission" => Some(body.end_date.unwrap_or(body.on_date)),
        _ => None,
    };

    if is_leave {
        let type_ok: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM leave_types WHERE id = $1 AND org_id = $2 AND is_active)",
        )
        .bind(body.leave_type_id)
        .bind(org_id)
        .fetch_one(pool)
        .await?;
        if !type_ok {
            return Err(AppError::NotFound(
                "Leave type not found or inactive".into(),
            ));
        }

        // Two live leave requests over the same day would double-count the balance.
        let overlapping: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM staff_requests \
              WHERE user_id = $1 AND kind = 'leave' AND status IN ('pending', 'approved') \
                AND on_date <= $3 AND COALESCE(end_date, on_date) >= $2",
        )
        .bind(user_id)
        .bind(body.on_date)
        .bind(end_date)
        .fetch_one(pool)
        .await?;
        if overlapping > 0 {
            return Err(AppError::Conflict(
                "There is already a pending or approved leave request over those dates".into(),
            ));
        }
    }

    if body.kind == "correction"
        && let Some(record_id) = body.attendance_record_id
    {
        // The record must be the requester's OWN and on the day being corrected.
        // Without this, anyone could file a correction against a colleague's
        // punch and — once a manager waved it through — rewrite their pay.
        let owned: Option<NaiveDate> = sqlx::query_scalar(
            "SELECT business_date FROM attendance_records \
              WHERE id = $1 AND user_id = $2 AND org_id = $3",
        )
        .bind(record_id)
        .bind(user_id)
        .bind(org_id)
        .fetch_optional(pool)
        .await?;
        match owned {
            None => return Err(AppError::NotFound("Attendance record not found".into())),
            Some(date) if date != body.on_date => {
                return Err(AppError::BadRequest(
                    "That record is not on the date you are correcting".into(),
                ));
            }
            Some(_) => {}
        }
    }

    // The partial unique index covers the other kinds: one live request per kind
    // per day, so `ON CONFLICT DO NOTHING` turns a duplicate into a clean 409
    // instead of a constraint error.
    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO staff_requests \
             (org_id, user_id, kind, on_date, end_date, from_time, to_time, \
              leave_type_id, is_half_day, title, location, attendance_record_id, reason) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, COALESCE($9, FALSE), $10, $11, $12, $13) \
         ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(org_id)
    .bind(user_id)
    .bind(&body.kind)
    .bind(body.on_date)
    .bind(end_date)
    .bind(body.from_time)
    .bind(body.to_time)
    .bind(body.leave_type_id)
    .bind(body.is_half_day)
    .bind(clean(body.title.as_ref()))
    .bind(clean(body.location.as_ref()))
    .bind(body.attendance_record_id)
    .bind(clean(body.reason.as_ref()))
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| {
        AppError::Conflict("There is already a live request of that kind for that date".into())
    })?;

    load_request(pool, id).await
}

pub(crate) async fn load_request(pool: &PgPool, id: Uuid) -> Result<StaffRequest, AppError> {
    sqlx::query_as::<_, StaffRequest>(&format!("{REQUEST_SELECT} WHERE r.id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::NotFound("Request not found".into()))
}

#[utoipa::path(
    get, path = "/staff/requests", tag = "staff",
    params(RequestListQuery),
    responses((status = 200, description = "Staff requests", body = Vec<StaffRequest>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_requests(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<RequestListQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "leave", "read").await?;
    let org_id = scope_org(&req, &claims)?;
    validate_status_filter(query.status.as_deref())?;
    if let Some(kind) = query.kind.as_deref() {
        validate_kind(kind)?;
    }

    let rows = sqlx::query_as::<_, StaffRequest>(&format!(
        "{REQUEST_SELECT} \
          WHERE r.org_id = $1 \
            AND ($2::uuid IS NULL OR r.user_id = $2) \
            AND ($3::text IS NULL OR r.kind = $3) \
            AND ($4::text IS NULL OR r.status = $4) \
            AND ($5::date IS NULL OR COALESCE(r.end_date, r.on_date) >= $5) \
            AND ($6::date IS NULL OR r.on_date <= $6) \
          ORDER BY r.status = 'pending' DESC, r.on_date DESC, r.created_at DESC"
    ))
    .bind(org_id)
    .bind(query.user_id)
    .bind(query.kind.as_deref())
    .bind(query.status.as_deref())
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    post, path = "/staff/requests", tag = "staff",
    request_body = CreateStaffRequest,
    responses((status = 201, description = "Request filed", body = StaffRequest), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_request_admin(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateStaffRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "leave", "create").await?;
    let org_id = scope_org(&req, &claims)?;
    let user_id = body
        .user_id
        .ok_or_else(|| AppError::BadRequest("user_id is required".into()))?;
    require_user_in_org(pool.get_ref(), org_id, user_id).await?;

    let row = insert_request(pool.get_ref(), org_id, user_id, &body).await?;
    Ok(HttpResponse::Created().json(row))
}

#[utoipa::path(
    post, path = "/staff/me/requests", tag = "staff",
    request_body = CreateStaffRequest,
    responses((status = 201, description = "Request filed", body = StaffRequest), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_my_request(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateStaffRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let user_id = claims.user_id_safe()?;
    let org_id = my_org(pool.get_ref(), user_id).await?;

    let row = insert_request(pool.get_ref(), org_id, user_id, &body).await?;
    Ok(HttpResponse::Created().json(row))
}

#[utoipa::path(
    get, path = "/staff/me/requests", tag = "staff",
    responses((status = 200, description = "The employee's own requests", body = Vec<StaffRequest>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_requests(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let user_id = claims.user_id_safe()?;
    my_org(pool.get_ref(), user_id).await?;

    let rows = sqlx::query_as::<_, StaffRequest>(&format!(
        "{REQUEST_SELECT} WHERE r.user_id = $1 ORDER BY r.on_date DESC, r.created_at DESC"
    ))
    .bind(user_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    patch, path = "/staff/requests/{id}/decision", tag = "staff",
    params(("id" = Uuid, Path, description = "Request ID")),
    request_body = RequestDecision,
    responses(
        (status = 200, description = "Decision recorded", body = StaffRequest),
        (status = 409, description = "Already decided"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn decide_request(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<RequestDecision>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let caller = claims.user_id_safe()?;
    let decision = validate_decision(&body.status)?;
    let org_id = scope_org(&req, &claims)?;

    #[derive(sqlx::FromRow)]
    struct Row {
        user_id: Uuid,
        kind: String,
        leave_type_id: Option<Uuid>,
        on_date: NaiveDate,
        end_date: Option<NaiveDate>,
        is_half_day: bool,
        status: String,
        from_time: Option<NaiveTime>,
        to_time: Option<NaiveTime>,
        attendance_record_id: Option<Uuid>,
    }

    let mut tx = pool.begin().await?;
    // FOR UPDATE: two managers hitting Approve at once must not both spend the
    // same days of leave.
    let existing: Row = sqlx::query_as(
        "SELECT user_id, kind, leave_type_id, on_date, end_date, is_half_day, status, \
                from_time, to_time, attendance_record_id \
           FROM staff_requests WHERE id = $1 AND org_id = $2 FOR UPDATE",
    )
    .bind(*id)
    .bind(org_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound("Request not found".into()))?;

    // Cancelling one's own pending request needs no permission; deciding someone
    // else's does.
    let self_cancel = decision == "cancelled" && existing.user_id == caller;
    if !self_cancel {
        check_permission(pool.get_ref(), &claims, "leave", "update").await?;
    }

    if existing.status == decision {
        return Err(AppError::Conflict(format!(
            "This request is already {decision}"
        )));
    }
    if existing.status == "rejected" || existing.status == "cancelled" {
        return Err(AppError::Conflict(format!(
            "This request was already {} and cannot be changed",
            existing.status
        )));
    }
    if existing.status == "approved" && decision == "rejected" {
        return Err(AppError::Conflict(
            "An approved request cannot be rejected — cancel it instead".into(),
        ));
    }

    // ── An approved correction rewrites the punch ───────────────
    // Applied BEFORE the status flip, and outside the transaction, on purpose:
    // writing the punch is IDEMPOTENT (the same values produce the same row),
    // whereas approving is not. If the status update then fails, the manager
    // retries and lands in exactly the same place. The reverse order could
    // leave a request marked approved whose punch was never written — a
    // correction that silently did nothing.
    if decision == "approved"
        && existing.kind == "correction"
        && let Some(record_id) = existing.attendance_record_id
    {
        // The BRANCH's clock, not the employee's. A proposed "17:00" means 17:00
        // where the shift was worked, and that is also the timezone the record
        // itself derives against — using the employee's would land the punch on
        // a different instant for anyone assigned to more than one branch.
        let record_branch: Uuid =
            sqlx::query_scalar("SELECT branch_id FROM attendance_records WHERE id = $1")
                .bind(record_id)
                .fetch_optional(pool.get_ref())
                .await?
                .ok_or_else(|| AppError::NotFound("Attendance record not found".into()))?;
        let tz = crate::staff::branch_timezone(pool.get_ref(), record_branch).await?;
        let proposed_in = match existing.from_time {
            Some(t) => Some(local_instant(pool.get_ref(), existing.on_date, t, &tz).await?),
            None => None,
        };
        let proposed_out = match existing.to_time {
            Some(t) => Some(local_instant(pool.get_ref(), existing.on_date, t, &tz).await?),
            None => None,
        };
        crate::staff::attendance::apply_punch_correction(
            pool.get_ref(),
            org_id,
            record_id,
            proposed_in,
            proposed_out,
            None,
            None,
            "Approved punch correction request",
            Some(caller),
        )
        .await?;
    }

    // ── Leave balance arithmetic ────────────────────────────────
    if existing.kind == "leave"
        && let Some(leave_type_id) = existing.leave_type_id
    {
        let end = existing.end_date.unwrap_or(existing.on_date);
        let days = leave_days(existing.on_date, end, existing.is_half_day);
        let year = existing.on_date.year();

        match (existing.status.as_str(), decision) {
            (_, "approved") => {
                sqlx::query(
                    "INSERT INTO leave_balances (org_id, user_id, leave_type_id, year, entitled_days, used_days) \
                     VALUES ($1, $2, $3, $4, 0, $5) \
                     ON CONFLICT (user_id, leave_type_id, year) DO UPDATE SET \
                         used_days = leave_balances.used_days + $5, updated_at = now()",
                )
                .bind(org_id)
                .bind(existing.user_id)
                .bind(leave_type_id)
                .bind(year)
                .bind(days)
                .execute(&mut *tx)
                .await?;
            }
            ("approved", "cancelled") => {
                sqlx::query(
                    "UPDATE leave_balances SET used_days = GREATEST(used_days - $4, 0), updated_at = now() \
                      WHERE user_id = $1 AND leave_type_id = $2 AND year = $3",
                )
                .bind(existing.user_id)
                .bind(leave_type_id)
                .bind(year)
                .bind(days)
                .execute(&mut *tx)
                .await?;
            }
            _ => {}
        }
    }

    // ── is_paid resolution ──────────────────────────────────────
    // Only the window kinds carry a pay decision. The approver's explicit choice
    // wins; otherwise the org default applies at the moment of approval, so later
    // edits to the default never retro-change a decided request.
    let is_paid = if decision == "approved"
        && matches!(existing.kind.as_str(), "excuse" | "early_departure")
    {
        match body.is_paid {
            Some(paid) => Some(paid),
            None => Some(
                sqlx::query_scalar::<_, bool>(
                    "SELECT excused_time_paid_default FROM attendance_settings \
                      WHERE org_id = $1 AND branch_id IS NULL LIMIT 1",
                )
                .bind(org_id)
                .fetch_optional(&mut *tx)
                .await?
                .unwrap_or(true),
            ),
        }
    } else {
        None
    };

    sqlx::query(
        "UPDATE staff_requests SET status = $2, decided_by = $3, \
             decided_at = CASE WHEN $2 = 'cancelled' THEN decided_at ELSE now() END, \
             decision_note = $4, is_paid = COALESCE($5, is_paid), updated_at = now() \
          WHERE id = $1",
    )
    .bind(*id)
    .bind(decision)
    .bind(caller)
    .bind(clean(body.note.as_ref()))
    .bind(is_paid)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let row = load_request(pool.get_ref(), *id).await?;
    Ok(HttpResponse::Ok().json(row))
}

// ── Leave types ───────────────────────────────────────────────

#[utoipa::path(
    get, path = "/staff/leave/types", tag = "staff",
    responses((status = 200, description = "Leave types", body = Vec<LeaveType>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_leave_types(
    req: HttpRequest,
    pool: crate::db::Db,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "leave", "read").await?;
    let org_id = scope_org(&req, &claims)?;

    let rows = sqlx::query_as::<_, LeaveType>(&format!(
        "SELECT {LEAVE_TYPE_COLS} FROM leave_types WHERE org_id = $1 ORDER BY lower(name)"
    ))
    .bind(org_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    post, path = "/staff/leave/types", tag = "staff",
    request_body = UpsertLeaveTypeRequest,
    responses((status = 201, description = "Leave type created", body = LeaveType), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_leave_type(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<UpsertLeaveTypeRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "leave", "create").await?;
    let org_id = scope_org(&req, &claims)?;

    let name = body.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("Leave type name is required".into()));
    }
    if body.annual_quota_days.is_some_and(|q| q < Decimal::ZERO) {
        return Err(AppError::BadRequest("Quota cannot be negative".into()));
    }

    let row = sqlx::query_as::<_, LeaveType>(&format!(
        "INSERT INTO leave_types (org_id, name, is_paid, annual_quota_days, requires_approval, is_active) \
         VALUES ($1, $2, COALESCE($3, TRUE), $4, COALESCE($5, TRUE), COALESCE($6, TRUE)) \
         RETURNING {LEAVE_TYPE_COLS}"
    ))
    .bind(org_id)
    .bind(name)
    .bind(body.is_paid)
    .bind(body.annual_quota_days)
    .bind(body.requires_approval)
    .bind(body.is_active)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Created().json(row))
}

#[utoipa::path(
    patch, path = "/staff/leave/types/{id}", tag = "staff",
    params(("id" = Uuid, Path, description = "Leave type ID")),
    request_body = UpsertLeaveTypeRequest,
    responses((status = 200, description = "Leave type updated", body = LeaveType), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_leave_type(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<UpsertLeaveTypeRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "leave", "update").await?;
    let org_id = scope_org(&req, &claims)?;

    let name = body.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("Leave type name is required".into()));
    }

    let row = sqlx::query_as::<_, LeaveType>(&format!(
        "UPDATE leave_types SET name = $3, \
             is_paid = COALESCE($4, is_paid), \
             annual_quota_days = $5, \
             requires_approval = COALESCE($6, requires_approval), \
             is_active = COALESCE($7, is_active), \
             updated_at = now() \
          WHERE id = $1 AND org_id = $2 RETURNING {LEAVE_TYPE_COLS}"
    ))
    .bind(*id)
    .bind(org_id)
    .bind(name)
    .bind(body.is_paid)
    .bind(body.annual_quota_days)
    .bind(body.requires_approval)
    .bind(body.is_active)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Leave type not found".into()))?;
    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    delete, path = "/staff/leave/types/{id}", tag = "staff",
    params(("id" = Uuid, Path, description = "Leave type ID")),
    responses((status = 204, description = "Leave type deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_leave_type(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "leave", "delete").await?;
    let org_id = scope_org(&req, &claims)?;

    // The FK is ON DELETE RESTRICT: a type someone has taken leave under is part
    // of the record. Say so instead of surfacing a foreign-key violation.
    let used: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM staff_requests WHERE leave_type_id = $1")
            .bind(*id)
            .fetch_one(pool.get_ref())
            .await?;
    if used > 0 {
        return Err(AppError::BadRequest(
            "This leave type has requests against it — deactivate it instead of deleting".into(),
        ));
    }

    let deleted = sqlx::query("DELETE FROM leave_types WHERE id = $1 AND org_id = $2")
        .bind(*id)
        .bind(org_id)
        .execute(pool.get_ref())
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(AppError::NotFound("Leave type not found".into()));
    }
    Ok(HttpResponse::NoContent().finish())
}

// ── Balances ──────────────────────────────────────────────────

#[utoipa::path(
    get, path = "/staff/leave/balances", tag = "staff",
    params(BalanceQuery),
    responses((status = 200, description = "Leave balances", body = Vec<LeaveBalance>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_balances(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<BalanceQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "leave", "read").await?;
    let org_id = scope_org(&req, &claims)?;
    let year = query.year.unwrap_or_else(|| Utc::now().year());

    let rows = sqlx::query_as::<_, LeaveBalance>(&format!(
        "{BALANCE_SELECT} WHERE b.org_id = $1 AND b.year = $2 \
          AND ($3::uuid IS NULL OR b.user_id = $3) ORDER BY lower(t.name)"
    ))
    .bind(org_id)
    .bind(year)
    .bind(query.user_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    put, path = "/staff/leave/balances", tag = "staff",
    request_body = PutBalanceRequest,
    responses((status = 200, description = "Balance saved", body = LeaveBalance), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_balance(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<PutBalanceRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "leave", "update").await?;
    let org_id = scope_org(&req, &claims)?;
    require_user_in_org(pool.get_ref(), org_id, body.user_id).await?;

    if body.entitled_days < Decimal::ZERO
        || body.carried_over_days.is_some_and(|c| c < Decimal::ZERO)
    {
        return Err(AppError::BadRequest("Day counts cannot be negative".into()));
    }

    // `used_days` is deliberately absent from the update: it belongs to the
    // approval path, and letting HR set it by hand would desynchronise it from
    // the approved requests it is supposed to mirror.
    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO leave_balances (org_id, user_id, leave_type_id, year, entitled_days, carried_over_days) \
         VALUES ($1, $2, $3, $4, $5, COALESCE($6, 0)) \
         ON CONFLICT (user_id, leave_type_id, year) DO UPDATE SET \
             entitled_days     = EXCLUDED.entitled_days, \
             carried_over_days = EXCLUDED.carried_over_days, \
             updated_at        = now() \
         RETURNING id",
    )
    .bind(org_id)
    .bind(body.user_id)
    .bind(body.leave_type_id)
    .bind(body.year)
    .bind(body.entitled_days)
    .bind(body.carried_over_days)
    .fetch_one(pool.get_ref())
    .await?;

    let row = sqlx::query_as::<_, LeaveBalance>(&format!("{BALANCE_SELECT} WHERE b.id = $1"))
        .bind(id)
        .fetch_one(pool.get_ref())
        .await?;
    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    get, path = "/staff/me/leave-balances", tag = "staff",
    params(BalanceQuery),
    responses((status = 200, description = "The employee's own balances", body = Vec<LeaveBalance>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_leave_balances(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<BalanceQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let user_id = claims.user_id_safe()?;
    my_org(pool.get_ref(), user_id).await?;
    let year = query.year.unwrap_or_else(|| Utc::now().year());

    let rows = sqlx::query_as::<_, LeaveBalance>(&format!(
        "{BALANCE_SELECT} WHERE b.user_id = $1 AND b.year = $2 ORDER BY lower(t.name)"
    ))
    .bind(user_id)
    .bind(year)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

// ── Shared ────────────────────────────────────────────────────

/// The org of the caller's own staff profile. Self-service endpoints scope by
/// this rather than by the token's org claim, so a login without a profile gets
/// a clear 403 instead of an empty list.
pub(crate) async fn my_org(pool: &PgPool, user_id: Uuid) -> Result<Uuid, AppError> {
    sqlx::query_scalar("SELECT org_id FROM staff_profiles WHERE user_id = $1")
        .bind(user_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| {
            AppError::Forbidden(
                "You do not have an employee profile yet — ask your manager to set one up".into(),
            )
        })
}
