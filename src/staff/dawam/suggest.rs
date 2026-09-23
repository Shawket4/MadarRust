//! Roster suggestions (SC-13, `DAWAM_ROSTER_SUGGESTIONS.md`) and their
//! guardrails.
//!
//! Fill → find gaps → score → propose → learn:
//! - The week comes from the one roster function; approved leave is removed.
//! - Gaps against the branch's coverage need: the typed grid, else POS sales
//!   (Madar orgs), else the headcount the standing pattern intends.
//! - Candidates are scored: coverage decides which slot; then preferences, the
//!   small gender default, what was learned (employee side weighted above the
//!   manager side), reliability, a fair spread of late shifts over four weeks
//!   and continuity. Labour limits are never broken; overtime is not a target.
//! - A greedy fill, then a bounded local search (reassign and swap moves under
//!   a deterministic budget), in pure Rust on a blocking thread.
//! - Accept/reject, manual edits, filled claims, approved swaps and attendance
//!   teach it (8-week half-life). Learning freezes, per branch, when managers
//!   accept under 40% over 4 weeks; the owner is told on every change.
//! - A monthly fairness audit per branch compares night share by gender with
//!   stated willingness and flags a gap over 20 points; the owner is told.
//!
//! A day-scoped block is offered only on its days, at that day's times. A
//! person may take a second shift on a day (a split day, owner Q3) only with a
//! two-hour gap and within the labour limits.

use std::collections::{HashMap, HashSet};
use std::time::{Duration as StdDuration, Instant};

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::roster::{
    CoverageNeed, RosterPerson, WorkShiftBrief, has_module, leave_days, pos_hourly, staff_at,
    work_shifts_of,
};
use super::{engine, notify, owners, week_start};
use crate::authz::Cap;
use crate::errors::{AppError, AppErrorResponse};
use crate::staff::access;
use crate::staff::days::{self, Block};
use crate::staff::principal::caller;
use crate::staff::schedules::{after_day_change, pattern_range, resolve_range};

/// One on-demand recompute per branch-week per 30 s (the cache goes stale on
/// every roster change; within the window the stale week is served).
const RECOMPUTE_EVERY_SECS: i64 = 30;
/// The local search's budget.
const SEARCH_BUDGET: StdDuration = StdDuration::from_millis(250);
const SEARCH_MOVES: usize = 20_000;
/// A second shift on one day needs this much of a gap.
const SPLIT_GAP_MINUTES: i64 = 120;

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug, PartialEq)]
pub struct Suggestion {
    /// Opaque; send it back to accept or reject.
    pub id: String,
    pub date: NaiveDate,
    pub work_shift_id: Uuid,
    pub shift_name: String,
    /// Who the suggestion puts on the shift.
    pub employee_id: Uuid,
    pub employee_name: String,
    /// Who it takes off it, for a reassignment.
    pub from_employee_id: Option<Uuid>,
    pub from_employee_name: Option<String>,
    /// A core i18n key for the one-line reason, and its arguments.
    pub reason_key: String,
    pub reason_args: serde_json::Value,
    /// 0–100. Low (≤ 40) whenever the gender default decided it.
    pub confidence: i32,
    /// The gender default decided it: without it someone else would have
    /// been suggested (it says so).
    pub by_default: bool,
    /// The shift's times that day (a day-scoped block's own, else its default).
    #[serde(default)]
    pub start_time: Option<NaiveTime>,
    #[serde(default)]
    pub end_time: Option<NaiveTime>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SuggestQuery {
    pub branch_id: Uuid,
    pub week_start: NaiveDate,
}

// ── the pure solver ────────────────────────────────────────────────────────

/// A slot: a block on a date, with its span that day.
#[derive(Clone, Copy, Debug)]
struct Slot {
    date: NaiveDate,
    shift: usize,
    span: engine::Span,
}

/// Per person, per class (day = 0, late = 1): absence and lateness rates.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Reliability {
    pub absence: [f64; 2],
    pub late: [f64; 2],
}

/// Everything the solver needs, owned, so it runs on a blocking thread.
pub(crate) struct Problem {
    pub week: NaiveDate,
    pub tz: chrono_tz::Tz,
    pub staff: Vec<RosterPerson>,
    pub shifts: Vec<WorkShiftBrief>,
    pub late: Vec<bool>,
    pub limits: engine::Limits,
    pub gender_mode: String,
    /// Existing assignments this week ±1 day: person → spans (all branches).
    pub spans: HashMap<Uuid, Vec<engine::Span>>,
    /// (date, shift index) → who is on it at this branch.
    pub by_day: HashMap<(NaiveDate, usize), Vec<Uuid>>,
    /// Pattern-intended headcount per (date, shift index).
    pub pattern_need: HashMap<(NaiveDate, usize), i64>,
    /// Hourly need (date, hour, department) → staff; empty = use the pattern.
    pub need: HashMap<(NaiveDate, i32, Option<Uuid>), i32>,
    pub leave: HashSet<(Uuid, NaiveDate)>,
    pub fits: HashMap<Uuid, engine::Fit>,
    pub frozen: bool,
    pub reliability: HashMap<Uuid, Reliability>,
    /// Late shifts each person was rostered on over the 4 weeks before.
    pub late_history: HashMap<Uuid, i32>,
    /// (person, shift index) seen in the 4 weeks before: their usual shifts.
    pub usual: HashSet<(Uuid, usize)>,
    pub decided: HashSet<String>,
}

/// A candidate's score for a slot and whether the gender default decided it.
#[derive(Clone, Copy, Debug)]
struct Pick {
    person: usize,
    score: f64,
    default: f64,
}

struct Board {
    spans: HashMap<Uuid, Vec<engine::Span>>,
    late_count: HashMap<Uuid, i32>,
    on_shift: HashMap<(Uuid, usize), i32>,
    by_day: HashMap<(NaiveDate, usize), Vec<Uuid>>,
}

impl Board {
    fn assign(&mut self, p: Uuid, slot: &Slot, late: bool) {
        self.spans.entry(p).or_default().push(slot.span);
        if late {
            *self.late_count.entry(p).or_default() += 1;
        }
        *self.on_shift.entry((p, slot.shift)).or_default() += 1;
        self.by_day
            .entry((slot.date, slot.shift))
            .or_default()
            .push(p);
    }

    fn unassign(&mut self, p: Uuid, slot: &Slot, late: bool) {
        if let Some(v) = self.spans.get_mut(&p)
            && let Some(i) = v
                .iter()
                .position(|s| s.start == slot.span.start && s.end == slot.span.end)
        {
            v.remove(i);
        }
        if late && let Some(n) = self.late_count.get_mut(&p) {
            *n -= 1;
        }
        if let Some(n) = self.on_shift.get_mut(&(p, slot.shift)) {
            *n -= 1;
        }
        if let Some(v) = self.by_day.get_mut(&(slot.date, slot.shift))
            && let Some(i) = v.iter().position(|u| *u == p)
        {
            v.remove(i);
        }
    }
}

/// What the solver proposes, before it is dressed as a [`Suggestion`].
#[derive(Clone, Debug)]
struct Proposal {
    slot: Slot,
    person: usize,
    /// For a reassignment: who it takes the shift from.
    from: Option<Uuid>,
    reason_key: &'static str,
    reason_args: serde_json::Value,
    score: f64,
    by_default: bool,
    runner_up: Option<f64>,
}

impl Problem {
    fn slot(&self, date: NaiveDate, shift: usize) -> Option<Slot> {
        use chrono::TimeZone;
        let w = &self.shifts[shift];
        if !w.valid_on(date) {
            return None;
        }
        let (start, end) = w.times_on(date);
        let end_date = if end <= start {
            date + Duration::days(1)
        } else {
            date
        };
        Some(Slot {
            date,
            shift,
            span: engine::Span {
                date,
                start: self
                    .tz
                    .from_local_datetime(&date.and_time(start))
                    .earliest()?
                    .with_timezone(&Utc),
                end: self
                    .tz
                    .from_local_datetime(&end_date.and_time(end))
                    .earliest()?
                    .with_timezone(&Utc),
            },
        })
    }

    fn id_add(&self, slot: &Slot, p: Uuid) -> String {
        format!("add|{}|{}|{p}", slot.date, self.shifts[slot.shift].id)
    }

    fn id_move(&self, slot: &Slot, from: Uuid, p: Uuid) -> String {
        format!(
            "move|{}|{}|{from}|{p}",
            slot.date, self.shifts[slot.shift].id
        )
    }

