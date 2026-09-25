//! The week (SC-*): a standing pattern fills each week, managers adjust dates
//! and publish, staff see only published weeks and are told of changes. Open
//! shifts, swaps, preferences, public holidays and coverage needs. The
//! suggestion engine and its guardrails are in [`super::suggest`].
//!
//! Every view here reads the one roster function
//! ([`crate::staff::schedules::resolve_range`]); every write goes through the
//! date-set writers in [`crate::staff::days`], inside one transaction with the
//! overlap check, and tells people only after it commits.

use std::collections::{BTreeSet, HashMap, HashSet};

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::{branches_of, employee_name, engine, notify, notify_managers, week_start};
use crate::authz::Cap;
use crate::errors::{AppError, AppErrorResponse};
use crate::staff::access;
use crate::staff::days::{self, Block};
use crate::staff::principal::{Me, caller};
use crate::staff::schedules::{DayTime, ResolvedShift, resolve_range};

pub use super::holidays::{
    __path_decide_holiday, HolidayDecision, HolidayView, decide_holiday, holidays_in,
};
pub use super::suggest::{
    __path_decide_suggestion, __path_fairness, __path_suggestions, DecideSuggestion, FairnessQuery,
    FairnessView, SuggestQuery, Suggestion, decide_suggestion, fairness, precompute, suggestions,
};

const MAX_DAYS: i64 = 62;

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RosterQuery {
    pub branch_id: Uuid,
    pub from: NaiveDate,
    pub to: NaiveDate,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MyRosterQuery {
    pub from: NaiveDate,
    pub to: NaiveDate,
}

#[derive(Serialize, ToSchema, Clone)]
pub struct RosterShift {
    pub employee_id: Uuid,
    pub employee_name: String,
    pub date: NaiveDate,
    pub branch_id: Uuid,
    pub work_shift_id: Uuid,
    pub shift_name: String,
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
    /// Effective wall-clock times at the branch (the assignment's own, else
    /// the block's for that weekday, else its default).
    pub start_time: NaiveTime,
    pub end_time: NaiveTime,
    /// Ends the next day.
    pub crosses_midnight: bool,
    /// This assignment has its own from/to (show it as edited).
    pub times_edited: bool,
    /// The date holds its own set, not the standing pattern.
    pub from_override: bool,
    /// Changed after its week was published (SC-4).
    pub changed: bool,
    /// On approved leave or a mission that day.
    pub on_leave: bool,
}

#[derive(Serialize, ToSchema, sqlx::FromRow, Clone)]
pub struct OpenShift {
    pub id: Uuid,
    pub branch_id: Uuid,
    pub work_shift_id: Uuid,
    pub shift_name: String,
    pub on_date: NaiveDate,
    /// `open` · `claimed` · `filled` · `cancelled`
    pub status: String,
    /// The employee who claimed it.
    pub claimed_by: Option<Uuid>,
    #[sqlx(default)]
    pub claimed_by_name: Option<String>,
    /// When the live claim was made; null while open.
    #[sqlx(default)]
    pub claimed_at: Option<DateTime<Utc>>,
    #[sqlx(default)]
    pub start_at: Option<DateTime<Utc>>,
    #[sqlx(default)]
    pub end_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, ToSchema, sqlx::FromRow, Clone)]
pub struct WorkShiftBrief {
    pub id: Uuid,
    pub name: String,
    pub branch_id: Option<Uuid>,
    pub start_time: NaiveTime,
    pub end_time: NaiveTime,
    pub crosses_midnight: bool,
    pub grace_minutes: i32,
    /// How long before its start a check-in opens — the window the server
    /// enforces (CL-3), so the app says "opens at" the same time.
    pub checkin_window_minutes: i32,
    /// Weekdays it may be rostered on (0 = Sunday … 6 = Saturday): offer it
    /// only on those.
    pub valid_days: Vec<i16>,
    /// Its own times on some weekdays; show that day's times.
    #[sqlx(skip)]
    pub day_times: Vec<DayTime>,
}

impl WorkShiftBrief {
    /// The block's times on `d`: that weekday's own, else its default.
    pub fn times_on(&self, d: NaiveDate) -> (NaiveTime, NaiveTime) {
        let dow = days::dow(d);
        self.day_times
            .iter()
            .find(|t| t.day_of_week == dow)
            .map_or((self.start_time, self.end_time), |t| {
                (t.start_time, t.end_time)
            })
    }

    pub fn valid_on(&self, d: NaiveDate) -> bool {
        self.valid_days.contains(&days::dow(d))
    }
}

#[derive(Serialize, ToSchema, sqlx::FromRow, Clone)]
pub struct RosterPerson {
    pub employee_id: Uuid,
    pub name: String,
    pub gender: Option<String>,
    pub pref_time: Option<String>,
    pub cant_work_days: Vec<i16>,
    pub department_id: Option<Uuid>,
    /// Who set the preferences last: `employee` or `manager` (SC-12).
    #[sqlx(default)]
    pub prefs_set_by: String,
}

/// A person's date that holds its own set (a date change), not the pattern.
#[derive(Serialize, ToSchema, sqlx::FromRow, Clone)]
pub struct DateSet {
    pub employee_id: Uuid,
    pub date: NaiveDate,
    /// The date is a day off by date change (it holds no shift).
    pub day_off: bool,
}

#[derive(Serialize, ToSchema)]
pub struct RosterView {
    pub branch_id: Uuid,
    pub from: NaiveDate,
    pub to: NaiveDate,
    /// Saturdays of the published weeks in range.
    pub published_weeks: Vec<NaiveDate>,
    pub shifts: Vec<RosterShift>,
    pub open_shifts: Vec<OpenShift>,
    pub work_shifts: Vec<WorkShiftBrief>,
    pub staff: Vec<RosterPerson>,
    pub holidays: Vec<HolidayView>,
    /// Labour limits the roster (or, for `overtime_day`, the clock) goes past.
    /// Warnings, never blocks (RU-13).
    pub warnings: Vec<engine::LabourWarning>,
    /// The limits are not yet confirmed by a lawyer; say so beside them.
    pub limits_unconfirmed: bool,
    /// The dates that hold their own set (a date change), a day off included:
    /// the ones "back to the pattern" applies to.
    #[serde(default)]
    pub date_sets: Vec<DateSet>,
}

pub(crate) fn check_range(from: NaiveDate, to: NaiveDate) -> Result<(), AppError> {
    if to < from || (to - from).num_days() > MAX_DAYS {
        return Err(AppError::BadRequest(format!(
            "The range must be 0–{MAX_DAYS} days"
        )));
    }
    Ok(())
}

