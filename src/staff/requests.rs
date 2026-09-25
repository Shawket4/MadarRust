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
//! ## Leave has no types and no balances (RQ-2, RQ-3)
//!
//! The employee asks for full or half days with a note; the manager approves
//! each as paid or unpaid. Unpaid leave is priced like an absence by
//! `penalties`. The old leave types and balances are no longer read or written
//! here; `/staff/me/leave-balances` still shows what older data holds.
//!
//! ## Who decides (RQ-5)
//!
//! A request goes to the managers of the requester's branch. A manager's own
//! request is approved as it is filed when they hold `hr.requests.self_approve`;
//! otherwise it waits for someone above them, and the owner is told. Nobody
//! approves their own request any other way.

use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    auth::jwt::Claims,
    authz::Cap,
    errors::{AppError, AppErrorResponse},
    staff::{
        access,
        attendance::{
            AttendanceSettings, DayAdjustments, ExcusedWindow, TimedRequest, WindowRequest,
        },
        principal::{Me, StaffPrincipal, caller},
        scope_org, validate_decision,
    },
};

/// The six things an employee can ask for.
///
/// The first five are EXCUSED WINDOWS — "forgive this part of my day". The
/// sixth, `correction`, is not: it proposes an edit to a punch that the clock
/// got wrong, and on approval it is written to the attendance record and
/// repriced.
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
    pub employee_id: Uuid,
    #[sqlx(default)]
    pub employee_name: Option<String>,
    /// `leave` | `late_arrival` | `early_departure` | `excuse` | `mission` | `correction`.
    pub kind: String,
    pub on_date: NaiveDate,
    /// Set for `leave` and `mission`: the span's last day. For an `excuse`
    /// that runs past midnight, the next day (its end is then on that day).
    pub end_date: Option<NaiveDate>,
    /// Start of the excused window. `None` = open to the shift's start.
    /// For a `correction`: the proposed check-in, branch-local.
    pub from_time: Option<NaiveTime>,
    /// End of the excused window. `None` = open to the shift's end.
    /// For a `correction`: the proposed check-out, branch-local (earlier on the
    /// clock than the check-in = the next morning, a night shift).
    pub to_time: Option<NaiveTime>,
    /// Deprecated (RQ-2): older rows only; never set on new requests.
    pub leave_type_id: Option<Uuid>,
    #[sqlx(default)]
    pub leave_type_name: Option<String>,
    pub is_half_day: bool,
    /// A half-day leave: `first` or `second` half of the day off (RQ-8).
    #[sqlx(default)]
    pub leave_half: Option<String>,
    /// The shift a late arrival, early departure or excuse is for (split days).
    #[sqlx(default)]
    pub work_shift_id: Option<Uuid>,
    pub title: Option<String>,
    pub location: Option<String>,
    /// The record a `correction` proposes to fix. `None` for every other kind.
    pub attendance_record_id: Option<Uuid>,
    pub reason: Option<String>,
    pub status: String,
    /// Whether the excused time is paid. `None` until decided.
    pub is_paid: Option<bool>,
    /// Who approved or rejected it, when and why. A later cancellation keeps
    /// these (the approval stays on record) and fills `cancelled_*`.
    pub decided_by: Option<Uuid>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decision_note: Option<String>,
    /// Who cancelled it (the person themselves or a manager), when and why.
    #[sqlx(default)]
    #[serde(default)]
    pub cancelled_by: Option<Uuid>,
    #[sqlx(default)]
    #[serde(default)]
    pub cancelled_at: Option<DateTime<Utc>>,
    #[sqlx(default)]
    #[serde(default)]
    pub cancel_note: Option<String>,
    /// Who decided it, by name — their employee's name when linked, else
    /// their account's — so a phone that can't look up the owner's account
    /// still names them (RQ-F6).
    #[sqlx(default)]
    #[serde(default)]
    pub decided_by_name: Option<String>,
    /// Who cancelled it, by name, the same way.
    #[sqlx(default)]
    #[serde(default)]
    pub cancelled_by_name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// A manager's own request waiting for someone above them (RQ-5): the
    /// owner decides it. Worked out by the server from capabilities.
    #[sqlx(default)]
    #[serde(default)]
    pub to_owner: bool,
    /// For a pending excuse or early departure: the business's (or branch's)
    /// rule, which the approve dialog starts from (RQ-7).
    #[sqlx(default)]
    #[serde(default)]
    pub paid_default: Option<bool>,
    /// For a correction: the record's current punches, so an approver sees
    /// what the proposal changes.
    #[sqlx(default)]
    #[serde(default)]
    pub record_check_in_at: Option<DateTime<Utc>>,
    #[sqlx(default)]
    #[serde(default)]
    pub record_check_out_at: Option<DateTime<Utc>>,
    /// The request is the CALLER's own (worked out for whoever asks).
    #[sqlx(default)]
    #[serde(default)]
    pub is_own: bool,
    /// The caller may approve or reject it now: it is pending, not their own,
    /// at one of their branches, and — a manager's request — they outrank
    /// the requester (RQ-5). The same checks the decision makes.
    #[sqlx(default)]
    #[serde(default)]
    pub can_decide: bool,
    /// A day of it is in an approved or paid month: approving or cancelling
    /// approved time is refused (PERIOD_CLOSED); rejecting still works.
    #[sqlx(default)]
    #[serde(default)]
    pub month_closed: bool,
    /// For a leave or mission: the days it covers that the person already
    /// clocked in on. Approving turns those worked days into leave (the
    /// punches are kept), so the approver is warned first (minor default
    /// M16). Empty for every other kind.
    #[sqlx(default)]
    #[serde(default)]
    pub worked_dates: Vec<NaiveDate>,
}

const REQUEST_SELECT: &str = r#"
    SELECT r.id, r.org_id, r.employee_id, e.name AS employee_name, r.kind, r.on_date,
           r.end_date, r.from_time, r.to_time, r.leave_type_id,
           t.name AS leave_type_name, r.is_half_day, r.leave_half, r.work_shift_id,
           r.title, r.location,
           r.attendance_record_id, r.reason, r.status, r.is_paid, r.decided_by, r.decided_at,
           r.decision_note, r.cancelled_by, r.cancelled_at, r.cancel_note,
           COALESCE(de.name, du.name) AS decided_by_name,
           COALESCE(ce.name, cu.name) AS cancelled_by_name,
           r.created_at, r.updated_at,
           ar.check_in_at AS record_check_in_at, ar.check_out_at AS record_check_out_at,
           CASE WHEN r.kind IN ('leave', 'mission') THEN
               ARRAY(SELECT DISTINCT w.business_date FROM attendance_records w
                      WHERE w.employee_id = r.employee_id AND w.covered_employee_id IS NULL
                        AND w.check_in_at IS NOT NULL
                        AND w.business_date BETWEEN r.on_date AND COALESCE(r.end_date, r.on_date)
                      ORDER BY 1)
           ELSE '{}'::date[] END AS worked_dates
      FROM staff_requests r
      JOIN employees e ON e.id = r.employee_id
      LEFT JOIN users du ON du.id = r.decided_by
      LEFT JOIN employees de ON de.user_id = r.decided_by
      LEFT JOIN users cu ON cu.id = r.cancelled_by
      LEFT JOIN employees ce ON ce.user_id = r.cancelled_by
      LEFT JOIN leave_types t ON t.id = r.leave_type_id
      LEFT JOIN attendance_records ar ON ar.id = r.attendance_record_id
