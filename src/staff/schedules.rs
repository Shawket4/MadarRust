//! Work shifts and the roster: who is expected where, when.
//!
//! ONE FUNCTION decides who works which shift on a date (SC-6, AT-9): the SQL
//! function `dawam_roster` (migration 20260930100000), reached from Rust through
//! [`resolve_range`] / [`resolve_shifts_for`]. Clock-in, the absence sweep, team
//! presence, the roster views, swaps, covers and payroll all call it. Highest
//! first:
//!
//!   1. `staff_schedule_overrides` for that exact date — the date's own SET of
//!      assignments (several on a split day, SC-11); a NULL shift is an explicit
//!      day off and outranks everything.
//!   2. `staff_schedules` rows whose `day_of_week` matches.
//!   3. `staff_schedules` rows with `day_of_week IS NULL` (every day).
//!
//! A block (work shift) is only rostered on its `valid_days`. Its EFFECTIVE times
//! are the assignment's own from/to, else the block's time for that weekday,
//! else the block's default; an end at or before the start runs into the next
//! day, and the shift still belongs to the day it starts (SC-10).
//!
//! The scheduled window is materialised in POSTGRES, not Rust:
//! `(date + time) AT TIME ZONE <branch tz>`. That makes the tz database — not us
//! — responsible for DST, which matters because a shift that starts at 09:00
//! local is 09:00 local on both sides of a clock change.

use std::collections::BTreeSet;

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    authz::Cap,
    errors::{AppError, AppErrorResponse},
    staff::{
        DEFAULT_TZ, access,
        dawam::engine::{self, LabourWarning},
        days::{self, Block},
        principal::{Me, caller},
        rules::ShiftRules,
        scope_org,
    },
};

// ── Models ────────────────────────────────────────────────────

