//! THE shift-pricing function (Dawam AT-9, RU-2, RU-6, RU-8).
//!
//! One shift is priced in exactly one place. The clock-in preview, the nightly
//! sweep (`penalties`), the overtime approval, the flag suggestion, the
//! labour report, the live estimate and the payroll run all call
//! [`price_shift`] with the same facts and the same branch rules, so a figure
//! shown in one place is the figure paid in another.
//!
//! Conventions (RU-6, AT-2):
//!   * money is integer piastres; every intermediate is a `Decimal`;
//!   * multiply before dividing: `salary × minutes × rate ÷ (days × rostered)`;
//!   * round once, at the end, half away from zero (`round_piastres`);
//!   * the day rate is `salary ÷ working days per month`, the minute rate is
//!     the day rate ÷ THAT day's rostered minutes — never an average.

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use uuid::Uuid;

use crate::costing::service::round_piastres;
use crate::staff::attendance::AttendanceSettings;
use crate::staff::rules::AttendanceStatus;
use madar_dawam::ladder::{
    LateTier, absence_deduction_piastres, late_deduction_piastres, select_late_tier,
};
use madar_dawam::salary::PayRates;

/// Fallback shift length when a record has no scheduled window — a standard
/// eight-hour day. Only ever used as the per-minute divisor.
pub const DEFAULT_SHIFT_MINUTES: i64 = 480;

/// The rules a shift is priced under: the branch's override when it has one,
/// else the business's (RU-2), plus the shift template's own overtime rates
/// when set (RU-8).
#[derive(Debug, Clone)]
pub struct ShiftRules {
    pub working_days_per_month: Decimal,
    pub late_tiers: Vec<LateTier>,
    pub absence_deduction_days: Decimal,
    /// `off` · `automatic` · `approval`
    pub overtime_mode: String,
    pub overtime_day_multiplier: Decimal,
    pub overtime_night_multiplier: Decimal,
    pub holiday_multiplier: Decimal,
    /// A confirmed cover is paid as the covered block's full day (`full_block`)
    /// rather than at the plain minute rate over an 8-hour day (`minute_rate`,
    /// the default; owner decision D5).
    pub cover_full_block: bool,
}

impl ShiftRules {
    /// The branch's (or business's) settings, with a shift template's own
    /// rates laid over them when it has any.
    pub fn from_settings(
        s: &AttendanceSettings,
        shift_day: Option<Decimal>,
        shift_night: Option<Decimal>,
    ) -> Self {
        Self {
            working_days_per_month: s.working_days_per_month,
            late_tiers: s.tiers().iter().map(Into::into).collect(),
            absence_deduction_days: s.absence_deduction_days,
            overtime_mode: s.overtime_mode.clone(),
            overtime_day_multiplier: shift_day.unwrap_or(s.overtime_day_multiplier),
            overtime_night_multiplier: shift_night.unwrap_or(s.overtime_night_multiplier),
            holiday_multiplier: s.holiday_multiplier,
            cover_full_block: s.cover_pay_mode == "full_block",
        }
    }
}

/// The facts about one attendance day that pricing needs. Everything comes
/// from the record and the roster; nothing is looked up here.
#[derive(Debug, Clone)]
pub struct ShiftFacts {
    /// The monthly salary in force ON THAT DAY — the full salary, never a
    /// prorated one (audit B2).
    pub base_salary_piastres: i64,
    /// The rostered length of THIS shift.
    pub scheduled_minutes: i64,
    /// Every rostered minute of the person's day (RU-5, RU-6): the divisor of
    /// the minute rate and of a missed shift's share. `0` means a one-shift
    /// day (same as `scheduled_minutes`).
    pub day_minutes: i64,
    pub status: AttendanceStatus,
    /// Minutes of this shift on approved leave (all of it for a day off), and
    /// whether that leave is paid (RQ-3, RQ-8). Unpaid leave is docked like an
    /// absence; a half day docks the off half's share.
    pub leave_minutes: i64,
    pub leave_paid: bool,
    /// Approved but unpaid time off inside the shift: an unpaid excuse or the
    /// unpaid tail of an early departure (RQ-7), at the plain minute rate.
    pub unpaid_excused_minutes: i64,
    pub late_minutes: i64,
    pub worked_minutes: i64,
    /// Overtime past the shift's threshold, as the clock measured it.
    pub overtime_minutes: i64,
    /// The part of `overtime_minutes` inside the night window (RU-9).
    pub night_overtime_minutes: i64,
    /// `approved` · `pending` · `rejected` · none — matters in approval mode.
    pub overtime_status: Option<String>,
    /// A confirmed cover of a colleague's shift (CV-4): paid as extra time at
    /// the coverer's own rate, no lateness or absence of its own.
    pub is_confirmed_cover: bool,
    /// A cover that is still pending or was rejected: nothing is paid.
    pub is_other_cover: bool,
    /// The day was set up as a holiday by the manager (RU-10).
    pub holiday: bool,
}

/// What one shift is worth, each figure in piastres, each already rounded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShiftPrice {
    /// The lateness ladder's charge (0 when nothing owed).
    pub late_penalty_piastres: i64,
    /// A missed shift (or unpaid leave) at the absence policy: its SHARE of
    /// the day (RU-5) — missing both halves of a split day is one absence.
    pub absence_piastres: i64,
    /// Minutes of this shift missed without leave, and minutes on unpaid
    /// leave: what `absence_piastres` docked, for the line's wording.
    pub absent_minutes: i64,
    pub unpaid_leave_minutes: i64,
    /// Approved but unpaid time off inside the shift, at the plain rate (RQ-7).
    pub excused_unpaid_piastres: i64,
    /// The overtime that COUNTS (0 in `off` mode; in `approval` mode only
    /// when approved).
    pub overtime_minutes: i64,
    pub night_overtime_minutes: i64,
    pub overtime_piastres: i64,
    /// Extra time worked covering a colleague, at the plain rate.
    pub cover_piastres: i64,
    /// The premium for working a holiday: worked minutes × (multiplier − 1).
    pub holiday_piastres: i64,
}