"#;

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct LeaveBalance {
    pub id: Uuid,
    pub org_id: Uuid,
    pub employee_id: Uuid,
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
    SELECT b.id, b.org_id, b.employee_id, b.leave_type_id, t.name AS leave_type_name,
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
    pub employee_id: Option<Uuid>,
    /// One of `leave`, `late_arrival`, `early_departure`, `excuse`, `mission`,
    /// `correction`.
    pub kind: String,
    pub on_date: NaiveDate,
    #[serde(default)]
    pub end_date: Option<NaiveDate>,
    /// Branch-local wall clock.
    #[serde(default)]
    pub from_time: Option<NaiveTime>,
    /// Branch-local wall clock. An excuse ending at or before it starts runs
    /// past midnight.
    #[serde(default)]
    pub to_time: Option<NaiveTime>,
    /// Deprecated (RQ-2): ignored. Leave has no types.
    #[serde(default)]
    pub leave_type_id: Option<Uuid>,
    #[serde(default)]
    pub is_half_day: Option<bool>,
    /// `first` | `second`: which half of the day a half-day leave takes off.
    /// Omitted on a half day = the first.
    #[serde(default)]
    pub leave_half: Option<String>,
    /// The shift a late arrival, early departure or excuse is for. Omitted =
    /// the shift of the day its time falls in.
    #[serde(default)]
    pub work_shift_id: Option<Uuid>,
    /// A mission's title; when omitted the note is used.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    /// `correction` only — the record whose punch is wrong.
    #[serde(default)]
    pub attendance_record_id: Option<Uuid>,
    #[serde(default)]
    pub reason: Option<String>,
    /// Only when the request is approved as it is filed (the filer holds
    /// `hr.requests.self_approve`): leave paid or unpaid — REQUIRED for such a
    /// leave (400 `LEAVE_PAY_REQUIRED`, RQ-2) — and an excuse's pay (omitted:
    /// the rule decides).
    #[serde(default)]
    pub is_paid: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct RequestDecision {
    /// `approved` | `rejected` | `cancelled`.
    pub status: String,
    /// Required when cancelling someone else's request, or any approved one
    /// (AT-7).
    #[serde(default)]
    pub note: Option<String>,
    /// Paid or unpaid. REQUIRED when approving leave (RQ-2). For `excuse` and
    /// `early_departure`, omitted falls back to the rule
    /// (`excused_time_paid_default`, branch then business, RQ-7).
    #[serde(default)]
    pub is_paid: Option<bool>,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct RequestListQuery {
    #[serde(default)]
    pub employee_id: Option<Uuid>,
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
    pub employee_id: Option<Uuid>,
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

/// `time` on the business date or on the day after, whichever lies nearer
/// `near` — a night shift's after-midnight punch belongs to the next morning
/// (B5). Without an anchor, the business date.
async fn local_instant_near(
    pool: &PgPool,
    date: NaiveDate,
    time: NaiveTime,
    timezone: &str,
    near: Option<DateTime<Utc>>,
) -> Result<DateTime<Utc>, AppError> {
    let same = local_instant(pool, date, time, timezone).await?;
    let Some(anchor) = near else {
        return Ok(same);
    };
    let next = local_instant(pool, date + Duration::days(1), time, timezone).await?;
    Ok(
        if (next - anchor).num_seconds().abs() < (same - anchor).num_seconds().abs() {
            next
        } else {
            same
        },
    )
}

/// Trim free text; a note of nothing but punctuation (the "." people type to
/// get past a field) is no note at all.
fn clean(value: Option<&String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| v.chars().any(char::is_alphanumeric))
}

/// What a valid request stores beyond the body.
struct Shape {
    end_date: Option<NaiveDate>,
    leave_half: Option<&'static str>,
    title: Option<String>,
}

/// Reject a request whose shape its kind does not allow, with a message that says
/// what was missing. The database CHECK enforces the same rules as a backstop;
/// this exists so the API answers "a late arrival needs a time" rather than
/// surfacing a constraint name.
fn validate_shape(body: &CreateStaffRequest) -> Result<Shape, AppError> {
    let bad = |m: &str| Err(AppError::BadRequest(m.to_string()));
    let mut shape = Shape {
        end_date: None,
        leave_half: None,
        title: None,
    };
    if body.work_shift_id.is_some()
        && !matches!(
            body.kind.as_str(),
            "late_arrival" | "early_departure" | "excuse" | "correction"
        )
    {
        return bad("Only a late arrival, early departure, excuse or correction names a shift");
    }
    match body.kind.as_str() {
        "leave" => {
            let end = body.end_date.unwrap_or(body.on_date);
            if end < body.on_date {
                return bad("End date is before start date");
            }
            let half = body.is_half_day.unwrap_or(false);
            if half && end != body.on_date {
                return bad("A half day must start and end on the same date");
            }
            shape.end_date = Some(end);
            shape.leave_half = match (half, body.leave_half.as_deref()) {
                (false, None) => None,
                (false, Some(_)) => return bad("Only a half day says which half"),
                (true, None | Some("first")) => Some("first"),
                (true, Some("second")) => Some("second"),
                (true, Some(_)) => return bad("A half day is the first or the second half"),
            };
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
            (Some(from), Some(to)) if to == from => {
                return bad("The window must end after it starts");
            }
            // Ending at or before it starts on the clock = past midnight.
            (Some(from), Some(to)) => {
                if to < from {
                    shape.end_date = Some(body.on_date + Duration::days(1));
                }
            }
            _ => return bad("A permission needs a start and an end time"),
        },
        "mission" => {
            // The app sends the note; a title-less mission takes it (§3).
            shape.title = clean(body.title.as_ref()).or_else(|| clean(body.reason.as_ref()));
            if shape.title.is_none() {
                return bad("A mission needs a title or a note saying where you'll be");
            }
            let end = body.end_date.unwrap_or(body.on_date);
            if end < body.on_date {
                return bad("End date is before start date");
            }
            shape.end_date = Some(end);
        }
        "correction" => {
            // The record it fixes, or — a rostered shift nobody clocked yet —
            // the shift (RQ-9).
            if body.attendance_record_id.is_none() && body.work_shift_id.is_none() {
                return bad("A correction needs the attendance record or the shift it fixes");
            }
            match (body.from_time, body.to_time) {
                (None, None) => return bad("A correction needs a proposed time"),
                (Some(from), Some(to)) if to == from => {
                    return bad("The check-out must be after the check-in");
                }
                _ => {}
            }
        }
        other => {
            validate_kind(other)?;
        }
    }
    Ok(shape)
}

// ── Where a request sits on its shift (RQ-11, B4, B5) ─────────

/// A rostered shift on the branch's wall clock.
struct LocalShift {
    id: Uuid,
    start: NaiveDateTime,
    end: NaiveDateTime,
    /// The branch's wall clock now.
    now: NaiveDateTime,
}

/// The person's rostered shifts of `date`, on the wall clock of the branch the
/// day is priced at. Empty when nothing is rostered or no branch is known.
async fn local_shifts(
    pool: &PgPool,
    employee_id: Uuid,
    date: NaiveDate,
) -> Result<Vec<LocalShift>, AppError> {
    let Some(branch) = request_branch(pool, employee_id, date).await? else {
        return Ok(Vec::new());
    };
    let tz = crate::staff::branch_timezone(pool, branch).await?;
    let shifts = crate::staff::schedules::resolve_shifts_for(pool, employee_id, date, &tz).await?;
    let now: NaiveDateTime = sqlx::query_scalar("SELECT (now() AT TIME ZONE $1)::timestamp")
        .bind(&tz)
        .fetch_one(pool)
        .await?;
    Ok(shifts
        .into_iter()
        .map(|s| {
            let end_day = if s.crosses_midnight || s.end_time <= s.start_time {
                s.on_date + Duration::days(1)
            } else {
                s.on_date
            };
            LocalShift {
                id: s.work_shift_id,
                start: s.on_date.and_time(s.start_time),
                end: end_day.and_time(s.end_time),
                now,
            }
        })
        .collect())
}

/// `time` on the shift's side of midnight: the business date or the day
/// after, whichever lies inside (or nearer) the shift.
fn place(shift: &LocalShift, date: NaiveDate, time: NaiveTime) -> NaiveDateTime {
    let distance = |at: NaiveDateTime| {
        if at < shift.start {
            (shift.start - at).num_seconds()
        } else if at > shift.end {
            (at - shift.end).num_seconds()
        } else {
            0
        }
    };
    let same = date.and_time(time);
    let next = same + Duration::days(1);
    if distance(next) < distance(same) {
        next
    } else {
        same
    }
}

/// A timed request's excused window on the branch's wall clock, resolved on
/// the shift it is for (the named one, else the one its time falls in): a
/// late arrival runs from the shift's start to the arrival, an early
/// departure from the departure to the shift's end, an excuse between its
/// two times on the shift's side of midnight. `None` when no shift answers
/// (the database then compares `on_date + time`, as before).
fn request_window(
    shifts: &[LocalShift],
    kind: &str,
    on_date: NaiveDate,
    work_shift_id: Option<Uuid>,
    from: Option<NaiveTime>,
    to: Option<NaiveTime>,
) -> Option<(NaiveDateTime, NaiveDateTime)> {
    let key = match kind {
        "late_arrival" => to?,
        "early_departure" | "excuse" => from?,
        _ => return None,
    };
    let shift = match work_shift_id {
        Some(id) => shifts.iter().find(|s| s.id == id)?,
        None => shifts.iter().find(|s| {
            let at = place(s, on_date, key);
            at >= s.start && at <= s.end
        })?,
    };
    Some(match kind {
        "late_arrival" => (shift.start, place(shift, on_date, to?).max(shift.start)),
        "early_departure" => (place(shift, on_date, from?).min(shift.end), shift.end),
        _ => {
            let f = place(shift, on_date, from?);
            let mut t = place(shift, on_date, to?);
            if t <= f {
                t = f.date().and_time(to?);
                if t <= f {
                    t += Duration::days(1);
                }
            }
            (f, t)
        }
    })
}

// ── The classifier's input ────────────────────────────────────

/// The half of a day's rostered time a half-day leave takes off (RQ-8): the
/// instant where half the day's rostered minutes have passed splits it, so on
/// a split day of two equal shifts the first half is the morning shift.
pub fn half_day_window(
    shifts: &[(DateTime<Utc>, DateTime<Utc>)],
    half: &str,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let mut sorted: Vec<_> = shifts.iter().copied().filter(|(s, e)| e > s).collect();
    sorted.sort();
    let first = sorted.first()?.0;
    let last = sorted.iter().map(|(_, e)| *e).max()?;
    let total: i64 = sorted.iter().map(|(s, e)| (*e - *s).num_seconds()).sum();
    let mut left = total / 2;
    let mut cut = last;
    for (s, e) in &sorted {
        let len = (*e - *s).num_seconds();
        if left <= len {
            cut = *s + Duration::seconds(left);
            break;
        }
        left -= len;
    }
    Some(if half == "second" {
        (cut, last)
    } else {
        (first, cut)
    })
}

/// Resolve one employee's approved requests for one business date into the
/// adjustments the attendance math consumes.
///
/// Times are stored as the branch's LOCAL wall clock (an employee agreeing to
/// arrive "by 10:00" means 10:00 where they work), so they are converted here
/// with the same `(date + time) AT TIME ZONE` treatment the rest of the module
/// uses — the tz database owns DST, not us. Each time is offered on the business
/// date and the day after; `DayAdjustments::for_shift` keeps the one inside the
/// shift, which is what makes a night shift's 01:00 mean the next morning.
pub(crate) async fn day_adjustments(
    pool: &PgPool,
    employee_id: Uuid,
    business_date: NaiveDate,
    timezone: &str,
    settings: &AttendanceSettings,
) -> Result<DayAdjustments, AppError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        kind: String,
        is_half_day: bool,
        leave_half: Option<String>,
        work_shift_id: Option<Uuid>,
        is_paid: Option<bool>,
        crosses: bool,
        from0: Option<DateTime<Utc>>,
        from1: Option<DateTime<Utc>>,
        to0: Option<DateTime<Utc>>,
        to1: Option<DateTime<Utc>>,
    }

    let rows: Vec<Row> = sqlx::query_as(
        r#"
        SELECT r.kind, r.is_half_day, r.leave_half, r.work_shift_id,
               COALESCE(r.is_paid, lt.is_paid) AS is_paid,
               COALESCE(r.kind = 'excuse' AND r.end_date = r.on_date + 1, false) AS crosses,
               ($2::date + r.from_time) AT TIME ZONE $3     AS from0,
               ($2::date + 1 + r.from_time) AT TIME ZONE $3 AS from1,
               ($2::date + r.to_time) AT TIME ZONE $3       AS to0,
               ($2::date + 1 + r.to_time) AT TIME ZONE $3   AS to1
          FROM staff_requests r
          LEFT JOIN leave_types lt ON lt.id = r.leave_type_id
         WHERE r.employee_id = $1
           AND r.status = 'approved'
           -- Corrections are excluded on purpose: they REWRITE the punch on
           -- approval rather than forgive a window. Letting one through here
           -- would forgive the very lateness it just recorded.
           AND r.kind <> 'correction'
           AND ((r.kind IN ('leave', 'mission')
                 AND r.on_date <= $2 AND COALESCE(r.end_date, r.on_date) >= $2)
                OR (r.kind NOT IN ('leave', 'mission') AND r.on_date = $2))
        "#,
    )
    .bind(employee_id)
    .bind(business_date)
    .bind(timezone)
    .fetch_all(pool)
    .await?;

    let default_paid = settings.excused_time_paid_default;
    let mut adj = DayAdjustments::default();
    // Where the pings put the person that day (D2): approved time off counts
    // only the minutes actually away.
    let pings: Vec<(Uuid, DateTime<Utc>, bool, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT p.attendance_record_id, p.at, p.inside, a.check_out_at \
           FROM attendance_pings p JOIN attendance_records a ON a.id = p.attendance_record_id \
          WHERE a.employee_id = $1 AND a.business_date = $2 AND a.covered_employee_id IS NULL \
          ORDER BY p.attendance_record_id, p.at",
    )
    .bind(employee_id)
    .bind(business_date)
    .fetch_all(pool)
    .await?;
    adj.away = away_runs(&pings);
    for row in rows {
        let timed = |candidates: [Option<DateTime<Utc>>; 2], paid: bool| TimedRequest {
            candidates: candidates.into_iter().flatten().collect(),
            work_shift_id: row.work_shift_id,
            paid,
        };
        match row.kind.as_str() {
            "late_arrival" => adj.late_arrivals.push(timed([row.to0, row.to1], true)),
            "early_departure" => adj.early_departures.push(timed(
                [row.from0, row.from1],
                row.is_paid.unwrap_or(default_paid),
            )),
            "excuse" => {
                let candidates = match (row.from0, row.from1, row.to0, row.to1) {
                    (Some(f0), _, _, Some(t1)) if row.crosses => vec![(f0, t1)],
                    (Some(f0), Some(f1), Some(t0), Some(t1)) => vec![(f0, t0), (f1, t1)],
                    _ => vec![],
                };
                adj.excuses.push(WindowRequest {
                    candidates,
                    work_shift_id: row.work_shift_id,
                    paid: row.is_paid.unwrap_or(default_paid),
                });
            }
            "mission" => {
                adj.on_leave = true;
                adj.leave_paid = true;
            }
            "leave" => {
                // A decided leave says paid or unpaid; an old one without an
                // answer stays as it always read — paid.
                let paid = row.is_paid.unwrap_or(true);
                if row.is_half_day && settings.half_day_leave_counts != "whole_day" {
                    let shifts = crate::staff::schedules::resolve_shifts_for(
                        pool,
                        employee_id,
                        business_date,
                        timezone,
                    )
                    .await?;
                    let windows: Vec<_> = shifts
                        .iter()
                        .map(|s| (s.scheduled_start_at, s.scheduled_end_at))
                        .collect();
                    let half = row.leave_half.as_deref().unwrap_or("first");
                    if let Some((from, to)) = half_day_window(&windows, half) {
                        adj.half_off = Some(ExcusedWindow { from, to, paid });
                    }
                } else {
                    adj.on_leave = true;
                    adj.leave_paid = adj.leave_paid || paid;
                }
            }
            _ => {}
        }
    }
    Ok(adj)
}

