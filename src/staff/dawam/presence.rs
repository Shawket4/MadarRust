//! Presence during a shift (CL-4..CL-17), covers (CV-*), overtime approval
//! (RU-7) and punching for someone (CL-13).

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::{
    branches_of, employee_name, notify, notify_managers, notify_managers_once, owners, user_name,
};
use crate::authz::{Cap, Decision, Request as AuthzRequest};
use crate::errors::{AppError, AppErrorResponse};
use crate::geo::osrm::{LatLng, haversine_meters};
use crate::staff::access;
use crate::staff::attendance::{AttendanceSettings, load_settings, require_active_employee};
use crate::staff::principal::{Me, caller};

/// Two pings in a row outside the fence is "left" (CL-6).
const OUTSIDE_STREAK: usize = 2;
/// Faster than this between two pings is not a person walking (CL-9).
const MAX_SPEED_MPS: f64 = 70.0;

pub(crate) async fn mark_tracking_off(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
    branch_id: Uuid,
    record_id: Uuid,
) -> Result<(), AppError> {
    sqlx::query("UPDATE attendance_records SET tracking_off = true WHERE id = $1")
        .bind(record_id)
        .execute(pool)
        .await?;
    raise_flag(
        pool,
        org_id,
        employee_id,
        Some(branch_id),
        Some(record_id),
        "tracking_off",
        0,
    )
    .await
    .map(|_| ())
}

/// At or under this, the employee is told to charge and silence reads "phone
/// likely died" (CL-12).
pub(crate) const LOW_BATTERY: i16 = 15;

/// Opens a flag once per shift and kind, and tells the managers — once, when
/// it opens (audit 03 bug 3): a later ping only updates the minutes away.
/// Returns whether this call opened it.
pub(crate) async fn raise_flag(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
    branch_id: Option<Uuid>,
    record_id: Option<Uuid>,
    kind: &str,
    minutes_away: i32,
) -> Result<bool, AppError> {
    // `xmax = 0` only on the row this statement inserted; an `ON CONFLICT
    // DO UPDATE` also reports one row, which is what re-notified every ping.
    let row: Option<(Uuid, bool)> = sqlx::query_as(
        "INSERT INTO attendance_flags (org_id, employee_id, branch_id, attendance_record_id, kind, minutes_away) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (attendance_record_id, kind) WHERE resolution IS NULL AND attendance_record_id IS NOT NULL \
         DO UPDATE SET minutes_away = GREATEST(attendance_flags.minutes_away, EXCLUDED.minutes_away) \
         RETURNING id, (xmax = 0)",
    )
    .bind(org_id)
    .bind(employee_id)
    .bind(branch_id)
    .bind(record_id)
    .bind(kind)
    .bind(minutes_away)
    .fetch_optional(pool)
    .await?;
    let opened = row.map(|(_, inserted)| inserted).unwrap_or(false);
    if let Some((flag, true)) = row {
        // Once per flag (06 B7): the next ping updates the same open flag and
        // must not push to every manager again; the notice is also keyed by the
        // flag, so a repeat can never write or push twice.
        let name = employee_name(pool, employee_id).await;
        notify_managers_once(
            pool,
            org_id,
            branch_id,
            Cap::HrAttendanceEdit,
            Some(employee_id),
            &format!("staff.n_flag_{kind}"),
            json!({ "name": name, "minutes": minutes_away }),
            &format!("flag:{flag}"),
        )
        .await;
    }
    Ok(opened)
}

/// The punch's own fix, judged like a ping's (CL-8, CL-9): the OS's mock
/// marker, or an accuracy no real receiver reports, flags the shift
/// "location suspicious". With tracking off there are no pings, so without
/// this a mocked clock-in would never be noticed.
pub(crate) async fn check_punch_fix(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
    branch_id: Uuid,
    record_id: Uuid,
    accuracy_meters: Option<f64>,
    is_mock: Option<bool>,
) -> Result<bool, AppError> {
    let suspicious = is_mock == Some(true) || accuracy_meters.is_some_and(|a| a <= 0.0);
    if suspicious {
        raise_flag(
            pool,
            org_id,
            employee_id,
            Some(branch_id),
            Some(record_id),
            "suspicious",
            0,
        )
        .await?;
    }
    Ok(suspicious)
}

/// After a check-out: overtime off, paid automatically, or pending (RU-7).
pub(crate) async fn after_check_out(
    pool: &PgPool,
    org_id: Uuid,
    record_id: Uuid,
    settings: &AttendanceSettings,
) -> Result<(), AppError> {
    let status = match settings.overtime_mode.as_str() {
        "automatic" => "approved",
        "approval" => "pending",
        _ => return Ok(()),
    };
    let row: Option<(Uuid, Uuid, i32)> = sqlx::query_as(
        "UPDATE attendance_records SET overtime_status = $2 \
          WHERE id = $1 AND overtime_minutes > 0 AND covered_employee_id IS NULL \
            AND overtime_status IS NULL \
          RETURNING employee_id, branch_id, overtime_minutes",
    )
    .bind(record_id)
    .bind(status)
    .fetch_optional(pool)
    .await?;
    if let (Some((employee_id, branch_id, minutes)), "pending") = (row, status) {
        let name = employee_name(pool, employee_id).await;
        notify_managers(
            pool,
            org_id,
            Some(branch_id),
            Cap::HrOvertimeApprove,
            Some(employee_id),
            "staff.n_overtime",
            json!({ "name": name, "minutes": minutes }),
        )
        .await;
    }
    Ok(())
}

// ── pings ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct PingRequest {
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub accuracy_meters: Option<f64>,
    /// The OS's own mock-location marker (Android `isMock`, iOS
    /// `isSimulatedBySoftware`) (CL-9).
    #[serde(default)]
    pub is_mock: Option<bool>,
    #[serde(default)]
    pub battery_percent: Option<i16>,
    /// Set when the ping was queued offline; the server rebuilds its time (CL-11).
    #[serde(default)]
    pub offline: Option<super::clock::OfflineStamp>,
}

#[derive(Serialize, ToSchema)]
pub struct PingResult {
    pub inside: bool,
    pub distance_meters: f64,
    /// Flags this ping raised: `left_mid_shift`, `suspicious`.
    pub flags: Vec<String>,
    /// At 15% or less during a shift: tell them to charge (CL-12).
    pub charge_phone: bool,
}

#[derive(sqlx::FromRow, Clone, Debug)]
pub(crate) struct PriorPing {
    pub at: DateTime<Utc>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub accuracy_meters: Option<f64>,
    pub inside: bool,
}

/// Accuracies iOS reports in fixed steps: an honest phone repeats them, so
/// they never read as "frozen" (audit 03 bug 11).
const QUANTISED_ACCURACY: [f64; 7] = [5.0, 10.0, 30.0, 65.0, 100.0, 165.0, 200.0];
/// How many readings in a row, this one included, make a frozen accuracy.
const FROZEN_RUN: usize = 4;
/// How many identical positions in a row, this one included, read as a
/// replayed or cached location (CL-8). One repeat is what an OS cache hands
/// back; three in a row across half an hour is not a person standing still.
const IDENTICAL_RUN: usize = 3;

/// The spoofing signals of one reading against the ones before it (CL-8, CL-9).
/// `prior` is newest first.
pub(crate) fn spoof_signals(
    here: (f64, f64),
    accuracy: Option<f64>,
    is_mock: Option<bool>,
    at: DateTime<Utc>,
    prior: &[PriorPing],
) -> bool {
    let identical = prior.len() + 1 >= IDENTICAL_RUN
        && prior
            .iter()
            .take(IDENTICAL_RUN - 1)
            .all(|p| p.latitude == Some(here.0) && p.longitude == Some(here.1));
    let frozen_accuracy = accuracy.is_some_and(|a| {
        a <= 0.0
            || (!QUANTISED_ACCURACY.contains(&a)
                && prior.len() + 1 >= FROZEN_RUN
                && prior
                    .iter()
                    .take(FROZEN_RUN - 1)
                    .all(|p| p.accuracy_meters == Some(a)))
    });
    let too_fast = prior
        .first()
        .is_some_and(|p| match (p.latitude, p.longitude) {
            (Some(lat), Some(lng)) => {
                let secs = (at - p.at).num_seconds().max(1) as f64;
                haversine_meters(
                    LatLng { lat, lng },
                    LatLng {
                        lat: here.0,
                        lng: here.1,
                    },
                ) / secs
                    > MAX_SPEED_MPS
            }
            _ => false,
        });
    is_mock == Some(true) || identical || frozen_accuracy || too_fast
}