/// A block's own start/end on one weekday, on top of its default times.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, sqlx::FromRow, ToSchema)]
pub struct DayTime {
    /// 0 = Sunday … 6 = Saturday.
    pub day_of_week: i16,
    pub start_time: NaiveTime,
    /// At or before the start = ends the next day.
    pub end_time: NaiveTime,
}

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
    /// This block's own day-overtime rate; `None` = the branch's rules (RU-8).
    #[schema(value_type = Option<f64>)]
    pub ot_day_multiplier: Option<Decimal>,
    /// This block's own night-overtime rate; `None` = the branch's rules.
    #[schema(value_type = Option<f64>)]
    pub ot_night_multiplier: Option<Decimal>,
    pub checkin_window_minutes: i32,
    pub is_active: bool,
    /// The weekdays the block may be rostered on (0 = Sunday … 6 = Saturday).
    pub valid_days: Vec<i16>,
    /// Its own times on some weekdays; other valid days use the default.
    #[sqlx(skip)]
    #[serde(default)]
    pub day_times: Vec<DayTime>,
    /// Some version of it (default or a weekday's) is longer than the labour
    /// presence cap. A warning, never a block (RU-13).
    #[sqlx(skip)]
    #[serde(default)]
    pub over_presence_cap: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const WORK_SHIFT_COLS: &str = "id, org_id, branch_id, name, start_time, end_time, \
     crosses_midnight, grace_minutes, break_minutes, paid_break, half_day_threshold_minutes, \
     overtime_threshold_minutes, overtime_multiplier, ot_day_multiplier, ot_night_multiplier, \
     checkin_window_minutes, is_active, valid_days, created_at, updated_at";

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct ScheduleAssignment {
    pub id: Uuid,
    pub org_id: Uuid,
    pub employee_id: Uuid,
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
    pub employee_id: Uuid,
    pub on_date: NaiveDate,
    /// `None` = an explicit day off.
    pub work_shift_id: Option<Uuid>,
    #[sqlx(default)]
    pub work_shift_name: Option<String>,
    /// This assignment's own from/to, when it has one (the block is unchanged).
    #[sqlx(default)]
    pub start_time: Option<NaiveTime>,
    #[sqlx(default)]
    pub end_time: Option<NaiveTime>,
    pub reason: Option<String>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    /// Labour limits the person's week now goes past. Warnings, never blocks.
    #[sqlx(skip)]
    #[serde(default)]
    pub warnings: Vec<LabourWarning>,
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
    /// The EFFECTIVE window: the assignment's own times, else the block's time
    /// for that weekday, else its default.
    pub scheduled_start_at: DateTime<Utc>,
    pub scheduled_end_at: DateTime<Utc>,
    /// Whose assignment this is.
    pub employee_id: Uuid,
    /// The business date: the day the shift starts on (SC-10).
    pub on_date: NaiveDate,
    /// The branch it is worked at (the block's, else the person's first).
    pub branch_id: Option<Uuid>,
    /// Effective wall-clock times in the branch's zone.
    pub start_time: NaiveTime,
    pub end_time: NaiveTime,
    /// Ends on the following date.
    pub crosses_midnight: bool,
    /// This assignment has its own from/to (shown as edited).
    pub times_edited: bool,
    /// The date holds its own set (a date change), not the pattern.
    pub from_override: bool,
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
    /// The block's branch; null = the whole business. On an update, omitted
    /// keeps the block's branch (E2E B-ROTA-8); on a create, omitted = the
    /// whole business.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<Uuid>, nullable)]
    pub branch_id: Option<Option<Uuid>>,
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
    /// The block's own day-overtime rate (RU-8). Omit to keep it, null to go
    /// back to the branch's rules.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<f64>, nullable)]
    pub ot_day_multiplier: Option<Option<Decimal>>,
    /// The block's own night-overtime rate. Omit to keep, null to clear.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<f64>, nullable)]
    pub ot_night_multiplier: Option<Option<Decimal>>,
    #[serde(default)]
    pub checkin_window_minutes: Option<i32>,
    #[serde(default)]
    pub is_active: Option<bool>,
    /// Weekdays it may be rostered on (0 = Sunday … 6 = Saturday). Omit to
    /// keep them (all days for a new block).
    #[serde(default)]
    pub valid_days: Option<Vec<i16>>,
    /// Its own times on some weekdays (each must be a valid day). Omit to keep
    /// them; an empty list clears them.
    #[serde(default)]
    pub day_times: Option<Vec<DayTime>>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CreateAssignmentRequest {
    pub employee_id: Uuid,
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
    pub employee_id: Uuid,
    pub on_date: NaiveDate,
    /// Omit (or send null) to mark the date an explicit day off. Otherwise the
    /// whole date becomes this one shift; `PUT /staff/schedules/days` sets a
    /// split day.
    #[serde(default)]
    pub work_shift_id: Option<Uuid>,
    /// This assignment's own from/to (both or neither).
    #[serde(default)]
    pub start_time: Option<NaiveTime>,
    #[serde(default)]
    pub end_time: Option<NaiveTime>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// One block on a date, with its own from/to if it has one.
#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct DayBlock {
    pub work_shift_id: Uuid,
    #[serde(default)]
    pub start_time: Option<NaiveTime>,
    #[serde(default)]
    pub end_time: Option<NaiveTime>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct PutDayRequest {
    pub employee_id: Uuid,
    pub on_date: NaiveDate,
    /// Every shift the person works that date; empty = a day off.
    pub shifts: Vec<DayBlock>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct DayKey {
    pub employee_id: Uuid,
    pub on_date: NaiveDate,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct PutTimesRequest {
    pub employee_id: Uuid,
    pub on_date: NaiveDate,
    pub work_shift_id: Uuid,
    /// Both, or neither to go back to the block's own times.
    #[serde(default)]
    pub start_time: Option<NaiveTime>,
    #[serde(default)]
    pub end_time: Option<NaiveTime>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct MoveShiftRequest {
    /// Who has the shift now.
    pub employee_id: Uuid,
    /// Who gets it.
    pub to_employee_id: Uuid,
    pub on_date: NaiveDate,
    pub work_shift_id: Uuid,
}

/// One person's date after a change.
#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct DayView {
    pub employee_id: Uuid,
    pub on_date: NaiveDate,
    /// Empty = a day off (or nothing rostered).
    pub shifts: Vec<ResolvedShift>,
    /// The date follows the standing pattern (no date change).
    pub follows_pattern: bool,
    /// Labour limits the person's week now goes past. Warnings, never blocks.
    pub warnings: Vec<LabourWarning>,
}

#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct MoveView {
    pub from: DayView,
    pub to: DayView,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct UserQuery {
    /// Omit for the WHOLE org's roster — what a schedule grid needs, and the
    /// only way to draw one without a request per employee.
    #[serde(default)]
    pub employee_id: Option<Uuid>,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct DayQuery {
    pub employee_id: Uuid,
    pub date: NaiveDate,
    /// Which branch's timezone the day is measured in. Defaults to the
    /// employee's only branch assignment when they have exactly one.
    #[serde(default)]
    pub branch_id: Option<Uuid>,
}

// ── Resolution ────────────────────────────────────────────────

const ROSTER_SQL: &str = "SELECT r.employee_id, r.on_date, r.work_shift_id, r.branch_id, \
        ws.name, ws.grace_minutes, ws.break_minutes, ws.paid_break, \
        ws.half_day_threshold_minutes, ws.overtime_threshold_minutes, ws.overtime_multiplier, \
        ws.checkin_window_minutes, r.start_local AS start_time, r.end_local AS end_time, \
        r.crosses_midnight, r.times_edited, r.from_override, \
        r.start_at AS scheduled_start_at, r.end_at AS scheduled_end_at \
   FROM dawam_roster($1, $2, $3, $4, $5) r \
   JOIN work_shifts ws ON ws.id = r.work_shift_id \
  ORDER BY r.employee_id, r.on_date, r.start_at";

/// THE roster (SC-6, AT-9): every shift these people are rostered for on each
/// date in `[from, to]`, ordered by person, date and start. `tz` pins the
/// zone; `None` = each block's branch (else the person's first branch, else
/// the org's).
pub(crate) async fn resolve_range<'e, E>(
    exec: E,
    employees: &[Uuid],
    from: NaiveDate,
    to: NaiveDate,
    tz: Option<&str>,
) -> Result<Vec<ResolvedShift>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    Ok(sqlx::query_as::<_, ResolvedShift>(ROSTER_SQL)
        .bind(employees)
        .bind(from)
        .bind(to)
        .bind(tz)
        .bind(true)
        .fetch_all(exec)
        .await?)
}

/// The standing pattern alone (date changes ignored): what the pattern
/// intends, e.g. the suggestion engine's coverage fallback.
pub(crate) async fn pattern_range<'e, E>(
    exec: E,
    employees: &[Uuid],
    from: NaiveDate,
    to: NaiveDate,
    tz: Option<&str>,
) -> Result<Vec<ResolvedShift>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    Ok(sqlx::query_as::<_, ResolvedShift>(ROSTER_SQL)
        .bind(employees)
        .bind(from)
        .bind(to)
        .bind(tz)
        .bind(false)
        .fetch_all(exec)
        .await?)
}

/// Every shift the employee is expected to work on `date`, ordered by start time.
///
/// Empty means a rest day: either an explicit override with no shift, or simply
/// nothing rostered.
pub(crate) async fn resolve_shifts_for(
    pool: &PgPool,
    employee_id: Uuid,
    date: NaiveDate,
    timezone: &str,
) -> Result<Vec<ResolvedShift>, AppError> {
    resolve_range(pool, &[employee_id], date, date, Some(timezone)).await
}

/// Which shift an event at `instant` belongs to (SC-10): one of today's, or a
/// night shift of yesterday still running. Returns it and its business date —
/// the day the shift STARTED — or `(None, today)` for an unrostered punch.
///
/// Nearest scheduled start wins, so a 06:50 punch lands on the morning shift and
/// a 17:10 punch on the evening one. Deliberately NOT filtered by the check-in
/// window: an employee arriving three hours early still belongs to *some* shift,
/// and the window's job is to gate the check-in, not to erase the association.
pub(crate) async fn shift_at_instant(
    pool: &PgPool,
    employee_id: Uuid,
    today: NaiveDate,
    timezone: &str,
    instant: DateTime<Utc>,
) -> Result<(Option<ResolvedShift>, NaiveDate), AppError> {
    let yesterday = today.pred_opt().unwrap_or(today);
    let all = resolve_range(pool, &[employee_id], yesterday, today, Some(timezone)).await?;
    Ok(match pick_for_instant(&all, today, instant) {
        Some(s) => (Some(s.clone()), s.on_date),
        None => (None, today),
    })
}

/// The pure half of [`shift_at_instant`]: today's shifts, plus yesterday's
/// that run past `instant`; nearest start wins, today on a tie.
pub(crate) fn pick_for_instant(
    candidates: &[ResolvedShift],
    today: NaiveDate,
    instant: DateTime<Utc>,
) -> Option<&ResolvedShift> {
    candidates
        .iter()
        .filter(|s| s.on_date == today || s.scheduled_end_at > instant)
        .min_by_key(|s| {
            (
                (s.scheduled_start_at - instant).num_seconds().abs(),
                s.on_date != today,
            )
        })
}

/// One day of an employee's own upcoming roster.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct ScheduledDay {
    pub date: NaiveDate,
    /// Empty = a rest day, or a week not published yet.
    pub shifts: Vec<ResolvedShift>,
    /// The branch each shift is worked at, when the employee has one assignment.
    pub branch_name: Option<String>,
    /// The week is published at the person's branch. Unpublished weeks are
    /// drafts: they come back empty (SC-3).
    #[serde(default)]
    pub published: bool,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct MyScheduleQuery {
    pub from: NaiveDate,
    pub to: NaiveDate,
}

/// The employee's OWN roster for a date range, published weeks only (SC-3).
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
    me: Me,
    pool: crate::db::Db,
    query: web::Query<MyScheduleQuery>,
) -> Result<HttpResponse, AppError> {
    let employee_id = me.employee_id;
    let org_id = me.org_id;

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

    let tz = employee_timezone(pool.get_ref(), org_id, employee_id).await?;
    // Named only when the employee has exactly ONE live branch — the same rule
    // check-in uses. Someone assigned to two branches has no single "their
    // branch" to print under a shift, so the row stays unlabelled.
    let branch_name: Option<String> = sqlx::query_scalar(
        "SELECT name FROM (
             SELECT b.name, COUNT(*) OVER () AS n
               FROM employee_branches eb
               JOIN branches b ON b.id = eb.branch_id
                              AND b.deleted_at IS NULL
                              AND b.org_id = $2
              WHERE eb.employee_id = $1
         ) assignments
          WHERE n = 1",
    )
    .bind(employee_id)
    .bind(org_id)
    .fetch_optional(pool.get_ref())
    .await?;
    // A week is shown once it is published at any of the person's branches.
    let published: BTreeSet<NaiveDate> = sqlx::query_scalar(
        "SELECT DISTINCT p.week_start FROM staff_week_publications p \
           JOIN employee_branches eb ON eb.branch_id = p.branch_id \
          WHERE eb.employee_id = $1 AND p.week_start BETWEEN $2 AND $3",
    )
    .bind(employee_id)
    .bind(crate::staff::dawam::week_start(query.from))
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?
    .into_iter()
    .collect();

    let all = resolve_range(
        pool.get_ref(),
        &[employee_id],
        query.from,
        query.to,
        Some(&tz),
    )
    .await?;
    let mut days = Vec::with_capacity(span as usize + 1);
    let mut date = query.from;
    while date <= query.to {
        let is_published = published.contains(&crate::staff::dawam::week_start(date));
        days.push(ScheduledDay {
            date,
            shifts: if is_published {
                all.iter().filter(|s| s.on_date == date).cloned().collect()
            } else {
                Vec::new()
            },
            branch_name: branch_name.clone(),
            published: is_published,
        });
        date = date.succ_opt().unwrap_or(date);
        if days.len() > 63 {
            break;
        }
    }
    Ok(HttpResponse::Ok().json(days))
}