/// The runs a record's pings put the person outside the fence: from the
/// first outside ping to the next inside one, an open run to the record's
/// check-out (or for good while it is open; readers clip to the punches).
/// `pings` are `(record, at, inside, check_out)`, grouped by record, oldest
/// first.
fn away_runs(
    pings: &[(Uuid, DateTime<Utc>, bool, Option<DateTime<Utc>>)],
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut runs = Vec::new();
    let mut open: Option<(Uuid, DateTime<Utc>, Option<DateTime<Utc>>)> = None;
    for &(record, at, inside, out) in pings {
        if let Some((r, from, end)) = open
            && r != record
        {
            runs.push((from, end.unwrap_or(DateTime::<Utc>::MAX_UTC)));
            open = None;
        }
        match (inside, open) {
            (false, None) => open = Some((record, at, out)),
            (true, Some((_, from, _))) => {
                runs.push((from, at));
                open = None;
            }
            _ => {}
        }
    }
    if let Some((_, from, end)) = open {
        runs.push((from, end.unwrap_or(DateTime::<Utc>::MAX_UTC)));
    }
    runs
}

// ── Month guard (RQ-4) ────────────────────────────────────────

/// A month whose payroll is approved (`generated`), paid or closed can't be
/// changed by a request or a manual attendance edit (RQ-4, AT-7): after
/// approval the fix goes into the next month. Every day of `[from, to]` is
/// checked, so a leave can't reach back into a closed month through its end.
///
/// A thin delegate to THE closed-month check, `period_lock` (409
/// `PERIOD_CLOSED`), so requests, attendance edits and pay lines answer alike.
pub(crate) async fn require_open_month(
    pool: &PgPool,
    org_id: Uuid,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<(), AppError> {
    crate::staff::period_lock::assert_range_open(pool, org_id, from, to.max(from), "this change")
        .await
}

// ── Who decides (RQ-5) ────────────────────────────────────────

/// Does this person decide requests themselves (a manager)? Their own
/// requests then go above them.
async fn is_decider(pool: &PgPool, user_id: Option<Uuid>) -> Result<bool, AppError> {
    let Some(user) = user_id else {
        return Ok(false);
    };
    let live: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id = $1 AND is_active AND deleted_at IS NULL)",
    )
    .bind(user)
    .fetch_one(pool)
    .await?;
    if !live {
        return Ok(false);
    }
    Ok(crate::authz::require::effective(pool, user, None)
        .await?
        .can(Cap::HrLeaveEdit))
}