    /// Everyone who may take `slot`, scored, best first. `id_of` names the
    /// suggestion a candidate would be (a decided one is not offered again).
    fn candidates(
        &self,
        board: &Board,
        slot: &Slot,
        dept: Option<Uuid>,
        id_of: &dyn Fn(Uuid) -> String,
    ) -> Vec<Pick> {
        let dow = days::dow(slot.date);
        let late = self.late[slot.shift];
        let class = usize::from(late);
        let mut out: Vec<Pick> = Vec::new();
        for (i, p) in self.staff.iter().enumerate() {
            let u = p.employee_id;
            if p.cant_work_days.contains(&dow)
                || self.leave.contains(&(u, slot.date))
                || dept.is_some_and(|x| p.department_id != Some(x))
                || self.decided.contains(&id_of(u))
                || board
                    .by_day
                    .get(&(slot.date, slot.shift))
                    .is_some_and(|v| v.contains(&u))
            {
                continue;
            }
            let empty = Vec::new();
            let spans = board.spans.get(&u).unwrap_or(&empty);
            // Never overlapping; a second shift that day only with a real gap
            // between (a split day, owner Q3).
            let gap = Duration::minutes(SPLIT_GAP_MINUTES);
            let same_day = spans.iter().any(|s| s.date == slot.date);
            let clash = spans.iter().any(|s| {
                let overlap = s.start < slot.span.end && slot.span.start < s.end;
                let near = s.date == slot.date
                    && s.start < slot.span.end + gap
                    && slot.span.start < s.end + gap;
                overlap || near
            });
            if clash {
                continue;
            }
            let learned = if self.frozen { None } else { self.fits.get(&u) };
            // Hard mode: late shifts only to women who said or showed they want them.
            if late
                && self.gender_mode == "hard"
                && p.gender.as_deref() == Some("f")
                && p.pref_time.as_deref() != Some("evening")
                && !learned.is_some_and(engine::Fit::willing_late)
            {
                continue;
            }
            if !engine::fits(u, spans, slot.span, &self.limits) {
                continue;
            }
            let stated = matches!(p.pref_time.as_deref(), Some("morning" | "evening"));
            let pref = match (p.pref_time.as_deref(), late) {
                (Some("morning"), true) | (Some("evening"), false) => -1.0,
                (Some("morning"), false) | (Some("evening"), true) => 1.0,
                _ => 0.0,
            };
            let events = learned.map_or(0, |f| f.events(late));
            let default = if late
                && self.gender_mode != "off"
                && !stated
                && events < 3
                && p.gender.as_deref() == Some("m")
            {
                engine::GENDER_DEFAULT
            } else {
                0.0
            };
            let fit = learned.map_or(0.0, |f| f.score(late));
            let rel = self.reliability.get(&u).copied().unwrap_or_default();
            let reliability = -0.5 * (rel.absence[class] + 0.5 * rel.late[class]);
            // Fair spread: this week's late shifts, plus the last four weeks'
            // at a quarter weight.
            let spread = if late {
                -0.1 * (f64::from(*board.late_count.get(&u).unwrap_or(&0))
                    + f64::from(*self.late_history.get(&u).unwrap_or(&0)) / 4.0)
            } else {
                0.0
            };
            let continuity = if board.on_shift.get(&(u, slot.shift)).is_some_and(|n| *n > 0)
                || self.usual.contains(&(u, slot.shift))
            {
                0.1
            } else {
                0.0
            };
            let second = if same_day { -0.3 } else { 0.0 };
            out.push(Pick {
                person: i,
                score: pref + default + fit + reliability + spread + continuity + second,
                default,
            });
        }
        // Deterministic: best score, then the staff order (by name).
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.person.cmp(&b.person)));
        out
    }

    /// The best candidate, whether the gender default decided it, and the
    /// runner-up's score.
    fn best(
        &self,
        board: &Board,
        slot: &Slot,
        dept: Option<Uuid>,
        id_of: &dyn Fn(Uuid) -> String,
    ) -> Option<(Pick, bool, Option<f64>)> {
        let c = self.candidates(board, slot, dept, id_of);
        let top = *c.first()?;
        // Would someone else win without the default? Then it decided.
        let without = c
            .iter()
            .map(|p| (p.person, p.score - p.default))
            .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)))
            .map(|x| x.0);
        let decided = top.default > 0.0 && without != Some(top.person);
        Some((top, decided, c.get(1).map(|p| p.score)))
    }

    fn fresh_board(&self) -> Board {
        let mut late_count: HashMap<Uuid, i32> = HashMap::new();
        let mut on_shift: HashMap<(Uuid, usize), i32> = HashMap::new();
        for ((_, shift), people) in &self.by_day {
            for u in people {
                *on_shift.entry((*u, *shift)).or_default() += 1;
                if self.late[*shift] {
                    *late_count.entry(*u).or_default() += 1;
                }
            }
        }
        Board {
            spans: self.spans.clone(),
            late_count,
            on_shift,
            by_day: self.by_day.clone(),
        }
    }

    fn dates(&self) -> Vec<NaiveDate> {
        (0..7).map(|i| self.week + Duration::days(i)).collect()
    }

    /// Greedy construction, then local search. Pure: no I/O, no clock but the
    /// search budget.
    pub(crate) fn solve(&self) -> Vec<Suggestion> {
        let mut board = self.fresh_board();
        let mut props: Vec<Proposal> = Vec::new();
        if self.need.is_empty() {
            self.fill_pattern_gaps(&mut board, &mut props);
        } else {
            self.fill_hourly(&mut board, &mut props);
        }
        self.move_off_cant_work(&mut board, &mut props);
        self.improve(&mut board, &mut props);
        props.into_iter().map(|p| self.dress(p)).collect()
    }

    fn fill_pattern_gaps(&self, board: &mut Board, props: &mut Vec<Proposal>) {
        for d in self.dates() {
            for w in 0..self.shifts.len() {
                let Some(slot) = self.slot(d, w) else {
                    continue;
                };
                let need = *self.pattern_need.get(&(d, w)).unwrap_or(&0);
                loop {
                    let have = board.by_day.get(&(d, w)).map_or(0, Vec::len) as i64;
                    if have >= need {
                        break;
                    }
                    let id_of = |u: Uuid| self.id_add(&slot, u);
                    let Some((pick, by_default, runner_up)) = self.best(board, &slot, None, &id_of)
                    else {
                        break;
                    };
                    let u = self.staff[pick.person].employee_id;
                    board.assign(u, &slot, self.late[w]);
                    props.push(Proposal {
                        slot,
                        person: pick.person,
                        from: None,
                        reason_key: "staff.sg_gap",
                        reason_args: json!({ "shift": self.shifts[w].name, "short": need - have }),
                        score: pick.score,
                        by_default,
                        runner_up,
                    });
                }
            }
        }
    }

    fn hours_of(&self, slot: &Slot) -> std::ops::Range<i32> {
        let (s, e) = self.shifts[slot.shift].times_on(slot.date);
        engine::shift_hours(s, e, e <= s)
    }

    fn covered(&self, board: &Board, d: NaiveDate, h: i32, dept: Option<Uuid>) -> i32 {
        let mut n = 0;
        for w in 0..self.shifts.len() {
            for (sd, hh) in [(d, h), (d - Duration::days(1), h + 24)] {
                let Some(slot) = self.slot(sd, w) else {
                    continue;
                };
                if self.hours_of(&slot).contains(&hh) {
                    n += board.by_day.get(&(sd, w)).map_or(0, |v| {
                        v.iter()
                            .filter(|u| {
                                dept.is_none_or(|x| {
                                    self.staff
                                        .iter()
                                        .find(|p| p.employee_id == **u)
                                        .and_then(|p| p.department_id)
                                        == Some(x)
                                })
                            })
                            .count() as i32
                    });
                }
            }
        }
        n
    }

    fn fill_hourly(&self, board: &mut Board, props: &mut Vec<Proposal>) {
        let mut keys: Vec<(NaiveDate, i32, Option<Uuid>)> = self.need.keys().copied().collect();
        keys.sort();
        for (d, h, dept) in keys {
            let want = self.need[&(d, h, dept)];
            loop {
                let short = want - self.covered(board, d, h, dept);
                if short <= 0 {
                    break;
                }
                // Shifts that cover this hour: starting today, or last night's.
                let mut options: Vec<(i32, Slot)> = Vec::new();
                for w in 0..self.shifts.len() {
                    for (sd, hh) in [(d, h), (d - Duration::days(1), h + 24)] {
                        if sd < self.week {
                            continue;
                        }
                        let Some(slot) = self.slot(sd, w) else {
                            continue;
                        };
                        let hours = self.hours_of(&slot);
                        if !hours.contains(&hh) {
                            continue;
                        }
                        let closes: i32 = hours
                            .map(|x| {
                                let (xd, xh) = if x >= 24 {
                                    (sd + Duration::days(1), x - 24)
                                } else {
                                    (sd, x)
                                };
                                self.need
                                    .get(&(xd, xh, dept))
                                    .map_or(0, |n| (n - self.covered(board, xd, xh, dept)).max(0))
                            })
                            .sum();
                        options.push((closes, slot));
                    }
                }
                options.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.shift.cmp(&b.1.shift)));
                let mut placed = false;
                for (_, slot) in options {
                    let id_of = |u: Uuid| self.id_add(&slot, u);
                    if let Some((pick, by_default, runner_up)) =
                        self.best(board, &slot, dept, &id_of)
                    {
                        let u = self.staff[pick.person].employee_id;
                        board.assign(u, &slot, self.late[slot.shift]);
                        props.push(Proposal {
                            slot,
                            person: pick.person,
                            from: None,
                            reason_key: "staff.sg_coverage",
                            reason_args: json!({
                                "shift": self.shifts[slot.shift].name,
                                "hour": format!("{h:02}:00"),
                                "short": short,
                            }),
                            score: pick.score,
                            by_default,
                            runner_up,
                        });
                        placed = true;
                        break;
                    }
                }
                if !placed {
                    break;
                }
            }
        }
    }

    /// Someone rostered on a day they said they can't work: suggest another.
    fn move_off_cant_work(&self, board: &mut Board, props: &mut Vec<Proposal>) {
        for d in self.dates() {
            let dow = days::dow(d);
            for w in 0..self.shifts.len() {
                let Some(slot) = self.slot(d, w) else {
                    continue;
                };
                let assigned = board.by_day.get(&(d, w)).cloned().unwrap_or_default();
                for uid in assigned {
                    let Some(person) = self.staff.iter().find(|p| p.employee_id == uid) else {
                        continue;
                    };
                    if !person.cant_work_days.contains(&dow) {
                        continue;
                    }
                    let id_of = |u: Uuid| self.id_move(&slot, uid, u);
                    if let Some((pick, by_default, runner_up)) =
                        self.best(board, &slot, None, &id_of)
                    {
                        let u = self.staff[pick.person].employee_id;
                        board.assign(u, &slot, self.late[w]);
                        props.push(Proposal {
                            slot,
                            person: pick.person,
                            from: Some(uid),
                            reason_key: "staff.sg_cant_work",
                            reason_args: json!({ "name": person.name }),
                            score: pick.score,
                            by_default,
                            runner_up,
                        });
                    }
                }
            }
        }
    }

    /// The fairness part of the objective: the spread of late shifts.
    fn spread_penalty(&self, board: &Board) -> f64 {
        0.05 * board
            .late_count
            .values()
            .map(|n| f64::from(*n).powi(2))
            .sum::<f64>()
    }

    /// Bounded local search over the proposals (design §3.5): move a
    /// proposal to another person, or swap two proposals' people, whenever
    /// the objective (candidate scores − late-shift spread) improves.
    /// Deterministic order, a time and move budget.
    #[allow(clippy::needless_range_loop)] // props[i] is replaced in place
    fn improve(&self, board: &mut Board, props: &mut [Proposal]) {
        let started = Instant::now();
        let mut moves = 0usize;
        let mut improved = true;
        while improved && moves < SEARCH_MOVES && started.elapsed() < SEARCH_BUDGET {
            improved = false;
            for i in 0..props.len() {
                if moves >= SEARCH_MOVES || started.elapsed() >= SEARCH_BUDGET {
                    return;
                }
                // Reassign i.
                let p = props[i].clone();
                let cur = self.staff[p.person].employee_id;
                let late = self.late[p.slot.shift];
                let before = self.spread_penalty(board);
                board.unassign(cur, &p.slot, late);
                let id_of = |u: Uuid| match p.from {
                    Some(f) => self.id_move(&p.slot, f, u),
                    None => self.id_add(&p.slot, u),
                };
                let dept = None;
                let cands = self.candidates(board, &p.slot, dept, &id_of);
                let current = cands.iter().find(|c| c.person == p.person).map(|c| c.score);
                let mut best: Option<(Pick, f64)> = None;
                for c in cands.iter().take(8) {
                    moves += 1;
                    if c.person == p.person {
                        continue;
                    }
                    let u = self.staff[c.person].employee_id;
                    board.assign(u, &p.slot, late);
                    let gain = c.score
                        - current.unwrap_or(f64::NEG_INFINITY)
                        - (self.spread_penalty(board) - before);
                    board.unassign(u, &p.slot, late);
                    if gain > 1e-9 && best.as_ref().is_none_or(|b| gain > b.1) {
                        best = Some((*c, gain));
                    }
                }
                match best {
                    Some((c, _)) if current.is_some() => {
                        let u = self.staff[c.person].employee_id;
                        board.assign(u, &p.slot, late);
                        props[i].person = c.person;
                        props[i].score = c.score;
                        props[i].by_default = false;
                        improved = true;
                    }
                    _ => board.assign(cur, &p.slot, late),
                }
            }
            // Swap the people of two same-class proposals on different days.
            for i in 0..props.len() {
                for j in i + 1..props.len() {
                    if moves >= SEARCH_MOVES || started.elapsed() >= SEARCH_BUDGET {
                        return;
                    }
                    moves += 1;
                    let (a, b) = (props[i].clone(), props[j].clone());
                    if a.person == b.person || a.from.is_some() || b.from.is_some() {
                        continue;
                    }
                    let (ua, ub) = (
                        self.staff[a.person].employee_id,
                        self.staff[b.person].employee_id,
                    );
                    let (la, lb) = (self.late[a.slot.shift], self.late[b.slot.shift]);
                    let before = a.score + b.score - self.spread_penalty(board);
                    board.unassign(ua, &a.slot, la);
                    board.unassign(ub, &b.slot, lb);
                    let score_of = |board: &Board, slot: &Slot, person: usize| {
                        self.candidates(board, slot, None, &|u| self.id_add(slot, u))
                            .into_iter()
                            .find(|c| c.person == person)
                    };
                    let na = score_of(board, &a.slot, b.person);
                    let applied = na.and_then(|na| {
                        board.assign(ub, &a.slot, la);
                        let nb = score_of(board, &b.slot, a.person);
                        match nb {
                            Some(nb) => {
                                board.assign(ua, &b.slot, lb);
                                let after = na.score + nb.score - self.spread_penalty(board);
                                if after > before + 1e-9 {
                                    Some((na, nb))
                                } else {
                                    board.unassign(ua, &b.slot, lb);
                                    board.unassign(ub, &a.slot, la);
                                    None
                                }
                            }
                            None => {
                                board.unassign(ub, &a.slot, la);
                                None
                            }
                        }
                    });
                    match applied {
                        Some((na, nb)) => {
                            props[i].person = b.person;
                            props[i].score = na.score;
                            props[i].by_default = false;
                            props[j].person = a.person;
                            props[j].score = nb.score;
                            props[j].by_default = false;
                            improved = true;
                        }
                        None => {
                            board.assign(ua, &a.slot, la);
                            board.assign(ub, &b.slot, lb);
                        }
                    }
                }
            }
        }
    }

    fn dress(&self, p: Proposal) -> Suggestion {
        let w = &self.shifts[p.slot.shift];
        let who = &self.staff[p.person];
        let (start, end) = w.times_on(p.slot.date);
        let from_name = p.from.and_then(|f| {
            self.staff
                .iter()
                .find(|x| x.employee_id == f)
                .map(|x| x.name.clone())
        });
        let base = if p.from.is_some() { 70.0 } else { 60.0 };
        let mut confidence = (base + 25.0 * p.score).clamp(30.0, 95.0);
        // A close call is not a confident one.
        if p.runner_up.is_some_and(|r| p.score - r < 0.2) {
            confidence = confidence.min(60.0);
        }
        if p.by_default {
            confidence = confidence.min(40.0);
        }
        Suggestion {
            id: match p.from {
                Some(f) => self.id_move(&p.slot, f, who.employee_id),
                None => self.id_add(&p.slot, who.employee_id),
            },
            date: p.slot.date,
            work_shift_id: w.id,
            shift_name: w.name.clone(),
            employee_id: who.employee_id,
            employee_name: who.name.clone(),
            from_employee_id: p.from,
            from_employee_name: from_name,
            reason_key: p.reason_key.into(),
            reason_args: p.reason_args,
            confidence: confidence as i32,
            by_default: p.by_default,
            start_time: Some(start),
            end_time: Some(end),
        }
    }
}