/// Minutes outside the fence on this shift, up to `now` (CL-7): every outside
/// run, each from its first outside ping to the ping that found them back
/// (or `now`), less the time an approved excuse, mission or early departure
/// covers. Pings are oldest first; only those after `since` (a left-mid-shift
/// flag already handled) count.
pub(crate) fn away_minutes(
    pings: &[(DateTime<Utc>, bool)],
    now: DateTime<Utc>,
    excused: &[(DateTime<Utc>, DateTime<Utc>)],
    since: Option<DateTime<Utc>>,
) -> i64 {
    let mut runs: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::new();
    let mut out_since: Option<DateTime<Utc>> = None;
    for (at, inside) in pings.iter().filter(|(at, _)| since.is_none_or(|s| *at > s)) {
        match (inside, out_since) {
            (false, None) => out_since = Some(*at),
            (true, Some(from)) => {
                runs.push((from, *at));
                out_since = None;
            }
            _ => {}
        }
    }
    if let Some(from) = out_since {
        runs.push((from, now));
    }
    let mut secs = 0i64;
    for (from, to) in runs {
        let mut run = (to - from).num_seconds().max(0);
        for (a, b) in excused {
            let (lo, hi) = (from.max(*a), to.min(*b));
            if hi > lo {
                run -= (hi - lo).num_seconds();
            }
        }
        secs += run.max(0);
    }
    secs / 60
}

/// Slack at the edges of an excused window: pings are 15 minutes apart.
const EXCUSE_SLACK_MIN: i64 = 10;

