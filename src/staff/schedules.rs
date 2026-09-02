//! Work shifts and the roster: who is expected where, when.
//!
//! Three layers resolve an employee's expected hours for a date, highest first:
//!
//!   1. `staff_schedule_overrides` for that exact date — a NULL shift is an
//!      explicit day off and outranks everything.
//!   2. `staff_schedules` rows whose `day_of_week` matches.
//!   3. `staff_schedules` rows with `day_of_week IS NULL` (every day).
//!
//! A weekday no row covers is simply a rest day; rotating patterns are dated
//! bands of weekly rows. When several shifts survive at the same layer the day is
//! genuinely multi-shift (split morning/evening), and
//! [`pick_shift_for_instant`] chooses between them at check-in time.
//!
//! The scheduled window is materialised in POSTGRES, not Rust:
//! `(date + time) AT TIME ZONE <branch tz>`. That makes the tz database — not us
//! — responsible for DST, which matters because a shift that starts at 09:00
//! local is 09:00 local on both sides of a clock change.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    errors::{AppError, AppErrorResponse},
    orgs::handlers::extract_claims,
    permissions::checker::check_permission,
    staff::{DEFAULT_TZ, require_user_in_org, rules::ShiftRules, scope_org},
};

// ── Models ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct WorkShift {
    pub id: Uuid,
    pub org_id: Uuid,
    /// `None` = an org-wide template usable at any branch.
    pub branch_id: Option<Uuid>,
    pub name: String,
    pub start_time: NaiveTime,
    pub end_time: NaiveTime,
    /// Derived by the database from `end_time <= start_time`.
    pub crosses_midnight: bool,
    pub grace_minutes: i32,
    pub break_minutes: i32,
    pub paid_break: bool,
    pub half_day_threshold_minutes: Option<i32>,
    pub overtime_threshold_minutes: i32,
    pub overtime_multiplier: Decimal,
    pub checkin_window_minutes: i32,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const WORK_SHIFT_COLS: &str = "id, org_id, branch_id, name, start_time, end_time, \
     crosses_midnight, grace_minutes, break_minutes, paid_break, half_day_threshold_minutes, \
     overtime_threshold_minutes, overtime_multiplier, checkin_window_minutes, is_active, \
     created_at, updated_at";

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct ScheduleAssignment {
    pub id: Uuid,
    pub org_id: Uuid,
    pub user_id: Uuid,
    pub work_shift_id: Uuid,
    #[sqlx(default)]
    pub work_shift_name: Option<String>,
    /// Postgres `EXTRACT(DOW)` convention: 0 = Sunday … 6 = Saturday.
    /// `None` = every day of the week.
    pub day_of_week: Option<i16>,
    pub effective_from: NaiveDate,
    pub effective_to: Option<NaiveDate>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct ScheduleOverride {
    pub id: Uuid,
    pub org_id: Uuid,
    pub user_id: Uuid,
    pub on_date: NaiveDate,
    /// `None` = an explicit day off.
    pub work_shift_id: Option<Uuid>,
    #[sqlx(default)]
    pub work_shift_name: Option<String>,
    pub reason: Option<String>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// A work shift resolved onto a concrete calendar date, with its window already
/// converted to UTC instants.
#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct ResolvedShift {
    pub work_shift_id: Uuid,
    pub name: String,
    pub grace_minutes: i32,
    pub break_minutes: i32,
    pub paid_break: bool,
    pub half_day_threshold_minutes: Option<i32>,
    pub overtime_threshold_minutes: i32,
    pub overtime_multiplier: Decimal,
    pub checkin_window_minutes: i32,
    pub scheduled_start_at: DateTime<Utc>,
    pub scheduled_end_at: DateTime<Utc>,
}

impl ResolvedShift {
    /// The tolerances the pure math in [`crate::staff::rules`] needs.
    pub fn rules(&self) -> ShiftRules {
        ShiftRules {
            grace_minutes: self.grace_minutes,
            break_minutes: self.break_minutes,
            paid_break: self.paid_break,
            half_day_threshold_minutes: self.half_day_threshold_minutes,
            overtime_threshold_minutes: self.overtime_threshold_minutes,
            overtime_multiplier: self.overtime_multiplier,
        }
    }

    /// Scheduled length in minutes — the half-day fallback and the payroll
    /// per-minute divisor.
    pub fn span_minutes(&self) -> i64 {
        (self.scheduled_end_at - self.scheduled_start_at)
            .num_minutes()
            .max(0)
    }
}

// ── Requests ──────────────────────────────────────────────────

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UpsertWorkShiftRequest {
    #[serde(default)]
    pub branch_id: Option<Uuid>,
    pub name: String,
    pub start_time: NaiveTime,
    pub end_time: NaiveTime,
    #[serde(default)]
    pub grace_minutes: Option<i32>,
    #[serde(default)]
    pub break_minutes: Option<i32>,
    #[serde(default)]
    pub paid_break: Option<bool>,
    #[serde(default)]
    pub half_day_threshold_minutes: Option<i32>,
    #[serde(default)]
    pub overtime_threshold_minutes: Option<i32>,
    #[serde(default)]
    pub overtime_multiplier: Option<Decimal>,
    #[serde(default)]
    pub checkin_window_minutes: Option<i32>,
    #[serde(default)]
    pub is_active: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateAssignmentRequest {
    pub user_id: Uuid,
    pub work_shift_id: Uuid,
    /// 0 = Sunday … 6 = Saturday. Omit for "every day".
    #[serde(default)]
    pub day_of_week: Option<i16>,
    #[serde(default)]
    pub effective_from: Option<NaiveDate>,
    #[serde(default)]
    pub effective_to: Option<NaiveDate>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct PutOverrideRequest {
    pub user_id: Uuid,
    pub on_date: NaiveDate,
    /// Omit (or send null) to mark the date an explicit day off.
    #[serde(default)]
    pub work_shift_id: Option<Uuid>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct UserQuery {
    /// Omit for the WHOLE org's roster — what a schedule grid needs, and the
    /// only way to draw one without a request per employee.
    #[serde(default)]
    pub user_id: Option<Uuid>,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct DayQuery {
    pub user_id: Uuid,
    pub date: NaiveDate,
    /// Which branch's timezone the day is measured in. Defaults to the
    /// employee's only branch assignment when they have exactly one.
    #[serde(default)]
    pub branch_id: Option<Uuid>,
}

// ── Resolution ────────────────────────────────────────────────

/// Every shift the employee is expected to work on `date`, ordered by start time.
///
/// Empty means a rest day: either an explicit override with no shift, or simply
/// nothing rostered.
pub(crate) async fn resolve_shifts_for(
    pool: &PgPool,
    user_id: Uuid,
    date: NaiveDate,
    timezone: &str,
) -> Result<Vec<ResolvedShift>, AppError> {
    let rows = sqlx::query_as::<_, ResolvedShift>(
        r#"
        WITH ov AS (
            SELECT work_shift_id
              FROM staff_schedule_overrides
             WHERE user_id = $1 AND on_date = $2
        ),
        -- A weekday-specific row beats the every-day catch-all; only the winning
        -- tier survives, so a Tuesday special does not stack with the default.
        ranked AS (
            SELECT s.work_shift_id,
                   CASE WHEN s.day_of_week IS NOT NULL THEN 0 ELSE 1 END AS pri
              FROM staff_schedules s
             WHERE NOT EXISTS (SELECT 1 FROM ov)
               AND s.user_id = $1
               AND s.effective_from <= $2
               AND (s.effective_to IS NULL OR s.effective_to >= $2)
               AND (s.day_of_week IS NULL
                    OR s.day_of_week = EXTRACT(DOW FROM $2::date)::smallint)
        ),
        picked AS (
            SELECT work_shift_id FROM ov WHERE work_shift_id IS NOT NULL
            UNION
            SELECT work_shift_id FROM ranked
             WHERE pri = (SELECT MIN(pri) FROM ranked)
        )
        SELECT ws.id AS work_shift_id, ws.name, ws.grace_minutes, ws.break_minutes,
               ws.paid_break, ws.half_day_threshold_minutes,
               ws.overtime_threshold_minutes, ws.overtime_multiplier,
               ws.checkin_window_minutes,
               ($2::date + ws.start_time) AT TIME ZONE $3 AS scheduled_start_at,
               ($2::date + ws.end_time
                    + CASE WHEN ws.crosses_midnight
                           THEN INTERVAL '1 day' ELSE INTERVAL '0 day' END
               ) AT TIME ZONE $3 AS scheduled_end_at
          FROM picked p
          JOIN work_shifts ws ON ws.id = p.work_shift_id
         WHERE ws.is_active
         ORDER BY ws.start_time
        "#,
    )
    .bind(user_id)
    .bind(date)
    .bind(timezone)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One day of an employee's own upcoming roster.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ScheduledDay {
    pub date: NaiveDate,
    /// Empty = a rest day.
    pub shifts: Vec<ResolvedShift>,
    /// The branch each shift is worked at, when the employee has one assignment.
    pub branch_name: Option<String>,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct MyScheduleQuery {
    pub from: NaiveDate,
    pub to: NaiveDate,
}

/// The employee's OWN roster for a date range — what the app's Shifts tab shows.
///
/// Own-row scoped like the rest of `/staff/me/*`: it needs no permission grant,
/// because seeing when you are expected at work is not an admin capability.
#[utoipa::path(
    get, path = "/staff/me/schedule", tag = "staff",
    params(MyScheduleQuery),
    responses((status = 200, description = "The employee's roster, one entry per day", body = Vec<ScheduledDay>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_schedule(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<MyScheduleQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let user_id = claims.user_id_safe()?;
    let org_id = crate::staff::attendance::require_active_profile(pool.get_ref(), user_id).await?;

    if query.to < query.from {
        return Err(AppError::BadRequest("`to` is before `from`".into()));
    }
    // A phone shows a week or a month; anything larger is a scrape, not a screen.
    let span = (query.to - query.from).num_days();
    if span > 62 {
        return Err(AppError::BadRequest(
            "Range too wide — request 62 days or fewer".into(),
        ));
    }

    let tz = employee_timezone(pool.get_ref(), org_id, user_id).await?;
    // Named only when the employee has exactly ONE live branch — the same rule
    // check-in uses. Someone assigned to two branches has no single "their
    // branch" to print under a shift, so the row stays unlabelled.
    let branch_name: Option<String> = sqlx::query_scalar(
        "SELECT name FROM (
             SELECT b.name, COUNT(*) OVER () AS n
               FROM user_branch_assignments uba
               JOIN branches b ON b.id = uba.branch_id
                              AND b.deleted_at IS NULL
                              AND b.org_id = $2
              WHERE uba.user_id = $1
         ) assignments
          WHERE n = 1",
    )
    .bind(user_id)
    .bind(org_id)
    .fetch_optional(pool.get_ref())
    .await?;

    let mut days = Vec::with_capacity(span as usize + 1);
    let mut date = query.from;
    while date <= query.to {
        days.push(ScheduledDay {
            date,
            shifts: resolve_shifts_for(pool.get_ref(), user_id, date, &tz).await?,
            branch_name: branch_name.clone(),
        });
        date = date.succ_opt().unwrap_or(date);
        if days.len() > 63 {
            break;
        }
    }
    Ok(HttpResponse::Ok().json(days))
}

/// Pick which of a multi-shift day's shifts an event at `instant` belongs to.
///
/// Nearest scheduled start wins, so a 06:50 punch lands on the morning shift and
/// an 17:10 punch on the evening one. Deliberately NOT filtered by the check-in
/// window: an employee arriving three hours early still belongs to *some* shift,
/// and the window's job is to gate the check-in, not to erase the association.
pub(crate) fn pick_shift_for_instant(
    candidates: &[ResolvedShift],
    instant: DateTime<Utc>,
) -> Option<&ResolvedShift> {
    candidates
        .iter()
        .min_by_key(|s| (s.scheduled_start_at - instant).num_seconds().abs())
}

// ── Work shifts ───────────────────────────────────────────────

fn validate_work_shift(body: &UpsertWorkShiftRequest) -> Result<String, AppError> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("Shift name is required".into()));
    }
    if body.start_time == body.end_time {
        return Err(AppError::BadRequest(
            "A shift cannot start and end at the same time".into(),
        ));
    }
    for (label, value) in [
        ("grace_minutes", body.grace_minutes),
        ("break_minutes", body.break_minutes),
        (
            "overtime_threshold_minutes",
            body.overtime_threshold_minutes,
        ),
    ] {
        if value.is_some_and(|v| v < 0) {
            return Err(AppError::BadRequest(format!("{label} cannot be negative")));
        }
    }
    if body.checkin_window_minutes.is_some_and(|v| v <= 0) {
        return Err(AppError::BadRequest(
            "checkin_window_minutes must be positive".into(),
        ));
    }
    if body.half_day_threshold_minutes.is_some_and(|v| v <= 0) {
        return Err(AppError::BadRequest(
            "half_day_threshold_minutes must be positive".into(),
        ));
    }
    if body.overtime_multiplier.is_some_and(|v| v <= Decimal::ZERO) {
        return Err(AppError::BadRequest(
            "overtime_multiplier must be positive".into(),
        ));
    }
    Ok(name.to_string())
}

#[utoipa::path(
    get, path = "/staff/work-shifts", tag = "staff",
    responses((status = 200, description = "Work shifts in the org", body = Vec<WorkShift>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_work_shifts(
    req: HttpRequest,
    pool: crate::db::Db,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "work_shifts", "read").await?;
    let org_id = scope_org(&req, &claims)?;

    let rows = sqlx::query_as::<_, WorkShift>(&format!(
        "SELECT {WORK_SHIFT_COLS} FROM work_shifts WHERE org_id = $1 ORDER BY start_time, lower(name)"
    ))
    .bind(org_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    post, path = "/staff/work-shifts", tag = "staff",
    request_body = UpsertWorkShiftRequest,
    responses((status = 201, description = "Work shift created", body = WorkShift), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_work_shift(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<UpsertWorkShiftRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "work_shifts", "create").await?;
    let org_id = scope_org(&req, &claims)?;
    let name = validate_work_shift(&body)?;

    let row = sqlx::query_as::<_, WorkShift>(&format!(
        r#"
        INSERT INTO work_shifts (
            org_id, branch_id, name, start_time, end_time, grace_minutes, break_minutes,
            paid_break, half_day_threshold_minutes, overtime_threshold_minutes,
            overtime_multiplier, checkin_window_minutes, is_active
        ) VALUES (
            $1, $2, $3, $4, $5, COALESCE($6, 15), COALESCE($7, 0),
            COALESCE($8, TRUE), $9, COALESCE($10, 15),
            COALESCE($11, 1.50), COALESCE($12, 120), COALESCE($13, TRUE)
        ) RETURNING {WORK_SHIFT_COLS}
        "#
    ))
    .bind(org_id)
    .bind(body.branch_id)
    .bind(&name)
    .bind(body.start_time)
    .bind(body.end_time)
    .bind(body.grace_minutes)
    .bind(body.break_minutes)
    .bind(body.paid_break)
    .bind(body.half_day_threshold_minutes)
    .bind(body.overtime_threshold_minutes)
    .bind(body.overtime_multiplier)
    .bind(body.checkin_window_minutes)
    .bind(body.is_active)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Created().json(row))
}

#[utoipa::path(
    patch, path = "/staff/work-shifts/{id}", tag = "staff",
    params(("id" = Uuid, Path, description = "Work shift ID")),
    request_body = UpsertWorkShiftRequest,
    responses((status = 200, description = "Work shift updated", body = WorkShift), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn update_work_shift(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<UpsertWorkShiftRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "work_shifts", "update").await?;
    let org_id = scope_org(&req, &claims)?;
    let name = validate_work_shift(&body)?;

    // Editing a shift never rewrites history: attendance rows carry their own
    // snapshot of the window they were judged against.
    let row = sqlx::query_as::<_, WorkShift>(&format!(
        r#"
        UPDATE work_shifts SET
            branch_id                  = $3,
            name                       = $4,
            start_time                 = $5,
            end_time                   = $6,
            grace_minutes              = COALESCE($7, grace_minutes),
            break_minutes              = COALESCE($8, break_minutes),
            paid_break                 = COALESCE($9, paid_break),
            half_day_threshold_minutes = $10,
            overtime_threshold_minutes = COALESCE($11, overtime_threshold_minutes),
            overtime_multiplier        = COALESCE($12, overtime_multiplier),
            checkin_window_minutes     = COALESCE($13, checkin_window_minutes),
            is_active                  = COALESCE($14, is_active),
            updated_at                 = now()
         WHERE id = $1 AND org_id = $2
        RETURNING {WORK_SHIFT_COLS}
        "#
    ))
    .bind(*id)
    .bind(org_id)
    .bind(body.branch_id)
    .bind(&name)
    .bind(body.start_time)
    .bind(body.end_time)
    .bind(body.grace_minutes)
    .bind(body.break_minutes)
    .bind(body.paid_break)
    .bind(body.half_day_threshold_minutes)
    .bind(body.overtime_threshold_minutes)
    .bind(body.overtime_multiplier)
    .bind(body.checkin_window_minutes)
    .bind(body.is_active)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Work shift not found".into()))?;
    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    delete, path = "/staff/work-shifts/{id}", tag = "staff",
    params(("id" = Uuid, Path, description = "Work shift ID")),
    responses((status = 204, description = "Work shift deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_work_shift(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "work_shifts", "delete").await?;
    let org_id = scope_org(&req, &claims)?;

    // Attendance keeps its rows (the FK is ON DELETE SET NULL) but the roster
    // cascades, which would silently unschedule people. Make that explicit.
    let assigned: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM staff_schedules WHERE work_shift_id = $1 AND org_id = $2",
    )
    .bind(*id)
    .bind(org_id)
    .fetch_one(pool.get_ref())
    .await?;
    if assigned > 0 {
        return Err(AppError::BadRequest(format!(
            "{assigned} roster assignment(s) still use this shift — remove them first, \
             or deactivate the shift instead"
        )));
    }

    let deleted = sqlx::query("DELETE FROM work_shifts WHERE id = $1 AND org_id = $2")
        .bind(*id)
        .bind(org_id)
        .execute(pool.get_ref())
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(AppError::NotFound("Work shift not found".into()));
    }
    Ok(HttpResponse::NoContent().finish())
}

// ── Roster assignments ────────────────────────────────────────

#[utoipa::path(
    get, path = "/staff/schedules", tag = "staff",
    params(UserQuery),
    responses((status = 200, description = "The employee's roster", body = Vec<ScheduleAssignment>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_assignments(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<UserQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "work_shifts", "read").await?;
    let org_id = scope_org(&req, &claims)?;

    let rows = sqlx::query_as::<_, ScheduleAssignment>(
        "SELECT s.id, s.org_id, s.user_id, s.work_shift_id, ws.name AS work_shift_name, \
                s.day_of_week, s.effective_from, s.effective_to, s.created_at \
           FROM staff_schedules s \
           JOIN work_shifts ws ON ws.id = s.work_shift_id \
          WHERE ($1::uuid IS NULL OR s.user_id = $1) AND s.org_id = $2 \
          ORDER BY s.user_id, s.effective_from DESC, s.day_of_week NULLS LAST, ws.start_time",
    )
    .bind(query.user_id)
    .bind(org_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    post, path = "/staff/schedules", tag = "staff",
    request_body = CreateAssignmentRequest,
    responses((status = 201, description = "Assignment created", body = ScheduleAssignment), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_assignment(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateAssignmentRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "work_shifts", "update").await?;
    let org_id = scope_org(&req, &claims)?;
    require_user_in_org(pool.get_ref(), org_id, body.user_id).await?;

    if body.day_of_week.is_some_and(|d| !(0..=6).contains(&d)) {
        return Err(AppError::BadRequest(
            "day_of_week must be 0 (Sunday) through 6 (Saturday)".into(),
        ));
    }
    if let (Some(from), Some(to)) = (body.effective_from, body.effective_to)
        && to < from
    {
        return Err(AppError::BadRequest(
            "effective_to is before effective_from".into(),
        ));
    }
    let shift_ok: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM work_shifts WHERE id = $1 AND org_id = $2)",
    )
    .bind(body.work_shift_id)
    .bind(org_id)
    .fetch_one(pool.get_ref())
    .await?;
    if !shift_ok {
        return Err(AppError::NotFound("Work shift not found".into()));
    }

    let row = sqlx::query_as::<_, ScheduleAssignment>(
        "WITH ins AS (
             INSERT INTO staff_schedules
                 (org_id, user_id, work_shift_id, day_of_week, effective_from, effective_to)
             VALUES ($1, $2, $3, $4, COALESCE($5, CURRENT_DATE), $6)
             RETURNING *
         )
         SELECT ins.id, ins.org_id, ins.user_id, ins.work_shift_id, ws.name AS work_shift_name,
                ins.day_of_week, ins.effective_from, ins.effective_to, ins.created_at
           FROM ins JOIN work_shifts ws ON ws.id = ins.work_shift_id",
    )
    .bind(org_id)
    .bind(body.user_id)
    .bind(body.work_shift_id)
    .bind(body.day_of_week)
    .bind(body.effective_from)
    .bind(body.effective_to)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Created().json(row))
}

#[utoipa::path(
    delete, path = "/staff/schedules/{id}", tag = "staff",
    params(("id" = Uuid, Path, description = "Assignment ID")),
    responses((status = 204, description = "Assignment removed"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_assignment(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "work_shifts", "update").await?;
    let org_id = scope_org(&req, &claims)?;

    let deleted = sqlx::query("DELETE FROM staff_schedules WHERE id = $1 AND org_id = $2")
        .bind(*id)
        .bind(org_id)
        .execute(pool.get_ref())
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(AppError::NotFound("Assignment not found".into()));
    }
    Ok(HttpResponse::NoContent().finish())
}

// ── Per-date overrides ────────────────────────────────────────

#[utoipa::path(
    put, path = "/staff/schedules/overrides", tag = "staff",
    request_body = PutOverrideRequest,
    responses((status = 200, description = "Override saved", body = ScheduleOverride), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_override(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<PutOverrideRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "work_shifts", "update").await?;
    let org_id = scope_org(&req, &claims)?;
    require_user_in_org(pool.get_ref(), org_id, body.user_id).await?;

    if let Some(shift_id) = body.work_shift_id {
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM work_shifts WHERE id = $1 AND org_id = $2)",
        )
        .bind(shift_id)
        .bind(org_id)
        .fetch_one(pool.get_ref())
        .await?;
        if !ok {
            return Err(AppError::NotFound("Work shift not found".into()));
        }
    }

    let row = sqlx::query_as::<_, ScheduleOverride>(
        "WITH up AS (
             INSERT INTO staff_schedule_overrides
                 (org_id, user_id, on_date, work_shift_id, reason, created_by)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (user_id, on_date) DO UPDATE SET
                 work_shift_id = EXCLUDED.work_shift_id,
                 reason        = EXCLUDED.reason,
                 created_by    = EXCLUDED.created_by
             RETURNING *
         )
         SELECT up.id, up.org_id, up.user_id, up.on_date, up.work_shift_id,
                ws.name AS work_shift_name, up.reason, up.created_by, up.created_at
           FROM up LEFT JOIN work_shifts ws ON ws.id = up.work_shift_id",
    )
    .bind(org_id)
    .bind(body.user_id)
    .bind(body.on_date)
    .bind(body.work_shift_id)
    .bind(
        body.reason
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty()),
    )
    .bind(claims.user_id())
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(row))
}