/// The branch a request's day is priced at: the record's on that date, else
/// the person's first branch.
async fn request_branch(
    pool: &PgPool,
    employee_id: Uuid,
    date: NaiveDate,
) -> Result<Option<Uuid>, AppError> {
    let on_record: Option<Uuid> = sqlx::query_scalar(
        "SELECT branch_id FROM attendance_records \
          WHERE employee_id = $1 AND business_date = $2 \
          ORDER BY covered_employee_id IS NOT NULL, check_in_at NULLS LAST LIMIT 1",
    )
    .bind(employee_id)
    .bind(date)
    .fetch_optional(pool)
    .await?;
    Ok(match on_record {
        Some(b) => Some(b),
        None => access::branches_of(pool, employee_id)
            .await?
            .first()
            .copied(),
    })
}

/// The rule's paid default for an excuse or early departure (RQ-7): the
/// branch's override, else the business's.
async fn paid_default_for(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
    date: NaiveDate,
) -> Result<bool, AppError> {
    let branch = request_branch(pool, employee_id, date).await?;
    Ok(
        crate::staff::attendance::load_settings(pool, org_id, branch)
            .await?
            .excused_time_paid_default,
    )
}

/// Fill the fields the server works out: who decides a pending request, and
/// the pay rule an approver starts from.
async fn enrich(pool: &PgPool, rows: &mut [StaffRequest]) -> Result<(), AppError> {
    let mut deciders: HashMap<Uuid, bool> = HashMap::new();
    let mut defaults: HashMap<(Uuid, NaiveDate), bool> = HashMap::new();
    for r in rows.iter_mut().filter(|r| r.status == "pending") {
        let to_owner = match deciders.get(&r.employee_id) {
            Some(v) => *v,
            None => {
                let user: Option<Uuid> =
                    sqlx::query_scalar("SELECT user_id FROM employees WHERE id = $1")
                        .bind(r.employee_id)
                        .fetch_optional(pool)
                        .await?
                        .flatten();
                let v = is_decider(pool, user).await?;
                deciders.insert(r.employee_id, v);
                v
            }
        };
        r.to_owner = to_owner;
        if matches!(r.kind.as_str(), "excuse" | "early_departure") {
            let key = (r.employee_id, r.on_date);
            let v = match defaults.get(&key) {
                Some(v) => *v,
                None => {
                    let v = paid_default_for(pool, r.org_id, r.employee_id, r.on_date).await?;
                    defaults.insert(key, v);
                    v
                }
            };
            r.paid_default = Some(v);
        }
    }
    Ok(())
}

/// Fill `is_own` / `can_decide` for the caller, with the same checks
/// [`decide_request`] makes, so no client guesses them.
async fn mark_for_caller(
    pool: &PgPool,
    org_id: Uuid,
    claims: Option<&Claims>,
    my_employee: Option<Uuid>,
    rows: &mut [StaffRequest],
) -> Result<(), AppError> {
    let mut seen: HashMap<Uuid, (bool, bool)> = HashMap::new();
    for r in rows.iter_mut() {
        let (own, may) = match seen.get(&r.employee_id) {
            Some(v) => *v,
            None => {
                let subject = access::subject(pool, org_id, r.employee_id).await?;
                let own =
                    my_employee == Some(r.employee_id) || claims.is_some_and(|c| subject.is(c));
                let may = match claims {
                    Some(c) if !own => {
                        access::require_for(pool, c, Cap::HrLeaveEdit, &subject)
                            .await
                            .is_ok()
                            && match subject.user_id {
                                Some(user) if is_decider(pool, Some(user)).await? => {
                                    crate::permissions::guard::require_dominance(
                                        pool,
                                        c,
                                        user,
                                        Cap::HrLeaveEdit,
                                    )
                                    .await
                                    .is_ok()
                                }
                                _ => true,
                            }
                    }
                    _ => false,
                };
                seen.insert(r.employee_id, (own, may));
                (own, may)
            }
        };
        r.is_own = own;
        r.can_decide = may && r.status == "pending";
        // Dated in an approved or paid month (RQ-4): approving is refused with
        // PERIOD_CLOSED (rejecting isn't), so clients offer Reject only.
        r.month_closed = crate::staff::period_lock::any_closed_in(
            pool,
            org_id,
            r.on_date,
            r.end_date.unwrap_or(r.on_date),
        )
        .await?;
    }
    Ok(())
}

