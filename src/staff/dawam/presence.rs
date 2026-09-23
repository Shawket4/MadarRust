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

use super::{branches_of, employee_name, notify, notify_managers, owners, user_name};
use crate::authz::{Cap, Decision, Request as AuthzRequest};
use crate::errors::{AppError, AppErrorResponse};
use crate::geo::osrm::{LatLng, haversine_meters};
use crate::staff::access;
use crate::staff::attendance::{AttendanceSettings, load_settings, require_active_employee};
use crate::staff::principal::{Me, caller};
use crate::staff::rules::PayRates;

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
}

/// At or under this, the employee is told to charge and silence reads "phone
/// likely died" (CL-12).
pub(crate) const LOW_BATTERY: i16 = 15;

/// Opens a flag once per shift and kind, and tells the managers.
pub(crate) async fn raise_flag(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
    branch_id: Option<Uuid>,
    record_id: Option<Uuid>,
    kind: &str,
    minutes_away: i32,
) -> Result<(), AppError> {
    let inserted = sqlx::query(
        "INSERT INTO attendance_flags (org_id, employee_id, branch_id, attendance_record_id, kind, minutes_away) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (attendance_record_id, kind) WHERE resolution IS NULL AND attendance_record_id IS NOT NULL \
         DO UPDATE SET minutes_away = GREATEST(attendance_flags.minutes_away, EXCLUDED.minutes_away)",
    )
    .bind(org_id)
    .bind(employee_id)
    .bind(branch_id)
    .bind(record_id)
    .bind(kind)
    .bind(minutes_away)
    .execute(pool)
    .await?;
    if inserted.rows_affected() > 0 {
        let name = employee_name(pool, employee_id).await;
        notify_managers(
            pool,
            org_id,
            branch_id,
            Cap::HrAttendanceEdit,
            Some(employee_id),
            &format!("staff.n_flag_{kind}"),
            json!({ "name": name, "minutes": minutes_away }),
        )
        .await;
    }
    Ok(())
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

#[derive(sqlx::FromRow)]
struct PriorPing {
    at: DateTime<Utc>,
    latitude: Option<f64>,
    longitude: Option<f64>,
    accuracy_meters: Option<f64>,
    inside: bool,
}

/// A location every 15 minutes between clock-in and clock-out (CL-4, CL-17).
#[utoipa::path(
    post, path = "/staff/me/pings", tag = "staff", request_body = PingRequest,
    responses((status = 200, body = PingResult), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn ping(
    me: Me,
    pool: crate::db::Db,
    body: web::Json<PingRequest>,
) -> Result<HttpResponse, AppError> {
    // From the employee's live phone only (CL-1, checked by StaffAuth).
    let employee_id = me.employee_id;
    let org_id = me.org_id;
    let pool = pool.get_ref();

    let now = super::clock::rebuild(body.offline.as_ref(), Utc::now())?.at;
    // Only while clocked in: location is never collected off shift (CL-17). A
    // queued ping belongs to the record that was open at ITS time.
    let open: Option<(Uuid, Uuid, NaiveDate)> = sqlx::query_as(
        "SELECT id, branch_id, business_date FROM attendance_records \
          WHERE employee_id = $1 AND check_in_at IS NOT NULL AND check_in_at <= $2 \
            AND (check_out_at IS NULL OR check_out_at >= $2) \
          ORDER BY check_in_at DESC LIMIT 1",
    )
    .bind(employee_id)
    .bind(now)
    .fetch_optional(pool)
    .await?;
    let Some((record_id, branch_id, business_date)) = open else {
        return Err(AppError::Conflict("You are not clocked in.".into()));
    };
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
          WHERE attendance_record_id = $1 AND at <= $2 ORDER BY at DESC LIMIT 3",
    )
    .bind(record_id)
    .bind(now)
    .fetch_all(pool)
    .await?;
    sqlx::query(
        "INSERT INTO attendance_pings (org_id, employee_id, attendance_record_id, at, latitude, \
            longitude, accuracy_meters, distance_meters, inside, is_mock, battery_percent) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
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
    .execute(pool)
    .await?;

    let mut flags = Vec::new();

    // Left mid-shift: two outside in a row, not covered by an approved excuse,
    // early departure or mission (CL-6). Nothing is charged here.
    let streak = 1 + prior.iter().take_while(|p| !p.inside).count();
    if !inside && streak >= OUTSIDE_STREAK && !excused_now(pool, employee_id, business_date).await?
    {
        let first_out = prior
            .iter()
            .take_while(|p| !p.inside)
            .last()
            .map_or(now, |p| p.at);
        let minutes = ((now - first_out).num_minutes().max(0) as i32).max(15);
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

    // Spoofing (CL-8, CL-9): real GPS always drifts, so identical coordinates,
    // a perfect or frozen accuracy, the OS's mock marker, or an impossible
    // speed all raise the same flag.
    let last = prior.first();
    let identical = last
        .is_some_and(|p| p.latitude == Some(body.latitude) && p.longitude == Some(body.longitude));
    let frozen_accuracy = body.accuracy_meters.is_some_and(|a| {
        a <= 0.0 || (prior.len() >= 2 && prior.iter().take(2).all(|p| p.accuracy_meters == Some(a)))
    });
    let too_fast = last.is_some_and(|p| match (p.latitude, p.longitude) {
        (Some(lat), Some(lng)) => {
            let secs = (now - p.at).num_seconds().max(1) as f64;
            haversine_meters(LatLng { lat, lng }, here) / secs > MAX_SPEED_MPS
        }
        _ => false,
    });
    if body.is_mock == Some(true) || identical || frozen_accuracy || too_fast {
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

/// An approved excuse, early departure or mission covers being away now.
async fn excused_now(pool: &PgPool, employee_id: Uuid, date: NaiveDate) -> Result<bool, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM staff_requests \
          WHERE employee_id = $1 AND status = 'approved' \
            AND kind IN ('excuse', 'early_departure', 'mission') \
            AND on_date <= $2 AND COALESCE(end_date, on_date) >= $2)",
    )
    .bind(employee_id)
    .bind(date)
    .fetch_one(pool)
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
                f.kind, f.minutes_away, f.detected_at, f.resolution, f.resolved_at \
           FROM attendance_flags f JOIN employees e ON e.id = f.employee_id \
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

/// Minutes away at the person's minute rate, to the nearest 5 EGP (CL-7, RU-12).
async fn away_cost(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
    record_id: Option<Uuid>,
    minutes: i32,
) -> Result<i64, AppError> {
    let salary: i64 =
        sqlx::query_scalar("SELECT base_salary_piastres FROM employees WHERE id = $1")
            .bind(employee_id)
            .fetch_optional(pool)
            .await?
            .unwrap_or(0);
    let scheduled: Option<i32> = match record_id {
        Some(id) => sqlx::query_scalar(
            "SELECT (EXTRACT(EPOCH FROM (scheduled_end_at - scheduled_start_at)) / 60)::int \
               FROM attendance_records WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(pool)
        .await?
        .flatten(),
        None => None,
    };
    let settings = load_settings(pool, org_id, None).await?;
    let rates = PayRates::from_base(
        salary,
        settings.working_days_per_month,
        i64::from(scheduled.unwrap_or(480).max(1)),
    );
    let exact = rates.minutes_piastres(Decimal::from(minutes.max(0)));
    // Nearest 500 piastres, halves away from zero.
    let fives = (exact / Decimal::from(500)).round();
    Ok((fives * Decimal::from(500)).try_into().unwrap_or(0))
}

#[derive(Deserialize, ToSchema)]
pub struct ResolveFlag {
    /// `ignore` · `excuse_paid` · `excuse_unpaid` · `deduct` · `revoke` (a new
    /// phone) · `confirm`
    pub action: String,
    /// For `deduct`: the amount the manager typed (CL-7).
    #[serde(default)]
    pub amount_piastres: Option<i64>,
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
        return Err(AppError::NotFound("That flag is already handled.".into()));
    };
    let subject = access::subject(pool, org_id, employee_id).await?;
    // At the flag's branch; a flag with none (a new phone before any branch)
    // at one of the person's branches — never "anywhere" (RO-6).
    match branch_id {
        Some(b) => access::require_at(pool, &claims, org_id, Cap::HrAttendanceEdit, b).await?,
        None => access::require_for(pool, &claims, Cap::HrAttendanceEdit, &subject).await?,
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
    let (resolution, amount, reason) = match body.action.as_str() {
        "ignore" => ("ignored", 0, ""),
        "confirm" => ("confirmed", 0, ""),
        "excuse_paid" => ("excused_paid", 0, ""),
        "excuse_unpaid" => (
            "excused_unpaid",
            away_cost(pool, org_id, employee_id, record_id, minutes).await?,
            "Unpaid excuse",
        ),
        "deduct" => {
            let amount = body
                .amount_piastres
                .filter(|a| *a > 0)
                .ok_or_else(|| AppError::BadRequest("Type the amount to deduct.".into()))?;
            ("deducted", amount, "Left mid-shift")
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
            return Err(AppError::Forbidden(
                "You can't add pay lines for yourself.".into(),
            ));
        }
        let at = access::decision_branch(pool, &claims, Cap::HrAdjustmentsCreate, &subject).await?;
        let mut ask = AuthzRequest::of(Cap::HrAdjustmentsCreate);
        ask.amount = Some(amount);
        let status = match crate::authz::require::decide_for(pool, by, &ask, at).await? {
            Decision::Allow => "approved",
            Decision::NeedsApproval(_) => "pending",
            Decision::Deny(_) => {
                return Err(crate::authz::require::denied(Cap::HrAdjustmentsCreate));
            }
        };
        let source = if resolution == "deducted" {
            "left_mid_shift"
        } else {
            "unpaid_excuse"
        };
        deduction_id = Some(
            sqlx::query_scalar(
                "INSERT INTO payroll_deductions (org_id, employee_id, amount_piastres, reason, \
                    effective_date, source, attendance_record_id, created_by, status) \
                 VALUES ($1, $2, $3, $4, COALESCE($5, CURRENT_DATE), $6, $7, $8, $9) \
                 RETURNING id",
            )
            .bind(org_id)
            .bind(employee_id)
            .bind(amount)
            .bind(reason)
            .bind(date)
            .bind(source)
            .bind(record_id)
            .bind(by)
            .bind(status)
            .fetch_one(pool)
            .await?,
        );
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
                f.kind, f.minutes_away, f.detected_at, f.resolution, f.resolved_at \
           FROM attendance_flags f JOIN employees e ON e.id = f.employee_id WHERE f.id = $1",
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
/// until the shift ends (CV-1, CV-2). Yesterday's night shift still running
/// after midnight counts, on the day it started (SC-10). One roster function.
async fn coverable_for(pool: &PgPool, employee_id: Uuid) -> Result<Vec<CoverableShift>, AppError> {
    let mine = branches_of(pool, employee_id).await?;
    let now = Utc::now();
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
    Ok(HttpResponse::Ok().json(coverable_for(pool.get_ref(), me.employee_id).await?))
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
    body: web::Json<OpenCover>,
) -> Result<HttpResponse, AppError> {
    let employee_id = me.employee_id;
    let org_id = me.org_id;
    let pool = pool.get_ref();
    crate::staff::attendance::require_rules(pool, org_id).await?;
    let shift = coverable_for(pool, employee_id)
        .await?
        .into_iter()
        .find(|c| c.employee_id == body.employee_id && c.work_shift_id == body.work_shift_id)
        .ok_or_else(|| AppError::Conflict("That shift can't be covered now.".into()))?;
    let settings = load_settings(pool, org_id, Some(shift.branch_id)).await?;
    let distance = crate::staff::attendance::check_geofence(
        pool,
        shift.branch_id,
        body.latitude,
        body.longitude,
        settings.require_geofence,
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
         VALUES ($1, $2, $3, $4, $5, 'present', $6, $7, now(), $8, $9, $10, 'cover', $11, \
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
    .fetch_one(pool)
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
    // The two people involved, and their accounts (if any).
    #[allow(clippy::type_complexity)]
    let row: Option<(Uuid, Uuid, Option<Uuid>, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "SELECT a.employee_id, a.branch_id, a.covered_employee_id, c.user_id, o.user_id \
           FROM attendance_records a \
           JOIN employees c ON c.id = a.employee_id \
           LEFT JOIN employees o ON o.id = a.covered_employee_id \
          WHERE a.id = $1 AND a.org_id = $2 AND a.cover_status = 'pending'",
    )
    .bind(*id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let Some((coverer, branch_id, _owner, coverer_user, owner_user)) = row else {
        return Err(AppError::NotFound("No cover waiting here.".into()));
    };
    access::require_at(pool, &claims, org_id, Cap::HrShiftCoverConfirm, branch_id).await?;
    if Some(by) == coverer_user || Some(by) == owner_user {
        return Err(AppError::Forbidden(
            "You can't confirm a cover you're part of.".into(),
        ));
    }
    let status = if body.approve {
        "confirmed"
    } else {
        "rejected"
    };
    // One decision only: a second confirm (or a race) finds nothing pending.
    let decided = sqlx::query(
        "UPDATE attendance_records SET cover_status = $2, edited_by = $3 \
          WHERE id = $1 AND cover_status = 'pending'",
    )
    .bind(*id)
    .bind(status)
    .bind(by)
    .execute(pool)
    .await?
    .rows_affected();
    if decided == 0 {
        return Err(AppError::Conflict("That cover was already decided.".into()));
    }
    // The flag says what was decided (CV-3).
    sqlx::query(
        "UPDATE attendance_flags SET resolution = $3, resolved_by = $2, resolved_at = now() \
          WHERE attendance_record_id = $1 AND kind = 'cover' AND resolution IS NULL",
    )
    .bind(*id)
    .bind(by)
    .bind(status)
    .execute(pool)
    .await?;
    notify(
        pool,
        org_id,
        coverer,
        if body.approve {
            "staff.n_cover_confirmed"
        } else {
            "staff.n_cover_rejected"
        },
        json!({}),
    )
    .await;
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
    let row: Option<(Uuid, Uuid, i32, Option<i32>)> = sqlx::query_as(
        "SELECT employee_id, branch_id, overtime_minutes, \
                (EXTRACT(EPOCH FROM (scheduled_end_at - scheduled_start_at)) / 60)::int \
           FROM attendance_records WHERE id = $1 AND org_id = $2 AND overtime_status = 'pending'",
    )
    .bind(*id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let Some((employee_id, branch_id, minutes, scheduled)) = row else {
        return Err(AppError::NotFound("No overtime waiting here.".into()));
    };
    let subject = access::subject(pool, org_id, employee_id).await?;
    if subject.is(&claims) {
        return Err(AppError::Forbidden(
            "You can't approve your own overtime.".into(),
        ));
    }
    access::require_at(pool, &claims, org_id, Cap::HrOvertimeApprove, branch_id).await?;
    if body.approve {
        let salary: i64 =
            sqlx::query_scalar("SELECT base_salary_piastres FROM employees WHERE id = $1")
                .bind(employee_id)
                .fetch_optional(pool)
                .await?
                .unwrap_or(0);
        let settings = load_settings(pool, org_id, Some(branch_id)).await?;
        let rates = PayRates::from_base(
            salary,
            settings.working_days_per_month,
            i64::from(scheduled.unwrap_or(480).max(1)),
        );
        let amount = crate::costing::round_piastres(
            rates.minutes_piastres(Decimal::from(minutes)) * settings.overtime_day_multiplier,
        );
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
    /// Required (CL-13): a dead phone, a forgotten one.
    pub reason: String,
}

/// Clock someone in, or out if they are in, now; marked as made by the
/// manager with the reason (CL-13, CL-16).
#[utoipa::path(
    post, path = "/staff/attendance/punch", tag = "staff", request_body = PunchFor,
    responses((status = 200, body = crate::staff::attendance::AttendanceRecord), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn punch_for(
    req: HttpRequest,
    pool: crate::db::Db,
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
        return Err(AppError::Conflict(
            "That person isn't an active employee.".into(),
        ));
    }
    // Nobody punches for themselves: their own phone or the till does that.
    if subject.is(&claims) {
        return Err(AppError::Forbidden(
            "Punch yourself in from your own phone.".into(),
        ));
    }
    crate::staff::attendance::require_rules(pool, org_id).await?;
    if subject.branches.is_empty() {
        return Err(AppError::BadRequest("That person has no branch.".into()));
    }
    access::require_for(pool, &claims, Cap::HrAttendancePunchOthers, &subject).await?;
    // At the branch the caller runs (one of theirs), else the first.
    let branch = access::decision_branch(pool, &claims, Cap::HrAttendancePunchOthers, &subject)
        .await?
        .ok_or_else(|| AppError::BadRequest("That person has no branch.".into()))?;

    let id = punch(
        pool,
        org_id,
        body.employee_id,
        branch,
        "manual",
        reason,
        Some(by),
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

/// Clock `employee_id` in at `branch` now, or out if they are in; `method`
/// says how (CL-13, CL-16), `by` the user who did it for them.
pub(crate) async fn punch(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
    branch: Uuid,
    method: &str,
    reason: &str,
    by: Option<Uuid>,
) -> Result<Uuid, AppError> {
    let open: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM attendance_records WHERE employee_id = $1 AND check_in_at IS NOT NULL \
            AND check_out_at IS NULL ORDER BY check_in_at DESC LIMIT 1",
    )
    .bind(employee_id)
    .fetch_optional(pool)
    .await?;
    let id = match open {
        Some(id) => {
            sqlx::query(
                "UPDATE attendance_records SET check_out_at = now(), check_out_method = $4, \
                        punch_reason = $2, edited_by = $3 WHERE id = $1",
            )
            .bind(id)
            .bind(reason)
            .bind(by)
            .bind(method)
            .execute(pool)
            .await?;
            id
        }
        None => {
            let tz = crate::staff::branch_timezone(pool, branch).await?;
            let today = crate::staff::attendance::today_in(pool, &tz).await?;
            let now = Utc::now();
            // The one "which shift is this" resolver (SC-10): after midnight a
            // night shift's punch lands on the day it started.
            let (resolved, today) =
                crate::staff::schedules::shift_at_instant(pool, employee_id, today, &tz, now)
                    .await?;
            let shift = resolved.as_ref();
            sqlx::query_scalar(
                "INSERT INTO attendance_records (org_id, employee_id, branch_id, work_shift_id, \
                    business_date, status, scheduled_start_at, scheduled_end_at, check_in_at, \
                    check_in_method, is_manual, punch_reason, created_by) \
                 VALUES ($1, $2, $3, $4, $5, 'present', $6, $7, now(), $10, $11, $8, $9) \
                 ON CONFLICT (employee_id, business_date, \
                    COALESCE(work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid)) WHERE covered_employee_id IS NULL \
                 DO NOTHING RETURNING id",
            )
            .bind(org_id)
            .bind(employee_id)
            .bind(branch)
            .bind(shift.map(|s| s.work_shift_id))
            .bind(today)
            .bind(shift.map(|s| s.scheduled_start_at))
            .bind(shift.map(|s| s.scheduled_end_at))
            .bind(reason)
            .bind(by)
            .bind(method)
            .bind(method == "manual")
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| AppError::Conflict("They already worked that shift today.".into()))?
        }
    };
    // Derive lateness, worked time and penalties like any punch.
    crate::staff::attendance::apply_punch_correction(
        pool, org_id, id, None, None, None, None, reason, by,
    )
    .await?;
    Ok(id)
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

/// A dead or forgotten phone in a Madar org: the person clocks in or out on
/// the branch till with their till PIN (CL-13). Marked `till` (CL-16). The
/// till is at the branch, so there is no geofence to check. Online only: a
/// PIN is never queued.
#[utoipa::path(
    post, path = "/staff/attendance/till-punch", tag = "staff", request_body = TillPunch,
    responses((status = 200, body = TillPunchResult), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn till_punch(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<TillPunch>,
) -> Result<HttpResponse, AppError> {
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
    for m in ["pos", "dawam"] {
        if !super::roster::has_module(pool, org_id, m).await? {
            return Err(AppError::Forbidden(
                "Till punches need both POS and Dawam switched on.".into(),
            ));
        }
    }
    crate::staff::attendance::require_rules(pool, org_id).await?;
    let device = req
        .headers()
        .get(crate::tickets::DEVICE_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    // The same growing delay as the till's own sign-in.
    crate::auth::pin_throttle::check(pool, device.as_deref(), body.branch_id).await?;
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
            crate::auth::pin_throttle::record_failure(pool, device.as_deref(), body.branch_id)
                .await;
            return Err(AppError::Unauthorized("Wrong PIN".into()));
        }
    };
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
    let id = punch(
        pool,
        org_id,
        employee_id,
        body.branch_id,
        "till",
        "Till PIN",
        None,
    )
    .await?;
    let record = crate::staff::attendance::load_record(pool, org_id, id).await?;
    Ok(HttpResponse::Ok().json(TillPunchResult {
        employee_id,
        name: employee_name(pool, employee_id).await,
        punched: if record.check_out_at.is_some() {
            "out"
        } else {
            "in"
        }
        .into(),
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