// ── loading the week ──────────────────────────────────────────────────────

/// Signals for learning (SC-13): per-person fit from 24 months of events.
async fn learned(
    pool: &PgPool,
    branch_id: Uuid,
    late_of: &HashMap<Uuid, bool>,
) -> Result<HashMap<Uuid, engine::Fit>, AppError> {
    // (person, shift, value, age in days, manager side)
    let rows: Vec<(Uuid, Uuid, f64, f64, bool)> = sqlx::query_as(
        "SELECT employee_id, work_shift_id, \
                CASE WHEN source = 'manual' THEN 0.5 ELSE 1.0 END \
                  * CASE WHEN accepted THEN 1.0 ELSE -1.0 END::float8, \
                EXTRACT(EPOCH FROM now() - created_at)::float8 / 86400, true \
           FROM staff_suggestion_events \
          WHERE branch_id = $1 AND employee_id IS NOT NULL AND work_shift_id IS NOT NULL \
            AND created_at > now() - INTERVAL '24 months' \
         UNION ALL \
         SELECT claimed_by, work_shift_id, 1.0, \
                EXTRACT(EPOCH FROM now() - COALESCE(claimed_at, created_at))::float8 / 86400, false \
           FROM staff_open_shifts \
          WHERE branch_id = $1 AND claimed_by IS NOT NULL AND status = 'filled' \
            AND created_at > now() - INTERVAL '24 months' \
         UNION ALL \
         SELECT x.uid, x.sid, x.v, EXTRACT(EPOCH FROM now() - s.created_at)::float8 / 86400, false \
           FROM staff_swaps s \
           JOIN employee_branches a ON a.employee_id = s.requester_id AND a.branch_id = $1 \
          CROSS JOIN LATERAL (VALUES (s.requester_id, s.peer_shift_id, 1.0::float8), \
                                     (s.requester_id, s.requester_shift_id, -1.0), \
                                     (s.peer_id, s.requester_shift_id, 1.0), \
                                     (s.peer_id, s.peer_shift_id, -1.0)) x(uid, sid, v) \
          WHERE s.status = 'approved' AND s.created_at > now() - INTERVAL '24 months' \
         UNION ALL \
         SELECT employee_id, work_shift_id, CASE WHEN status = 'absent' THEN -1.0 ELSE 1.0 END::float8, \
                EXTRACT(EPOCH FROM now() - business_date::timestamp)::float8 / 86400, false \
           FROM attendance_records \
          WHERE branch_id = $1 AND work_shift_id IS NOT NULL AND status <> 'on_leave' \
            AND covered_employee_id IS NULL \
            AND business_date > CURRENT_DATE - 365",
    )
    .bind(branch_id)
    .fetch_all(pool)
    .await?;
    let signals: Vec<engine::Signal> = rows
        .into_iter()
        .filter_map(|(employee_id, shift, value, age_days, manager)| {
            Some(engine::Signal {
                employee_id,
                late: *late_of.get(&shift)?,
                value,
                age_days,
                manager,
            })
        })
        .collect();
    Ok(engine::learn(&signals))
}

