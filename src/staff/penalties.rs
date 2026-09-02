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
//!     it, or waive it.
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

use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::errors::AppError;
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
    pub user_id: Uuid,
    pub business_date: NaiveDate,
    pub status: AttendanceStatus,
    /// The day is `on_leave` under an UNPAID leave type. Excused (no disciplinary
    /// absence) but still not paid, so it is docked like one.
    pub unpaid_leave: bool,
    pub late_minutes: i64,
    /// The shift's scheduled length; the per-minute pay divisor.
    pub scheduled_minutes: i64,
    pub base_salary_piastres: i64,
}

/// Recompute both automatic deductions for one attendance day.
///
/// Returns the number of rows written or updated — 0 when nothing was owed, or
/// when every candidate row was already under human control.
pub async fn recompute_for_day(
    conn: &mut PgConnection,
    day: &PricedDay,
    settings: &AttendanceSettings,
) -> Result<u64, AppError> {
    let rates = PayRates::from_base(
        day.base_salary_piastres,
        settings.working_days_per_month,
        day.scheduled_minutes.max(1),
    );
    let mut written = 0;

    // ── Late penalty ────────────────────────────────────────────
    let tiers = settings.tiers();
    let late_amount = match select_late_tier(&tiers, day.late_minutes) {
        Some(tier) => late_deduction_piastres(tier, &rates),
        None => 0,
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
    // `on_leave` under a PAID type is not absence: it is exactly the case the
    // employee asked permission for. Under an UNPAID type the day is still
    // excused — no disciplinary absence — but it is not paid either, so it is
    // docked at the same daily rate with a reason that says which it was.
    let (absent_amount, absent_reason) = match (day.status, day.unpaid_leave) {
        (AttendanceStatus::Absent, _) => (
            absence_deduction_piastres(&rates, Decimal::ONE, settings.absence_deduction_days),
            "Absent — no check-in recorded",
        ),
        (AttendanceStatus::OnLeave, true) => (
            // Unpaid leave docks exactly the day, never the harsher absence
            // multiplier — the employee did ask, and was told yes.
            absence_deduction_piastres(&rates, Decimal::ONE, Decimal::ONE),
            "Unpaid leave",
        ),
        _ => (0, ""),
    };
    written += upsert_auto_deduction(conn, day, "absence", absent_amount, absent_reason).await?;

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
             (org_id, user_id, amount_piastres, original_amount_piastres, reason, \
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
    .bind(day.user_id)
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

/// Load the pricing facts for one attendance record, then recompute it.
///
/// The convenience path used by check-out and by attendance corrections, where
/// the caller has a record id and nothing else. Employees with no salary on file
/// price at zero rather than failing — an incomplete profile must not block a
/// clock-out.
pub async fn recompute_record(
    conn: &mut PgConnection,
    record_id: Uuid,
    settings: &AttendanceSettings,
) -> Result<u64, AppError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        org_id: Uuid,
        user_id: Uuid,
        business_date: NaiveDate,
        status: String,
        late_minutes: i32,
        scheduled_minutes: Option<i32>,
        base_salary_piastres: Option<i64>,
        unpaid_leave: bool,
    }

    let row: Option<Row> = sqlx::query_as(
        "SELECT a.org_id, a.user_id, a.business_date, a.status, a.late_minutes, \
                (EXTRACT(EPOCH FROM (a.scheduled_end_at - a.scheduled_start_at)) / 60)::int \
                    AS scheduled_minutes, \
                p.base_salary_piastres, \
                EXISTS ( \
                    SELECT 1 FROM staff_requests r \
                      JOIN leave_types lt ON lt.id = r.leave_type_id \
                     WHERE r.user_id = a.user_id AND r.kind = 'leave' \
                       AND r.status = 'approved' AND NOT lt.is_paid \
                       AND r.on_date <= a.business_date \
                       AND COALESCE(r.end_date, r.on_date) >= a.business_date \
                ) AS unpaid_leave \
           FROM attendance_records a \
           LEFT JOIN staff_profiles p ON p.user_id = a.user_id \
          WHERE a.id = $1",
    )
    .bind(record_id)
    .fetch_optional(&mut *conn)
    .await?;

    let Some(row) = row else {
        return Ok(0);
    };
    let day = PricedDay {
        record_id,
        org_id: row.org_id,
        user_id: row.user_id,
        business_date: row.business_date,
        status: rules::AttendanceStatus::parse(&row.status)?,
        unpaid_leave: row.unpaid_leave,
        late_minutes: row.late_minutes as i64,
        scheduled_minutes: row.scheduled_minutes.unwrap_or(480).max(1) as i64,
        base_salary_piastres: row.base_salary_piastres.unwrap_or(0),
    };
    recompute_for_day(conn, &day, settings).await
}
