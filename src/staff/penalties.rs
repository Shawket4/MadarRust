//! Where rules become money — and where a human can say no.
//!
//! Every automatic deduction in the system is written here, and nowhere else. One
//! attendance record in, zero or more `payroll_deductions` rows out:
//!
//!   * `source = 'late_penalty'` — priced from the org's tier ladder
//!     (`attendance_settings.late_deduction_tiers`), e.g. "30 minutes late costs
//!     half a day".
//!   * `source = 'absence'` — a day nobody showed up for, priced at the org's
//!     absence policy. This used to be computed invisibly inside
//!     `compute_net_salary`; making it a row is what lets anyone see it, explain
//!     it, or waive it. A missed shift of a split day costs its share of the
//!     day (RU-5), and unpaid leave is priced like an absence (RQ-3).
//!   * `source = 'excused_unpaid'` — approved but unpaid time off inside a
//!     shift: an unpaid excuse or early departure (RQ-7).
//!
//! ## Two properties this module must never lose
//!
//! **Idempotent.** It runs at check-out, on every attendance correction, and on
//! every nightly sweep. Running it twice must not dock anyone twice — hence the
//! partial unique index on `(attendance_record_id, source)` and the upsert below.
//!
//! **A human decision is final.** If a manager has waived or overridden a row,
//! recomputation LEAVES IT ALONE. Without this the nightly sweep would silently
//! undo every act of judgement made during the day, which is worse than having no
//! override feature at all — the manager would believe the waiver held.
//!
//! ## Approved requests suppress penalties
//!
//! An approved `late_arrival` moves the grace deadline, so the lateness the ladder
//! would have priced never exists. Approved `leave` / `mission` mean the day is
//! `on_leave`, not `absent`, so no absence row is written. That is the whole point
//! of asking permission: the request removes the penalty at its source rather than
//! generating one and cancelling it.

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::authz::Cap;
use crate::errors::{AppError, AppErrorResponse};
use crate::staff::{access, principal::caller, scope_org};
use crate::staff::attendance::AttendanceSettings;
use crate::staff::rules::{
    self, AttendanceStatus, PayRates, absence_deduction_piastres, late_deduction_piastres,
    select_late_tier,
};

/// The facts about one attendance day that pricing needs.
#[derive(Debug, Clone)]
pub struct PricedDay {
    pub record_id: Uuid,
    pub org_id: Uuid,
    pub employee_id: Uuid,
    pub business_date: NaiveDate,
    pub status: AttendanceStatus,
    pub late_minutes: i64,
    /// This shift's scheduled length.
    pub scheduled_minutes: i64,
    /// Every rostered minute of the person's day (RU-5, RU-6): the divisor of
    /// the minute rate and of a missed shift's share. Equals
    /// `scheduled_minutes` on a one-shift day.
    pub day_minutes: i64,
    /// Minutes of this shift on approved leave (all of it for a day off),
    /// and whether that leave is paid (RQ-3, RQ-8).
    pub leave_minutes: i64,
    pub leave_paid: bool,
    /// Approved but unpaid time off inside the shift: an unpaid excuse or
    /// early departure (RQ-7).
    pub unpaid_excused_minutes: i64,
    pub base_salary_piastres: i64,
}