// ── Work shifts ───────────────────────────────────────────────

/// `absent` → None, `null` → Some(None), value → Some(Some(v)).
fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

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
    for (label, value) in [
        ("ot_day_multiplier", body.ot_day_multiplier.flatten()),
        ("ot_night_multiplier", body.ot_night_multiplier.flatten()),
    ] {
        // numeric(4,2): above zero, below 100.
        if value.is_some_and(|v| v <= Decimal::ZERO || v >= Decimal::from(100)) {
            return Err(AppError::BadRequest(format!(
                "{label} must be above 0 and below 100"
            )));
        }
    }
    if let Some(days) = &body.valid_days
        && (days.is_empty() || days.iter().any(|d| !(0..=6).contains(d)))
    {
        return Err(AppError::BadRequest(
            "valid_days needs at least one day, 0 (Sunday) through 6 (Saturday)".into(),
        ));
    }
    if let Some(times) = &body.day_times {
        let mut seen = BTreeSet::new();
        for t in times {
            if !(0..=6).contains(&t.day_of_week) || !seen.insert(t.day_of_week) {
                return Err(AppError::BadRequest(
                    "day_times: one entry per day, 0 (Sunday) through 6 (Saturday)".into(),
                ));
            }
            if t.start_time == t.end_time {
                return Err(AppError::BadRequest(
                    "A shift cannot start and end at the same time".into(),
                ));
            }
        }
    }
    Ok(name.to_string())
}

/// Minutes from `start` to `end`, running into the next day when the end is at
/// or before the start.
pub(crate) fn span_of_times(start: NaiveTime, end: NaiveTime) -> i64 {
    let m = (end - start).num_minutes();
    if m <= 0 { m + 24 * 60 } else { m }
}

