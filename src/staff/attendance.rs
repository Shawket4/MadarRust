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
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::{
    errors::{AppError, AppErrorResponse},
    geo::osrm::{LatLng, haversine_meters},
    orgs::handlers::extract_claims,
    permissions::checker::check_permission,
    staff::{
        branch_timezone, require_user_in_org,
        rules::{self, AttendanceStatus, LateTier},
        schedules::{ResolvedShift, pick_shift_for_instant, resolve_shifts_for},
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
    pub user_id: Uuid,
    #[sqlx(default)]
    pub user_name: Option<String>,
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
}

/// Every attendance column plus the two denormalised names, in `AttendanceRecord`
/// field order. One constant so list, single, and returning queries cannot drift.
const RECORD_COLS: &str = r#"
    a.id, a.org_id, a.user_id, u.name AS user_name, a.branch_id, a.work_shift_id,
    ws.name AS work_shift_name, a.business_date, a.status,
    a.scheduled_start_at, a.scheduled_end_at,
    a.check_in_at, a.check_in_latitude, a.check_in_longitude,
    a.check_in_distance_meters, a.check_in_method,
    a.check_out_at, a.check_out_latitude, a.check_out_longitude,
    a.check_out_distance_meters, a.check_out_method,
    a.late_minutes, a.early_leave_minutes, a.overtime_minutes, a.worked_minutes,
    a.is_manual, a.notes, a.edit_reason, a.created_by, a.edited_by,
    a.created_at, a.updated_at
"#;

const RECORD_JOINS: &str = "FROM attendance_records a \
     JOIN users u ON u.id = a.user_id \
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
    pub weekend_days: Vec<i16>,
    pub working_days_per_month: Decimal,
    pub require_geofence: bool,
    /// Whether an approved mid-shift permission or early departure is PAID by
    /// default. The approver may override it on any individual request.
    pub excused_time_paid_default: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const SETTINGS_COLS: &str = "id, org_id, branch_id, late_deduction_tiers, absence_deduction_days, \
     default_overtime_multiplier, auto_checkout_buffer_minutes, weekend_days, \
     working_days_per_month, require_geofence, excused_time_paid_default, \
     created_at, updated_at";

impl AttendanceSettings {
    /// Parse the jsonb ladder. A malformed ladder is treated as "no penalties"
    /// rather than an error: payroll must still run for everyone else.
    pub(crate) fn tiers(&self) -> Vec<LateTier> {
        serde_json::from_value(self.late_deduction_tiers.clone()).unwrap_or_default()
    }
}

/// One employee's totals over a reporting window.
#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct AttendanceSummary {
    pub user_id: Uuid,
    pub user_name: String,
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
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct CheckOutRequest {
    #[serde(default)]
    pub latitude: Option<f64>,
    #[serde(default)]
    pub longitude: Option<f64>,
}

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct ManualRecordRequest {
    pub user_id: Uuid,
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
    pub weekend_days: Option<Vec<i16>>,
    #[serde(default)]
    pub working_days_per_month: Option<Decimal>,
    #[serde(default)]
    pub require_geofence: Option<bool>,
    #[serde(default)]
    pub excused_time_paid_default: Option<bool>,
}

#[derive(Deserialize, IntoParams, Debug)]
#[into_params(parameter_in = Query)]
pub struct AttendanceQuery {
    pub from: NaiveDate,
    pub to: NaiveDate,
    #[serde(default)]
    pub branch_id: Option<Uuid>,
    #[serde(default)]
    pub user_id: Option<Uuid>,
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
pub(crate) struct Derived {
    pub late_minutes: i64,
    pub early_leave_minutes: i64,
    pub overtime_minutes: i64,
    pub worked_minutes: i64,
    pub status: AttendanceStatus,
}

/// What the day's APPROVED requests forgive.
///
/// Resolved from `staff_requests` by [`crate::staff::requests::day_adjustments`]
/// and passed in, so this file's math stays a pure function of its arguments and
/// the rules remain testable without a database.
///
/// The three windows are the same shape seen from different ends — see the
/// `staff_requests` migration: a late arrival is a window open at the start, an
/// early departure one open at the end, an excuse one closed at both.
#[derive(Debug, Clone, Default)]
pub(crate) struct DayAdjustments {
    /// Approved `late_arrival` — the grace deadline moves to this instant.
    pub excused_until: Option<DateTime<Utc>>,
    /// Approved `early_departure` — leaving after this instant is not early.
    pub excused_from: Option<DateTime<Utc>>,
    /// Approved `excuse` windows: stepped out and came back.
    pub excused_windows: Vec<(DateTime<Utc>, DateTime<Utc>)>,
    /// Whether excused time counts as worked. Org default
    /// (`excused_time_paid_default`), overridable per request by the approver.
    pub excused_time_paid: bool,
    /// Approved leave or mission covers the whole day.
    pub on_leave: bool,
}

impl DayAdjustments {
    /// Minutes inside `[in, out]` that an approved excuse forgives.
    ///
    /// Clipped to the attendance window because an excuse that runs past
    /// check-out did not consume time the employee was being paid for anyway;
    /// crediting it would pay them for being absent twice over.
    fn excused_minutes(&self, check_in: DateTime<Utc>, check_out: DateTime<Utc>) -> i64 {
        self.excused_windows
            .iter()
            .map(|(from, to)| {
                let start = (*from).max(check_in);
                let end = (*to).min(check_out);
                (end - start).num_minutes().max(0)
            })
            .sum()
    }
}

pub(crate) fn derive(
    check_in_at: Option<DateTime<Utc>>,
    check_out_at: Option<DateTime<Utc>>,
    scheduled_start_at: Option<DateTime<Utc>>,
    scheduled_end_at: Option<DateTime<Utc>>,
    shift: Option<&ResolvedShift>,
    adjustments: &DayAdjustments,
) -> Derived {
    let rules_for = shift.map(|s| s.rules());
    let grace = rules_for.map(|r| r.grace_minutes).unwrap_or(0);

    let late = match (scheduled_start_at, check_in_at) {
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
    // the gap is already missing from the clocked span, which is the deduction.
    if adjustments.excused_time_paid
        && let (Some(in_at), Some(out_at)) = (check_in_at, check_out_at)
    {
        worked += adjustments.excused_minutes(in_at, out_at);
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
        // An approved early departure shortens the day the employee OWED, so the
        // half-day threshold shrinks with it — otherwise permission to leave at
        // noon would still be recorded as half a day.
        let span = shift.map(|s| s.span_minutes()).unwrap_or(0);
        let owed = match (adjustments.excused_from, scheduled_end_at) {
            (Some(from), Some(end)) => (span - (end - from).num_minutes().max(0)).max(0),
            _ => span,
        };
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

/// The effective settings for a branch: its own row, else the org-wide row, else
/// the built-in defaults. Never fails for want of configuration.
pub async fn load_settings(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Option<Uuid>,
) -> Result<AttendanceSettings, AppError> {
    let row = sqlx::query_as::<_, AttendanceSettings>(&format!(
        "SELECT {SETTINGS_COLS} FROM attendance_settings \
         WHERE org_id = $1 AND (branch_id = $2 OR branch_id IS NULL) \
         ORDER BY branch_id NULLS LAST LIMIT 1"
    ))
    .bind(org_id)
    .bind(branch_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.unwrap_or_else(|| AttendanceSettings {
        id: Uuid::nil(),
        org_id,
        branch_id: None,
        late_deduction_tiers: serde_json::json!([]),
        absence_deduction_days: Decimal::ONE,
        default_overtime_multiplier: Decimal::new(150, 2),
        auto_checkout_buffer_minutes: 120,
        weekend_days: vec![5, 6],
        working_days_per_month: Decimal::from(30),
        require_geofence: true,
        excused_time_paid_default: true,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }))
}

#[utoipa::path(
    get, path = "/staff/attendance/settings", tag = "staff",
    params(SettingsQuery),
    responses((status = 200, description = "Effective attendance settings", body = AttendanceSettings), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_attendance_settings(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<SettingsQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "attendance", "read").await?;
    let org_id = scope_org(&req, &claims)?;
    let settings = load_settings(pool.get_ref(), org_id, query.branch_id).await?;
    Ok(HttpResponse::Ok().json(settings))
}

#[utoipa::path(
    put, path = "/staff/attendance/settings", tag = "staff",
    request_body = PutAttendanceSettingsRequest,
    responses((status = 200, description = "Settings saved", body = AttendanceSettings), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_attendance_settings(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<PutAttendanceSettingsRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "attendance", "update").await?;
    let org_id = scope_org(&req, &claims)?;

    // A bad ladder must never reach payroll, so it is rejected at the door.
    if let Some(tiers) = body.late_deduction_tiers.as_deref() {
        rules::validate_tiers(tiers)?;
    }
    if let Some(days) = body.weekend_days.as_deref()
        && days.iter().any(|d| !(0..=6).contains(d))
    {
        return Err(AppError::BadRequest(
            "weekend_days must be 0 (Sunday) through 6 (Saturday)".into(),
        ));
    }
    if body
        .working_days_per_month
        .is_some_and(|d| d <= Decimal::ZERO)
    {
        return Err(AppError::BadRequest(
            "working_days_per_month must be positive".into(),
        ));
    }
    if body
        .default_overtime_multiplier
        .is_some_and(|m| m <= Decimal::ZERO)
    {
        return Err(AppError::BadRequest(
            "default_overtime_multiplier must be positive".into(),
        ));
    }
    if body.auto_checkout_buffer_minutes.is_some_and(|m| m < 0) {
        return Err(AppError::BadRequest(
            "auto_checkout_buffer_minutes cannot be negative".into(),
        ));
    }

    let tiers = body
        .late_deduction_tiers
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| AppError::BadRequest("Invalid late deduction tiers".into()))?;

    let row = sqlx::query_as::<_, AttendanceSettings>(&format!(
        r#"
        INSERT INTO attendance_settings (
            org_id, branch_id, late_deduction_tiers, absence_deduction_days,
            default_overtime_multiplier, auto_checkout_buffer_minutes, weekend_days,
            working_days_per_month, require_geofence, excused_time_paid_default
        ) VALUES (
            $1, $2, COALESCE($3, '[]'::jsonb), COALESCE($4, 1.00), COALESCE($5, 1.50),
            COALESCE($6, 120), COALESCE($7, ARRAY[5,6]::smallint[]),
            COALESCE($8, 30.00), COALESCE($9, TRUE), COALESCE($10, TRUE)
        )
        ON CONFLICT (org_id, COALESCE(branch_id, '00000000-0000-0000-0000-000000000000'::uuid))
        DO UPDATE SET
            late_deduction_tiers         = COALESCE($3, attendance_settings.late_deduction_tiers),
            absence_deduction_days       = COALESCE($4, attendance_settings.absence_deduction_days),
            default_overtime_multiplier  = COALESCE($5, attendance_settings.default_overtime_multiplier),
            auto_checkout_buffer_minutes = COALESCE($6, attendance_settings.auto_checkout_buffer_minutes),
            weekend_days                 = COALESCE($7, attendance_settings.weekend_days),
            working_days_per_month       = COALESCE($8, attendance_settings.working_days_per_month),
            require_geofence             = COALESCE($9, attendance_settings.require_geofence),
            excused_time_paid_default    = COALESCE($10, attendance_settings.excused_time_paid_default),
            updated_at                   = now()
        RETURNING {SETTINGS_COLS}
        "#
    ))
    .bind(org_id)
    .bind(body.branch_id)
    .bind(tiers)
    .bind(body.absence_deduction_days)
    .bind(body.default_overtime_multiplier)
    .bind(body.auto_checkout_buffer_minutes)
    .bind(body.weekend_days.as_deref())
    .bind(body.working_days_per_month)
    .bind(body.require_geofence)
    .bind(body.excused_time_paid_default)
    .fetch_one(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(row))
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
async fn check_geofence(
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
            return Err(AppError::BadRequest(
                "This branch has no coordinates set, so location cannot be verified. \
                 Set them on the branch, or turn off geofencing in attendance settings."
                    .into(),
            ));
        }
        return Ok(None);
    };

    let (Some(lat), Some(lng)) = (latitude, longitude) else {
        if require {
            return Err(AppError::BadRequest(
                "Location is required to check in at this branch".into(),
            ));
        }
        return Ok(None);
    };
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
        return Err(AppError::BadRequest("Coordinates are out of range".into()));
    }

    let distance = haversine_meters(
        LatLng {
            lat: b_lat,
            lng: b_lng,
        },
        LatLng { lat, lng },
    );
    let radius = branch.geo_radius_meters.unwrap_or(200).max(0) as f64;
    if require && distance > radius {
        return Err(AppError::Forbidden(format!(
            "You are {distance:.0} m from the branch — you must be within {radius:.0} m to clock in"
        )));
    }
    Ok(Some(distance))
}

// ── Self-service ──────────────────────────────────────────────

/// The caller's own live, active staff profile. Anything else is a 403: a
/// suspended or terminated employee must not be able to clock in.
pub(crate) async fn require_active_profile(pool: &PgPool, user_id: Uuid) -> Result<Uuid, AppError> {
    let row: Option<(Uuid, String)> =
        sqlx::query_as("SELECT org_id, employment_status FROM staff_profiles WHERE user_id = $1")
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    match row {
        Some((org_id, status)) if status == "active" => Ok(org_id),
        Some((_, status)) => Err(AppError::Forbidden(format!(
            "Your employment is {status} — contact your manager"
        ))),
        None => Err(AppError::Forbidden(
            "You do not have an employee profile yet — ask your manager to set one up".into(),
        )),
    }
}

/// Today's calendar date in a given timezone, decided by Postgres so the tz
/// database owns DST rather than the server process.
async fn today_in(pool: &PgPool, timezone: &str) -> Result<NaiveDate, AppError> {
    Ok(
        sqlx::query_scalar::<_, NaiveDate>("SELECT (now() AT TIME ZONE $1)::date")
            .bind(timezone)
            .fetch_one(pool)
            .await?,
    )
}

/// What the day's approved requests forgive. Thin wrapper over
/// [`crate::staff::requests::day_adjustments`] that supplies the org's
/// excused-time-paid default.
async fn adjustments_for(
    pool: &PgPool,
    settings: &AttendanceSettings,
    user_id: Uuid,
    date: NaiveDate,
    timezone: &str,
) -> Result<DayAdjustments, AppError> {
    crate::staff::requests::day_adjustments(
        pool,
        user_id,
        date,
        timezone,
        settings.excused_time_paid_default,
    )
    .await
}

/// Resolve the shift a punch at `now` belongs to, looking at both today's and
/// yesterday's roster so a night shift's after-midnight arrival stays on the day
/// the shift started. Returns the shift and the business date it belongs to.
async fn resolve_punch_shift(
    pool: &PgPool,
    user_id: Uuid,
    today: NaiveDate,
    timezone: &str,
    now: DateTime<Utc>,
) -> Result<(Option<ResolvedShift>, NaiveDate), AppError> {
    let mut best: Option<(ResolvedShift, NaiveDate)> = None;

    for date in [today, today.pred_opt().unwrap_or(today)] {
        let candidates = resolve_shifts_for(pool, user_id, date, timezone).await?;
        // Yesterday only ever contributes a shift that actually runs into today.
        let candidates: Vec<ResolvedShift> = if date == today {
            candidates
        } else {
            candidates
                .into_iter()
                .filter(|s| s.scheduled_end_at > now)
                .collect()
        };
        if let Some(pick) = pick_shift_for_instant(&candidates, now) {
            let closer = best.as_ref().is_none_or(|(current, _)| {
                (pick.scheduled_start_at - now).num_seconds().abs()
                    < (current.scheduled_start_at - now).num_seconds().abs()
            });
            if closer {
                best = Some((pick.clone(), date));
            }
        }
    }

    Ok(match best {
        Some((shift, date)) => (Some(shift), date),
        // Unrostered day: still a real attendance record, just with nothing to
        // be late for.
        None => (None, today),
    })
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
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CheckInRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let user_id = claims.user_id_safe()?;
    let org_id = require_active_profile(pool.get_ref(), user_id).await?;

    let branch_org = crate::staff::resolve_branch_org(pool.get_ref(), body.branch_id).await?;
    if branch_org != org_id {
        return Err(AppError::Forbidden(
            "That branch belongs to a different organization".into(),
        ));
    }

    let settings = load_settings(pool.get_ref(), org_id, Some(body.branch_id)).await?;
    let distance = check_geofence(
        pool.get_ref(),
        body.branch_id,
        body.latitude,
        body.longitude,
        settings.require_geofence,
    )
    .await?;

    let tz = branch_timezone(pool.get_ref(), body.branch_id).await?;
    let now = Utc::now();
    let today = today_in(pool.get_ref(), &tz).await?;
    let (shift, business_date) =
        resolve_punch_shift(pool.get_ref(), user_id, today, &tz, now).await?;

    // Arriving before the shift's check-in window is a mistake, not a punch —
    // otherwise an early bird opens the record that the real shift needs.
    if let Some(s) = &shift {
        let opens_at = s.scheduled_start_at
            - chrono::Duration::minutes(s.checkin_window_minutes.max(0) as i64);
        if now < opens_at {
            return Err(AppError::BadRequest(format!(
                "Too early — check-in for {} opens {} minutes before it starts",
                s.name, s.checkin_window_minutes
            )));
        }
    }

    let adjustments =
        adjustments_for(pool.get_ref(), &settings, user_id, business_date, &tz).await?;
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
            org_id, user_id, branch_id, work_shift_id, business_date, status,
            scheduled_start_at, scheduled_end_at,
            check_in_at, check_in_latitude, check_in_longitude,
            check_in_distance_meters, check_in_method,
            late_minutes, created_by
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, 'mobile_gps', $13, $2)
        ON CONFLICT (user_id, business_date,
                     COALESCE(work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid))
        DO NOTHING
        RETURNING id
        "#,
    )
    .bind(org_id)
    .bind(user_id)
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
    .fetch_optional(pool.get_ref())
    .await?;

    let Some(id) = inserted else {
        return Err(AppError::Conflict(
            "You have already checked in for this shift".into(),
        ));
    };
    let record = load_record(pool.get_ref(), org_id, id).await?;
    Ok(HttpResponse::Created().json(record))
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
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<CheckOutRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let user_id = claims.user_id_safe()?;
    let org_id = require_active_profile(pool.get_ref(), user_id).await?;

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

    let open: Open = sqlx::query_as(
        "SELECT id, branch_id, business_date, work_shift_id, check_in_at, \
                scheduled_start_at, scheduled_end_at \
           FROM attendance_records \
          WHERE user_id = $1 AND check_in_at IS NOT NULL AND check_out_at IS NULL \
          ORDER BY check_in_at DESC LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("You are not checked in".into()))?;

    let settings = load_settings(pool.get_ref(), org_id, Some(open.branch_id)).await?;
    let distance = check_geofence(
        pool.get_ref(),
        open.branch_id,
        body.latitude,
        body.longitude,
        settings.require_geofence,
    )
    .await?;

    let tz = branch_timezone(pool.get_ref(), open.branch_id).await?;
    let now = Utc::now();
    let shift = load_shift_snapshot(pool.get_ref(), &open.work_shift_id, open.business_date, &tz)
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
    let adjustments =
        adjustments_for(pool.get_ref(), &settings, user_id, open.business_date, &tz).await?;

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
            check_out_distance_meters = $5, check_out_method = 'mobile_gps', \
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
    .execute(pool.get_ref())
    .await?;

    // The shift just closed, so price it now — a manager should see the penalty
    // immediately, not the next morning after the sweep.
    let mut conn = pool.acquire().await?;
    crate::staff::penalties::recompute_record(&mut conn, open.id, &settings).await?;
    drop(conn);

    let record = load_record(pool.get_ref(), org_id, open.id).await?;
    Ok(HttpResponse::Ok().json(record))
}

/// Re-materialise a work shift's window on a given business date. Used by
/// checkout and correction, where the record already names its shift.
pub(crate) async fn load_shift_snapshot(
    pool: &PgPool,
    work_shift_id: &Option<Uuid>,
    business_date: NaiveDate,
    timezone: &str,
) -> Result<Option<ResolvedShift>, AppError> {
    let Some(shift_id) = work_shift_id else {
        return Ok(None);
    };
    Ok(sqlx::query_as::<_, ResolvedShift>(
        r#"
        SELECT ws.id AS work_shift_id, ws.name, ws.grace_minutes, ws.break_minutes,
               ws.paid_break, ws.half_day_threshold_minutes,
               ws.overtime_threshold_minutes, ws.overtime_multiplier,
               ws.checkin_window_minutes,
               ($2::date + ws.start_time) AT TIME ZONE $3 AS scheduled_start_at,
               ($2::date + ws.end_time
                    + CASE WHEN ws.crosses_midnight
                           THEN INTERVAL '1 day' ELSE INTERVAL '0 day' END
               ) AT TIME ZONE $3 AS scheduled_end_at
          FROM work_shifts ws WHERE ws.id = $1
        "#,
    )
    .bind(shift_id)
    .bind(business_date)
    .bind(timezone)
    .fetch_optional(pool)
    .await?)
}

#[utoipa::path(
    get, path = "/staff/me/today", tag = "staff",
    responses((status = 200, description = "The employee's own status right now", body = MyAttendanceToday), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_today(req: HttpRequest, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let user_id = claims.user_id_safe()?;
    let org_id = require_active_profile(pool.get_ref(), user_id).await?;

    let tz = crate::staff::schedules::employee_timezone(pool.get_ref(), org_id, user_id).await?;
    let today = today_in(pool.get_ref(), &tz).await?;
    let scheduled = resolve_shifts_for(pool.get_ref(), user_id, today, &tz).await?;

    let records = sqlx::query_as::<_, AttendanceRecord>(&format!(
        "SELECT {RECORD_COLS} {RECORD_JOINS} \
          WHERE a.user_id = $1 AND a.business_date = $2 ORDER BY a.check_in_at NULLS LAST"
    ))
    .bind(user_id)
    .bind(today)
    .fetch_all(pool.get_ref())
    .await?;

    // The open record may belong to YESTERDAY's business date on a night shift,
    // so it is looked up independently of today's rows.
    let open_record = sqlx::query_as::<_, AttendanceRecord>(&format!(
        "SELECT {RECORD_COLS} {RECORD_JOINS} \
          WHERE a.user_id = $1 AND a.check_in_at IS NOT NULL AND a.check_out_at IS NULL \
          ORDER BY a.check_in_at DESC LIMIT 1"
    ))
    .bind(user_id)
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
        user_id,
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
    user_id: Uuid,
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
             SELECT uba.branch_id, COUNT(*) OVER () AS n
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
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<RangeQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let user_id = claims.user_id_safe()?;
    require_active_profile(pool.get_ref(), user_id).await?;
    validate_range(query.from, query.to, MAX_RANGE_DAYS)?;

    let rows = sqlx::query_as::<_, AttendanceRecord>(&format!(
        "SELECT {RECORD_COLS} {RECORD_JOINS} \
          WHERE a.user_id = $1 AND a.business_date BETWEEN $2 AND $3 \
          ORDER BY a.business_date DESC, a.check_in_at DESC NULLS LAST"
    ))
    .bind(user_id)
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
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "attendance", "read").await?;
    let org_id = scope_org(&req, &claims)?;
    validate_range(query.from, query.to, MAX_RANGE_DAYS)?;
    if let Some(status) = query.status.as_deref() {
        AttendanceStatus::parse(status)?;
    }

    let rows = sqlx::query_as::<_, AttendanceRecord>(&format!(
        "SELECT {RECORD_COLS} {RECORD_JOINS} \
          WHERE a.org_id = $1 \
            AND a.business_date BETWEEN $2 AND $3 \
            AND ($4::uuid IS NULL OR a.branch_id = $4) \
            AND ($5::uuid IS NULL OR a.user_id = $5) \
            AND ($6::text IS NULL OR a.status = $6) \
          ORDER BY a.business_date DESC, lower(u.name)"
    ))
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(query.branch_id)
    .bind(query.user_id)
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
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "attendance", "read").await?;
    let org_id = scope_org(&req, &claims)?;
    validate_range(query.from, query.to, MAX_RANGE_DAYS)?;

    let rows = sqlx::query_as::<_, AttendanceSummary>(
        r#"
        SELECT a.user_id, u.name AS user_name,
               COUNT(*) FILTER (WHERE a.status = 'present')  AS present_days,
               COUNT(*) FILTER (WHERE a.status = 'late')     AS late_days,
               COUNT(*) FILTER (WHERE a.status = 'absent')   AS absent_days,
               COUNT(*) FILTER (WHERE a.status = 'half_day') AS half_days,
               COUNT(*) FILTER (WHERE a.status = 'on_leave') AS leave_days,
               COALESCE(SUM(a.late_minutes), 0)::bigint     AS total_late_minutes,
               COALESCE(SUM(a.overtime_minutes), 0)::bigint AS total_overtime_minutes,
               COALESCE(SUM(a.worked_minutes), 0)::bigint   AS total_worked_minutes
          FROM attendance_records a
          JOIN users u ON u.id = a.user_id
         WHERE a.org_id = $1
           AND a.business_date BETWEEN $2 AND $3
           AND ($4::uuid IS NULL OR a.branch_id = $4)
           AND ($5::uuid IS NULL OR a.user_id = $5)
         GROUP BY a.user_id, u.name
         ORDER BY lower(u.name)
        "#,
    )
    .bind(org_id)
    .bind(query.from)
    .bind(query.to)
    .bind(query.branch_id)
    .bind(query.user_id)
    .fetch_all(pool.get_ref())
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

/// One person's state right now, for the manager's live team list.
#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow, ToSchema)]
pub struct PresenceRow {
    pub user_id: Uuid,
    pub user_name: String,
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
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "attendance", "read").await?;
    let org_id = scope_org(&req, &claims)?;

    let tz = crate::staff::schedules::org_timezone(pool.get_ref(), org_id).await?;
    let today = today_in(pool.get_ref(), &tz).await?;

    let rows = sqlx::query_as::<_, PresenceRow>(
        r#"
        WITH roster AS (
            -- Everyone active, with the minutes they are rostered for today and
            -- when that shift was due to start.
            SELECT p.user_id,
                   u.name AS user_name,
                   p.job_title,
                   (SELECT b.name
                      FROM user_branch_assignments uba
                      JOIN branches b ON b.id = uba.branch_id
                                     AND b.deleted_at IS NULL
                                     AND b.org_id = p.org_id
                     WHERE uba.user_id = p.user_id
                     LIMIT 1)                                     AS branch_name,
                   COALESCE((
                       SELECT SUM(EXTRACT(EPOCH FROM (
                                  ws.end_time - ws.start_time
                                  + CASE WHEN ws.crosses_midnight
                                         THEN INTERVAL '1 day' ELSE INTERVAL '0 day' END
                              )) / 60)::bigint
                         FROM staff_schedules s
                         JOIN work_shifts ws ON ws.id = s.work_shift_id AND ws.is_active
                        WHERE s.user_id = p.user_id
                          AND s.effective_from <= $2
                          AND (s.effective_to IS NULL OR s.effective_to >= $2)
                          AND (s.day_of_week IS NULL
                               OR s.day_of_week = EXTRACT(DOW FROM $2::date)::smallint)
                   ), 0)                                          AS scheduled_minutes,
                   (SELECT MIN(($2::date + ws.start_time) AT TIME ZONE $3)
                      FROM staff_schedules s
                      JOIN work_shifts ws ON ws.id = s.work_shift_id AND ws.is_active
                     WHERE s.user_id = p.user_id
                       AND s.effective_from <= $2
                       AND (s.effective_to IS NULL OR s.effective_to >= $2)
                       AND (s.day_of_week IS NULL
                            OR s.day_of_week = EXTRACT(DOW FROM $2::date)::smallint)
                   )                                              AS due_at
              FROM staff_profiles p
              JOIN users u ON u.id = p.user_id AND u.deleted_at IS NULL
             WHERE p.org_id = $1 AND p.employment_status = 'active'
        ),
        today AS (
            SELECT DISTINCT ON (a.user_id)
                   a.user_id, a.check_in_at, a.check_out_at, a.status,
                   a.late_minutes, a.worked_minutes, a.branch_id
              FROM attendance_records a
             WHERE a.org_id = $1 AND a.business_date = $2
             ORDER BY a.user_id, a.check_in_at DESC NULLS LAST
        )
        SELECT r.user_id, r.user_name, r.job_title, r.branch_name,
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
                   -- Rostered, nothing recorded, and the shift is already due:
                   -- that is an absence. Before it is due, they are just off.
                   WHEN r.scheduled_minutes > 0 AND r.due_at <= now() THEN 'absent'
                   ELSE 'off'
               END AS state
          FROM roster r
          LEFT JOIN today t ON t.user_id = r.user_id
         WHERE ($4::uuid IS NULL
                OR t.branch_id = $4
                OR EXISTS (SELECT 1 FROM user_branch_assignments uba
                            WHERE uba.user_id = r.user_id AND uba.branch_id = $4))
         ORDER BY lower(r.user_name)
        "#,
    )
    .bind(org_id)
    .bind(today)
    .bind(&tz)
    .bind(query.branch_id)
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
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "attendance", "create").await?;
    let org_id = scope_org(&req, &claims)?;
    require_user_in_org(pool.get_ref(), org_id, body.user_id).await?;

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
    let shift =
        load_shift_snapshot(pool.get_ref(), &body.work_shift_id, body.business_date, &tz).await?;
    let settings = load_settings(pool.get_ref(), org_id, Some(body.branch_id)).await?;
    let adjustments = adjustments_for(
        pool.get_ref(),
        &settings,
        body.user_id,
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
            org_id, user_id, branch_id, work_shift_id, business_date, status,
            scheduled_start_at, scheduled_end_at,
            check_in_at, check_in_method, check_out_at, check_out_method,
            late_minutes, early_leave_minutes, overtime_minutes, worked_minutes,
            is_manual, notes, edit_reason, created_by, edited_by
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8,
            $9, CASE WHEN $9::timestamptz IS NULL THEN NULL ELSE 'manual' END,
            $10, CASE WHEN $10::timestamptz IS NULL THEN NULL ELSE 'manual' END,
            $11, $12, $13, $14, TRUE, $15, $16, $17, $17
        )
        ON CONFLICT (user_id, business_date,
                     COALESCE(work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid))
        DO NOTHING
        RETURNING id
        "#,
    )
    .bind(org_id)
    .bind(body.user_id)
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
    .bind(claims.user_id())
    .fetch_optional(pool.get_ref())
    .await?;

    let Some(id) = inserted else {
        return Err(AppError::Conflict(
            "This employee already has a record for that day and shift — correct it instead".into(),
        ));
    };

    let mut conn = pool.acquire().await?;
    crate::staff::penalties::recompute_record(&mut conn, id, &settings).await?;
    drop(conn);

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
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "attendance", "update").await?;
    let org_id = scope_org(&req, &claims)?;

    let reason = body.reason.trim();
    if reason.is_empty() {
        return Err(AppError::BadRequest("A correction needs a reason".into()));
    }

    apply_punch_correction(
        pool.get_ref(),
        org_id,
        *id,
        body.check_in_at,
        body.check_out_at,
        body.status.as_deref(),
        body.notes.as_deref(),
        reason,
        Some(claims.user_id()),
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
    let existing = load_record(pool, org_id, record_id).await?;
    let check_in_at = check_in_at.or(existing.check_in_at);
    let check_out_at = check_out_at.or(existing.check_out_at);
    if let (Some(in_at), Some(out_at)) = (check_in_at, check_out_at)
        && out_at < in_at
    {
        return Err(AppError::BadRequest("Check-out is before check-in".into()));
    }

    let tz = branch_timezone(pool, existing.branch_id).await?;
    let shift =
        load_shift_snapshot(pool, &existing.work_shift_id, existing.business_date, &tz).await?;
    let settings = load_settings(pool, org_id, Some(existing.branch_id)).await?;
    let adjustments = adjustments_for(
        pool,
        &settings,
        existing.user_id,
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
    let status = match status_override {
        Some(s) => AttendanceStatus::parse(s)?,
        None => derived.status,
    };

    sqlx::query(
        "UPDATE attendance_records SET \
            check_in_at  = $3, \
            check_in_method  = COALESCE(check_in_method,  CASE WHEN $3::timestamptz IS NULL THEN NULL ELSE 'manual' END), \
            check_out_at = $4, \
            check_out_method = COALESCE(check_out_method, CASE WHEN $4::timestamptz IS NULL THEN NULL ELSE 'manual' END), \
            status = $5, late_minutes = $6, early_leave_minutes = $7, \
            overtime_minutes = $8, worked_minutes = $9, \
            notes = COALESCE($10, notes), edit_reason = $11, edited_by = $12, \
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
    .execute(pool)
    .await?;

    // A correction changes what is owed. Recompute — but `penalties` leaves any
    // deduction a human has already waived or overridden exactly as it is.
    let mut conn = pool.acquire().await?;
    crate::staff::penalties::recompute_record(&mut conn, record_id, &settings).await?;
    Ok(())
}

#[utoipa::path(
    delete, path = "/staff/attendance/{id}", tag = "staff",
    params(("id" = Uuid, Path, description = "Attendance record ID")),
    responses((status = 204, description = "Record deleted"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn delete_record(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    check_permission(pool.get_ref(), &claims, "attendance", "delete").await?;
    let org_id = scope_org(&req, &claims)?;

    let deleted = sqlx::query("DELETE FROM attendance_records WHERE id = $1 AND org_id = $2")
        .bind(*id)
        .bind(org_id)
        .execute(pool.get_ref())
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(AppError::NotFound("Attendance record not found".into()));
    }
    Ok(HttpResponse::NoContent().finish())
}