/// A location every 15 minutes between clock-in and clock-out (CL-4, CL-17).
#[utoipa::path(
    post, path = "/staff/me/pings", tag = "staff", request_body = PingRequest,
    responses((status = 200, body = PingResult), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn ping(
    me: Me,
    pool: crate::db::Db,
    secret: web::Data<crate::auth::jwt::JwtSecret>,
    body: web::Json<PingRequest>,
) -> Result<HttpResponse, AppError> {
    // From the employee's live phone only (CL-1, checked by StaffAuth).
    let employee_id = me.employee_id;
    let org_id = me.org_id;
    let pool = pool.get_ref();
    // Location only after the notice was accepted on this phone (AT-5).
    super::privacy::require_accepted(pool, &me).await?;

    let stamped = super::clock::rebuild(body.offline.as_ref(), Utc::now(), me.verifier(&secret))?;
    let now = stamped.at;
    // Only while clocked in: location is never collected off shift (CL-17). A
    // queued ping belongs to the record that was open at ITS time.
    #[allow(clippy::type_complexity)]
    let open: Option<(Uuid, Uuid, NaiveDate, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT id, branch_id, business_date, scheduled_start_at FROM attendance_records \
          WHERE employee_id = $1 AND check_in_at IS NOT NULL AND check_in_at <= $2 \
            AND (check_out_at IS NULL OR check_out_at >= $2) \
          ORDER BY check_in_at DESC LIMIT 1",
    )
    .bind(employee_id)
    .bind(now)
    .fetch_optional(pool)
    .await?;
    let Some((record_id, branch_id, business_date, scheduled_start)) = open else {
        return Err(AppError::Refused {
            code: "NOT_CLOCKED_IN",
            reason: "You are not clocked in.".into(),
        });
    };
    // Nothing is written into an approved or paid month — not a ping, not
    // its flags (owner decision BC-3).
    crate::staff::period_lock::assert_open(pool, org_id, business_date, "a location ping").await?;
    let fence: (Option<f64>, Option<f64>, Option<i32>) =
        sqlx::query_as("SELECT latitude, longitude, geo_radius_meters FROM branches WHERE id = $1")
            .bind(branch_id)
            .fetch_one(pool)
            .await?;
    let here = LatLng {
        lat: body.latitude,
        lng: body.longitude,
    };
    let (distance, inside) = match (fence.0, fence.1) {
        (Some(lat), Some(lng)) => {
            let d = haversine_meters(LatLng { lat, lng }, here);
            (d, d <= f64::from(fence.2.unwrap_or(200).max(0)))
        }
        _ => (0.0, true),
    };

    let prior: Vec<PriorPing> = sqlx::query_as(
        "SELECT at, latitude, longitude, accuracy_meters, inside FROM attendance_pings \
          WHERE attendance_record_id = $1 AND at <= $2 ORDER BY at DESC LIMIT 4",
    )
    .bind(record_id)
    .bind(now)
    .fetch_all(pool)
    .await?;
    sqlx::query(
        "INSERT INTO attendance_pings (org_id, employee_id, attendance_record_id, at, latitude, \
            longitude, accuracy_meters, distance_meters, inside, is_mock, battery_percent, \
            time_unverified) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(org_id)
    .bind(employee_id)
    .bind(record_id)
    .bind(now)
    .bind(body.latitude)
    .bind(body.longitude)
    .bind(body.accuracy_meters)
    .bind(distance)
    .bind(inside)
    .bind(body.is_mock.unwrap_or(false))
    .bind(body.battery_percent)
    .bind(stamped.unverified)
    .execute(pool)
    .await?;

    let mut flags = Vec::new();

    // Left mid-shift: two outside in a row, not covered by an approved excuse,
    // early departure or mission (CL-6). Nothing is charged here.
    let streak = 1 + prior.iter().take_while(|p| !p.inside).count();
    if !inside && streak >= OUTSIDE_STREAK {
        let tz = crate::staff::branch_timezone(pool, branch_id).await?;
        let excused =
            excused_windows(pool, employee_id, business_date, &tz, scheduled_start).await?;
        let slack = chrono::Duration::minutes(EXCUSE_SLACK_MIN);
        let covered = excused
            .iter()
            .any(|(a, b)| *a - slack <= now && now <= *b + slack);
        if !covered {
            let since: Option<DateTime<Utc>> = sqlx::query_scalar(
                "SELECT MAX(resolved_at) FROM attendance_flags \
                  WHERE attendance_record_id = $1 AND kind = 'left_mid_shift' \
                    AND resolution IS NOT NULL",
            )
            .bind(record_id)
            .fetch_one(pool)
            .await?;
            let trail: Vec<(DateTime<Utc>, bool)> = sqlx::query_as(
                "SELECT at, inside FROM attendance_pings \
                  WHERE attendance_record_id = $1 AND at <= $2 ORDER BY at",
            )
            .bind(record_id)
            .bind(now)
            .fetch_all(pool)
            .await?;
            let minutes = away_minutes(&trail, now, &excused, since).max(15) as i32;
            raise_flag(
                pool,
                org_id,
                employee_id,
                Some(branch_id),
                Some(record_id),
                "left_mid_shift",
                minutes,
            )
            .await?;
            flags.push("left_mid_shift".to_string());
        }
    }

    // Spoofing (CL-8, CL-9): real GPS always drifts, so identical coordinates,
    // a perfect or frozen accuracy, the OS's mock marker, or an impossible
    // speed all raise the same flag.
    if spoof_signals(
        (body.latitude, body.longitude),
        body.accuracy_meters,
        body.is_mock,
        now,
        &prior,
    ) {
        raise_flag(
            pool,
            org_id,
            employee_id,
            Some(branch_id),
            Some(record_id),
            "suspicious",
            0,
        )
        .await?;
        flags.push("suspicious".to_string());
    }
    // A queued ping whose time the server can't vouch for (CL-11).
    if stamped.unverified {
        raise_flag(
            pool,
            org_id,
            employee_id,
            Some(branch_id),
            Some(record_id),
            "time_unverified",
            0,
        )
        .await?;
    }

    // Low battery on shift: say so once, by push as well, since the app may
    // be in the background (CL-12).
    let charge_phone = body.battery_percent.is_some_and(|b| b <= LOW_BATTERY);
    if charge_phone && body.offline.is_none() {
        let first = sqlx::query(
            "UPDATE attendance_records SET low_battery_warned_at = now() \
              WHERE id = $1 AND low_battery_warned_at IS NULL",
        )
        .bind(record_id)
        .execute(pool)
        .await?
        .rows_affected()
            > 0;
        if first {
            notify(pool, org_id, employee_id, "staff.n_charge_phone", json!({})).await;
        }
    }

    Ok(HttpResponse::Ok().json(PingResult {
        inside,
        distance_meters: distance,
        flags,
        charge_phone,
    }))
}

/// The windows an approved excuse, early departure or mission excuses being
/// away (CL-6), as instants in the branch's zone. An excuse's hours bound
/// it; an early departure runs from its time past the end of the shift; a
/// mission without hours is its whole days. On a night shift a time earlier
/// than the shift's start is the next morning's (SC-10).
pub(crate) async fn excused_windows(
    pool: &PgPool,
    employee_id: Uuid,
    business_date: NaiveDate,
    tz: &str,
    shift_start: Option<DateTime<Utc>>,
) -> Result<Vec<(DateTime<Utc>, DateTime<Utc>)>, AppError> {
    Ok(sqlx::query_as(
        "WITH r AS ( \
             SELECT kind, on_date, COALESCE(end_date, on_date) AS end_date, from_time, to_time, \
                    CASE WHEN $4::timestamptz IS NOT NULL AND from_time IS NOT NULL \
                              AND on_date = $2 \
                              AND (on_date + from_time) AT TIME ZONE $3 < $4::timestamptz - INTERVAL '1 hour' \
                         THEN INTERVAL '1 day' ELSE INTERVAL '0 day' END AS shift \
               FROM staff_requests \
              WHERE employee_id = $1 AND status = 'approved' \
                AND kind IN ('excuse', 'early_departure', 'mission') \
                AND on_date <= $2 AND COALESCE(end_date, on_date) >= $2 \
         ) \
         SELECT CASE WHEN from_time IS NULL THEN on_date::timestamp AT TIME ZONE $3 \
                     ELSE (on_date + from_time + shift) AT TIME ZONE $3 END, \
                CASE WHEN kind = 'early_departure' THEN (end_date + 2)::timestamp AT TIME ZONE $3 \
                     WHEN to_time IS NULL THEN (end_date + 1)::timestamp AT TIME ZONE $3 \
                     ELSE (end_date + to_time + shift \
                           + CASE WHEN from_time IS NOT NULL AND to_time <= from_time \
                                  THEN INTERVAL '1 day' ELSE INTERVAL '0 day' END) AT TIME ZONE $3 END \
           FROM r",
    )
    .bind(employee_id)
    .bind(business_date)
    .bind(tz)
    .bind(shift_start)
    .fetch_all(pool)
    .await?)
}

// ── flags ──────────────────────────────────────────────────────────────────

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct AttendanceFlag {
    pub id: Uuid,
    pub employee_id: Uuid,
    pub employee_name: String,
    pub branch_id: Option<Uuid>,
    pub attendance_record_id: Option<Uuid>,
    /// `left_mid_shift` · `suspicious` · `tracking_off` · `time_unverified` ·
    /// `new_phone` · `cover`
    pub kind: String,
    pub minutes_away: i32,
    pub detected_at: DateTime<Utc>,
    pub resolution: Option<String>,
    pub resolved_at: Option<DateTime<Utc>>,
    /// Time away × the person's minute rate, rounded to the nearest 5 EGP (CL-7).
    #[sqlx(default)]
    pub suggested_deduction_piastres: i64,
    /// The deduction the flag was handled with (a deduct or an unpaid
    /// excuse), and its status: `approved`, or `pending` = over the
    /// manager's limit, it waits for the owner (minor default M33).
    #[sqlx(default)]
    pub deduction_id: Option<Uuid>,
    #[sqlx(default)]
    pub deduction_status: Option<String>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FlagQuery {
    #[serde(default)]
    pub branch_id: Option<Uuid>,
    /// Include handled flags.
    #[serde(default)]
    pub all: Option<bool>,
}

/// The flags a manager should look at, for their branches (RO-6).
#[utoipa::path(
    operation_id = "list_attendance_flags",
    get, path = "/staff/flags", tag = "staff", params(FlagQuery),
    responses((status = 200, body = Vec<AttendanceFlag>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_flags(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<FlagQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let branches = access::scope_at(
        pool.get_ref(),
        &claims,
        org_id,
        Cap::HrAttendanceRead,
        query.branch_id,
    )
    .await?;
    let mut rows: Vec<AttendanceFlag> = sqlx::query_as(
        "SELECT f.id, f.employee_id, e.name AS employee_name, f.branch_id, f.attendance_record_id, \
                f.kind, f.minutes_away, f.detected_at, f.resolution, f.resolved_at, \
                f.deduction_id, fd.status AS deduction_status \
           FROM attendance_flags f JOIN employees e ON e.id = f.employee_id \
           LEFT JOIN payroll_deductions fd ON fd.id = f.deduction_id \
          WHERE f.org_id = $1 AND ($2 OR f.resolution IS NULL) \
            AND ($3::uuid[] IS NULL OR f.branch_id = ANY($3)) \
          ORDER BY f.detected_at DESC LIMIT 200",
    )
    .bind(org_id)
    .bind(query.all.unwrap_or(false))
    .bind(branches.as_deref())
    .fetch_all(pool.get_ref())
    .await?;
    for f in &mut rows {
        if f.kind == "left_mid_shift" {
            f.suggested_deduction_piastres = away_cost(
                pool.get_ref(),
                org_id,
                f.employee_id,
                f.attendance_record_id,
                f.minutes_away,
            )
            .await?;
        }
    }
    Ok(HttpResponse::Ok().json(rows))
}

/// Minutes away at the person's minute rate, exact (piastres), from the
/// rules of the shift's branch (RU-2) — what an unpaid excuse deducts (RU-12:
/// stored amounts are exact).
async fn away_exact(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
    record_id: Option<Uuid>,
    minutes: i32,
) -> Result<Decimal, AppError> {
    let settings = load_settings(pool, org_id, None).await?;
    // With a record: the same facts payroll prices (the salary in force that
    // day, the day's rostered minutes, the shift's branch rules — AT-9).
    if let Some(id) = record_id
        && let Some(day) = crate::staff::penalties::load_facts(pool, id, &settings).await?
    {
        return Ok(crate::staff::pricing::minutes_piastres_exact(
            day.facts.base_salary_piastres,
            day.rules.working_days_per_month,
            day.facts.day_minutes.max(day.facts.scheduled_minutes),
            i64::from(minutes),
        ));
    }
    let salary: i64 =
        sqlx::query_scalar("SELECT COALESCE(base_salary_piastres, 0) FROM employees WHERE id = $1")
            .bind(employee_id)
            .fetch_optional(pool)
            .await?
            .unwrap_or(0);
    Ok(crate::staff::pricing::minutes_piastres_exact(
        salary,
        settings.working_days_per_month,
        crate::staff::pricing::DEFAULT_SHIFT_MINUTES,
        i64::from(minutes),
    ))
}

/// The suggestion shown to the manager: time away at the minute rate, to the
/// nearest 5 EGP, halves away from zero (CL-7).
async fn away_cost(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
    record_id: Option<Uuid>,
    minutes: i32,
) -> Result<i64, AppError> {
    let exact = away_exact(pool, org_id, employee_id, record_id, minutes).await?;
    Ok(nearest_five_pounds(exact))
}

/// Nearest 500 piastres, halves away from zero.
pub(crate) fn nearest_five_pounds(piastres: Decimal) -> i64 {
    let fives = (piastres / Decimal::from(500))
        .round_dp_with_strategy(0, rust_decimal::RoundingStrategy::MidpointAwayFromZero);
    (fives * Decimal::from(500)).try_into().unwrap_or(0)
}

#[derive(Deserialize, ToSchema)]
pub struct ResolveFlag {
    /// `ignore` · `excuse_paid` · `excuse_unpaid` · `deduct` · `revoke` (a new
    /// phone) · `confirm`
    pub action: String,
    /// For `deduct`: the amount the manager typed (CL-7).
    #[serde(default)]
    pub amount_piastres: Option<i64>,
    /// For `deduct`: why, on the pay line the employee sees (AD-9).
    #[serde(default)]
    pub reason: Option<String>,
}

/// Handle a flag. Nothing is ever charged automatically (CL-6).
#[utoipa::path(
    patch, path = "/staff/flags/{id}", tag = "staff", request_body = ResolveFlag,
    params(("id" = Uuid, Path)),
    responses((status = 200, body = AttendanceFlag), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn resolve_flag(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<ResolveFlag>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let by = claims.user_id_safe()?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrAttendanceEdit).await?;
    #[allow(clippy::type_complexity)]
    let flag: Option<(Uuid, Option<Uuid>, Option<Uuid>, String, i32)> = sqlx::query_as(
        "SELECT employee_id, branch_id, attendance_record_id, kind, minutes_away \
           FROM attendance_flags WHERE id = $1 AND org_id = $2 AND resolution IS NULL",
    )
    .bind(*id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let Some((employee_id, branch_id, record_id, kind, minutes)) = flag else {
        return Err(AppError::Coded {
            status: 404,
            code: "FLAG_HANDLED",
            reason: "That flag is already handled.".into(),
        });
    };
    let subject = access::subject(pool, org_id, employee_id).await?;
    // At the flag's branch; a flag with none (a new phone before any branch)
    // at one of the person's branches — never "anywhere" (RO-6).
    match branch_id {
        Some(b) => access::require_at(pool, &claims, org_id, Cap::HrAttendanceEdit, b).await?,
        None => access::require_for(pool, &claims, Cap::HrAttendanceEdit, &subject).await?,
    }
    // Nobody decides their own flag (RQ-5: "nobody approves their own request
    // any other way", E2E B-TEAM-1): excusing your own time away, ignoring
    // your own suspicious punch or confirming your own cover is someone
    // else's call. Signing your own phone out favours nobody, so it stays.
    if subject.is(&claims) && body.action != "revoke" {
        return Err(AppError::Coded {
            status: 403,
            code: "OWN_DECISION",
            reason: "Someone else has to decide this one.".into(),
        });
    }
    // Each act on its own right as well (PM-4): confirming a cover is the
    // cover-confirm right (as on the covers list), signing a phone out is the
    // staff-edit right (as on the employee). A deduction asks its own below.
    let own = match (body.action.as_str(), kind.as_str()) {
        ("confirm", "cover") => Some(Cap::HrShiftCoverConfirm),
        ("revoke", _) => Some(Cap::HrStaffEdit),
        _ => None,
    };
    if let Some(cap) = own {
        match branch_id {
            Some(b) => access::require_at(pool, &claims, org_id, cap, b).await?,
            None => access::require_for(pool, &claims, cap, &subject).await?,
        }
    }
    let date: Option<NaiveDate> = match record_id {
        Some(r) => {
            sqlx::query_scalar("SELECT business_date FROM attendance_records WHERE id = $1")
                .bind(r)
                .fetch_optional(pool)
                .await?
        }
        None => None,
    };
    // Confirming a cover from its flag confirms the COVER, not only the flag
    // (CV-5: it is paid once a manager confirms it, and it leaves Approvals).
    // (A cover flag always names its record; one without has no cover to
    // decide, and only the flag is resolved.)
    if let (true, Some(record)) = (body.action == "confirm" && kind == "cover", record_id) {
        decide_cover_record(pool, &claims, org_id, by, record, true).await?;
    }
    let (resolution, amount, reason) = match body.action.as_str() {
        "ignore" => ("ignored", 0, ""),
        "confirm" => ("confirmed", 0, ""),
        "excuse_paid" => ("excused_paid", 0, ""),
        // The exact minute pay, not the rounded suggestion (RU-12).
        "excuse_unpaid" => (
            "excused_unpaid",
            crate::costing::round_piastres(
                away_exact(pool, org_id, employee_id, record_id, minutes).await?,
            ),
            "Unpaid excuse",
        ),
        "deduct" => {
            let amount = body
                .amount_piastres
                .filter(|a| *a > 0)
                .ok_or_else(|| AppError::BadRequest("Type the amount to deduct.".into()))?;
            (
                "deducted",
                amount,
                body.reason
                    .as_deref()
                    .map(str::trim)
                    .filter(|r| !r.is_empty())
                    .unwrap_or("Left mid-shift"),
            )
        }
        "revoke" if kind == "new_phone" => {
            super::revoke_devices(pool, employee_id).await?;
            ("revoked", 0, "")
        }
        _ => return Err(AppError::BadRequest("Unknown action".into())),
    };
    let mut deduction_id: Option<Uuid> = None;
    if amount > 0 {
        // A deduction from a flag is a pay line like any other: nobody deducts
        // from themselves, and above the manager's limit it waits for the owner
        // (AD-5, audit B-8).
        if subject.is(&claims) {
            return Err(AppError::Coded {
                status: 403,
                code: "OWN_PAY_LINE",
                reason: "You can't add pay lines for yourself.".into(),
            });
        }
        if let Some(d) = date {
            crate::staff::period_lock::assert_open(pool, org_id, d, "this deduction").await?;
        }
        let at = access::decision_branch(pool, &claims, Cap::HrDeductionsCreate, &subject).await?;
        let mut ask = AuthzRequest::of(Cap::HrDeductionsCreate);
        ask.amount = Some(amount);
        let status = match crate::authz::require::decide_for(pool, by, &ask, at).await? {
            Decision::Allow => "approved",
            Decision::NeedsApproval(_) => "pending",
            Decision::Deny(_) => {
                return Err(crate::authz::require::denied(Cap::HrDeductionsCreate));
            }
        };
        // One name for unpaid excused time (orchestrator decision 2):
        // `excused_unpaid`, never the old `unpaid_excuse`.
        // The server's own words get a code; a manager's typed reason doesn't.
        let reason_code = match (resolution, reason) {
            ("excused_unpaid", _) => Some("unpaid_excuse"),
            ("deducted", "Left mid-shift") => Some("left_mid_shift"),
            _ => None,
        };
        let source = if resolution == "deducted" {
            "left_mid_shift"
        } else {
            "excused_unpaid"
        };
        // A manager's decision, not a rule's row: it is NOT keyed to the
        // attendance record (the flag keeps the link, `deduction_id`). Keyed,
        // it collided with the one automatic row per record and source — a
        // second flag on the same shift could not be deducted (409), and the
        // rules' recompute of that record deleted an unpaid excuse made here.
        deduction_id = Some(
            sqlx::query_scalar(
                "INSERT INTO payroll_deductions (org_id, employee_id, amount_piastres, reason, \
                    effective_date, source, attendance_record_id, created_by, status, reason_code) \
                 VALUES ($1, $2, $3, $4, COALESCE($5, CURRENT_DATE), $6, NULL, $7, $8, $9) \
                 RETURNING id",
            )
            .bind(org_id)
            .bind(employee_id)
            .bind(amount)
            .bind(reason)
            .bind(date)
            .bind(source)
            .bind(by)
            .bind(status)
            .bind(reason_code)
            .fetch_one(pool)
            .await?,
        );
        crate::staff::payroll::audit(
            pool,
            org_id,
            Some(by),
            "adjustment.create",
            "payroll_deductions",
            deduction_id,
            Some(employee_id),
            None,
            Some(reason),
            json!({ "kind": "deduction", "value_piastres": amount, "source": source,
                    "flag_id": *id, "status": status, "effective_date": date }),
        )
        .await?;
        if status == "approved" {
            notify(
                pool,
                org_id,
                employee_id,
                "staff.n_deduction_added",
                json!({ "reason": reason, "amount": amount }),
            )
            .await;
        } else {
            let who = subject.name.clone();
            let name = user_name(pool, by).await;
            for o in owners(pool, org_id).await? {
                notify(
                    pool,
                    org_id,
                    o,
                    "staff.n_adjustment_pending",
                    json!({ "name": who, "by": name, "amount": amount }),
                )
                .await;
            }
        }
    }
    sqlx::query(
        "UPDATE attendance_flags SET resolution = $2, resolved_by = $3, resolved_at = now(), \
                deduction_id = $4 WHERE id = $1",
    )
    .bind(*id)
    .bind(resolution)
    .bind(by)
    .bind(deduction_id)
    .execute(pool)
    .await?;
    let row: AttendanceFlag = sqlx::query_as(
        "SELECT f.id, f.employee_id, e.name AS employee_name, f.branch_id, f.attendance_record_id, \
                f.kind, f.minutes_away, f.detected_at, f.resolution, f.resolved_at, \
                f.deduction_id, fd.status AS deduction_status \
           FROM attendance_flags f JOIN employees e ON e.id = f.employee_id \
           LEFT JOIN payroll_deductions fd ON fd.id = f.deduction_id WHERE f.id = $1",
    )
    .bind(*id)
    .fetch_one(pool)
    .await?;
    Ok(HttpResponse::Ok().json(row))
}

// ── covers ─────────────────────────────────────────────────────────────────

#[derive(Serialize, ToSchema)]
pub struct CoverableShift {
    pub employee_id: Uuid,
    pub employee_name: String,
    pub branch_id: Uuid,
    pub work_shift_id: Uuid,
    pub shift_name: String,
    pub business_date: NaiveDate,
    pub scheduled_start_at: DateTime<Utc>,
    pub scheduled_end_at: DateTime<Utc>,
}

/// Rostered shifts at my branches whose owner is past grace without a punch,
/// until the shift ends (CV-1, CV-2), as of `now` (a cover queued offline is
/// judged at its own time). Yesterday's night shift still running after
/// midnight counts, on the day it started (SC-10). One roster function.
async fn coverable_for(
    pool: &PgPool,
    employee_id: Uuid,
    now: DateTime<Utc>,
) -> Result<Vec<CoverableShift>, AppError> {
    let mine = branches_of(pool, employee_id).await?;
    let colleagues: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT DISTINCT e.id, e.name FROM employee_branches a \
           JOIN employees e ON e.id = a.employee_id AND e.employment_status = 'active' \
          WHERE a.branch_id = ANY($1) AND e.id <> $2",
    )
    .bind(&mine)
    .bind(employee_id)
    .fetch_all(pool)
    .await?;
    let ids: Vec<Uuid> = colleagues.iter().map(|c| c.0).collect();
    let names: std::collections::HashMap<Uuid, String> = colleagues.into_iter().collect();
    // Today and yesterday in UTC dates bracket every branch's local today and
    // yesterday; the window test below keeps only shifts running now.
    let today = now.date_naive();
    let shifts = crate::staff::schedules::resolve_range(
        pool,
        &ids,
        today - chrono::Duration::days(2),
        today + chrono::Duration::days(1),
        None,
    )
    .await?;
    let mut out = Vec::new();
    for s in shifts {
        let Some(branch_id) = s.branch_id.filter(|b| mine.contains(b)) else {
            continue;
        };
        let past_grace =
            now > s.scheduled_start_at + chrono::Duration::minutes(i64::from(s.grace_minutes));
        if !past_grace || now >= s.scheduled_end_at {
            continue;
        }
        let punched: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM attendance_records \
              WHERE business_date = $1 AND work_shift_id = $2 \
                AND (employee_id = $3 OR covered_employee_id = $3) \
                AND (cover_status IS NULL OR cover_status <> 'rejected'))",
        )
        .bind(s.on_date)
        .bind(s.work_shift_id)
        .bind(s.employee_id)
        .fetch_one(pool)
        .await?;
        let on_leave: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM staff_requests WHERE employee_id = $1 \
               AND status = 'approved' AND kind IN ('leave', 'mission') \
               AND on_date <= $2 AND COALESCE(end_date, on_date) >= $2)",
        )
        .bind(s.employee_id)
        .bind(s.on_date)
        .fetch_one(pool)
        .await?;
        if !punched && !on_leave {
            out.push(CoverableShift {
                employee_id: s.employee_id,
                employee_name: names.get(&s.employee_id).cloned().unwrap_or_default(),
                branch_id,
                work_shift_id: s.work_shift_id,
                shift_name: s.name.clone(),
                business_date: s.on_date,
                scheduled_start_at: s.scheduled_start_at,
                scheduled_end_at: s.scheduled_end_at,
            });
        }
    }
    Ok(out)
}

#[utoipa::path(
    get, path = "/staff/me/coverable", tag = "staff",
    responses((status = 200, body = Vec<CoverableShift>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_coverable(me: Me, pool: crate::db::Db) -> Result<HttpResponse, AppError> {
    Ok(HttpResponse::Ok().json(coverable_for(pool.get_ref(), me.employee_id, Utc::now()).await?))
}

#[derive(Deserialize, ToSchema)]
pub struct OpenCover {
    /// Whose shift.
    pub employee_id: Uuid,
    pub work_shift_id: Uuid,
    #[serde(default)]
    pub latitude: Option<f64>,
    #[serde(default)]
    pub longitude: Option<f64>,
    /// The fix's reported accuracy, metres (CL-9).
    #[serde(default)]
    pub accuracy_meters: Option<f64>,
    /// The OS's mock-location marker for this fix (CL-9).
    #[serde(default)]
    pub is_mock: Option<bool>,
    /// Set when the cover was queued offline: it starts at its own time, not
    /// when the phone got a signal back (CL-11, audit 03 bug 6).
    #[serde(default)]
    pub offline: Option<super::clock::OfflineStamp>,
}

/// Open a colleague's missed shift as a cover: the same phone and geofence
/// checks as a clock-in; flagged for the manager; paid only once confirmed
/// (CV-1..CV-5, CV-7).
#[utoipa::path(
    post, path = "/staff/me/cover", tag = "staff", request_body = OpenCover,
    responses((status = 201, body = crate::staff::attendance::AttendanceRecord), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn open_cover(
    me: Me,
    pool: crate::db::Db,
    secret: web::Data<crate::auth::jwt::JwtSecret>,
    body: web::Json<OpenCover>,
) -> Result<HttpResponse, AppError> {
    let employee_id = me.employee_id;
    let org_id = me.org_id;
    let pool = pool.get_ref();
    crate::staff::attendance::require_rules(pool, org_id).await?;
    super::privacy::require_accepted(pool, &me).await?;
    let stamped = super::clock::rebuild(body.offline.as_ref(), Utc::now(), me.verifier(&secret))?;
    let shift = coverable_for(pool, employee_id, stamped.at)
        .await?
        .into_iter()
        .find(|c| c.employee_id == body.employee_id && c.work_shift_id == body.work_shift_id)
        .ok_or_else(|| AppError::Refused {
            code: "SHIFT_NOT_COVERABLE",
            reason: "That shift can't be covered now.".into(),
        })?;
    // Nothing is written into an approved or paid month (BC-3).
    crate::staff::period_lock::assert_open(pool, org_id, shift.business_date, "a cover").await?;
    // The same fence as a clock-in, always (CL-2).
    let distance = crate::staff::attendance::check_geofence(
        pool,
        shift.branch_id,
        body.latitude,
        body.longitude,
        true,
    )
    .await?;
    let open: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM attendance_records WHERE employee_id = $1 \
           AND check_in_at IS NOT NULL AND check_out_at IS NULL)",
    )
    .bind(employee_id)
    .fetch_one(pool)
    .await?;
    if open {
        return Err(AppError::Conflict(
            "Clock out of your own shift first.".into(),
        ));
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO attendance_records (org_id, employee_id, branch_id, work_shift_id, business_date, \
            status, scheduled_start_at, scheduled_end_at, check_in_at, check_in_latitude, \
            check_in_longitude, check_in_distance_meters, check_in_method, covered_employee_id, \
            cover_status, created_by) \
         VALUES ($1, $2, $3, $4, $5, 'present', $6, $7, $13, $8, $9, $10, 'cover', $11, \
            'pending', $12) RETURNING id",
    )
    .bind(org_id)
    .bind(employee_id)
    .bind(shift.branch_id)
    .bind(shift.work_shift_id)
    .bind(shift.business_date)
    .bind(shift.scheduled_start_at)
    .bind(shift.scheduled_end_at)
    .bind(body.latitude)
    .bind(body.longitude)
    .bind(distance)
    .bind(body.employee_id)
    .bind(me.user_id)
    .bind(stamped.at)
    .fetch_one(pool)
    .await?;
    if stamped.unverified {
        raise_flag(
            pool,
            org_id,
            employee_id,
            Some(shift.branch_id),
            Some(id),
            "time_unverified",
            0,
        )
        .await?;
    }
    check_punch_fix(
        pool,
        org_id,
        employee_id,
        shift.branch_id,
        id,
        body.accuracy_meters,
        body.is_mock,
    )
    .await?;
    let _ = sqlx::query(
        "INSERT INTO attendance_flags (org_id, employee_id, branch_id, attendance_record_id, kind) \
         VALUES ($1, $2, $3, $4, 'cover')",
    )
    .bind(org_id)
    .bind(employee_id)
    .bind(shift.branch_id)
    .bind(id)
    .execute(pool)
    .await?;
    let name = employee_name(pool, employee_id).await;
    notify_managers(
        pool,
        org_id,
        Some(shift.branch_id),
        Cap::HrShiftCoverConfirm,
        Some(employee_id),
        "staff.n_cover",
        json!({ "name": name, "owner": shift.employee_name }),
    )
    .await;
    let record = crate::staff::attendance::load_record(pool, org_id, id).await?;
    Ok(HttpResponse::Created().json(record))
}

#[derive(Deserialize, ToSchema)]
pub struct Decide {
    pub approve: bool,
}

/// Decide one pending cover (CV-5): confirmed pays it, rejected pays nothing.
/// The one place a cover is decided — the covers list (`decide_cover`) and
/// the cover's own flag on the Team board (`resolve_flag` "confirm") both call
/// it, so either settles the record AND its flag (CV-3).
async fn decide_cover_record(
    pool: &PgPool,
    claims: &crate::auth::jwt::Claims,
    org_id: Uuid,
    by: Uuid,
    id: Uuid,
    approve: bool,
) -> Result<(), AppError> {
    // The two people involved, and their accounts (if any).
    #[allow(clippy::type_complexity)]
    let row: Option<(Uuid, Uuid, Option<Uuid>, Option<Uuid>, Option<Uuid>, NaiveDate)> = sqlx::query_as(
        "SELECT a.employee_id, a.branch_id, a.covered_employee_id, c.user_id, o.user_id, a.business_date \
           FROM attendance_records a \
           JOIN employees c ON c.id = a.employee_id \
           LEFT JOIN employees o ON o.id = a.covered_employee_id \
          WHERE a.id = $1 AND a.org_id = $2 AND a.cover_status = 'pending'",
    )
    .bind(id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let Some((coverer, branch_id, _owner, coverer_user, owner_user, day)) = row else {
        return Err(AppError::NotFound("No cover waiting here.".into()));
    };
    access::require_at(pool, &claims, org_id, Cap::HrShiftCoverConfirm, branch_id).await?;
    if Some(by) == coverer_user || Some(by) == owner_user {
        return Err(AppError::Coded {
            status: 403,
            code: "OWN_COVER",
            reason: "You can't confirm a cover you're part of.".into(),
        });
    }
    // Confirming pays the cover: never into an approved or paid month
    // (AD-10). Rejecting pays nothing, so it may still be recorded. After the
    // rights checks (AT-11: someone at another branch hears 403, not 409).
    if approve {
        crate::staff::period_lock::assert_open(pool, org_id, day, "this cover").await?;
    }
    let status = if approve { "confirmed" } else { "rejected" };
    // One decision only: a second confirm (or a race) finds nothing pending.
    let decided = sqlx::query(
        "UPDATE attendance_records SET cover_status = $2, edited_by = $3 \
          WHERE id = $1 AND cover_status = 'pending'",
    )
    .bind(id)
    .bind(status)
    .bind(by)
    .execute(pool)
    .await?
    .rows_affected();
    if decided == 0 {
        return Err(AppError::Conflict("That cover was already decided.".into()));
    }
    // A money act: who, when (AT-10, D8).
    crate::staff::payroll::audit(
        pool,
        org_id,
        Some(by),
        "cover.decide",
        "attendance_records",
        Some(id),
        Some(coverer),
        None,
        None,
        json!({ "approve": approve, "date": day }),
    )
    .await?;
    // The flag says what was decided (CV-3).
    sqlx::query(
        "UPDATE attendance_flags SET resolution = $3, resolved_by = $2, resolved_at = now() \
          WHERE attendance_record_id = $1 AND kind = 'cover' AND resolution IS NULL",
    )
    .bind(id)
    .bind(by)
    .bind(status)
    .execute(pool)
    .await?;
    notify(
        pool,
        org_id,
        coverer,
        if approve {
            "staff.n_cover_confirmed"
        } else {
            "staff.n_cover_rejected"
        },
        json!({}),
    )
    .await;
    Ok(())
}

/// Confirm or reject a cover. Rejecting pays nothing (CV-5); the confirmer is
/// neither person involved.
#[utoipa::path(
    patch, path = "/staff/attendance/{id}/cover", tag = "staff", request_body = Decide,
    params(("id" = Uuid, Path)),
    responses((status = 200, body = crate::staff::attendance::AttendanceRecord), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn decide_cover(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<Decide>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let by = claims.user_id_safe()?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrShiftCoverConfirm).await?;
    decide_cover_record(pool, &claims, org_id, by, *id, body.approve).await?;
    let record = crate::staff::attendance::load_record(pool, org_id, *id).await?;
    Ok(HttpResponse::Ok().json(record))
}

/// Approve or reject a shift's overtime, within the approver's money limit.
#[utoipa::path(
    patch, path = "/staff/attendance/{id}/overtime", tag = "staff", request_body = Decide,
    params(("id" = Uuid, Path)),
    responses((status = 200, body = crate::staff::attendance::AttendanceRecord), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn decide_overtime(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<Decide>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let by = claims.user_id_safe()?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrOvertimeApprove).await?;
    let row: Option<(Uuid, Uuid, i32, NaiveDate)> = sqlx::query_as(
        "SELECT a.employee_id, a.branch_id, a.overtime_minutes, a.business_date \
           FROM attendance_records a \
          WHERE a.id = $1 AND a.org_id = $2 AND a.overtime_status = 'pending'",
    )
    .bind(*id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let Some((employee_id, branch_id, minutes, on_date)) = row else {
        return Err(AppError::NotFound("No overtime waiting here.".into()));
    };
    let subject = access::subject(pool, org_id, employee_id).await?;
    if subject.is(&claims) {
        return Err(AppError::Coded {
            status: 403,
            code: "OWN_OVERTIME",
            reason: "You can't approve your own overtime.".into(),
        });
    }
    access::require_at(pool, &claims, org_id, Cap::HrOvertimeApprove, branch_id).await?;
    if body.approve {
        // An approved month is a snapshot: approving would pay into it
        // (AD-10). Rejecting moves no money, so a pending overtime in a paid
        // month can still leave Approvals (minor default M32); the fix is a
        // line in next month.
        crate::staff::period_lock::assert_open(pool, org_id, on_date, "this overtime").await?;
        // Priced exactly as payroll will price it (AT-9): the same facts
        // (the salary in force that day, the day's rostered minutes, the
        // night minutes) through the same function under the branch's rules
        // and the shift's own rates (RU-8), as if already approved.
        let settings = load_settings(pool, org_id, Some(branch_id)).await?;
        let day = crate::staff::penalties::load_facts(pool, *id, &settings)
            .await?
            .ok_or_else(|| AppError::NotFound("No overtime waiting here.".into()))?;
        let mut facts = day.facts.clone();
        facts.overtime_status = Some("approved".into());
        let amount = crate::staff::pricing::price_shift(&facts, &day.rules).overtime_piastres;
        let mut ask = AuthzRequest::of(Cap::HrOvertimeApprove);
        ask.amount = Some(amount);
        let pending = crate::authz::Pending {
            request: ask,
            subject_id: subject.authz_key(),
            requested_by: subject.authz_key(),
            why: crate::authz::Why::NotHeld,
        };
        crate::authz::require::settle(pool, by, &pending, Some(branch_id)).await?;
    }
    sqlx::query("UPDATE attendance_records SET overtime_status = $2, edited_by = $3 WHERE id = $1")
        .bind(*id)
        .bind(if body.approve { "approved" } else { "rejected" })
        .bind(by)
        .execute(pool)
        .await?;
    // A money act: who, when (AT-10, D8).
    crate::staff::payroll::audit(
        pool,
        org_id,
        Some(by),
        "overtime.decide",
        "attendance_records",
        Some(*id),
        Some(employee_id),
        None,
        None,
        json!({ "approve": body.approve, "minutes": minutes, "date": on_date }),
    )
    .await?;
    notify(
        pool,
        org_id,
        employee_id,
        if body.approve {
            "staff.n_overtime_approved"
        } else {
            "staff.n_overtime_rejected"
        },
        json!({ "minutes": minutes }),
    )
    .await;
    let record = crate::staff::attendance::load_record(pool, org_id, *id).await?;
    Ok(HttpResponse::Ok().json(record))
}

// ── punch for someone ─────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct PunchFor {
    pub employee_id: Uuid,
    /// Required (CL-13): a dead phone, a forgotten one. Missing reads as
    /// blank, so the answer is "A reason is required." (Mac E2E BC-4).
    #[serde(default)]
    pub reason: String,
    /// Set when the manager's phone queued the punch offline: it is dated at
    /// its own time, not when the phone got a signal back (audit 03 bug 6).
    #[serde(default)]
    pub offline: Option<super::clock::OfflineStamp>,
}

/// Clock someone in, or out if they are in; marked as made by the manager
/// (`manager`) with the reason (CL-13, CL-16). The check-in window, the
/// night shift's business date and the shift's own branch apply exactly as
/// for the app.
#[utoipa::path(
    post, path = "/staff/attendance/punch", tag = "staff", request_body = PunchFor,
    responses((status = 200, body = crate::staff::attendance::AttendanceRecord), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn punch_for(
    req: HttpRequest,
    pool: crate::db::Db,
    secret: web::Data<crate::auth::jwt::JwtSecret>,
    body: web::Json<PunchFor>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let by = claims.user_id_safe()?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrAttendancePunchOthers).await?;
    let reason = body.reason.trim();
    if reason.is_empty() {
        return Err(AppError::BadRequest("A reason is required.".into()));
    }
    let subject = access::subject(pool, org_id, body.employee_id).await?;
    if subject.employment_status != "active" {
        return Err(AppError::CodedVars {
            status: 403,
            code: "EMPLOYMENT_NOT_ACTIVE",
            reason: "That person isn't an active employee.".into(),
            vars: json!({ "status": subject.employment_status }),
        });
    }
    // Nobody punches for themselves: their own phone or the till does that.
    if subject.is(&claims) {
        return Err(AppError::Coded {
            status: 403,
            code: "OWN_PUNCH",
            reason: "Punch yourself in from your own phone.".into(),
        });
    }
    crate::staff::attendance::require_rules(pool, org_id).await?;
    if subject.branches.is_empty() {
        return Err(AppError::BadRequest("That person has no branch.".into()));
    }
    access::require_for(pool, &claims, Cap::HrAttendancePunchOthers, &subject).await?;
    // Only the manager's own phone can vouch for a queued time; from the
    // dashboard an offline stamp is dated but doubted.
    let verifier = super::clock::Verifier {
        secret: &secret,
        device: crate::staff::principal::staff_principal(&req).map(|p| p.device_id),
    };
    let stamped = super::clock::rebuild(body.offline.as_ref(), Utc::now(), verifier)?;
    // At the branch the caller runs (one of theirs); a rostered shift's own
    // branch wins when the caller holds the right there too.
    let fallback = access::decision_branch(pool, &claims, Cap::HrAttendancePunchOthers, &subject)
        .await?
        .ok_or_else(|| AppError::BadRequest("That person has no branch.".into()))?;
    let branch = match shift_branch_at(pool, body.employee_id, fallback, stamped.at).await? {
        Some(b) if b != fallback => {
            access::require_at(pool, &claims, org_id, Cap::HrAttendancePunchOthers, b).await?;
            b
        }
        _ => fallback,
    };

    let (id, _) = punch(
        pool,
        org_id,
        body.employee_id,
        branch,
        "manager",
        reason,
        Some(by),
        stamped,
    )
    .await?;
    notify(
        pool,
        org_id,
        body.employee_id,
        "staff.n_punched_for_you",
        json!({ "name": user_name(pool, by).await, "reason": reason }),
    )
    .await;
    let record = crate::staff::attendance::load_record(pool, org_id, id).await?;
    Ok(HttpResponse::Ok().json(record))
}

/// The branch of the rostered shift a punch at `at` would open, when the
/// shift template names one.
async fn shift_branch_at(
    pool: &PgPool,
    employee_id: Uuid,
    near: Uuid,
    at: DateTime<Utc>,
) -> Result<Option<Uuid>, AppError> {
    let tz = crate::staff::branch_timezone(pool, near).await?;
    let today = crate::staff::attendance::day_in(pool, at, &tz).await?;
    let (shift, _) =
        crate::staff::attendance::resolve_punch_shift(pool, employee_id, today, &tz, at).await?;
    let Some(shift) = shift else {
        return Ok(None);
    };
    Ok(sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT ws.branch_id FROM work_shifts ws \
           JOIN employee_branches eb ON eb.branch_id = ws.branch_id AND eb.employee_id = $2 \
          WHERE ws.id = $1",
    )
    .bind(shift.work_shift_id)
    .bind(employee_id)
    .fetch_optional(pool)
    .await?
    .flatten())
}

/// Clock `employee_id` in at `branch` at the stamped moment, or out if they
/// are in; `method` says how (CL-13, CL-16), `by` the user who did it for
/// them. The check-in lands on the business date of the shift it belongs to
/// (a night shift after midnight is yesterday's, SC-10) and is refused before
/// that shift's window opens (CL-3). Returns the record and whether this was
/// a check-in.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn punch(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
    branch: Uuid,
    method: &str,
    reason: &str,
    by: Option<Uuid>,
    stamped: super::clock::Stamped,
) -> Result<(Uuid, bool), AppError> {
    let at = stamped.at;
    let open: Option<(Uuid, Uuid, DateTime<Utc>, NaiveDate)> = sqlx::query_as(
        "SELECT id, branch_id, check_in_at, business_date FROM attendance_records \
          WHERE employee_id = $1 AND check_in_at IS NOT NULL AND check_in_at <= $2 \
            AND check_out_at IS NULL ORDER BY check_in_at DESC LIMIT 1",
    )
    .bind(employee_id)
    .bind(at)
    .fetch_optional(pool)
    .await?;
    let (id, checked_in, flag_branch) = match open {
        Some((id, record_branch, _, day)) => {
            // Nothing is written into an approved or paid month (BC-3).
            crate::staff::period_lock::assert_open(pool, org_id, day, "a punch").await?;
            // The out-reason beside the in-reason, never over it (BC-1).
            sqlx::query(
                "UPDATE attendance_records SET check_out_at = $5, check_out_method = $4, \
                        check_out_reason = $2, edited_by = $3 WHERE id = $1",
            )
            .bind(id)
            .bind(reason)
            .bind(by)
            .bind(method)
            .bind(at)
            .execute(pool)
            .await?;
            (id, false, record_branch)
        }
        None => {
            let tz = crate::staff::branch_timezone(pool, branch).await?;
            let today = crate::staff::attendance::day_in(pool, at, &tz).await?;
            let (shift, business_date) =
                crate::staff::attendance::resolve_punch_shift(pool, employee_id, today, &tz, at)
                    .await?;
            crate::staff::attendance::check_window(shift.as_ref(), at)?;
            crate::staff::period_lock::assert_open(pool, org_id, business_date, "a punch").await?;
            // A colleague is covering it: never paid twice (D1).
            crate::staff::attendance::refuse_if_covered(
                pool,
                employee_id,
                business_date,
                shift.as_ref().map(|s| s.work_shift_id),
            )
            .await?;
            let id = sqlx::query_scalar(
                "INSERT INTO attendance_records (org_id, employee_id, branch_id, work_shift_id, \
                    business_date, status, scheduled_start_at, scheduled_end_at, check_in_at, \
                    check_in_method, is_manual, punch_reason, created_by) \
                 VALUES ($1, $2, $3, $4, $5, 'present', $6, $7, $12, $10, $11, $8, $9) \
                 ON CONFLICT (employee_id, business_date, \
                    COALESCE(work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid)) WHERE covered_employee_id IS NULL \
                 DO NOTHING RETURNING id",
            )
            .bind(org_id)
            .bind(employee_id)
            .bind(branch)
            .bind(shift.as_ref().map(|s| s.work_shift_id))
            .bind(business_date)
            .bind(shift.as_ref().map(|s| s.scheduled_start_at))
            .bind(shift.as_ref().map(|s| s.scheduled_end_at))
            .bind(reason)
            .bind(by)
            .bind(method)
            .bind(method == "manager")
            .bind(at)
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| AppError::Conflict("They already worked that shift today.".into()))?;
            (id, true, branch)
        }
    };
    // Derive lateness, worked time and penalties like any punch.
    crate::staff::attendance::apply_punch_correction(
        pool, org_id, id, None, None, None, None, reason, by,
    )
    .await?;
    if stamped.unverified {
        raise_flag(
            pool,
            org_id,
            employee_id,
            Some(flag_branch),
            Some(id),
            "time_unverified",
            0,
        )
        .await?;
    }
    Ok((id, checked_in))
}

