//! The attendance ledger: clocking in and out, and correcting the result.
//!
//! THE SERVER DECIDES. The client supplies coordinates and nothing else — not the
//! time, not the branch's distance, not whether it counts as late. A device can
//! lie about all three, so every one of them is derived here:
//!
//!   * **When** — `Utc::now()`, turned into a business date by
//!     `AT TIME ZONE <branch tz>` in Postgres.
//!   * **Where** — `haversine_meters` against the branch's stored coordinates,
//!     compared to its `geo_radius_meters`. The measured distance is written to
//!     the row so a disputed check-in can be audited later.
//!   * **Which shift** — resolved from the roster, then the nearest scheduled
//!     start wins on a multi-shift day.
//!   * **Late / overtime / status** — the pure functions in
//!     [`crate::staff::rules`].
//!
//! NIGHT SHIFTS: a shift that runs 22:00→06:00 belongs to the day it STARTED. A
//! check-in just after midnight therefore looks at both today's and yesterday's
//! roster and takes the nearer scheduled start, so the 00:10 arrival lands on
//! yesterday's business date alongside the rest of that shift.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    authz::Cap,
    errors::{AppError, AppErrorResponse},
    geo::osrm::{LatLng, haversine_meters},
    staff::{
        access, branch_timezone,
        principal::{Me, caller},
        require_employee_in_org,
        rules::{self, AttendanceStatus, LateTier},
        schedules::{ResolvedShift, resolve_shifts_for},
        scope_org, validate_range,
    },
};

/// Widest attendance window one request may ask for.
const MAX_RANGE_DAYS: i64 = 400;

// ── Models ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct AttendanceRecord {
    pub id: Uuid,
    pub org_id: Uuid,
    pub employee_id: Uuid,
    #[sqlx(default)]
    pub employee_name: Option<String>,
    pub branch_id: Uuid,
    pub work_shift_id: Option<Uuid>,
    #[sqlx(default)]
    pub work_shift_name: Option<String>,
    pub business_date: NaiveDate,
    pub status: String,
    pub scheduled_start_at: Option<DateTime<Utc>>,
    pub scheduled_end_at: Option<DateTime<Utc>>,
    pub check_in_at: Option<DateTime<Utc>>,
    pub check_in_latitude: Option<f64>,
    pub check_in_longitude: Option<f64>,
    pub check_in_distance_meters: Option<f64>,
    pub check_in_method: Option<String>,
    pub check_out_at: Option<DateTime<Utc>>,
    pub check_out_latitude: Option<f64>,
    pub check_out_longitude: Option<f64>,
    pub check_out_distance_meters: Option<f64>,
    pub check_out_method: Option<String>,
    pub late_minutes: i32,
    pub early_leave_minutes: i32,
    pub overtime_minutes: i32,
    pub worked_minutes: i32,
    pub is_manual: bool,
    pub notes: Option<String>,
    pub edit_reason: Option<String>,
    pub created_by: Option<Uuid>,
    pub edited_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// A cover: whose shift this person worked (CV-*).
    #[sqlx(default)]
    pub covered_employee_id: Option<Uuid>,
    /// `pending` · `confirmed` · `rejected` for a cover.
    #[sqlx(default)]
    pub cover_status: Option<String>,
    /// `pending` · `approved` · `rejected` when overtime needs a decision.
    #[sqlx(default)]
    pub overtime_status: Option<String>,
    #[sqlx(default)]
    pub tracking_off: bool,
    /// Why someone else punched for this person.
    #[sqlx(default)]
    pub punch_reason: Option<String>,
    /// A manager set this day's status by hand; automation keeps it (AT-7).
    #[sqlx(default)]
    #[serde(default)]
    pub status_overridden: bool,
    /// Its day is in an approved or paid month (period_lock): an overtime or
    /// cover approval, a correction or a deduction on it is refused with
    /// PERIOD_CLOSED, so clients don't offer them.
    #[sqlx(default)]
    #[serde(default)]
    pub month_closed: bool,
}

/// Every attendance column plus the two denormalised names, in `AttendanceRecord`
/// field order. One constant so list, single, and returning queries cannot drift.
const RECORD_COLS: &str = r#"
    a.id, a.org_id, a.employee_id, emp.name AS employee_name, a.branch_id, a.work_shift_id,
    ws.name AS work_shift_name, a.business_date, a.status,
    a.scheduled_start_at, a.scheduled_end_at,
    a.check_in_at, a.check_in_latitude, a.check_in_longitude,
    a.check_in_distance_meters, a.check_in_method,
    a.check_out_at, a.check_out_latitude, a.check_out_longitude,
    a.check_out_distance_meters, a.check_out_method,
    a.late_minutes, a.early_leave_minutes, a.overtime_minutes, a.worked_minutes,
    a.is_manual, a.notes, a.edit_reason, a.created_by, a.edited_by,
    a.created_at, a.updated_at, a.covered_employee_id, a.cover_status,
    a.overtime_status, a.tracking_off, a.punch_reason, a.status_overridden,
    EXISTS (SELECT 1 FROM payroll_periods pp
             WHERE pp.org_id = a.org_id AND pp.status IN ('generated', 'paid', 'closed')
               AND pp.start_date <= a.business_date AND pp.end_date >= a.business_date)
        AS month_closed
"#;

const RECORD_JOINS: &str = "FROM attendance_records a \
     JOIN employees emp ON emp.id = a.employee_id \
     LEFT JOIN work_shifts ws ON ws.id = a.work_shift_id";

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct AttendanceSettings {
    pub id: Uuid,
    pub org_id: Uuid,
    pub branch_id: Option<Uuid>,
    pub late_deduction_tiers: serde_json::Value,
    pub absence_deduction_days: Decimal,
    pub default_overtime_multiplier: Decimal,
    pub auto_checkout_buffer_minutes: i32,
    pub working_days_per_month: Decimal,
    pub require_geofence: bool,
    /// Whether an approved mid-shift permission or early departure is PAID by
    /// default. The approver may override it on any individual request.
    pub excused_time_paid_default: bool,
    /// Day of the month a pay period opens (PAY-1): 26 = a 26th–25th cycle.
    pub period_start_day: i16,
    /// `off` · `automatic` · `approval` (RU-7).
    pub overtime_mode: String,
    pub overtime_day_multiplier: Decimal,
    pub overtime_night_multiplier: Decimal,
    /// What working a set-up holiday pays (RU-10).
    pub holiday_multiplier: Decimal,
    /// Salary advances owed may reach this share of monthly salary (AV-5).
    pub advance_cap_percent: Decimal,
    /// `half_shift` · `whole_day`: what a half-day leave counts as (RQ-8).
    pub half_day_leave_counts: String,
    /// Night for the night overtime rate and for suggestions (RU-8, RU-9).
    pub night_start: NaiveTime,
    pub night_end: NaiveTime,
    /// `off` · `soft` · `hard`: how the gender default weighs in suggestions (SC-12).
    pub gender_mode: String,
    /// When the business saved its rules; nobody clocks in before (RU-1).
    pub rules_saved_at: Option<DateTime<Utc>>,
    /// Labour limits, hours (RU-13). They warn, never block, and stay
    /// unconfirmed until a lawyer signs them off.
    pub limit_day_hours: Decimal,
    pub limit_week_hours: Decimal,
    pub limit_presence_hours: Decimal,
    pub limit_rest_hours: Decimal,
    pub limit_overtime_day_hours: Decimal,
    /// POS-derived coverage: one person per this many orders an hour.
    pub orders_per_staff: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// For a branch: the rules it sets itself (every other field is the
    /// business's, RU-2). Empty for the business.
    #[serde(default)]
    pub overridden: Vec<String>,
    /// The ladder the set-up step suggests (RU-1). Never used for pricing.
    #[sqlx(skip)]
    #[serde(default)]
    pub suggested_tiers: Vec<LateTier>,
}

impl AttendanceSettings {
    /// Parse the jsonb ladder. A malformed ladder is treated as "no penalties"
    /// rather than an error: payroll must still run for everyone else.
    pub(crate) fn tiers(&self) -> Vec<LateTier> {
        match serde_json::from_value(self.late_deduction_tiers.clone()) {
            Ok(tiers) => tiers,
            Err(e) => {
                // RU-4: every write is validated, so this is a hand-edited row.
                // Say so loudly instead of silently pricing nothing.
                tracing::error!(
                    org = %self.org_id, branch = ?self.branch_id, error = %e,
                    "late_deduction_tiers does not parse: no late penalties until fixed"
                );
                Vec::new()
            }
        }
    }
}

/// One employee's totals over a reporting window.
#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct AttendanceSummary {
    pub employee_id: Uuid,
    pub employee_name: String,
    pub present_days: i64,
    pub late_days: i64,
    pub absent_days: i64,
    pub half_days: i64,
    pub leave_days: i64,
    pub total_late_minutes: i64,
    pub total_overtime_minutes: i64,
    pub total_worked_minutes: i64,
}

/// What the mobile app shows on its home screen.
#[derive(Debug, Serialize, Clone, ToSchema)]
pub struct MyAttendanceToday {
    /// The business date in the relevant branch's timezone — not the device's.
    pub business_date: NaiveDate,
    /// The still-open record, when the employee is currently clocked in.
    pub open_record: Option<AttendanceRecord>,
    /// Records already closed today.
    pub closed_records: Vec<AttendanceRecord>,
    /// Shifts rostered for today. Empty = a rest day.
    pub scheduled: Vec<ResolvedShift>,
    pub can_check_in: bool,
    pub can_check_out: bool,
    /// Why `can_check_in` is false, in words the app can show verbatim.
    pub blocked_reason: Option<String>,
    /// WHERE to clock in today. Resolved server-side — from the open record, the
    /// rostered shift's branch, or the employee's single branch assignment — so
    /// the app never has to ask. A branch picker would make the geofence
    /// answerable to a dropdown, which defeats the point of having one.
    /// `None` means we cannot tell, and the app should say so rather than guess.
    pub branch_id: Option<Uuid>,
    /// That branch's name, so the app's geofence chip can say WHERE it is about
    /// to clock in rather than merely that it can.
    pub branch_name: Option<String>,
    /// The IANA timezone this payload's instants are shown in (see `crate::tz`).
    /// Additive; older clients ignore it.
    #[serde(default)]
    pub timezone: Option<String>,
}

