//! Pure attendance and payroll math.
//!
//! No database, no clock, no I/O — every input is a parameter. This is where the
//! money lives, so it is the unit-test, `cargo-mutants`, and fuzz target for the
//! staff module, exactly like `costing::service` is for cost rollups.
//!
//! Conventions, once, for the whole module:
//!   * **Money is piastres** (`i64`). Never a float, never major units.
//!   * **Durations are whole minutes** (`i64`), truncated toward zero.
//!   * Every "how late / how much overtime" figure is measured against the
//!     shift's own tolerance, so a value of `0` always means "nothing owed and
//!     nothing to explain".

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::costing::service::round_piastres;
use crate::errors::AppError;

// ── Status ───────────────────────────────────────────────────────

/// The five mutually-exclusive states an attendance row can be in.
///
/// Stored as `text` with a CHECK constraint rather than a Postgres enum: statuses
/// here are a closed set we control, and text avoids the `ALTER TYPE ... ADD
/// VALUE` transaction dance every time the set grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttendanceStatus {
    Present,
    Late,
    Absent,
    HalfDay,
    OnLeave,
}

impl AttendanceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Late => "late",
            Self::Absent => "absent",
            Self::HalfDay => "half_day",
            Self::OnLeave => "on_leave",
        }
    }

    pub fn parse(s: &str) -> Result<Self, AppError> {
        match s {
            "present" => Ok(Self::Present),
            "late" => Ok(Self::Late),
            "absent" => Ok(Self::Absent),
            "half_day" => Ok(Self::HalfDay),
            "on_leave" => Ok(Self::OnLeave),
            other => Err(AppError::BadRequest(format!(
                "Unknown attendance status '{other}' — expected one of \
                 present, late, absent, half_day, on_leave"
            ))),
        }
    }
}

// ── Shift tolerances ─────────────────────────────────────────────

/// The subset of a `work_shifts` row the math needs. Copied onto the attendance
/// record's calculation rather than joined at read time, so editing a shift later
/// never silently rewrites a historical figure.
#[derive(Debug, Clone, Copy)]
pub struct ShiftRules {
    /// Minutes after the scheduled start before lateness starts accruing.
    pub grace_minutes: i32,
    pub break_minutes: i32,
    /// An unpaid break is subtracted from worked minutes.
    pub paid_break: bool,
    /// Worked below this → half day. `None` = half the scheduled span.
    pub half_day_threshold_minutes: Option<i32>,
    /// Minutes past the scheduled end that must elapse before overtime accrues —
    /// and which are themselves unpaid.
    pub overtime_threshold_minutes: i32,
    pub overtime_multiplier: Decimal,
}

// ── Time math ────────────────────────────────────────────────────

/// Minutes late, measured from the END of the grace period — so a shift with 15
/// minutes of grace reports `0` for anyone arriving within those 15 minutes, and
/// `5` for someone arriving 20 minutes after the start.
///
/// `excused_until` is an approved late pass's agreed arrival instant. It can only
/// ever push the deadline later, never earlier: a pass for 10:00 on a shift whose
/// grace already runs to 10:30 does not make the employee late at 10:15.
pub fn late_minutes(
    scheduled_start: DateTime<Utc>,
    actual_in: DateTime<Utc>,
    grace_minutes: i32,
    excused_until: Option<DateTime<Utc>>,
) -> i64 {
    let mut deadline = scheduled_start + chrono::Duration::minutes(grace_minutes.max(0) as i64);
    if let Some(excused) = excused_until
        && excused > deadline
    {
        deadline = excused;
    }
    (actual_in - deadline).num_minutes().max(0)
}

/// Minutes actually worked between the two stamps, net of an unpaid break.
///
/// A break longer than the attendance itself cannot push the total negative — the
/// floor is zero, not a debt.
pub fn worked_minutes(
    check_in: DateTime<Utc>,
    check_out: DateTime<Utc>,
    break_minutes: i32,
    paid_break: bool,
) -> i64 {
    let raw = (check_out - check_in).num_minutes().max(0);
    if paid_break {
        raw
    } else {
        (raw - break_minutes.max(0) as i64).max(0)
    }
}

