//! The roster engine's pure parts: what counts as late or night (RU-9), the
//! labour limits (RU-13), coverage hours and the learned fit (SC-13). No I/O;
//! `roster.rs` loads the week and asks these.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;

use chrono::{DateTime, NaiveDate, NaiveTime, Timelike, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use super::week_start;
use crate::staff::attendance::AttendanceSettings;

fn minute_of(t: NaiveTime) -> i32 {
    (t.hour() * 60 + t.minute()) as i32
}

/// Late or night: starts at 16:00 or later, crosses midnight, or touches the
/// night window.
pub fn is_late(
    start: NaiveTime,
    end: NaiveTime,
    crosses_midnight: bool,
    night: (NaiveTime, NaiveTime),
) -> bool {
    if crosses_midnight || start >= NaiveTime::from_hms_opt(16, 0, 0).expect("valid time") {
        return true;
    }
    let (s, e) = (minute_of(start), minute_of(end));
    let (ns, ne) = (minute_of(night.0), minute_of(night.1));
    let bands = if ns < ne {
        vec![(ns, ne)]
    } else {
        vec![(ns, 1440), (0, ne)]
    };
    bands.iter().any(|&(a, b)| s < b && a < e)
}

/// The clock hours a shift touches, counted from its own date's midnight
/// (24.. is the next morning).
pub fn shift_hours(start: NaiveTime, end: NaiveTime, crosses_midnight: bool) -> Range<i32> {
    let s = minute_of(start);
    let mut e = minute_of(end);
    if crosses_midnight || e <= s {
        e += 1440;
    }
    (s / 60)..((e + 59) / 60)
}

/// The clock hours a coverage band touches.
pub fn band_hours(start: NaiveTime, end: NaiveTime) -> Range<i32> {
    (minute_of(start) / 60)..((minute_of(end) + 59) / 60)
}

/// POS-derived need for an hour: one person per `per_staff` orders (at least
/// one whenever anything sells).
pub fn pos_need(orders_per_hour: f64, per_staff: i32) -> i32 {
    if orders_per_hour <= 0.0 {
        return 0;
    }
    (orders_per_hour / f64::from(per_staff.max(1))).ceil() as i32
}

/// Labour limits in minutes (RU-13).
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub day: i64,
    pub week: i64,
    pub presence: i64,
    pub rest: i64,
}

fn minutes(h: Decimal) -> i64 {
    (h * Decimal::from(60)).to_i64().unwrap_or(i64::MAX)
}

impl Limits {
    pub fn of(s: &AttendanceSettings) -> Self {
        Self {
            day: minutes(s.limit_day_hours),
            week: minutes(s.limit_week_hours),
            presence: minutes(s.limit_presence_hours),
            rest: minutes(s.limit_rest_hours),
        }
    }
}