/// Fill a block's weekday times and its presence-cap warning.
async fn complete_shifts(
    pool: &PgPool,
    org_id: Uuid,
    rows: &mut [WorkShift],
) -> Result<(), AppError> {
    let ids: Vec<Uuid> = rows.iter().map(|w| w.id).collect();
    let times: Vec<(Uuid, i16, NaiveTime, NaiveTime)> = sqlx::query_as(
        "SELECT work_shift_id, day_of_week, start_time, end_time FROM work_shift_day_times \
          WHERE work_shift_id = ANY($1) ORDER BY day_of_week",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    let settings = crate::staff::attendance::load_settings(pool, org_id, None).await?;
    let cap = engine::Limits::of(&settings).presence;
    for w in rows.iter_mut() {
        w.day_times = times
            .iter()
            .filter(|t| t.0 == w.id)
            .map(|t| DayTime {
                day_of_week: t.1,
                start_time: t.2,
                end_time: t.3,
            })
            .collect();
        w.over_presence_cap = span_of_times(w.start_time, w.end_time) > cap
            || w.day_times
                .iter()
                .any(|t| span_of_times(t.start_time, t.end_time) > cap);
    }
    Ok(())
}

async fn load_work_shift(pool: &PgPool, org_id: Uuid, id: Uuid) -> Result<WorkShift, AppError> {
    let mut row = sqlx::query_as::<_, WorkShift>(&format!(
        "SELECT {WORK_SHIFT_COLS} FROM work_shifts WHERE id = $1 AND org_id = $2"
    ))
    .bind(id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Work shift not found".into()))?;
    complete_shifts(pool, org_id, std::slice::from_mut(&mut row)).await?;
    Ok(row)
}

async fn write_day_times(
    conn: &mut PgConnection,
    org_id: Uuid,
    id: Uuid,
    times: &[DayTime],
) -> Result<(), AppError> {
    sqlx::query("DELETE FROM work_shift_day_times WHERE work_shift_id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    for t in times {
        sqlx::query(
            "INSERT INTO work_shift_day_times (work_shift_id, org_id, day_of_week, start_time, end_time) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(org_id)
        .bind(t.day_of_week)
        .bind(t.start_time)
        .bind(t.end_time)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
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
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    let scope = access::scope(pool.get_ref(), &claims, org_id, Cap::HrScheduleRead).await?;

    // Org-wide templates, and the ones of the caller's branches.
    let mut rows = sqlx::query_as::<_, WorkShift>(&format!(
        "SELECT {WORK_SHIFT_COLS} FROM work_shifts WHERE org_id = $1 \
           AND ($2::uuid[] IS NULL OR branch_id IS NULL OR branch_id = ANY($2)) \
         ORDER BY start_time, lower(name)"
    ))
    .bind(org_id)
    .bind(scope.as_deref())
    .fetch_all(pool.get_ref())
    .await?;
    complete_shifts(pool.get_ref(), org_id, &mut rows).await?;
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
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::gate(pool.get_ref(), &claims, org_id, Cap::HrScheduleCreate).await?;
    require_shift_scope(
        pool.get_ref(),
        &claims,
        org_id,
        Cap::HrScheduleCreate,
        body.branch_id.flatten(),
    )
    .await?;
    let name = validate_work_shift(&body)?;
    let valid_days = body.valid_days.clone().unwrap_or_else(|| (0..7).collect());
    let day_times = body.day_times.clone().unwrap_or_default();
    refuse_times_off_days(&valid_days, &day_times)?;

    let mut tx = pool.get_ref().begin().await?;
    let id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO work_shifts (
            org_id, branch_id, name, start_time, end_time, grace_minutes, break_minutes,
            paid_break, half_day_threshold_minutes, overtime_threshold_minutes,
            overtime_multiplier, checkin_window_minutes, is_active, valid_days,
            ot_day_multiplier, ot_night_multiplier
        ) VALUES (
            $1, $2, $3, $4, $5, COALESCE($6, 15), COALESCE($7, 0),
            COALESCE($8, TRUE), $9, COALESCE($10, 15),
            COALESCE($11, 1.50), COALESCE($12, 120), COALESCE($13, TRUE), $14, $15, $16
        ) RETURNING id
        "#,
    )
    .bind(org_id)
    .bind(body.branch_id.flatten())
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
    .bind(&valid_days)
    .bind(body.ot_day_multiplier.flatten())
    .bind(body.ot_night_multiplier.flatten())
    .fetch_one(&mut *tx)
    .await?;
    write_day_times(&mut tx, org_id, id, &day_times).await?;
    tx.commit().await?;
    let row = load_work_shift(pool.get_ref(), org_id, id).await?;
    Ok(HttpResponse::Created().json(row))
}

fn refuse_times_off_days(valid_days: &[i16], times: &[DayTime]) -> Result<(), AppError> {
    if let Some(t) = times.iter().find(|t| !valid_days.contains(&t.day_of_week)) {
        return Err(AppError::BadRequest(format!(
            "day {} has its own times but isn't one of the block's days",
            t.day_of_week
        )));
    }
    Ok(())
}

/// Everyone whose roster a block touches: in the pattern, or on a date.
async fn people_on_block(conn: &mut PgConnection, id: Uuid) -> Result<Vec<Uuid>, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT employee_id FROM staff_schedules WHERE work_shift_id = $1 \
         UNION SELECT employee_id FROM staff_schedule_overrides WHERE work_shift_id = $1",
    )
    .bind(id)
    .fetch_all(&mut *conn)
    .await?)
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
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::gate(pool.get_ref(), &claims, org_id, Cap::HrScheduleEdit).await?;
    let current = shift_branch(pool.get_ref(), org_id, *id).await?;
    require_shift_scope(
        pool.get_ref(),
        &claims,
        org_id,
        Cap::HrScheduleEdit,
        current,
    )
    .await?;
    // Omitted keeps the branch; only an explicit null moves the block to the
    // whole business (E2E B-ROTA-8).
    let branch = body.branch_id.unwrap_or(current);
    if branch != current {
        require_shift_scope(pool.get_ref(), &claims, org_id, Cap::HrScheduleEdit, branch).await?;
    }
    let name = validate_work_shift(&body)?;
    let before = load_work_shift(pool.get_ref(), org_id, *id).await?;
    let valid_days = body.valid_days.clone().unwrap_or(before.valid_days.clone());
    let day_times = body.day_times.clone().unwrap_or(before.day_times.clone());
    refuse_times_off_days(&valid_days, &day_times)?;

    let mut tx = pool.get_ref().begin().await?;
    // Taking a day away from a block that people are rostered on that day,
    // by a weekday row naming it or by a future date, would silently unroster
    // them: move them first. An every-day row means "on the block's days", so
    // it follows the change, which is marked and told like any other (SC-4).
    let dropped: Vec<i16> = before
        .valid_days
        .iter()
        .copied()
        .filter(|d| !valid_days.contains(d))
        .collect();
    if !dropped.is_empty() {
        let stuck: i64 = sqlx::query_scalar(
            "SELECT (SELECT COUNT(*) FROM staff_schedules \
                      WHERE work_shift_id = $1 AND day_of_week = ANY($2) \
                        AND (effective_to IS NULL OR effective_to >= CURRENT_DATE)) \
                  + (SELECT COUNT(*) FROM staff_schedule_overrides \
                      WHERE work_shift_id = $1 AND on_date >= CURRENT_DATE \
                        AND EXTRACT(DOW FROM on_date)::smallint = ANY($2))",
        )
        .bind(*id)
        .bind(&dropped)
        .fetch_one(&mut *tx)
        .await?;
        if stuck > 0 {
            // With its figures (AT-13, E2E B-ROTA-3).
            return Err(AppError::CodedVars {
                status: 409,
                code: "SHIFT_DAYS_IN_USE",
                reason: format!(
                    "{stuck} roster entr{} still put {} on the days you took away — move them first.",
                    if stuck == 1 { "y" } else { "ies" },
                    before.name
                ),
                vars: serde_json::json!({ "n": stuck, "name": before.name, "days": dropped }),
            });
        }
    }
    let people = people_on_block(&mut tx, *id).await?;
    let horizon = days::published_horizon(&mut tx, &people).await?;
    let snap_before = match horizon {
        Some((a, b)) => days::snapshot(&mut tx, &people, a, b).await?,
        None => Default::default(),
    };

    // Editing a shift never rewrites history: attendance rows carry their own
    // snapshot of the window they were judged against.
    sqlx::query(
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
            valid_days                 = $15,
            ot_day_multiplier          = CASE WHEN $16 THEN $17 ELSE ot_day_multiplier END,
            ot_night_multiplier        = CASE WHEN $18 THEN $19 ELSE ot_night_multiplier END,
            updated_at                 = now()
         WHERE id = $1 AND org_id = $2
        "#,
    )
    .bind(*id)
    .bind(org_id)
    .bind(branch)
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
    .bind(&valid_days)
    .bind(body.ot_day_multiplier.is_some())
    .bind(body.ot_day_multiplier.flatten())
    .bind(body.ot_night_multiplier.is_some())
    .bind(body.ot_night_multiplier.flatten())
    .execute(&mut *tx)
    .await?;
    write_day_times(&mut tx, org_id, *id, &day_times).await?;

    // New times may make someone's shifts overlap: refuse, the block stays.
    let today = Utc::now().date_naive();
    for p in &people {
        days::check_overlaps(&mut tx, *p, today, today + Duration::days(14)).await?;
    }
    let changes = match horizon {
        Some((a, b)) => days::diff(&snap_before, &days::snapshot(&mut tx, &people, a, b).await?),
        None => BTreeSet::new(),
    };
    tx.commit().await?;
    // A published week that changed tells the people it touched (SC-4).
    days::mark_changed(pool.get_ref(), org_id, &changes).await?;
    let row = load_work_shift(pool.get_ref(), org_id, *id).await?;
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
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::gate(pool.get_ref(), &claims, org_id, Cap::HrScheduleDelete).await?;
    let current = shift_branch(pool.get_ref(), org_id, *id).await?;
    require_shift_scope(
        pool.get_ref(),
        &claims,
        org_id,
        Cap::HrScheduleDelete,
        current,
    )
    .await?;

    // Attendance keeps its rows (the FK is ON DELETE SET NULL) but the roster
    // cascades, which would silently unschedule people. Make that explicit.
    let assigned: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM staff_schedules WHERE work_shift_id = $1 AND org_id = $2) \
              + (SELECT COUNT(*) FROM staff_schedule_overrides \
                  WHERE work_shift_id = $1 AND org_id = $2 AND on_date >= CURRENT_DATE)",
    )
    .bind(*id)
    .bind(org_id)
    .fetch_one(pool.get_ref())
    .await?;
    if assigned > 0 {
        // Coded with its figures, for the client's own wording (AT-13, E2E
        // B-ROTA-2); a conflict with the roster, like SHIFT_DAYS_IN_USE.
        let name: String = sqlx::query_scalar("SELECT name FROM work_shifts WHERE id = $1")
            .bind(*id)
            .fetch_one(pool.get_ref())
            .await?;
        return Err(AppError::CodedVars {
            status: 409,
            code: "SHIFT_IN_USE",
            reason: format!(
                "{assigned} roster assignment(s) still use {name} — remove them first, \
                 or switch the shift off instead"
            ),
            vars: serde_json::json!({ "n": assigned, "name": name }),
        });
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
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    let scope = access::scope(pool.get_ref(), &claims, org_id, Cap::HrScheduleRead).await?;

    let rows = sqlx::query_as::<_, ScheduleAssignment>(&format!(
        "SELECT s.id, s.org_id, s.employee_id, s.work_shift_id, ws.name AS work_shift_name, \
                s.day_of_week, s.effective_from, s.effective_to, s.created_at \
           FROM staff_schedules s \
           JOIN work_shifts ws ON ws.id = s.work_shift_id \
          WHERE ($1::uuid IS NULL OR s.employee_id = $1) AND s.org_id = $2 AND {} \
          ORDER BY s.employee_id, s.effective_from DESC, s.day_of_week NULLS LAST, ws.start_time",
        access::in_scope("s.employee_id", 3)
    ))
    .bind(query.employee_id)
    .bind(org_id)
    .bind(scope.as_deref())
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

/// The editor holds the edit right at the person's branches (RO-6) and, for a
/// branch's own block, at that branch too.
async fn require_edit_block(
    pool: &PgPool,
    claims: &Claims,
    org_id: Uuid,
    branch: Option<Uuid>,
) -> Result<(), AppError> {
    if let Some(b) = branch {
        access::require_at(pool, claims, org_id, Cap::HrScheduleEdit, b).await?;
    }
    Ok(())
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
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::gate(pool.get_ref(), &claims, org_id, Cap::HrScheduleEdit).await?;
    let subject = access::subject(pool.get_ref(), org_id, body.employee_id).await?;
    access::require_for(pool.get_ref(), &claims, Cap::HrScheduleEdit, &subject).await?;

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
    let info = days::block_info(
        &mut *pool.get_ref().acquire().await?,
        org_id,
        body.work_shift_id,
    )
    .await?;
    require_edit_block(pool.get_ref(), &claims, org_id, info.branch_id).await?;
    if let Some(d) = body.day_of_week
        && !info.valid_days.contains(&d)
    {
        return Err(AppError::Coded {
            status: 400,
            code: "SHIFT_NOT_ON_DAY",
            reason: format!("{} isn't a shift on that weekday.", info.name),
        });
    }
    if let Some(b) = info.branch_id
        && !subject.branches.contains(&b)
    {
        return Err(AppError::Coded {
            status: 400,
            code: "SHIFT_OTHER_BRANCH",
            reason: format!("{} doesn't work at {}'s branch.", info.name, subject.name),
        });
    }
    let mut tx = pool.get_ref().begin().await?;
    let horizon = days::published_horizon(&mut tx, &[subject.id]).await?;
    let before = match horizon {
        Some((a, b)) => days::snapshot(&mut tx, &[subject.id], a, b).await?,
        None => Default::default(),
    };
    let row = sqlx::query_as::<_, ScheduleAssignment>(
        "WITH ins AS (
             INSERT INTO staff_schedules
                 (org_id, employee_id, work_shift_id, day_of_week, effective_from, effective_to)
             VALUES ($1, $2, $3, $4, COALESCE($5, CURRENT_DATE), $6)
             RETURNING *
         )
         SELECT ins.id, ins.org_id, ins.employee_id, ins.work_shift_id, ws.name AS work_shift_name,
                ins.day_of_week, ins.effective_from, ins.effective_to, ins.created_at
           FROM ins JOIN work_shifts ws ON ws.id = ins.work_shift_id",
    )
    .bind(org_id)
    .bind(body.employee_id)
    .bind(body.work_shift_id)
    .bind(body.day_of_week)
    .bind(body.effective_from)
    .bind(body.effective_to)
    .fetch_one(&mut *tx)
    .await?;
    // Two weeks from when it starts covers every weekday twice, the night
    // into the next morning included.
    let from = row.effective_from.max(Utc::now().date_naive());
    days::check_overlaps(&mut tx, subject.id, from, from + Duration::days(14)).await?;
    let changes = match horizon {
        Some((a, b)) => days::diff(
            &before,
            &days::snapshot(&mut tx, &[subject.id], a, b).await?,
        ),
        None => BTreeSet::new(),
    };
    tx.commit().await?;
    days::mark_changed(pool.get_ref(), org_id, &changes).await?;
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
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::gate(pool.get_ref(), &claims, org_id, Cap::HrScheduleEdit).await?;
    let owner: Uuid =
        sqlx::query_scalar("SELECT employee_id FROM staff_schedules WHERE id = $1 AND org_id = $2")
            .bind(*id)
            .bind(org_id)
            .fetch_optional(pool.get_ref())
            .await?
            .ok_or_else(|| AppError::NotFound("Assignment not found".into()))?;
    let subject = access::subject(pool.get_ref(), org_id, owner).await?;
    access::require_for(pool.get_ref(), &claims, Cap::HrScheduleEdit, &subject).await?;

    let mut tx = pool.get_ref().begin().await?;
    let horizon = days::published_horizon(&mut tx, &[owner]).await?;
    let before = match horizon {
        Some((a, b)) => days::snapshot(&mut tx, &[owner], a, b).await?,
        None => Default::default(),
    };
    let deleted = sqlx::query("DELETE FROM staff_schedules WHERE id = $1 AND org_id = $2")
        .bind(*id)
        .bind(org_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(AppError::NotFound("Assignment not found".into()));
    }
    let changes = match horizon {
        Some((a, b)) => days::diff(&before, &days::snapshot(&mut tx, &[owner], a, b).await?),
        None => BTreeSet::new(),
    };
    tx.commit().await?;
    days::mark_changed(pool.get_ref(), org_id, &changes).await?;
    Ok(HttpResponse::NoContent().finish())
}