// ── Requests CRUD ─────────────────────────────────────────────

async fn insert_request(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
    body: &CreateStaffRequest,
) -> Result<StaffRequest, AppError> {
    validate_kind(&body.kind)?;
    let shape = validate_shape(body)?;
    // RQ-4: an approved, paid or closed month is frozen — every day of it.
    require_open_month(
        pool,
        org_id,
        body.on_date,
        shape.end_date.unwrap_or(body.on_date),
    )
    .await?;

    if let Some(shift) = body.work_shift_id {
        let known: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM work_shifts WHERE id = $1 AND org_id = $2)",
        )
        .bind(shift)
        .bind(org_id)
        .fetch_one(pool)
        .await?;
        if !known {
            return Err(AppError::NotFound("Shift not found".into()));
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
              WHERE id = $1 AND employee_id = $2 AND org_id = $3",
        )
        .bind(record_id)
        .bind(employee_id)
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

    // A correction that names a shift instead of a record (RQ-9): the shift
    // must be the person's, rostered that day and already started. If the
    // day has a record for it by now, the correction fixes that record.
    let mut record_id = body.attendance_record_id;
    let mut shift_id = body.work_shift_id;
    let needs_shifts = matches!(
        body.kind.as_str(),
        "late_arrival" | "early_departure" | "excuse"
    ) || (body.kind == "correction" && record_id.is_none());
    let shifts = if needs_shifts {
        local_shifts(pool, employee_id, body.on_date).await?
    } else {
        Vec::new()
    };
    if body.kind == "correction" {
        if record_id.is_some() {
            shift_id = None;
        } else if let Some(shift) = shift_id {
            if !shifts.iter().any(|s| s.id == shift) {
                return Err(AppError::BadRequest(
                    "That shift isn't on your roster that day".into(),
                ));
            }
            // Nobody corrects a shift that hasn't started yet.
            if shifts.iter().any(|s| s.id == shift && s.start > s.now) {
                return Err(AppError::BadRequest("That shift hasn't started yet".into()));
            }
            record_id = sqlx::query_scalar(
                "SELECT id FROM attendance_records \
                  WHERE employee_id = $1 AND business_date = $2 AND work_shift_id = $3 \
                    AND covered_employee_id IS NULL",
            )
            .bind(employee_id)
            .bind(body.on_date)
            .bind(shift)
            .fetch_optional(pool)
            .await?;
            if record_id.is_some() {
                shift_id = None;
            }
        }
    }
    let window = request_window(
        &shifts,
        &body.kind,
        body.on_date,
        body.work_shift_id,
        body.from_time,
        body.to_time,
    );

    // `staff_requests_no_overlap` and the one-live-correction indexes are the
    // arbiters: a violation comes back as a DB error, mapped to the right 409
    // message in `AppError::from`.
    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO staff_requests \
             (org_id, employee_id, kind, on_date, end_date, from_time, to_time, \
              is_half_day, leave_half, work_shift_id, title, location, \
              attendance_record_id, reason, window_from, window_to) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, COALESCE($8, FALSE), $9, $10, $11, $12, $13, $14, \
                 $15, $16) \
         RETURNING id",
    )
    .bind(org_id)
    .bind(employee_id)
    .bind(&body.kind)
    .bind(body.on_date)
    .bind(shape.end_date)
    .bind(body.from_time)
    .bind(body.to_time)
    .bind(body.is_half_day)
    .bind(shape.leave_half)
    .bind(shift_id)
    .bind(shape.title)
    .bind(clean(body.location.as_ref()))
    .bind(record_id)
    .bind(clean(body.reason.as_ref()))
    .bind(window.map(|w| w.0))
    .bind(window.map(|w| w.1))
    .fetch_one(pool)
    .await?;

    load_request(pool, id).await
}