/// The branch's 4-week acceptance of SUGGESTIONS (manual edits don't count).
async fn acceptance_4w(pool: &PgPool, branch_id: Uuid) -> Result<(i64, i64), AppError> {
    Ok(sqlx::query_as(
        "SELECT COUNT(*) FILTER (WHERE accepted), COUNT(*) FROM staff_suggestion_events \
          WHERE branch_id = $1 AND source = 'suggestion' \
            AND created_at > now() - INTERVAL '28 days'",
    )
    .bind(branch_id)
    .fetch_one(pool)
    .await?)
}

/// Is learning frozen at this branch? Records the state, and tells the owners
/// when it flips (drift alarm, design §4.2).
pub(crate) async fn learning_frozen(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
) -> Result<bool, AppError> {
    let (accepted, decided) = acceptance_4w(pool, branch_id).await?;
    let frozen = engine::frozen(accepted, decided);
    let was: Option<bool> =
        sqlx::query_scalar("SELECT frozen FROM staff_learning_state WHERE branch_id = $1")
            .bind(branch_id)
            .fetch_optional(pool)
            .await?;
    if was != Some(frozen) {
        sqlx::query(
            "INSERT INTO staff_learning_state (branch_id, org_id, frozen) VALUES ($1, $2, $3) \
             ON CONFLICT (branch_id) DO UPDATE SET frozen = EXCLUDED.frozen, changed_at = now()",
        )
        .bind(branch_id)
        .bind(org_id)
        .bind(frozen)
        .execute(pool)
        .await?;
        // First sighting of a healthy branch is not news; a freeze or a thaw is.
        if frozen || was == Some(true) {
            let branch = branch_name(pool, branch_id).await?;
            for o in owners(pool, org_id).await? {
                notify(
                    pool,
                    org_id,
                    o,
                    if frozen {
                        "staff.n_learning_frozen"
                    } else {
                        "staff.n_learning_resumed"
                    },
                    json!({ "branch": branch, "accepted": accepted, "decided": decided }),
                )
                .await;
            }
        }
    }
    Ok(frozen)
}

async fn branch_name(pool: &PgPool, branch_id: Uuid) -> Result<String, AppError> {
    Ok(
        sqlx::query_scalar::<_, String>("SELECT name FROM branches WHERE id = $1")
            .bind(branch_id)
            .fetch_optional(pool)
            .await?
            .unwrap_or_default(),
    )
}

/// Hourly coverage need for the week: the typed grid, else (POS on) derived
/// from the last 8 weeks of orders. Empty = fall back to the pattern.
async fn coverage_need(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    ws: NaiveDate,
    tz: &str,
    orders_per_staff: i32,
) -> Result<HashMap<(NaiveDate, i32, Option<Uuid>), i32>, AppError> {
    let mut need = HashMap::new();
    let grid: Vec<CoverageNeed> = sqlx::query_as(
        "SELECT day_of_week, band_start, band_end, staff, department_id \
           FROM staff_coverage_needs WHERE branch_id = $1 ORDER BY day_of_week, band_start",
    )
    .bind(branch_id)
    .fetch_all(pool)
    .await?;
    let dates: Vec<NaiveDate> = (0..7).map(|i| ws + Duration::days(i)).collect();
    if !grid.is_empty() {
        for d in &dates {
            let dow = days::dow(*d);
            for g in grid.iter().filter(|g| g.day_of_week == dow) {
                for h in engine::band_hours(g.band_start, g.band_end) {
                    let e = need.entry((*d, h, g.department_id)).or_insert(0);
                    *e = (*e).max(i32::from(g.staff));
                }
            }
        }
        return Ok(need);
    }
    if !has_module(pool, org_id, "pos").await? {
        return Ok(need);
    }
    let rows = pos_hourly(pool, branch_id, tz).await?;
    for d in &dates {
        let dow = i32::from(days::dow(*d));
        for (_, h, n) in rows.iter().filter(|(x, _, _)| *x == dow) {
            let staff = engine::pos_need(*n, orders_per_staff);
            if staff > 0 {
                need.insert((*d, *h, None), staff);
            }
        }
    }
    Ok(need)
}

/// Load everything the solver needs for one branch-week.
pub(crate) async fn load_problem(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    ws: NaiveDate,
) -> Result<Problem, AppError> {
    let settings = crate::staff::attendance::load_settings(pool, org_id, Some(branch_id)).await?;
    let night = (settings.night_start, settings.night_end);
    let tz_name = crate::staff::branch_timezone(pool, branch_id).await?;
    let tz: chrono_tz::Tz = tz_name.parse().unwrap_or(chrono_tz::Africa::Cairo);
    let shifts: Vec<WorkShiftBrief> = work_shifts_of(pool, org_id)
        .await?
        .into_iter()
        .filter(|w| w.branch_id.is_none_or(|b| b == branch_id))
        .collect();
    // Late or night on any of its days is a late block.
    let late: Vec<bool> = shifts
        .iter()
        .map(|w| {
            (0..7).any(|i| {
                let d = ws + Duration::days(i);
                let (s, e) = w.times_on(d);
                w.valid_on(d) && engine::is_late(s, e, e <= s, night)
            })
        })
        .collect();
    let index: HashMap<Uuid, usize> = shifts.iter().enumerate().map(|(i, w)| (w.id, i)).collect();
    let late_of: HashMap<Uuid, bool> = shifts.iter().zip(&late).map(|(w, l)| (w.id, *l)).collect();
    let staff = staff_at(pool, branch_id).await?;
    let ids: Vec<Uuid> = staff.iter().map(|p| p.employee_id).collect();
    let to = ws + Duration::days(6);
    let leave = leave_days(pool, &ids, ws - Duration::days(1), to + Duration::days(1)).await?;

    // The week ±1 day for the limits (every branch), and this branch's slots.
    let mut spans: HashMap<Uuid, Vec<engine::Span>> = HashMap::new();
    let mut by_day: HashMap<(NaiveDate, usize), Vec<Uuid>> = HashMap::new();
    for s in resolve_range(
        pool,
        &ids,
        ws - Duration::days(1),
        to + Duration::days(1),
        None,
    )
    .await?
    {
        if leave.contains(&(s.employee_id, s.on_date)) {
            continue;
        }
        spans.entry(s.employee_id).or_default().push(engine::Span {
            date: s.on_date,
            start: s.scheduled_start_at,
            end: s.scheduled_end_at,
        });
        if s.on_date >= ws
            && s.on_date <= to
            && s.branch_id == Some(branch_id)
            && let Some(&i) = index.get(&s.work_shift_id)
        {
            by_day
                .entry((s.on_date, i))
                .or_default()
                .push(s.employee_id);
        }
    }
    // What the standing pattern intends (tiers applied, date changes ignored).
    let mut pattern_need: HashMap<(NaiveDate, usize), i64> = HashMap::new();
    for s in pattern_range(pool, &ids, ws, to, None).await? {
        if s.branch_id == Some(branch_id)
            && let Some(&i) = index.get(&s.work_shift_id)
        {
            *pattern_need.entry((s.on_date, i)).or_default() += 1;
        }
    }
    // The four weeks before: late-shift history and usual shifts.
    let mut late_history: HashMap<Uuid, i32> = HashMap::new();
    let mut usual: HashSet<(Uuid, usize)> = HashSet::new();
    for s in resolve_range(
        pool,
        &ids,
        ws - Duration::days(28),
        ws - Duration::days(1),
        None,
    )
    .await?
    {
        if let Some(&i) = index.get(&s.work_shift_id) {
            usual.insert((s.employee_id, i));
            if late[i] {
                *late_history.entry(s.employee_id).or_default() += 1;
            }
        }
    }
    // Reliability over 8 weeks: absence and lateness per class (design §3.4).
    let rows: Vec<(Uuid, Uuid, i64, i64, i64)> = sqlx::query_as(
        "SELECT employee_id, work_shift_id, COUNT(*), \
                COUNT(*) FILTER (WHERE status = 'absent'), \
                COUNT(*) FILTER (WHERE late_minutes > 0) \
           FROM attendance_records \
          WHERE employee_id = ANY($1) AND work_shift_id IS NOT NULL \
            AND covered_employee_id IS NULL AND status <> 'on_leave' \
            AND business_date >= $2 - 56 AND business_date < $2 \
          GROUP BY 1, 2",
    )
    .bind(&ids)
    .bind(ws)
    .fetch_all(pool)
    .await?;
    let mut totals: HashMap<(Uuid, usize), (i64, i64, i64)> = HashMap::new();
    for (u, shift, n, absent, late_n) in rows {
        let class = usize::from(*late_of.get(&shift).unwrap_or(&false));
        let e = totals.entry((u, class)).or_default();
        e.0 += n;
        e.1 += absent;
        e.2 += late_n;
    }
    let mut reliability: HashMap<Uuid, Reliability> = HashMap::new();
    for ((u, class), (n, absent, late_n)) in totals {
        // Shrunk toward 0 like the fits: two misses in two shifts is not 100%.
        let r = reliability.entry(u).or_default();
        r.absence[class] = absent as f64 / (n as f64 + 2.0);
        r.late[class] = late_n as f64 / (n as f64 + 2.0);
    }
    let fits = learned(pool, branch_id, &late_of).await?;
    let frozen = learning_frozen(pool, org_id, branch_id).await?;
    let decided: HashSet<String> = sqlx::query_scalar(
        "SELECT suggestion FROM staff_suggestion_events WHERE branch_id = $1 \
            AND source = 'suggestion' AND on_date BETWEEN $2 AND $3",
    )
    .bind(branch_id)
    .bind(ws)
    .bind(to)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    let need = coverage_need(
        pool,
        org_id,
        branch_id,
        ws,
        &tz_name,
        settings.orders_per_staff,
    )
    .await?;
    Ok(Problem {
        week: ws,
        tz,
        staff,
        shifts,
        late,
        limits: engine::Limits::of(&settings),
        gender_mode: settings.gender_mode.clone(),
        spans,
        by_day,
        pattern_need,
        need,
        leave,
        fits,
        frozen,
        reliability,
        late_history,
        usual,
        decided,
    })
}