/// Recompute every automatic deduction for one attendance day.
///
/// Returns the number of rows written or updated — 0 when nothing was owed, or
/// when every candidate row was already under human control.
pub async fn recompute_for_day(
    conn: &mut PgConnection,
    day: &PricedDay,
    settings: &AttendanceSettings,
) -> Result<u64, AppError> {
    let day_minutes = day.day_minutes.max(day.scheduled_minutes).max(1);
    // RU-6: the minute rate divides a day's pay by THAT DAY's rostered minutes.
    let rates = PayRates::from_base(
        day.base_salary_piastres,
        settings.working_days_per_month,
        day_minutes,
    );
    let share = |minutes: i64| Decimal::from(minutes.max(0)) / Decimal::from(day_minutes);
    let mut written = 0;

    // ── Late penalty ────────────────────────────────────────────
    // Nobody is late for a shift they have off (B3).
    let tiers = settings.tiers();
    let late_amount = match day.status {
        AttendanceStatus::OnLeave | AttendanceStatus::Absent => 0,
        _ => match select_late_tier(&tiers, day.late_minutes) {
            Some(tier) => late_deduction_piastres(tier, &rates),
            None => 0,
        },
    };
    written += upsert_auto_deduction(
        conn,
        day,
        "late_penalty",
        late_amount,
        &format!("Late by {} minutes", day.late_minutes),
    )
    .await?;

    // ── Absence / unpaid leave ──────────────────────────────────
    // A missed shift costs its SHARE of the day's absence cost (RU-5): the
    // morning half of a split day is half an absence, and missing both halves
    // is one absence, not two. Unpaid leave is priced like an absence (RQ-3);
    // paid leave costs nothing. On a half-day leave only the worked half can
    // be missed.
    let own_leave = day.leave_minutes.min(day.scheduled_minutes).max(0);
    let absent_minutes = match day.status {
        AttendanceStatus::Absent => (day.scheduled_minutes - own_leave).max(0),
        _ => 0,
    };
    let unpaid_leave_minutes = if day.leave_paid { 0 } else { own_leave };
    let days_docked = share(absent_minutes + unpaid_leave_minutes);
    let absent_amount =
        absence_deduction_piastres(&rates, days_docked, settings.absence_deduction_days);
    let absent_reason = match (absent_minutes > 0, unpaid_leave_minutes > 0) {
        (true, true) => "Absent from the worked half · unpaid half-day leave",
        (true, false) => "Absent — no check-in recorded",
        (false, true) => "Unpaid leave",
        (false, false) => "",
    };
    written += upsert_auto_deduction(conn, day, "absence", absent_amount, absent_reason).await?;

    // ── Unpaid excused time (RQ-7) ──────────────────────────────
    let excused_amount = if day.unpaid_excused_minutes > 0 {
        crate::costing::service::round_piastres(
            rates.minutes_piastres(Decimal::from(day.unpaid_excused_minutes)),
        )
        .max(0)
    } else {
        0
    };
    written += upsert_auto_deduction(
        conn,
        day,
        "excused_unpaid",
        excused_amount,
        &format!("Unpaid excused time: {} minutes", day.unpaid_excused_minutes),
    )
    .await?;

    Ok(written)
}