pub(crate) async fn published_weeks(
    pool: &PgPool,
    branches: &[Uuid],
    from: NaiveDate,
    to: NaiveDate,
) -> Result<HashSet<(Uuid, NaiveDate)>, AppError> {
    let rows: Vec<(Uuid, NaiveDate)> = sqlx::query_as(
        "SELECT branch_id, week_start FROM staff_week_publications \
          WHERE branch_id = ANY($1) AND week_start BETWEEN $2 AND $3",
    )
    .bind(branches)
    .bind(week_start(from))
    .bind(to)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// The org's live blocks with their days and weekday times.
pub(crate) async fn work_shifts_of(
    pool: &PgPool,
    org_id: Uuid,
) -> Result<Vec<WorkShiftBrief>, AppError> {
    let mut rows: Vec<WorkShiftBrief> = sqlx::query_as(
        "SELECT id, name, branch_id, start_time, end_time, crosses_midnight, grace_minutes, \
                checkin_window_minutes, valid_days \
           FROM work_shifts WHERE org_id = $1 AND is_active ORDER BY start_time",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?;
    let times: Vec<(Uuid, i16, NaiveTime, NaiveTime)> = sqlx::query_as(
        "SELECT work_shift_id, day_of_week, start_time, end_time FROM work_shift_day_times \
          WHERE org_id = $1 ORDER BY day_of_week",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?;
    for w in &mut rows {
        w.day_times = times
            .iter()
            .filter(|t| t.0 == w.id)
            .map(|t| DayTime {
                day_of_week: t.1,
                start_time: t.2,
                end_time: t.3,
            })
            .collect();
    }
    Ok(rows)
}

pub(crate) async fn staff_at(
    pool: &PgPool,
    branch_id: Uuid,
) -> Result<Vec<RosterPerson>, AppError> {
    Ok(sqlx::query_as(
        "SELECT e.id AS employee_id, e.name, e.gender, e.pref_time, e.cant_work_days, \
                e.department_id, e.prefs_set_by \
           FROM employee_branches a \
           JOIN employees e ON e.id = a.employee_id AND e.employment_status = 'active' \
          WHERE a.branch_id = $1 ORDER BY lower(e.name), e.id",
    )
    .bind(branch_id)
    .fetch_all(pool)
    .await?)
}

/// Approved leave and missions per person and day.
pub(crate) async fn leave_days(
    pool: &PgPool,
    employees: &[Uuid],
    from: NaiveDate,
    to: NaiveDate,
) -> Result<HashSet<(Uuid, NaiveDate)>, AppError> {
    let rows: Vec<(Uuid, NaiveDate, NaiveDate)> = sqlx::query_as(
        "SELECT employee_id, on_date, COALESCE(end_date, on_date) FROM staff_requests \
          WHERE employee_id = ANY($1) AND status = 'approved' AND kind IN ('leave', 'mission') \
            AND on_date <= $3 AND COALESCE(end_date, on_date) >= $2",
    )
    .bind(employees)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;
    let mut out = HashSet::new();
    for (u, a, b) in rows {
        let mut d = a.max(from);
        while d <= b.min(to) {
            out.insert((u, d));
            d += Duration::days(1);
        }
    }
    Ok(out)
}

/// People's shifts over a range, from the one resolver (SC-6), as roster
/// rows. `branch`: keep only the shifts worked there.
pub(crate) async fn roster_rows(
    pool: &PgPool,
    people: &[(Uuid, String)],
    branch: Option<Uuid>,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<RosterShift>, AppError> {
    let ids: Vec<Uuid> = people.iter().map(|p| p.0).collect();
    let names: HashMap<Uuid, &str> = people.iter().map(|p| (p.0, p.1.as_str())).collect();
    let changed: HashSet<(Uuid, NaiveDate)> = sqlx::query_as::<_, (Uuid, NaiveDate)>(
        "SELECT employee_id, on_date FROM staff_roster_changes \
          WHERE employee_id = ANY($1) AND on_date BETWEEN $2 AND $3",
    )
    .bind(&ids)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    let leave = leave_days(pool, &ids, from, to).await?;
    Ok(resolve_range(pool, &ids, from, to, None)
        .await?
        .into_iter()
        .filter(|s| branch.is_none_or(|b| s.branch_id == Some(b)))
        .filter_map(|s| {
            let branch_id = s.branch_id?;
            Some(RosterShift {
                employee_id: s.employee_id,
                employee_name: names.get(&s.employee_id).copied().unwrap_or("").into(),
                date: s.on_date,
                branch_id,
                work_shift_id: s.work_shift_id,
                shift_name: s.name.clone(),
                start_at: s.scheduled_start_at,
                end_at: s.scheduled_end_at,
                start_time: s.start_time,
                end_time: s.end_time,
                crosses_midnight: s.crosses_midnight,
                times_edited: s.times_edited,
                from_override: s.from_override,
                changed: changed.contains(&(s.employee_id, s.on_date)),
                on_leave: leave.contains(&(s.employee_id, s.on_date)),
            })
        })
        .collect())
}

pub(crate) async fn open_shifts_at(
    pool: &PgPool,
    branches: &[Uuid],
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<OpenShift>, AppError> {
    let mut rows: Vec<OpenShift> = sqlx::query_as(
        "SELECT o.id, o.branch_id, o.work_shift_id, ws.name AS shift_name, o.on_date, o.status, \
                o.claimed_by, e.name AS claimed_by_name, o.claimed_at, \
                (o.on_date + COALESCE(dt.start_time, ws.start_time)) \
                    AT TIME ZONE COALESCE(b.timezone::text, org.timezone::text, 'Africa/Cairo') \
                    AS start_at, \
                (o.on_date + COALESCE(dt.end_time, ws.end_time) \
                    + CASE WHEN COALESCE(dt.end_time, ws.end_time) \
                                <= COALESCE(dt.start_time, ws.start_time) \
                           THEN INTERVAL '1 day' ELSE INTERVAL '0 day' END) \
                    AT TIME ZONE COALESCE(b.timezone::text, org.timezone::text, 'Africa/Cairo') \
                    AS end_at \
           FROM staff_open_shifts o \
           JOIN work_shifts ws ON ws.id = o.work_shift_id \
           JOIN branches b ON b.id = o.branch_id \
           JOIN organizations org ON org.id = o.org_id \
           LEFT JOIN work_shift_day_times dt ON dt.work_shift_id = ws.id \
                AND dt.day_of_week = EXTRACT(DOW FROM o.on_date)::smallint \
           LEFT JOIN employees e ON e.id = o.claimed_by \
          WHERE o.branch_id = ANY($1) AND o.on_date BETWEEN $2 AND $3 \
            AND o.status IN ('open', 'claimed') \
          ORDER BY o.on_date, start_at",
    )
    .bind(branches)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;
    rows.dedup_by_key(|o| o.id);
    Ok(rows)
}

/// The manager's roster for one branch (SC-7, RO-6).
#[utoipa::path(
    get, path = "/staff/roster", tag = "staff", params(RosterQuery),
    responses((status = 200, body = RosterView), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn roster(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<RosterQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    check_range(query.from, query.to)?;
    let pool = pool.get_ref();
    access::require_at(pool, &claims, org_id, Cap::HrScheduleRead, query.branch_id).await?;
    let work_shifts = work_shifts_of(pool, org_id).await?;
    let staff = staff_at(pool, query.branch_id).await?;
    let people: Vec<(Uuid, String)> = staff
        .iter()
        .map(|p| (p.employee_id, p.name.clone()))
        .collect();
    // All their shifts, wherever worked: the limits count every hour, the
    // grid shows this branch's.
    let everywhere = roster_rows(pool, &people, None, query.from, query.to).await?;
    let published = published_weeks(pool, &[query.branch_id], query.from, query.to).await?;
    let holidays = holidays_in(pool, org_id, query.from, query.to).await?;
    let settings =
        crate::staff::attendance::load_settings(pool, org_id, Some(query.branch_id)).await?;
    let warnings =
        labour_warnings(pool, &settings, &staff, &everywhere, query.from, query.to).await?;
    let shifts = everywhere
        .into_iter()
        .filter(|s| s.branch_id == query.branch_id)
        .collect();
    let ids: Vec<Uuid> = staff.iter().map(|p| p.employee_id).collect();
    let date_sets: Vec<DateSet> = sqlx::query_as(
        "SELECT employee_id, on_date AS date, bool_and(work_shift_id IS NULL) AS day_off \
           FROM staff_schedule_overrides \
          WHERE employee_id = ANY($1) AND on_date BETWEEN $2 AND $3 \
          GROUP BY employee_id, on_date ORDER BY on_date, employee_id",
    )
    .bind(&ids)
    .bind(query.from)
    .bind(query.to)
    .fetch_all(pool)
    .await?;
    Ok(HttpResponse::Ok().json(RosterView {
        branch_id: query.branch_id,
        from: query.from,
        to: query.to,
        published_weeks: published.into_iter().map(|(_, w)| w).collect(),
        shifts,
        open_shifts: open_shifts_at(pool, &[query.branch_id], query.from, query.to).await?,
        work_shifts: work_shifts
            .into_iter()
            .filter(|w| w.branch_id.is_none_or(|b| b == query.branch_id))
            .collect(),
        staff,
        holidays,
        warnings,
        limits_unconfirmed: true,
        date_sets,
    }))
}

/// RU-13 over a roster range: the rostered limits per person, plus days whose
/// recorded overtime went past the daily cap.
async fn labour_warnings(
    pool: &PgPool,
    settings: &crate::staff::attendance::AttendanceSettings,
    staff: &[RosterPerson],
    shifts: &[RosterShift],
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<engine::LabourWarning>, AppError> {
    let limits = engine::Limits::of(settings);
    let mut out = Vec::new();
    for p in staff {
        let spans: Vec<engine::Span> = shifts
            .iter()
            .filter(|s| s.employee_id == p.employee_id && !s.on_leave)
            .map(|s| engine::Span {
                date: s.date,
                start: s.start_at,
                end: s.end_at,
            })
            .collect();
        out.extend(engine::breaks(p.employee_id, &spans, &limits));
    }
    let cap = (settings.limit_overtime_day_hours * rust_decimal::Decimal::from(60))
        .round()
        .try_into()
        .unwrap_or(i64::MAX);
    let ids: Vec<Uuid> = staff.iter().map(|p| p.employee_id).collect();
    let over: Vec<(Uuid, NaiveDate, i64)> = sqlx::query_as(
        "SELECT employee_id, business_date, SUM(overtime_minutes)::int8 FROM attendance_records \
          WHERE employee_id = ANY($1) AND business_date BETWEEN $2 AND $3 \
          GROUP BY 1, 2 HAVING SUM(overtime_minutes) > $4",
    )
    .bind(&ids)
    .bind(from)
    .bind(to)
    .bind(cap)
    .fetch_all(pool)
    .await?;
    out.extend(
        over.into_iter()
            .map(|(employee_id, date, minutes)| engine::LabourWarning {
                employee_id,
                date,
                kind: "overtime_day".into(),
                minutes,
                limit_minutes: cap,
            }),
    );
    Ok(out)
}

/// One of my claims on an open shift, and how it ended (SC-9, S-162): a
/// request like any other, so it stays in my Requests once decided.
#[derive(Serialize, ToSchema, sqlx::FromRow, Clone)]
pub struct MyClaim {
    pub id: Uuid,
    pub open_shift_id: Uuid,
    pub branch_id: Uuid,
    pub on_date: NaiveDate,
    pub work_shift_id: Uuid,
    pub shift_name: String,
    /// `pending` · `approved` · `declined` · `withdrawn`. A shift the
    /// manager took back while the claim waited is `declined`.
    pub status: String,
    pub claimed_at: DateTime<Utc>,
    /// When it was decided or withdrawn; null while pending, and on a claim
    /// decided before the server kept this history.
    pub decided_at: Option<DateTime<Utc>>,
}

/// My claims on dates in `from..=to`, and every pending one wherever it falls.
async fn my_claims_in(
    pool: &PgPool,
    employee_id: Uuid,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<MyClaim>, AppError> {
    Ok(sqlx::query_as(
        "SELECT c.id, c.open_shift_id, o.branch_id, o.on_date, o.work_shift_id, \
                ws.name AS shift_name, c.status, c.created_at AS claimed_at, c.decided_at \
           FROM staff_open_shift_claims c \
           JOIN staff_open_shifts o ON o.id = c.open_shift_id \
           JOIN work_shifts ws ON ws.id = o.work_shift_id \
          WHERE c.employee_id = $1 AND (o.on_date BETWEEN $2 AND $3 OR c.status = 'pending') \
          ORDER BY o.on_date, c.created_at",
    )
    .bind(employee_id)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?)
}

/// Close an open shift's pending claim (the log beside `staff_open_shifts`,
/// which keeps only the live claimer).
async fn close_claim(
    conn: &mut sqlx::PgConnection,
    open_shift_id: Uuid,
    status: &str,
    by: Option<Uuid>,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE staff_open_shift_claims SET status = $2, decided_at = now(), decided_by = $3 \
          WHERE open_shift_id = $1 AND status = 'pending'",
    )
    .bind(open_shift_id)
    .bind(status)
    .bind(by)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[derive(Serialize, ToSchema)]
pub struct MyRosterView {
    pub from: NaiveDate,
    pub to: NaiveDate,
    /// Only shifts in published weeks (SC-3).
    pub shifts: Vec<RosterShift>,
    /// Weeks in range that are not published yet at my branch.
    pub unpublished_weeks: Vec<NaiveDate>,
    /// Open shifts at my branches, in published weeks (SC-9).
    pub open_shifts: Vec<OpenShift>,
    /// My claims on open shifts, decided ones included: those on dates in
    /// range, and every pending one wherever it falls (SC-9, S-162).
    pub my_claims: Vec<MyClaim>,
    pub swaps: Vec<Swap>,
    /// Colleagues' published shifts at my branches — what a swap can be with.
    pub team: Vec<RosterShift>,
    pub pref_time: Option<String>,
    pub cant_work_days: Vec<i16>,
    /// `employee` or `manager`: who set my preferences last.
    pub prefs_set_by: String,
}

/// My published shifts, open shifts to claim, and my swaps.
#[utoipa::path(
    get, path = "/staff/me/roster", tag = "staff", params(MyRosterQuery),
    responses((status = 200, body = MyRosterView), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_roster(
    me: Me,
    pool: crate::db::Db,
    query: web::Query<MyRosterQuery>,
) -> Result<HttpResponse, AppError> {
    let employee_id = me.employee_id;
    let org_id = me.org_id;
    check_range(query.from, query.to)?;
    let pool = pool.get_ref();
    let branches = branches_of(pool, employee_id).await?;
    let Some(&home) = branches.first() else {
        return Err(AppError::BadRequest(
            "You have no branch yet — ask your manager.".into(),
        ));
    };
    let published = published_weeks(pool, &branches, query.from, query.to).await?;
    let name = employee_name(pool, employee_id).await;
    let shifts: Vec<RosterShift> =
        roster_rows(pool, &[(employee_id, name)], None, query.from, query.to)
            .await?
            .into_iter()
            .filter(|s| published.contains(&(s.branch_id, week_start(s.date))))
            .collect();
    let mut colleagues: Vec<(Uuid, String)> = Vec::new();
    for &b in &branches {
        for p in staff_at(pool, b).await? {
            if p.employee_id != employee_id && !colleagues.iter().any(|c| c.0 == p.employee_id) {
                colleagues.push((p.employee_id, p.name));
            }
        }
    }
    let team = roster_rows(pool, &colleagues, None, query.from, query.to)
        .await?
        .into_iter()
        .filter(|s| {
            branches.contains(&s.branch_id)
                && published.contains(&(s.branch_id, week_start(s.date)))
                && !s.on_leave
        })
        .collect();
    let mut unpublished = Vec::new();
    let mut w = week_start(query.from);
    while w <= query.to {
        if !published.contains(&(home, w)) {
            unpublished.push(w);
        }
        w += Duration::days(7);
    }
    let now = Utc::now();
    let open_shifts = open_shifts_at(pool, &branches, query.from, query.to)
        .await?
        .into_iter()
        // One that already started can't be claimed, so it isn't offered.
        .filter(|o| {
            (o.status == "open" && not_started(o, now)) || o.claimed_by == Some(employee_id)
        })
        .filter(|o| published.contains(&(o.branch_id, week_start(o.on_date))))
        .collect();
    let (pref_time, cant_work_days, prefs_set_by): (Option<String>, Vec<i16>, String) =
        sqlx::query_as(
            "SELECT pref_time, cant_work_days, prefs_set_by FROM employees WHERE id = $1",
        )
        .bind(employee_id)
        .fetch_one(pool)
        .await?;
    Ok(HttpResponse::Ok().json(MyRosterView {
        from: query.from,
        to: query.to,
        shifts,
        unpublished_weeks: unpublished,
        open_shifts,
        my_claims: my_claims_in(pool, employee_id, query.from, query.to).await?,
        swaps: swaps_of(pool, org_id, Some(employee_id), None, None, None).await?,
        team,
        pref_time,
        cant_work_days,
        prefs_set_by,
    }))
}

#[derive(Deserialize, ToSchema)]
pub struct PublishWeek {
    pub branch_id: Uuid,
    /// Any day of the week; it is rounded to its Saturday.
    pub week_start: NaiveDate,
}

/// Publish a week: staff see it and are told (SC-3), and the week's open
/// shifts are announced now that people can see them (SC-9).
#[utoipa::path(
    post, path = "/staff/roster/publish", tag = "staff", request_body = PublishWeek,
    responses((status = 204), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn publish(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<PublishWeek>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrSchedulePublish).await?;
    access::require_at(
        pool,
        &claims,
        org_id,
        Cap::HrSchedulePublish,
        body.branch_id,
    )
    .await?;
    let ws = week_start(body.week_start);
    let fresh = sqlx::query(
        "INSERT INTO staff_week_publications (org_id, branch_id, week_start, published_by) \
         VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
    )
    .bind(org_id)
    .bind(body.branch_id)
    .bind(ws)
    .bind(claims.user_id_safe().ok())
    .execute(pool)
    .await?
    .rows_affected();
    if fresh > 0 {
        // Each open shift is announced by its OWN date, one notice per date
        // (SC-9, N-031, Mac E2E R-B2) — as posting into a published week does.
        // One that already started is not (hunt B-H1-2).
        let now = Utc::now();
        let mut open_dates: Vec<NaiveDate> =
            open_shifts_at(pool, &[body.branch_id], ws, ws + Duration::days(6))
                .await?
                .into_iter()
                .filter(|o| o.status == "open" && not_started(o, now))
                .map(|o| o.on_date)
                .collect();
        open_dates.dedup();
        let own = own_employees(pool, org_id, &claims).await?;
        for p in staff_at(pool, body.branch_id).await? {
            notify(
                pool,
                org_id,
                p.employee_id,
                "staff.n_week_published",
                json!({ "date": ws }),
            )
            .await;
            if own.contains(&p.employee_id) {
                continue;
            }
            for d in &open_dates {
                notify(
                    pool,
                    org_id,
                    p.employee_id,
                    "staff.n_open_shift",
                    json!({ "date": d }),
                )
                .await;
            }
        }
    }
    Ok(HttpResponse::NoContent().finish())
}

/// Has `shift` started on `date` at `branch` — `date` plus that weekday's
/// start (else the block's), on the branch's clock, as [`open_shifts_at`]
/// shows it — at or before now?
async fn block_started(
    pool: &PgPool,
    branch_id: Uuid,
    shift_id: Uuid,
    date: NaiveDate,
) -> Result<bool, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT ($3::date + COALESCE(dt.start_time, ws.start_time)) \
                    AT TIME ZONE COALESCE(b.timezone::text, org.timezone::text, 'Africa/Cairo') \
                <= now() \
           FROM work_shifts ws \
           JOIN branches b ON b.id = $1 \
           JOIN organizations org ON org.id = b.org_id \
           LEFT JOIN work_shift_day_times dt ON dt.work_shift_id = ws.id \
                AND dt.day_of_week = EXTRACT(DOW FROM $3::date)::smallint \
          WHERE ws.id = $2",
    )
    .bind(branch_id)
    .bind(shift_id)
    .bind(date)
    .fetch_optional(pool)
    .await?
    .unwrap_or(false))
}

/// The caller's own employee record(s) in the org: whoever posts or
/// publishes an open shift isn't told to claim it (hunt B-H1-3).
async fn own_employees(
    pool: &PgPool,
    org_id: Uuid,
    claims: &crate::auth::jwt::Claims,
) -> Result<Vec<Uuid>, AppError> {
    let Ok(user) = claims.user_id_safe() else {
        return Ok(Vec::new());
    };
    Ok(
        sqlx::query_scalar("SELECT id FROM employees WHERE user_id = $1 AND org_id = $2")
            .bind(user)
            .bind(org_id)
            .fetch_all(pool)
            .await?,
    )
}

/// An open shift that already started can be neither posted nor claimed.
fn shift_started() -> AppError {
    AppError::Refused {
        code: "SHIFT_STARTED",
        reason: "That shift has already started.".into(),
    }
}

/// An open shift still worth offering: not yet started.
fn not_started(o: &OpenShift, now: DateTime<Utc>) -> bool {
    o.start_at.is_none_or(|s| s > now)
}

/// Is `date`'s week published at `branch`?
async fn is_published(pool: &PgPool, branch: Uuid, date: NaiveDate) -> Result<bool, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM staff_week_publications WHERE branch_id = $1 AND week_start = $2)",
    )
    .bind(branch)
    .bind(week_start(date))
    .fetch_one(pool)
    .await?)
}

// ── open shifts ────────────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct PostOpenShift {
    pub branch_id: Uuid,
    pub work_shift_id: Uuid,
    pub on_date: NaiveDate,
}

#[utoipa::path(
    post, path = "/staff/open-shifts", tag = "staff", request_body = PostOpenShift,
    responses((status = 201, body = OpenShift), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn post_open_shift(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<PostOpenShift>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrScheduleEdit).await?;
    access::require_at(pool, &claims, org_id, Cap::HrScheduleEdit, body.branch_id).await?;
    let info = days::block_info(&mut *pool.acquire().await?, org_id, body.work_shift_id).await?;
    // A switched-off block can't be claimed, so it can't be posted (E2E
    // B-ROTA-4): refused like a day edit on it.
    if !info.is_active {
        return Err(AppError::Coded {
            status: 400,
            code: "SHIFT_INACTIVE",
            reason: format!("{} is switched off.", info.name),
        });
    }
    if info.branch_id.is_some_and(|b| b != body.branch_id) {
        return Err(AppError::Coded {
            status: 400,
            code: "SHIFT_OTHER_BRANCH",
            reason: format!("{} is another branch's shift.", info.name),
        });
    }
    if !info.valid_days.contains(&days::dow(body.on_date)) {
        return Err(AppError::Coded {
            status: 400,
            code: "SHIFT_NOT_ON_DAY",
            reason: format!(
                "{} isn't a shift on {}.",
                info.name,
                body.on_date.format("%A")
            ),
        });
    }
    // Nobody could work it, and everyone would be told (hunt B-H1-2).
    if block_started(pool, body.branch_id, body.work_shift_id, body.on_date).await? {
        return Err(shift_started());
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO staff_open_shifts (org_id, branch_id, work_shift_id, on_date, posted_by) \
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(org_id)
    .bind(body.branch_id)
    .bind(body.work_shift_id)
    .bind(body.on_date)
    .bind(claims.user_id_safe().ok())
    .fetch_one(pool)
    .await?;
    // A draft week's open shift is announced when the week is published.
    if is_published(pool, body.branch_id, body.on_date).await? {
        let own = own_employees(pool, org_id, &claims).await?;
        for p in staff_at(pool, body.branch_id).await? {
            if own.contains(&p.employee_id) {
                continue;
            }
            notify(
                pool,
                org_id,
                p.employee_id,
                "staff.n_open_shift",
                json!({ "date": body.on_date }),
            )
            .await;
        }
    }
    let row = open_shifts_at(pool, &[body.branch_id], body.on_date, body.on_date)
        .await?
        .into_iter()
        .find(|o| o.id == id)
        .ok_or(AppError::Internal)?;
    Ok(HttpResponse::Created().json(row))
}

/// Claim an open shift; the manager approves the claim (SC-9). Only a
/// published week's, and only one that fits beside the person's own shifts.
#[utoipa::path(
    post, path = "/staff/open-shifts/{id}/claim", tag = "staff",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = OpenShift), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn claim_open_shift(
    me: Me,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let employee_id = me.employee_id;
    let org_id = me.org_id;
    let pool = pool.get_ref();
    let subject = access::subject(pool, org_id, employee_id).await?;
    let row: Option<(Uuid, Uuid, NaiveDate, String)> = sqlx::query_as(
        "SELECT branch_id, work_shift_id, on_date, status FROM staff_open_shifts \
          WHERE id = $1 AND org_id = $2 AND branch_id = ANY($3)",
    )
    .bind(*id)
    .bind(org_id)
    .bind(&subject.branches)
    .fetch_optional(pool)
    .await?;
    let Some((branch_id, shift_id, on_date, status)) = row else {
        return Err(AppError::NotFound("No open shift here.".into()));
    };
    if status != "open" {
        return Err(AppError::Refused {
            code: "ALREADY_CLAIMED",
            reason: "Someone already claimed that shift.".into(),
        });
    }
    if block_started(pool, branch_id, shift_id, on_date).await? {
        return Err(shift_started());
    }
    if !is_published(pool, branch_id, on_date).await? {
        return Err(AppError::Refused {
            code: "WEEK_NOT_PUBLISHED",
            reason: "That week isn't published yet.".into(),
        });
    }
    // Would it fit? Try it in a transaction that never commits.
    {
        let mut tx = pool.begin().await?;
        let block = Block {
            work_shift_id: shift_id,
            times: None,
        };
        days::validate_block(&mut tx, &subject, on_date, &block).await?;
        let already = resolve_range(&mut *tx, &[employee_id], on_date, on_date, None)
            .await?
            .iter()
            .any(|s| s.work_shift_id == shift_id);
        if already {
            return Err(AppError::Refused {
                code: "ALREADY_ROSTERED",
                reason: "You're already on that shift.".into(),
            });
        }
        days::add_block(&mut tx, org_id, employee_id, on_date, &block, None, None).await?;
        days::check_overlaps(&mut tx, employee_id, on_date, on_date).await?;
        tx.rollback().await?;
    }
    let mut tx = pool.begin().await?;
    let claimed: Option<Uuid> = sqlx::query_scalar(
        "UPDATE staff_open_shifts SET status = 'claimed', claimed_by = $2, claimed_at = now() \
          WHERE id = $1 AND org_id = $3 AND status = 'open' RETURNING id",
    )
    .bind(*id)
    .bind(employee_id)
    .bind(org_id)
    .fetch_optional(&mut *tx)
    .await?;
    if claimed.is_none() {
        // Lost the race to a colleague's claim.
        return Err(AppError::Refused {
            code: "ALREADY_CLAIMED",
            reason: "Someone already claimed that shift.".into(),
        });
    }
    // Logged with the same now() as claimed_at (one transaction).
    sqlx::query(
        "INSERT INTO staff_open_shift_claims (org_id, open_shift_id, employee_id, status) \
         VALUES ($1, $2, $3, 'pending')",
    )
    .bind(org_id)
    .bind(*id)
    .bind(employee_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let name = employee_name(pool, employee_id).await;
    notify_managers(
        pool,
        org_id,
        Some(branch_id),
        Cap::HrScheduleEdit,
        Some(employee_id),
        "staff.n_claim",
        json!({ "name": name, "date": on_date }),
    )
    .await;
    let row = open_shifts_at(pool, &[branch_id], on_date, on_date)
        .await?
        .into_iter()
        .find(|o| o.id == *id)
        .ok_or(AppError::Internal)?;
    Ok(HttpResponse::Ok().json(row))
}

/// Take back my claim while it waits (SC-9, S-162), as the one who asked can
/// cancel any pending request: the shift is open again, the claim stays in
/// my Requests as `withdrawn`, and the managers told of it hear. 409
/// `NO_PENDING_CLAIM` when I have no claim waiting on it, 409
/// `CLAIM_ALREADY_DECIDED` once it was approved or declined.
#[utoipa::path(
    post, path = "/staff/open-shifts/{id}/withdraw", tag = "staff",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = OpenShift), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn withdraw_claim(
    me: Me,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let (employee_id, org_id) = (me.employee_id, me.org_id);
    let pool = pool.get_ref();
    let subject = access::subject(pool, org_id, employee_id).await?;
    let found: Option<(Uuid, NaiveDate)> = sqlx::query_as(
        "SELECT branch_id, on_date FROM staff_open_shifts \
          WHERE id = $1 AND org_id = $2 AND branch_id = ANY($3)",
    )
    .bind(*id)
    .bind(org_id)
    .bind(&subject.branches)
    .fetch_optional(pool)
    .await?;
    let Some((branch_id, on_date)) = found else {
        return Err(AppError::NotFound("No open shift here.".into()));
    };
    let mut tx = pool.begin().await?;
    let reopened: Option<Uuid> = sqlx::query_scalar(
        "UPDATE staff_open_shifts SET status = 'open', claimed_by = NULL, claimed_at = NULL \
          WHERE id = $1 AND status = 'claimed' AND claimed_by = $2 RETURNING id",
    )
    .bind(*id)
    .bind(employee_id)
    .fetch_optional(&mut *tx)
    .await?;
    if reopened.is_none() {
        drop(tx);
        let last: Option<String> = sqlx::query_scalar(
            "SELECT status FROM staff_open_shift_claims \
              WHERE open_shift_id = $1 AND employee_id = $2 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(*id)
        .bind(employee_id)
        .fetch_optional(pool)
        .await?;
        return Err(match last.as_deref() {
            Some("approved" | "declined") => AppError::Refused {
                code: "CLAIM_ALREADY_DECIDED",
                reason: "Your claim was already decided. Ask your manager to change it.".into(),
            },
            _ => AppError::Refused {
                code: "NO_PENDING_CLAIM",
                reason: "You have no claim waiting on this shift.".into(),
            },
        });
    }
    close_claim(&mut tx, *id, "withdrawn", None).await?;
    tx.commit().await?;
    // The same people the claim told (`claim_open_shift`).
    let name = employee_name(pool, employee_id).await;
    notify_managers(
        pool,
        org_id,
        Some(branch_id),
        Cap::HrScheduleEdit,
        Some(employee_id),
        "staff.n_claim_withdrawn",
        json!({ "name": name, "date": on_date }),
    )
    .await;
    let row = open_shifts_at(pool, &[branch_id], on_date, on_date)
        .await?
        .into_iter()
        .find(|o| o.id == *id)
        .ok_or(AppError::Internal)?;
    Ok(HttpResponse::Ok().json(row))
}

#[derive(Deserialize, ToSchema)]
pub struct DecideRoster {
    pub approve: bool,
}

/// Approve a claim: the shift becomes theirs for that date, beside the rest
/// of their day. Rejecting reopens it. One decision only.
#[utoipa::path(
    patch, path = "/staff/open-shifts/{id}/decision", tag = "staff", request_body = DecideRoster,
    params(("id" = Uuid, Path)),
    responses((status = 204), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn decide_claim(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<DecideRoster>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let by = claims.user_id_safe()?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrScheduleEdit).await?;
    let row: Option<(Uuid, Uuid, NaiveDate, Option<Uuid>)> = sqlx::query_as(
        "SELECT branch_id, work_shift_id, on_date, claimed_by FROM staff_open_shifts \
          WHERE id = $1 AND org_id = $2 AND status = 'claimed'",
    )
    .bind(*id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let Some((branch_id, shift_id, on_date, Some(claimer))) = row else {
        return Err(AppError::NotFound("No claim waiting here.".into()));
    };
    access::require_at(pool, &claims, org_id, Cap::HrScheduleEdit, branch_id).await?;
    let subject = access::subject(pool, org_id, claimer).await?;
    if subject.is(&claims) {
        return Err(AppError::Coded {
            status: 403,
            code: "OWN_CLAIM",
            reason: "You can't approve your own claim.".into(),
        });
    }
    let mut tx = pool.begin().await?;
    if body.approve {
        let won: Option<Uuid> = sqlx::query_scalar(
            "UPDATE staff_open_shifts SET status = 'filled', decided_by = $2 \
              WHERE id = $1 AND status = 'claimed' RETURNING id",
        )
        .bind(*id)
        .bind(by)
        .fetch_optional(&mut *tx)
        .await?;
        if won.is_none() {
            return Err(AppError::Conflict("That claim was already decided.".into()));
        }
        close_claim(&mut tx, *id, "approved", Some(by)).await?;
        let block = Block {
            work_shift_id: shift_id,
            times: None,
        };
        days::validate_block(&mut tx, &subject, on_date, &block).await?;
        days::add_block(
            &mut tx,
            org_id,
            claimer,
            on_date,
            &block,
            Some("Open shift claimed"),
            Some(by),
        )
        .await?;
        days::check_overlaps(&mut tx, claimer, on_date, on_date).await?;
        tx.commit().await?;
        // Marked as changed; the claim's own notice tells them.
        days::mark_changed_and_tell(pool, org_id, &BTreeSet::from([(claimer, on_date)]), false)
            .await?;
        notify(
            pool,
            org_id,
            claimer,
            "staff.n_claim_approved",
            json!({ "date": on_date }),
        )
        .await;
    } else {
        let won: Option<Uuid> = sqlx::query_scalar(
            "UPDATE staff_open_shifts SET status = 'open', claimed_by = NULL, claimed_at = NULL \
              WHERE id = $1 AND status = 'claimed' RETURNING id",
        )
        .bind(*id)
        .fetch_optional(&mut *tx)
        .await?;
        if won.is_none() {
            return Err(AppError::Conflict("That claim was already decided.".into()));
        }
        // The shift reopens, but the claimer's declined claim stays theirs.
        close_claim(&mut tx, *id, "declined", Some(by)).await?;
        tx.commit().await?;
        notify(
            pool,
            org_id,
            claimer,
            "staff.n_claim_rejected",
            json!({ "date": on_date }),
        )
        .await;
    }
    Ok(HttpResponse::NoContent().finish())
}

/// Take an open shift back (open or claimed, never filled). A claimer hears.
#[utoipa::path(
    post, path = "/staff/open-shifts/{id}/cancel", tag = "staff",
    params(("id" = Uuid, Path)),
    responses((status = 204), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn cancel_open_shift(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrScheduleEdit).await?;
    let row: Option<(Uuid, NaiveDate, String)> = sqlx::query_as(
        "SELECT branch_id, on_date, status FROM staff_open_shifts WHERE id = $1 AND org_id = $2",
    )
    .bind(*id)
    .bind(org_id)
    .fetch_optional(pool)
    .await?;
    let Some((branch_id, on_date, _)) = row else {
        return Err(AppError::NotFound("No open shift here.".into()));
    };
    access::require_at(pool, &claims, org_id, Cap::HrScheduleEdit, branch_id).await?;
    let by = claims.user_id_safe().ok();
    let mut tx = pool.begin().await?;
    let gone: Option<Option<Uuid>> = sqlx::query_scalar(
        "UPDATE staff_open_shifts SET status = 'cancelled', decided_by = $2 \
          WHERE id = $1 AND status IN ('open', 'claimed') \
          RETURNING claimed_by",
    )
    .bind(*id)
    .bind(by)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(claimer) = gone else {
        return Err(AppError::Conflict(
            "That shift was already filled or cancelled.".into(),
        ));
    };
    // A claim waiting on it ends declined, and stays in the claimer's Requests.
    close_claim(&mut tx, *id, "declined", by).await?;
    tx.commit().await?;
    if let Some(c) = claimer {
        notify(
            pool,
            org_id,
            c,
            "staff.n_open_shift_cancelled",
            json!({ "date": on_date }),
        )
        .await;
    }
    Ok(HttpResponse::NoContent().finish())
}

// ── swaps ─────────────────────────────────────────────────────────────────

#[derive(Serialize, ToSchema, sqlx::FromRow, Clone)]
pub struct Swap {
    pub id: Uuid,
    pub requester_id: Uuid,
    pub requester_name: String,
    pub requester_date: NaiveDate,
    pub requester_shift_id: Uuid,
    pub requester_shift_name: String,
    pub peer_id: Uuid,
    pub peer_name: String,
    pub peer_date: NaiveDate,
    pub peer_shift_id: Uuid,
    pub peer_shift_name: String,
    /// `awaiting_peer` · `pending` · `approved` · `rejected` · `cancelled`
    pub status: String,
    pub created_at: DateTime<Utc>,
}

/// Swaps in the org: one person's, or those touching a set of branches
/// (`None` = all), or the one `id`. Every open one (awaiting the colleague
/// or the manager) however old, and the newest 100 decided ones: a page of
/// history must never hide a swap still waiting (hunt H2-B6).
async fn swaps_of(
    pool: &PgPool,
    org_id: Uuid,
    person: Option<Uuid>,
    status: Option<&str>,
    branches: Option<&[Uuid]>,
    id: Option<Uuid>,
) -> Result<Vec<Swap>, AppError> {
    let select = format!(
        "SELECT s.id, s.requester_id, ru.name AS requester_name, s.requester_date, \
                s.requester_shift_id, rs.name AS requester_shift_name, s.peer_id, \
                pu.name AS peer_name, s.peer_date, s.peer_shift_id, ps.name AS peer_shift_name, \
                s.status, s.created_at \
           FROM staff_swaps s \
           JOIN employees ru ON ru.id = s.requester_id JOIN employees pu ON pu.id = s.peer_id \
           JOIN work_shifts rs ON rs.id = s.requester_shift_id \
           JOIN work_shifts ps ON ps.id = s.peer_shift_id \
          WHERE s.org_id = $1 \
            AND ($2::uuid IS NULL OR s.requester_id = $2 OR s.peer_id = $2) \
            AND ($3::text IS NULL OR s.status = $3) \
            AND ($5::uuid IS NULL OR s.id = $5) \
            AND ({} OR {})",
        access::in_scope("s.requester_id", 4),
        access::in_scope("s.peer_id", 4)
    );
    Ok(sqlx::query_as(&format!(
        "({select} AND s.status IN ('awaiting_peer', 'pending')) \
         UNION ALL \
         ({select} AND s.status NOT IN ('awaiting_peer', 'pending') \
          ORDER BY s.created_at DESC LIMIT 100) \
         ORDER BY created_at DESC"
    ))
    .bind(org_id)
    .bind(person)
    .bind(status)
    .bind(branches)
    .bind(id)
    .fetch_all(pool)
    .await?)
}

#[derive(Deserialize, ToSchema)]
pub struct AskSwap {
    /// The date of MY shift I give away.
    pub my_date: NaiveDate,
    /// MY shift (a block I'm rostered on that date).
    pub my_shift_id: Uuid,
    pub peer_id: Uuid,
    /// The date of the colleague's shift I take.
    pub peer_date: NaiveDate,
    /// The COLLEAGUE's shift.
    pub peer_shift_id: Uuid,
}

/// Is `employee` rostered on `shift` on `date` (in its own branch's zone)?
async fn rostered_on(
    conn: &mut sqlx::PgConnection,
    employee: Uuid,
    date: NaiveDate,
    shift: Uuid,
) -> Result<Option<ResolvedShift>, AppError> {
    Ok(resolve_range(&mut *conn, &[employee], date, date, None)
        .await?
        .into_iter()
        .find(|s| s.work_shift_id == shift))
}

/// The two sides of a swap applied to the roster: each gives their block and
/// takes the other's, the rest of both days unchanged (SC-8, SC-11).
async fn apply_swap(
    conn: &mut sqlx::PgConnection,
    org_id: Uuid,
    s: &Swap,
    requester: &access::Subject,
    peer: &access::Subject,
    by: Option<Uuid>,
) -> Result<(), AppError> {
    let mine = days::remove_block(
        conn,
        org_id,
        s.requester_id,
        s.requester_date,
        s.requester_shift_id,
        Some("Swap"),
        by,
    )
    .await?;
    let theirs = days::remove_block(
        conn,
        org_id,
        s.peer_id,
        s.peer_date,
        s.peer_shift_id,
        Some("Swap"),
        by,
    )
    .await?;
    let (Some(mine), Some(theirs)) = (mine, theirs) else {
        return Err(AppError::Refused {
            code: "SWAP_STALE",
            reason: "Those shifts aren't on the roster any more.".into(),
        });
    };
    let to_requester = Block {
        work_shift_id: s.peer_shift_id,
        times: theirs,
    };
    let to_peer = Block {
        work_shift_id: s.requester_shift_id,
        times: mine,
    };
    days::validate_block(conn, requester, s.peer_date, &to_requester).await?;
    days::validate_block(conn, peer, s.requester_date, &to_peer).await?;
    days::add_block(
        conn,
        org_id,
        s.requester_id,
        s.peer_date,
        &to_requester,
        Some("Swap"),
        by,
    )
    .await?;
    days::add_block(
        conn,
        org_id,
        s.peer_id,
        s.requester_date,
        &to_peer,
        Some("Swap"),
        by,
    )
    .await?;
    let (lo, hi) = (
        s.requester_date.min(s.peer_date),
        s.requester_date.max(s.peer_date),
    );
    days::check_overlaps(conn, s.requester_id, lo, hi).await?;
    days::check_overlaps(conn, s.peer_id, lo, hi).await?;
    Ok(())
}

/// 409 `SWAP_EXISTS`: this very swap is already asked and still open.
pub(crate) fn swap_exists() -> AppError {
    AppError::Refused {
        code: "SWAP_EXISTS",
        reason: "You've already asked for this swap — it's waiting.".into(),
    }
}

/// Ask a colleague to swap: they agree first, then the manager (SC-8). Both
/// shifts must be on the published roster, ahead, at a branch both work at,
/// and must fit where they land.
#[utoipa::path(
    post, path = "/staff/me/swaps", tag = "staff", request_body = AskSwap,
    responses((status = 201, body = Swap), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn ask_swap(
    me: Me,
    pool: crate::db::Db,
    body: web::Json<AskSwap>,
) -> Result<HttpResponse, AppError> {
    let employee_id = me.employee_id;
    let org_id = me.org_id;
    let pool = pool.get_ref();
    if body.peer_id == employee_id {
        return Err(AppError::BadRequest("Pick a colleague.".into()));
    }
    let requester = access::subject(pool, org_id, employee_id).await?;
    let peer = access::subject(pool, org_id, body.peer_id).await?;
    if peer.employment_status != "active" {
        return Err(AppError::NotFound("Employee not found".into()));
    }
    let mut conn = pool.acquire().await?;
    let mine = rostered_on(&mut conn, employee_id, body.my_date, body.my_shift_id).await?;
    let theirs = rostered_on(&mut conn, body.peer_id, body.peer_date, body.peer_shift_id).await?;
    drop(conn);
    let (Some(mine), Some(theirs)) = (mine, theirs) else {
        return Err(AppError::Conflict(
            "Those shifts aren't on the roster.".into(),
        ));
    };
    let now = Utc::now();
    if mine.scheduled_start_at <= now || theirs.scheduled_start_at <= now {
        return Err(AppError::Refused {
            code: "SWAP_STARTED",
            reason: "A shift that already started can't be swapped.".into(),
        });
    }
    for s in [&mine, &theirs] {
        let at = s.branch_id.unwrap_or_default();
        if !requester.branches.contains(&at) || !peer.branches.contains(&at) {
            return Err(AppError::Refused {
                code: "SWAP_OTHER_BRANCH",
                reason: "You can swap only with a colleague at the same branch.".into(),
            });
        }
        if !is_published(pool, at, s.on_date).await? {
            return Err(AppError::Refused {
                code: "WEEK_NOT_PUBLISHED",
                reason: "That week isn't published yet.".into(),
            });
        }
    }
    // The same swap, still open, is not asked again (SC-8, E2E B-ROTA-7);
    // the `staff_swaps_one_open` index settles a race.
    let open_already: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM staff_swaps WHERE requester_id = $1 AND requester_date = $2 \
            AND requester_shift_id = $3 AND peer_id = $4 AND peer_date = $5 AND peer_shift_id = $6 \
            AND status IN ('awaiting_peer', 'pending'))",
    )
    .bind(employee_id)
    .bind(body.my_date)
    .bind(body.my_shift_id)
    .bind(body.peer_id)
    .bind(body.peer_date)
    .bind(body.peer_shift_id)
    .fetch_one(pool)
    .await?;
    if open_already {
        return Err(swap_exists());
    }
    // Would it fit on both sides? Try it in a transaction that never commits.
    let draft = Swap {
        id: Uuid::nil(),
        requester_id: employee_id,
        requester_name: String::new(),
        requester_date: body.my_date,
        requester_shift_id: body.my_shift_id,
        requester_shift_name: mine.name.clone(),
        peer_id: body.peer_id,
        peer_name: String::new(),
        peer_date: body.peer_date,
        peer_shift_id: body.peer_shift_id,
        peer_shift_name: theirs.name.clone(),
        status: "awaiting_peer".into(),
        created_at: now,
    };
    let mut probe = pool.begin().await?;
    let fits = apply_swap(&mut probe, org_id, &draft, &requester, &peer, None).await;
    probe.rollback().await?;
    fits?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO staff_swaps (org_id, requester_id, requester_date, requester_shift_id, \
            peer_id, peer_date, peer_shift_id) VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(org_id)
    .bind(employee_id)
    .bind(body.my_date)
    .bind(body.my_shift_id)
    .bind(body.peer_id)
    .bind(body.peer_date)
    .bind(body.peer_shift_id)
    .fetch_one(pool)
    .await?;
    let name = employee_name(pool, employee_id).await;
    notify(
        pool,
        org_id,
        body.peer_id,
        "staff.n_swap_asked",
        json!({ "name": name }),
    )
    .await;
    let row = swaps_of(pool, org_id, Some(employee_id), None, None, Some(id))
        .await?
        .into_iter()
        .find(|s| s.id == id)
        .ok_or(AppError::Internal)?;
    Ok(HttpResponse::Created().json(row))
}

/// The colleague agrees or declines.
#[utoipa::path(
    patch, path = "/staff/me/swaps/{id}", tag = "staff", request_body = DecideRoster,
    params(("id" = Uuid, Path)),
    responses((status = 200, body = Swap), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn answer_swap(
    me: Me,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<DecideRoster>,
) -> Result<HttpResponse, AppError> {
    let employee_id = me.employee_id;
    let org_id = me.org_id;
    let pool = pool.get_ref();
    let row: Option<Uuid> = sqlx::query_scalar(
        "UPDATE staff_swaps SET status = $3 WHERE id = $1 AND peer_id = $2 \
            AND status = 'awaiting_peer' RETURNING requester_id",
    )
    .bind(*id)
    .bind(employee_id)
    .bind(if body.approve { "pending" } else { "rejected" })
    .fetch_optional(pool)
    .await?;
    let Some(requester) = row else {
        return Err(AppError::NotFound("No swap waiting for you here.".into()));
    };
    let name = employee_name(pool, employee_id).await;
    notify(
        pool,
        org_id,
        requester,
        if body.approve {
            "staff.n_swap_agreed"
        } else {
            "staff.n_swap_declined"
        },
        json!({ "name": name }),
    )
    .await;
    if body.approve {
        let branch = branches_of(pool, requester).await?.first().copied();
        notify_managers(
            pool,
            org_id,
            branch,
            Cap::HrScheduleEdit,
            None,
            "staff.n_swap_pending",
            json!({}),
        )
        .await;
    }
    let row = swaps_of(pool, org_id, Some(employee_id), None, None, Some(*id))
        .await?
        .into_iter()
        .find(|s| s.id == *id)
        .ok_or(AppError::Internal)?;
    Ok(HttpResponse::Ok().json(row))
}

/// The one who asked takes it back before the manager decides.
#[utoipa::path(
    post, path = "/staff/me/swaps/{id}/cancel", tag = "staff",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = Swap), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn cancel_swap(
    me: Me,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let pool = pool.get_ref();
    let peer: Option<Uuid> = sqlx::query_scalar(
        "UPDATE staff_swaps SET status = 'cancelled' WHERE id = $1 AND requester_id = $2 \
            AND status IN ('awaiting_peer', 'pending') \
          RETURNING peer_id",
    )
    .bind(*id)
    .bind(me.employee_id)
    .fetch_optional(pool)
    .await?;
    let Some(peer) = peer else {
        return Err(AppError::NotFound(
            "No swap of yours to cancel here.".into(),
        ));
    };
    let name = employee_name(pool, me.employee_id).await;
    notify(
        pool,
        me.org_id,
        peer,
        "staff.n_swap_cancelled",
        json!({ "name": name }),
    )
    .await;
    let row = swaps_of(pool, me.org_id, Some(me.employee_id), None, None, Some(*id))
        .await?
        .into_iter()
        .find(|s| s.id == *id)
        .ok_or(AppError::Internal)?;
    Ok(HttpResponse::Ok().json(row))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SwapQuery {
    #[serde(default)]
    pub status: Option<String>,
}

#[utoipa::path(
    get, path = "/staff/swaps", tag = "staff", params(SwapQuery),
    responses((status = 200, body = Vec<Swap>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_swaps(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<SwapQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let scope = access::scope(pool.get_ref(), &claims, org_id, Cap::HrScheduleEdit).await?;
    let rows = swaps_of(
        pool.get_ref(),
        org_id,
        None,
        query.status.as_deref(),
        scope.as_deref(),
        None,
    )
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct OpenShiftQuery {
    pub from: NaiveDate,
    pub to: NaiveDate,
}

/// Open shifts and their claims at the branches I run (SC-9) — the
/// dashboard's approvals queue and schedule.
#[utoipa::path(
    get, path = "/staff/open-shifts", tag = "staff", params(OpenShiftQuery),
    responses((status = 200, body = Vec<OpenShift>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn list_open_shifts(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<OpenShiftQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let branches = match access::scope(pool.get_ref(), &claims, org_id, Cap::HrScheduleEdit).await?
    {
        Some(b) => b,
        None => {
            sqlx::query_scalar("SELECT id FROM branches WHERE org_id = $1 AND deleted_at IS NULL")
                .bind(org_id)
                .fetch_all(pool.get_ref())
                .await?
        }
    };
    let rows = open_shifts_at(pool.get_ref(), &branches, query.from, query.to).await?;
    Ok(HttpResponse::Ok().json(rows))
}

/// The manager approves: both rosters update for those dates (SC-8), in one
/// transaction, once, and only if both shifts are still where they were.
#[utoipa::path(
    patch, path = "/staff/swaps/{id}/decision", tag = "staff", request_body = DecideRoster,
    params(("id" = Uuid, Path)),
    responses((status = 204), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn decide_swap(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<DecideRoster>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let by = claims.user_id_safe()?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrScheduleEdit).await?;
    let s = swaps_of(pool, org_id, None, Some("pending"), None, Some(*id))
        .await?
        .into_iter()
        .find(|s| s.id == *id)
        .ok_or_else(|| AppError::NotFound("No swap waiting here.".into()))?;
    // Both rosters change, so both sides' manager rights count (RO-6).
    let requester = access::subject(pool, org_id, s.requester_id).await?;
    let peer = access::subject(pool, org_id, s.peer_id).await?;
    access::require_for(pool, &claims, Cap::HrScheduleEdit, &requester).await?;
    access::require_for(pool, &claims, Cap::HrScheduleEdit, &peer).await?;
    if requester.is(&claims) || peer.is(&claims) {
        return Err(AppError::Coded {
            status: 403,
            code: "OWN_SWAP",
            reason: "You can't approve a swap you're part of.".into(),
        });
    }
    // Approving re-checks what the ask checked (Mac E2E R-B1, SC-8): time has
    // passed since, so a shift may have begun or its week been withdrawn. The
    // swap then stays pending and neither roster moves. (A shift no longer on
    // the roster is SWAP_STALE, from apply_swap.)
    if body.approve {
        let mut conn = pool.acquire().await?;
        let mine = rostered_on(
            &mut conn,
            s.requester_id,
            s.requester_date,
            s.requester_shift_id,
        )
        .await?;
        let theirs = rostered_on(&mut conn, s.peer_id, s.peer_date, s.peer_shift_id).await?;
        drop(conn);
        let now = Utc::now();
        for r in [&mine, &theirs].into_iter().flatten() {
            if r.scheduled_start_at <= now {
                return Err(AppError::Refused {
                    code: "SWAP_STARTED",
                    reason: "A shift that already started can't be swapped.".into(),
                });
            }
        }
        for r in [&mine, &theirs].into_iter().flatten() {
            if !is_published(pool, r.branch_id.unwrap_or_default(), r.on_date).await? {
                return Err(AppError::Refused {
                    code: "WEEK_NOT_PUBLISHED",
                    reason: "That week isn't published any more.".into(),
                });
            }
        }
    }
    let mut tx = pool.begin().await?;
    let won: Option<Uuid> = sqlx::query_scalar(
        "UPDATE staff_swaps SET status = $2, decided_by = $3 \
          WHERE id = $1 AND status = 'pending' RETURNING id",
    )
    .bind(*id)
    .bind(if body.approve { "approved" } else { "rejected" })
    .bind(by)
    .fetch_optional(&mut *tx)
    .await?;
    if won.is_none() {
        return Err(AppError::Conflict("That swap was already decided.".into()));
    }
    if body.approve {
        apply_swap(&mut tx, org_id, &s, &requester, &peer, Some(by)).await?;
    }
    tx.commit().await?;
    if body.approve {
        // Marked as changed; the approval notice below tells them.
        let touched = BTreeSet::from([
            (s.requester_id, s.requester_date),
            (s.requester_id, s.peer_date),
            (s.peer_id, s.peer_date),
            (s.peer_id, s.requester_date),
        ]);
        days::mark_changed_and_tell(pool, org_id, &touched, false).await?;
    }
    for who in [s.requester_id, s.peer_id] {
        notify(
            pool,
            org_id,
            who,
            if body.approve {
                "staff.n_swap_approved"
            } else {
                "staff.n_swap_rejected"
            },
            json!({}),
        )
        .await;
    }
    Ok(HttpResponse::NoContent().finish())
}

// ── preferences (SC-12) ────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct Preferences {
    /// `morning` · `evening` · null
    #[serde(default)]
    pub pref_time: Option<String>,
    /// Days I can't work: 0 = Sunday … 6 = Saturday.
    #[serde(default)]
    pub cant_work_days: Vec<i16>,
    /// Why (a manager's override).
    #[serde(default)]
    pub note: Option<String>,
}

fn check_preferences(body: &Preferences) -> Result<Vec<i16>, AppError> {
    if body
        .pref_time
        .as_deref()
        .is_some_and(|p| p != "morning" && p != "evening")
        || body.cant_work_days.iter().any(|d| !(0..=6).contains(d))
        || body
            .note
            .as_deref()
            .is_some_and(|n| n.chars().count() > 300)
    {
        return Err(AppError::BadRequest("Invalid preferences".into()));
    }
    let mut days = body.cant_work_days.clone();
    days.sort_unstable();
    days.dedup();
    Ok(days)
}

async fn save_preferences(
    pool: &PgPool,
    org_id: Uuid,
    employee_id: Uuid,
    body: &Preferences,
    source: &str,
    by: Option<Uuid>,
) -> Result<(), AppError> {
    let days = check_preferences(body)?;
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE employees SET pref_time = $2, cant_work_days = $3, prefs_set_by = $4 \
          WHERE id = $1",
    )
    .bind(employee_id)
    .bind(body.pref_time.as_deref())
    .bind(&days)
    .bind(source)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO staff_preference_log \
             (org_id, employee_id, source, pref_time, cant_work_days, changed_by, note) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(org_id)
    .bind(employee_id)
    .bind(source)
    .bind(body.pref_time.as_deref())
    .bind(&days)
    .bind(by)
    .bind(
        body.note
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty()),
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Preferred times and days I can't work; managers see them (SC-12). Logged.
#[utoipa::path(
    put, path = "/staff/me/preferences", tag = "staff", request_body = Preferences,
    responses((status = 204), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_preferences(
    me: Me,
    pool: crate::db::Db,
    body: web::Json<Preferences>,
) -> Result<HttpResponse, AppError> {
    save_preferences(
        pool.get_ref(),
        me.org_id,
        me.employee_id,
        &body,
        "employee",
        me.user_id,
    )
    .await?;
    Ok(HttpResponse::NoContent().finish())
}

/// A manager overrides someone's preferences (SC-12). Logged, and the
/// person is told.
#[utoipa::path(
    put, path = "/staff/employees/{id}/preferences", tag = "staff", request_body = Preferences,
    params(("id" = Uuid, Path)),
    responses((status = 204), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_employee_preferences(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<Preferences>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrStaffEdit).await?;
    let subject = access::subject(pool, org_id, *id).await?;
    access::require_for(pool, &claims, Cap::HrStaffEdit, &subject).await?;
    save_preferences(
        pool,
        org_id,
        subject.id,
        &body,
        "manager",
        claims.user_id_safe().ok(),
    )
    .await?;
    if !subject.is(&claims) {
        notify(pool, org_id, subject.id, "staff.n_prefs_changed", json!({})).await;
    }
    Ok(HttpResponse::NoContent().finish())
}

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct PreferenceChange {
    /// `employee` or `manager`.
    pub source: String,
    pub pref_time: Option<String>,
    pub cant_work_days: Vec<i16>,
    /// The manager's name, for a manager's change.
    pub changed_by_name: Option<String>,
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Who changed someone's preferences, and when (SC-12, newest first).
#[utoipa::path(
    get, path = "/staff/employees/{id}/preferences/log", tag = "staff",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = Vec<PreferenceChange>), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn preference_log(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrScheduleRead).await?;
    let subject = access::subject(pool, org_id, *id).await?;
    access::require_for(pool, &claims, Cap::HrScheduleRead, &subject).await?;
    let rows: Vec<PreferenceChange> = sqlx::query_as(
        "SELECT l.source, l.pref_time, l.cant_work_days, u.name AS changed_by_name, l.note, \
                l.created_at \
           FROM staff_preference_log l LEFT JOIN users u ON u.id = l.changed_by \
          WHERE l.employee_id = $1 ORDER BY l.created_at DESC LIMIT 50",
    )
    .bind(subject.id)
    .fetch_all(pool)
    .await?;
    Ok(HttpResponse::Ok().json(rows))
}

/// Does the org run `module` (`pos`, `dawam`)?
pub(crate) async fn has_module(
    pool: &PgPool,
    org_id: Uuid,
    module: &str,
) -> Result<bool, AppError> {
    Ok(
        sqlx::query_scalar("SELECT $2 = ANY(modules) FROM organizations WHERE id = $1")
            .bind(org_id)
            .bind(module)
            .fetch_optional(pool)
            .await?
            .unwrap_or(false),
    )
}

/// Average orders per weekday and hour over the last 8 weeks.
pub(crate) async fn pos_hourly(
    pool: &PgPool,
    branch_id: Uuid,
    tz: &str,
) -> Result<Vec<(i32, i32, f64)>, AppError> {
    Ok(sqlx::query_as(
        "SELECT EXTRACT(DOW FROM created_at AT TIME ZONE $2)::int, \
                EXTRACT(HOUR FROM created_at AT TIME ZONE $2)::int, COUNT(*)::float8 / 8 \
           FROM orders WHERE branch_id = $1 AND created_at > now() - INTERVAL '56 days' \
            AND status NOT IN ('voided', 'refunded') \
          GROUP BY 1, 2 ORDER BY 1, 2",
    )
    .bind(branch_id)
    .bind(tz)
    .fetch_all(pool)
    .await?)
}

// ── holidays (RU-10) ───────────────────────────────────────────────────────
// Suggested and decided in [`super::holidays`]; the roster shows them.

// ── Coverage needs (SC-13) ────────────────────────────────────────────────

/// One band of the weekly coverage grid.
#[derive(Serialize, Deserialize, ToSchema, sqlx::FromRow, Clone)]
pub struct CoverageNeed {
    /// 0 = Sunday … 6 = Saturday.
    pub day_of_week: i16,
    pub band_start: NaiveTime,
    pub band_end: NaiveTime,
    pub staff: i16,
    /// Only people of this department count toward it.
    #[serde(default)]
    pub department_id: Option<Uuid>,
}

async fn coverage_rows(pool: &PgPool, branch_id: Uuid) -> Result<Vec<CoverageNeed>, AppError> {
    Ok(sqlx::query_as(
        "SELECT day_of_week, band_start, band_end, staff, department_id \
           FROM staff_coverage_needs WHERE branch_id = $1 \
          ORDER BY day_of_week, band_start",
    )
    .bind(branch_id)
    .fetch_all(pool)
    .await?)
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CoverageQuery {
    pub branch_id: Uuid,
}

#[derive(Serialize, ToSchema)]
pub struct CoverageView {
    /// What the engine uses: `grid` (typed), `pos` (derived from sales) or
    /// `pattern` (the standing pattern's own coverage).
    pub source: String,
    /// The typed grid.
    pub needs: Vec<CoverageNeed>,
    /// What POS sales suggest, one-hour bands, when POS is on.
    pub derived: Vec<CoverageNeed>,
    pub orders_per_staff: i32,
}

#[utoipa::path(
    get, path = "/staff/roster/coverage", tag = "staff", params(CoverageQuery),
    responses((status = 200, body = CoverageView), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn get_coverage(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<CoverageQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::require_at(pool, &claims, org_id, Cap::HrScheduleRead, query.branch_id).await?;
    let settings =
        crate::staff::attendance::load_settings(pool, org_id, Some(query.branch_id)).await?;
    let needs = coverage_rows(pool, query.branch_id).await?;
    let mut derived = Vec::new();
    if has_module(pool, org_id, "pos").await? {
        let tz = crate::staff::branch_timezone(pool, query.branch_id).await?;
        for (dow, h, n) in pos_hourly(pool, query.branch_id, &tz).await? {
            let staff = engine::pos_need(n, settings.orders_per_staff);
            let start = NaiveTime::from_hms_opt(h as u32, 0, 0).expect("valid hour");
            if staff > 0 {
                derived.push(CoverageNeed {
                    day_of_week: dow as i16,
                    band_start: start,
                    band_end: NaiveTime::from_hms_opt(h as u32 + 1, 0, 0)
                        .unwrap_or(NaiveTime::from_hms_opt(23, 59, 59).expect("valid time")),
                    staff: staff as i16,
                    department_id: None,
                });
            }
        }
    }
    let source = if !needs.is_empty() {
        "grid"
    } else if !derived.is_empty() {
        "pos"
    } else {
        "pattern"
    };
    Ok(HttpResponse::Ok().json(CoverageView {
        source: source.into(),
        needs,
        derived,
        orders_per_staff: settings.orders_per_staff,
    }))
}

#[derive(Deserialize, ToSchema)]
pub struct PutCoverage {
    pub branch_id: Uuid,
    /// The whole grid; an empty list clears it.
    pub needs: Vec<CoverageNeed>,
}

#[utoipa::path(
    put, path = "/staff/roster/coverage", tag = "staff", request_body = PutCoverage,
    responses((status = 204), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_coverage(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<PutCoverage>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    access::gate(pool, &claims, org_id, Cap::HrScheduleEdit).await?;
    access::require_at(pool, &claims, org_id, Cap::HrScheduleEdit, body.branch_id).await?;
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM staff_coverage_needs WHERE branch_id = $1 AND org_id = $2")
        .bind(body.branch_id)
        .bind(org_id)
        .execute(&mut *tx)
        .await?;
    for n in &body.needs {
        sqlx::query(
            "INSERT INTO staff_coverage_needs \
                 (org_id, branch_id, day_of_week, band_start, band_end, staff, department_id) \
             VALUES ($1, $2, $3, $4, $5, $6, \
                 (SELECT id FROM departments WHERE id = $7 AND org_id = $1))",
        )
        .bind(org_id)
        .bind(body.branch_id)
        .bind(n.day_of_week)
        .bind(n.band_start)
        .bind(n.band_end)
        .bind(n.staff)
        .bind(n.department_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(HttpResponse::NoContent().finish())
}