/// Fill, find gaps, score, propose — the solve off the async executor.
async fn suggest(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    ws: NaiveDate,
) -> Result<Vec<Suggestion>, AppError> {
    let problem = load_problem(pool, org_id, branch_id, ws).await?;
    let decided = problem.decided.clone();
    let staff = problem.staff.clone();
    let shifts = problem.shifts.clone();
    let mut out = tokio::task::spawn_blocking(move || problem.solve())
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "roster solver panicked");
            AppError::Internal
        })?;
    out.extend(pattern_updates(pool, ws, &staff, &shifts, &decided).await?);
    Ok(out)
}

/// After 4 identical weeks of the same day edit, suggest making it the pattern.
async fn pattern_updates(
    pool: &PgPool,
    ws: NaiveDate,
    staff: &[RosterPerson],
    work_shifts: &[WorkShiftBrief],
    decided: &HashSet<String>,
) -> Result<Vec<Suggestion>, AppError> {
    let ids: Vec<Uuid> = staff.iter().map(|p| p.employee_id).collect();
    // (employee, weekday, shift or null = off) edited identically on each of the
    // 4 weeks before — and the ONLY change that date (a split day is not a
    // single-shift pattern). A swap or a claimed open shift is a trade between
    // colleagues, not what the manager wants the week to be: it doesn't count.
    let rows: Vec<(Uuid, i32, Option<Uuid>)> = sqlx::query_as(
        "SELECT employee_id, EXTRACT(DOW FROM on_date)::int, work_shift_id \
           FROM staff_schedule_overrides o \
          WHERE employee_id = ANY($1) AND on_date >= $2 - 28 AND on_date < $2 \
            AND start_time IS NULL \
            AND COALESCE(o.reason, '') NOT IN ('Swap', 'Open shift claimed') \
            AND (SELECT COUNT(*) FROM staff_schedule_overrides x \
                  WHERE x.employee_id = o.employee_id AND x.on_date = o.on_date) = 1 \
          GROUP BY 1, 2, 3 HAVING COUNT(DISTINCT on_date) = 4",
    )
    .bind(&ids)
    .bind(ws)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::new();
    for (uid, dow, shift) in rows {
        let d = ws + Duration::days(i64::from((dow + 1) % 7)); // Sat = 0
        // Already the pattern? Then there is nothing to suggest.
        let std: Vec<Uuid> = pattern_range(pool, &[uid], d, d, None)
            .await?
            .into_iter()
            .map(|s| s.work_shift_id)
            .collect();
        if std.as_slice() == shift.as_slice() {
            continue;
        }
        let shift_id = shift.unwrap_or_default();
        let id = format!("pattern|{d}|{shift_id}|{uid}");
        if decided.contains(&id) {
            continue;
        }
        let Some(p) = staff.iter().find(|p| p.employee_id == uid) else {
            continue;
        };
        let block = work_shifts.iter().find(|w| Some(w.id) == shift);
        if block.is_some_and(|w| !w.valid_on(d)) {
            continue;
        }
        let name = block.map(|w| w.name.clone());
        let times = block.map(|w| w.times_on(d));
        out.push(Suggestion {
            id,
            date: d,
            work_shift_id: shift_id,
            shift_name: name.clone().unwrap_or_default(),
            employee_id: uid,
            employee_name: p.name.clone(),
            from_employee_id: None,
            from_employee_name: None,
            reason_key: if shift.is_some() {
                "staff.sg_pattern"
            } else {
                "staff.sg_pattern_off"
            }
            .into(),
            reason_args: json!({ "name": p.name, "shift": name, "weeks": 4 }),
            confidence: 80,
            by_default: false,
            start_time: times.map(|t| t.0),
            end_time: times.map(|t| t.1),
        });
    }
    Ok(out)
}