/// Paid overtime minutes: time past the scheduled end, less the threshold.
///
/// The threshold is both the trigger and unpaid — staying 20 minutes past the end
/// of a shift with a 15-minute threshold earns 5 minutes of overtime, not 20.
/// This keeps "hung around to finish a table" from turning into a payroll line.
pub fn overtime_minutes(
    scheduled_end: DateTime<Utc>,
    check_out: DateTime<Utc>,
    threshold_minutes: i32,
) -> i64 {
    let past = (check_out - scheduled_end).num_minutes();
    (past - threshold_minutes.max(0) as i64).max(0)
}

/// Minutes between an early checkout and the scheduled end. Informational — it
/// feeds the half-day check via `worked_minutes`, not a separate penalty.
pub fn early_leave_minutes(scheduled_end: DateTime<Utc>, check_out: DateTime<Utc>) -> i64 {
    (scheduled_end - check_out).num_minutes().max(0)
}

/// Rank the outcome of a day: **absent → half day → late → present**.
///
/// The order is deliberate. Half day outranks late because it is the one that
/// moves money; late outranks present because it is the one that needs
/// explaining. `scheduled_span_minutes` is the shift's own length, used as the
/// half-day threshold when the shift does not set one.
///
/// `attended` is whether the employee CLOCKED IN AT ALL, and it is the only
/// thing that produces `Absent`. Worked minutes alone must never decide this:
/// someone who clocks in and straight back out has worked ~0 minutes but is
/// plainly not absent, and absent is the status payroll docks a whole day for.
/// They get a half day — which is what an almost-empty shift actually is.
pub fn classify(
    attended: bool,
    worked_minutes: i64,
    scheduled_span_minutes: i64,
    half_day_threshold_minutes: Option<i32>,
    late_minutes: i64,
) -> AttendanceStatus {
    if !attended {
        return AttendanceStatus::Absent;
    }
    let threshold = half_day_threshold_minutes
        .map(|m| m.max(0) as i64)
        .unwrap_or_else(|| scheduled_span_minutes.max(0) / 2);
    if worked_minutes < threshold.max(1) {
        return AttendanceStatus::HalfDay;
    }
    if late_minutes > 0 {
        return AttendanceStatus::Late;
    }
    AttendanceStatus::Present
}

// ── Late-penalty tiers ───────────────────────────────────────────

/// What a tier costs the employee.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LateDeductionKind {
    /// Dock N minutes of pay (not necessarily the minutes actually lost).
    Minutes,
    /// Dock a flat sum.
    Piastres,
    /// Dock a fraction of a day's pay — `0.5` is the classic "late twice, lose
    /// half a day".
    DayFraction,
}

/// One rung of the late-penalty ladder. Ranges are inclusive at both ends;
/// `to_minutes = None` is the open-ended top rung.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LateTier {
    pub from_minutes: i32,
    #[serde(default)]
    pub to_minutes: Option<i32>,
    pub kind: LateDeductionKind,
    pub value: Decimal,
}

/// Reject a ladder an operator could not reason about: negative bounds, inverted
/// ranges, negative penalties, or overlapping rungs. Called on write so a bad
/// ladder can never reach payroll.
pub fn validate_tiers(tiers: &[LateTier]) -> Result<(), AppError> {
    let mut sorted: Vec<&LateTier> = tiers.iter().collect();
    sorted.sort_by_key(|t| t.from_minutes);

    let mut previous_end: Option<i32> = None;
    for tier in &sorted {
        if tier.from_minutes < 0 {
            return Err(AppError::BadRequest(
                "Late tier from_minutes cannot be negative".into(),
            ));
        }
        if let Some(to) = tier.to_minutes
            && to < tier.from_minutes
        {
            return Err(AppError::BadRequest(format!(
                "Late tier {}–{} ends before it starts",
                tier.from_minutes, to
            )));
        }
        if tier.value < Decimal::ZERO {
            return Err(AppError::BadRequest(
                "Late tier value cannot be negative".into(),
            ));
        }
        if let Some(prev_end) = previous_end
            && tier.from_minutes <= prev_end
        {
            return Err(AppError::BadRequest(format!(
                "Late tiers overlap at {} minutes",
                tier.from_minutes
            )));
        }
        // An open-ended rung must be the last one; anything after it is dead.
        match tier.to_minutes {
            Some(to) => previous_end = Some(to),
            None => previous_end = Some(i32::MAX),
        }
    }
    Ok(())
}