impl ShiftPrice {
    /// Everything this shift adds to the payslip.
    pub fn earnings_piastres(&self) -> i64 {
        self.overtime_piastres
            .saturating_add(self.cover_piastres)
            .saturating_add(self.holiday_piastres)
    }

    /// Everything this shift takes off it (before any human waives a line).
    pub fn deductions_piastres(&self) -> i64 {
        self.late_penalty_piastres
            .saturating_add(self.absence_piastres)
            .saturating_add(self.excused_unpaid_piastres)
    }
}

/// Every rostered minute of a person's day (RU-5, RU-6) from the roster's
/// shifts `(work_shift_id, minutes)` and the record's own shift. A shift no
/// longer on the roster (edited since) still counts its own minutes, so a
/// share never exceeds the whole. The sweep, the estimate and payroll all
/// build the divisor this way.
pub fn day_minutes_of(rostered: &[(Uuid, i64)], own_shift: Option<Uuid>, own_minutes: i64) -> i64 {
    let total: i64 = rostered.iter().map(|(_, m)| (*m).max(0)).sum();
    let own_listed = own_shift.is_some_and(|id| rostered.iter().any(|(s, _)| *s == id));
    if own_listed {
        total.max(own_minutes)
    } else {
        total + own_minutes.max(0)
    }
}

/// The plain rates a shift is priced at: the day rate is `salary ÷ working
/// days`, the minute rate divides that by EVERY rostered minute of the day
/// (RU-6), never by an average.
pub fn rates_of(f: &ShiftFacts, r: &ShiftRules) -> PayRates {
    PayRates::from_base(
        f.base_salary_piastres,
        r.working_days_per_month,
        f.day_minutes.max(f.scheduled_minutes).max(1),
    )
}

/// Does this shift's overtime count under the rules (RU-7)?
pub fn overtime_counts(mode: &str, status: Option<&str>) -> bool {
    match mode {
        "automatic" => true,
        "approval" => status == Some("approved"),
        _ => false,
    }
}

/// `salary × (day × day_rate + night × night_rate) ÷ (working days × rostered)`,
/// multiplied before divided, rounded once (RU-6, RU-8).
pub fn overtime_piastres(
    base_salary_piastres: i64,
    working_days_per_month: Decimal,
    scheduled_minutes: i64,
    day_minutes: i64,
    night_minutes: i64,
    day_multiplier: Decimal,
    night_multiplier: Decimal,
) -> i64 {
    if working_days_per_month <= Decimal::ZERO || scheduled_minutes <= 0 {
        return 0;
    }
    let weighted = Decimal::from(day_minutes.max(0)) * day_multiplier.max(Decimal::ZERO)
        + Decimal::from(night_minutes.max(0)) * night_multiplier.max(Decimal::ZERO);
    round_piastres(
        Decimal::from(base_salary_piastres.max(0)) * weighted
            / (working_days_per_month * Decimal::from(scheduled_minutes)),
    )
    .max(0)
}

/// Price one shift under one set of rules.
pub fn price_shift(f: &ShiftFacts, r: &ShiftRules) -> ShiftPrice {
    let day_minutes = f.day_minutes.max(f.scheduled_minutes).max(1);
    let rates = rates_of(f, r);
    let mut out = ShiftPrice::default();

    // A cover is someone else's shift: extra time, no discipline (CV-4, CV-5).
    // Owner decision D5: by default the coverer's plain minute rate, the day
    // rate over an 8-hour day, so 2.5 hours pay 2.5 hours; `full_block` pays
    // the covered block as a full day (the block's length is the divisor).
    if f.is_confirmed_cover {
        let rates = if r.cover_full_block {
            rates
        } else {
            PayRates::from_base(
                f.base_salary_piastres,
                r.working_days_per_month,
                DEFAULT_SHIFT_MINUTES,
            )
        };
        out.cover_piastres =
            round_piastres(rates.minutes_piastres(Decimal::from(f.worked_minutes.max(0)))).max(0);
        return out;
    }
    if f.is_other_cover {
        return out;
    }

    // ── lateness ladder (RU-3, RU-4) ────────────────────────────
    // Nobody is late for a shift they have off (B3).
    if !matches!(
        f.status,
        AttendanceStatus::OnLeave | AttendanceStatus::Absent
    ) && let Some(tier) = select_late_tier(&r.late_tiers, f.late_minutes)
    {
        out.late_penalty_piastres = late_deduction_piastres(tier, &rates);
    }

    // ── absence / unpaid leave (RU-5, RQ-3, RQ-8) ───────────────
    // A missed shift costs its SHARE of the day's absence cost: the morning
    // half of a split day is half an absence, and missing both halves is one
    // absence, not two. Unpaid leave is priced like an absence (RQ-3, locked
    // in the spec); paid leave costs nothing. On a half-day leave only the
    // worked half can be missed.
    let own_leave = f.leave_minutes.clamp(0, f.scheduled_minutes.max(0));
    out.absent_minutes = match f.status {
        AttendanceStatus::Absent => (f.scheduled_minutes - own_leave).max(0),
        _ => 0,
    };
    out.unpaid_leave_minutes = if f.leave_paid { 0 } else { own_leave };
    let docked =
        Decimal::from(out.absent_minutes + out.unpaid_leave_minutes) / Decimal::from(day_minutes);
    out.absence_piastres = absence_deduction_piastres(&rates, docked, r.absence_deduction_days);

    // ── unpaid excused time (RQ-7) ──────────────────────────────
    if f.unpaid_excused_minutes > 0 {
        out.excused_unpaid_piastres =
            round_piastres(rates.minutes_piastres(Decimal::from(f.unpaid_excused_minutes))).max(0);
    }

    // ── overtime (RU-7, RU-8, RU-9) ─────────────────────────────
    if overtime_counts(&r.overtime_mode, f.overtime_status.as_deref()) && f.overtime_minutes > 0 {
        let total = f.overtime_minutes.max(0);
        let night = f.night_overtime_minutes.clamp(0, total);
        out.overtime_minutes = total;
        out.night_overtime_minutes = night;
        out.overtime_piastres = overtime_piastres(
            f.base_salary_piastres,
            r.working_days_per_month,
            day_minutes,
            total - night,
            night,
            r.overtime_day_multiplier,
            r.overtime_night_multiplier,
        );
    }

    // ── holiday premium (RU-10) ─────────────────────────────────
    if f.holiday && f.worked_minutes > 0 {
        out.holiday_piastres = round_piastres(
            rates.minutes_piastres(Decimal::from(f.worked_minutes))
                * (r.holiday_multiplier - Decimal::ONE).max(Decimal::ZERO),
        )
        .max(0);
    }
    out
}