// ── A date's assignments ──────────────────────────────────────

fn times_of(
    start: Option<NaiveTime>,
    end: Option<NaiveTime>,
) -> Result<Option<(NaiveTime, NaiveTime)>, AppError> {
    match (start, end) {
        (None, None) => Ok(None),
        (Some(s), Some(e)) if s != e => Ok(Some((s, e))),
        (Some(_), Some(_)) => Err(AppError::Coded {
            status: 400,
            code: "SHIFT_EMPTY",
            reason: "A shift can't start and end at the same time.".into(),
        }),
        _ => Err(AppError::BadRequest(
            "Send both start_time and end_time, or neither".into(),
        )),
    }
}

fn clean_reason(r: &Option<String>) -> Option<&str> {
    r.as_deref().map(str::trim).filter(|r| !r.is_empty())
}

/// A person's date as it now stands, with the week's labour warnings.
pub(crate) async fn day_view(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
    on_date: NaiveDate,
) -> Result<DayView, AppError> {
    let ws = crate::staff::dawam::week_start(on_date);
    let week = resolve_range(pool, &[employee_id], ws, ws + Duration::days(6), None).await?;
    let follows_pattern: bool = !sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM staff_schedule_overrides WHERE employee_id = $1 AND on_date = $2)",
    )
    .bind(employee_id)
    .bind(on_date)
    .fetch_one(pool)
    .await?;
    let home = week.first().and_then(|s| s.branch_id);
    let settings = crate::staff::attendance::load_settings(pool, org_id, home).await?;
    let spans: Vec<engine::Span> = week
        .iter()
        .map(|s| engine::Span {
            date: s.on_date,
            start: s.scheduled_start_at,
            end: s.scheduled_end_at,
        })
        .collect();
    let warnings = engine::breaks(employee_id, &spans, &engine::Limits::of(&settings))
        .into_iter()
        .filter(|w| w.date == on_date || w.date == ws)
        .collect();
    Ok(DayView {
        employee_id,
        on_date,
        shifts: week.into_iter().filter(|s| s.on_date == on_date).collect(),
        follows_pattern,
        warnings,
    })
}