#[derive(Deserialize, ToSchema)]
pub struct TillPunch {
    pub branch_id: Uuid,
    /// The person's own till PIN.
    pub pin: String,
}

#[derive(Serialize, ToSchema)]
pub struct TillPunchResult {
    /// The employee the PIN's owner is.
    pub employee_id: Uuid,
    pub name: String,
    /// `in` · `out`
    pub punched: String,
    pub record: crate::staff::attendance::AttendanceRecord,
}

fn till_only(reason: &str) -> AppError {
    AppError::Coded {
        status: 403,
        code: "TILL_ONLY",
        reason: reason.into(),
    }
}

/// A dead or forgotten phone in a Madar org: the person clocks in or out on
/// the branch till with their till PIN (CL-13). Marked `till` (CL-16). The
/// till is at the branch, so there is no geofence to check — which is why
/// it is accepted ONLY from a real till (audit 03 P0): a POS session (never
/// the Dawam app's) on the branch's registered POS device, proven by its
/// credential when it has one, with a till session open on that device at
/// that branch. Online only: a PIN is never queued. Wrong PINs slow down
/// like the till's own sign-in, and the branch's managers are told.
#[utoipa::path(
    post, path = "/staff/attendance/till-punch", tag = "staff", request_body = TillPunch,
    params(
        ("X-Madar-Device" = String, Header, description = "The till's device id"),
        ("X-Madar-Device-Token" = Option<String>, Header, description = "The device credential, when it has one"),
    ),
    responses((status = 200, body = TillPunchResult), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn till_punch(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<TillPunch>,
) -> Result<HttpResponse, AppError> {
    // The staff app can never punch as the till, whoever is signed in on it.
    if crate::staff::principal::staff_principal(&req).is_some() {
        return Err(till_only("Till punches are made on the branch till."));
    }
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    crate::authz::scope::org_read_branches(pool, &claims, org_id, Some(body.branch_id)).await?;
    let branch_ok: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM branches WHERE id = $1 AND org_id = $2 AND deleted_at IS NULL)",
    )
    .bind(body.branch_id)
    .bind(org_id)
    .fetch_one(pool)
    .await?;
    if !branch_ok {
        return Err(AppError::NotFound("Branch not found".into()));
    }
    // Either module off: 403 MODULE_OFF with the spec's sentence (P-010,
    // PS-7), so the till words it instead of "no permission" (B-POS-1).
    for m in ["pos", "dawam"] {
        if !super::roster::has_module(pool, org_id, m).await? {
            return Err(AppError::CodedVars {
                status: 403,
                code: "MODULE_OFF",
                reason: "Till punches need both POS and Dawam switched on.".into(),
                vars: serde_json::json!({ "module": m }),
            });
        }
    }
    // The branch's own registered POS device, proven when it can be.
    let device = crate::devices::DeviceHeader::from_request_headers(&req)
        .ok_or_else(|| till_only("Till punches are made on the branch till."))?;
    let row: Option<(Option<Uuid>, Option<String>)> = sqlx::query_as(
        "SELECT branch_id, credential_hash FROM devices \
          WHERE id = $1 AND org_id = $2 AND retired_at IS NULL AND kind = 'pos'",
    )
    .bind(device)
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let Some((device_branch, credential)) = row else {
        return Err(till_only("This device isn't a till of this business."));
    };
    if device_branch != Some(body.branch_id) {
        return Err(till_only("This till belongs to another branch."));
    }
    if credential.is_some() {
        let token = req
            .headers()
            .get(crate::devices::activation::DEVICE_TOKEN_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if !crate::devices::activation::verify_credential(pool, device, token).await? {
            return Err(till_only(
                "This till could not prove it is the branch's till.",
            ));
        }
    }
    let till_open: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM tills \
          WHERE branch_id = $1 AND device_id = $2 AND status = 'open')",
    )
    .bind(body.branch_id)
    .bind(device)
    .fetch_one(pool)
    .await?;
    if !till_open {
        return Err(AppError::Coded {
            status: 409,
            code: "NO_TILL_SESSION",
            reason: "Open the till first, then punch with your PIN.".into(),
        });
    }
    crate::staff::attendance::require_rules(pool, org_id).await?;
    let device_key = device.to_string();
    // The same growing delay as the till's own sign-in: a colleague's PIN
    // can't be guessed at speed.
    crate::auth::pin_throttle::check(pool, Some(&device_key), body.branch_id).await?;
    let holder = match crate::auth::handlers::find_pin_holder_by_pin(pool, org_id, body.pin.trim())
        .await?
    {
        Some(Ok(u)) => u,
        Some(Err(())) => {
            return Err(AppError::Refused {
                code: "PIN_NOT_UNIQUE",
                reason: "This PIN belongs to more than one person. Ask a manager for a new PIN."
                    .into(),
            });
        }
        None => {
            crate::auth::pin_throttle::record_failure(pool, Some(&device_key), body.branch_id)
                .await;
            return Err(AppError::Unauthorized("Wrong PIN".into()));
        }
    };
    // Deliberately NOT cleared on a right PIN: someone guessing a colleague's
    // PIN could otherwise reset the count with their own between guesses.
    // The PIN verified: upgrade a legacy hash and stamp the fingerprint, as
    // sign-in does, so this holder leaves the wrong-PIN scan (B-POS-3).
    crate::auth::handlers::upgrade_verified_pin(
        pool,
        org_id,
        holder.id,
        body.pin.trim(),
        holder.pin_hash.as_deref(),
    )
    .await;
    // The PIN names a till user; the punch is for the employee linked to them.
    let employee: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM employees WHERE user_id = $1 AND org_id = $2")
            .bind(holder.id)
            .bind(org_id)
            .fetch_optional(pool)
            .await?;
    let Some(employee_id) = employee else {
        return Err(AppError::Coded {
            status: 403,
            code: "NOT_AN_EMPLOYEE",
            reason: format!("{} isn't set up as an employee in Dawam.", holder.name),
        });
    };
    require_active_employee(pool, employee_id).await?;
    let (id, checked_in) = punch(
        pool,
        org_id,
        employee_id,
        body.branch_id,
        "till",
        "Till PIN",
        None,
        super::clock::Stamped {
            at: Utc::now(),
            offline: false,
            unverified: false,
        },
    )
    .await?;
    let record = crate::staff::attendance::load_record(pool, org_id, id).await?;
    let name = employee_name(pool, employee_id).await;
    // A punch without the phone's fence: the branch's managers hear of it.
    notify_managers(
        pool,
        org_id,
        Some(record.branch_id),
        Cap::HrAttendanceEdit,
        Some(employee_id),
        if checked_in {
            "staff.n_till_punch_in"
        } else {
            "staff.n_till_punch_out"
        },
        json!({ "name": name }),
    )
    .await;
    Ok(HttpResponse::Ok().json(TillPunchResult {
        employee_id,
        name,
        punched: if checked_in { "in" } else { "out" }.into(),
        record,
    }))
}

