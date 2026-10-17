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
//!     shift: an unpaid excuse or early departure (RQ-7), only the minutes
//!     actually away inside its window (owner decision D2).
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

use chrono::NaiveDate;
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
    recompute_for_days(conn, std::slice::from_ref(day)).await
}

/// [`recompute_for_day`] for many days: every machine row a day no longer owes
/// is retired in ONE statement for the whole batch, and only the rows that owe
/// something are upserted one by one. The rows that end up written are exactly
/// what pricing each day alone writes; the count is their sum.
pub async fn recompute_for_days(
    conn: &mut PgConnection,
    days: &[PricedDay],
) -> Result<u64, AppError> {
    let mut retire_records: Vec<Uuid> = Vec::new();
    let mut retire_sources: Vec<String> = Vec::new();
    let mut owed: Vec<(&PricedDay, AutoDeduction)> = Vec::new();
    for day in days {
        for d in auto_deductions(day) {
            if d.amount <= 0 {
                retire_records.push(day.record_id);
                retire_sources.push(d.source.to_string());
            } else {
                owed.push((day, d));
            }
        }
    }
    let mut written = 0;
    if !retire_records.is_empty() {
        written += retire_auto_deductions(conn, &retire_records, &retire_sources).await?;
    }
    for (day, d) in owed {
        written += upsert_auto_deduction(conn, day, &d).await?;
    }
    Ok(written)
}

/// One machine-generated deduction a priced day owes (or no longer owes, at
/// `amount <= 0`), with the stable code + figures clients word in their own
/// language (AT-13).
struct AutoDeduction {
    source: &'static str,
    amount: i64,
    reason: String,
    code: &'static str,
    vars: serde_json::Value,
}

/// The day's three automatic deductions, as [`crate::staff::pricing`] prices
/// them: late penalty, absence / unpaid leave, unpaid excused time.
fn auto_deductions(day: &PricedDay) -> [AutoDeduction; 3] {
    let price = day.price();

    // ── Absence / unpaid leave (RU-5, RQ-3, RQ-8) ───────────────
    let (absent_reason, absent_code) =
        match (price.absent_minutes > 0, price.unpaid_leave_minutes > 0) {
            (true, true) => (
                "Absent from the worked half · unpaid half-day leave",
                "absent_half_unpaid_leave",
            ),
            (true, false) => ("Absent — no check-in recorded", "absent_no_punch"),
            (false, true) => ("Unpaid leave", "unpaid_leave"),
            (false, false) => ("", ""),
        };
    [
        // ── Late penalty ────────────────────────────────────────────
        AutoDeduction {
            source: "late_penalty",
            amount: price.late_penalty_piastres,
            reason: format!("Late by {} minutes", day.facts.late_minutes),
            code: "late",
            vars: serde_json::json!({ "minutes": day.facts.late_minutes }),
        },
        AutoDeduction {
            source: "absence",
            amount: price.absence_piastres,
            reason: absent_reason.to_string(),
            code: absent_code,
            vars: serde_json::json!({}),
        },
        // ── Unpaid excused time (RQ-7) ──────────────────────────────
        AutoDeduction {
            source: "excused_unpaid",
            amount: price.excused_unpaid_piastres,
            reason: format!(
                "Unpaid excused time: {} minutes",
                day.facts.unpaid_excused_minutes
            ),
            code: "unpaid_excused_minutes",
            vars: serde_json::json!({ "minutes": day.facts.unpaid_excused_minutes }),
        },
    ]
}

/// Retire machine-generated deductions the rule no longer owes (a correction
/// fixed the lateness, say), one `(record, source)` pair per position. The row
/// is DELETED rather than zeroed, so a payslip never carries a meaningless
/// "EGP 0" line — but only if no human has touched it, because a waived row is
/// a record of a decision and must survive. A row a manager wrote by hand from
/// a flag (`created_by` set, e.g. an unpaid excuse of a mid-shift absence) is
/// theirs too: never rewritten, never deleted.
async fn retire_auto_deductions(
    conn: &mut PgConnection,
    records: &[Uuid],
    sources: &[String],
) -> Result<u64, AppError> {
    Ok(sqlx::query!(
        "DELETE FROM payroll_deductions d \
          USING unnest($1::uuid[], $2::text[]) AS k(record_id, source) \
          WHERE d.attendance_record_id = k.record_id AND d.source = k.source \
            AND d.waived_at IS NULL AND d.overridden_at IS NULL AND d.created_by IS NULL",
        records,
        sources,
    )
    .execute(&mut *conn)
    .await?
    .rows_affected())
}