#[utoipa::path(
    delete, path = "/staff/schedules/overrides/{id}", tag = "staff",
    params(("id" = Uuid, Path, description = "Override ID")),
    responses((status = 204, description = "Override removed"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_override(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "work_shifts", "update").await?;
    let org_id = scope_org(&req, &claims)?;

    let deleted = sqlx::query("DELETE FROM staff_schedule_overrides WHERE id = $1 AND org_id = $2")
        .bind(*id)
        .bind(org_id)
        .execute(pool.get_ref())
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(AppError::NotFound("Override not found".into()));
    }
    Ok(HttpResponse::NoContent().finish())
}

// ── Resolved day ──────────────────────────────────────────────

#[utoipa::path(
    get, path = "/staff/schedules/day", tag = "staff",
    params(DayQuery),
    responses(
        (status = 200, description = "Shifts the employee is expected to work that day (empty = rest day)", body = Vec<ResolvedShift>),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn get_scheduled_day(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<DayQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "work_shifts", "read").await?;
    let org_id = scope_org(&req, &claims)?;
    require_user_in_org(pool.get_ref(), org_id, query.user_id).await?;

    let tz = match query.branch_id {
        Some(branch_id) => crate::staff::branch_timezone(pool.get_ref(), branch_id).await?,
        None => employee_timezone(pool.get_ref(), org_id, query.user_id).await?,
    };
    let rows = resolve_shifts_for(pool.get_ref(), query.user_id, query.date, &tz).await?;
    Ok(HttpResponse::Ok().json(rows))
}