/// The editor's rights over the blocks: a branch's own block needs the edit
/// right at that branch. Checked before any transaction opens.
async fn authorize_blocks(
    pool: &PgPool,
    claims: &Claims,
    org_id: Uuid,
    blocks: &[Block],
) -> Result<(), AppError> {
    let ids: Vec<Uuid> = blocks.iter().map(|b| b.work_shift_id).collect();
    let branches: Vec<Option<Uuid>> = sqlx::query_scalar(
        "SELECT DISTINCT branch_id FROM work_shifts WHERE id = ANY($1) AND org_id = $2",
    )
    .bind(&ids)
    .bind(org_id)
    .fetch_all(pool)
    .await?;
    for b in branches {
        require_edit_block(pool, claims, org_id, b).await?;
    }
    Ok(())
}

/// Each block may be rostered for the person on that date.
async fn check_blocks(
    conn: &mut PgConnection,
    subject: &access::Subject,
    date: NaiveDate,
    blocks: &[Block],
) -> Result<(), AppError> {
    for b in blocks {
        days::validate_block(conn, subject, date, b).await?;
    }
    Ok(())
}

async fn editable_subject(
    pool: &PgPool,
    claims: &Claims,
    org_id: Uuid,
    employee_id: Uuid,
) -> Result<access::Subject, AppError> {
    let subject = access::subject(pool, org_id, employee_id).await?;
    access::require_for(pool, claims, Cap::HrScheduleEdit, &subject).await?;
    Ok(subject)
}

/// Set a date to exactly one shift, or a day off (the older single-shift
/// form of `PUT /staff/schedules/days`).
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
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrScheduleEdit).await?;
    let subject = editable_subject(pool, &claims, org_id, body.employee_id).await?;
    let times = times_of(body.start_time, body.end_time)?;
    let blocks: Vec<Block> = body
        .work_shift_id
        .map(|work_shift_id| Block {
            work_shift_id,
            times,
        })
        .into_iter()
        .collect();
    if body.work_shift_id.is_none() && times.is_some() {
        return Err(AppError::BadRequest("A day off has no times".into()));
    }
    let by = claims.user_id_safe().ok();
    authorize_blocks(pool, &claims, org_id, &blocks).await?;
    let mut tx = pool.begin().await?;
    check_blocks(&mut tx, &subject, body.on_date, &blocks).await?;
    days::replace_day(
        &mut tx,
        org_id,
        subject.id,
        body.on_date,
        &blocks,
        clean_reason(&body.reason),
        by,
    )
    .await?;
    days::check_overlaps(&mut tx, subject.id, body.on_date, body.on_date).await?;
    for b in &blocks {
        days::log_manual(
            &mut tx,
            org_id,
            subject.id,
            body.on_date,
            b.work_shift_id,
            by,
        )
        .await?;
    }
    tx.commit().await?;
    after_day_change(pool, org_id, subject.id, body.on_date).await?;

    let mut row = sqlx::query_as::<_, ScheduleOverride>(
        "SELECT o.id, o.org_id, o.employee_id, o.on_date, o.work_shift_id, \
                ws.name AS work_shift_name, o.start_time, o.end_time, o.reason, o.created_by, \
                o.created_at \
           FROM staff_schedule_overrides o LEFT JOIN work_shifts ws ON ws.id = o.work_shift_id \
          WHERE o.employee_id = $1 AND o.on_date = $2 \
          ORDER BY o.created_at DESC LIMIT 1",
    )
    .bind(subject.id)
    .bind(body.on_date)
    .fetch_one(pool)
    .await?;
    row.warnings = day_view(pool, org_id, subject.id, body.on_date)
        .await?
        .warnings;
    Ok(HttpResponse::Ok().json(row))
}