#[utoipa::path(
    get, path = "/staff/roster/suggestions", tag = "staff", params(SuggestQuery),
    responses((status = 200, body = Vec<Suggestion>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn suggestions(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<SuggestQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    access::require_at(
        pool.get_ref(),
        &claims,
        org_id,
        Cap::HrScheduleEdit,
        query.branch_id,
    )
    .await?;
    let out = cached_suggestions(
        pool.get_ref(),
        org_id,
        query.branch_id,
        week_start(query.week_start),
    )
    .await?;
    Ok(HttpResponse::Ok().json(out))
}

/// The kept week when it is current, else recomputed — at most once per 30 s
/// per branch-week (a stale week is served inside the window). Decided ones
/// are left out.
async fn cached_suggestions(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    ws: NaiveDate,
) -> Result<Vec<Suggestion>, AppError> {
    let cached: Option<(serde_json::Value, bool, DateTime<Utc>)> = sqlx::query_as(
        "SELECT payload, stale, computed_at FROM staff_suggestion_cache \
          WHERE branch_id = $1 AND week_start = $2",
    )
    .bind(branch_id)
    .bind(ws)
    .fetch_optional(pool)
    .await?;
    let fresh = |at: DateTime<Utc>| (Utc::now() - at).num_seconds() < RECOMPUTE_EVERY_SECS;
    let usable = cached.and_then(|(v, stale, at)| {
        (!stale || fresh(at))
            .then(|| serde_json::from_value::<Vec<Suggestion>>(v).ok())
            .flatten()
    });
    let all = match usable {
        Some(v) => v,
        None => precompute(pool, org_id, branch_id, ws).await?,
    };
    let decided: HashSet<String> = sqlx::query_scalar(
        "SELECT suggestion FROM staff_suggestion_events WHERE branch_id = $1 \
            AND source = 'suggestion' AND on_date BETWEEN $2 AND $2 + 6",
    )
    .bind(branch_id)
    .bind(ws)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    Ok(all
        .into_iter()
        .filter(|s| !decided.contains(&s.id))
        .collect())
}

/// Compute one branch-week and keep it (the Wednesday 22:00 job and cache misses).
pub async fn precompute(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    ws: NaiveDate,
) -> Result<Vec<Suggestion>, AppError> {
    let out = suggest(pool, org_id, branch_id, ws).await?;
    sqlx::query(
        "INSERT INTO staff_suggestion_cache (org_id, branch_id, week_start, payload) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (branch_id, week_start) DO UPDATE SET payload = EXCLUDED.payload, \
             computed_at = now(), stale = false",
    )
    .bind(org_id)
    .bind(branch_id)
    .bind(ws)
    .bind(json!(out))
    .execute(pool)
    .await?;
    Ok(out)
}

/// Make `shift` (None = off) the standing pattern for `d`'s weekday from `d`
/// on. Rows covering that weekday end the day before; an every-day row is
/// split so the other weekdays keep it, except where they have their own.
async fn set_pattern_day(
    conn: &mut sqlx::PgConnection,
    org_id: Uuid,
    employee_id: Uuid,
    d: NaiveDate,
    shift: Option<Uuid>,
) -> Result<(), AppError> {
    let dow = days::dow(d);
    type Row = (Uuid, Uuid, Option<i16>, NaiveDate, Option<NaiveDate>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, work_shift_id, day_of_week, effective_from, effective_to \
           FROM staff_schedules WHERE employee_id = $1 AND org_id = $2 \
            AND (effective_to IS NULL OR effective_to >= $3) \
            AND (day_of_week IS NULL OR day_of_week = $4) FOR UPDATE",
    )
    .bind(employee_id)
    .bind(org_id)
    .bind(d)
    .bind(dow)
    .fetch_all(&mut *conn)
    .await?;
    for (id, ws_id, day, from, until) in rows {
        if day.is_none() {
            let start = from.max(d);
            for other in (0..7i16).filter(|x| *x != dow) {
                sqlx::query(
                    "INSERT INTO staff_schedules \
                         (org_id, employee_id, work_shift_id, day_of_week, effective_from, effective_to) \
                     SELECT $1, $2, $3, $4, $5, $6 WHERE NOT EXISTS ( \
                         SELECT 1 FROM staff_schedules WHERE employee_id = $2 AND day_of_week = $4 \
                            AND (effective_to IS NULL OR effective_to >= $5) \
                            AND ($6::date IS NULL OR effective_from <= $6))",
                )
                .bind(org_id)
                .bind(employee_id)
                .bind(ws_id)
                .bind(other)
                .bind(start)
                .bind(until)
                .execute(&mut *conn)
                .await?;
            }
        }
        if from >= d {
            sqlx::query("DELETE FROM staff_schedules WHERE id = $1")
                .bind(id)
                .execute(&mut *conn)
                .await?;
        } else {
            sqlx::query("UPDATE staff_schedules SET effective_to = $2 WHERE id = $1")
                .bind(id)
                .bind(d - Duration::days(1))
                .execute(&mut *conn)
                .await?;
        }
    }
    if let Some(shift) = shift {
        sqlx::query(
            "INSERT INTO staff_schedules (org_id, employee_id, work_shift_id, day_of_week, effective_from) \
             SELECT $1, $2, id, $4, $5 FROM work_shifts WHERE id = $3 AND org_id = $1",
        )
        .bind(org_id)
        .bind(employee_id)
        .bind(shift)
        .bind(dow)
        .bind(d)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

#[derive(Deserialize, ToSchema)]
pub struct DecideSuggestion {
    pub branch_id: Uuid,
    pub id: String,
    pub accept: bool,
}

/// Accept (changes that date only) or reject; either way it is remembered.
/// Only a suggestion the engine actually made for that branch-week is taken
/// (a crafted id is refused), and accepting touches only the block it names.
#[utoipa::path(
    post, path = "/staff/roster/suggestions/decide", tag = "staff", request_body = DecideSuggestion,
    responses((status = 204), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn decide_suggestion(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<DecideSuggestion>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let by = claims.user_id_safe()?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrScheduleEdit).await?;
    access::require_at(pool, &claims, org_id, Cap::HrScheduleEdit, body.branch_id).await?;
    let bad = || AppError::BadRequest("Unknown suggestion".into());
    let date: NaiveDate = body
        .id
        .split('|')
        .nth(1)
        .and_then(|d| d.parse().ok())
        .ok_or_else(bad)?;
    let s = cached_suggestions(pool, org_id, body.branch_id, week_start(date))
        .await?
        .into_iter()
        .find(|s| s.id == body.id)
        .ok_or_else(|| AppError::Refused {
            code: "SUGGESTION_STALE",
            reason: "That suggestion no longer applies — refresh the suggestions.".into(),
        })?;
    let pattern = s.id.starts_with("pattern|");
    // Everyone it names must be the branch's people of this org (RO-6).
    let to = access::subject(pool, org_id, s.employee_id).await?;
    access::require_for(pool, &claims, Cap::HrScheduleEdit, &to).await?;
    let from = match s.from_employee_id {
        Some(f) => {
            let f = access::subject(pool, org_id, f).await?;
            access::require_for(pool, &claims, Cap::HrScheduleEdit, &f).await?;
            Some(f)
        }
        None => None,
    };
    if body.accept {
        let mut tx = pool.begin().await?;
        if pattern {
            // The standing pattern itself: the one suggestion that changes it.
            set_pattern_day(
                &mut tx,
                org_id,
                to.id,
                s.date,
                (!s.work_shift_id.is_nil()).then_some(s.work_shift_id),
            )
            .await?;
            days::check_overlaps(&mut tx, to.id, s.date, s.date + Duration::days(14)).await?;
        } else {
            let block = Block {
                work_shift_id: s.work_shift_id,
                times: None,
            };
            let mut times = None;
            if let Some(f) = &from {
                times = days::remove_block(
                    &mut tx,
                    org_id,
                    f.id,
                    s.date,
                    s.work_shift_id,
                    Some("Suggestion accepted"),
                    Some(by),
                )
                .await?
                .ok_or_else(|| AppError::Refused {
                    code: "SUGGESTION_STALE",
                    reason: "That shift moved since — refresh the suggestions.".into(),
                })?;
            }
            let block = Block { times, ..block };
            days::validate_block(&mut tx, &to, s.date, &block).await?;
            days::add_block(
                &mut tx,
                org_id,
                to.id,
                s.date,
                &block,
                Some("Suggestion accepted"),
                Some(by),
            )
            .await?;
            days::check_overlaps(&mut tx, to.id, s.date, s.date).await?;
        }
        tx.commit().await?;
        if !pattern {
            if let Some(f) = &from {
                after_day_change(pool, org_id, f.id, s.date).await?;
            }
            after_day_change(pool, org_id, to.id, s.date).await?;
        }
    }
    sqlx::query(
        "INSERT INTO staff_suggestion_events (org_id, branch_id, suggestion, employee_id, on_date, \
            accepted, by_default, decided_by, work_shift_id, source) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, (SELECT id FROM work_shifts WHERE id = $9), \
            'suggestion')",
    )
    .bind(org_id)
    .bind(body.branch_id)
    .bind(&s.id)
    .bind(to.id)
    .bind(s.date)
    .bind(body.accept)
    .bind(s.by_default)
    .bind(by)
    .bind((!pattern).then_some(s.work_shift_id))
    .execute(pool)
    .await?;
    // A rejection asks for the next best, so the kept week is recomputed.
    sqlx::query(
        "UPDATE staff_suggestion_cache SET stale = true, computed_at = 'epoch' \
          WHERE branch_id = $1 AND week_start = $2",
    )
    .bind(body.branch_id)
    .bind(week_start(s.date))
    .execute(pool)
    .await?;
    Ok(HttpResponse::NoContent().finish())
}

// ── Fairness audit (SC-13 guardrail) ──────────────────────────────────────

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FairnessQuery {
    /// Any day of the month.
    pub month: NaiveDate,
    /// One branch; omit for every branch and the business as a whole.
    #[serde(default)]
    pub branch_id: Option<Uuid>,
}

#[derive(Serialize, Deserialize, ToSchema, sqlx::FromRow, Clone, Debug)]
pub struct FairnessRow {
    /// `m` · `f` · null (not set)
    pub gender: Option<String>,
    pub people: i64,
    /// Said they prefer evenings.
    pub willing: i64,
    pub shifts: i64,
    pub night_shifts: i64,
    /// Suggestions for people of this gender decided in the month, and
    /// accepted.
    #[sqlx(default)]
    #[serde(default)]
    pub suggested: i64,
    #[sqlx(default)]
    #[serde(default)]
    pub accepted: i64,
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct BranchFairness {
    pub branch_id: Uuid,
    pub branch_name: String,
    pub rows: Vec<FairnessRow>,
    /// The widest gap, in percentage points, between a gender's share of the
    /// night shifts and its share of stated willingness (of headcount when
    /// nobody stated any).
    pub gap_points: i64,
    /// Over 20 points (design §4.2).
    pub flagged: bool,
    /// Suggestions the gender default decided, of those decided in the month.
    pub by_default_decided: i64,
    pub decided: i64,
    pub accepted: i64,
    /// Learning is paused at this branch: under 40% accepted over 4 weeks.
    pub learning_frozen: bool,
}

#[derive(Serialize, ToSchema)]
pub struct FairnessView {
    pub month: NaiveDate,
    /// Night share by gender against stated willingness, the whole business.
    pub rows: Vec<FairnessRow>,
    /// Suggestions managers decided in the last 4 weeks, and how many they accepted.
    pub decided_4w: i64,
    pub accepted_4w: i64,
    /// Learning is paused at some branch.
    pub learning_frozen: bool,
    /// The same, branch by branch, each with its own gap flag.
    pub branches: Vec<BranchFairness>,
}

async fn fairness_rows(
    pool: &PgPool,
    org_id: Uuid,
    branch: Option<Uuid>,
    month: NaiveDate,
    night: (NaiveTime, NaiveTime),
) -> Result<Vec<FairnessRow>, AppError> {
    Ok(sqlx::query_as(
        "SELECT p.gender, COUNT(DISTINCT p.id) AS people, \
                COUNT(DISTINCT p.id) FILTER (WHERE p.pref_time = 'evening') AS willing, \
                COUNT(a.id) AS shifts, \
                COUNT(a.id) FILTER (WHERE dawam_night_minutes(a.scheduled_start_at, \
                    a.scheduled_end_at, br.timezone::text, $3, $4) > 0) AS night_shifts, \
                (SELECT COUNT(*) FROM staff_suggestion_events ev \
                   JOIN employees x ON x.id = ev.employee_id \
                  WHERE ev.org_id = $1 AND ev.source = 'suggestion' \
                    AND x.gender IS NOT DISTINCT FROM p.gender \
                    AND ($5::uuid IS NULL OR ev.branch_id = $5) \
                    AND ev.created_at >= $2 AND ev.created_at < ($2 + INTERVAL '1 month')) \
                    AS suggested, \
                (SELECT COUNT(*) FROM staff_suggestion_events ev \
                   JOIN employees x ON x.id = ev.employee_id \
                  WHERE ev.org_id = $1 AND ev.source = 'suggestion' AND ev.accepted \
                    AND x.gender IS NOT DISTINCT FROM p.gender \
                    AND ($5::uuid IS NULL OR ev.branch_id = $5) \
                    AND ev.created_at >= $2 AND ev.created_at < ($2 + INTERVAL '1 month')) \
                    AS accepted \
           FROM employees p \
           LEFT JOIN attendance_records a ON a.employee_id = p.id \
                AND a.business_date >= $2 AND a.business_date < ($2 + INTERVAL '1 month')::date \
                AND a.scheduled_start_at IS NOT NULL AND a.status <> 'on_leave' \
                AND a.covered_employee_id IS NULL \
                AND ($5::uuid IS NULL OR a.branch_id = $5) \
           LEFT JOIN branches br ON br.id = a.branch_id \
          WHERE p.org_id = $1 AND p.employment_status = 'active' \
            AND ($5::uuid IS NULL OR EXISTS (SELECT 1 FROM employee_branches eb \
                                             WHERE eb.employee_id = p.id AND eb.branch_id = $5)) \
          GROUP BY p.gender ORDER BY p.gender NULLS LAST",
    )
    .bind(org_id)
    .bind(month)
    .bind(night.0)
    .bind(night.1)
    .bind(branch)
    .fetch_all(pool)
    .await?)
}

/// The widest gap between a gender's night share and its willingness share.
pub(crate) fn gap_points(rows: &[FairnessRow]) -> i64 {
    let nights: i64 = rows.iter().map(|r| r.night_shifts).sum();
    if nights == 0 {
        return 0;
    }
    let willing: i64 = rows.iter().map(|r| r.willing).sum();
    let people: i64 = rows.iter().map(|r| r.people).sum();
    rows.iter()
        .filter(|r| r.gender.is_some())
        .map(|r| {
            let share = r.night_shifts as f64 / nights as f64;
            let base = if willing > 0 {
                r.willing as f64 / willing as f64
            } else if people > 0 {
                r.people as f64 / people as f64
            } else {
                0.0
            };
            ((share - base).abs() * 100.0).round() as i64
        })
        .max()
        .unwrap_or(0)
}

pub(crate) async fn branch_fairness(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    month: NaiveDate,
) -> Result<BranchFairness, AppError> {
    let settings = crate::staff::attendance::load_settings(pool, org_id, Some(branch_id)).await?;
    let rows = fairness_rows(
        pool,
        org_id,
        Some(branch_id),
        month,
        (settings.night_start, settings.night_end),
    )
    .await?;
    let (by_default_decided, decided, accepted): (i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*) FILTER (WHERE by_default), COUNT(*), COUNT(*) FILTER (WHERE accepted) \
           FROM staff_suggestion_events \
          WHERE branch_id = $1 AND source = 'suggestion' \
            AND created_at >= $2 AND created_at < ($2 + INTERVAL '1 month')",
    )
    .bind(branch_id)
    .bind(month)
    .fetch_one(pool)
    .await?;
    let gap = gap_points(&rows);
    Ok(BranchFairness {
        branch_id,
        branch_name: branch_name(pool, branch_id).await?,
        rows,
        gap_points: gap,
        flagged: gap > 20,
        by_default_decided,
        decided,
        accepted,
        learning_frozen: learning_frozen(pool, org_id, branch_id).await?,
    })
}

/// Owner only, monthly: who works the nights, by gender, against who said
/// they want them — for the business and branch by branch.
#[utoipa::path(
    get, path = "/staff/roster/fairness", tag = "staff", params(FairnessQuery),
    responses((status = 200, body = FairnessView), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn fairness(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<FairnessQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::require_everywhere(pool, &claims, org_id, Cap::HrRosterSettings).await?;
    let settings = crate::staff::attendance::load_settings(pool, org_id, None).await?;
    let month = query.month.with_day(1).unwrap_or(query.month);
    let rows = fairness_rows(
        pool,
        org_id,
        None,
        month,
        (settings.night_start, settings.night_end),
    )
    .await?;
    let (accepted, decided): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*) FILTER (WHERE accepted), COUNT(*) FROM staff_suggestion_events \
          WHERE org_id = $1 AND source = 'suggestion' AND created_at > now() - INTERVAL '28 days'",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await?;
    let ids: Vec<Uuid> =
        match query.branch_id {
            Some(b) => {
                access::require_at(pool, &claims, org_id, Cap::HrRosterSettings, b).await?;
                vec![b]
            }
            None => sqlx::query_scalar(
                "SELECT id FROM branches WHERE org_id = $1 AND deleted_at IS NULL ORDER BY name",
            )
            .bind(org_id)
            .fetch_all(pool)
            .await?,
        };
    let mut branches = Vec::new();
    for b in ids {
        branches.push(branch_fairness(pool, org_id, b, month).await?);
    }
    Ok(HttpResponse::Ok().json(FairnessView {
        month,
        rows,
        decided_4w: decided,
        accepted_4w: accepted,
        learning_frozen: branches.iter().any(|b| b.learning_frozen),
        branches,
    }))
}

/// The monthly audit (design §4.2): on the first days of a month, each live
/// branch's previous month is audited once, kept, and the owners are told —
/// "flagged" when a gap is over 20 points.
#[doc(hidden)]
pub async fn monthly_fairness(pool: &PgPool) -> Result<(), AppError> {
    let due: Vec<(Uuid, Uuid, NaiveDate)> = sqlx::query_as(
        "SELECT b.org_id, b.id, \
                (date_trunc('month', now() AT TIME ZONE COALESCE(b.timezone::text, o.timezone::text)) \
                    - INTERVAL '1 month')::date \
           FROM branches b JOIN organizations o ON o.id = b.org_id \
          WHERE o.is_active AND o.deleted_at IS NULL AND 'dawam' = ANY(o.modules) \
            AND b.deleted_at IS NULL \
            AND EXISTS (SELECT 1 FROM employee_branches a WHERE a.branch_id = b.id) \
            AND NOT EXISTS (SELECT 1 FROM staff_fairness_audits f WHERE f.branch_id = b.id \
                   AND f.month = (date_trunc('month', now() AT TIME ZONE \
                        COALESCE(b.timezone::text, o.timezone::text)) - INTERVAL '1 month')::date) \
          LIMIT 50",
    )
    .fetch_all(pool)
    .await?;
    for (org_id, branch_id, month) in due {
        let report = branch_fairness(pool, org_id, branch_id, month).await?;
        let fresh = sqlx::query(
            "INSERT INTO staff_fairness_audits (org_id, branch_id, month, flagged, payload) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
        )
        .bind(org_id)
        .bind(branch_id)
        .bind(month)
        .bind(report.flagged)
        .bind(json!(report))
        .execute(pool)
        .await?
        .rows_affected();
        if fresh == 0 {
            continue;
        }
        for o in owners(pool, org_id).await? {
            notify(
                pool,
                org_id,
                o,
                if report.flagged {
                    "staff.n_fairness_flagged"
                } else {
                    "staff.n_fairness_ready"
                },
                json!({
                    "branch": report.branch_name,
                    "month": month,
                    "gap": report.gap_points,
                }),
            )
            .await;
        }
    }
    Ok(())
}

#[derive(Serialize, ToSchema)]
pub struct FairnessAudit {
    pub branch_id: Uuid,
    pub month: NaiveDate,
    pub flagged: bool,
    pub computed_at: DateTime<Utc>,
    pub report: BranchFairness,
}

/// The kept monthly audits, newest first (owner).
#[utoipa::path(
    get, path = "/staff/roster/fairness/audits", tag = "staff",
    responses((status = 200, body = Vec<FairnessAudit>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn fairness_audits(
    req: HttpRequest,
    pool: crate::db::Db,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::require_everywhere(pool, &claims, org_id, Cap::HrRosterSettings).await?;
    let rows: Vec<(Uuid, NaiveDate, bool, DateTime<Utc>, serde_json::Value)> = sqlx::query_as(
        "SELECT branch_id, month, flagged, computed_at, payload FROM staff_fairness_audits \
          WHERE org_id = $1 ORDER BY month DESC, branch_id LIMIT 60",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?;
    let out: Vec<FairnessAudit> = rows
        .into_iter()
        .filter_map(|(branch_id, month, flagged, computed_at, payload)| {
            Some(FairnessAudit {
                branch_id,
                month,
                flagged,
                computed_at,
                report: serde_json::from_value(payload).ok()?,
            })
        })
        .collect();
    Ok(HttpResponse::Ok().json(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(h: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, 0, 0).unwrap()
    }

    fn person(name: &str, gender: Option<&str>, pref: Option<&str>) -> RosterPerson {
        RosterPerson {
            employee_id: Uuid::new_v4(),
            name: name.into(),
            gender: gender.map(Into::into),
            pref_time: pref.map(Into::into),
            cant_work_days: Vec::new(),
            department_id: None,
            prefs_set_by: "employee".into(),
        }
    }

    fn block(name: &str, start: NaiveTime, end: NaiveTime, days: &[i16]) -> WorkShiftBrief {
        WorkShiftBrief {
            id: Uuid::new_v4(),
            name: name.into(),
            branch_id: None,
            start_time: start,
            end_time: end,
            crosses_midnight: end <= start,
            grace_minutes: 15,
            checkin_window_minutes: 120,
            valid_days: days.to_vec(),
            day_times: Vec::new(),
        }
    }

    /// A Saturday.
    fn week() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 3).unwrap()
    }

    fn problem(staff: Vec<RosterPerson>, shifts: Vec<WorkShiftBrief>) -> Problem {
        let night = (t(22), t(6));
        let late = shifts
            .iter()
            .map(|w| engine::is_late(w.start_time, w.end_time, w.crosses_midnight, night))
            .collect();
        Problem {
            week: week(),
            tz: chrono_tz::UTC,
            staff,
            shifts,
            late,
            limits: engine::Limits {
                day: 8 * 60,
                week: 48 * 60,
                presence: 10 * 60,
                rest: 12 * 60,
            },
            gender_mode: "soft".into(),
            spans: HashMap::new(),
            by_day: HashMap::new(),
            pattern_need: HashMap::new(),
            need: HashMap::new(),
            leave: HashSet::new(),
            fits: HashMap::new(),
            frozen: false,
            reliability: HashMap::new(),
            late_history: HashMap::new(),
            usual: HashSet::new(),
            decided: HashSet::new(),
        }
    }

    fn span(p: &Problem, date: NaiveDate, shift: usize) -> engine::Span {
        p.slot(date, shift).unwrap().span
    }

    #[test]
    fn a_block_is_offered_only_on_its_days_at_that_days_times() {
        let all: Vec<i16> = (0..7).collect();
        let mut brunch = block("Brunch", t(10), t(14), &[6, 0, 1, 2, 3, 4]);
        brunch.day_times.push(crate::staff::schedules::DayTime {
            day_of_week: 0,
            start_time: t(11),
            end_time: t(15),
        });
        let staff = vec![person("Amal", None, None)];
        let mut p = problem(staff, vec![brunch, block("Day", t(8), t(12), &all)]);
        let fri = week() + Duration::days(6);
        let sun = week() + Duration::days(1);
        p.pattern_need.insert((fri, 0), 1);
        p.pattern_need.insert((sun, 0), 1);
        let out = p.solve();
        assert!(out.iter().all(|s| s.date != fri), "not a Friday shift");
        let on_sun = out.iter().find(|s| s.date == sun).unwrap();
        assert_eq!(
            (on_sun.start_time, on_sun.end_time),
            (Some(t(11)), Some(t(15)))
        );
    }

    #[test]
    fn a_second_shift_that_day_needs_a_two_hour_gap() {
        let all: Vec<i16> = (0..7).collect();
        let staff = vec![person("Amal", None, None)];
        let mut p = problem(
            staff,
            vec![
                block("Morning", t(8), t(11), &all),
                block("Lunch", t(12), t(15), &all),
                block("Evening", t(13), t(16), &all),
            ],
        );
        let d = week() + Duration::days(2);
        let u = p.staff[0].employee_id;
        p.spans.insert(u, vec![span(&p, d, 0)]);
        p.by_day.insert((d, 0), vec![u]);
        p.pattern_need.insert((d, 1), 1);
        p.pattern_need.insert((d, 2), 1);
        let out = p.solve();
        // Lunch starts an hour after the morning ends: too close. Evening,
        // two hours after: a split day.
        assert!(!out.iter().any(|s| s.work_shift_id == p.shifts[1].id));
        let ev = out
            .iter()
            .find(|s| s.work_shift_id == p.shifts[2].id)
            .unwrap();
        assert_eq!(ev.employee_id, u);
        assert_eq!(ev.id, format!("add|{d}|{}|{u}", p.shifts[2].id));
    }

    #[test]
    fn the_gender_default_decides_only_when_nothing_else_does_and_says_so() {
        let all: Vec<i16> = (0..7).collect();
        let d = week() + Duration::days(3);
        let staff = vec![
            person("Amal", Some("f"), None),
            person("Bassem", Some("m"), None),
        ];
        let mut p = problem(staff, vec![block("Late", t(16), t(23), &all)]);
        p.pattern_need.insert((d, 0), 1);
        let out = p.solve();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].employee_name, "Bassem");
        assert!(out[0].by_default, "the default decided it");
        assert!(out[0].confidence <= 40);

        // A stated preference outweighs it, and then it decided nothing.
        let staff = vec![
            person("Amal", Some("f"), Some("evening")),
            person("Bassem", Some("m"), None),
        ];
        let mut p = problem(staff, vec![block("Late", t(16), t(23), &all)]);
        p.pattern_need.insert((d, 0), 1);
        let out = p.solve();
        assert_eq!(out[0].employee_name, "Amal");
        assert!(!out[0].by_default);

        // Off: no default at all.
        let staff = vec![
            person("Amal", Some("f"), None),
            person("Bassem", Some("m"), None),
        ];
        let mut p = problem(staff, vec![block("Late", t(16), t(23), &all)]);
        p.gender_mode = "off".into();
        p.pattern_need.insert((d, 0), 1);
        let out = p.solve();
        assert!(!out[0].by_default);
        assert_eq!(
            out[0].employee_name, "Amal",
            "a tie goes to the staff order"
        );
    }

    #[test]
    fn a_labour_limit_is_never_broken_and_leave_and_cant_work_days_are_respected() {
        let all: Vec<i16> = (0..7).collect();
        let d = week() + Duration::days(3);
        let tired = person("Amal", None, None);
        let away = person("Bassem", None, None);
        let mut busy = person("Cyrine", None, None);
        busy.cant_work_days = vec![days::dow(d)];
        let mut p = problem(
            vec![tired, away, busy],
            vec![block("Day", t(9), t(17), &all)],
        );
        let (a, b) = (p.staff[0].employee_id, p.staff[1].employee_id);
        // Amal worked till 23:00 the night before: 12 h rest not met.
        let before = d - Duration::days(1);
        let late_before = engine::Span {
            date: before,
            start: before.and_time(t(15)).and_utc(),
            end: before.and_time(t(23)).and_utc(),
        };
        p.spans.insert(a, vec![late_before]);
        p.leave.insert((b, d));
        p.pattern_need.insert((d, 0), 1);
        let out = p.solve();
        assert!(out.is_empty(), "nobody may take it: {out:?}");
    }

    #[test]
    fn the_solve_is_deterministic_and_never_double_books() {
        let all: Vec<i16> = (0..7).collect();
        let staff: Vec<RosterPerson> = (0..6)
            .map(|i| {
                person(
                    &format!("P{i}"),
                    Some(if i % 2 == 0 { "f" } else { "m" }),
                    None,
                )
            })
            .collect();
        let mut p = problem(
            staff,
            vec![
                block("Morning", t(8), t(14), &all),
                block("Evening", t(15), t(21), &all),
                block("Night", t(22), t(6), &all),
            ],
        );
        for i in 0..7 {
            let d = week() + Duration::days(i);
            for w in 0..3 {
                p.pattern_need.insert((d, w), 1);
            }
        }
        let a = p.solve();
        let b = p.solve();
        assert_eq!(a, b);
        assert!(!a.is_empty());
        // Nobody twice on one slot, and no one person's two shifts overlap.
        let mut spans: HashMap<Uuid, Vec<engine::Span>> = HashMap::new();
        for s in &a {
            let w = p
                .shifts
                .iter()
                .position(|w| w.id == s.work_shift_id)
                .unwrap();
            let sp = span(&p, s.date, w);
            let mine = spans.entry(s.employee_id).or_default();
            assert!(
                mine.iter().all(|x| !(x.start < sp.end && sp.start < x.end)),
                "overlap for {}",
                s.employee_name
            );
            mine.push(sp);
        }
        for (u, list) in &spans {
            assert!(engine::breaks(*u, list, &p.limits).is_empty(), "{list:?}");
        }
    }
}