/// A payslip's bottom line, from figures already priced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SettledNet {
    pub bonuses_piastres: i64,
    /// Deductions that survived to payday, clamped to earnings.
    pub deductions_piastres: i64,
    /// What was ACTUALLY collected against advances.
    pub advance_piastres: i64,
    pub net_piastres: i64,
    /// Deductions past what was earned: carried to the next payslip (PAY-12).
    pub capped_piastres: i64,
}

/// `net = base + overtime + bonuses − deductions − advance`, with two guards
/// (PAY-4, PAY-12): deductions never exceed earnings (the rest carries), and
/// the advance is collected only out of what is left (the rest stays owed).
pub fn settle_net(
    base: i64,
    overtime: i64,
    bonuses: i64,
    deductions: i64,
    advance_wanted: i64,
) -> SettledNet {
    let bonuses = bonuses.max(0);
    let earnings = base
        .max(0)
        .saturating_add(overtime.max(0))
        .saturating_add(bonuses);
    let asked = deductions.max(0);
    let deductions = asked.min(earnings);
    let after = earnings - deductions;
    let advance = advance_wanted.max(0).min(after);
    SettledNet {
        bonuses_piastres: bonuses,
        deductions_piastres: deductions,
        advance_piastres: advance,
        net_piastres: after - advance,
        capped_piastres: asked - deductions,
    }
}

/// A percentage of a salary, in piastres: `salary × percent ÷ 100`, rounded
/// half away from zero (AT-2 — never banker's rounding). The rule is
/// madar-shared's (`madar_dawam::pay::percent_of_salary`, DW3), the staff
/// app's too; the SQL twins (`dawam_advance_cap`, an adjustment's
/// `value_piastres`) are pinned to it by `tests/dawam_shared_rules_tests.rs`.
pub fn percent_of_salary(base_salary_piastres: i64, percent: Decimal) -> i64 {
    madar_dawam::pay::percent_of_salary(base_salary_piastres, percent)
}

/// Minutes of pay at the plain rate, exact (a repeating decimal, usually):
/// what an unpaid excuse deducts before the one rounding (RU-12).
pub fn minutes_piastres_exact(
    base_salary_piastres: i64,
    working_days_per_month: Decimal,
    day_minutes: i64,
    minutes: i64,
) -> Decimal {
    let rates = PayRates::from_base(
        base_salary_piastres,
        working_days_per_month,
        day_minutes.max(1),
    );
    rates
        .minutes_piastres(Decimal::from(minutes.max(0)))
        .max(Decimal::ZERO)
}

/// Minutes of pay at the plain rate, rounded once.
pub fn minutes_piastres(
    base_salary_piastres: i64,
    working_days_per_month: Decimal,
    day_minutes: i64,
    minutes: i64,
) -> i64 {
    round_piastres(minutes_piastres_exact(
        base_salary_piastres,
        working_days_per_month,
        day_minutes,
        minutes,
    ))
    .max(0)
}