/// Set every shift a person works on a date: a split day, one shift with its
/// own times, or a day off (SC-5, SC-11).
#[utoipa::path(
    put, path = "/staff/schedules/days", tag = "staff",
    request_body = PutDayRequest,
    responses((status = 200, body = DayView), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_day(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<PutDayRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrScheduleEdit).await?;
    let subject = editable_subject(pool, &claims, org_id, body.employee_id).await?;
    let mut blocks = Vec::with_capacity(body.shifts.len());
    for s in &body.shifts {
        blocks.push(Block {
            work_shift_id: s.work_shift_id,
            times: times_of(s.start_time, s.end_time)?,
        });
    }
    let by = claims.user_id_safe().ok();
    authorize_blocks(pool, &claims, org_id, &blocks).await?;
    let mut tx = pool.begin().await?;
    check_blocks(&mut tx, &subject, body.on_date, &blocks).await?;
    days::replace_day(
        &mut tx,
        org_id,
        subject.id,
        body.on_date,
        &blocks,
        clean_reason(&body.reason),
        by,
    )
    .await?;
    days::check_overlaps(&mut tx, subject.id, body.on_date, body.on_date).await?;
    for b in &blocks {
        days::log_manual(
            &mut tx,
            org_id,
            subject.id,
            body.on_date,
            b.work_shift_id,
            by,
        )
        .await?;
    }
    tx.commit().await?;
    after_day_change(pool, org_id, subject.id, body.on_date).await?;
    Ok(HttpResponse::Ok().json(day_view(pool, org_id, subject.id, body.on_date).await?))
}

/// Put a date back on the standing pattern.
#[utoipa::path(
    delete, path = "/staff/schedules/days", tag = "staff",
    params(DayKey),
    responses((status = 200, body = DayView), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn reset_day(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<DayKey>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrScheduleEdit).await?;
    let subject = editable_subject(pool, &claims, org_id, query.employee_id).await?;
    let mut tx = pool.begin().await?;
    let removed = days::reset_day(&mut tx, subject.id, query.on_date).await?;
    days::check_overlaps(&mut tx, subject.id, query.on_date, query.on_date).await?;
    tx.commit().await?;
    if removed > 0 {
        after_day_change(pool, org_id, subject.id, query.on_date).await?;
    }
    Ok(HttpResponse::Ok().json(day_view(pool, org_id, subject.id, query.on_date).await?))
}

/// One assignment's own from/to (one person, one date, one block), without
/// changing the block. Both null = back to the block's times.
#[utoipa::path(
    put, path = "/staff/schedules/days/times", tag = "staff",
    request_body = PutTimesRequest,
    responses((status = 200, body = DayView), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_times(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<PutTimesRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrScheduleEdit).await?;
    let subject = editable_subject(pool, &claims, org_id, body.employee_id).await?;
    let times = times_of(body.start_time, body.end_time)?;
    let info = days::block_info(&mut *pool.acquire().await?, org_id, body.work_shift_id).await?;
    require_edit_block(pool, &claims, org_id, info.branch_id).await?;
    let mut tx = pool.begin().await?;
    let found = days::set_times(
        &mut tx,
        org_id,
        subject.id,
        body.on_date,
        body.work_shift_id,
        times,
        claims.user_id_safe().ok(),
    )
    .await?;
    if !found {
        return Err(AppError::Refused {
            code: "NOT_ROSTERED",
            reason: format!("{} isn't on {} that day.", subject.name, info.name),
        });
    }
    days::check_overlaps(&mut tx, subject.id, body.on_date, body.on_date).await?;
    tx.commit().await?;
    after_day_change(pool, org_id, subject.id, body.on_date).await?;
    Ok(HttpResponse::Ok().json(day_view(pool, org_id, subject.id, body.on_date).await?))
}

/// Give one person's shift on a date to someone else; both keep the rest of
/// their day.
#[utoipa::path(
    post, path = "/staff/schedules/days/move", tag = "staff",
    request_body = MoveShiftRequest,
    responses((status = 200, body = MoveView), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn move_shift(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<MoveShiftRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrScheduleEdit).await?;
    if body.employee_id == body.to_employee_id {
        return Err(AppError::BadRequest("Pick someone else.".into()));
    }
    let from = editable_subject(pool, &claims, org_id, body.employee_id).await?;
    let to = editable_subject(pool, &claims, org_id, body.to_employee_id).await?;
    if to.employment_status != "active" {
        return Err(AppError::NotFound("Employee not found".into()));
    }
    let by = claims.user_id_safe().ok();
    let block_only = Block {
        work_shift_id: body.work_shift_id,
        times: None,
    };
    authorize_blocks(pool, &claims, org_id, &[block_only]).await?;
    let mut tx = pool.begin().await?;
    let removed = days::remove_block(
        &mut tx,
        org_id,
        from.id,
        body.on_date,
        body.work_shift_id,
        Some("Moved"),
        by,
    )
    .await?;
    let Some(times) = removed else {
        return Err(AppError::Refused {
            code: "NOT_ROSTERED",
            reason: format!("{} isn't on that shift that day.", from.name),
        });
    };
    // Already on that block that day? Adding it would be a no-op on the
    // date's set and the block would simply vanish from `from` (E2E
    // B-ROTA-1). Refused; the transaction rolls back and `from` keeps it.
    if let Some(on) = resolve_range(&mut *tx, &[to.id], body.on_date, body.on_date, None)
        .await?
        .into_iter()
        .find(|s| s.work_shift_id == body.work_shift_id)
    {
        return Err(AppError::CodedVars {
            status: 409,
            code: "ALREADY_ROSTERED",
            reason: format!("{} is already on {} that day.", to.name, on.name),
            vars: serde_json::json!({ "name": to.name, "shift": on.name, "date": body.on_date }),
        });
    }
    let block = Block {
        work_shift_id: body.work_shift_id,
        times,
    };
    check_blocks(&mut tx, &to, body.on_date, &[block]).await?;
    days::add_block(
        &mut tx,
        org_id,
        to.id,
        body.on_date,
        &block,
        Some("Moved"),
        by,
    )
    .await?;
    days::check_overlaps(&mut tx, to.id, body.on_date, body.on_date).await?;
    days::log_manual(&mut tx, org_id, to.id, body.on_date, body.work_shift_id, by).await?;
    tx.commit().await?;
    after_day_change(pool, org_id, from.id, body.on_date).await?;
    after_day_change(pool, org_id, to.id, body.on_date).await?;
    Ok(HttpResponse::Ok().json(MoveView {
        from: day_view(pool, org_id, from.id, body.on_date).await?,
        to: day_view(pool, org_id, to.id, body.on_date).await?,
    }))
}

/// A date changed for one person: in a published week, mark it and tell them
/// (SC-4, SC-5).
pub(crate) async fn after_day_change(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
    on_date: NaiveDate,
) -> Result<(), AppError> {
    days::mark_changed(pool, org_id, &BTreeSet::from([(employee_id, on_date)])).await
}