/// Write, update, or retire one machine-generated deduction.
///
/// `amount == 0` means the rule no longer owes anything (a correction fixed the
/// lateness, say). The existing row is DELETED rather than zeroed, so a payslip
/// never carries a meaningless "EGP 0" line — but only if no human has touched it,
/// because a waived row is a record of a decision and must survive.
async fn upsert_auto_deduction(
    conn: &mut PgConnection,
    day: &PricedDay,
    source: &str,
    amount: i64,
    reason: &str,
) -> Result<u64, AppError> {
    if amount <= 0 {
        let deleted = sqlx::query(
            "DELETE FROM payroll_deductions \
              WHERE attendance_record_id = $1 AND source = $2 \
                AND waived_at IS NULL AND overridden_at IS NULL",
        )
        .bind(day.record_id)
        .bind(source)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        return Ok(deleted);
    }

    // The DO UPDATE deliberately excludes rows a human has touched: re-running the
    // sweep must never resurrect a waived penalty or overwrite a corrected figure.
    let affected = sqlx::query(
        "INSERT INTO payroll_deductions \
             (org_id, employee_id, amount_piastres, original_amount_piastres, reason, \
              effective_date, source, attendance_record_id) \
         VALUES ($1, $2, $3, $3, $4, $5, $6, $7) \
         ON CONFLICT (attendance_record_id, source) \
             WHERE attendance_record_id IS NOT NULL AND source <> 'manual' \
         DO UPDATE SET amount_piastres          = EXCLUDED.amount_piastres, \
                       original_amount_piastres = EXCLUDED.original_amount_piastres, \
                       reason                   = EXCLUDED.reason, \
                       updated_at               = now() \
              WHERE payroll_deductions.waived_at IS NULL \
                AND payroll_deductions.overridden_at IS NULL",
    )
    .bind(day.org_id)
    .bind(day.employee_id)
    .bind(amount)
    .bind(reason)
    .bind(day.business_date)
    .bind(source)
    .bind(day.record_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();

    Ok(affected)
}

/// Every rostered minute of `employee`'s `date` (RU-5, RU-6), from THE roster
/// function (AT-9). A shift that is no longer on the roster (edited since)
/// still counts its own minutes, so a share never exceeds the whole.
async fn day_rostered_minutes(
    pool: &PgPool,
    employee_id: Uuid,
    date: NaiveDate,
    timezone: &str,
    own_shift: Option<Uuid>,
    own_minutes: i64,
) -> Result<i64, AppError> {
    let shifts =
        crate::staff::schedules::resolve_shifts_for(pool, employee_id, date, timezone).await?;
    let rostered: i64 = shifts
        .iter()
        .map(|s| (s.scheduled_end_at - s.scheduled_start_at).num_minutes().max(0))
        .sum();
    let own_listed = own_shift.is_some_and(|id| shifts.iter().any(|s| s.work_shift_id == id));
    Ok(if own_listed {
        rostered.max(own_minutes)
    } else {
        rostered + own_minutes
    })
}

/// Load the pricing facts for one attendance record, then recompute it.
///
/// The path used by check-out, corrections, request decisions and the sweep,
/// where the caller has a record id and nothing else. Employees with no salary
/// on file price at zero rather than failing — an incomplete profile must not
/// block a clock-out.
pub async fn recompute_record(
    pool: &PgPool,
    record_id: Uuid,
    settings: &AttendanceSettings,
) -> Result<u64, AppError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        org_id: Uuid,
        employee_id: Uuid,
        branch_id: Uuid,
        work_shift_id: Option<Uuid>,
        business_date: NaiveDate,
        status: String,
        is_cover: bool,
        late_minutes: i32,
        scheduled_start_at: Option<DateTime<Utc>>,
        scheduled_end_at: Option<DateTime<Utc>>,
        check_in_at: Option<DateTime<Utc>>,
        check_out_at: Option<DateTime<Utc>>,
        base_salary_piastres: Option<i64>,
    }

    let row: Option<Row> = sqlx::query_as(
        "SELECT a.org_id, a.employee_id, a.branch_id, a.work_shift_id, a.business_date, \
                a.status, a.covered_employee_id IS NOT NULL AS is_cover, a.late_minutes, \
                a.scheduled_start_at, a.scheduled_end_at, a.check_in_at, a.check_out_at, \
                p.base_salary_piastres \
           FROM attendance_records a \
           LEFT JOIN employees p ON p.id = a.employee_id \
          WHERE a.id = $1",
    )
    .bind(record_id)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(0);
    };
    let scheduled_minutes = match (row.scheduled_start_at, row.scheduled_end_at) {
        (Some(s), Some(e)) => (e - s).num_minutes().max(1),
        _ => 480,
    };
    let day = if row.is_cover {
        // A cover is paid as extra time at the coverer's own rate (CV-4); the
        // shift it covered was someone else's, so it carries no lateness,
        // absence or leave of its own.
        PricedDay {
            record_id,
            org_id: row.org_id,
            employee_id: row.employee_id,
            business_date: row.business_date,
            status: AttendanceStatus::Present,
            late_minutes: 0,
            scheduled_minutes,
            day_minutes: scheduled_minutes,
            leave_minutes: 0,
            leave_paid: true,
            unpaid_excused_minutes: 0,
            base_salary_piastres: row.base_salary_piastres.unwrap_or(0),
        }
    } else {
        let tz = crate::staff::branch_timezone(pool, row.branch_id).await?;
        let adjustments = crate::staff::attendance::adjustments_for(
            pool,
            settings,
            row.employee_id,
            row.business_date,
            &tz,
        )
        .await?
        .for_shift(
            row.scheduled_start_at,
            row.scheduled_end_at,
            row.work_shift_id,
        );
        let day_minutes = day_rostered_minutes(
            pool,
            row.employee_id,
            row.business_date,
            &tz,
            row.work_shift_id,
            scheduled_minutes,
        )
        .await?;
        let status = rules::AttendanceStatus::parse(&row.status)?;
        let (leave_minutes, leave_paid) = match (status, adjustments.on_leave) {
            (AttendanceStatus::OnLeave, true) => (scheduled_minutes, adjustments.leave_paid),
            // A leave day set by hand with no request behind it: never docked.
            (AttendanceStatus::OnLeave, false) => (scheduled_minutes, true),
            // A manager's own status over an approved leave wins (AT-7).
            (_, true) => (0, true),
            // The half of a half-day leave inside this shift, if any.
            (_, false) => (adjustments.leave_minutes, adjustments.leave_paid),
        };
        PricedDay {
            record_id,
            org_id: row.org_id,
            employee_id: row.employee_id,
            business_date: row.business_date,
            status,
            late_minutes: row.late_minutes as i64,
            scheduled_minutes,
            day_minutes,
            leave_minutes,
            leave_paid,
            unpaid_excused_minutes: adjustments.unpaid_excused_minutes(
                row.check_in_at,
                row.check_out_at,
                row.scheduled_end_at,
            ),
            base_salary_piastres: row.base_salary_piastres.unwrap_or(0),
        }
    };
    let mut conn = pool.acquire().await?;
    recompute_for_day(&mut conn, &day, settings).await
}

