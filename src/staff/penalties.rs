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
//! THE MATHS LIVES IN `pricing::price_shift` (AT-9): this module only gathers
//! the facts of a day (the record, the roster, the approved requests, the
//! salary in force, the branch's rules — RU-2) and writes what the one
//! function says. Payroll and the estimate read these rows; the overtime
//! approval and the flag suggestion price the same facts through the same
//! function ([`load_facts`]).
//!
//! ## Three properties this module must never lose
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
//! **An approved month is frozen.** A record dated inside an approved, paid or
//! closed period is not re-priced (AD-10): its payslip is a snapshot. A
//! correction of such a day changes the record, not the money; the manager
//! adds a line to the next month.
//!
//! ## Approved requests suppress penalties
//!
//! An approved `late_arrival` moves the grace deadline, so the lateness the ladder
//! would have priced never exists. Approved `leave` / `mission` mean the day is
//! `on_leave`, not `absent`, so no absence row is written. That is the whole point
//! of asking permission: the request removes the penalty at its source rather than
//! generating one and cancelling it.

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::errors::AppError;
use crate::staff::attendance::{AttendanceSettings, load_settings};
use crate::staff::period_lock;
use crate::staff::pricing::{self, ShiftFacts, ShiftPrice, ShiftRules};
use crate::staff::rules::{self, AttendanceStatus};

/// One attendance day with everything pricing needs, under the rules of the
/// branch it was worked at.
#[derive(Debug, Clone)]
pub struct PricedDay {
    pub record_id: Uuid,
    pub org_id: Uuid,
    pub employee_id: Uuid,
    pub branch_id: Uuid,
    pub business_date: NaiveDate,
    pub facts: ShiftFacts,
    pub rules: ShiftRules,
}

impl PricedDay {
    /// What the day is worth, as every path prices it.
    pub fn price(&self) -> ShiftPrice {
        pricing::price_shift(&self.facts, &self.rules)
    }
}

/// Recompute every automatic deduction for one attendance day.
///
/// Returns the number of rows written or updated — 0 when nothing was owed, or
/// when every candidate row was already under human control.
pub async fn recompute_for_day(conn: &mut PgConnection, day: &PricedDay) -> Result<u64, AppError> {
    let price = day.price();
    let mut written = 0;

    // ── Late penalty ────────────────────────────────────────────
    written += upsert_auto_deduction(
        conn,
        day,
        "late_penalty",
        price.late_penalty_piastres,
        &format!("Late by {} minutes", day.facts.late_minutes),
    )
    .await?;

    // ── Absence / unpaid leave (RU-5, RQ-3, RQ-8) ───────────────
    let absent_reason = match (price.absent_minutes > 0, price.unpaid_leave_minutes > 0) {
        (true, true) => "Absent from the worked half · unpaid half-day leave",
        (true, false) => "Absent — no check-in recorded",
        (false, true) => "Unpaid leave",
        (false, false) => "",
    };
    written +=
        upsert_auto_deduction(conn, day, "absence", price.absence_piastres, absent_reason).await?;

    // ── Unpaid excused time (RQ-7) ──────────────────────────────
    written += upsert_auto_deduction(
        conn,
        day,
        "excused_unpaid",
        price.excused_unpaid_piastres,
        &format!(
            "Unpaid excused time: {} minutes",
            day.facts.unpaid_excused_minutes
        ),
    )
    .await?;

    Ok(written)
}