/// Sign a person's phone out now (RO-4): the device, every staff token minted
/// for it, and its pushes.
#[utoipa::path(
    delete, path = "/staff/employees/{employee_id}/device", tag = "staff",
    params(("employee_id" = Uuid, Path)),
    responses((status = 204), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn revoke_device(
    req: HttpRequest,
    pool: crate::db::Db,
    employee_id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    access::gate(pool.get_ref(), &claims, org_id, Cap::HrStaffEdit).await?;
    let subject = access::subject(pool.get_ref(), org_id, *employee_id).await?;
    access::require_for(pool.get_ref(), &claims, Cap::HrStaffEdit, &subject).await?;
    super::revoke_devices(pool.get_ref(), *employee_id).await?;
    Ok(HttpResponse::NoContent().finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn t(m: i64) -> DateTime<Utc> {
        "2026-09-22T08:00:00Z".parse::<DateTime<Utc>>().unwrap() + Duration::minutes(m)
    }

    #[test]
    fn time_away_is_every_outside_run_less_the_excused_part() {
        // In, out for 75 minutes, back, out again for 30.
        let pings = [
            (t(0), true),
            (t(15), false),
            (t(30), false),
            (t(90), true),
            (t(105), false),
        ];
        assert_eq!(away_minutes(&pings, t(135), &[], None), 75 + 30);
        // An excuse over 20 of those minutes.
        assert_eq!(away_minutes(&pings, t(135), &[(t(40), t(60))], None), 85);
        // Since a handled flag: only what came after it.
        assert_eq!(away_minutes(&pings, t(135), &[], Some(t(90))), 30);
        // Never counted past what is excused.
        assert_eq!(away_minutes(&pings, t(135), &[(t(0), t(200))], None), 0);
    }

    fn p(m: i64, lat: f64, acc: f64) -> PriorPing {
        PriorPing {
            at: t(m),
            latitude: Some(lat),
            longitude: Some(31.0),
            accuracy_meters: Some(acc),
            inside: true,
        }
    }

    #[test]
    fn spoofing_needs_more_than_one_repeat_and_ignores_ios_steps() {
        let here = (30.0, 31.0);
        // One repeated fix (an OS cache) is not suspicious; three in a row is.
        assert!(!spoof_signals(
            here,
            Some(12.0),
            None,
            t(30),
            &[p(15, 30.0, 11.0), p(0, 30.1, 9.0)]
        ));
        assert!(spoof_signals(
            here,
            Some(12.0),
            None,
            t(30),
            &[p(15, 30.0, 11.0), p(0, 30.0, 9.0)]
        ));
        // iOS's 65 m step four times running is honest; 7.3 m is frozen.
        let steps = [p(45, 30.01, 65.0), p(30, 30.02, 65.0), p(15, 30.03, 65.0)];
        assert!(!spoof_signals(here, Some(65.0), None, t(60), &steps));
        let frozen = [p(45, 30.01, 7.3), p(30, 30.02, 7.3), p(15, 30.03, 7.3)];
        assert!(spoof_signals(here, Some(7.3), None, t(60), &frozen));
        // A perfect accuracy, the OS's own marker, or a jet.
        assert!(spoof_signals(here, Some(0.0), None, t(0), &[]));
        assert!(spoof_signals(here, Some(8.0), Some(true), t(0), &[]));
        assert!(spoof_signals(
            (31.0, 31.0),
            Some(8.0),
            None,
            t(16),
            &[p(15, 30.0, 9.0)]
        ));
    }

    #[test]
    fn the_suggestion_rounds_halves_away_from_zero() {
        assert_eq!(nearest_five_pounds(Decimal::from(750)), 1000);
        assert_eq!(nearest_five_pounds(Decimal::from(1250)), 1500);
        assert_eq!(nearest_five_pounds(Decimal::from(749)), 500);
        assert_eq!(nearest_five_pounds(Decimal::from(0)), 0);
    }
}