/// The timezone an employee's day is measured in when no branch is named.
///
/// Uses their single branch assignment when they have exactly one — the common
/// case — and the org's timezone otherwise, because an employee who floats
/// between branches has no one "home" clock.
/// The org's own clock — used where there is no single employee to derive one
/// from, e.g. a manager's team view spanning every branch.
pub(crate) async fn org_timezone(pool: &PgPool, org_id: Uuid) -> Result<String, AppError> {
    let tz: Option<String> =
        sqlx::query_scalar("SELECT timezone::text FROM organizations WHERE id = $1")
            .bind(org_id)
            .fetch_optional(pool)
            .await?
            .flatten();
    Ok(tz.unwrap_or_else(|| DEFAULT_TZ.to_string()))
}

pub(crate) async fn employee_timezone(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
) -> Result<String, AppError> {
    let tz: Option<String> = sqlx::query_scalar(
        r#"
        SELECT COALESCE(
            -- MIN() over an aggregate-only subquery: HAVING COUNT(*) = 1 makes it
            -- return NULL (no row) unless the employee has exactly one branch,
            -- which is what "their branch's clock" means.
            (SELECT MIN(b.timezone::text)
               FROM user_branch_assignments uba
               JOIN branches b ON b.id = uba.branch_id AND b.deleted_at IS NULL
              WHERE uba.user_id = $1
             HAVING COUNT(*) = 1),
            (SELECT o.timezone::text FROM organizations o WHERE o.id = $2),
            $3
        )
        "#,
    )
    .bind(user_id)
    .bind(org_id)
    .bind(DEFAULT_TZ)
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(tz.unwrap_or_else(|| DEFAULT_TZ.to_string()))
}