/// Write or update one machine-generated deduction that is owed (`amount > 0`).
async fn upsert_auto_deduction(
    conn: &mut PgConnection,
    day: &PricedDay,
    d: &AutoDeduction,
) -> Result<u64, AppError> {
    // The DO UPDATE deliberately excludes rows a human has touched: re-running the
    // sweep must never resurrect a waived penalty or overwrite a corrected figure.
    let affected = sqlx::query(
        "INSERT INTO payroll_deductions \
             (org_id, employee_id, amount_piastres, original_amount_piastres, reason, \
              effective_date, source, attendance_record_id, reason_code, reason_vars) \
         VALUES ($1, $2, $3, $3, $4, $5, $6, $7, $8, $9) \
         ON CONFLICT (attendance_record_id, source) \
             WHERE attendance_record_id IS NOT NULL AND source <> 'manual' \
         DO UPDATE SET amount_piastres          = EXCLUDED.amount_piastres, \
                       original_amount_piastres = EXCLUDED.original_amount_piastres, \
                       reason                   = EXCLUDED.reason, \
                       reason_code              = EXCLUDED.reason_code, \
                       reason_vars              = EXCLUDED.reason_vars, \
                       updated_at               = now() \
              WHERE payroll_deductions.waived_at IS NULL \
                AND payroll_deductions.overridden_at IS NULL \
                AND payroll_deductions.created_by IS NULL",
    )
    .bind(day.org_id)
    .bind(day.employee_id)
    .bind(d.amount)
    .bind(&d.reason)
    .bind(day.business_date)
    .bind(d.source)
    .bind(day.record_id)
    .bind(Some(d.code))
    .bind(Some(&d.vars))
    .execute(&mut *conn)
    .await?
    .rows_affected();

    Ok(affected)
}

/// [`rostered_by_day`]'s answer: per `(person, date)`, `(work_shift_id, minutes)`.
type RosterByDay = HashMap<(Uuid, NaiveDate), Vec<(Uuid, i64)>>;

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
    Ok(load_facts_many(pool, &[(record_id, settings)])
        .await?
        .pop()
        .flatten())
}