/// The rung `late_minutes` falls on, or `None` when the ladder does not reach it
/// (including the always-correct case of zero lateness).
pub fn select_late_tier(tiers: &[LateTier], late_minutes: i64) -> Option<&LateTier> {
    if late_minutes <= 0 {
        return None;
    }
    tiers.iter().find(|t| {
        let above = late_minutes >= t.from_minutes.max(0) as i64;
        let below = t.to_minutes.is_none_or(|to| late_minutes <= to as i64);
        above && below
    })
}

/// Turn a matched tier into piastres.
///
/// Takes the whole [`PayRates`] rather than pre-divided numbers so that
/// multiply-before-divide holds — see the note on [`PayRates`].
pub fn late_deduction_piastres(tier: &LateTier, rates: &PayRates) -> i64 {
    let raw = match tier.kind {
        LateDeductionKind::Minutes => rates.minutes_piastres(tier.value),
        LateDeductionKind::Piastres => tier.value,
        LateDeductionKind::DayFraction => rates.days_piastres(tier.value),
    };
    round_piastres(raw).max(0)
}

// ── Payroll ──────────────────────────────────────────────────────

/// A monthly salary and the two divisors that break it into days and minutes.
///
/// The rates are deliberately NOT precomputed. A per-minute rate is usually a
/// repeating decimal (10,000 piastres/day ÷ 480 min = 20.8333…), and dividing
/// first then multiplying loses the tail: 30 minutes' worth would come out as
/// 62.499… → 62 piastres instead of 62.5 → 63. Every accessor therefore
/// multiplies by the quantity BEFORE dividing by the divisors.
#[derive(Debug, Clone, Copy)]
pub struct PayRates {
    base_salary_piastres: i64,
    working_days_per_month: Decimal,
    scheduled_minutes_per_day: i64,
}

impl PayRates {
    /// Both divisors are guarded: a zero (or negative) `working_days_per_month`
    /// or `scheduled_minutes_per_day` yields a zero rate rather than a panic, so
    /// a half-configured org produces a visibly wrong-but-safe payslip instead of
    /// taking down the generator.
    pub fn from_base(
        base_salary_piastres: i64,
        working_days_per_month: Decimal,
        scheduled_minutes_per_day: i64,
    ) -> Self {
        Self {
            base_salary_piastres: base_salary_piastres.max(0),
            working_days_per_month,
            scheduled_minutes_per_day,
        }
    }

    fn base(&self) -> Decimal {
        Decimal::from(self.base_salary_piastres)
    }

    /// What `days` days of work are worth.
    pub fn days_piastres(&self, days: Decimal) -> Decimal {
        if self.working_days_per_month <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        self.base() * days / self.working_days_per_month
    }

    /// One day's pay.
    pub fn daily_piastres(&self) -> Decimal {
        self.days_piastres(Decimal::ONE)
    }

    /// What `minutes` minutes of work are worth, at the plain (non-overtime) rate.
    pub fn minutes_piastres(&self, minutes: Decimal) -> Decimal {
        if self.working_days_per_month <= Decimal::ZERO || self.scheduled_minutes_per_day <= 0 {
            return Decimal::ZERO;
        }
        self.base() * minutes
            / (self.working_days_per_month * Decimal::from(self.scheduled_minutes_per_day))
    }
}