// ── Undoing a waiver (AT-7) ───────────────────────────────────

#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct UnwaiveDeductionRequest {
    /// Required: undoing a decision says why (AT-7, AT-10).
    pub reason: String,
}

/// Take a waiver back: the deduction counts again, re-priced by its rule if it
/// is an automatic one. Who, when and why are kept on the row. Allowed until
/// the month's payroll is approved (AT-7), with the same right as waiving.
#[utoipa::path(
    patch, path = "/staff/payroll/deductions/{id}/unwaive", tag = "staff",
    params(("id" = Uuid, Path, description = "Deduction ID")),
    request_body = UnwaiveDeductionRequest,
    responses((status = 204, description = "The waiver is undone"), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn unwaive_deduction(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<UnwaiveDeductionRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrPayrollEdit).await?;
    let row: Option<(Uuid, NaiveDate, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "SELECT d.employee_id, d.effective_date, d.attendance_record_id, a.branch_id \
           FROM payroll_deductions d \
           LEFT JOIN attendance_records a ON a.id = d.attendance_record_id \
          WHERE d.id = $1 AND d.org_id = $2",
    )
    .bind(*id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let Some((employee_id, date, record, branch)) = row else {
        return Err(AppError::NotFound("Deduction not found".into()));
    };
    let subject = access::subject(pool, org_id, employee_id).await?;
    access::require_for(pool, &claims, Cap::HrPayrollEdit, &subject).await?;
    let reason = body.reason.trim();
    if reason.is_empty() {
        return Err(AppError::BadRequest(
            "Say why the waiver is taken back".into(),
        ));
    }
    crate::staff::requests::require_open_month(pool, org_id, date, date).await?;

    let updated = sqlx::query(
        "UPDATE payroll_deductions SET waived_at = NULL, waived_by = NULL, waive_reason = NULL, \
             unwaived_at = now(), unwaived_by = $3, unwaive_reason = $4, updated_at = now() \
          WHERE id = $1 AND org_id = $2 AND waived_at IS NOT NULL",
    )
    .bind(*id)
    .bind(org_id)
    .bind(claims.user_id_safe().ok())
    .bind(reason)
    .execute(pool)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(AppError::Conflict("That deduction isn't waived.".into()));
    }
    // An automatic deduction is priced again by today's rule for its day.
    if let Some(record) = record {
        let settings = crate::staff::attendance::load_settings(pool, org_id, branch).await?;
        recompute_record(pool, record, &settings).await?;
    }
    Ok(HttpResponse::NoContent().finish())
}