/// A suggested amount, to the nearest 5 EGP, halves away from zero (RU-12).
/// Stored amounts stay exact; this is only ever what a form is pre-filled with.
pub fn round_to_five_egp(piastres: i64) -> i64 {
    let five = Decimal::from(500);
    let units = round_piastres(Decimal::from(piastres) / five);
    (Decimal::from(units) * five).to_i64().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use madar_dawam::salary::{prorated_base, salary_on};
    use rust_decimal_macros::dec;

    fn rules() -> ShiftRules {
        ShiftRules {
            working_days_per_month: dec!(26),
            late_tiers: vec![
                LateTier {
                    from_minutes: 1,
                    to_minutes: Some(15),
                    kind: madar_dawam::ladder::LateDeductionKind::Minutes,
                    value: dec!(15),
                },
                LateTier {
                    from_minutes: 16,
                    to_minutes: Some(30),
                    kind: madar_dawam::ladder::LateDeductionKind::Minutes,
                    value: dec!(60),
                },
                LateTier {
                    from_minutes: 31,
                    to_minutes: None,
                    kind: madar_dawam::ladder::LateDeductionKind::DayFraction,
                    value: dec!(0.5),
                },
            ],
            absence_deduction_days: dec!(1),
            overtime_mode: "automatic".into(),
            overtime_day_multiplier: dec!(1.35),
            overtime_night_multiplier: dec!(1.70),
            holiday_multiplier: dec!(2),
            cover_full_block: false,
        }
    }

    fn facts() -> ShiftFacts {
        ShiftFacts {
            base_salary_piastres: 600_000,
            scheduled_minutes: 480,
            day_minutes: 0,
            status: AttendanceStatus::Present,
            leave_minutes: 0,
            leave_paid: true,
            unpaid_excused_minutes: 0,
            late_minutes: 0,
            worked_minutes: 480,
            overtime_minutes: 0,
            night_overtime_minutes: 0,
            overtime_status: None,
            is_confirmed_cover: false,
            is_other_cover: false,
            holiday: false,
        }
    }

    /// A business's settings as `load_settings` defaults them, with overtime on.
    fn settings() -> crate::staff::attendance::AttendanceSettings {
        crate::staff::attendance::AttendanceSettings {
            id: uuid::Uuid::nil(),
            org_id: uuid::Uuid::nil(),
            branch_id: None,
            late_deduction_tiers: serde_json::json!([]),
            absence_deduction_days: Decimal::ONE,
            default_overtime_multiplier: dec!(1.5),
            auto_checkout_buffer_minutes: 120,
            working_days_per_month: dec!(26),
            require_geofence: true,
            excused_time_paid_default: true,
            period_start_day: 26,
            overtime_mode: "automatic".into(),
            overtime_day_multiplier: dec!(1.35),
            overtime_night_multiplier: dec!(1.70),
            holiday_multiplier: dec!(2),
            advance_cap_percent: dec!(50),
            half_day_leave_counts: "half_shift".into(),
            night_start: chrono::NaiveTime::from_hms_opt(22, 0, 0).unwrap(),
            night_end: chrono::NaiveTime::from_hms_opt(6, 0, 0).unwrap(),
            gender_mode: "soft".into(),
            rules_saved_at: None,
            overridden: Vec::new(),
            suggested_tiers: Vec::new(),
            limit_day_hours: dec!(8),
            limit_week_hours: dec!(48),
            limit_presence_hours: dec!(10),
            limit_rest_hours: dec!(12),
            limit_overtime_day_hours: dec!(2),
            orders_per_staff: 12,
            cover_pay_mode: "minute_rate".into(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    // Rue's August (audit 05, "Data check"), reproduced to the piastre.
    #[test]
    fn mahmouds_late_rungs_match_the_hand_computation() {
        // 24 min → rung 16-30: 60 min of pay = 600000×60/12480 = 2884.6 → 2885
        let p = price_shift(
            &ShiftFacts {
                late_minutes: 24,
                status: AttendanceStatus::Late,
                ..facts()
            },
            &rules(),
        );
        assert_eq!(p.late_penalty_piastres, 2_885);
        // 38 min → half a day = 600000 × 0.5 / 26 = 11538.46 → 11538
        let p = price_shift(
            &ShiftFacts {
                late_minutes: 38,
                status: AttendanceStatus::Late,
                ..facts()
            },
            &rules(),
        );
        assert_eq!(p.late_penalty_piastres, 11_538);
        // 14 min → 15 min of pay = 600000×15/12480 = 721.15 → 721
        let p = price_shift(
            &ShiftFacts {
                late_minutes: 14,
                status: AttendanceStatus::Late,
                ..facts()
            },
            &rules(),
        );
        assert_eq!(p.late_penalty_piastres, 721);
    }

    #[test]
    fn an_absence_costs_a_day_at_the_policy() {
        // 600000 / 26 = 23076.9 → 23077
        let p = price_shift(
            &ShiftFacts {
                status: AttendanceStatus::Absent,
                worked_minutes: 0,
                ..facts()
            },
            &rules(),
        );
        assert_eq!(p.absence_piastres, 23_077);
        // Harsher policy: 1.5 days.
        let harsh = ShiftRules {
            absence_deduction_days: dec!(1.5),
            ..rules()
        };
        let p = price_shift(
            &ShiftFacts {
                status: AttendanceStatus::Absent,
                ..facts()
            },
            &harsh,
        );
        assert_eq!(p.absence_piastres, 34_615);
    }

    #[test]
    fn unpaid_leave_is_priced_like_an_absence() {
        let harsh = ShiftRules {
            absence_deduction_days: dec!(2),
            ..rules()
        };
        let p = price_shift(
            &ShiftFacts {
                status: AttendanceStatus::OnLeave,
                leave_minutes: 480,
                leave_paid: false,
                ..facts()
            },
            &harsh,
        );
        // RQ-3: the absence cost (2 days here), not one day.
        assert_eq!(p.absence_piastres, 46_154);
        assert_eq!(p.unpaid_leave_minutes, 480);
        // Paid leave costs nothing.
        let p = price_shift(
            &ShiftFacts {
                status: AttendanceStatus::OnLeave,
                leave_minutes: 480,
                leave_paid: true,
                ..facts()
            },
            &harsh,
        );
        assert_eq!(p.absence_piastres, 0);
    }

    // Rules' facts (RU-5, RQ-7, RQ-8), priced here so the sweep, the
    // estimate and payroll agree (AT-9, orchestrator decision #3).
    #[test]
    fn a_missed_shift_of_a_split_day_costs_its_share_of_the_day() {
        // Morning 4 h + evening 4 h: missing the morning is half an absence,
        // 600000 × (240/480) / 26 = 11538.46 → 11538; both halves = 23077.
        let morning = ShiftFacts {
            status: AttendanceStatus::Absent,
            scheduled_minutes: 240,
            day_minutes: 480,
            worked_minutes: 0,
            ..facts()
        };
        let p = price_shift(&morning, &rules());
        assert_eq!(p.absence_piastres, 11_538);
        assert_eq!(p.absent_minutes, 240);
        let evening = ShiftFacts { ..morning.clone() };
        let both = p.absence_piastres + price_shift(&evening, &rules()).absence_piastres;
        assert_eq!(both, 23_076, "two halves ≈ one absence, each rounded once");
        // The minute rate on a split day divides by the WHOLE day (RU-6):
        // 30 min late on the 4-hour morning of an 8-hour day = 600000×60/12480
        // for the 16–30 rung, not 600000×60/(26×240).
        let late = ShiftFacts {
            status: AttendanceStatus::Late,
            late_minutes: 24,
            scheduled_minutes: 240,
            day_minutes: 480,
            ..facts()
        };
        assert_eq!(price_shift(&late, &rules()).late_penalty_piastres, 2_885);
        // A shift no longer on the roster still counts its own minutes.
        assert_eq!(day_minutes_of(&[(Uuid::nil(), 240)], None, 240), 480);
        let own = Uuid::new_v4();
        assert_eq!(
            day_minutes_of(&[(own, 240), (Uuid::nil(), 240)], Some(own), 240),
            480
        );
        assert_eq!(day_minutes_of(&[(own, 100)], Some(own), 240), 240);
        assert_eq!(day_minutes_of(&[], None, 480), 480);
    }

    #[test]
    fn a_half_day_of_unpaid_leave_docks_the_off_half_and_nothing_else() {
        // Second half off, unpaid, present for the first: 240/480 of a day.
        let p = price_shift(
            &ShiftFacts {
                status: AttendanceStatus::Present,
                leave_minutes: 240,
                leave_paid: false,
                worked_minutes: 240,
                ..facts()
            },
            &rules(),
        );
        assert_eq!(p.absence_piastres, 11_538);
        assert_eq!(p.unpaid_leave_minutes, 240);
        assert_eq!(p.absent_minutes, 0);
        // Absent on the worked half too: the whole day, once.
        let p = price_shift(
            &ShiftFacts {
                status: AttendanceStatus::Absent,
                leave_minutes: 240,
                leave_paid: false,
                worked_minutes: 0,
                ..facts()
            },
            &rules(),
        );
        assert_eq!(p.absence_piastres, 23_077);
        assert_eq!((p.absent_minutes, p.unpaid_leave_minutes), (240, 240));
        // A paid half day and a lateness on the worked half: only the ladder.
        let p = price_shift(
            &ShiftFacts {
                status: AttendanceStatus::Late,
                late_minutes: 10,
                leave_minutes: 240,
                leave_paid: true,
                ..facts()
            },
            &rules(),
        );
        assert_eq!((p.absence_piastres, p.late_penalty_piastres), (0, 721));
        // Nobody is late for a shift they have off (B3).
        let p = price_shift(
            &ShiftFacts {
                status: AttendanceStatus::OnLeave,
                late_minutes: 40,
                leave_minutes: 480,
                ..facts()
            },
            &rules(),
        );
        assert_eq!(p.late_penalty_piastres, 0);
    }

    #[test]
    fn unpaid_excused_minutes_cost_the_plain_minute_rate() {
        // 90 min of an unpaid excuse: 600000×90/12480 = 4326.9 → 4327.
        let p = price_shift(
            &ShiftFacts {
                unpaid_excused_minutes: 90,
                ..facts()
            },
            &rules(),
        );
        assert_eq!(p.excused_unpaid_piastres, 4_327);
        assert_eq!(p.deductions_piastres(), 4_327);
        assert_eq!(
            minutes_piastres_exact(600_000, dec!(26), 480, 90).round_dp(6),
            dec!(4326.923077)
        );
        // Alongside a lateness: both, independently rounded.
        let p = price_shift(
            &ShiftFacts {
                unpaid_excused_minutes: 90,
                late_minutes: 14,
                status: AttendanceStatus::Late,
                ..facts()
            },
            &rules(),
        );
        assert_eq!(p.deductions_piastres(), 4_327 + 721);
    }

    #[test]
    fn night_overtime_is_priced_at_the_night_rate() {
        // Mahmoud: 30 min all at night = 600000×30×1.7/12480 = 2451.9 → 2452
        let p = price_shift(
            &ShiftFacts {
                overtime_minutes: 30,
                night_overtime_minutes: 30,
                ..facts()
            },
            &rules(),
        );
        assert_eq!(p.overtime_piastres, 2_452);
        // Ziad: 1350 night minutes on 650000 = 650000×1350×1.7/12480 = 119531.25 → 119531
        let p = price_shift(
            &ShiftFacts {
                base_salary_piastres: 650_000,
                overtime_minutes: 1_350,
                night_overtime_minutes: 1_350,
                ..facts()
            },
            &rules(),
        );
        assert_eq!(p.overtime_piastres, 119_531);
        // Mixed: 60 day + 30 night = 600000×(60×1.35+30×1.7)/12480 = 6346.15 → 6346
        let p = price_shift(
            &ShiftFacts {
                overtime_minutes: 90,
                night_overtime_minutes: 30,
                ..facts()
            },
            &rules(),
        );
        assert_eq!(p.overtime_piastres, 6_346);
    }

    #[test]
    fn overtime_follows_the_mode() {
        let mut r = rules();
        let f = ShiftFacts {
            overtime_minutes: 60,
            overtime_status: Some("pending".into()),
            ..facts()
        };
        r.overtime_mode = "off".into();
        assert_eq!(price_shift(&f, &r).overtime_piastres, 0);
        r.overtime_mode = "approval".into();
        assert_eq!(price_shift(&f, &r).overtime_piastres, 0);
        let approved = ShiftFacts {
            overtime_status: Some("approved".into()),
            ..f.clone()
        };
        // 600000×60×1.35/12480 = 3894.2 → 3894
        assert_eq!(price_shift(&approved, &r).overtime_piastres, 3_894);
        r.overtime_mode = "automatic".into();
        assert_eq!(price_shift(&f, &r).overtime_piastres, 3_894);
    }

    #[test]
    fn a_shift_templates_own_rate_overrides_the_branch_rate() {
        let s = settings();
        let r = ShiftRules::from_settings(&s, Some(dec!(2)), None);
        assert_eq!(r.overtime_day_multiplier, dec!(2));
        assert_eq!(r.overtime_night_multiplier, dec!(1.70));
        let p = price_shift(
            &ShiftFacts {
                overtime_minutes: 60,
                ..facts()
            },
            &r,
        );
        // 600000×60×2/12480 = 5769.2 → 5769
        assert_eq!(p.overtime_piastres, 5_769);
    }

    #[test]
    fn a_prorated_person_still_earns_overtime_at_the_full_minute_rate() {
        // Audit B2: 600 min at 1.35 on the FULL 600000 = 38942.3 → 38942,
        // whatever proration the base pay got.
        let p = price_shift(
            &ShiftFacts {
                overtime_minutes: 600,
                ..facts()
            },
            &rules(),
        );
        assert_eq!(p.overtime_piastres, 38_942);
    }

    #[test]
    fn a_confirmed_cover_pays_the_time_and_carries_no_penalty() {
        let p = price_shift(
            &ShiftFacts {
                is_confirmed_cover: true,
                late_minutes: 40,
                worked_minutes: 240,
                status: AttendanceStatus::Late,
                ..facts()
            },
            &rules(),
        );
        // 600000×240/12480 = 11538.46 → 11538
        assert_eq!(p.cover_piastres, 11_538);
        assert_eq!(p.late_penalty_piastres, 0);
        let p = price_shift(
            &ShiftFacts {
                is_other_cover: true,
                worked_minutes: 240,
                ..facts()
            },
            &rules(),
        );
        assert_eq!(p, ShiftPrice::default());
    }

    #[test]
    fn a_holiday_worked_pays_the_premium_on_the_minutes_worked() {
        // 480 min × (2 − 1) = one day = 23077
        let p = price_shift(
            &ShiftFacts {
                holiday: true,
                ..facts()
            },
            &rules(),
        );
        assert_eq!(p.holiday_piastres, 23_077);
    }

    #[test]
    fn the_minute_rate_uses_that_days_rostered_minutes() {
        // A 6-hour day: 600000×30/(26×360) = 1923.07 → 1923, not the 8-hour 1442.
        assert_eq!(minutes_piastres(600_000, dec!(26), 360, 30), 1_923);
        assert_eq!(minutes_piastres(600_000, dec!(26), 480, 30), 1_442);
    }

    #[test]
    fn percent_bonuses_round_half_away_from_zero() {
        // 0.5% of 1 EGP = 0.5 piastre → 1, not banker's 0.
        assert_eq!(percent_of_salary(100, dec!(0.5)), 1);
        assert_eq!(percent_of_salary(1_200_000, dec!(20)), 240_000);
        // 12.5% of 501 = 62.625 → 63
        assert_eq!(percent_of_salary(501, dec!(12.5)), 63);
    }

    #[test]
    fn five_egp_rounding_is_half_away_from_zero() {
        assert_eq!(round_to_five_egp(250), 500, "2.50 → 5, not banker's 0");
        assert_eq!(round_to_five_egp(249), 0);
        assert_eq!(round_to_five_egp(750), 1_000);
        assert_eq!(round_to_five_egp(1_249), 1_000);
        assert_eq!(round_to_five_egp(0), 0);
    }

    #[test]
    fn proration_pays_each_day_at_that_days_salary() {
        let hist = vec![(d(2026, 1, 1), 600_000), (d(2026, 8, 11), 660_000)];
        // July 26 – Aug 25 (31 days): 16 days at 600000 + 15 at 660000.
        // (16×600000 + 15×660000)/31 = 629032.26 → 629032
        assert_eq!(
            prorated_base(
                &hist,
                0,
                d(2026, 7, 26),
                d(2026, 8, 25),
                d(2026, 7, 26),
                d(2026, 8, 25)
            ),
            629_032
        );
        // A full window at one salary is exactly the salary.
        assert_eq!(
            prorated_base(
                &hist,
                0,
                d(2026, 6, 26),
                d(2026, 7, 25),
                d(2026, 6, 26),
                d(2026, 7, 25)
            ),
            600_000
        );
        // Joined on day 16 of 31: 16 paid days → 600000×16/31 = 309677.4 → 309677 (audit B2).
        assert_eq!(
            prorated_base(
                &hist,
                0,
                d(2026, 6, 26),
                d(2026, 7, 26),
                d(2026, 7, 11),
                d(2026, 7, 26)
            ),
            309_677
        );
        // Left before the window: nothing.
        assert_eq!(
            prorated_base(
                &hist,
                0,
                d(2026, 6, 26),
                d(2026, 7, 25),
                d(2026, 6, 26),
                d(2026, 6, 1)
            ),
            0
        );
        // No history yet: the fallback salary.
        assert_eq!(
            prorated_base(
                &[],
                300_000,
                d(2026, 9, 1),
                d(2026, 9, 30),
                d(2026, 9, 1),
                d(2026, 9, 30)
            ),
            300_000
        );
    }

    #[test]
    fn salary_on_picks_the_latest_row_at_or_before_the_day() {
        let hist = vec![
            (d(2026, 1, 1), 100),
            (d(2026, 3, 1), 200),
            (d(2026, 6, 1), 300),
        ];
        assert_eq!(salary_on(&hist, d(2025, 12, 31), 7), 7);
        assert_eq!(salary_on(&hist, d(2026, 1, 1), 7), 100);
        assert_eq!(salary_on(&hist, d(2026, 5, 31), 7), 200);
        assert_eq!(salary_on(&hist, d(2026, 6, 1), 7), 300);
    }

    #[test]
    fn the_net_never_goes_below_zero_and_the_shortfall_carries() {
        let n = settle_net(300_000, 1_875, 0, 999_999, 0);
        assert_eq!(n.deductions_piastres, 301_875);
        assert_eq!(n.net_piastres, 0, "a payslip is never a bill");
        assert_eq!(n.capped_piastres, 999_999 - 301_875);
        let n = settle_net(300_000, 0, 0, 280_000, 50_000);
        assert_eq!(
            n.advance_piastres, 20_000,
            "the advance takes only what is left"
        );
        assert_eq!(n.net_piastres, 0);
        assert_eq!(n.capped_piastres, 0);
        let n = settle_net(300_000, 0, 50_000, 0, 100_000);
        assert_eq!(n.net_piastres, 250_000);
        assert_eq!(settle_net(-5, -5, -5, -5, -5), SettledNet::default());
    }

    #[test]
    fn zero_divisors_price_at_zero_rather_than_panicking() {
        assert_eq!(
            overtime_piastres(600_000, dec!(0), 480, 60, 0, dec!(1.35), dec!(1.7)),
            0
        );
        assert_eq!(
            overtime_piastres(600_000, dec!(26), 0, 60, 0, dec!(1.35), dec!(1.7)),
            0
        );
    }

    /// The Dawam money moved to madar-shared (`madar_dawam::salary`,
    /// `madar_dawam::ladder`) from this file and `rules.rs`. `old` is the
    /// backend's code as it stood, word for word; payroll, deduction and
    /// salary figures must not move by a piastre (nor an exact `Decimal` by a
    /// digit) for any input.
    mod old {
        use crate::costing::service::round_piastres;
        use crate::staff::rules::{LateDeductionKind, LateTier};
        use rust_decimal::Decimal;

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
                if self.working_days_per_month <= Decimal::ZERO
                    || self.scheduled_minutes_per_day <= 0
                {
                    return Decimal::ZERO;
                }
                self.base() * minutes
                    / (self.working_days_per_month * Decimal::from(self.scheduled_minutes_per_day))
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

        /// The salary in force on `day` from a dated history (newest row at or
        /// before the day wins); `fallback` when the history starts later.
        pub fn salary_on(
            history: &[(chrono::NaiveDate, i64)],
            day: chrono::NaiveDate,
            fallback: i64,
        ) -> i64 {
            history
                .iter()
                .filter(|(from, _)| *from <= day)
                .max_by_key(|(from, _)| *from)
                .map_or(fallback, |(_, s)| *s)
        }

        /// Base pay for a window, pro rata by calendar days at each day's salary
        /// (PAY-13): `Σ salary(day) ÷ window days`, summed before divided, rounded
        /// once. A full window at one salary pays exactly that salary.
        pub fn prorated_base(
            history: &[(chrono::NaiveDate, i64)],
            fallback_salary: i64,
            window_start: chrono::NaiveDate,
            window_end: chrono::NaiveDate,
            paid_from: chrono::NaiveDate,
            paid_to: chrono::NaiveDate,
        ) -> i64 {
            let window_days = (window_end - window_start).num_days() + 1;
            if window_days <= 0 || paid_to < paid_from {
                return 0;
            }
            let mut sum = Decimal::ZERO;
            let mut day = paid_from.max(window_start);
            let last = paid_to.min(window_end);
            while day <= last {
                sum += Decimal::from(salary_on(history, day, fallback_salary));
                day += chrono::Duration::days(1);
            }
            round_piastres(sum / Decimal::from(window_days)).max(0)
        }
    }

    #[test]
    fn dawam_money_is_the_old_backend_arithmetic() {
        use crate::staff::rules::{LateDeductionKind as K, LateTier as Wire};
        use madar_dawam::{ladder, salary};
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = |m: u64| {
            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (x >> 33) % m
        };
        let ds = |v: &[&str]| -> Vec<Decimal> { v.iter().map(|s| s.parse().unwrap()).collect() };
        let mut n = 0u64;

        // ── PayRates: a day's, n days' and n minutes' worth, exact ──────
        let mut salaries = vec![
            -5, 0, 1, 99, 30_000, 300_000, 600_000, 1_234_567, 99_999_999,
        ];
        salaries.extend((0..20).map(|_| next(10_000_000) as i64));
        let days = ds(&[
            "-1", "0", "0.01", "1", "22", "26", "26.5", "30", "30.42", "31",
        ]);
        let day_minutes = [-10, 0, 1, 240, 420, 450, 480, 510, 600, 1440];
        let mut qtys = ds(&[
            "-2", "0", "0.25", "0.5", "1", "1.5", "7", "13.333", "15", "30", "59", "60", "480",
        ]);
        qtys.extend((0..10).map(|_| Decimal::new(next(2_000_000) as i64, 3)));
        let mut rates = Vec::new();
        for &s in &salaries {
            for &wd in &days {
                for &m in &day_minutes {
                    let (o, c) = (
                        old::PayRates::from_base(s, wd, m),
                        salary::PayRates::from_base(s, wd, m),
                    );
                    let same = |a: Decimal, b: Decimal| a.to_string() == b.to_string();
                    assert!(same(o.daily_piastres(), c.daily_piastres()), "{s} {wd} {m}");
                    for &q in &qtys {
                        assert!(
                            same(o.days_piastres(q), c.days_piastres(q)),
                            "{s} {wd} {m} {q}"
                        );
                        assert!(
                            same(o.minutes_piastres(q), c.minutes_piastres(q)),
                            "{s} {wd} {m} {q}"
                        );
                        n += 3;
                    }
                    rates.push((o, c));
                }
            }
        }

        // ── the ladder: which rung, what it costs, what an absence costs ──
        let tier = |from, to, kind, value: &str| Wire {
            from_minutes: from,
            to_minutes: to,
            kind,
            value: value.parse().unwrap(),
        };
        let kinds = [K::Minutes, K::Piastres, K::DayFraction];
        let mut ladders = vec![
            vec![],
            crate::staff::attendance::suggested_tiers(),
            vec![
                tier(1, Some(15), K::Minutes, "30"),
                tier(16, Some(60), K::DayFraction, "0.5"),
                tier(61, None, K::DayFraction, "1"),
            ],
            vec![tier(30, Some(60), K::Piastres, "500")],
            // Unsorted, overlapping, inverted, negative: write-validation
            // refuses these, but a hand-edited row still prices the same.
            vec![
                tier(-5, Some(3), K::Piastres, "100"),
                tier(10, Some(5), K::Minutes, "7"),
                tier(2, None, K::Minutes, "12.5"),
                tier(0, Some(0), K::DayFraction, "0.333"),
                tier(4, Some(9), K::Piastres, "-40"),
            ],
        ];
        for _ in 0..20 {
            ladders.push(
                (0..1 + next(6))
                    .map(|_| {
                        let from = next(120) as i32 - 5;
                        Wire {
                            from_minutes: from,
                            to_minutes: (next(4) > 0).then(|| from + next(60) as i32 - 5),
                            kind: kinds[next(3) as usize],
                            value: Decimal::new(next(200_000) as i64, next(4) as u32),
                        }
                    })
                    .collect(),
            );
        }
        let mut lates: Vec<i64> = (-5..=200).collect();
        lates.extend([
            1_000,
            10_000,
            i32::MAX as i64,
            i32::MAX as i64 + 1,
            i64::MAX,
        ]);
        for l in &ladders {
            let c: Vec<ladder::LateTier> = l.iter().map(Into::into).collect();
            for &m in &lates {
                let o =
                    old::select_late_tier(l, m).map(|t| l.iter().position(|x| std::ptr::eq(x, t)));
                let n_ = ladder::select_late_tier(&c, m)
                    .map(|t| c.iter().position(|x| std::ptr::eq(x, t)));
                assert_eq!(o, n_, "{l:?} late {m}");
                n += 1;
            }
            for (t, ct) in l.iter().zip(&c) {
                for (o, cr) in rates.iter().step_by(7) {
                    assert_eq!(
                        old::late_deduction_piastres(t, o),
                        ladder::late_deduction_piastres(ct, cr),
                        "{t:?} {o:?}"
                    );
                    n += 1;
                }
            }
        }
        let per_absence = ds(&["-1", "0", "0.5", "1", "1.5", "2"]);
        for (o, c) in &rates {
            for &q in &qtys {
                for &p in &per_absence {
                    assert_eq!(
                        old::absence_deduction_piastres(o, q, p),
                        ladder::absence_deduction_piastres(c, q, p),
                        "{o:?} {q} {p}"
                    );
                    n += 1;
                }
            }
        }

        // ── salary history: the salary on a day, the pro-rated base ──
        let day0 = NaiveDate::from_ymd_opt(2025, 12, 1).unwrap();
        let at = |i: u64| day0 + chrono::Duration::days(i as i64);
        let mut histories = vec![
            vec![],
            vec![(d(2026, 1, 1), 600_000), (d(2026, 8, 11), 660_000)],
            vec![
                (d(2026, 6, 1), 300),
                (d(2026, 1, 1), 100),
                (d(2026, 3, 1), 200),
                (d(2026, 3, 1), 250),
            ],
        ];
        for _ in 0..10 {
            histories.push(
                (0..1 + next(6))
                    .map(|_| (at(next(430)), next(2_000_000) as i64))
                    .collect(),
            );
        }
        for h in &histories {
            for fallback in [0, 7, 500_000] {
                for i in 0..430 {
                    assert_eq!(
                        old::salary_on(h, at(i), fallback),
                        salary::salary_on(h, at(i), fallback)
                    );
                    n += 1;
                }
            }
            for _ in 0..1_500 {
                let start = at(next(400));
                let end = start + chrono::Duration::days(next(45) as i64 - 3);
                let from = start + chrono::Duration::days(next(70) as i64 - 20);
                let to = from + chrono::Duration::days(next(70) as i64 - 10);
                let fallback = next(1_000_000) as i64;
                assert_eq!(
                    old::prorated_base(h, fallback, start, end, from, to),
                    salary::prorated_base(h, fallback, start, end, from, to),
                    "{h:?} {fallback} {start}..{end} paid {from}..{to}"
                );
                n += 1;
            }
        }
        assert!(n > 200_000, "{n} cases");
        println!("{n} cases compared");
    }
}