/// Everything one payslip is computed from. Assembled by `payroll.rs` out of the
/// attendance ledger and the approved adjustment rows.
///
/// NOTE what is NOT here: absence. Absences used to be handed in as `unpaid_days`
/// and docked inside this function, which meant a day of pay could disappear from
/// a payslip with no line item explaining it and nothing a manager could override.
/// They are now ordinary `source='absence'` rows in `payroll_deductions`, written
/// by [`crate::staff::penalties`], and arrive here already summed into
/// `deductions_piastres` like every other deduction.
#[derive(Debug, Clone)]
pub struct PayrollInputs {
    pub base_salary_piastres: i64,
    pub working_days_per_month: Decimal,
    pub scheduled_minutes_per_day: i64,
    pub overtime_minutes: i64,
    pub overtime_multiplier: Decimal,
    /// Already-resolved approved bonuses (percent rows converted to piastres).
    pub bonuses_piastres: i64,
    /// Every live, approved, un-waived deduction — late penalties, absences, and
    /// manual entries alike.
    pub deductions_piastres: i64,
    /// What the live salary advances would like to collect this period.
    pub advance_installment_piastres: i64,
}

/// The frozen result. Every field lands on a `payslips` column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayrollTotals {
    pub base_piastres: i64,
    pub overtime_piastres: i64,
    pub bonuses_piastres: i64,
    /// Every deduction that survived to payday, clamped to earnings.
    pub deductions_piastres: i64,
    /// What was ACTUALLY collected against advances — clamped so net never goes
    /// below zero (see [`compute_net_salary`]).
    pub advance_installment_piastres: i64,
    pub net_piastres: i64,
}

/// `Net = base + overtime + bonuses − deductions − advance installment`
///
/// Two guards make the result payable rather than merely arithmetic:
///
/// 1. Deductions can never exceed earnings, so a month of absences produces a
///    zero payslip, not a bill.
/// 2. The advance installment is collected only out of what is left. A wage
///    cannot be garnished into the negative, so the shortfall stays on the
///    advance's `remaining_piastres` and is collected next period.
pub fn compute_net_salary(input: &PayrollInputs) -> PayrollTotals {
    let rates = PayRates::from_base(
        input.base_salary_piastres,
        input.working_days_per_month,
        input.scheduled_minutes_per_day,
    );

    let overtime = round_piastres(
        rates.minutes_piastres(Decimal::from(input.overtime_minutes.max(0)))
            * input.overtime_multiplier.max(Decimal::ZERO),
    )
    .max(0);

    let base = input.base_salary_piastres.max(0);
    let bonuses = input.bonuses_piastres.max(0);
    let earnings = base.saturating_add(overtime).saturating_add(bonuses);

    // Guard 1 — deductions cannot exceed earnings.
    let deductions = input.deductions_piastres.max(0).min(earnings);

    // Guard 2 — the advance takes only what is actually left.
    let after_deductions = earnings - deductions;
    let advance = input
        .advance_installment_piastres
        .max(0)
        .min(after_deductions);

    PayrollTotals {
        base_piastres: base,
        overtime_piastres: overtime,
        bonuses_piastres: bonuses,
        deductions_piastres: deductions,
        advance_installment_piastres: advance,
        net_piastres: after_deductions - advance,
    }
}

/// What one absent day costs, at the org's absence policy.
///
/// Lives here rather than inside `compute_net_salary` because absence is now a
/// deduction ROW: `penalties.rs` prices the day with this, writes the row, and a
/// manager can then waive or override it like any other.
pub fn absence_deduction_piastres(
    rates: &PayRates,
    days_absent: Decimal,
    deduction_days_per_absence: Decimal,
) -> i64 {
    round_piastres(rates.days_piastres(
        days_absent.max(Decimal::ZERO) * deduction_days_per_absence.max(Decimal::ZERO),
    ))
    .max(0)
}