/// Write, update, or retire one machine-generated deduction.
///
/// `amount == 0` means the rule no longer owes anything (a correction fixed the
/// lateness, say). The existing row is DELETED rather than zeroed, so a payslip
/// never carries a meaningless "EGP 0" line — but only if no human has touched it,
/// because a waived row is a record of a decision and must survive. A row a
/// manager wrote by hand from a flag (`created_by` set, e.g. an unpaid excuse
/// of a mid-shift absence) is theirs too: never rewritten, never deleted.
async fn upsert_auto_deduction(
    conn: &mut PgConnection,
    day: &PricedDay,
    source: &str,
    amount: i64,
    reason: &str,
) -> Result<u64, AppError> {
    // A row a person wrote (a flag's unpaid excuse carries `created_by`) is a
    // decision, like a waiver: the sweep never deletes or re-prices it.
    if amount <= 0 {
        let deleted = sqlx::query(
            "DELETE FROM payroll_deductions \
              WHERE attendance_record_id = $1 AND source = $2 \
                AND waived_at IS NULL AND overridden_at IS NULL AND created_by IS NULL",
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
                AND payroll_deductions.overridden_at IS NULL \
                AND payroll_deductions.created_by IS NULL",
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

/// The roster's shifts per person and day, as `(work_shift_id, minutes)`,
/// from THE roster function (AT-9) in one query for the whole window. Feed
/// it to [`pricing::day_minutes_of`] with the record's own shift.
pub async fn rostered_by_day<'e, E>(
    exec: E,
    employees: &[Uuid],
    from: NaiveDate,
    to: NaiveDate,
    timezone: Option<&str>,
) -> Result<HashMap<(Uuid, NaiveDate), Vec<(Uuid, i64)>>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let shifts =
        crate::staff::schedules::resolve_range(exec, employees, from, to, timezone).await?;
    let mut out: HashMap<(Uuid, NaiveDate), Vec<(Uuid, i64)>> = HashMap::new();
    for s in shifts {
        out.entry((s.employee_id, s.on_date)).or_default().push((
            s.work_shift_id,
            (s.scheduled_end_at - s.scheduled_start_at)
                .num_minutes()
                .max(0),
        ));
    }
    Ok(out)
}

/// Every rostered minute of `employee`'s `date` (RU-5, RU-6).
async fn day_rostered_minutes(
    pool: &PgPool,
    employee_id: Uuid,
    date: NaiveDate,
    timezone: &str,
    own_shift: Option<Uuid>,
    own_minutes: i64,
) -> Result<i64, AppError> {
    let by_day = rostered_by_day(pool, &[employee_id], date, date, Some(timezone)).await?;
    let rostered = by_day
        .get(&(employee_id, date))
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    Ok(pricing::day_minutes_of(rostered, own_shift, own_minutes))
}

/// Load the pricing facts for one attendance record: the record, the roster
/// of that day, the approved requests that touch it, the salary in force ON
/// THAT DAY (PAY-13) and the rules of the shift's branch (RU-2) with the shift
/// template's own overtime rates (RU-8). `None` when there is no such record.
///
/// Employees with no salary on file price at zero rather than failing — an
/// incomplete profile must not block a clock-out.
pub async fn load_facts(
    pool: &PgPool,
    record_id: Uuid,
    settings: &AttendanceSettings,
) -> Result<Option<PricedDay>, AppError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        org_id: Uuid,
        employee_id: Uuid,
        branch_id: Uuid,
        work_shift_id: Option<Uuid>,
        business_date: NaiveDate,
        status: String,
        is_cover: bool,
        cover_status: Option<String>,
        late_minutes: i32,
        worked_minutes: i32,
        overtime_minutes: i32,
        overtime_status: Option<String>,
        night_overtime_minutes: i64,
        holiday: bool,
        scheduled_start_at: Option<DateTime<Utc>>,
        scheduled_end_at: Option<DateTime<Utc>>,
        check_in_at: Option<DateTime<Utc>>,
        check_out_at: Option<DateTime<Utc>>,
        base_salary_piastres: Option<i64>,
        shift_ot_day: Option<rust_decimal::Decimal>,
        shift_ot_night: Option<rust_decimal::Decimal>,
    }

    let row: Option<Row> = sqlx::query_as(
        "SELECT a.org_id, a.employee_id, a.branch_id, a.work_shift_id, a.business_date, \
                a.status, a.covered_employee_id IS NOT NULL AS is_cover, a.cover_status, \
                a.late_minutes, COALESCE(a.worked_minutes, 0) AS worked_minutes, \
                COALESCE(a.overtime_minutes, 0) AS overtime_minutes, a.overtime_status, \
                COALESCE(dawam_night_minutes(a.scheduled_end_at, a.check_out_at, br.timezone::text, $2, $3), 0)::bigint \
                    AS night_overtime_minutes, \
                EXISTS (SELECT 1 FROM staff_holidays h WHERE h.org_id = a.org_id \
                         AND h.on_date = a.business_date AND h.decision = 'holiday') AS holiday, \
                a.scheduled_start_at, a.scheduled_end_at, a.check_in_at, a.check_out_at, \
                COALESCE((SELECT h.base_salary_piastres FROM employee_salary_history h \
                           WHERE h.employee_id = a.employee_id AND h.effective_from <= a.business_date \
                           ORDER BY h.effective_from DESC LIMIT 1), p.base_salary_piastres) \
                    AS base_salary_piastres, \
                ws.ot_day_multiplier AS shift_ot_day, ws.ot_night_multiplier AS shift_ot_night \
           FROM attendance_records a \
           JOIN branches br ON br.id = a.branch_id \
           LEFT JOIN employees p ON p.id = a.employee_id \
           LEFT JOIN work_shifts ws ON ws.id = a.work_shift_id \
          WHERE a.id = $1",
    )
    .bind(record_id)
    .bind(settings.night_start)
    .bind(settings.night_end)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };
    // Priced under the record's own branch's rules (RU-2).
    let branch_settings;
    let settings = if settings.branch_id == Some(row.branch_id) {
        settings
    } else {
        branch_settings = load_settings(pool, row.org_id, Some(row.branch_id)).await?;
        &branch_settings
    };
    let rules = ShiftRules::from_settings(settings, row.shift_ot_day, row.shift_ot_night);
    let scheduled_minutes = match (row.scheduled_start_at, row.scheduled_end_at) {
        (Some(s), Some(e)) => (e - s).num_minutes().max(1),
        _ => pricing::DEFAULT_SHIFT_MINUTES,
    };
    let salary = row.base_salary_piastres.unwrap_or(0);
    let facts = if row.is_cover {
        // A cover is paid as extra time at the coverer's own rate (CV-4); the
        // shift it covered was someone else's, so it carries no lateness,
        // absence or leave of its own.
        ShiftFacts {
            base_salary_piastres: salary,
            scheduled_minutes,
            day_minutes: scheduled_minutes,
            status: AttendanceStatus::Present,
            leave_minutes: 0,
            leave_paid: true,
            unpaid_excused_minutes: 0,
            late_minutes: 0,
            worked_minutes: i64::from(row.worked_minutes),
            overtime_minutes: 0,
            night_overtime_minutes: 0,
            overtime_status: None,
            is_confirmed_cover: row.cover_status.as_deref() == Some("confirmed"),
            is_other_cover: row.cover_status.as_deref() != Some("confirmed"),
            holiday: false,
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
        ShiftFacts {
            base_salary_piastres: salary,
            scheduled_minutes,
            day_minutes,
            status,
            leave_minutes,
            leave_paid,
            unpaid_excused_minutes: adjustments.unpaid_excused_minutes(
                row.check_in_at,
                row.check_out_at,
                row.scheduled_end_at,
            ),
            late_minutes: i64::from(row.late_minutes),
            worked_minutes: i64::from(row.worked_minutes),
            overtime_minutes: i64::from(row.overtime_minutes),
            night_overtime_minutes: row.night_overtime_minutes,
            overtime_status: row.overtime_status,
            is_confirmed_cover: false,
            is_other_cover: false,
            holiday: row.holiday,
        }
    };
    Ok(Some(PricedDay {
        record_id,
        org_id: row.org_id,
        employee_id: row.employee_id,
        branch_id: row.branch_id,
        business_date: row.business_date,
        facts,
        rules,
    }))
}

/// Load the pricing facts for one attendance record, then recompute it.
///
/// The path used by check-out, corrections, request decisions and the sweep,
/// where the caller has a record id and nothing else. A record in an approved
/// month is left exactly as it is (AD-10).
pub async fn recompute_record(
    pool: &PgPool,
    record_id: Uuid,
    settings: &AttendanceSettings,
) -> Result<u64, AppError> {
    let (org_id, business_date): (Uuid, NaiveDate) =
        match sqlx::query_as("SELECT org_id, business_date FROM attendance_records WHERE id = $1")
            .bind(record_id)
            .fetch_optional(pool)
            .await?
        {
            Some(row) => row,
            None => return Ok(0),
        };
    // An approved month is a snapshot: the penalty rows stay as they were.
    if period_lock::is_closed(pool, org_id, business_date).await? {
        return Ok(0);
    }
    let Some(day) = load_facts(pool, record_id, settings).await? else {
        return Ok(0);
    };
    let mut conn = pool.acquire().await?;
    recompute_for_day(&mut conn, &day).await
}