/// [`load_facts`] for many records at once — what the background sweep prices
/// per tick. The record rows, the approved requests, the pings and the roster
/// are each ONE statement for the whole batch (the roster one per time zone),
/// and a branch's zone or rules are read once however many of its records are
/// in it. One result per input, in input order; each is exactly what
/// [`load_facts`] returns for that record alone (it is this, with one record).
pub async fn load_facts_many(
    pool: &PgPool,
    records: &[(Uuid, &AttendanceSettings)],
) -> Result<Vec<Option<PricedDay>>, AppError> {
    if records.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = records.iter().map(|(id, _)| *id).collect();
    let night_starts: Vec<chrono::NaiveTime> = records.iter().map(|(_, s)| s.night_start).collect();
    let night_ends: Vec<chrono::NaiveTime> = records.iter().map(|(_, s)| s.night_end).collect();

    // Night overtime is measured against the night of the rules each record
    // was handed (its own key's `night_start`/`night_end`).
    let rows = sqlx::query!(
        r#"SELECT k.ord AS "ord!", a.org_id, a.employee_id, a.branch_id, a.work_shift_id, a.business_date,
                a.status, a.covered_employee_id IS NOT NULL AS "is_cover!", a.cover_status,
                a.late_minutes, COALESCE(a.worked_minutes, 0) AS "worked_minutes!",
                COALESCE(a.overtime_minutes, 0) AS "overtime_minutes!", a.overtime_status,
                COALESCE(dawam_night_minutes(a.scheduled_end_at, a.check_out_at, br.timezone::text, k.night_start, k.night_end), 0)::bigint
                    AS "night_overtime_minutes!",
                EXISTS (SELECT 1 FROM staff_holidays h WHERE h.org_id = a.org_id
                         AND h.on_date = a.business_date AND h.decision = 'holiday') AS "holiday!",
                a.scheduled_start_at, a.scheduled_end_at, a.check_in_at, a.check_out_at,
                COALESCE((SELECT h.base_salary_piastres FROM employee_salary_history h
                           WHERE h.employee_id = a.employee_id AND h.effective_from <= a.business_date
                           ORDER BY h.effective_from DESC LIMIT 1), p.base_salary_piastres)
                    AS base_salary_piastres,
                ws.ot_day_multiplier AS "shift_ot_day: rust_decimal::Decimal", ws.ot_night_multiplier AS "shift_ot_night: rust_decimal::Decimal"
           FROM unnest($1::uuid[], $2::time[], $3::time[]) WITH ORDINALITY
                    AS k(record_id, night_start, night_end, ord)
           JOIN attendance_records a ON a.id = k.record_id
           JOIN branches br ON br.id = a.branch_id
           LEFT JOIN employees p ON p.id = a.employee_id
           LEFT JOIN work_shifts ws ON ws.id = a.work_shift_id
          ORDER BY k.ord"#,
        &ids,
        &night_starts,
        &night_ends,
    )
    .fetch_all(pool)
    .await?;
    let mut by_ord: HashMap<i64, _> = rows.into_iter().map(|r| (r.ord, r)).collect();

    // Priced under the record's own branch's rules (RU-2): the rules handed in
    // when they are that branch's, else the branch's own, read once per branch.
    let mut branch_rules: HashMap<(Uuid, Uuid), AttendanceSettings> = HashMap::new();
    let mut zones: HashMap<Uuid, String> = HashMap::new();
    let mut found = Vec::with_capacity(records.len());
    for (i, (record_id, given)) in records.iter().enumerate() {
        let Some(row) = by_ord.remove(&(i as i64 + 1)) else {
            found.push(None);
            continue;
        };
        if given.branch_id != Some(row.branch_id)
            && !branch_rules.contains_key(&(row.org_id, row.branch_id))
        {
            let s = load_settings(pool, row.org_id, Some(row.branch_id)).await?;
            branch_rules.insert((row.org_id, row.branch_id), s);
        }
        if !row.is_cover && !zones.contains_key(&row.branch_id) {
            let tz = crate::staff::branch_timezone(pool, row.branch_id).await?;
            zones.insert(row.branch_id, tz);
        }
        found.push(Some((*record_id, *given, row)));
    }
    let rules_of =
        |given: &'_ AttendanceSettings, org_id: Uuid, branch_id: Uuid| -> AttendanceSettings {
            if given.branch_id == Some(branch_id) {
                given.clone()
            } else {
                branch_rules[&(org_id, branch_id)].clone()
            }
        };

    // The approved requests and pings of every non-cover day, in one pass.
    let resolved: Vec<Option<(Uuid, AttendanceSettings, _)>> = found
        .into_iter()
        .map(|f| f.map(|(id, given, row)| (id, rules_of(given, row.org_id, row.branch_id), row)))
        .collect();
    let keys: Vec<crate::staff::requests::AdjustmentsKey<'_>> = resolved
        .iter()
        .flatten()
        .filter(|(_, _, row)| !row.is_cover)
        .map(
            |(_, settings, row)| crate::staff::requests::AdjustmentsKey {
                employee_id: row.employee_id,
                business_date: row.business_date,
                timezone: zones[&row.branch_id].as_str(),
                settings,
            },
        )
        .collect();
    let mut adjustments = crate::staff::requests::day_adjustments_many(pool, &keys)
        .await?
        .into_iter();

    // Every rostered minute of each person's day (RU-5, RU-6): the roster is
    // per person and date, so one read per zone covers the whole batch.
    let mut by_zone: HashMap<&str, (Vec<Uuid>, NaiveDate, NaiveDate)> = HashMap::new();
    for (_, _, row) in resolved.iter().flatten().filter(|(_, _, r)| !r.is_cover) {
        let e = by_zone.entry(zones[&row.branch_id].as_str()).or_insert((
            Vec::new(),
            row.business_date,
            row.business_date,
        ));
        if !e.0.contains(&row.employee_id) {
            e.0.push(row.employee_id);
        }
        e.1 = e.1.min(row.business_date);
        e.2 = e.2.max(row.business_date);
    }
    let mut roster: HashMap<&str, RosterByDay> = HashMap::new();
    for (zone, (employees, from, to)) in &by_zone {
        roster.insert(
            zone,
            rostered_by_day(pool, employees, *from, *to, Some(zone)).await?,
        );
    }

    let mut out = Vec::with_capacity(resolved.len());
    for entry in resolved {
        let Some((record_id, settings, row)) = entry else {
            out.push(None);
            continue;
        };
        let rules = ShiftRules::from_settings(&settings, row.shift_ot_day, row.shift_ot_night);
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
            let adjustments = adjustments.next().unwrap_or_default().for_shift(
                row.scheduled_start_at,
                row.scheduled_end_at,
                row.work_shift_id,
            );
            let zone = zones[&row.branch_id].as_str();
            let rostered = roster
                .get(zone)
                .and_then(|r| r.get(&(row.employee_id, row.business_date)))
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let day_minutes =
                pricing::day_minutes_of(rostered, row.work_shift_id, scheduled_minutes);
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
                unpaid_excused_minutes: adjustments
                    .unpaid_excused_minutes(row.check_in_at, row.check_out_at),
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
        out.push(Some(PricedDay {
            record_id,
            org_id: row.org_id,
            employee_id: row.employee_id,
            branch_id: row.branch_id,
            business_date: row.business_date,
            facts,
            rules,
        }));
    }
    Ok(out)
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
    recompute_records(pool, &[(record_id, settings)]).await
}

/// [`recompute_record`] for a batch (the background sweep): the record and
/// closed-month checks, the facts and the retired rows are each one statement
/// for the whole batch. Every record ends up exactly as recomputing it alone
/// leaves it; the count is the sum. A record that is gone, or dated inside an
/// approved month, is skipped (AD-10).
pub async fn recompute_records(
    pool: &PgPool,
    records: &[(Uuid, &AttendanceSettings)],
) -> Result<u64, AppError> {
    let ids: Vec<Uuid> = records.iter().map(|(id, _)| *id).collect();
    let open = period_lock::open_records(pool, &ids).await?;
    let priced: Vec<(Uuid, &AttendanceSettings)> = records
        .iter()
        .filter(|(id, _)| open.contains(id))
        .copied()
        .collect();
    if priced.is_empty() {
        return Ok(0);
    }
    let days: Vec<PricedDay> = load_facts_many(pool, &priced)
        .await?
        .into_iter()
        .flatten()
        .collect();
    let mut conn = pool.acquire().await?;
    recompute_for_days(&mut conn, &days).await
}