/// Resolve an adjustment row that may be either a flat sum or a percentage of
/// base into piastres. Percent rows are frozen at generation time, which is why
/// the payslip stores the resolved figure and not the rate.
pub fn resolve_adjustment_piastres(
    amount_piastres: Option<i64>,
    percent_of_base: Option<Decimal>,
    base_salary_piastres: i64,
) -> i64 {
    match (amount_piastres, percent_of_base) {
        (Some(amount), _) => amount.max(0),
        (None, Some(percent)) => round_piastres(
            Decimal::from(base_salary_piastres.max(0)) * percent.max(Decimal::ZERO)
                / Decimal::from(100),
        )
        .max(0),
        (None, None) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal_macros::dec;

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 8, hour, minute, 0).unwrap()
    }

    // ── late_minutes ─────────────────────────────────────────────

    #[test]
    fn arriving_inside_the_grace_period_is_not_late() {
        assert_eq!(late_minutes(at(9, 0), at(9, 14), 15, None), 0);
    }

    #[test]
    fn late_is_measured_from_the_end_of_grace_not_the_start_time() {
        // 09:20 on a 09:00 shift with 15 minutes of grace = 5 minutes late.
        assert_eq!(late_minutes(at(9, 0), at(9, 20), 15, None), 5);
    }

    #[test]
    fn exactly_on_the_grace_boundary_is_still_not_late() {
        assert_eq!(late_minutes(at(9, 0), at(9, 15), 15, None), 0);
    }

    #[test]
    fn arriving_early_is_never_negative() {
        assert_eq!(late_minutes(at(9, 0), at(8, 30), 15, None), 0);
    }

    #[test]
    fn an_approved_late_pass_extends_the_deadline() {
        // Pass agreed for 10:00; arriving 10:05 is 5 minutes late, not 50.
        assert_eq!(
            late_minutes(at(9, 0), at(10, 5), 15, Some(at(10, 0))),
            5,
            "the pass, not the shift start, should set the deadline"
        );
    }

    #[test]
    fn a_late_pass_earlier_than_grace_cannot_make_someone_late() {
        // Pass for 09:05 but grace already runs to 09:15 — 09:10 is on time.
        assert_eq!(late_minutes(at(9, 0), at(9, 10), 15, Some(at(9, 5))), 0);
    }

    // ── worked_minutes ───────────────────────────────────────────

    #[test]
    fn paid_break_is_not_subtracted() {
        assert_eq!(worked_minutes(at(9, 0), at(17, 0), 30, true), 480);
    }

    #[test]
    fn unpaid_break_is_subtracted() {
        assert_eq!(worked_minutes(at(9, 0), at(17, 0), 30, false), 450);
    }

    #[test]
    fn an_unpaid_break_longer_than_the_shift_floors_at_zero() {
        assert_eq!(worked_minutes(at(9, 0), at(9, 10), 60, false), 0);
    }

    #[test]
    fn a_checkout_before_checkin_yields_zero_not_a_negative_day() {
        assert_eq!(worked_minutes(at(17, 0), at(9, 0), 0, true), 0);
    }

    // ── overtime ─────────────────────────────────────────────────

    #[test]
    fn staying_within_the_overtime_threshold_earns_nothing() {
        assert_eq!(overtime_minutes(at(17, 0), at(17, 15), 15), 0);
    }

    #[test]
    fn overtime_excludes_the_threshold_itself() {
        // 20 minutes past a 15-minute threshold pays 5, not 20.
        assert_eq!(overtime_minutes(at(17, 0), at(17, 20), 15), 5);
    }

    #[test]
    fn leaving_early_is_never_overtime() {
        assert_eq!(overtime_minutes(at(17, 0), at(16, 0), 15), 0);
    }

    #[test]
    fn early_leave_is_the_mirror_of_overtime() {
        assert_eq!(early_leave_minutes(at(17, 0), at(16, 30)), 30);
        assert_eq!(early_leave_minutes(at(17, 0), at(17, 30)), 0);
    }

    // ── classify ─────────────────────────────────────────────────

    #[test]
    fn never_clocking_in_is_absent() {
        assert_eq!(classify(false, 0, 480, None, 0), AttendanceStatus::Absent);
    }

    #[test]
    fn showing_up_and_leaving_immediately_is_a_half_day_not_an_absence() {
        // Absent is what payroll docks a whole day for; someone who was
        // physically there and clocked in has not earned that.
        assert_eq!(
            classify(true, 0, 480, None, 0),
            AttendanceStatus::HalfDay,
            "a clocked-in employee must never be marked absent"
        );
    }

    #[test]
    fn half_the_shift_defaults_the_half_day_threshold() {
        assert_eq!(classify(true, 239, 480, None, 0), AttendanceStatus::HalfDay);
        assert_eq!(classify(true, 240, 480, None, 0), AttendanceStatus::Present);
    }

    #[test]
    fn an_explicit_half_day_threshold_overrides_the_default() {
        assert_eq!(
            classify(true, 300, 480, Some(360), 0),
            AttendanceStatus::HalfDay
        );
    }

    #[test]
    fn half_day_outranks_late() {
        assert_eq!(
            classify(true, 100, 480, None, 45),
            AttendanceStatus::HalfDay
        );
    }

    #[test]
    fn a_full_day_with_lateness_is_late() {
        assert_eq!(classify(true, 480, 480, None, 5), AttendanceStatus::Late);
    }

    #[test]
    fn an_unscheduled_day_has_no_half_day_threshold_to_fall_below() {
        // No shift means no span, so any attendance at all is a full present day
        // rather than a permanent half day.
        assert_eq!(classify(true, 300, 0, None, 0), AttendanceStatus::Present);
    }

    #[test]
    fn status_round_trips_through_its_string_form() {
        for status in [
            AttendanceStatus::Present,
            AttendanceStatus::Late,
            AttendanceStatus::Absent,
            AttendanceStatus::HalfDay,
            AttendanceStatus::OnLeave,
        ] {
            assert_eq!(AttendanceStatus::parse(status.as_str()).unwrap(), status);
        }
        assert!(AttendanceStatus::parse("sick").is_err());
    }

    // ── tiers ────────────────────────────────────────────────────

    fn ladder() -> Vec<LateTier> {
        vec![
            LateTier {
                from_minutes: 1,
                to_minutes: Some(15),
                kind: LateDeductionKind::Minutes,
                value: dec!(30),
            },
            LateTier {
                from_minutes: 16,
                to_minutes: Some(60),
                kind: LateDeductionKind::DayFraction,
                value: dec!(0.5),
            },
            LateTier {
                from_minutes: 61,
                to_minutes: None,
                kind: LateDeductionKind::DayFraction,
                value: dec!(1),
            },
        ]
    }

    #[test]
    fn a_valid_ladder_passes_validation() {
        validate_tiers(&ladder()).unwrap();
    }

    #[test]
    fn overlapping_tiers_are_rejected() {
        let mut tiers = ladder();
        tiers[1].from_minutes = 10; // now overlaps 1–15
        assert!(validate_tiers(&tiers).is_err());
    }

    #[test]
    fn an_inverted_tier_is_rejected() {
        let tiers = vec![LateTier {
            from_minutes: 30,
            to_minutes: Some(10),
            kind: LateDeductionKind::Piastres,
            value: dec!(100),
        }];
        assert!(validate_tiers(&tiers).is_err());
    }

    #[test]
    fn a_negative_penalty_is_rejected() {
        let tiers = vec![LateTier {
            from_minutes: 1,
            to_minutes: None,
            kind: LateDeductionKind::Piastres,
            value: dec!(-1),
        }];
        assert!(validate_tiers(&tiers).is_err());
    }

    #[test]
    fn punctuality_never_matches_a_tier() {
        assert!(select_late_tier(&ladder(), 0).is_none());
    }

    #[test]
    fn tier_selection_is_inclusive_at_both_bounds() {
        assert_eq!(select_late_tier(&ladder(), 1).unwrap().from_minutes, 1);
        assert_eq!(select_late_tier(&ladder(), 15).unwrap().from_minutes, 1);
        assert_eq!(select_late_tier(&ladder(), 16).unwrap().from_minutes, 16);
        assert_eq!(select_late_tier(&ladder(), 60).unwrap().from_minutes, 16);
    }

    #[test]
    fn the_open_ended_rung_catches_everything_above_it() {
        assert_eq!(select_late_tier(&ladder(), 9_999).unwrap().from_minutes, 61);
    }

    #[test]
    fn a_gap_in_the_ladder_matches_nothing() {
        let tiers = vec![LateTier {
            from_minutes: 30,
            to_minutes: Some(60),
            kind: LateDeductionKind::Piastres,
            value: dec!(500),
        }];
        assert!(select_late_tier(&tiers, 10).is_none());
    }

    #[test]
    fn each_tier_kind_converts_to_piastres_its_own_way() {
        // 30,000 piastres/month ÷ 30 days = 1,000/day ÷ 480 min = 2.0833…/min
        let rates = PayRates::from_base(30_000, dec!(30), 480);

        let minutes = LateTier {
            from_minutes: 1,
            to_minutes: None,
            kind: LateDeductionKind::Minutes,
            value: dec!(30),
        };
        assert_eq!(
            late_deduction_piastres(&minutes, &rates),
            63, // 30 × 2.08333… = exactly 62.5 → half away from zero
        );

        let flat = LateTier {
            kind: LateDeductionKind::Piastres,
            value: dec!(250),
            ..minutes.clone()
        };
        assert_eq!(late_deduction_piastres(&flat, &rates), 250);

        let fraction = LateTier {
            kind: LateDeductionKind::DayFraction,
            value: dec!(0.5),
            ..minutes
        };
        assert_eq!(late_deduction_piastres(&fraction, &rates), 500);
    }

    // ── rates ────────────────────────────────────────────────────

    #[test]
    fn zero_divisors_produce_zero_rates_rather_than_panicking() {
        let no_days = PayRates::from_base(30_000, dec!(0), 480);
        assert_eq!(no_days.daily_piastres(), Decimal::ZERO);
        assert_eq!(no_days.minutes_piastres(dec!(30)), Decimal::ZERO);

        let no_minutes = PayRates::from_base(30_000, dec!(30), 0);
        assert_eq!(no_minutes.daily_piastres(), dec!(1000));
        assert_eq!(no_minutes.minutes_piastres(dec!(30)), Decimal::ZERO);
    }

    #[test]
    fn minute_pay_multiplies_before_dividing() {
        // 1,000/day ÷ 480 min is a repeating decimal. Dividing first would give
        // 62.499… → 62; multiplying first gives exactly 62.5 → 63.
        let rates = PayRates::from_base(30_000, dec!(30), 480);
        assert_eq!(rates.minutes_piastres(dec!(30)), dec!(62.5));
    }

    // ── net salary ───────────────────────────────────────────────

    fn inputs() -> PayrollInputs {
        PayrollInputs {
            base_salary_piastres: 300_000, // 3,000 EGP
            working_days_per_month: dec!(30),
            scheduled_minutes_per_day: 480,
            overtime_minutes: 0,
            overtime_multiplier: dec!(1.5),
            bonuses_piastres: 0,
            deductions_piastres: 0,
            advance_installment_piastres: 0,
        }
    }

    fn rates() -> PayRates {
        PayRates::from_base(300_000, dec!(30), 480)
    }

    #[test]
    fn a_clean_month_pays_exactly_base() {
        let totals = compute_net_salary(&inputs());
        assert_eq!(totals.net_piastres, 300_000);
        assert_eq!(totals.overtime_piastres, 0);
        assert_eq!(totals.deductions_piastres, 0);
    }

    #[test]
    fn overtime_is_paid_at_the_multiplier() {
        // 10,000/day ÷ 480 = 20.8333/min × 60 min × 1.5 = 1,875
        let totals = compute_net_salary(&PayrollInputs {
            overtime_minutes: 60,
            ..inputs()
        });
        assert_eq!(totals.overtime_piastres, 1_875);
        assert_eq!(totals.net_piastres, 301_875);
    }

    #[test]
    fn an_absent_day_is_priced_at_the_daily_rate() {
        // 300,000 / 30 days = 10,000 per day.
        assert_eq!(
            absence_deduction_piastres(&rates(), dec!(2), dec!(1)),
            20_000
        );
    }

    #[test]
    fn a_harsher_absence_policy_docks_more_than_the_day() {
        // Some contracts dock 1.5 days per unexcused absence.
        assert_eq!(
            absence_deduction_piastres(&rates(), dec!(1), dec!(1.5)),
            15_000
        );
    }

    #[test]
    fn absence_reaches_the_payslip_as_an_ordinary_deduction() {
        // The point of the change: two absent days are a ROW a manager can see
        // and waive, not an invisible subtraction inside the net calculation.
        let docked = absence_deduction_piastres(&rates(), dec!(2), dec!(1));
        let totals = compute_net_salary(&PayrollInputs {
            deductions_piastres: docked,
            ..inputs()
        });
        assert_eq!(totals.deductions_piastres, 20_000);
        assert_eq!(totals.net_piastres, 280_000);
    }

    #[test]
    fn a_negative_absence_policy_cannot_pay_someone_extra() {
        assert_eq!(absence_deduction_piastres(&rates(), dec!(-3), dec!(1)), 0);
        assert_eq!(absence_deduction_piastres(&rates(), dec!(2), dec!(-1)), 0);
    }

    #[test]
    fn deductions_can_never_exceed_earnings() {
        let totals = compute_net_salary(&PayrollInputs {
            deductions_piastres: 999_999_999,
            ..inputs()
        });
        assert_eq!(totals.deductions_piastres, 300_000);
        assert_eq!(totals.net_piastres, 0, "a payslip is never a bill");
    }

    #[test]
    fn a_full_month_of_absence_zeroes_the_payslip_without_going_negative() {
        let docked = absence_deduction_piastres(&rates(), dec!(30), dec!(1));
        let totals = compute_net_salary(&PayrollInputs {
            deductions_piastres: docked,
            ..inputs()
        });
        assert_eq!(totals.net_piastres, 0);
    }

    #[test]
    fn an_advance_collects_only_what_is_left() {
        let totals = compute_net_salary(&PayrollInputs {
            deductions_piastres: 280_000,
            advance_installment_piastres: 50_000,
            ..inputs()
        });
        assert_eq!(
            totals.advance_installment_piastres, 20_000,
            "the shortfall stays owed on the advance instead of garnishing the wage"
        );
        assert_eq!(totals.net_piastres, 0);
    }

    #[test]
    fn bonuses_are_added_before_the_advance_is_taken() {
        let totals = compute_net_salary(&PayrollInputs {
            bonuses_piastres: 50_000,
            advance_installment_piastres: 100_000,
            ..inputs()
        });
        assert_eq!(totals.bonuses_piastres, 50_000);
        assert_eq!(totals.advance_installment_piastres, 100_000);
        assert_eq!(totals.net_piastres, 250_000);
    }

    #[test]
    fn negative_inputs_are_clamped_rather_than_trusted() {
        let totals = compute_net_salary(&PayrollInputs {
            base_salary_piastres: -100,
            bonuses_piastres: -50,
            deductions_piastres: -50,
            advance_installment_piastres: -50,
            overtime_minutes: -60,
            ..inputs()
        });
        assert_eq!(
            totals,
            PayrollTotals {
                base_piastres: 0,
                overtime_piastres: 0,
                bonuses_piastres: 0,
                deductions_piastres: 0,
                advance_installment_piastres: 0,
                net_piastres: 0,
            }
        );
    }

    // ── adjustments ──────────────────────────────────────────────

    #[test]
    fn a_flat_adjustment_passes_straight_through() {
        assert_eq!(
            resolve_adjustment_piastres(Some(2_500), None, 300_000),
            2_500
        );
    }

    #[test]
    fn a_percentage_adjustment_resolves_against_base() {
        assert_eq!(
            resolve_adjustment_piastres(None, Some(dec!(10)), 300_000),
            30_000
        );
    }

    #[test]
    fn an_amount_wins_when_both_are_somehow_set() {
        assert_eq!(
            resolve_adjustment_piastres(Some(100), Some(dec!(50)), 300_000),
            100
        );
    }

    #[test]
    fn an_empty_adjustment_is_worth_nothing() {
        assert_eq!(resolve_adjustment_piastres(None, None, 300_000), 0);
    }
}
