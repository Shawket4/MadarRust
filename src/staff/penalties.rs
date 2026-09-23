//! Where rules become money — and where a human can say no.
//!
//! Every automatic deduction in the system is written here, and nowhere else. One
//! attendance record in, zero or more `payroll_deductions` rows out:
//!
//!   * `source = 'late_penalty'` — priced from the tier ladder
//!     (`attendance_settings.late_deduction_tiers`), e.g. "30 minutes late costs
//!     half a day".
//!   * `source = 'absence'` — a day nobody showed up for, priced at the absence
//!     policy. This used to be computed invisibly inside `compute_net_salary`;
//!     making it a row is what lets anyone see it, explain it, or waive it.
//!
//! THE FIGURES COME FROM `pricing::price_shift` (AT-9): the same function the
//! payroll run, the estimate, the overtime approval and the reports use,
//! under the BRANCH's rules (RU-2) — a branch override reaches the penalty.
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

use chrono::NaiveDate;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::errors::AppError;
use crate::staff::attendance::{AttendanceSettings, load_settings};
use crate::staff::period_lock;
use crate::staff::pricing::{self, ShiftFacts, ShiftRules};
use crate::staff::rules::{self, AttendanceStatus};

/// The facts about one attendance day that pricing needs.
#[derive(Debug, Clone)]
pub struct PricedDay {
    pub record_id: Uuid,
    pub org_id: Uuid,
    pub employee_id: Uuid,
    pub business_date: NaiveDate,
    pub status: AttendanceStatus,
    /// The day is `on_leave` under an UNPAID leave. Excused (no disciplinary
    /// absence) but still not paid, so it is docked like one.
    pub unpaid_leave: bool,
    pub late_minutes: i64,
    /// The shift's scheduled length; the per-minute pay divisor (RU-6).
    pub scheduled_minutes: i64,
    /// The salary in force on that day.
    pub base_salary_piastres: i64,
}

/// Recompute both automatic deductions for one attendance day under
/// `settings` (the record's branch's rules).
///
/// Returns the number of rows written or updated — 0 when nothing was owed, or
/// when every candidate row was already under human control.
pub async fn recompute_for_day(
    conn: &mut PgConnection,
    day: &PricedDay,
    settings: &AttendanceSettings,
) -> Result<u64, AppError> {
    let rules = ShiftRules::from_settings(settings, None, None);
    let price = pricing::price_shift(
        &ShiftFacts {
            base_salary_piastres: day.base_salary_piastres,
            scheduled_minutes: day.scheduled_minutes.max(1),
            status: day.status,
            unpaid_leave: day.unpaid_leave,
            late_minutes: day.late_minutes,
            worked_minutes: 0,
            overtime_minutes: 0,
            night_overtime_minutes: 0,
            overtime_status: None,
            is_confirmed_cover: false,
            is_other_cover: false,
            holiday: false,
        },
        &rules,
    );
    let mut written = 0;

    // ── Late penalty ────────────────────────────────────────────
    written += upsert_auto_deduction(
        conn,
        day,
        "late_penalty",
        price.late_penalty_piastres,
        &format!("Late by {} minutes", day.late_minutes),
    )
    .await?;

    // ── Absence / unpaid leave ──────────────────────────────────
    // `on_leave` under a PAID type is not absence: it is exactly the case the
    // employee asked permission for. Under an UNPAID type the day is still
    // excused — no disciplinary absence — but it is not paid either, so it is
    // docked at the daily rate with a reason that says which it was.
    let absent_reason = match (day.status, day.unpaid_leave) {
        (AttendanceStatus::Absent, _) => "Absent — no check-in recorded",
        (AttendanceStatus::OnLeave, true) => "Unpaid leave",
        _ => "",
    };
    written +=
        upsert_auto_deduction(conn, day, "absence", price.absence_piastres, absent_reason).await?;

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

/// Load the pricing facts for one attendance record, then recompute it under
/// its branch's rules.
///
/// The convenience path used by check-out and by attendance corrections, where
/// the caller has a record id and nothing else. `settings` is used as given
/// when it is the record's branch's; otherwise the branch's own rules are
/// loaded (RU-2). Employees with no salary on file price at zero rather than
/// failing — an incomplete profile must not block a clock-out. A record in an
/// approved month is left exactly as it is (AD-10).
pub async fn recompute_record(
    conn: &mut PgConnection,
    record_id: Uuid,
    settings: &AttendanceSettings,
) -> Result<u64, AppError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        org_id: Uuid,
        employee_id: Uuid,
        branch_id: Uuid,
        business_date: NaiveDate,
        status: String,
        late_minutes: i32,
        scheduled_minutes: Option<i32>,
        base_salary_piastres: Option<i64>,
        unpaid_leave: bool,
    }

    let row: Option<Row> = sqlx::query_as(
        // A cover is paid as extra time at the coverer's own rate (CV-4); the
        // shift it covered was someone else's, so it carries no lateness or
        // absence of its own. The salary is the one in force ON THAT DAY.
        "SELECT a.org_id, a.employee_id, a.branch_id, a.business_date, \
                CASE WHEN a.covered_employee_id IS NULL THEN a.status ELSE 'present' END AS status, \
                CASE WHEN a.covered_employee_id IS NULL THEN a.late_minutes ELSE 0 END AS late_minutes, \
                (EXTRACT(EPOCH FROM (a.scheduled_end_at - a.scheduled_start_at)) / 60)::int \
                    AS scheduled_minutes, \
                COALESCE((SELECT h.base_salary_piastres FROM employee_salary_history h \
                           WHERE h.employee_id = a.employee_id AND h.effective_from <= a.business_date \
                           ORDER BY h.effective_from DESC LIMIT 1), p.base_salary_piastres) \
                    AS base_salary_piastres, \
                EXISTS ( \
                    SELECT 1 FROM staff_requests r \
                      JOIN leave_types lt ON lt.id = r.leave_type_id \
                     WHERE r.employee_id = a.employee_id AND r.kind = 'leave' \
                       AND r.status = 'approved' AND NOT COALESCE(r.is_paid, lt.is_paid) \
                       AND r.on_date <= a.business_date \
                       AND COALESCE(r.end_date, r.on_date) >= a.business_date \
                ) AS unpaid_leave \
           FROM attendance_records a \
           LEFT JOIN employees p ON p.id = a.employee_id \
          WHERE a.id = $1",
    )
    .bind(record_id)
    .fetch_optional(&mut *conn)
    .await?;

    let Some(row) = row else {
        return Ok(0);
    };
    // An approved month is a snapshot: the penalty rows stay as they were.
    if period_lock::is_closed(&mut *conn, row.org_id, row.business_date).await? {
        return Ok(0);
    }
    let branch_settings;
    let settings = if settings.branch_id == Some(row.branch_id) {
        settings
    } else {
        branch_settings = load_settings(&mut *conn, row.org_id, Some(row.branch_id)).await?;
        &branch_settings
    };
    let day = PricedDay {
        record_id,
        org_id: row.org_id,
        employee_id: row.employee_id,
        business_date: row.business_date,
        status: rules::AttendanceStatus::parse(&row.status)?,
        unpaid_leave: row.unpaid_leave,
        late_minutes: row.late_minutes as i64,
        scheduled_minutes: row.scheduled_minutes.unwrap_or(480).max(1) as i64,
        base_salary_piastres: row.base_salary_piastres.unwrap_or(0),
    };
    recompute_for_day(conn, &day, settings).await
}