pub(crate) async fn load_request(pool: &PgPool, id: Uuid) -> Result<StaffRequest, AppError> {
    let mut row = sqlx::query_as::<_, StaffRequest>(&format!("{REQUEST_SELECT} WHERE r.id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::NotFound("Request not found".into()))?;
    enrich(pool, std::slice::from_mut(&mut row)).await?;
    Ok(row)
}

/// Will this filing be approved as it is filed? Only the filer's OWN request,
/// when they hold `hr.requests.self_approve` (RQ-5).
async fn self_approves(
    pool: &PgPool,
    claims: Option<&Claims>,
    subject: &access::Subject,
) -> Result<bool, AppError> {
    match claims {
        Some(c) if subject.is(c) => {
            access::can_for(pool, c, Cap::HrRequestsSelfApprove, subject).await
        }
        _ => Ok(false),
    }
}

/// A leave that self-approves at filing has no approver to choose paid or
/// unpaid, so it must carry the choice (RQ-2: every leave is approved as paid
/// or unpaid). Checked BEFORE the request is stored, so a refusal leaves
/// nothing behind.
async fn require_leave_pay_choice(
    pool: &PgPool,
    claims: Option<&Claims>,
    subject: &access::Subject,
    body: &CreateStaffRequest,
) -> Result<(), AppError> {
    if body.kind == "leave"
        && body.is_paid.is_none()
        && self_approves(pool, claims, subject).await?
    {
        return Err(AppError::CodedVars {
            status: 400,
            code: "LEAVE_PAY_REQUIRED",
            reason: "Say whether this leave is paid or unpaid: it is approved as it is filed."
                .into(),
            vars: serde_json::json!({}),
        });
    }
    Ok(())
}

/// After filing: approve it at once when the filer approves their own
/// requests (RQ-5), otherwise tell whoever decides it. Returns the row as it
/// now stands, so the app says what the server did.
async fn after_filing(
    pool: &PgPool,
    org_id: Uuid,
    claims: Option<&Claims>,
    subject: &access::Subject,
    row: StaffRequest,
    is_paid: Option<bool>,
) -> Result<StaffRequest, AppError> {
    if let Some(c) = claims
        && self_approves(pool, claims, subject).await?
    {
        let paid = match row.kind.as_str() {
            // Required at filing (require_leave_pay_choice); never guessed.
            "leave" => is_paid,
            "excuse" | "early_departure" => is_paid,
            _ => None,
        };
        apply_decision(
            pool,
            org_id,
            row.id,
            "approved",
            c.user_id_safe().ok(),
            None,
            paid,
        )
        .await?;
        return load_request(pool, row.id).await;
    }

    let name = crate::staff::dawam::employee_name(pool, subject.id).await;
    let args = serde_json::json!({ "name": name, "kind": row.kind, "date": row.on_date });
    if row.to_owner {
        // A manager's own request waits for someone above them: the owner is
        // told (RQ-5), not the requester's peers.
        for owner in crate::staff::dawam::owners(pool, org_id).await? {
            if owner != subject.id {
                crate::staff::dawam::notify(pool, org_id, owner, "staff.n_request", args.clone())
                    .await;
            }
        }
    } else {
        crate::staff::dawam::notify_managers(
            pool,
            org_id,
            subject.home(),
            Cap::HrLeaveEdit,
            Some(subject.id),
            "staff.n_request",
            args,
        )
        .await;
    }
    Ok(row)
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
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    // A manager sees the requests of their branches' people (RO-6).
    let scope = access::scope(pool.get_ref(), &claims, org_id, Cap::HrLeaveRead).await?;
    validate_status_filter(query.status.as_deref())?;
    if let Some(kind) = query.kind.as_deref() {
        validate_kind(kind)?;
    }

    let mut rows = sqlx::query_as::<_, StaffRequest>(&format!(
        "{REQUEST_SELECT} \
          WHERE r.org_id = $1 \
            AND ($2::uuid IS NULL OR r.employee_id = $2) \
            AND ($3::text IS NULL OR r.kind = $3) \
            AND ($4::text IS NULL OR r.status = $4) \
            AND ($5::date IS NULL OR COALESCE(r.end_date, r.on_date) >= $5) \
            AND ($6::date IS NULL OR r.on_date <= $6) \
            AND {} \
          ORDER BY r.status = 'pending' DESC, r.on_date DESC, r.created_at DESC",
        access::in_scope("r.employee_id", 7)
    ))
    .bind(org_id)
    .bind(query.employee_id)
    .bind(query.kind.as_deref())
    .bind(query.status.as_deref())
    .bind(query.from)
    .bind(query.to)
    .bind(scope.as_deref())
    .fetch_all(pool.get_ref())
    .await?;
    enrich(pool.get_ref(), &mut rows).await?;
    let me: Option<Uuid> = req
        .extensions()
        .get::<StaffPrincipal>()
        .map(|m| m.employee_id);
    mark_for_caller(pool.get_ref(), org_id, Some(&claims), me, &mut rows).await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    post, path = "/staff/requests", tag = "staff",
    request_body = CreateStaffRequest,
    responses((status = 201, description = "Request filed (approved at once when it is the filer's own and they approve their own)", body = StaffRequest), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_request_admin(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CreateStaffRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::gate(pool.get_ref(), &claims, org_id, Cap::HrLeaveCreate).await?;
    let employee_id = body
        .employee_id
        .ok_or_else(|| AppError::BadRequest("employee_id is required".into()))?;
    let subject = access::subject(pool.get_ref(), org_id, employee_id).await?;
    access::require_for(pool.get_ref(), &claims, Cap::HrLeaveCreate, &subject).await?;

    require_leave_pay_choice(pool.get_ref(), Some(&claims), &subject, &body).await?;
    let row = insert_request(pool.get_ref(), org_id, employee_id, &body).await?;
    let row = after_filing(
        pool.get_ref(),
        org_id,
        Some(&claims),
        &subject,
        row,
        body.is_paid,
    )
    .await?;
    let mut rows = [row];
    mark_for_caller(pool.get_ref(), org_id, Some(&claims), None, &mut rows).await?;
    let [row] = rows;
    Ok(HttpResponse::Created().json(row))
}

#[utoipa::path(
    post, path = "/staff/me/requests", tag = "staff",
    request_body = CreateStaffRequest,
    responses((status = 201, description = "Request filed; `status` says whether it was approved as filed (RQ-5)", body = StaffRequest), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn create_my_request(
    req: HttpRequest,
    me: Me,
    pool: crate::db::Db,
    body: web::Json<CreateStaffRequest>,
) -> Result<HttpResponse, AppError> {
    let org_id = me.org_id;
    let employee_id = me.employee_id;
    let pool = pool.get_ref();
    let subject = access::subject(pool, org_id, employee_id).await?;

    // A linked, active manager acts through their account (`caller`).
    let claims: Option<Claims> = req.extensions().get::<Claims>().cloned();
    require_leave_pay_choice(pool, claims.as_ref(), &subject, &body).await?;
    let row = insert_request(pool, org_id, employee_id, &body).await?;
    let row = after_filing(pool, org_id, claims.as_ref(), &subject, row, body.is_paid).await?;
    let mut rows = [row];
    mark_for_caller(pool, org_id, None, Some(employee_id), &mut rows).await?;
    let [row] = rows;
    Ok(HttpResponse::Created().json(row))
}

#[utoipa::path(
    get, path = "/staff/me/requests", tag = "staff",
    responses((status = 200, description = "The employee's own requests", body = Vec<StaffRequest>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_requests(me: Me, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let mut rows = sqlx::query_as::<_, StaffRequest>(&format!(
        "{REQUEST_SELECT} WHERE r.employee_id = $1 ORDER BY r.on_date DESC, r.created_at DESC"
    ))
    .bind(me.employee_id)
    .fetch_all(pool.get_ref())
    .await?;
    enrich(pool.get_ref(), &mut rows).await?;
    for r in rows.iter_mut() {
        r.is_own = true;
    }
    Ok(HttpResponse::Ok().json(rows))
}

#[derive(sqlx::FromRow)]
struct Existing {
    employee_id: Uuid,
    kind: String,
    on_date: NaiveDate,
    end_date: Option<NaiveDate>,
    status: String,
    from_time: Option<NaiveTime>,
    to_time: Option<NaiveTime>,
    attendance_record_id: Option<Uuid>,
    work_shift_id: Option<Uuid>,
}

const EXISTING_COLS: &str = "employee_id, kind, on_date, end_date, status, from_time, to_time, \
     attendance_record_id, work_shift_id";

fn check_transition(existing: &str, decision: &str) -> Result<(), AppError> {
    // 409 REQUEST_ALREADY_DECIDED with {status} (the request's own), for the
    // client's wording (AT-13, Mac E2E S-235).
    let decided = |reason: String| AppError::CodedVars {
        status: 409,
        code: "REQUEST_ALREADY_DECIDED",
        reason,
        vars: serde_json::json!({ "status": existing }),
    };
    if existing == decision {
        return Err(decided(format!("This request is already {decision}")));
    }
    if existing == "rejected" || existing == "cancelled" {
        return Err(decided(format!(
            "This request was already {existing} and cannot be changed"
        )));
    }
    if existing == "approved" && decision == "rejected" {
        return Err(decided(
            "An approved request cannot be rejected — cancel it instead".into(),
        ));
    }
    Ok(())
}

#[utoipa::path(
    patch, path = "/staff/requests/{id}/decision", tag = "staff",
    params(("id" = Uuid, Path, description = "Request ID")),
    request_body = RequestDecision,
    responses(
        (status = 200, description = "Decision recorded", body = StaffRequest),
        (status = 409, description = "Already decided, or the month is closed"),
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
    // The acting person: a Madar user (dashboard, or the app through a linked
    // account), and/or the staff app's own employee.
    let claims: Option<Claims> = req.extensions().get::<Claims>().cloned();
    let me: Option<StaffPrincipal> = req.extensions().get::<StaffPrincipal>().cloned();
    // Only cancelling one's OWN request needs no permission (checked below, once
    // the request is loaded). Anything else needs a manager's account and is
    // refused before the request is looked up.
    let org_id = match (&claims, &me) {
        (Some(c), _) => scope_org(&req, c)?,
        (None, Some(m)) => m.org_id,
        (None, None) => return Err(AppError::Unauthorized("Missing claims".into())),
    };
    if body.status != "cancelled" {
        let c = caller(&req)?;
        access::gate(pool.get_ref(), &c, org_id, Cap::HrLeaveEdit).await?;
    }
    let decision = validate_decision(&body.status)?;
    let actor: Option<Uuid> = claims
        .as_ref()
        .and_then(|c| c.user_id_safe().ok())
        .or(me.as_ref().and_then(|m| m.user_id));
    let pool = pool.get_ref();

    let existing: Existing = sqlx::query_as(&format!(
        "SELECT {EXISTING_COLS} FROM staff_requests WHERE id = $1 AND org_id = $2"
    ))
    .bind(*id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Request not found".into()))?;

    let subject = access::subject(pool, org_id, existing.employee_id).await?;
    let is_own = me
        .as_ref()
        .is_some_and(|m| m.employee_id == existing.employee_id)
        || claims.as_ref().is_some_and(|c| subject.is(c));
    // Cancelling one's own request needs no permission; deciding someone
    // else's does, at one of their branches (RO-6).
    let self_cancel = decision == "cancelled" && is_own;
    if !self_cancel {
        let claims = caller(&req)?;
        access::require_for(pool, &claims, Cap::HrLeaveEdit, &subject).await?;
        // RQ-5: nobody approves their own request by hand. With
        // `hr.requests.self_approve` it was approved as it was filed; without
        // it, someone above them decides.
        if is_own {
            return Err(AppError::Coded {
                status: 403,
                code: "OWN_REQUEST",
                reason: "Your own requests are decided by someone above you.".into(),
            });
        }
        // A manager's request goes ABOVE them: a peer manager of the same
        // branch can't decide it (RQ-5).
        if let Some(user) = subject.user_id
            && is_decider(pool, Some(user)).await?
        {
            crate::permissions::guard::require_dominance(pool, &claims, user, Cap::HrLeaveEdit)
                .await
                .map_err(|_| AppError::Coded {
                    status: 403,
                    code: "MANAGER_REQUEST_ABOVE",
                    reason:
                        "A manager's own request is decided by someone above them, usually the owner."
                            .into(),
                })?;
        }
    }
    let note = clean(body.note.as_ref());
    if decision == "cancelled" {
        // AT-7: undoing a decision, or someone else's request, says why.
        if (!is_own || existing.status == "approved") && note.is_none() {
            return Err(AppError::BadRequest(if existing.status == "approved" {
                "Say why this approved request is cancelled".into()
            } else {
                "Say why you are cancelling someone else's request".into()
            }));
        }
        // An approved correction has rewritten the punch; cancelling the
        // request would free the shift for another correction while the punch
        // stays rewritten (RQ-9). The punch is corrected on the attendance
        // record instead.
        if existing.kind == "correction" && existing.status == "approved" {
            return Err(AppError::Conflict(
                "An approved correction has already rewritten the punch — correct the attendance record instead".into(),
            ));
        }
    }
    if decision == "approved" && existing.kind == "leave" && body.is_paid.is_none() {
        return Err(AppError::BadRequest(
            "Say whether this leave is paid or unpaid".into(),
        ));
    }
    if decision != "rejected" {
        require_open_month(
            pool,
            org_id,
            existing.on_date,
            existing.end_date.unwrap_or(existing.on_date),
        )
        .await?;
    }
    check_transition(&existing.status, decision)?;

    let note_for_notice = note.clone().unwrap_or_default();
    apply_decision(pool, org_id, *id, decision, actor, note, body.is_paid).await?;

    if !is_own {
        // Someone else decided it — or cancelled it, approved or not: the
        // person hears either way, with the reason a cancel always carries.
        let (key, args) = match decision {
            "approved" => (
                "staff.n_request_approved",
                serde_json::json!({ "kind": existing.kind, "date": existing.on_date }),
            ),
            "cancelled" => (
                "staff.n_request_cancelled",
                serde_json::json!({ "kind": existing.kind, "date": existing.on_date,
                                    "note": note_for_notice }),
            ),
            _ => (
                "staff.n_request_rejected",
                serde_json::json!({ "kind": existing.kind, "date": existing.on_date }),
            ),
        };
        crate::staff::dawam::notify(pool, org_id, existing.employee_id, key, args).await;
    }
    let mut rows = [load_request(pool, *id).await?];
    let my_employee = me.as_ref().map(|m| m.employee_id);
    mark_for_caller(pool, org_id, claims.as_ref(), my_employee, &mut rows).await?;
    let [row] = rows;
    Ok(HttpResponse::Ok().json(row))
}

/// Record a decision and carry it out: the status flip under a row lock, then
/// an approved correction's punch, then the repricing of every covered day.
///
/// The flip comes FIRST (audit B6): a second manager rejecting or cancelling
/// in between can no longer leave a punch rewritten under a rejected request.
/// If writing the punch then fails, the approval is taken back, so a request
/// never reads approved with its correction unapplied.
async fn apply_decision(
    pool: &PgPool,
    org_id: Uuid,
    id: Uuid,
    decision: &str,
    actor: Option<Uuid>,
    note: Option<String>,
    body_is_paid: Option<bool>,
) -> Result<(), AppError> {
    let existing: Existing = sqlx::query_as(&format!(
        "SELECT {EXISTING_COLS} FROM staff_requests WHERE id = $1 AND org_id = $2"
    ))
    .bind(id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Request not found".into()))?;

    // ── is_paid resolution ──────────────────────────────────────
    // Only the window kinds and leave carry a pay decision. The approver's
    // explicit choice wins; otherwise the rule (the branch's, else the
    // business's) applies at the moment of approval, so later edits to the
    // rule never retro-change a decided request (RQ-7).
    let is_paid = match (decision, existing.kind.as_str()) {
        ("approved", "leave") => body_is_paid,
        ("approved", "excuse" | "early_departure") => Some(match body_is_paid {
            Some(paid) => paid,
            None => paid_default_for(pool, org_id, existing.employee_id, existing.on_date).await?,
        }),
        _ => None,
    };

    let mut tx = pool.begin().await?;
    // FOR UPDATE: two managers hitting Approve at once must not both decide.
    let locked: Existing = sqlx::query_as(&format!(
        "SELECT {EXISTING_COLS} FROM staff_requests WHERE id = $1 AND org_id = $2 FOR UPDATE"
    ))
    .bind(id)
    .bind(org_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::NotFound("Request not found".into()))?;
    check_transition(&locked.status, decision)?;
    let before = locked.status.clone();

    // A cancellation has its own who / when / why: the decision's columns
    // keep the approval it undoes (AT-10, E2E B-TEAM-3).
    sqlx::query(
        "UPDATE staff_requests SET status = $2, \
             decided_by = CASE WHEN $2 = 'cancelled' THEN decided_by ELSE $3 END, \
             decided_at = CASE WHEN $2 = 'cancelled' THEN decided_at ELSE now() END, \
             decision_note = CASE WHEN $2 = 'cancelled' THEN decision_note ELSE $4 END, \
             cancelled_by = CASE WHEN $2 = 'cancelled' THEN $3 ELSE cancelled_by END, \
             cancelled_at = CASE WHEN $2 = 'cancelled' THEN now() ELSE cancelled_at END, \
             cancel_note = CASE WHEN $2 = 'cancelled' THEN $4 ELSE cancel_note END, \
             is_paid = COALESCE($5, is_paid), updated_at = now() \
          WHERE id = $1",
    )
    .bind(id)
    .bind(decision)
    .bind(actor)
    .bind(note)
    .bind(is_paid)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    // ── An approved correction rewrites the punch ───────────────
    // A correction of a shift nobody clocked writes that shift's record
    // first (RQ-9), and from then on points at it.
    let record = if decision == "approved" && locked.kind == "correction" {
        match (locked.attendance_record_id, locked.work_shift_id) {
            (Some(r), _) => Some(Ok(r)),
            (None, Some(shift)) => {
                Some(record_for_shift(pool, org_id, id, &locked, shift, actor).await)
            }
            (None, None) => None,
        }
    } else {
        None
    };
    let applied = match record {
        Some(Ok(record_id)) => apply_correction(pool, org_id, &locked, record_id, actor).await,
        Some(Err(e)) => Err(e),
        None => Ok(()),
    };
    if let Err(e) = applied {
        sqlx::query(
            "UPDATE staff_requests SET status = $2, decided_by = NULL, decided_at = NULL, \
                 decision_note = NULL, updated_at = now() \
              WHERE id = $1 AND status = 'approved'",
        )
        .bind(id)
        .bind(before)
        .execute(pool)
        .await?;
        return Err(e);
    }

    // Approving removes a penalty at its source and cancelling brings it back
    // (RQ-6, RQ-12): re-derive every day the request covers. A correction was
    // applied above; a human waive, override or status is never touched (AT-7).
    if locked.kind != "correction" && matches!(decision, "approved" | "cancelled") {
        reprice_days(
            pool,
            org_id,
            locked.employee_id,
            locked.on_date,
            locked.end_date.unwrap_or(locked.on_date),
        )
        .await?;
    }
    Ok(())
}

/// The record an approved correction of an unclocked shift writes (RQ-9):
/// the rostered shift of that day, as the sweep would have written it but
/// set by a person, so a manager's earlier deletion of the day doesn't hide
/// it. If the day got a record meanwhile (the sweep's absence), that one.
async fn record_for_shift(
    pool: &PgPool,
    org_id: Uuid,
    request_id: Uuid,
    request: &Existing,
    shift_id: Uuid,
    actor: Option<Uuid>,
) -> Result<Uuid, AppError> {
    let Some(branch) = request_branch(pool, request.employee_id, request.on_date).await? else {
        return Err(AppError::Conflict(
            "That shift is no longer on the roster".into(),
        ));
    };
    let tz = crate::staff::branch_timezone(pool, branch).await?;
    let shift = crate::staff::schedules::resolve_shifts_for(
        pool,
        request.employee_id,
        request.on_date,
        &tz,
    )
    .await?
    .into_iter()
    .find(|s| s.work_shift_id == shift_id)
    .ok_or_else(|| AppError::Conflict("That shift is no longer on the roster".into()))?;
    // A correction that clocks the owner in on a shift a colleague is
    // covering would pay it twice (D1).
    if request.from_time.is_some() {
        crate::staff::attendance::refuse_if_covered(
            pool,
            request.employee_id,
            request.on_date,
            Some(shift_id),
        )
        .await?;
    }
    sqlx::query(
        "INSERT INTO attendance_records \
             (org_id, employee_id, branch_id, work_shift_id, business_date, status, \
              scheduled_start_at, scheduled_end_at, is_manual, created_by, edit_reason) \
         VALUES ($1, $2, $3, $4, $5, 'absent', $6, $7, FALSE, $8, \
                 'Written by an approved punch correction') \
         ON CONFLICT (employee_id, business_date, \
                      COALESCE(work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid)) \
            WHERE covered_employee_id IS NULL \
         DO NOTHING",
    )
    .bind(org_id)
    .bind(request.employee_id)
    .bind(shift.branch_id.unwrap_or(branch))
    .bind(shift_id)
    .bind(request.on_date)
    .bind(shift.scheduled_start_at)
    .bind(shift.scheduled_end_at)
    .bind(actor)
    .execute(pool)
    .await?;
    let record_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM attendance_records \
          WHERE employee_id = $1 AND business_date = $2 AND work_shift_id = $3 \
            AND covered_employee_id IS NULL",
    )
    .bind(request.employee_id)
    .bind(request.on_date)
    .bind(shift_id)
    .fetch_one(pool)
    .await?;
    // From now on the request fixes that record (and the one-live-correction
    // rule per record applies).
    sqlx::query(
        "UPDATE staff_requests SET attendance_record_id = $2, updated_at = now() WHERE id = $1",
    )
    .bind(request_id)
    .bind(record_id)
    .execute(pool)
    .await?;
    Ok(record_id)
}

/// Write an approved correction's proposed times to the record, in the
/// BRANCH's clock (AT-1), each on the calendar day nearest the shift so a
/// night shift's check-out after midnight lands on the next morning (B5).
async fn apply_correction(
    pool: &PgPool,
    org_id: Uuid,
    request: &Existing,
    record_id: Uuid,
    actor: Option<Uuid>,
) -> Result<(), AppError> {
    #[derive(sqlx::FromRow)]
    struct Rec {
        branch_id: Uuid,
        scheduled_start_at: Option<DateTime<Utc>>,
        scheduled_end_at: Option<DateTime<Utc>>,
        check_in_at: Option<DateTime<Utc>>,
        check_out_at: Option<DateTime<Utc>>,
    }
    let rec: Rec = sqlx::query_as(
        "SELECT branch_id, scheduled_start_at, scheduled_end_at, check_in_at, check_out_at \
           FROM attendance_records WHERE id = $1",
    )
    .bind(record_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Attendance record not found".into()))?;
    let tz = crate::staff::branch_timezone(pool, rec.branch_id).await?;
    let proposed_in = match request.from_time {
        Some(t) => Some(
            local_instant_near(
                pool,
                request.on_date,
                t,
                &tz,
                rec.scheduled_start_at.or(rec.check_in_at),
            )
            .await?,
        ),
        None => None,
    };
    let proposed_out = match request.to_time {
        Some(t) => {
            let near = rec
                .scheduled_end_at
                .or(rec.check_out_at)
                .or(proposed_in.or(rec.check_in_at));
            let mut out = local_instant_near(pool, request.on_date, t, &tz, near).await?;
            // A check-out always follows its check-in: earlier on the clock
            // means the next morning.
            if let Some(in_at) = proposed_in.or(rec.check_in_at)
                && out <= in_at
            {
                out = local_instant(pool, request.on_date + Duration::days(1), t, &tz).await?;
            }
            Some(out)
        }
        None => None,
    };
    // CL-16: an approved correction is written as `correction`.
    crate::staff::attendance::apply_punch_correction_as(
        pool,
        org_id,
        record_id,
        proposed_in,
        proposed_out,
        None,
        None,
        "Approved punch correction request",
        actor,
        Some("correction"),
    )
    .await
}

/// Re-derive and re-price a person's attendance days, e.g. after a request that
/// covers them was approved or cancelled. Automation: a manager's status and
/// the audit columns stay theirs (AT-7).
pub(crate) async fn reprice_days(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<(), AppError> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM attendance_records \
          WHERE org_id = $1 AND employee_id = $2 AND business_date BETWEEN $3 AND $4",
    )
    .bind(org_id)
    .bind(employee_id)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;
    for id in ids {
        crate::staff::attendance::reprice_record(pool, org_id, id).await?;
    }
    Ok(())
}

// ── Balances (read-only legacy) ───────────────────────────────

/// Deprecated (RQ-3): Dawam has no leave balances. This shows what older data
/// holds and is never written any more.
#[utoipa::path(
    get, path = "/staff/me/leave-balances", tag = "staff",
    params(BalanceQuery),
    responses((status = 200, description = "The employee's own balances", body = Vec<LeaveBalance>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_leave_balances(
    me: Me,
    pool: crate::db::Db,
    query: web::Query<BalanceQuery>,
) -> Result<HttpResponse, AppError> {
    let year = query.year.unwrap_or_else(|| Utc::now().year());

    let rows = sqlx::query_as::<_, LeaveBalance>(&format!(
        "{BALANCE_SELECT} WHERE b.employee_id = $1 AND b.year = $2 ORDER BY lower(t.name)"
    ))
    .bind(me.employee_id)
    .bind(year)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}