/// One rostered shift of one person.
#[derive(Clone, Copy, Debug)]
pub struct Span {
    pub date: NaiveDate,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// A roster past a labour limit. Warns, never blocks (RU-13).
#[derive(Serialize, ToSchema, Clone, Debug, PartialEq, Eq, Hash)]
pub struct LabourWarning {
    pub employee_id: Uuid,
    /// The day (or, for a week's limit, the Saturday it starts).
    pub date: NaiveDate,
    /// `day_hours` · `week_hours` · `presence` · `rest` · `weekly_rest` · `overtime_day`
    pub kind: String,
    pub minutes: i64,
    pub limit_minutes: i64,
}

/// Every limit one person's spans break.
pub fn breaks(employee_id: Uuid, spans: &[Span], l: &Limits) -> Vec<LabourWarning> {
    let mut out = Vec::new();
    let w = |date, kind: &str, minutes, limit_minutes| LabourWarning {
        employee_id,
        date,
        kind: kind.into(),
        minutes,
        limit_minutes,
    };
    let mut days: BTreeMap<NaiveDate, (i64, DateTime<Utc>, DateTime<Utc>)> = BTreeMap::new();
    for s in spans {
        let e = days.entry(s.date).or_insert((0, s.start, s.end));
        e.0 += (s.end - s.start).num_minutes();
        e.1 = e.1.min(s.start);
        e.2 = e.2.max(s.end);
    }
    let mut weeks: BTreeMap<NaiveDate, (i64, usize)> = BTreeMap::new();
    for (d, (worked, first, last)) in &days {
        if *worked > l.day {
            out.push(w(*d, "day_hours", *worked, l.day));
        }
        let presence = (*last - *first).num_minutes();
        if presence > l.presence {
            out.push(w(*d, "presence", presence, l.presence));
        }
        let wk = weeks.entry(week_start(*d)).or_default();
        wk.0 += worked;
        wk.1 += 1;
    }
    for (ws, (worked, n)) in weeks {
        if worked > l.week {
            out.push(w(ws, "week_hours", worked, l.week));
        }
        if n >= 7 {
            out.push(w(ws, "weekly_rest", 0, 24 * 60));
        }
    }
    let mut sorted: Vec<&Span> = spans.iter().collect();
    sorted.sort_by_key(|s| s.start);
    for pair in sorted.windows(2) {
        // Split shifts on one day are allowed (SC-11); rest is between days.
        if pair[0].date != pair[1].date {
            let gap = (pair[1].start - pair[0].end).num_minutes();
            if gap < l.rest {
                out.push(w(pair[1].date, "rest", gap.max(0), l.rest));
            }
        }
    }
    out
}

/// Would adding `extra` break a limit the person doesn't already break?
pub fn fits(employee_id: Uuid, spans: &[Span], extra: Span, l: &Limits) -> bool {
    let before: HashSet<(NaiveDate, String)> = breaks(employee_id, spans, l)
        .into_iter()
        .map(|b| (b.date, b.kind))
        .collect();
    let mut with = spans.to_vec();
    with.push(extra);
    breaks(employee_id, &with, l)
        .into_iter()
        .all(|b| before.contains(&(b.date, b.kind)))
}

/// One learning event: +1 took or kept a shift of that class, −1 refused or
/// missed it.
pub struct Signal {
    pub employee_id: Uuid,
    pub late: bool,
    pub value: f64,
    pub age_days: f64,
    /// Manager-side (accept/reject) or employee-side (claims, swaps, turning up).
    pub manager: bool,
}

/// 8-week half-life.
pub fn decay(age_days: f64) -> f64 {
    0.5f64.powf(age_days.max(0.0) / 56.0)
}

/// A person's learned fit, per class (day, late), kept apart per side.
#[derive(Default, Clone, Debug)]
pub struct Fit {
    /// [side][class] = (weighted sum, weight, events); side 0 = manager.
    acc: [[(f64, f64, u32); 2]; 2],
}

impl Fit {
    fn term((sum, weight, _): (f64, f64, u32)) -> f64 {
        // Shrunk toward 0: one event says little.
        sum / (weight + 1.0)
    }
    pub fn score(&self, late: bool) -> f64 {
        let c = usize::from(late);
        0.5 * Self::term(self.acc[0][c]) + 0.5 * Self::term(self.acc[1][c])
    }
    pub fn events(&self, late: bool) -> u32 {
        let c = usize::from(late);
        self.acc[0][c].2 + self.acc[1][c].2
    }
    /// The person themselves has shown they take late shifts.
    pub fn willing_late(&self) -> bool {
        self.acc[1][1].2 >= 3 && Self::term(self.acc[1][1]) > 0.0
    }
}

pub fn learn(signals: &[Signal]) -> HashMap<Uuid, Fit> {
    let mut out: HashMap<Uuid, Fit> = HashMap::new();
    for s in signals {
        let f = out.entry(s.employee_id).or_default();
        let wgt = decay(s.age_days);
        let slot = &mut f.acc[usize::from(!s.manager)][usize::from(s.late)];
        slot.0 += wgt * s.value;
        slot.1 += wgt;
        slot.2 += 1;
    }
    out
}

/// Learning freezes when managers accept under 40% of suggestions over the
/// last 4 weeks (needs a handful to judge).
pub fn frozen(accepted: i64, decided: i64) -> bool {
    decided >= 5 && (accepted as f64) < 0.4 * decided as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn late_follows_the_night_window() {
        let night = (t(22, 0), t(6, 0));
        assert!(!is_late(t(9, 0), t(17, 0), false, night));
        assert!(is_late(t(14, 0), t(23, 0), false, night));
        assert!(is_late(t(5, 0), t(13, 0), false, night));
        assert!(is_late(t(16, 0), t(21, 0), false, night));
        assert!(is_late(t(6, 30), t(15, 0), false, (t(19, 0), t(7, 0))));
        assert!(!is_late(t(8, 0), t(15, 0), false, (t(22, 0), t(6, 0))));
    }

    #[test]
    fn hours_and_needs() {
        assert_eq!(shift_hours(t(22, 0), t(6, 0), true), 22..30);
        assert_eq!(shift_hours(t(10, 30), t(14, 0), false), 10..14);
        assert_eq!(band_hours(t(12, 0), t(15, 0)), 12..15);
        assert_eq!(pos_need(0.0, 12), 0);
        assert_eq!(pos_need(1.0, 12), 1);
        assert_eq!(pos_need(25.0, 12), 3);
    }

    #[test]
    fn limits_warn_on_each_break() {
        let u = Uuid::nil();
        let l = Limits {
            day: 480,
            week: 48 * 60,
            presence: 600,
            rest: 720,
        };
        let at = |d: u32, h: u32| Utc.with_ymd_and_hms(2026, 9, d, h, 0, 0).unwrap();
        let day = |d: u32| NaiveDate::from_ymd_opt(2026, 9, d).unwrap();
        // Split day 08–12 + 18–23: 9 h worked, 15 h present.
        let split = [
            Span {
                date: day(19),
                start: at(19, 8),
                end: at(19, 12),
            },
            Span {
                date: day(19),
                start: at(19, 18),
                end: at(19, 23),
            },
        ];
        let kinds: Vec<String> = breaks(u, &split, &l).into_iter().map(|b| b.kind).collect();
        assert_eq!(kinds, ["day_hours", "presence"]);
        // 23:00 → 07:00 next day is 8 h of rest.
        let short = [
            Span {
                date: day(19),
                start: at(19, 15),
                end: at(19, 23),
            },
            Span {
                date: day(20),
                start: at(20, 7),
                end: at(20, 15),
            },
        ];
        assert_eq!(breaks(u, &short, &l)[0].kind, "rest");
        assert!(!fits(u, &short[..1], short[1], &l));
        let fine = Span {
            date: day(20),
            start: at(20, 12),
            end: at(20, 20),
        };
        assert!(fits(u, &short[..1], fine, &l));
        // Seven days in a Saturday week.
        let seven: Vec<Span> = (19..26)
            .map(|d| Span {
                date: day(d),
                start: at(d, 9),
                end: at(d, 13),
            })
            .collect();
        assert!(
            breaks(u, &seven, &l)
                .iter()
                .any(|b| b.kind == "weekly_rest")
        );
    }

    #[test]
    fn learning_decays_and_freezes() {
        assert!((decay(56.0) - 0.5).abs() < 1e-9);
        let u = Uuid::from_u128(1);
        let s = |value, age_days, manager| Signal {
            employee_id: u,
            late: true,
            value,
            age_days,
            manager,
        };
        let fit = learn(&[
            s(1.0, 0.0, false),
            s(1.0, 7.0, false),
            s(1.0, 14.0, false),
            s(-1.0, 0.0, true),
        ]);
        let f = &fit[&u];
        assert_eq!(f.events(true), 4);
        assert!(f.willing_late());
        assert!(f.score(true) > 0.0);
        assert_eq!(f.score(false), 0.0);
        assert!(frozen(1, 5));
        assert!(!frozen(2, 5));
        assert!(!frozen(0, 4));
    }
}