/// Remove one row of a date's set: one block of a split day, or the date's
/// last change (back to the pattern).
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
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::gate(pool.get_ref(), &claims, org_id, Cap::HrScheduleEdit).await?;
    let row: (Uuid, NaiveDate) = sqlx::query_as(
        "SELECT employee_id, on_date FROM staff_schedule_overrides WHERE id = $1 AND org_id = $2",
    )
    .bind(*id)
    .bind(org_id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("Override not found".into()))?;
    let (owner, on_date) = row;
    let subject = access::subject(pool.get_ref(), org_id, owner).await?;
    access::require_for(pool.get_ref(), &claims, Cap::HrScheduleEdit, &subject).await?;

    let mut tx = pool.get_ref().begin().await?;
    let deleted = sqlx::query("DELETE FROM staff_schedule_overrides WHERE id = $1 AND org_id = $2")
        .bind(*id)
        .bind(org_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(AppError::NotFound("Override not found".into()));
    }
    days::check_overlaps(&mut tx, owner, on_date, on_date).await?;
    tx.commit().await?;
    // Reverting a day is a change too (SC-4).
    after_day_change(pool.get_ref(), org_id, owner, on_date).await?;
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
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::gate(pool.get_ref(), &claims, org_id, Cap::HrScheduleRead).await?;
    let subject = access::subject(pool.get_ref(), org_id, query.employee_id).await?;
    access::require_for(pool.get_ref(), &claims, Cap::HrScheduleRead, &subject).await?;

    let tz = match query.branch_id {
        Some(branch_id) => crate::staff::branch_timezone(pool.get_ref(), branch_id).await?,
        None => employee_timezone(pool.get_ref(), org_id, query.employee_id).await?,
    };
    let rows = resolve_shifts_for(pool.get_ref(), query.employee_id, query.date, &tz).await?;
    Ok(HttpResponse::Ok().json(rows))
}

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

/// The timezone an employee's day is measured in when no branch is named.
///
/// Uses their single branch assignment when they have exactly one — the common
/// case — and the org's timezone otherwise, because an employee who floats
/// between branches has no one "home" clock.
pub(crate) async fn employee_timezone(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
) -> Result<String, AppError> {
    let tz: Option<String> = sqlx::query_scalar(
        r#"
        SELECT COALESCE(
            -- MIN() over an aggregate-only subquery: HAVING COUNT(*) = 1 makes it
            -- return NULL (no row) unless the employee has exactly one branch,
            -- which is what "their branch's clock" means.
            (SELECT MIN(b.timezone::text)
               FROM employee_branches eb
               JOIN branches b ON b.id = eb.branch_id AND b.deleted_at IS NULL
              WHERE eb.employee_id = $1
             HAVING COUNT(*) = 1),
            (SELECT o.timezone::text FROM organizations o WHERE o.id = $2),
            $3
        )
        "#,
    )
    .bind(employee_id)
    .bind(org_id)
    .bind(DEFAULT_TZ)
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(tz.unwrap_or_else(|| DEFAULT_TZ.to_string()))
}

/// The branch a work shift belongs to (`None` = an org-wide template).
async fn shift_branch(pool: &PgPool, org_id: Uuid, id: Uuid) -> Result<Option<Uuid>, AppError> {
    let row: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT branch_id FROM work_shifts WHERE id = $1 AND org_id = $2")
            .bind(id)
            .bind(org_id)
            .fetch_optional(pool)
            .await?;
    row.ok_or_else(|| AppError::NotFound("Work shift not found".into()))
}

/// A branch's shift is its manager's; an org-wide template needs the
/// capability at every branch (RO-6).
async fn require_shift_scope(
    pool: &PgPool,
    claims: &Claims,
    org_id: Uuid,
    cap: Cap,
    branch: Option<Uuid>,
) -> Result<(), AppError> {
    match branch {
        Some(b) => access::require_at(pool, claims, org_id, cap, b).await,
        None => access::require_everywhere(pool, claims, org_id, cap).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn shift(on: NaiveDate, start: DateTime<Utc>, hours: i64) -> ResolvedShift {
        ResolvedShift {
            work_shift_id: Uuid::new_v4(),
            name: "x".into(),
            grace_minutes: 15,
            break_minutes: 0,
            paid_break: true,
            half_day_threshold_minutes: None,
            overtime_threshold_minutes: 15,
            overtime_multiplier: Decimal::ONE,
            checkin_window_minutes: 120,
            scheduled_start_at: start,
            scheduled_end_at: start + Duration::hours(hours),
            employee_id: Uuid::nil(),
            on_date: on,
            branch_id: None,
            start_time: t(0, 0),
            end_time: t(0, 0),
            crosses_midnight: false,
            times_edited: false,
            from_override: false,
        }
    }

    #[test]
    fn spans_run_into_the_next_day() {
        assert_eq!(span_of_times(t(18, 0), t(2, 0)), 8 * 60);
        assert_eq!(span_of_times(t(16, 0), t(0, 0)), 8 * 60);
        assert_eq!(span_of_times(t(9, 0), t(17, 30)), 8 * 60 + 30);
    }

    #[test]
    fn an_after_midnight_event_belongs_to_last_nights_shift() {
        let d = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let y = d.pred_opt().unwrap();
        let night = shift(y, Utc.with_ymd_and_hms(2026, 9, 30, 22, 0, 0).unwrap(), 8);
        let morning = shift(d, Utc.with_ymd_and_hms(2026, 10, 1, 10, 0, 0).unwrap(), 6);
        let rows = vec![night.clone(), morning.clone()];
        let at = Utc.with_ymd_and_hms(2026, 10, 1, 1, 0, 0).unwrap();
        assert_eq!(pick_for_instant(&rows, d, at).unwrap().on_date, y);
        let at = Utc.with_ymd_and_hms(2026, 10, 1, 9, 0, 0).unwrap();
        assert_eq!(pick_for_instant(&rows, d, at).unwrap().on_date, d);
        // Yesterday's day shift that already ended never matches.
        let old = shift(y, Utc.with_ymd_and_hms(2026, 9, 30, 9, 0, 0).unwrap(), 6);
        let at = Utc.with_ymd_and_hms(2026, 10, 1, 1, 0, 0).unwrap();
        assert!(pick_for_instant(&[old], d, at).is_none());
    }

    #[test]
    fn overlaps_are_found_across_midnight() {
        let d = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let n = d.succ_opt().unwrap();
        let night = shift(d, Utc.with_ymd_and_hms(2026, 10, 1, 18, 0, 0).unwrap(), 8);
        let early = shift(n, Utc.with_ymd_and_hms(2026, 10, 2, 1, 0, 0).unwrap(), 6);
        let later = shift(n, Utc.with_ymd_and_hms(2026, 10, 2, 9, 0, 0).unwrap(), 6);
        assert!(days::first_overlap(&[night.clone(), early], d, d).is_some());
        assert!(days::first_overlap(&[night, later], d, d).is_none());
    }
}