// ── Requests ──────────────────────────────────────────────────

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CheckInRequest {
    pub branch_id: Uuid,
    /// Device coordinates. Required whenever the org enforces the geofence.
    #[serde(default)]
    pub latitude: Option<f64>,
    #[serde(default)]
    pub longitude: Option<f64>,
    /// "Always" location was refused: the shift is marked and the manager told
    /// (CL-5). Location at the punch is still required.
    #[serde(default)]
    pub tracking_off: Option<bool>,
    /// Set when the punch was queued offline; the server rebuilds its time (CL-11).
    #[serde(default)]
    pub offline: Option<crate::staff::dawam::clock::OfflineStamp>,
    /// The fix's reported accuracy, metres (CL-9: a perfect one is suspicious).
    #[serde(default)]
    pub accuracy_meters: Option<f64>,
    /// The OS's mock-location marker for this fix (CL-9).
    #[serde(default)]
    pub is_mock: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CheckOutRequest {
    #[serde(default)]
    pub latitude: Option<f64>,
    #[serde(default)]
    pub longitude: Option<f64>,
    /// Set when the punch was queued offline; the server rebuilds its time (CL-11).
    #[serde(default)]
    pub offline: Option<crate::staff::dawam::clock::OfflineStamp>,
    /// The fix's reported accuracy, metres (CL-9).
    #[serde(default)]
    pub accuracy_meters: Option<f64>,
    /// The OS's mock-location marker for this fix (CL-9).
    #[serde(default)]
    pub is_mock: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct ManualRecordRequest {
    pub employee_id: Uuid,
    pub branch_id: Uuid,
    pub business_date: NaiveDate,
    #[serde(default)]
    pub work_shift_id: Option<Uuid>,
    #[serde(default)]
    pub check_in_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub check_out_at: Option<DateTime<Utc>>,
    /// Force a status instead of deriving one — the only way to record an
    /// `absent` or `on_leave` day by hand.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// Required: a hand-written attendance row always says why it exists.
    pub reason: String,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CorrectRecordRequest {
    #[serde(default)]
    pub check_in_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub check_out_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// Required — corrections are audited.
    pub reason: String,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct PutAttendanceSettingsRequest {
    /// `None` = the org-wide default row.
    #[serde(default)]
    pub branch_id: Option<Uuid>,
    #[serde(default)]
    pub late_deduction_tiers: Option<Vec<LateTier>>,
    #[serde(default)]
    pub absence_deduction_days: Option<Decimal>,
    #[serde(default)]
    pub default_overtime_multiplier: Option<Decimal>,
    #[serde(default)]
    pub auto_checkout_buffer_minutes: Option<i32>,
    #[serde(default)]
    pub working_days_per_month: Option<Decimal>,
    #[serde(default)]
    pub require_geofence: Option<bool>,
    #[serde(default)]
    pub excused_time_paid_default: Option<bool>,
    #[serde(default)]
    pub period_start_day: Option<i16>,
    /// `off` · `automatic` · `approval`.
    #[serde(default)]
    pub overtime_mode: Option<String>,
    #[serde(default)]
    pub overtime_day_multiplier: Option<Decimal>,
    #[serde(default)]
    pub overtime_night_multiplier: Option<Decimal>,
    #[serde(default)]
    pub holiday_multiplier: Option<Decimal>,
    #[serde(default)]
    pub advance_cap_percent: Option<Decimal>,
    /// `half_shift` · `whole_day`.
    #[serde(default)]
    pub half_day_leave_counts: Option<String>,
    #[serde(default)]
    pub night_start: Option<NaiveTime>,
    #[serde(default)]
    pub night_end: Option<NaiveTime>,
    /// `off` · `soft` · `hard`; owner only (`hr.roster.settings`).
    #[serde(default)]
    pub gender_mode: Option<String>,
    #[serde(default)]
    pub limit_day_hours: Option<Decimal>,
    #[serde(default)]
    pub limit_week_hours: Option<Decimal>,
    #[serde(default)]
    pub limit_presence_hours: Option<Decimal>,
    #[serde(default)]
    pub limit_rest_hours: Option<Decimal>,
    #[serde(default)]
    pub limit_overtime_day_hours: Option<Decimal>,
    #[serde(default)]
    pub orders_per_staff: Option<i32>,
    /// Branch only: rules to take from the business again (field names, as
    /// in `overridden`).
    #[serde(default)]
    pub inherit: Option<Vec<String>>,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct AttendanceQuery {
    pub from: NaiveDate,
    pub to: NaiveDate,
    #[serde(default)]
    pub branch_id: Option<Uuid>,
    #[serde(default)]
    pub employee_id: Option<Uuid>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct RangeQuery {
    pub from: NaiveDate,
    pub to: NaiveDate,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct SettingsQuery {
    #[serde(default)]
    pub branch_id: Option<Uuid>,
}

// ── Derivation ────────────────────────────────────────────────

/// The numbers a record's stamps imply. Computed identically for a live check-out
/// and for an admin's manual correction, so a corrected row is indistinguishable
/// from one that was clocked properly.
#[derive(Debug, Clone, Copy)]
pub struct Derived {
    pub late_minutes: i64,
    pub early_leave_minutes: i64,
    pub overtime_minutes: i64,
    pub worked_minutes: i64,
    pub status: AttendanceStatus,
}

/// An approved window of time off inside a shift, and whether it is paid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExcusedWindow {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub paid: bool,
}

impl ExcusedWindow {
    /// Minutes of this window inside `[from, to]`.
    pub fn minutes_within(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> i64 {
        (self.to.min(to) - self.from.max(from)).num_minutes().max(0)
    }
}

/// One approved late arrival or early departure: the agreed time on the
/// business date and on the day after (a night shift's arrival after
/// midnight), and the shift it names, if any (split days, B4).
#[derive(Debug, Clone, Default)]
pub struct TimedRequest {
    pub candidates: Vec<DateTime<Utc>>,
    pub work_shift_id: Option<Uuid>,
    pub paid: bool,
}

/// One approved excuse: the window on the business date and on the day after.
#[derive(Debug, Clone, Default)]
pub struct WindowRequest {
    pub candidates: Vec<(DateTime<Utc>, DateTime<Utc>)>,
    pub work_shift_id: Option<Uuid>,
    pub paid: bool,
}

/// What the day's APPROVED requests forgive.
///
/// Resolved from `staff_requests` by [`crate::staff::requests::day_adjustments`]
/// and passed in, so this file's math stays a pure function of its arguments and
/// the rules remain testable without a database.
///
/// A request belongs to ONE shift of the day (RQ-9, audit B4): the one it
/// names, else the one its time falls in. [`DayAdjustments::for_shift`] picks
/// what applies to a shift, so on a split day an arrival agreed for the
/// evening never excuses the morning, and a night shift's after-midnight time
/// lands on the next calendar day (B5).
#[derive(Debug, Clone, Default)]
pub struct DayAdjustments {
    pub late_arrivals: Vec<TimedRequest>,
    pub early_departures: Vec<TimedRequest>,
    pub excuses: Vec<WindowRequest>,
    /// Approved leave or mission covers the whole day (or a half-day leave
    /// under the business's "counts as the whole day" rule, RQ-8).
    pub on_leave: bool,
    /// Whether that day off is paid (a mission always is).
    pub leave_paid: bool,
    /// A half-day leave: the half of the day's rostered time that is off (RQ-8).
    pub half_off: Option<ExcusedWindow>,
}

/// [`DayAdjustments`] resolved for one shift.
#[derive(Debug, Clone, Default)]
pub struct ShiftAdjustments {
    /// The grace deadline moves to this instant (a late arrival, or the end
    /// of a first-half leave).
    pub excused_until: Option<DateTime<Utc>>,
    /// Leaving after this instant is not early (an early departure, or the
    /// start of a second-half leave).
    pub excused_from: Option<DateTime<Utc>>,
    /// The approved early departure alone (not a half-day leave), with its pay.
    pub early_departure: Option<(DateTime<Utc>, bool)>,
    pub excuses: Vec<ExcusedWindow>,
    /// The whole shift is off.
    pub on_leave: bool,
    /// Minutes of this shift on leave (all of it when `on_leave`), and whether
    /// they are paid.
    pub leave_minutes: i64,
    pub leave_paid: bool,
}

impl DayAdjustments {
    /// What applies to the shift `[start, end]` (`shift` = its template).
    /// Without a rostered window only the whole-day facts and the excuses
    /// apply; lateness and early leave need a schedule to exist at all.
    pub fn for_shift(
        &self,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
        shift: Option<Uuid>,
    ) -> ShiftAdjustments {
        let mine = |named: Option<Uuid>| named.is_none() || named == shift;
        let window_minutes = match (start, end) {
            (Some(s), Some(e)) => (e - s).num_minutes().max(0),
            _ => 0,
        };
        let mut out = ShiftAdjustments {
            on_leave: self.on_leave,
            leave_paid: self.leave_paid,
            leave_minutes: if self.on_leave { window_minutes } else { 0 },
            ..Default::default()
        };
        if let (Some(s), Some(e)) = (start, end) {
            // A late arrival's agreed time inside the shift; the latest wins
            // (the more generous one is the one last agreed).
            for r in self.late_arrivals.iter().filter(|r| mine(r.work_shift_id)) {
                for c in r.candidates.iter().filter(|c| **c > s && **c <= e) {
                    if out.excused_until.is_none_or(|cur| *c > cur) {
                        out.excused_until = Some(*c);
                    }
                }
            }
            for r in self
                .early_departures
                .iter()
                .filter(|r| mine(r.work_shift_id))
            {
                for c in r.candidates.iter().filter(|c| **c >= s && **c < e) {
                    if out.early_departure.is_none_or(|(cur, _)| *c < cur) {
                        out.early_departure = Some((*c, r.paid));
                    }
                }
            }
            out.excused_from = out.early_departure.map(|(at, _)| at);
            for r in self.excuses.iter().filter(|r| mine(r.work_shift_id)) {
                if let Some((from, to)) = r.candidates.iter().find(|(f, t)| *f < e && *t > s) {
                    out.excuses.push(ExcusedWindow {
                        from: *from,
                        to: *to,
                        paid: r.paid,
                    });
                }
            }
            if let Some(off) = self.half_off.filter(|_| !self.on_leave) {
                let inside = off.minutes_within(s, e);
                if inside > 0 {
                    out.leave_paid = off.paid;
                    if off.from <= s && off.to >= e {
                        out.on_leave = true;
                        out.leave_minutes = window_minutes;
                    } else {
                        out.leave_minutes = inside;
                        if off.from <= s {
                            // First half off: they are due when it ends.
                            if out.excused_until.is_none_or(|cur| off.to > cur) {
                                out.excused_until = Some(off.to);
                            }
                        } else if out.excused_from.is_none_or(|cur| off.from < cur) {
                            // Second half off: they may leave when it starts.
                            out.excused_from = Some(off.from);
                        }
                    }
                }
            }
        } else {
            for r in self.excuses.iter().filter(|r| mine(r.work_shift_id)) {
                if let Some((from, to)) = r.candidates.first() {
                    out.excuses.push(ExcusedWindow {
                        from: *from,
                        to: *to,
                        paid: r.paid,
                    });
                }
            }
        }
        out
    }
}

impl ShiftAdjustments {
    /// Minutes inside `[in, out]` that an approved excuse forgives, paid or
    /// unpaid as asked.
    ///
    /// Clipped to the attendance window because an excuse that runs past
    /// check-out did not consume time the employee was being paid for anyway;
    /// crediting it would pay them for being absent twice over.
    fn excused_minutes(
        &self,
        check_in: DateTime<Utc>,
        check_out: DateTime<Utc>,
        paid: bool,
    ) -> i64 {
        self.excuses
            .iter()
            .filter(|w| w.paid == paid)
            .map(|w| w.minutes_within(check_in, check_out))
            .sum()
    }

    /// Minutes of approved but UNPAID time off inside a closed shift (RQ-7):
    /// an unpaid excuse while clocked in, and the tail of the shift an unpaid
    /// early departure covers. Priced as an `excused_unpaid` deduction.
    pub fn unpaid_excused_minutes(
        &self,
        check_in: Option<DateTime<Utc>>,
        check_out: Option<DateTime<Utc>>,
        scheduled_end: Option<DateTime<Utc>>,
    ) -> i64 {
        let (Some(in_at), Some(out_at)) = (check_in, check_out) else {
            return 0;
        };
        let mut minutes = self.excused_minutes(in_at, out_at, false);
        if let (Some((from, false)), Some(end)) = (self.early_departure, scheduled_end)
            && out_at < end
        {
            minutes += (end - out_at.max(from)).num_minutes().max(0);
        }
        minutes
    }
}

pub fn derive(
    check_in_at: Option<DateTime<Utc>>,
    check_out_at: Option<DateTime<Utc>>,
    scheduled_start_at: Option<DateTime<Utc>>,
    scheduled_end_at: Option<DateTime<Utc>>,
    shift: Option<&ResolvedShift>,
    day: &DayAdjustments,
) -> Derived {
    let adjustments = day.for_shift(
        scheduled_start_at,
        scheduled_end_at,
        shift.map(|s| s.work_shift_id),
    );
    let rules_for = shift.map(|s| s.rules());
    let grace = rules_for.map(|r| r.grace_minutes).unwrap_or(0);

    let late = match (scheduled_start_at, check_in_at) {
        // Nobody is late for a shift they have off (B3).
        _ if adjustments.on_leave => 0,
        (Some(start), Some(actual)) => {
            rules::late_minutes(start, actual, grace, adjustments.excused_until)
        }
        _ => 0,
    };

    let mut worked = match (check_in_at, check_out_at) {
        (Some(in_at), Some(out_at)) => rules::worked_minutes(
            in_at,
            out_at,
            rules_for.map(|r| r.break_minutes).unwrap_or(0),
            rules_for.map(|r| r.paid_break).unwrap_or(true),
        ),
        _ => 0,
    };

    // A PAID excuse credits the time back: the employee was permitted to be away,
    // so those minutes count toward the day. An UNPAID one leaves `worked` alone —
    // the gap is missing from the clocked span, and the `excused_unpaid`
    // deduction prices it.
    if let (Some(in_at), Some(out_at)) = (check_in_at, check_out_at) {
        worked += adjustments.excused_minutes(in_at, out_at, true);
    }

    let (overtime, early) = match (scheduled_end_at, check_out_at) {
        (Some(end), Some(out_at)) => {
            let raw_early = rules::early_leave_minutes(end, out_at);
            // An approved early departure means leaving at (or after) the agreed
            // time is not early at all. Before it, the excess still counts.
            let early = match adjustments.excused_from {
                Some(from) if out_at >= from => 0,
                Some(from) => rules::early_leave_minutes(end, out_at)
                    .saturating_sub((end - from).num_minutes().max(0)),
                None => raw_early,
            };
            (
                rules::overtime_minutes(
                    end,
                    out_at,
                    rules_for.map(|r| r.overtime_threshold_minutes).unwrap_or(0),
                ),
                early,
            )
        }
        _ => (0, 0),
    };

    // Still clocked in: the day is not over, so it is not yet a half day. Report
    // present/late from the arrival alone and let checkout settle the rest.
    let status = if adjustments.on_leave {
        AttendanceStatus::OnLeave
    } else if check_out_at.is_none() {
        if check_in_at.is_none() {
            AttendanceStatus::Absent
        } else if late > 0 {
            AttendanceStatus::Late
        } else {
            AttendanceStatus::Present
        }
    } else {
        // An approved early departure or late arrival (or half a day on
        // leave) shortens the time the employee OWED, so the half-day
        // threshold shrinks with it — otherwise permission to leave at noon,
        // or to come in at four, would still be recorded as half a day.
        let span = shift.map(|s| s.span_minutes()).unwrap_or(0);
        let excused_tail = match (adjustments.excused_from, scheduled_end_at) {
            (Some(from), Some(end)) => (end - from).num_minutes().max(0),
            _ => 0,
        };
        // The head is excused by a first-half leave or by an approved late
        // arrival alike (the agreed arrival time; Mac E2E): `excused_until`
        // carries whichever is later.
        let excused_head = match (adjustments.excused_until, scheduled_start_at) {
            (Some(until), Some(start)) => (until - start).num_minutes().max(0),
            _ => 0,
        };
        let owed = (span - excused_tail - excused_head).max(0);
        rules::classify(
            check_in_at.is_some(),
            worked,
            owed,
            rules_for.and_then(|r| r.half_day_threshold_minutes),
            late,
        )
    };

    Derived {
        late_minutes: late,
        early_leave_minutes: early,
        overtime_minutes: overtime,
        worked_minutes: worked,
        status,
    }
}

// ── Settings ──────────────────────────────────────────────────

/// One stored rule: its column, the built-in default (SQL), and whether a
/// branch may override it (RU-2). The pay period, the advance cap and the
/// gender mode are the business's alone: payroll runs for the whole business
/// (RO-9) and the gender mode is a roster setting of the owner's.
struct RuleField {
    name: &'static str,
    default: &'static str,
    branch: bool,
}

const RULE_FIELDS: &[RuleField] = &[
    RuleField {
        name: "late_deduction_tiers",
        default: "'[]'::jsonb",
        branch: true,
    },
    RuleField {
        name: "absence_deduction_days",
        default: "1.00",
        branch: true,
    },
    RuleField {
        name: "default_overtime_multiplier",
        default: "1.50",
        branch: true,
    },
    RuleField {
        name: "auto_checkout_buffer_minutes",
        default: "120",
        branch: true,
    },
    RuleField {
        name: "working_days_per_month",
        default: "30.00",
        branch: true,
    },
    RuleField {
        name: "require_geofence",
        default: "TRUE",
        branch: true,
    },
    RuleField {
        name: "excused_time_paid_default",
        default: "TRUE",
        branch: true,
    },
    RuleField {
        name: "period_start_day",
        default: "26::smallint",
        branch: false,
    },
    RuleField {
        name: "overtime_mode",
        default: "'off'",
        branch: true,
    },
    RuleField {
        name: "overtime_day_multiplier",
        default: "1.35",
        branch: true,
    },
    RuleField {
        name: "overtime_night_multiplier",
        default: "1.70",
        branch: true,
    },
    RuleField {
        name: "holiday_multiplier",
        default: "2.00",
        branch: true,
    },
    RuleField {
        name: "advance_cap_percent",
        default: "50",
        branch: false,
    },
    RuleField {
        name: "half_day_leave_counts",
        default: "'half_shift'",
        branch: true,
    },
    RuleField {
        name: "night_start",
        default: "'22:00'::time",
        branch: true,
    },
    RuleField {
        name: "night_end",
        default: "'06:00'::time",
        branch: true,
    },
    RuleField {
        name: "gender_mode",
        default: "'soft'",
        branch: false,
    },
    RuleField {
        name: "limit_day_hours",
        default: "8",
        branch: true,
    },
    RuleField {
        name: "limit_week_hours",
        default: "48",
        branch: true,
    },
    RuleField {
        name: "limit_presence_hours",
        default: "10",
        branch: true,
    },
    RuleField {
        name: "limit_rest_hours",
        default: "12",
        branch: true,
    },
    RuleField {
        name: "limit_overtime_day_hours",
        default: "2",
        branch: true,
    },
    RuleField {
        name: "orders_per_staff",
        default: "12",
        branch: true,
    },
];

/// The rules a branch may override, by name (the wire's field names).
pub fn branch_rule_fields() -> Vec<&'static str> {
    RULE_FIELDS
        .iter()
        .filter(|f| f.branch)
        .map(|f| f.name)
        .collect()
}

/// `ARRAY['field', …]` of the branch row's non-NULL overridable columns.
fn overridden_sql(row: &str) -> String {
    let parts: Vec<String> = RULE_FIELDS
        .iter()
        .filter(|f| f.branch)
        .map(|f| format!("CASE WHEN {row}.{0} IS NOT NULL THEN '{0}' END", f.name))
        .collect();
    format!(
        "COALESCE(array_remove(ARRAY[{}]::text[], NULL), '{{}}'::text[])",
        parts.join(", ")
    )
}

/// THE resolver (RU-2): the business's row, with a branch's overrides laid
/// over it field by field, and the built-in defaults under both. One query, so
/// it runs on a pool, a connection or a transaction alike.
fn effective_settings_sql() -> String {
    let fields: Vec<String> = RULE_FIELDS
        .iter()
        .map(|f| {
            if f.branch {
                format!("COALESCE(b.{0}, o.{0}, {1}) AS {0}", f.name, f.default)
            } else {
                format!("COALESCE(o.{0}, {1}) AS {0}", f.name, f.default)
            }
        })
        .collect();
    format!(
        "WITH o AS (SELECT * FROM attendance_settings WHERE org_id = $1 AND branch_id IS NULL), \
              b AS (SELECT * FROM attendance_settings \
                     WHERE org_id = $1 AND $2::uuid IS NOT NULL AND branch_id = $2) \
         SELECT COALESCE(b.id, o.id, '00000000-0000-0000-0000-000000000000'::uuid) AS id, \
                $1::uuid AS org_id, $2::uuid AS branch_id, {}, o.rules_saved_at, \
                COALESCE(b.created_at, o.created_at, now()) AS created_at, \
                COALESCE(b.updated_at, o.updated_at, now()) AS updated_at, \
                {} AS overridden \
           FROM (SELECT 1) one LEFT JOIN o ON true LEFT JOIN b ON true",
        fields.join(", "),
        overridden_sql("b")
    )
}

/// The ladder the set-up step starts from (RU-1): a few minutes cost minutes,
/// then a quarter, a half and a whole day. A suggestion only — pricing uses a
/// ladder the owner saved, never this one.
pub fn suggested_tiers() -> Vec<LateTier> {
    use rules::LateDeductionKind::{DayFraction, Minutes};
    let tier = |from, to, kind, value: Decimal| LateTier {
        from_minutes: from,
        to_minutes: to,
        kind,
        value,
    };
    vec![
        tier(1, Some(15), Minutes, Decimal::from(15)),
        tier(16, Some(30), DayFraction, Decimal::new(25, 2)),
        tier(31, Some(60), DayFraction, Decimal::new(50, 2)),
        tier(61, None, DayFraction, Decimal::ONE),
    ]
}

/// The effective settings for a branch (`None` = the business's): the
/// business row, the branch's overrides over it field by field, and the
/// built-in defaults beneath. Never fails for want of configuration.
pub async fn load_settings<'e, E>(
    pool: E,
    org_id: Uuid,
    branch_id: Option<Uuid>,
) -> Result<AttendanceSettings, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    Ok(
        sqlx::query_as::<_, AttendanceSettings>(&effective_settings_sql())
            .bind(org_id)
            .bind(branch_id)
            .fetch_one(pool)
            .await?,
    )
}

/// Who may SEE the rules (owner decision 2026-09-23): `hr.rules.view`, at the
/// branch asked about; the business's rules for anyone holding it somewhere.
async fn require_rules_view(
    pool: &PgPool,
    claims: &crate::auth::jwt::Claims,
    org_id: Uuid,
    branch_id: Option<Uuid>,
) -> Result<(), AppError> {
    match branch_id {
        Some(b) => access::require_at(pool, claims, org_id, Cap::HrRulesView, b).await,
        None => access::gate(pool, claims, org_id, Cap::HrRulesView).await,
    }
}

#[utoipa::path(
    get, path = "/staff/attendance/settings", tag = "staff",
    params(SettingsQuery),
    responses((status = 200, description = "Effective attendance settings: the business's, or a branch's with its overrides", body = AttendanceSettings), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_attendance_settings(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<SettingsQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    require_rules_view(pool.get_ref(), &claims, org_id, query.branch_id).await?;
    let mut settings = load_settings(pool.get_ref(), org_id, query.branch_id).await?;
    settings.suggested_tiers = suggested_tiers();
    Ok(HttpResponse::Ok().json(settings))
}

/// A branch and which of its rules it overrides.
#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct BranchRules {
    pub branch_id: Uuid,
    pub branch_name: String,
    /// The fields this branch sets itself; every other one is the business's.
    pub overridden: Vec<String>,
    pub updated_at: Option<DateTime<Utc>>,
}

#[utoipa::path(
    get, path = "/staff/attendance/settings/branches", tag = "staff",
    responses((status = 200, description = "The caller's branches and which rules each overrides", body = Vec<BranchRules>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_branch_rules(
    req: HttpRequest,
    pool: crate::db::Db,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    // A manager sees their own branches' overrides only.
    let scope = access::scope(pool.get_ref(), &claims, org_id, Cap::HrRulesView).await?;
    let rows = sqlx::query_as::<_, BranchRules>(&format!(
        "SELECT br.id AS branch_id, br.name AS branch_name, {} AS overridden, s.updated_at \
           FROM branches br \
           LEFT JOIN attendance_settings s ON s.org_id = br.org_id AND s.branch_id = br.id \
          WHERE br.org_id = $1 AND br.deleted_at IS NULL \
            AND ($2::uuid[] IS NULL OR br.id = ANY($2)) \
          ORDER BY lower(br.name), br.id",
        overridden_sql("s")
    ))
    .bind(org_id)
    .bind(scope.as_deref())
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    delete, path = "/staff/attendance/settings/branches/{branch_id}", tag = "staff",
    params(("branch_id" = Uuid, Path, description = "Branch whose overrides go")),
    responses((status = 204, description = "The branch follows the business's rules again"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_branch_rules(
    req: HttpRequest,
    pool: crate::db::Db,
    branch_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::require_everywhere(pool.get_ref(), &claims, org_id, Cap::HrRulesEdit).await?;
    access::require_at(
        pool.get_ref(),
        &claims,
        org_id,
        Cap::HrRulesEdit,
        *branch_id,
    )
    .await?;
    sqlx::query("DELETE FROM attendance_settings WHERE org_id = $1 AND branch_id = $2")
        .bind(org_id)
        .bind(*branch_id)
        .execute(pool.get_ref())
        .await?;
    Ok(HttpResponse::NoContent().finish())
}

/// Bind a PUT body's value for one rule column (`None` = not sent).
fn bind_rule<'q>(
    q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    name: &str,
    body: &'q PutAttendanceSettingsRequest,
    tiers: &'q Option<serde_json::Value>,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    match name {
        "late_deduction_tiers" => q.bind(tiers.clone()),
        "absence_deduction_days" => q.bind(body.absence_deduction_days),
        "default_overtime_multiplier" => q.bind(body.default_overtime_multiplier),
        "auto_checkout_buffer_minutes" => q.bind(body.auto_checkout_buffer_minutes),
        "working_days_per_month" => q.bind(body.working_days_per_month),
        "require_geofence" => q.bind(body.require_geofence),
        "excused_time_paid_default" => q.bind(body.excused_time_paid_default),
        "period_start_day" => q.bind(body.period_start_day),
        "overtime_mode" => q.bind(body.overtime_mode.as_deref()),
        "overtime_day_multiplier" => q.bind(body.overtime_day_multiplier),
        "overtime_night_multiplier" => q.bind(body.overtime_night_multiplier),
        "holiday_multiplier" => q.bind(body.holiday_multiplier),
        "advance_cap_percent" => q.bind(body.advance_cap_percent),
        "half_day_leave_counts" => q.bind(body.half_day_leave_counts.as_deref()),
        "night_start" => q.bind(body.night_start),
        "night_end" => q.bind(body.night_end),
        "gender_mode" => q.bind(body.gender_mode.as_deref()),
        "limit_day_hours" => q.bind(body.limit_day_hours),
        "limit_week_hours" => q.bind(body.limit_week_hours),
        "limit_presence_hours" => q.bind(body.limit_presence_hours),
        "limit_rest_hours" => q.bind(body.limit_rest_hours),
        "limit_overtime_day_hours" => q.bind(body.limit_overtime_day_hours),
        "orders_per_staff" => q.bind(body.orders_per_staff),
        other => unreachable!("rule field {other} has no binding"),
    }
}

/// Was this rule sent in the PUT body?
fn rule_sent(name: &str, body: &PutAttendanceSettingsRequest) -> bool {
    match name {
        "late_deduction_tiers" => body.late_deduction_tiers.is_some(),
        "absence_deduction_days" => body.absence_deduction_days.is_some(),
        "default_overtime_multiplier" => body.default_overtime_multiplier.is_some(),
        "auto_checkout_buffer_minutes" => body.auto_checkout_buffer_minutes.is_some(),
        "working_days_per_month" => body.working_days_per_month.is_some(),
        "require_geofence" => body.require_geofence.is_some(),
        "excused_time_paid_default" => body.excused_time_paid_default.is_some(),
        "period_start_day" => body.period_start_day.is_some(),
        "overtime_mode" => body.overtime_mode.is_some(),
        "overtime_day_multiplier" => body.overtime_day_multiplier.is_some(),
        "overtime_night_multiplier" => body.overtime_night_multiplier.is_some(),
        "holiday_multiplier" => body.holiday_multiplier.is_some(),
        "advance_cap_percent" => body.advance_cap_percent.is_some(),
        "half_day_leave_counts" => body.half_day_leave_counts.is_some(),
        "night_start" => body.night_start.is_some(),
        "night_end" => body.night_end.is_some(),
        "gender_mode" => body.gender_mode.is_some(),
        "limit_day_hours" => body.limit_day_hours.is_some(),
        "limit_week_hours" => body.limit_week_hours.is_some(),
        "limit_presence_hours" => body.limit_presence_hours.is_some(),
        "limit_rest_hours" => body.limit_rest_hours.is_some(),
        "limit_overtime_day_hours" => body.limit_overtime_day_hours.is_some(),
        "orders_per_staff" => body.orders_per_staff.is_some(),
        _ => false,
    }
}

#[utoipa::path(
    put, path = "/staff/attendance/settings", tag = "staff",
    request_body = PutAttendanceSettingsRequest,
    responses((status = 200, description = "Settings saved; the effective settings", body = AttendanceSettings), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_attendance_settings(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<PutAttendanceSettingsRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    // The rules are the business's, set by the owner for every branch (RO-9,
    // audit B2): the lateness ladder, absence cost, working days, overtime,
    // the pay period and the advance cap. A branch override is the same call.
    // Seeing them (`hr.rules.view`) never lets anyone change them.
    access::require_everywhere(pool.get_ref(), &claims, org_id, Cap::HrRulesEdit).await?;
    if let Some(b) = body.branch_id {
        access::require_at(pool.get_ref(), &claims, org_id, Cap::HrRulesEdit, b).await?;
    }

    // A bad ladder must never reach payroll, so it is rejected at the door.
    if let Some(tiers) = body.late_deduction_tiers.as_deref() {
        rules::validate_tiers(tiers)?;
    }
    check_setting_ranges(&body)?;
    if body.gender_mode.is_some() {
        // Roster settings are the owner's (hr.roster.settings).
        access::require_everywhere(pool.get_ref(), &claims, org_id, Cap::HrRosterSettings).await?;
    }
    let inherit: Vec<String> = body.inherit.clone().unwrap_or_default();
    let branch_fields = branch_rule_fields();
    if body.branch_id.is_some() {
        // RU-2: a branch overrides the rules, never the business's own settings.
        if let Some(f) = RULE_FIELDS
            .iter()
            .find(|f| !f.branch && rule_sent(f.name, &body))
        {
            return Err(AppError::BadRequest(format!(
                "{} is the business's setting, not a branch's",
                f.name
            )));
        }
        if let Some(bad) = inherit
            .iter()
            .find(|n| !branch_fields.contains(&n.as_str()))
        {
            return Err(AppError::BadRequest(format!(
                "'{bad}' is not a rule a branch can override"
            )));
        }
    } else if !inherit.is_empty() {
        return Err(AppError::BadRequest(
            "Only a branch can go back to the business's rules".into(),
        ));
    }

    let tiers = body
        .late_deduction_tiers
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| AppError::BadRequest("Invalid late deduction tiers".into()))?;

    // Saving the lateness ladder and the absence cost together is the RU-1
    // step that lets people clock in; a one-field save (the gender mode, a
    // limit) never does.
    let saves_rules = body.late_deduction_tiers.is_some() && body.absence_deduction_days.is_some();

    let fields: Vec<&RuleField> = RULE_FIELDS
        .iter()
        .filter(|f| body.branch_id.is_none() || f.branch)
        .collect();
    let names: Vec<&str> = fields.iter().map(|f| f.name).collect();
    // Parameters: $1 org, $2 branch, $3 inherit[], $4 saves_rules, then one per field.
    let (values, updates): (Vec<String>, Vec<String>) = fields
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let n = i + 5;
            if body.branch_id.is_some() {
                (
                    format!("${n}"),
                    format!(
                        "{0} = CASE WHEN '{0}' = ANY($3) THEN NULL \
                                    ELSE COALESCE(${n}, attendance_settings.{0}) END",
                        f.name
                    ),
                )
            } else {
                (
                    format!("COALESCE(${n}, {})", f.default),
                    format!("{0} = COALESCE(${n}, attendance_settings.{0})", f.name),
                )
            }
        })
        .unzip();
    let sql = format!(
        "INSERT INTO attendance_settings (org_id, branch_id, {}, rules_saved_at) \
         VALUES ($1, $2, {}, CASE WHEN $2::uuid IS NULL AND $4 THEN now() END) \
         ON CONFLICT (org_id, COALESCE(branch_id, '00000000-0000-0000-0000-000000000000'::uuid)) \
         DO UPDATE SET {}, \
             rules_saved_at = CASE WHEN attendance_settings.branch_id IS NULL AND $4 \
                                   THEN COALESCE(attendance_settings.rules_saved_at, now()) \
                                   ELSE attendance_settings.rules_saved_at END, \
             updated_at = now()",
        names.join(", "),
        values.join(", "),
        updates.join(", ")
    );
    let mut q = sqlx::query(&sql)
        .bind(org_id)
        .bind(body.branch_id)
        .bind(&inherit)
        .bind(saves_rules);
    for f in &fields {
        q = bind_rule(q, f.name, &body, &tiers);
    }
    q.execute(pool.get_ref()).await?;

    let mut row = load_settings(pool.get_ref(), org_id, body.branch_id).await?;
    row.suggested_tiers = suggested_tiers();
    Ok(HttpResponse::Ok().json(row))
}

/// Every number and choice of the rules within what makes sense — checked
/// here, so an impossible value is never stored and never surfaces as a raw
/// "Database error" (E2E B-SETUP-1, RU-8, AV-5, RU-13, AT-11). The refusal is
/// 400 `SETTING_OUT_OF_RANGE` with `{field, min, max}` (or `{field, allowed}`)
/// for the client's own wording (AT-13).
fn check_setting_ranges(body: &PutAttendanceSettingsRequest) -> Result<(), AppError> {
    fn out(field: &str, why: String, vars: serde_json::Value) -> AppError {
        let mut vars = vars;
        vars["field"] = serde_json::json!(field);
        AppError::CodedVars {
            status: 400,
            code: "SETTING_OUT_OF_RANGE",
            reason: why,
            vars,
        }
    }
    // (field, value, min, min inclusive, max, max inclusive)
    let d = |v: i64| Decimal::from(v);
    let ranges: [(&str, Option<Decimal>, Decimal, bool, Decimal, bool); 13] = [
        (
            "overtime_day_multiplier",
            body.overtime_day_multiplier,
            d(1),
            true,
            d(100),
            false,
        ),
        (
            "overtime_night_multiplier",
            body.overtime_night_multiplier,
            d(1),
            true,
            d(100),
            false,
        ),
        (
            "holiday_multiplier",
            body.holiday_multiplier,
            d(1),
            true,
            d(100),
            false,
        ),
        (
            "default_overtime_multiplier",
            body.default_overtime_multiplier,
            d(1),
            true,
            d(100),
            false,
        ),
        (
            "advance_cap_percent",
            body.advance_cap_percent,
            d(0),
            true,
            d(100),
            true,
        ),
        (
            "absence_deduction_days",
            body.absence_deduction_days,
            d(0),
            true,
            d(31),
            true,
        ),
        (
            "working_days_per_month",
            body.working_days_per_month,
            d(0),
            false,
            d(31),
            true,
        ),
        (
            "limit_day_hours",
            body.limit_day_hours,
            d(0),
            false,
            d(168),
            true,
        ),
        (
            "limit_week_hours",
            body.limit_week_hours,
            d(0),
            false,
            d(168),
            true,
        ),
        (
            "limit_presence_hours",
            body.limit_presence_hours,
            d(0),
            false,
            d(168),
            true,
        ),
        (
            "limit_rest_hours",
            body.limit_rest_hours,
            d(0),
            true,
            d(168),
            true,
        ),
        (
            "limit_overtime_day_hours",
            body.limit_overtime_day_hours,
            d(0),
            true,
            d(168),
            true,
        ),
        (
            "auto_checkout_buffer_minutes",
            body.auto_checkout_buffer_minutes.map(Decimal::from),
            d(0),
            true,
            d(24 * 60),
            true,
        ),
    ];
    for (field, value, min, min_in, max, max_in) in ranges {
        let Some(v) = value else { continue };
        let low = if min_in { v < min } else { v <= min };
        let high = if max_in { v > max } else { v >= max };
        if low || high {
            let (lo, hi) = (
                if min_in { "from" } else { "above" },
                if max_in { "up to" } else { "below" },
            );
            return Err(out(
                field,
                format!("{field} must be {lo} {min} and {hi} {max}"),
                serde_json::json!({ "min": min, "max": max,
                                    "min_inclusive": min_in, "max_inclusive": max_in }),
            ));
        }
    }
    if let Some(n) = body.orders_per_staff
        && !(1..=1000).contains(&n)
    {
        return Err(out(
            "orders_per_staff",
            "orders_per_staff must be from 1 and up to 1000".into(),
            serde_json::json!({ "min": 1, "max": 1000 }),
        ));
    }
    if let Some(n) = body.period_start_day
        && !(1..=28).contains(&n)
    {
        return Err(out(
            "period_start_day",
            "period_start_day must be from 1 and up to 28".into(),
            serde_json::json!({ "min": 1, "max": 28 }),
        ));
    }
    let choices: [(&str, Option<&str>, &[&str]); 3] = [
        (
            "overtime_mode",
            body.overtime_mode.as_deref(),
            &["off", "automatic", "approval"],
        ),
        (
            "half_day_leave_counts",
            body.half_day_leave_counts.as_deref(),
            &["half_shift", "whole_day"],
        ),
        (
            "gender_mode",
            body.gender_mode.as_deref(),
            &["off", "soft", "hard"],
        ),
    ];
    for (field, value, allowed) in choices {
        if let Some(v) = value
            && !allowed.contains(&v)
        {
            return Err(out(
                field,
                format!("{field} is one of {}", allowed.join(", ")),
                serde_json::json!({ "allowed": allowed }),
            ));
        }
    }
    Ok(())
}

// ── Geofence ──────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct BranchFence {
    latitude: Option<f64>,
    longitude: Option<f64>,
    geo_radius_meters: Option<i32>,
}

/// Distance from the branch centre, or an error when the punch is outside the
/// fence. Returns `None` when there is nothing to measure against and the org
/// does not require one.
pub(crate) async fn check_geofence(
    pool: &PgPool,
    branch_id: Uuid,
    latitude: Option<f64>,
    longitude: Option<f64>,
    require: bool,
) -> Result<Option<f64>, AppError> {
    let branch: BranchFence = sqlx::query_as(
        "SELECT latitude, longitude, geo_radius_meters \
           FROM branches WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(branch_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Branch not found".into()))?;

    let (Some(b_lat), Some(b_lng)) = (branch.latitude, branch.longitude) else {
        if require {
            return Err(AppError::CodedVars {
                status: 400,
                code: "BRANCH_NO_LOCATION",
                reason: "This branch has no coordinates set, so location cannot be verified. \
                 Set them on the branch."
                    .into(),
                vars: serde_json::json!({}),
            });
        }
        return Ok(None);
    };

    let (Some(lat), Some(lng)) = (latitude, longitude) else {
        if require {
            return Err(AppError::CodedVars {
                status: 400,
                code: "LOCATION_REQUIRED",
                reason: "Location is required to check in at this branch".into(),
                vars: serde_json::json!({}),
            });
        }
        return Ok(None);
    };
    if !madar_dawam::geofence::in_range(lat, lng) {
        return Err(AppError::BadRequest("Coordinates are out of range".into()));
    }

    let distance = haversine_meters(
        LatLng {
            lat: b_lat,
            lng: b_lng,
        },
        LatLng { lat, lng },
    );
    // The radius and the inside test are madar-shared's (`madar_dawam::geofence`),
    // the staff app's fence too (DW2: an unset radius is 200 m, 0 is 0 m).
    let radius_m = madar_dawam::geofence::effective_radius(branch.geo_radius_meters.map(i64::from));
    let radius = radius_m as f64;
    if require && !madar_dawam::geofence::inside(distance, radius_m) {
        // Coded with its figures so the app words it in the person's language
        // (AT-13, CL-2).
        return Err(AppError::CodedVars {
            status: 403,
            code: "OUTSIDE_FENCE",
            reason: format!(
                "You are {distance:.0} m from the branch — you must be within {radius:.0} m to clock in"
            ),
            vars: serde_json::json!({
                "distance_m": distance.round() as i64, "radius_m": radius.round() as i64
            }),
        });
    }
    Ok(Some(distance))
}

// ── Self-service ──────────────────────────────────────────────

/// An active employee's org. Anything else is a 403: a suspended or
/// terminated employee must not be able to clock in (the till punch; the
/// staff app's own session is checked on every request by `StaffAuth`).
pub(crate) async fn require_active_employee(
    pool: &PgPool,
    employee_id: Uuid,
) -> Result<Uuid, AppError> {
    let row: Option<(Uuid, String)> =
        sqlx::query_as("SELECT org_id, employment_status FROM employees WHERE id = $1")
            .bind(employee_id)
            .fetch_optional(pool)
            .await?;
    match row {
        Some((org_id, status)) if status == "active" => Ok(org_id),
        Some((_, status)) => Err(AppError::CodedVars {
            status: 403,
            code: "EMPLOYMENT_NOT_ACTIVE",
            reason: format!("Your employment is {status} — contact your manager"),
            vars: serde_json::json!({ "status": status }),
        }),
        None => Err(AppError::CodedVars {
            status: 403,
            code: "NOT_AN_EMPLOYEE",
            reason: "You're not an employee here — ask your manager to add you".into(),
            vars: serde_json::json!({}),
        }),
    }
}

/// Today's calendar date in a given timezone, decided by Postgres so the tz
/// database owns DST rather than the server process.
/// Nobody clocks in before the business has saved its rules (RU-1): the
/// lateness ladder and the absence cost must exist before anything is priced.
pub(crate) async fn require_rules(pool: &PgPool, org_id: Uuid) -> Result<(), AppError> {
    let saved: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM attendance_settings \
          WHERE org_id = $1 AND branch_id IS NULL AND rules_saved_at IS NOT NULL)",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await?;
    if saved {
        return Ok(());
    }
    Err(AppError::Coded {
        status: 409,
        code: "RULES_NOT_SET",
        reason:
            "Your business hasn't set its attendance rules yet — ask the owner to finish set-up."
                .into(),
    })
}

/// The calendar date `at` falls on in `timezone` (AT-1).
pub(crate) async fn day_in(
    pool: &PgPool,
    at: DateTime<Utc>,
    timezone: &str,
) -> Result<NaiveDate, AppError> {
    Ok(
        sqlx::query_scalar::<_, NaiveDate>("SELECT ($1::timestamptz AT TIME ZONE $2)::date")
            .bind(at)
            .bind(timezone)
            .fetch_one(pool)
            .await?,
    )
}

pub(crate) async fn today_in(pool: &PgPool, timezone: &str) -> Result<NaiveDate, AppError> {
    Ok(
        sqlx::query_scalar::<_, NaiveDate>("SELECT (now() AT TIME ZONE $1)::date")
            .bind(timezone)
            .fetch_one(pool)
            .await?,
    )
}

/// What the day's approved requests forgive. Thin wrapper over
/// [`crate::staff::requests::day_adjustments`] with the branch's rules (the
/// excused-time pay default and how half-day leave counts).
pub(crate) async fn adjustments_for(
    pool: &PgPool,
    settings: &AttendanceSettings,
    employee_id: Uuid,
    date: NaiveDate,
    timezone: &str,
) -> Result<DayAdjustments, AppError> {
    crate::staff::requests::day_adjustments(pool, employee_id, date, timezone, settings).await
}

/// Resolve the shift a punch at `now` belongs to, looking at both today's and
/// yesterday's roster so a night shift's after-midnight arrival stays on the day
/// the shift started. Returns the shift and the business date it belongs to.
/// Every punch path uses it: the app, a manager's and the till's (SC-10).
pub(crate) async fn resolve_punch_shift(
    pool: &PgPool,
    employee_id: Uuid,
    today: NaiveDate,
    timezone: &str,
    now: DateTime<Utc>,
) -> Result<(Option<ResolvedShift>, NaiveDate), AppError> {
    // The one "which shift is this" resolver, shared with the manager and till
    // punches and covers (SC-10).
    crate::staff::schedules::shift_at_instant(pool, employee_id, today, timezone, now).await
}

#[utoipa::path(
    post, path = "/staff/me/check-in", tag = "staff",
    request_body = CheckInRequest,
    responses(
        (status = 201, description = "Checked in", body = AttendanceRecord),
        (status = 409, description = "Already checked in for this shift"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn check_in(
    me: Me,
    pool: crate::db::Db,
    secret: web::Data<crate::auth::jwt::JwtSecret>,
    body: web::Json<CheckInRequest>,
) -> Result<HttpResponse, AppError> {
    // Only from the employee's live phone (CL-1): `Me` is a staff-app session
    // whose device, employee, org and module `StaffAuth` checked just now.
    let employee_id = me.employee_id;
    let org_id = me.org_id;
    require_rules(pool.get_ref(), org_id).await?;
    // Location only after the notice was accepted on this phone (AT-5).
    crate::staff::dawam::privacy::require_accepted(pool.get_ref(), &me).await?;

    let branch_org = crate::staff::resolve_branch_org(pool.get_ref(), body.branch_id).await?;
    if branch_org != org_id {
        return Err(AppError::CodedVars {
            status: 403,
            code: "BRANCH_OTHER_ORG",
            reason: "That branch belongs to a different organization".into(),
            vars: serde_json::json!({}),
        });
    }
    let mine = crate::staff::access::branches_of(pool.get_ref(), employee_id).await?;
    if !mine.contains(&body.branch_id) {
        return Err(AppError::CodedVars {
            status: 403,
            code: "NOT_YOUR_BRANCH",
            reason: "You don't work at that branch — ask your manager to add you to it.".into(),
            vars: serde_json::json!({}),
        });
    }

    let settings = load_settings(pool.get_ref(), org_id, Some(body.branch_id)).await?;
    // The app's punch is always fenced (CL-2): the org's `require_geofence`
    // switch no longer reaches the phone.
    let distance = check_geofence(
        pool.get_ref(),
        body.branch_id,
        body.latitude,
        body.longitude,
        true,
    )
    .await?;

    let tz = branch_timezone(pool.get_ref(), body.branch_id).await?;
    let stamped = crate::staff::dawam::clock::rebuild(
        body.offline.as_ref(),
        Utc::now(),
        me.verifier(&secret),
    )?;
    let now = stamped.at;
    let today = day_in(pool.get_ref(), now, &tz).await?;
    let (shift, business_date) =
        resolve_punch_shift(pool.get_ref(), employee_id, today, &tz, now).await?;

    check_window(shift.as_ref(), now)?;

    let adjustments =
        adjustments_for(pool.get_ref(), &settings, employee_id, business_date, &tz).await?;
    let derived = derive(
        Some(now),
        None,
        shift.as_ref().map(|s| s.scheduled_start_at),
        shift.as_ref().map(|s| s.scheduled_end_at),
        shift.as_ref(),
        &adjustments,
    );

    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO attendance_records (
            org_id, employee_id, branch_id, work_shift_id, business_date, status,
            scheduled_start_at, scheduled_end_at,
            check_in_at, check_in_latitude, check_in_longitude,
            check_in_distance_meters, check_in_method,
            late_minutes, created_by
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $14, $13, $15)
        ON CONFLICT (employee_id, business_date,
                     COALESCE(work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid)) WHERE covered_employee_id IS NULL
        DO NOTHING
        RETURNING id
        "#,
    )
    .bind(org_id)
    .bind(employee_id)
    .bind(body.branch_id)
    .bind(shift.as_ref().map(|s| s.work_shift_id))
    .bind(business_date)
    .bind(derived.status.as_str())
    .bind(shift.as_ref().map(|s| s.scheduled_start_at))
    .bind(shift.as_ref().map(|s| s.scheduled_end_at))
    .bind(now)
    .bind(body.latitude)
    .bind(body.longitude)
    .bind(distance)
    .bind(derived.late_minutes as i32)
    .bind(if stamped.offline { "offline" } else { "mobile_gps" })
    .bind(me.user_id)
    .fetch_optional(pool.get_ref())
    .await?;

    let Some(id) = inserted else {
        return Err(AppError::CodedVars {
            status: 409,
            code: "ALREADY_CHECKED_IN",
            reason: "You have already checked in for this shift".into(),
            vars: serde_json::json!({}),
        });
    };
    if stamped.unverified {
        crate::staff::dawam::presence::raise_flag(
            pool.get_ref(),
            org_id,
            employee_id,
            Some(body.branch_id),
            Some(id),
            "time_unverified",
            0,
        )
        .await?;
    }
    if body.tracking_off == Some(true) {
        crate::staff::dawam::presence::mark_tracking_off(
            pool.get_ref(),
            org_id,
            employee_id,
            body.branch_id,
            id,
        )
        .await?;
    }
    // The punch's own fix is checked too (CL-8/9): with tracking off there
    // are no pings to catch a mocked location.
    crate::staff::dawam::presence::check_punch_fix(
        pool.get_ref(),
        org_id,
        employee_id,
        body.branch_id,
        id,
        body.accuracy_meters,
        body.is_mock,
    )
    .await?;
    let record = load_record(pool.get_ref(), org_id, id).await?;
    Ok(HttpResponse::Created().json(record))
}

/// A check-in before the shift's window opens, or after it ended, is refused
/// (CL-3) — for every way of punching: the app, a manager's, the till's.
/// An unrostered day has no window: the punch opens a record with nothing to
/// be late for (a deliberate choice, see the clocking report).
pub(crate) fn check_window(
    shift: Option<&ResolvedShift>,
    now: DateTime<Utc>,
) -> Result<(), AppError> {
    let Some(s) = shift else {
        return Ok(());
    };
    // Arriving before the shift's check-in window is a mistake, not a punch —
    // otherwise an early bird opens the record that the real shift needs.
    let opens_at =
        s.scheduled_start_at - chrono::Duration::minutes(s.checkin_window_minutes.max(0) as i64);
    if now < opens_at {
        return Err(AppError::CodedVars {
            status: 400,
            code: "CHECKIN_TOO_EARLY",
            reason: format!(
                "Too early — check-in for {} opens {} minutes before it starts",
                s.name, s.checkin_window_minutes
            ),
            vars: serde_json::json!({
                "shift": s.name, "minutes": s.checkin_window_minutes, "opens_at": opens_at
            }),
        });
    }
    if now >= s.scheduled_end_at {
        return Err(AppError::CodedVars {
            status: 400,
            code: "SHIFT_ENDED",
            reason: format!("{} has already ended", s.name),
            vars: serde_json::json!({ "shift": s.name }),
        });
    }
    Ok(())
}

#[utoipa::path(
    post, path = "/staff/me/check-out", tag = "staff",
    request_body = CheckOutRequest,
    responses(
        (status = 200, description = "Checked out", body = AttendanceRecord),
        (status = 404, description = "No open check-in"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn check_out(
    me: Me,
    pool: crate::db::Db,
    secret: web::Data<crate::auth::jwt::JwtSecret>,
    body: web::Json<CheckOutRequest>,
) -> Result<HttpResponse, AppError> {
    let employee_id = me.employee_id;
    let org_id = me.org_id;
    crate::staff::dawam::privacy::require_accepted(pool.get_ref(), &me).await?;
    let stamped = crate::staff::dawam::clock::rebuild(
        body.offline.as_ref(),
        Utc::now(),
        me.verifier(&secret),
    )?;

    #[derive(sqlx::FromRow)]
    struct Open {
        id: Uuid,
        branch_id: Uuid,
        business_date: NaiveDate,
        work_shift_id: Option<Uuid>,
        check_in_at: Option<DateTime<Utc>>,
        scheduled_start_at: Option<DateTime<Utc>>,
        scheduled_end_at: Option<DateTime<Utc>>,
    }

    let mut open: Option<Open> = sqlx::query_as(
        "SELECT id, branch_id, business_date, work_shift_id, check_in_at, \
                scheduled_start_at, scheduled_end_at \
           FROM attendance_records \
          WHERE employee_id = $1 AND check_in_at IS NOT NULL AND check_out_at IS NULL \
          ORDER BY check_in_at DESC LIMIT 1",
    )
    .bind(employee_id)
    .fetch_optional(pool.get_ref())
    .await?;
    if open.is_none() && stamped.offline {
        // A check-out queued offline while the sweep auto-closed the shift
        // (CL-15): the real time the phone recorded replaces the automatic
        // one, so it is not lost as a refused op.
        open = sqlx::query_as(
            "SELECT id, branch_id, business_date, work_shift_id, check_in_at, \
                    scheduled_start_at, scheduled_end_at \
               FROM attendance_records \
              WHERE employee_id = $1 AND check_out_method = 'auto' \
                AND check_in_at IS NOT NULL AND check_in_at <= $2 \
                AND $2 < check_in_at + INTERVAL '24 hours' \
              ORDER BY check_in_at DESC LIMIT 1",
        )
        .bind(employee_id)
        .bind(stamped.at)
        .fetch_optional(pool.get_ref())
        .await?;
    }
    let open = open.ok_or_else(|| AppError::NotFound("You are not checked in".into()))?;

    let settings = load_settings(pool.get_ref(), org_id, Some(open.branch_id)).await?;
    let distance = check_geofence(
        pool.get_ref(),
        open.branch_id,
        body.latitude,
        body.longitude,
        true,
    )
    .await?;

    let tz = branch_timezone(pool.get_ref(), open.branch_id).await?;
    // A queued check-out can't close before the check-in it follows.
    let now = open.check_in_at.map_or(stamped.at, |i| stamped.at.max(i));
    let shift = load_shift_snapshot(
        pool.get_ref(),
        employee_id,
        &open.work_shift_id,
        open.business_date,
        &tz,
    )
    .await?
    .map(|mut s| {
        // Judge against the window the record was OPENED with, not whatever
        // the shift says today — editing a shift must not retro-move a
        // historical checkout.
        if let Some(start) = open.scheduled_start_at {
            s.scheduled_start_at = start;
        }
        if let Some(end) = open.scheduled_end_at {
            s.scheduled_end_at = end;
        }
        s
    });
    let adjustments = adjustments_for(
        pool.get_ref(),
        &settings,
        employee_id,
        open.business_date,
        &tz,
    )
    .await?;

    let derived = derive(
        open.check_in_at,
        Some(now),
        open.scheduled_start_at,
        open.scheduled_end_at,
        shift.as_ref(),
        &adjustments,
    );

    sqlx::query(
        "UPDATE attendance_records SET \
            check_out_at = $2, check_out_latitude = $3, check_out_longitude = $4, \
            check_out_distance_meters = $5, check_out_method = $11, \
            status = $6, late_minutes = $7, early_leave_minutes = $8, \
            overtime_minutes = $9, worked_minutes = $10, updated_at = now() \
          WHERE id = $1",
    )
    .bind(open.id)
    .bind(now)
    .bind(body.latitude)
    .bind(body.longitude)
    .bind(distance)
    .bind(derived.status.as_str())
    .bind(derived.late_minutes as i32)
    .bind(derived.early_leave_minutes as i32)
    .bind(derived.overtime_minutes as i32)
    .bind(derived.worked_minutes as i32)
    .bind(if stamped.offline {
        "offline"
    } else {
        "mobile_gps"
    })
    .execute(pool.get_ref())
    .await?;
    settle_cover(pool.get_ref(), open.id).await?;
    if stamped.unverified {
        crate::staff::dawam::presence::raise_flag(
            pool.get_ref(),
            org_id,
            employee_id,
            Some(open.branch_id),
            Some(open.id),
            "time_unverified",
            0,
        )
        .await?;
    }
    crate::staff::dawam::presence::check_punch_fix(
        pool.get_ref(),
        org_id,
        employee_id,
        open.branch_id,
        open.id,
        body.accuracy_meters,
        body.is_mock,
    )
    .await?;

    // The shift just closed, so price it now — a manager should see the penalty
    // immediately, not the next morning after the sweep.
    crate::staff::penalties::recompute_record(pool.get_ref(), open.id, &settings).await?;
    // Overtime: off, paid automatically, or waiting for a manager (RU-7).
    crate::staff::dawam::presence::after_check_out(pool.get_ref(), org_id, open.id, &settings)
        .await?;

    let record = load_record(pool.get_ref(), org_id, open.id).await?;
    Ok(HttpResponse::Ok().json(record))
}

/// A cover is time worked for a colleague, counted as a cover (CV-3, CV-7):
/// the coverer is never late or leaving early against the colleague's shift,
/// and a short cover is not a half day. Run after every write that derives a
/// record's figures (a no-op for anything but a cover); a status set by hand
/// sticks (AT-7). E2E B-TEAM-5.
pub(crate) async fn settle_cover(pool: &PgPool, record_id: Uuid) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE attendance_records SET late_minutes = 0, early_leave_minutes = 0, \
                status = CASE WHEN status_overridden THEN status ELSE 'present' END \
          WHERE id = $1 AND covered_employee_id IS NOT NULL",
    )
    .bind(record_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Re-materialise a work shift's window on a given business date. Used by
/// checkout and correction, where the record already names its shift.
pub(crate) async fn load_shift_snapshot(
    pool: &PgPool,
    employee_id: Uuid,
    work_shift_id: &Option<Uuid>,
    business_date: NaiveDate,
    timezone: &str,
) -> Result<Option<ResolvedShift>, AppError> {
    let Some(shift_id) = work_shift_id else {
        return Ok(None);
    };
    // The assignment's EFFECTIVE window when the person is rostered on it that
    // day (its own from/to, else the block's weekday time, else the default).
    if let Some(s) = resolve_shifts_for(pool, employee_id, business_date, timezone)
        .await?
        .into_iter()
        .find(|s| s.work_shift_id == *shift_id)
    {
        return Ok(Some(s));
    }
    // Not rostered on it (a manual record for any shift): the block's own
    // times that weekday.
    Ok(sqlx::query_as::<_, ResolvedShift>(
        r#"
        SELECT ws.id AS work_shift_id, ws.name, ws.grace_minutes, ws.break_minutes,
               ws.paid_break, ws.half_day_threshold_minutes,
               ws.overtime_threshold_minutes, ws.overtime_multiplier,
               ws.checkin_window_minutes,
               ($2::date + COALESCE(dt.start_time, ws.start_time)) AT TIME ZONE $3
                   AS scheduled_start_at,
               ($2::date + COALESCE(dt.end_time, ws.end_time)
                    + CASE WHEN COALESCE(dt.end_time, ws.end_time)
                                <= COALESCE(dt.start_time, ws.start_time)
                           THEN INTERVAL '1 day' ELSE INTERVAL '0 day' END
               ) AT TIME ZONE $3 AS scheduled_end_at,
               $4::uuid AS employee_id, $2::date AS on_date, ws.branch_id,
               COALESCE(dt.start_time, ws.start_time) AS start_time,
               COALESCE(dt.end_time, ws.end_time) AS end_time,
               COALESCE(dt.end_time, ws.end_time) <= COALESCE(dt.start_time, ws.start_time)
                   AS crosses_midnight,
               false AS times_edited, false AS from_override
          FROM work_shifts ws
          LEFT JOIN work_shift_day_times dt
                 ON dt.work_shift_id = ws.id
                AND dt.day_of_week = EXTRACT(DOW FROM $2::date)::smallint
         WHERE ws.id = $1
        "#,
    )
    .bind(shift_id)
    .bind(business_date)
    .bind(timezone)
    .bind(employee_id)
    .fetch_optional(pool)
    .await?)
}

#[utoipa::path(
    get, path = "/staff/me/today", tag = "staff",
    responses((status = 200, description = "The employee's own status right now", body = MyAttendanceToday), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_today(me: Me, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let employee_id = me.employee_id;
    let org_id = me.org_id;

    let tz =
        crate::staff::schedules::employee_timezone(pool.get_ref(), org_id, employee_id).await?;
    let today = today_in(pool.get_ref(), &tz).await?;
    let scheduled = resolve_shifts_for(pool.get_ref(), employee_id, today, &tz).await?;

    let records = sqlx::query_as::<_, AttendanceRecord>(&format!(
        "SELECT {RECORD_COLS} {RECORD_JOINS} \
          WHERE a.employee_id = $1 AND a.business_date = $2 ORDER BY a.check_in_at NULLS LAST"
    ))
    .bind(employee_id)
    .bind(today)
    .fetch_all(pool.get_ref())
    .await?;

    // The open record may belong to YESTERDAY's business date on a night shift,
    // so it is looked up independently of today's rows.
    let open_record = sqlx::query_as::<_, AttendanceRecord>(&format!(
        "SELECT {RECORD_COLS} {RECORD_JOINS} \
          WHERE a.employee_id = $1 AND a.check_in_at IS NOT NULL AND a.check_out_at IS NULL \
          ORDER BY a.check_in_at DESC LIMIT 1"
    ))
    .bind(employee_id)
    .fetch_optional(pool.get_ref())
    .await?;

    let open_id = open_record.as_ref().map(|r| r.id);
    let closed_records: Vec<AttendanceRecord> = records
        .into_iter()
        .filter(|r| Some(r.id) != open_id)
        .collect();

    let can_check_out = open_record.is_some();
    let branch_id = resolve_my_branch(
        pool.get_ref(),
        org_id,
        employee_id,
        open_record.as_ref(),
        &scheduled,
    )
    .await?;

    let blocked_reason = if can_check_out {
        Some("You are already checked in".to_string())
    } else if branch_id.is_none() {
        Some(
            "We can't tell which branch you work at — ask your manager to assign you to one."
                .to_string(),
        )
    } else {
        None
    };

    let branch_name: Option<String> = match branch_id {
        Some(id) => {
            sqlx::query_scalar("SELECT name FROM branches WHERE id = $1")
                .bind(id)
                .fetch_optional(pool.get_ref())
                .await?
        }
        None => None,
    };

    Ok(HttpResponse::Ok().json(MyAttendanceToday {
        business_date: today,
        open_record,
        closed_records,
        scheduled,
        can_check_in: !can_check_out && branch_id.is_some(),
        can_check_out,
        blocked_reason,
        branch_id,
        branch_name,
        timezone: Some(tz),
    }))
}

/// Where this employee clocks in today, in order of confidence:
///
/// 1. the branch of the record they are currently clocked into (they are
///    physically there);
/// 2. the branch their rostered shift belongs to;
/// 3. their branch assignment, when they have exactly one.
///
/// `None` when none of those settle it — someone assigned to several branches
/// with no branch-specific shift today. The app then says so rather than
/// guessing, because guessing wrong means clocking in somewhere they aren't.
async fn resolve_my_branch(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
    open_record: Option<&AttendanceRecord>,
    scheduled: &[ResolvedShift],
) -> Result<Option<Uuid>, AppError> {
    if let Some(record) = open_record {
        return Ok(Some(record.branch_id));
    }

    if let Some(shift) = scheduled.first() {
        let branch: Option<Uuid> =
            sqlx::query_scalar("SELECT branch_id FROM work_shifts WHERE id = $1")
                .bind(shift.work_shift_id)
                .fetch_optional(pool)
                .await?
                .flatten();
        if branch.is_some() {
            return Ok(branch);
        }
    }

    // A window count rather than MIN(): Postgres has no MIN aggregate for uuid.
    // The `n = 1` filter is the point — it yields a row only when the employee
    // has exactly ONE live branch, which is what "their branch" means.
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT branch_id FROM (
             SELECT eb.branch_id, COUNT(*) OVER () AS n
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
    .fetch_optional(pool)
    .await?)
}

#[utoipa::path(
    get, path = "/staff/me/attendance", tag = "staff",
    params(RangeQuery),
    responses((status = 200, description = "The employee's own attendance history", body = Vec<AttendanceRecord>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_attendance(
    me: Me,
    pool: crate::db::Db,
    query: web::Query<RangeQuery>,
) -> Result<HttpResponse, AppError> {
    validate_range(query.from, query.to, MAX_RANGE_DAYS)?;

    let rows = sqlx::query_as::<_, AttendanceRecord>(&format!(
        "SELECT {RECORD_COLS} {RECORD_JOINS} \
          WHERE a.employee_id = $1 AND a.business_date BETWEEN $2 AND $3 \
          ORDER BY a.business_date DESC, a.check_in_at DESC NULLS LAST"
    ))
    .bind(me.employee_id)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

// ── Admin surface ─────────────────────────────────────────────

pub(crate) async fn load_record(
    pool: &PgPool,
    org_id: Uuid,
    id: Uuid,
) -> Result<AttendanceRecord, AppError> {
    sqlx::query_as::<_, AttendanceRecord>(&format!(
        "SELECT {RECORD_COLS} {RECORD_JOINS} WHERE a.id = $1 AND a.org_id = $2"
    ))
    .bind(id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Attendance record not found".into()))
}

#[utoipa::path(
    get, path = "/staff/attendance", tag = "staff",
    params(AttendanceQuery),
    responses((status = 200, description = "Attendance records", body = Vec<AttendanceRecord>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_attendance(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<AttendanceQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    // A manager reads the records of their branches only (RO-6).
    let scope = access::scope_at(
        pool.get_ref(),
        &claims,
        org_id,
        Cap::HrAttendanceRead,
        query.branch_id,
    )
    .await?;
    validate_range(query.from, query.to, MAX_RANGE_DAYS)?;
    if let Some(status) = query.status.as_deref() {
        AttendanceStatus::parse(status)?;
    }

    let rows = sqlx::query_as::<_, AttendanceRecord>(&format!(
        "SELECT {RECORD_COLS} {RECORD_JOINS} \
          WHERE a.org_id = $1 \
            AND a.business_date BETWEEN $2 AND $3 \
            AND ($4::uuid[] IS NULL OR a.branch_id = ANY($4)) \
            AND ($5::uuid IS NULL OR a.employee_id = $5) \
            AND ($6::text IS NULL OR a.status = $6) \
          ORDER BY a.business_date DESC, lower(emp.name)"
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(scope.as_deref())
    .bind(query.employee_id)
    .bind(query.status.as_deref())
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[utoipa::path(
    get, path = "/staff/attendance/summary", tag = "staff",
    params(AttendanceQuery),
    responses((status = 200, description = "Per-employee totals over the window", body = Vec<AttendanceSummary>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn attendance_summary(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<AttendanceQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    let scope = access::scope_at(
        pool.get_ref(),
        &claims,
        org_id,
        Cap::HrAttendanceRead,
        query.branch_id,
    )
    .await?;
    validate_range(query.from, query.to, MAX_RANGE_DAYS)?;

    let rows = sqlx::query_as::<_, AttendanceSummary>(
        r#"
        SELECT a.employee_id, emp.name AS employee_name,
               COUNT(*) FILTER (WHERE a.status = 'present')  AS present_days,
               COUNT(*) FILTER (WHERE a.status = 'late')     AS late_days,
               COUNT(*) FILTER (WHERE a.status = 'absent')   AS absent_days,
               COUNT(*) FILTER (WHERE a.status = 'half_day') AS half_days,
               COUNT(*) FILTER (WHERE a.status = 'on_leave') AS leave_days,
               COALESCE(SUM(a.late_minutes), 0)::bigint     AS total_late_minutes,
               COALESCE(SUM(a.overtime_minutes), 0)::bigint AS total_overtime_minutes,
               COALESCE(SUM(a.worked_minutes), 0)::bigint   AS total_worked_minutes
          FROM attendance_records a
          JOIN employees emp ON emp.id = a.employee_id
         WHERE a.org_id = $1
           AND a.business_date BETWEEN $2 AND $3
           AND ($4::uuid[] IS NULL OR a.branch_id = ANY($4))
           AND ($5::uuid IS NULL OR a.employee_id = $5)
         GROUP BY a.employee_id, emp.name
         ORDER BY lower(emp.name)
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(scope.as_deref())
    .bind(query.employee_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

/// One person's state right now, for the manager's live team list.
#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct PresenceRow {
    pub employee_id: Uuid,
    pub employee_name: String,
    pub job_title: Option<String>,
    pub branch_name: Option<String>,
    /// `in` | `late` | `absent` | `on_leave` | `off` | `done`.
    pub state: String,
    pub check_in_at: Option<DateTime<Utc>>,
    pub check_out_at: Option<DateTime<Utc>>,
    pub late_minutes: i32,
    pub worked_minutes: i32,
    /// Minutes this person is rostered for today — the denominator of the
    /// labour-vs-plan bar.
    pub scheduled_minutes: i64,
}

/// The whole team's state right now, plus the day's labour against plan.
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct TeamPresence {
    /// The branch's business date, in ITS timezone — not the manager's device.
    pub business_date: NaiveDate,
    pub present: i64,
    pub late: i64,
    pub absent: i64,
    pub on_leave: i64,
    /// Minutes actually worked so far today across the team.
    pub worked_minutes: i64,
    /// Minutes rostered for today across the team.
    pub planned_minutes: i64,
    pub rows: Vec<PresenceRow>,
    /// The IANA timezone this payload's instants are shown in (see `crate::tz`).
    /// Additive; older clients ignore it.
    #[serde(default)]
    pub timezone: Option<String>,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct PresenceQuery {
    /// Omit for every branch in the org.
    #[serde(default)]
    pub branch_id: Option<Uuid>,
}

/// Who is in, late, absent or on leave right now.
///
/// Computed from TODAY'S attendance rows joined against the roster, so someone
/// rostered with no row yet is `absent` only once their shift has actually
/// started — before that they are simply `off`, not a red number on a manager's
/// dashboard at 6am.
#[utoipa::path(
    get, path = "/staff/team/presence", tag = "staff",
    params(PresenceQuery),
    responses((status = 200, description = "Live team state", body = TeamPresence), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn team_presence(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<PresenceQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    // A manager sees the people of their branches only (RO-6).
    let scope = access::scope_at(
        pool.get_ref(),
        &claims,
        org_id,
        Cap::HrAttendanceRead,
        query.branch_id,
    )
    .await?;

    // AT-1: each person's "today" is their branch's day, not the org's or the
    // database server's. The header's day and zone are the asked-for branch's,
    // else the org's.
    let tz = match query.branch_id {
        Some(b) => branch_timezone(pool.get_ref(), b).await?,
        None => crate::staff::schedules::org_timezone(pool.get_ref(), org_id).await?,
    };
    let today = today_in(pool.get_ref(), &tz).await?;

    let rows = sqlx::query_as::<_, PresenceRow>(
        r#"
        WITH people AS (
            -- Everyone active, at the branch they are looked at for: the one
            -- asked for, else their first (in scope) — and that branch's zone.
            SELECT p.id, p.org_id, p.name, p.job_title,
                   hb.name AS branch_name,
                   COALESCE(hb.timezone::text, o.timezone::text, 'Africa/Cairo') AS tz
              FROM employees p
              JOIN organizations o ON o.id = p.org_id
              LEFT JOIN LATERAL (
                  SELECT b.name, b.timezone
                    FROM employee_branches eb
                    JOIN branches b ON b.id = eb.branch_id
                                   AND b.deleted_at IS NULL
                                   AND b.org_id = p.org_id
                   WHERE eb.employee_id = p.id
                     AND ($2::uuid[] IS NULL OR eb.branch_id = ANY($2))
                   ORDER BY eb.assigned_at
                   LIMIT 1
              ) hb ON true
             WHERE p.org_id = $1 AND p.employment_status = 'active'
        ),
        local AS (
            SELECT pe.*, (now() AT TIME ZONE pe.tz)::date AS d FROM people pe
        ),
        roster AS (
            -- The minutes each is rostered for their today and when that
            -- shift was due to start.
            SELECT l.id AS employee_id,
                   l.name AS employee_name,
                   l.job_title,
                   l.branch_name,
                   l.d,
                   COALESCE(sh.minutes, 0)                        AS scheduled_minutes,
                   sh.due_at                                      AS due_at,
                   -- An approved leave or mission covers their today: the
                   -- sweep excuses the day on the same test (E2E B-TEAM-7).
                   EXISTS (
                       SELECT 1 FROM staff_requests sr
                        WHERE sr.employee_id = l.id AND sr.status = 'approved'
                          AND sr.kind IN ('leave', 'mission')
                          AND sr.on_date <= l.d
                          AND COALESCE(sr.end_date, sr.on_date) >= l.d
                   )                                              AS away
              FROM local l
              -- Their today from the one roster function (SC-6): date changes,
              -- split days, day-scoped blocks and their effective times.
              LEFT JOIN LATERAL (
                  SELECT SUM(EXTRACT(EPOCH FROM (r.end_at - r.start_at)) / 60)::bigint AS minutes,
                         MIN(r.start_at) AS due_at
                    FROM dawam_roster(ARRAY[l.id], l.d, l.d) r
              ) sh ON true
        ),
        today AS (
            -- Their day's record, or a night shift from yesterday still open.
            SELECT DISTINCT ON (a.employee_id)
                   a.employee_id, a.check_in_at, a.check_out_at, a.status,
                   a.late_minutes, a.worked_minutes, a.branch_id
              FROM attendance_records a
              JOIN roster r ON r.employee_id = a.employee_id
             WHERE a.org_id = $1
               AND (a.business_date = r.d
                    OR (a.business_date = r.d - 1 AND a.check_in_at IS NOT NULL
                        AND a.check_out_at IS NULL))
             ORDER BY a.employee_id, a.check_in_at DESC NULLS LAST
        )
        SELECT r.employee_id, r.employee_name, r.job_title, r.branch_name,
               COALESCE(t.check_in_at, NULL)  AS check_in_at,
               COALESCE(t.check_out_at, NULL) AS check_out_at,
               COALESCE(t.late_minutes, 0)    AS late_minutes,
               COALESCE(t.worked_minutes, 0)  AS worked_minutes,
               r.scheduled_minutes,
               CASE
                   WHEN t.status = 'on_leave'                       THEN 'on_leave'
                   WHEN t.check_in_at IS NOT NULL
                        AND t.check_out_at IS NULL
                        AND COALESCE(t.late_minutes, 0) > 0         THEN 'late'
                   WHEN t.check_in_at IS NOT NULL
                        AND t.check_out_at IS NULL                  THEN 'in'
                   WHEN t.check_out_at IS NOT NULL                  THEN 'done'
                   -- On an approved leave today and not clocked in: on leave
                   -- from the moment it is approved, not absent until the
                   -- sweep writes the day (DSH-1, APP-7).
                   WHEN r.away AND r.scheduled_minutes > 0          THEN 'on_leave'
                   -- Rostered, nothing recorded, and the shift is already due:
                   -- that is an absence. Before it is due, they are just off.
                   WHEN r.scheduled_minutes > 0 AND r.due_at <= now() THEN 'absent'
                   ELSE 'off'
               END AS state
          FROM roster r
          LEFT JOIN today t ON t.employee_id = r.employee_id
         WHERE ($2::uuid[] IS NULL
                OR t.branch_id = ANY($2)
                OR EXISTS (SELECT 1 FROM employee_branches eb
                            WHERE eb.employee_id = r.employee_id AND eb.branch_id = ANY($2)))
         ORDER BY lower(r.employee_name)
        "#,
    )
    .bind(org_id)
    .bind(scope.as_deref())
    .fetch_all(pool.get_ref())
    .await?;

    let count = |state: &str| rows.iter().filter(|r| r.state == state).count() as i64;
    let body = TeamPresence {
        business_date: today,
        // `done` counts as present: someone who finished their shift was here.
        present: count("in") + count("done"),
        late: count("late"),
        absent: count("absent"),
        on_leave: count("on_leave"),
        worked_minutes: rows.iter().map(|r| r.worked_minutes as i64).sum(),
        planned_minutes: rows.iter().map(|r| r.scheduled_minutes).sum(),
        rows,
        timezone: Some(tz.clone()),
    };
    Ok(HttpResponse::Ok().json(body))
}

#[utoipa::path(
    post, path = "/staff/attendance", tag = "staff",
    request_body = ManualRecordRequest,
    responses(
        (status = 201, description = "Manual record created", body = AttendanceRecord),
        (status = 409, description = "A record already exists for that day and shift"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn create_manual_record(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<ManualRecordRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::gate(pool.get_ref(), &claims, org_id, Cap::HrAttendanceCreate).await?;
    // At the branch the day is recorded at (RO-6).
    access::require_at(
        pool.get_ref(),
        &claims,
        org_id,
        Cap::HrAttendanceCreate,
        body.branch_id,
    )
    .await?;
    require_employee_in_org(pool.get_ref(), org_id, body.employee_id).await?;
    // AT-7: after the month's payroll is approved, the fix goes into the next
    // month. The one closed-month check (`period_lock`, 409 `PERIOD_CLOSED`).
    crate::staff::period_lock::assert_open(
        pool.get_ref(),
        org_id,
        body.business_date,
        "an attendance record",
    )
    .await?;

    let reason = body.reason.trim();
    if reason.is_empty() {
        return Err(AppError::BadRequest(
            "A manual attendance record needs a reason".into(),
        ));
    }
    if let (Some(in_at), Some(out_at)) = (body.check_in_at, body.check_out_at)
        && out_at < in_at
    {
        return Err(AppError::BadRequest("Check-out is before check-in".into()));
    }
    if body.check_out_at.is_some() && body.check_in_at.is_none() {
        return Err(AppError::BadRequest("A check-out needs a check-in".into()));
    }
    let branch_org = crate::staff::resolve_branch_org(pool.get_ref(), body.branch_id).await?;
    if branch_org != org_id {
        return Err(AppError::Forbidden(
            "That branch belongs to a different organization".into(),
        ));
    }

    let tz = branch_timezone(pool.get_ref(), body.branch_id).await?;
    let shift = load_shift_snapshot(
        pool.get_ref(),
        body.employee_id,
        &body.work_shift_id,
        body.business_date,
        &tz,
    )
    .await?;
    let settings = load_settings(pool.get_ref(), org_id, Some(body.branch_id)).await?;
    let adjustments = adjustments_for(
        pool.get_ref(),
        &settings,
        body.employee_id,
        body.business_date,
        &tz,
    )
    .await?;

    let derived = derive(
        body.check_in_at,
        body.check_out_at,
        shift.as_ref().map(|s| s.scheduled_start_at),
        shift.as_ref().map(|s| s.scheduled_end_at),
        shift.as_ref(),
        &adjustments,
    );
    // An explicit status is the ONLY way to record an absence or a leave day,
    // neither of which has stamps to derive anything from.
    let status = match body.status.as_deref() {
        Some(s) => AttendanceStatus::parse(s)?,
        None => derived.status,
    };

    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO attendance_records (
            org_id, employee_id, branch_id, work_shift_id, business_date, status,
            scheduled_start_at, scheduled_end_at,
            check_in_at, check_in_method, check_out_at, check_out_method,
            late_minutes, early_leave_minutes, overtime_minutes, worked_minutes,
            is_manual, notes, edit_reason, created_by, edited_by, status_overridden
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8,
            $9, CASE WHEN $9::timestamptz IS NULL THEN NULL ELSE 'manual' END,
            $10, CASE WHEN $10::timestamptz IS NULL THEN NULL ELSE 'manual' END,
            $11, $12, $13, $14, TRUE, $15, $16, $17, $17, $18
        )
        ON CONFLICT (employee_id, business_date,
                     COALESCE(work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid)) WHERE covered_employee_id IS NULL
        DO NOTHING
        RETURNING id
        "#,
    )
    .bind(org_id)
    .bind(body.employee_id)
    .bind(body.branch_id)
    .bind(body.work_shift_id)
    .bind(body.business_date)
    .bind(status.as_str())
    .bind(shift.as_ref().map(|s| s.scheduled_start_at))
    .bind(shift.as_ref().map(|s| s.scheduled_end_at))
    .bind(body.check_in_at)
    .bind(body.check_out_at)
    .bind(derived.late_minutes as i32)
    .bind(derived.early_leave_minutes as i32)
    .bind(derived.overtime_minutes as i32)
    .bind(derived.worked_minutes as i32)
    .bind(
        body.notes
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty()),
    )
    .bind(reason)
    .bind(claims.user_id_safe().ok())
    // A status set by hand sticks (AT-7).
    .bind(body.status.is_some())
    .fetch_optional(pool.get_ref())
    .await?;

    let Some(id) = inserted else {
        return Err(AppError::Conflict(
            "This employee already has a record for that day and shift — correct it instead".into(),
        ));
    };

    crate::staff::penalties::recompute_record(pool.get_ref(), id, &settings).await?;
    // Its overtime waits for a manager like a phone's (RU-7, E2E B-TEAM-6).
    crate::staff::dawam::presence::after_check_out(pool.get_ref(), org_id, id, &settings).await?;

    let record = load_record(pool.get_ref(), org_id, id).await?;
    Ok(HttpResponse::Created().json(record))
}

#[utoipa::path(
    patch, path = "/staff/attendance/{id}", tag = "staff",
    params(("id" = Uuid, Path, description = "Attendance record ID")),
    request_body = CorrectRecordRequest,
    responses((status = 200, description = "Record corrected", body = AttendanceRecord), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn correct_record(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<CorrectRecordRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::gate(pool.get_ref(), &claims, org_id, Cap::HrAttendanceEdit).await?;
    let existing = load_record(pool.get_ref(), org_id, *id).await?;
    access::require_at(
        pool.get_ref(),
        &claims,
        org_id,
        Cap::HrAttendanceEdit,
        existing.branch_id,
    )
    .await?;

    let reason = body.reason.trim();
    if reason.is_empty() {
        return Err(AppError::BadRequest("A correction needs a reason".into()));
    }
    // AT-7: after the month's payroll is approved, the fix goes into the next
    // month. The one closed-month check (`period_lock`, 409 `PERIOD_CLOSED`).
    crate::staff::period_lock::assert_open(
        pool.get_ref(),
        org_id,
        existing.business_date,
        "a correction",
    )
    .await?;

    // A punch the manager moves is recorded as a correction (CL-16).
    apply_punch_correction_as(
        pool.get_ref(),
        org_id,
        *id,
        body.check_in_at,
        body.check_out_at,
        body.status.as_deref(),
        body.notes.as_deref(),
        reason,
        claims.user_id_safe().ok(),
        Some("correction"),
    )
    .await?;

    let record = load_record(pool.get_ref(), org_id, *id).await?;
    Ok(HttpResponse::Ok().json(record))
}

/// Rewrite a record's punches and reprice the day.
///
/// Shared by the admin edit and by an APPROVED correction request, so a
/// manager approving "I forgot to clock out at 17:00" produces exactly the
/// record a manual edit would — same derivation, same repricing, same audit
/// columns. Idempotent: applying the same values twice writes the same row.
///
/// `status_override`: a status the person sets by hand. It sticks (AT-7):
/// later automatic re-derives keep it, until someone sets `"derived"` to hand
/// the day back to the rules. It keeps each punch's method;
/// [`apply_punch_correction_as`] marks the punches it moves (CL-16).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_punch_correction(
    pool: &PgPool,
    org_id: Uuid,
    record_id: Uuid,
    check_in_at: Option<DateTime<Utc>>,
    check_out_at: Option<DateTime<Utc>>,
    status_override: Option<&str>,
    notes: Option<&str>,
    reason: &str,
    editor: Option<Uuid>,
) -> Result<(), AppError> {
    rederive(
        pool,
        org_id,
        record_id,
        check_in_at,
        check_out_at,
        status_override,
        Some((notes, reason, editor)),
        None,
    )
    .await
}

/// [`apply_punch_correction`], recording `method` (`correction`) on every
/// punch whose time it changes (CL-16). A punch it leaves alone keeps how it
/// was made.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_punch_correction_as(
    pool: &PgPool,
    org_id: Uuid,
    record_id: Uuid,
    check_in_at: Option<DateTime<Utc>>,
    check_out_at: Option<DateTime<Utc>>,
    status_override: Option<&str>,
    notes: Option<&str>,
    reason: &str,
    editor: Option<Uuid>,
    method: Option<&str>,
) -> Result<(), AppError> {
    rederive(
        pool,
        org_id,
        record_id,
        check_in_at,
        check_out_at,
        status_override,
        Some((notes, reason, editor)),
        method,
    )
    .await
}

/// Re-derive a record from its stored punches after something it depends on
/// changed (a request approved or cancelled, a holiday): automation, so it
/// keeps a manager's status and never touches the audit columns — the Legal
/// report's "who edited this and why" stays the person's (AT-7, AT-10, B10).
pub(crate) async fn reprice_record(
    pool: &PgPool,
    org_id: Uuid,
    record_id: Uuid,
) -> Result<(), AppError> {
    rederive(pool, org_id, record_id, None, None, None, None, None).await
}

#[allow(clippy::too_many_arguments)]
async fn rederive(
    pool: &PgPool,
    org_id: Uuid,
    record_id: Uuid,
    check_in_at: Option<DateTime<Utc>>,
    check_out_at: Option<DateTime<Utc>>,
    status_override: Option<&str>,
    human: Option<(Option<&str>, &str, Option<Uuid>)>,
    method: Option<&str>,
) -> Result<(), AppError> {
    let existing = load_record(pool, org_id, record_id).await?;
    let check_in_at = check_in_at.or(existing.check_in_at);
    let check_out_at = check_out_at.or(existing.check_out_at);
    if let (Some(in_at), Some(out_at)) = (check_in_at, check_out_at)
        && out_at < in_at
    {
        return Err(AppError::BadRequest("Check-out is before check-in".into()));
    }

    let tz = branch_timezone(pool, existing.branch_id).await?;
    let shift = load_shift_snapshot(
        pool,
        existing.employee_id,
        &existing.work_shift_id,
        existing.business_date,
        &tz,
    )
    .await?;
    let settings = load_settings(pool, org_id, Some(existing.branch_id)).await?;
    let adjustments = adjustments_for(
        pool,
        &settings,
        existing.employee_id,
        existing.business_date,
        &tz,
    )
    .await?;

    let derived = derive(
        check_in_at,
        check_out_at,
        existing.scheduled_start_at,
        existing.scheduled_end_at,
        shift.as_ref(),
        &adjustments,
    );
    // A cover's own status stays what the cover flow set.
    let (status, overridden) = match status_override {
        Some("derived") => (derived.status, false),
        Some(s) => (AttendanceStatus::parse(s)?, true),
        None if existing.status_overridden => (AttendanceStatus::parse(&existing.status)?, true),
        None => (derived.status, false),
    };
    let (notes, reason, editor, touched) = match human {
        Some((notes, reason, editor)) => (notes, Some(reason), editor, true),
        None => (None, None, None, false),
    };

    sqlx::query(
        "UPDATE attendance_records SET \
            check_in_method = CASE \
                WHEN $15::text IS NOT NULL AND $3::timestamptz IS NOT NULL \
                     AND $3::timestamptz IS DISTINCT FROM check_in_at THEN $15 \
                ELSE COALESCE(check_in_method, CASE WHEN $3::timestamptz IS NULL THEN NULL ELSE 'manual' END) END, \
            check_out_method = CASE \
                WHEN $15::text IS NOT NULL AND $4::timestamptz IS NOT NULL \
                     AND $4::timestamptz IS DISTINCT FROM check_out_at THEN $15 \
                ELSE COALESCE(check_out_method, CASE WHEN $4::timestamptz IS NULL THEN NULL ELSE 'manual' END) END, \
            check_in_at  = $3, \
            check_out_at = $4, \
            status = $5, late_minutes = $6, early_leave_minutes = $7, \
            overtime_minutes = $8, worked_minutes = $9, status_overridden = $13, \
            notes = COALESCE($10, notes), \
            edit_reason = CASE WHEN $14 THEN $11 ELSE edit_reason END, \
            edited_by = CASE WHEN $14 THEN $12 ELSE edited_by END, \
            updated_at = now() \
          WHERE id = $1 AND org_id = $2",
    )
    .bind(record_id)
    .bind(org_id)
    .bind(check_in_at)
    .bind(check_out_at)
    .bind(status.as_str())
    .bind(derived.late_minutes as i32)
    .bind(derived.early_leave_minutes as i32)
    .bind(derived.overtime_minutes as i32)
    .bind(derived.worked_minutes as i32)
    .bind(notes.map(str::trim).filter(|n| !n.is_empty()))
    .bind(reason)
    .bind(editor)
    .bind(overridden)
    .bind(touched)
    .bind(method)
    .execute(pool)
    .await?;

    settle_cover(pool, record_id).await?;
    // A correction changes what is owed. Recompute — but `penalties` leaves any
    // deduction a human has already waived or overridden exactly as it is.
    crate::staff::penalties::recompute_record(pool, record_id, &settings).await?;
    // Overtime a correction, a manager's punch or a repricing produced goes
    // off / paid / to approval like a phone's check-out (RU-7, E2E B-TEAM-6).
    // A decided overtime keeps its decision.
    crate::staff::dawam::presence::after_check_out(pool, org_id, record_id, &settings).await?;
    Ok(())
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct DeleteRecordQuery {
    /// Why the day goes (kept with the tombstone the absence sweep honours).
    #[serde(default)]
    pub reason: Option<String>,
}

#[utoipa::path(
    delete, path = "/staff/attendance/{id}", tag = "staff",
    params(("id" = Uuid, Path, description = "Attendance record ID"), DeleteRecordQuery),
    responses((status = 204, description = "Record deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_record(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    query: web::Query<DeleteRecordQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    access::gate(pool.get_ref(), &claims, org_id, Cap::HrAttendanceDelete).await?;
    let existing = load_record(pool.get_ref(), org_id, *id).await?;
    access::require_at(
        pool.get_ref(),
        &claims,
        org_id,
        Cap::HrAttendanceDelete,
        existing.branch_id,
    )
    .await?;
    // The one closed-month check (`period_lock`, 409 `PERIOD_CLOSED`).
    crate::staff::period_lock::assert_open(
        pool.get_ref(),
        org_id,
        existing.business_date,
        "this attendance record",
    )
    .await?;

    let mut tx = pool.begin().await?;
    // AT-7: the absence sweep never writes back a day a manager deleted.
    if existing.covered_employee_id.is_none() {
        sqlx::query(
            "INSERT INTO attendance_tombstones \
                 (org_id, employee_id, business_date, work_shift_id, deleted_by, reason) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (employee_id, business_date, \
                          COALESCE(work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid)) \
             DO UPDATE SET deleted_by = EXCLUDED.deleted_by, reason = EXCLUDED.reason, \
                           created_at = now()",
        )
        .bind(org_id)
        .bind(existing.employee_id)
        .bind(existing.business_date)
        .bind(existing.work_shift_id)
        .bind(claims.user_id_safe().ok())
        .bind(
            query
                .reason
                .as_deref()
                .map(str::trim)
                .filter(|r| !r.is_empty()),
        )
        .execute(&mut *tx)
        .await?;
    }
    let deleted = sqlx::query("DELETE FROM attendance_records WHERE id = $1 AND org_id = $2")
        .bind(*id)
        .bind(org_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(AppError::NotFound("Attendance record not found".into()));
    }
    tx.commit().await?;
    Ok(HttpResponse::NoContent().finish())
}
