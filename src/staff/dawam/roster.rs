//! The week (SC-*): a standing pattern fills each week, managers adjust dates
//! and publish, staff see only published weeks and are told of changes. Open
//! shifts, swaps, preferences, public holidays and roster suggestions.

use std::collections::{HashMap, HashSet};

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::{branches_of, engine, notify, notify_managers, user_name, week_start};
use crate::authz::Cap;
use crate::errors::{AppError, AppErrorResponse};
use crate::orgs::handlers::extract_claims;
use crate::staff::attendance::require_active_profile;
use crate::staff::schedules::resolve_shifts_for;

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
    pub user_id: Uuid,
    pub user_name: String,
    pub date: NaiveDate,
    pub branch_id: Uuid,
    pub work_shift_id: Uuid,
    pub shift_name: String,
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
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
    pub claimed_by: Option<Uuid>,
    #[sqlx(default)]
    pub claimed_by_name: Option<String>,
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
}

#[derive(Serialize, ToSchema, sqlx::FromRow, Clone)]
pub struct RosterPerson {
    pub user_id: Uuid,
    pub name: String,
    pub gender: Option<String>,
    pub pref_time: Option<String>,
    pub cant_work_days: Vec<i16>,
    pub department_id: Option<Uuid>,
}

#[derive(Serialize, ToSchema, sqlx::FromRow, Clone)]
pub struct HolidayView {
    pub on_date: NaiveDate,
    pub name_en: String,
    pub name_ar: String,
    /// null = not decided yet: a normal day unless set up (RU-10).
    pub decision: Option<String>,
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
}

fn check_range(from: NaiveDate, to: NaiveDate) -> Result<(), AppError> {
    if to < from || (to - from).num_days() > MAX_DAYS {
        return Err(AppError::BadRequest(format!(
            "The range must be 0–{MAX_DAYS} days"
        )));
    }
    Ok(())
}

async fn published_weeks(
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

async fn work_shifts_of(pool: &PgPool, org_id: Uuid) -> Result<Vec<WorkShiftBrief>, AppError> {
    Ok(sqlx::query_as(
        "SELECT id, name, branch_id, start_time, end_time, crosses_midnight, grace_minutes \
           FROM work_shifts WHERE org_id = $1 AND is_active ORDER BY start_time",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await?)
}

async fn staff_at(pool: &PgPool, branch_id: Uuid) -> Result<Vec<RosterPerson>, AppError> {
    Ok(sqlx::query_as(
        "SELECT u.id AS user_id, u.name, p.gender, p.pref_time, p.cant_work_days, p.department_id \
           FROM user_branch_assignments a \
           JOIN users u ON u.id = a.user_id AND u.deleted_at IS NULL AND u.is_active \
           JOIN staff_profiles p ON p.user_id = u.id AND p.employment_status = 'active' \
          WHERE a.branch_id = $1 ORDER BY lower(u.name)",
    )
    .bind(branch_id)
    .fetch_all(pool)
    .await?)
}

async fn on_leave_days(
    pool: &PgPool,
    user_id: Uuid,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<HashSet<NaiveDate>, AppError> {
    let rows: Vec<(NaiveDate, NaiveDate)> = sqlx::query_as(
        "SELECT on_date, COALESCE(end_date, on_date) FROM staff_requests \
          WHERE user_id = $1 AND status = 'approved' AND kind IN ('leave', 'mission') \
            AND on_date <= $3 AND COALESCE(end_date, on_date) >= $2",
    )
    .bind(user_id)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;
    let mut out = HashSet::new();
    for (a, b) in rows {
        let mut d = a;
        while d <= b {
            out.insert(d);
            d += Duration::days(1);
        }
    }
    Ok(out)
}

/// One person's shifts at a branch over a range: the one resolver (SC-6).
async fn shifts_of(
    pool: &PgPool,
    person: (Uuid, &str),
    branch_id: Uuid,
    shift_branch: &HashMap<Uuid, Option<Uuid>>,
    from: NaiveDate,
    to: NaiveDate,
    tz: &str,
) -> Result<Vec<RosterShift>, AppError> {
    let changed: HashSet<NaiveDate> = sqlx::query_scalar::<_, NaiveDate>(
        "SELECT on_date FROM staff_schedule_overrides \
          WHERE user_id = $1 AND changed_after_publish AND on_date BETWEEN $2 AND $3",
    )
    .bind(person.0)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    let leave = on_leave_days(pool, person.0, from, to).await?;
    let mut out = Vec::new();
    let mut d = from;
    while d <= to {
        for s in resolve_shifts_for(pool, person.0, d, tz).await? {
            let at = shift_branch.get(&s.work_shift_id).copied().flatten();
            if at.is_some_and(|b| b != branch_id) {
                continue;
            }
            out.push(RosterShift {
                user_id: person.0,
                user_name: person.1.to_string(),
                date: d,
                branch_id,
                work_shift_id: s.work_shift_id,
                shift_name: s.name.clone(),
                start_at: s.scheduled_start_at,
                end_at: s.scheduled_end_at,
                changed: changed.contains(&d),
                on_leave: leave.contains(&d),
            });
        }
        d += Duration::days(1);
    }
    Ok(out)
}

async fn open_shifts_at(
    pool: &PgPool,
    branches: &[Uuid],
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<OpenShift>, AppError> {
    let mut rows: Vec<OpenShift> = sqlx::query_as(
        "SELECT o.id, o.branch_id, o.work_shift_id, ws.name AS shift_name, o.on_date, o.status, \
                o.claimed_by, u.name AS claimed_by_name \
           FROM staff_open_shifts o \
           JOIN work_shifts ws ON ws.id = o.work_shift_id \
           LEFT JOIN users u ON u.id = o.claimed_by \
          WHERE o.branch_id = ANY($1) AND o.on_date BETWEEN $2 AND $3 \
            AND o.status IN ('open', 'claimed') \
          ORDER BY o.on_date",
    )
    .bind(branches)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;
    for o in &mut rows {
        let tz = crate::staff::branch_timezone(pool, o.branch_id).await?;
        let (start, end): (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
            "SELECT ($1::date + start_time) AT TIME ZONE $3, \
                    ($1::date + end_time + CASE WHEN crosses_midnight THEN INTERVAL '1 day' \
                                                ELSE INTERVAL '0 day' END) AT TIME ZONE $3 \
               FROM work_shifts WHERE id = $2",
        )
        .bind(o.on_date)
        .bind(o.work_shift_id)
        .bind(&tz)
        .fetch_one(pool)
        .await?;
        o.start_at = Some(start);
        o.end_at = Some(end);
    }
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
    let claims = extract_claims(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    check_range(query.from, query.to)?;
    let pool = pool.get_ref();
    crate::authz::scope::org_read_branches(pool, &claims, org_id, Some(query.branch_id)).await?;
    crate::authz::require::require(pool, &claims, Cap::HrScheduleRead, Some(query.branch_id))
        .await?;
    let tz = crate::staff::branch_timezone(pool, query.branch_id).await?;
    let work_shifts = work_shifts_of(pool, org_id).await?;
    let shift_branch: HashMap<Uuid, Option<Uuid>> =
        work_shifts.iter().map(|w| (w.id, w.branch_id)).collect();
    let staff = staff_at(pool, query.branch_id).await?;
    let mut shifts = Vec::new();
    for p in &staff {
        shifts.extend(
            shifts_of(
                pool,
                (p.user_id, &p.name),
                query.branch_id,
                &shift_branch,
                query.from,
                query.to,
                &tz,
            )
            .await?,
        );
    }
    let published = published_weeks(pool, &[query.branch_id], query.from, query.to).await?;
    let holidays = holidays_in(pool, org_id, query.from, query.to).await?;
    let settings =
        crate::staff::attendance::load_settings(pool, org_id, Some(query.branch_id)).await?;
    let warnings = labour_warnings(pool, &settings, &staff, &shifts, query.from, query.to).await?;
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
            .filter(|s| s.user_id == p.user_id && !s.on_leave)
            .map(|s| engine::Span {
                date: s.date,
                start: s.start_at,
                end: s.end_at,
            })
            .collect();
        out.extend(engine::breaks(p.user_id, &spans, &limits));
    }
    let cap = (settings.limit_overtime_day_hours * rust_decimal::Decimal::from(60))
        .round()
        .try_into()
        .unwrap_or(i64::MAX);
    let ids: Vec<Uuid> = staff.iter().map(|p| p.user_id).collect();
    let over: Vec<(Uuid, NaiveDate, i64)> = sqlx::query_as(
        "SELECT user_id, business_date, SUM(overtime_minutes)::int8 FROM attendance_records \
          WHERE user_id = ANY($1) AND business_date BETWEEN $2 AND $3 \
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
            .map(|(user_id, date, minutes)| engine::LabourWarning {
                user_id,
                date,
                kind: "overtime_day".into(),
                minutes,
                limit_minutes: cap,
            }),
    );
    Ok(out)
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
    pub swaps: Vec<Swap>,
    /// Colleagues' published shifts at my branches — what a swap can be with.
    pub team: Vec<RosterShift>,
    pub pref_time: Option<String>,
    pub cant_work_days: Vec<i16>,
}

/// My published shifts, open shifts to claim, and my swaps.
#[utoipa::path(
    get, path = "/staff/me/roster", tag = "staff", params(MyRosterQuery),
    responses((status = 200, body = MyRosterView), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn my_roster(
    req: HttpRequest,
    pool: crate::db::Db,
    query: web::Query<MyRosterQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let user_id = claims.user_id_safe()?;
    let org_id = require_active_profile(pool.get_ref(), user_id).await?;
    check_range(query.from, query.to)?;
    let pool = pool.get_ref();
    let branches = branches_of(pool, user_id).await?;
    let Some(&home) = branches.first() else {
        return Err(AppError::BadRequest(
            "You have no branch yet — ask your manager.".into(),
        ));
    };
    let tz = crate::staff::branch_timezone(pool, home).await?;
    let work_shifts = work_shifts_of(pool, org_id).await?;
    let shift_branch: HashMap<Uuid, Option<Uuid>> =
        work_shifts.iter().map(|w| (w.id, w.branch_id)).collect();
    let published = published_weeks(pool, &branches, query.from, query.to).await?;
    let name = user_name(pool, user_id).await;
    let mut shifts = Vec::new();
    for &b in &branches {
        for mut s in shifts_of(
            pool,
            (user_id, &name),
            b,
            &shift_branch,
            query.from,
            query.to,
            &tz,
        )
        .await?
        {
            let at = shift_branch
                .get(&s.work_shift_id)
                .copied()
                .flatten()
                .unwrap_or(home);
            s.branch_id = at;
            if published.contains(&(at, week_start(s.date)))
                && !shifts
                    .iter()
                    .any(|x: &RosterShift| x.date == s.date && x.work_shift_id == s.work_shift_id)
            {
                shifts.push(s);
            }
        }
    }
    let mut team = Vec::new();
    for &b in &branches {
        for p in staff_at(pool, b).await? {
            if p.user_id == user_id {
                continue;
            }
            for s in shifts_of(
                pool,
                (p.user_id, &p.name),
                b,
                &shift_branch,
                query.from,
                query.to,
                &tz,
            )
            .await?
            {
                if published.contains(&(b, week_start(s.date))) && !s.on_leave {
                    team.push(s);
                }
            }
        }
    }
    let mut unpublished = Vec::new();
    let mut w = week_start(query.from);
    while w <= query.to {
        if !published.contains(&(home, w)) {
            unpublished.push(w);
        }
        w += Duration::days(7);
    }
    let open_shifts = open_shifts_at(pool, &branches, query.from, query.to)
        .await?
        .into_iter()
        .filter(|o| published.contains(&(o.branch_id, week_start(o.on_date))))
        .collect();
    let (pref_time, cant_work_days): (Option<String>, Vec<i16>) =
        sqlx::query_as("SELECT pref_time, cant_work_days FROM staff_profiles WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(pool)
            .await?;
    Ok(HttpResponse::Ok().json(MyRosterView {
        from: query.from,
        to: query.to,
        shifts,
        unpublished_weeks: unpublished,
        open_shifts,
        swaps: swaps_of(pool, org_id, Some(user_id), None).await?,
        team,
        pref_time,
        cant_work_days,
    }))
}

/// After a date-level change: a published week tells the person (SC-4, SC-5).
pub(crate) async fn after_day_change(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    on_date: NaiveDate,
) -> Result<(), AppError> {
    let branches = branches_of(pool, user_id).await?;
    let published: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM staff_week_publications \
                        WHERE branch_id = ANY($1) AND week_start = $2)",
    )
    .bind(&branches)
    .bind(week_start(on_date))
    .fetch_one(pool)
    .await?;
    if published {
        sqlx::query(
            "UPDATE staff_schedule_overrides SET changed_after_publish = true \
              WHERE user_id = $1 AND on_date = $2",
        )
        .bind(user_id)
        .bind(on_date)
        .execute(pool)
        .await?;
        notify(
            pool,
            org_id,
            user_id,
            "staff.n_shift_changed",
            json!({ "date": on_date }),
        )
        .await;
    }
    Ok(())
}

async fn set_day(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    on_date: NaiveDate,
    work_shift_id: Option<Uuid>,
    reason: &str,
    by: Uuid,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO staff_schedule_overrides (org_id, user_id, on_date, work_shift_id, reason, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (user_id, on_date) DO UPDATE SET work_shift_id = EXCLUDED.work_shift_id, \
             reason = EXCLUDED.reason, created_by = EXCLUDED.created_by",
    )
    .bind(org_id)
    .bind(user_id)
    .bind(on_date)
    .bind(work_shift_id)
    .bind(reason)
    .bind(by)
    .execute(pool)
    .await?;
    after_day_change(pool, org_id, user_id, on_date).await
}

#[derive(Deserialize, ToSchema)]
pub struct PublishWeek {
    pub branch_id: Uuid,
    /// Any day of the week; it is rounded to its Saturday.
    pub week_start: NaiveDate,
}

/// Publish a week: staff see it and are told (SC-3).
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
    let claims = extract_claims(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    crate::authz::scope::org_read_branches(pool, &claims, org_id, Some(body.branch_id)).await?;
    crate::authz::require::require(pool, &claims, Cap::HrSchedulePublish, Some(body.branch_id))
        .await?;
    let ws = week_start(body.week_start);
    let fresh = sqlx::query(
        "INSERT INTO staff_week_publications (org_id, branch_id, week_start, published_by) \
         VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
    )
    .bind(org_id)
    .bind(body.branch_id)
    .bind(ws)
    .bind(claims.user_id())
    .execute(pool)
    .await?
    .rows_affected();
    if fresh > 0 {
        for p in staff_at(pool, body.branch_id).await? {
            notify(
                pool,
                org_id,
                p.user_id,
                "staff.n_week_published",
                json!({ "date": ws }),
            )
            .await;
        }
    }
    Ok(HttpResponse::NoContent().finish())
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
    let claims = extract_claims(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    crate::authz::scope::org_read_branches(pool, &claims, org_id, Some(body.branch_id)).await?;
    crate::authz::require::require(pool, &claims, Cap::HrScheduleEdit, Some(body.branch_id))
        .await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO staff_open_shifts (org_id, branch_id, work_shift_id, on_date, posted_by) \
         SELECT $1, $2, ws.id, $4, $5 FROM work_shifts ws WHERE ws.id = $3 AND ws.org_id = $1 \
         RETURNING id",
    )
    .bind(org_id)
    .bind(body.branch_id)
    .bind(body.work_shift_id)
    .bind(body.on_date)
    .bind(claims.user_id())
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::NotFound("Work shift not found".into()))?;
    let published: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM staff_week_publications WHERE branch_id = $1 AND week_start = $2)",
    )
    .bind(body.branch_id)
    .bind(week_start(body.on_date))
    .fetch_one(pool)
    .await?;
    if published {
        for p in staff_at(pool, body.branch_id).await? {
            notify(
                pool,
                org_id,
                p.user_id,
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

/// Claim an open shift; the manager approves the claim (SC-9).
#[utoipa::path(
    post, path = "/staff/open-shifts/{id}/claim", tag = "staff",
    params(("id" = Uuid, Path)),
    responses((status = 200, body = OpenShift), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn claim_open_shift(
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let user_id = claims.user_id_safe()?;
    let org_id = require_active_profile(pool.get_ref(), user_id).await?;
    let pool = pool.get_ref();
    let branches = branches_of(pool, user_id).await?;
    let row: Option<(Uuid, NaiveDate)> = sqlx::query_as(
        "UPDATE staff_open_shifts SET status = 'claimed', claimed_by = $2, claimed_at = now() \
          WHERE id = $1 AND org_id = $3 AND status = 'open' AND branch_id = ANY($4) \
          RETURNING branch_id, on_date",
    )
    .bind(*id)
    .bind(user_id)
    .bind(org_id)
    .bind(&branches)
    .fetch_optional(pool)
    .await?;
    let Some((branch_id, on_date)) = row else {
        return Err(AppError::Conflict(
            "Someone already claimed that shift.".into(),
        ));
    };
    let name = user_name(pool, user_id).await;
    notify_managers(
        pool,
        org_id,
        Some(branch_id),
        Some(user_id),
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

#[derive(Deserialize, ToSchema)]
pub struct DecideRoster {
    pub approve: bool,
}

/// Approve a claim: the shift becomes theirs for that date. Rejecting reopens it.
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
    let claims = extract_claims(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let caller = claims.user_id_safe()?;
    let pool = pool.get_ref();
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
    crate::authz::require::require(pool, &claims, Cap::HrScheduleEdit, Some(branch_id)).await?;
    if body.approve {
        sqlx::query(
            "UPDATE staff_open_shifts SET status = 'filled', decided_by = $2 WHERE id = $1",
        )
        .bind(*id)
        .bind(caller)
        .execute(pool)
        .await?;
        set_day(
            pool,
            org_id,
            claimer,
            on_date,
            Some(shift_id),
            "Open shift claimed",
            caller,
        )
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
        sqlx::query(
            "UPDATE staff_open_shifts SET status = 'open', claimed_by = NULL, claimed_at = NULL \
              WHERE id = $1",
        )
        .bind(*id)
        .execute(pool)
        .await?;
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

async fn swaps_of(
    pool: &PgPool,
    org_id: Uuid,
    person: Option<Uuid>,
    status: Option<&str>,
) -> Result<Vec<Swap>, AppError> {
    Ok(sqlx::query_as(
        "SELECT s.id, s.requester_id, ru.name AS requester_name, s.requester_date, \
                s.requester_shift_id, rs.name AS requester_shift_name, s.peer_id, \
                pu.name AS peer_name, s.peer_date, s.peer_shift_id, ps.name AS peer_shift_name, \
                s.status, s.created_at \
           FROM staff_swaps s \
           JOIN users ru ON ru.id = s.requester_id JOIN users pu ON pu.id = s.peer_id \
           JOIN work_shifts rs ON rs.id = s.requester_shift_id \
           JOIN work_shifts ps ON ps.id = s.peer_shift_id \
          WHERE s.org_id = $1 \
            AND ($2::uuid IS NULL OR s.requester_id = $2 OR s.peer_id = $2) \
            AND ($3::text IS NULL OR s.status = $3) \
          ORDER BY s.created_at DESC LIMIT 100",
    )
    .bind(org_id)
    .bind(person)
    .bind(status)
    .fetch_all(pool)
    .await?)
}

#[derive(Deserialize, ToSchema)]
pub struct AskSwap {
    pub my_date: NaiveDate,
    pub my_shift_id: Uuid,
    pub peer_id: Uuid,
    pub peer_date: NaiveDate,
    pub peer_shift_id: Uuid,
}

/// Ask a colleague to swap: they agree first, then the manager (SC-8).
#[utoipa::path(
    post, path = "/staff/me/swaps", tag = "staff", request_body = AskSwap,
    responses((status = 201, body = Swap), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn ask_swap(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<AskSwap>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let user_id = claims.user_id_safe()?;
    let org_id = require_active_profile(pool.get_ref(), user_id).await?;
    let pool = pool.get_ref();
    if body.peer_id == user_id {
        return Err(AppError::BadRequest("Pick a colleague.".into()));
    }
    crate::staff::require_user_in_org(pool, org_id, body.peer_id).await?;
    let tz = crate::staff::schedules::employee_timezone(pool, org_id, user_id).await?;
    let mine = resolve_shifts_for(pool, user_id, body.my_date, &tz).await?;
    let theirs = resolve_shifts_for(pool, body.peer_id, body.peer_date, &tz).await?;
    if !mine.iter().any(|s| s.work_shift_id == body.my_shift_id)
        || !theirs.iter().any(|s| s.work_shift_id == body.peer_shift_id)
    {
        return Err(AppError::Conflict(
            "Those shifts aren't on the roster.".into(),
        ));
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO staff_swaps (org_id, requester_id, requester_date, requester_shift_id, \
            peer_id, peer_date, peer_shift_id) VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(org_id)
    .bind(user_id)
    .bind(body.my_date)
    .bind(body.my_shift_id)
    .bind(body.peer_id)
    .bind(body.peer_date)
    .bind(body.peer_shift_id)
    .fetch_one(pool)
    .await?;
    let name = user_name(pool, user_id).await;
    notify(
        pool,
        org_id,
        body.peer_id,
        "staff.n_swap_asked",
        json!({ "name": name }),
    )
    .await;
    let row = swaps_of(pool, org_id, Some(user_id), None)
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
    req: HttpRequest,
    pool: crate::db::Db,
    id: web::Path<Uuid>,
    body: web::Json<DecideRoster>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let user_id = claims.user_id_safe()?;
    let org_id = require_active_profile(pool.get_ref(), user_id).await?;
    let pool = pool.get_ref();
    let row: Option<Uuid> = sqlx::query_scalar(
        "UPDATE staff_swaps SET status = $3 WHERE id = $1 AND peer_id = $2 \
            AND status = 'awaiting_peer' RETURNING requester_id",
    )
    .bind(*id)
    .bind(user_id)
    .bind(if body.approve { "pending" } else { "rejected" })
    .fetch_optional(pool)
    .await?;
    let Some(requester) = row else {
        return Err(AppError::NotFound("No swap waiting for you here.".into()));
    };
    let name = user_name(pool, user_id).await;
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
            None,
            "staff.n_swap_pending",
            json!({}),
        )
        .await;
    }
    let row = swaps_of(pool, org_id, Some(user_id), None)
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
    let claims = extract_claims(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    crate::authz::require::require(pool.get_ref(), &claims, Cap::HrScheduleEdit, None).await?;
    let branches =
        crate::authz::scope::org_read_branches(pool.get_ref(), &claims, org_id, None).await?;
    let mut rows = swaps_of(pool.get_ref(), org_id, None, query.status.as_deref()).await?;
    if let Some(mine) = branches {
        let mut keep = Vec::new();
        for s in rows {
            let b = branches_of(pool.get_ref(), s.requester_id).await?;
            if b.iter().any(|x| mine.contains(x)) {
                keep.push(s);
            }
        }
        rows = keep;
    }
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
    let claims = extract_claims(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    crate::authz::require::require(pool.get_ref(), &claims, Cap::HrScheduleEdit, None).await?;
    let branches = match crate::authz::scope::org_read_branches(
        pool.get_ref(),
        &claims,
        org_id,
        None,
    )
    .await?
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

/// The manager approves: both rosters update for those dates (SC-8).
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
    let claims = extract_claims(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let caller = claims.user_id_safe()?;
    let pool = pool.get_ref();
    let s = swaps_of(pool, org_id, None, Some("pending"))
        .await?
        .into_iter()
        .find(|s| s.id == *id)
        .ok_or_else(|| AppError::NotFound("No swap waiting here.".into()))?;
    let branch = branches_of(pool, s.requester_id).await?.first().copied();
    crate::authz::require::require(pool, &claims, Cap::HrScheduleEdit, branch).await?;
    if caller == s.requester_id || caller == s.peer_id {
        return Err(AppError::Forbidden(
            "You can't approve a swap you're part of.".into(),
        ));
    }
    sqlx::query("UPDATE staff_swaps SET status = $2, decided_by = $3 WHERE id = $1")
        .bind(*id)
        .bind(if body.approve { "approved" } else { "rejected" })
        .bind(caller)
        .execute(pool)
        .await?;
    if body.approve {
        // Each takes the other's shift; on different dates each loses their own.
        if s.requester_date == s.peer_date {
            set_day(
                pool,
                org_id,
                s.requester_id,
                s.peer_date,
                Some(s.peer_shift_id),
                "Swap",
                caller,
            )
            .await?;
            set_day(
                pool,
                org_id,
                s.peer_id,
                s.requester_date,
                Some(s.requester_shift_id),
                "Swap",
                caller,
            )
            .await?;
        } else {
            set_day(
                pool,
                org_id,
                s.requester_id,
                s.requester_date,
                None,
                "Swap",
                caller,
            )
            .await?;
            set_day(
                pool,
                org_id,
                s.requester_id,
                s.peer_date,
                Some(s.peer_shift_id),
                "Swap",
                caller,
            )
            .await?;
            set_day(pool, org_id, s.peer_id, s.peer_date, None, "Swap", caller).await?;
            set_day(
                pool,
                org_id,
                s.peer_id,
                s.requester_date,
                Some(s.requester_shift_id),
                "Swap",
                caller,
            )
            .await?;
        }
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

// ── preferences ────────────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct Preferences {
    /// `morning` · `evening` · null
    #[serde(default)]
    pub pref_time: Option<String>,
    /// Days I can't work: 0 = Sunday … 6 = Saturday.
    #[serde(default)]
    pub cant_work_days: Vec<i16>,
}

/// Preferred times and days I can't work; managers see them (SC-12).
#[utoipa::path(
    put, path = "/staff/me/preferences", tag = "staff", request_body = Preferences,
    responses((status = 204), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn put_preferences(
    req: HttpRequest,
    pool: crate::db::Db,
    body: web::Json<Preferences>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let user_id = claims.user_id_safe()?;
    require_active_profile(pool.get_ref(), user_id).await?;
    if body
        .pref_time
        .as_deref()
        .is_some_and(|p| p != "morning" && p != "evening")
        || body.cant_work_days.iter().any(|d| !(0..=6).contains(d))
    {
        return Err(AppError::BadRequest("Invalid preferences".into()));
    }
    sqlx::query("UPDATE staff_profiles SET pref_time = $2, cant_work_days = $3 WHERE user_id = $1")
        .bind(user_id)
        .bind(body.pref_time.as_deref())
        .bind(&body.cant_work_days)
        .execute(pool.get_ref())
        .await?;
    Ok(HttpResponse::NoContent().finish())
}

// ── holidays (RU-10) ───────────────────────────────────────────────────────

/// Egypt's public holidays, suggested — never applied automatically. The
/// Islamic dates are the expected ones and move with the moon; the manager
/// confirms each.
fn egypt_holidays(year: i32) -> Vec<(NaiveDate, &'static str, &'static str)> {
    let d = |m, day| NaiveDate::from_ymd_opt(year, m, day).expect("valid holiday date");
    let mut out = vec![
        (d(1, 7), "Coptic Christmas", "عيد الميلاد المجيد"),
        (d(1, 25), "Revolution Day (25 January)", "عيد ثورة 25 يناير"),
        (d(4, 25), "Sinai Liberation Day", "عيد تحرير سيناء"),
        (d(5, 1), "Labour Day", "عيد العمال"),
        (d(6, 30), "30 June Revolution", "ذكرى ثورة 30 يونيو"),
        (d(7, 23), "Revolution Day (23 July)", "عيد ثورة 23 يوليو"),
        (d(10, 6), "Armed Forces Day", "عيد القوات المسلحة"),
    ];
    let lunar: &[(u32, u32, &str, &str)] = match year {
        2026 => &[
            (3, 20, "Eid al-Fitr", "عيد الفطر"),
            (5, 27, "Eid al-Adha", "عيد الأضحى"),
            (6, 16, "Islamic New Year", "رأس السنة الهجرية"),
            (8, 25, "Prophet's Birthday", "المولد النبوي"),
        ],
        2027 => &[
            (3, 10, "Eid al-Fitr", "عيد الفطر"),
            (5, 16, "Eid al-Adha", "عيد الأضحى"),
            (6, 6, "Islamic New Year", "رأس السنة الهجرية"),
            (8, 15, "Prophet's Birthday", "المولد النبوي"),
        ],
        _ => &[],
    };
    for &(m, day, en, ar) in lunar {
        out.push((d(m, day), en, ar));
    }
    out
}

async fn holidays_in(
    pool: &PgPool,
    org_id: Uuid,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<HolidayView>, AppError> {
    for year in from.year()..=to.year() {
        for (date, en, ar) in egypt_holidays(year) {
            if date >= from && date <= to {
                sqlx::query(
                    "INSERT INTO staff_holidays (org_id, on_date, name_en, name_ar) \
                     VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
                )
                .bind(org_id)
                .bind(date)
                .bind(en)
                .bind(ar)
                .execute(pool)
                .await?;
            }
        }
    }
    Ok(sqlx::query_as(
        "SELECT on_date, name_en, name_ar, decision FROM staff_holidays \
          WHERE org_id = $1 AND on_date BETWEEN $2 AND $3 ORDER BY on_date",
    )
    .bind(org_id)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?)
}

#[derive(Deserialize, ToSchema)]
pub struct HolidayDecision {
    /// `holiday` (nobody marked absent; working it pays the multiplier) or
    /// `dismissed` (a normal day).
    pub decision: String,
}

#[utoipa::path(
    put, path = "/staff/holidays/{date}", tag = "staff", request_body = HolidayDecision,
    params(("date" = NaiveDate, Path)),
    responses((status = 200, body = HolidayView), AppErrorResponse),
    security(("bearer_jwt" = []))
)]
pub async fn decide_holiday(
    req: HttpRequest,
    pool: crate::db::Db,
    date: web::Path<NaiveDate>,
    body: web::Json<HolidayDecision>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    crate::authz::require::require(pool.get_ref(), &claims, Cap::HrSchedulePublish, None).await?;
    if body.decision != "holiday" && body.decision != "dismissed" {
        return Err(AppError::BadRequest(
            "decision is holiday or dismissed".into(),
        ));
    }
    holidays_in(pool.get_ref(), org_id, *date, *date).await?;
    let row: HolidayView = sqlx::query_as(
        "UPDATE staff_holidays SET decision = $3, decided_by = $4 \
          WHERE org_id = $1 AND on_date = $2 \
          RETURNING on_date, name_en, name_ar, decision",
    )
    .bind(org_id)
    .bind(*date)
    .bind(&body.decision)
    .bind(claims.user_id())
    .fetch_optional(pool.get_ref())
    .await?
    .ok_or_else(|| AppError::NotFound("No public holiday on that date.".into()))?;
    Ok(HttpResponse::Ok().json(row))
}

// ── suggestions (SC-13, rule-based phase) ─────────────────────────────────

#[derive(Serialize, Deserialize, ToSchema, Clone)]
pub struct Suggestion {
    /// Opaque; send it back to accept or reject.
    pub id: String,
    pub date: NaiveDate,
    pub work_shift_id: Uuid,
    pub shift_name: String,
    /// Who the suggestion puts on the shift.
    pub user_id: Uuid,
    pub user_name: String,
    /// Who it takes off it, for a reassignment.
    pub from_user_id: Option<Uuid>,
    pub from_user_name: Option<String>,
    /// A core i18n key for the one-line reason, and its arguments.
    pub reason_key: String,
    pub reason_args: serde_json::Value,
    /// 0–100.
    pub confidence: i32,
    /// The gender default decided it (it says so).
    pub by_default: bool,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SuggestQuery {
    pub branch_id: Uuid,
    pub week_start: NaiveDate,
}

/// Everyone's rostered week, as the engine sees it.
struct Board {
    busy: HashSet<(NaiveDate, Uuid)>,
    spans: HashMap<Uuid, Vec<engine::Span>>,
    late_count: HashMap<Uuid, i32>,
    /// (person, shift) → times this week, for continuity.
    on_shift: HashMap<(Uuid, Uuid), i32>,
    /// (date, shift) → who.
    by_day: HashMap<(NaiveDate, Uuid), Vec<Uuid>>,
}

fn span_of(w: &WorkShiftBrief, d: NaiveDate, tz: &chrono_tz::Tz) -> Option<engine::Span> {
    use chrono::TimeZone;
    let end_date = if w.crosses_midnight || w.end_time <= w.start_time {
        d + Duration::days(1)
    } else {
        d
    };
    Some(engine::Span {
        date: d,
        start: tz
            .from_local_datetime(&d.and_time(w.start_time))
            .earliest()?
            .with_timezone(&Utc),
        end: tz
            .from_local_datetime(&end_date.and_time(w.end_time))
            .earliest()?
            .with_timezone(&Utc),
    })
}

impl Board {
    fn assign(&mut self, p: Uuid, w: &WorkShiftBrief, late: bool, span: engine::Span) {
        self.busy.insert((span.date, p));
        self.spans.entry(p).or_default().push(span);
        if late {
            *self.late_count.entry(p).or_default() += 1;
        }
        *self.on_shift.entry((p, w.id)).or_default() += 1;
        self.by_day.entry((span.date, w.id)).or_default().push(p);
    }
}

/// What the engine learned (SC-13): per-person fit from 24 months of events,
/// and whether learning is frozen.
async fn learned(
    pool: &PgPool,
    branch_id: Uuid,
    late_of: &HashMap<Uuid, bool>,
) -> Result<(HashMap<Uuid, engine::Fit>, bool), AppError> {
    // (user, shift, +1/−1, age in days, manager side)
    let rows: Vec<(Uuid, Uuid, f64, f64, bool)> = sqlx::query_as(
        "SELECT user_id, work_shift_id, CASE WHEN accepted THEN 1.0 ELSE -1.0 END::float8, \
                EXTRACT(EPOCH FROM now() - created_at)::float8 / 86400, true \
           FROM staff_suggestion_events \
          WHERE branch_id = $1 AND user_id IS NOT NULL AND work_shift_id IS NOT NULL \
            AND created_at > now() - INTERVAL '24 months' \
         UNION ALL \
         SELECT claimed_by, work_shift_id, 1.0, \
                EXTRACT(EPOCH FROM now() - COALESCE(claimed_at, created_at))::float8 / 86400, false \
           FROM staff_open_shifts \
          WHERE branch_id = $1 AND claimed_by IS NOT NULL AND status IN ('claimed', 'filled') \
            AND created_at > now() - INTERVAL '24 months' \
         UNION ALL \
         SELECT x.uid, x.sid, x.v, EXTRACT(EPOCH FROM now() - s.created_at)::float8 / 86400, false \
           FROM staff_swaps s \
           JOIN user_branch_assignments a ON a.user_id = s.requester_id AND a.branch_id = $1 \
          CROSS JOIN LATERAL (VALUES (s.requester_id, s.peer_shift_id, 1.0::float8), \
                                     (s.requester_id, s.requester_shift_id, -1.0), \
                                     (s.peer_id, s.requester_shift_id, 1.0), \
                                     (s.peer_id, s.peer_shift_id, -1.0)) x(uid, sid, v) \
          WHERE s.status = 'approved' AND s.created_at > now() - INTERVAL '24 months' \
         UNION ALL \
         SELECT user_id, work_shift_id, CASE WHEN status = 'absent' THEN -1.0 ELSE 1.0 END::float8, \
                EXTRACT(EPOCH FROM now() - business_date::timestamp)::float8 / 86400, false \
           FROM attendance_records \
          WHERE branch_id = $1 AND work_shift_id IS NOT NULL AND status <> 'on_leave' \
            AND business_date > CURRENT_DATE - 365",
    )
    .bind(branch_id)
    .fetch_all(pool)
    .await?;
    let signals: Vec<engine::Signal> = rows
        .into_iter()
        .filter_map(|(user_id, shift, value, age_days, manager)| {
            Some(engine::Signal {
                user_id,
                late: *late_of.get(&shift)?,
                value,
                age_days,
                manager,
            })
        })
        .collect();
    let (accepted, decided): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*) FILTER (WHERE accepted), COUNT(*) FROM staff_suggestion_events \
          WHERE branch_id = $1 AND created_at > now() - INTERVAL '28 days'",
    )
    .bind(branch_id)
    .fetch_one(pool)
    .await?;
    Ok((engine::learn(&signals), engine::frozen(accepted, decided)))
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
    let grid: Vec<CoverageNeed> = coverage_rows(pool, branch_id).await?;
    let dates: Vec<NaiveDate> = (0..7).map(|i| ws + Duration::days(i)).collect();
    if !grid.is_empty() {
        for d in &dates {
            let dow = d.weekday().num_days_from_sunday() as i16;
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
        let dow = d.weekday().num_days_from_sunday() as i32;
        for (_, h, n) in rows.iter().filter(|(x, _, _)| *x == dow) {
            let staff = engine::pos_need(*n, orders_per_staff);
            if staff > 0 {
                need.insert((*d, *h, None), staff);
            }
        }
    }
    Ok(need)
}

/// Average orders per weekday and hour over the last 8 weeks.
async fn pos_hourly(
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

/// Fill: the week from the pattern, leave removed. Find gaps against the
/// coverage need (grid, POS or the pattern's own). Score: coverage first, then
/// fair spread of late shifts, preferences, the gender default, what was
/// learned (reliability included) and continuity. Never past a labour limit,
/// never publishes, never changes the pattern; overtime is not a target.
async fn suggest(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    ws: NaiveDate,
) -> Result<Vec<Suggestion>, AppError> {
    let settings = crate::staff::attendance::load_settings(pool, org_id, Some(branch_id)).await?;
    let night = (settings.night_start, settings.night_end);
    let limits = engine::Limits::of(&settings);
    let tz_name = crate::staff::branch_timezone(pool, branch_id).await?;
    let tz: chrono_tz::Tz = tz_name.parse().unwrap_or(chrono_tz::Africa::Cairo);
    let work_shifts: Vec<WorkShiftBrief> = work_shifts_of(pool, org_id)
        .await?
        .into_iter()
        .filter(|w| w.branch_id.is_none_or(|b| b == branch_id))
        .collect();
    let late_of: HashMap<Uuid, bool> = work_shifts
        .iter()
        .map(|w| {
            (
                w.id,
                engine::is_late(w.start_time, w.end_time, w.crosses_midnight, night),
            )
        })
        .collect();
    let shift_branch: HashMap<Uuid, Option<Uuid>> =
        work_shifts.iter().map(|w| (w.id, w.branch_id)).collect();
    let staff = staff_at(pool, branch_id).await?;
    let to = ws + Duration::days(6);
    let mut board = Board {
        busy: HashSet::new(),
        spans: HashMap::new(),
        late_count: HashMap::new(),
        on_shift: HashMap::new(),
        by_day: HashMap::new(),
    };
    // A day either side, for the rest between shifts.
    for p in &staff {
        for s in shifts_of(
            pool,
            (p.user_id, &p.name),
            branch_id,
            &shift_branch,
            ws - Duration::days(1),
            to + Duration::days(1),
            &tz_name,
        )
        .await?
        {
            let in_week = s.date >= ws && s.date <= to;
            if in_week {
                board.busy.insert((s.date, p.user_id));
            }
            if s.on_leave {
                continue;
            }
            board
                .spans
                .entry(p.user_id)
                .or_default()
                .push(engine::Span {
                    date: s.date,
                    start: s.start_at,
                    end: s.end_at,
                });
            if in_week {
                board
                    .by_day
                    .entry((s.date, s.work_shift_id))
                    .or_default()
                    .push(p.user_id);
                *board
                    .on_shift
                    .entry((p.user_id, s.work_shift_id))
                    .or_default() += 1;
                if late_of.get(&s.work_shift_id).copied().unwrap_or(false) {
                    *board.late_count.entry(p.user_id).or_default() += 1;
                }
            }
        }
    }
    let (fits, frozen) = learned(pool, branch_id, &late_of).await?;
    let decided: HashSet<String> = sqlx::query_scalar(
        "SELECT suggestion FROM staff_suggestion_events WHERE branch_id = $1 \
            AND on_date BETWEEN $2 AND $3",
    )
    .bind(branch_id)
    .bind(ws)
    .bind(to)
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();
    let gender_mode = settings.gender_mode.as_str();

    // The best person for shift `w` on `d`, or None. (score, by the gender default)
    let best = |board: &Board,
                w: &WorkShiftBrief,
                d: NaiveDate,
                dept: Option<Uuid>,
                id_of: &dyn Fn(Uuid) -> String|
     -> Option<(&RosterPerson, f64, bool)> {
        let dow = d.weekday().num_days_from_sunday() as i16;
        let late = late_of.get(&w.id).copied().unwrap_or(false);
        let span = span_of(w, d, &tz)?;
        let mut c: Vec<(&RosterPerson, f64, bool)> = staff
            .iter()
            .filter(|p| {
                !board.busy.contains(&(d, p.user_id))
                    && !p.cant_work_days.contains(&dow)
                    && dept.is_none_or(|x| p.department_id == Some(x))
                    && !decided.contains(&id_of(p.user_id))
            })
            .filter_map(|p| {
                let learned = if frozen { None } else { fits.get(&p.user_id) };
                let stated = matches!(p.pref_time.as_deref(), Some("morning" | "evening"));
                let pref = match p.pref_time.as_deref() {
                    Some("morning") if late => -1.0,
                    Some("morning") => 1.0,
                    Some("evening") if late => 1.0,
                    Some("evening") => -1.0,
                    _ => 0.0,
                };
                // Hard mode: late shifts only to women who said or showed they want them.
                if late
                    && gender_mode == "hard"
                    && p.gender.as_deref() == Some("f")
                    && p.pref_time.as_deref() != Some("evening")
                    && !learned.is_some_and(engine::Fit::willing_late)
                {
                    return None;
                }
                let events = learned.map_or(0, |f| f.events(late));
                let default = if late
                    && gender_mode != "off"
                    && !stated
                    && events < 3
                    && p.gender.as_deref() == Some("m")
                {
                    0.15
                } else {
                    0.0
                };
                let empty = Vec::new();
                let spans = board.spans.get(&p.user_id).unwrap_or(&empty);
                if !engine::fits(p.user_id, spans, span, &limits) {
                    return None;
                }
                let spread = if late {
                    -0.1 * f64::from(*board.late_count.get(&p.user_id).unwrap_or(&0))
                } else {
                    0.0
                };
                let continuity = if board.on_shift.contains_key(&(p.user_id, w.id)) {
                    0.1
                } else {
                    0.0
                };
                let fit = learned.map_or(0.0, |f| f.score(late));
                Some((p, pref + default + fit + spread + continuity, default > 0.0))
            })
            .collect();
        c.sort_by(|a, b| b.1.total_cmp(&a.1));
        c.into_iter().next()
    };
    let add = |d: NaiveDate,
               w: &WorkShiftBrief,
               p: &RosterPerson,
               score: f64,
               by_default: bool,
               key: &str,
               args: serde_json::Value| Suggestion {
        id: format!("add|{d}|{}|{}", w.id, p.user_id),
        date: d,
        work_shift_id: w.id,
        shift_name: w.name.clone(),
        user_id: p.user_id,
        user_name: p.name.clone(),
        from_user_id: None,
        from_user_name: None,
        reason_key: key.into(),
        reason_args: args,
        confidence: (60.0 + 25.0 * score).clamp(30.0, 95.0) as i32,
        by_default,
    };

    let mut out = Vec::new();
    let need = coverage_need(
        pool,
        org_id,
        branch_id,
        ws,
        &tz_name,
        settings.orders_per_staff,
    )
    .await?;
    let dept_of: HashMap<Uuid, Option<Uuid>> =
        staff.iter().map(|p| (p.user_id, p.department_id)).collect();
    if need.is_empty() {
        // Coverage need = how many the standing pattern puts on each shift and weekday.
        let pattern: Vec<(Uuid, Option<i16>, i64)> = sqlx::query_as(
            "SELECT s.work_shift_id, s.day_of_week, COUNT(*) FROM staff_schedules s \
               JOIN user_branch_assignments a ON a.user_id = s.user_id AND a.branch_id = $1 \
              WHERE s.effective_from <= $3 AND (s.effective_to IS NULL OR s.effective_to >= $2) \
              GROUP BY 1, 2",
        )
        .bind(branch_id)
        .bind(ws)
        .bind(to)
        .fetch_all(pool)
        .await?;
        let mut d = ws;
        while d <= to {
            let dow = d.weekday().num_days_from_sunday() as i16;
            for w in &work_shifts {
                let need = pattern
                    .iter()
                    .filter(|(sid, day, _)| *sid == w.id && day.is_none_or(|x| x == dow))
                    .map(|(_, _, n)| *n)
                    .sum::<i64>();
                loop {
                    let have = board.by_day.get(&(d, w.id)).map_or(0, Vec::len) as i64;
                    if have >= need {
                        break;
                    }
                    let id_of = |u: Uuid| format!("add|{d}|{}|{u}", w.id);
                    let Some((p, score, by_default)) = best(&board, w, d, None, &id_of) else {
                        break;
                    };
                    out.push(add(
                        d,
                        w,
                        p,
                        score,
                        by_default,
                        "staff.sg_gap",
                        json!({ "shift": w.name, "short": need - have }),
                    ));
                    let late = late_of.get(&w.id).copied().unwrap_or(false);
                    if let Some(span) = span_of(w, d, &tz) {
                        board.assign(p.user_id, w, late, span);
                    }
                }
            }
            d += Duration::days(1);
        }
    } else {
        // Hour by hour: cover the hour with the shift that closes the most gap.
        let covered = |board: &Board, d: NaiveDate, h: i32, dept: Option<Uuid>| -> i32 {
            let mut n = 0;
            for w in &work_shifts {
                let hours = engine::shift_hours(w.start_time, w.end_time, w.crosses_midnight);
                for (sd, hh) in [(d, h), (d - Duration::days(1), h + 24)] {
                    if hours.contains(&hh) {
                        n += board.by_day.get(&(sd, w.id)).map_or(0, |v| {
                            v.iter()
                                .filter(|u| {
                                    dept.is_none_or(|x| {
                                        dept_of.get(u).copied().flatten() == Some(x)
                                    })
                                })
                                .count() as i32
                        });
                    }
                }
            }
            n
        };
        let mut keys: Vec<(NaiveDate, i32, Option<Uuid>)> = need.keys().copied().collect();
        keys.sort();
        for (d, h, dept) in keys {
            let want = need[&(d, h, dept)];
            loop {
                let short = want - covered(&board, d, h, dept);
                if short <= 0 {
                    break;
                }
                // Shifts that cover this hour: starting today, or last night's.
                let mut options: Vec<(i32, NaiveDate, &WorkShiftBrief)> = Vec::new();
                for w in &work_shifts {
                    let hours = engine::shift_hours(w.start_time, w.end_time, w.crosses_midnight);
                    for (sd, hh) in [(d, h), (d - Duration::days(1), h + 24)] {
                        if sd < ws || !hours.contains(&hh) {
                            continue;
                        }
                        let closes: i32 = hours
                            .clone()
                            .map(|x| {
                                let (xd, xh) = if x >= 24 {
                                    (sd + Duration::days(1), x - 24)
                                } else {
                                    (sd, x)
                                };
                                need.get(&(xd, xh, dept))
                                    .map_or(0, |n| (n - covered(&board, xd, xh, dept)).max(0))
                            })
                            .sum();
                        options.push((closes, sd, w));
                    }
                }
                options.sort_by_key(|o| std::cmp::Reverse(o.0));
                let mut placed = false;
                for (_, sd, w) in options {
                    let id_of = |u: Uuid| format!("add|{sd}|{}|{u}", w.id);
                    if let Some((p, score, by_default)) = best(&board, w, sd, dept, &id_of) {
                        out.push(add(sd, w, p, score, by_default, "staff.sg_coverage",
                            json!({ "shift": w.name, "hour": format!("{h:02}:00"), "short": short })));
                        let late = late_of.get(&w.id).copied().unwrap_or(false);
                        if let Some(span) = span_of(w, sd, &tz) {
                            board.assign(p.user_id, w, late, span);
                        }
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

    // Someone rostered on a day they said they can't work.
    let mut d = ws;
    while d <= to {
        let dow = d.weekday().num_days_from_sunday() as i16;
        for w in &work_shifts {
            let assigned = board.by_day.get(&(d, w.id)).cloned().unwrap_or_default();
            for uid in &assigned {
                let Some(person) = staff.iter().find(|p| p.user_id == *uid) else {
                    continue;
                };
                if !person.cant_work_days.contains(&dow) {
                    continue;
                }
                let id_of = |u: Uuid| format!("move|{d}|{}|{uid}|{u}", w.id);
                if let Some((p, score, by_default)) = best(&board, w, d, None, &id_of) {
                    out.push(Suggestion {
                        id: id_of(p.user_id),
                        date: d,
                        work_shift_id: w.id,
                        shift_name: w.name.clone(),
                        user_id: p.user_id,
                        user_name: p.name.clone(),
                        from_user_id: Some(*uid),
                        from_user_name: Some(person.name.clone()),
                        reason_key: "staff.sg_cant_work".into(),
                        reason_args: json!({ "name": person.name }),
                        confidence: (70.0 + 20.0 * score).clamp(40.0, 95.0) as i32,
                        by_default,
                    });
                    let late = late_of.get(&w.id).copied().unwrap_or(false);
                    if let Some(span) = span_of(w, d, &tz) {
                        board.assign(p.user_id, w, late, span);
                    }
                }
            }
        }
        d += Duration::days(1);
    }
    out.extend(pattern_updates(pool, ws, &staff, &work_shifts, &decided).await?);
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
    let ids: Vec<Uuid> = staff.iter().map(|p| p.user_id).collect();
    // (user, weekday, shift or null = off) edited identically on each of the 4 weeks before.
    let rows: Vec<(Uuid, i32, Option<Uuid>)> = sqlx::query_as(
        "SELECT user_id, EXTRACT(DOW FROM on_date)::int, work_shift_id \
           FROM staff_schedule_overrides \
          WHERE user_id = ANY($1) AND on_date >= $2 - 28 AND on_date < $2 \
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
        let std: Vec<Uuid> = sqlx::query_scalar(
            "SELECT work_shift_id FROM staff_schedules WHERE user_id = $1 \
                AND effective_from <= $2 AND (effective_to IS NULL OR effective_to >= $2) \
                AND (day_of_week IS NULL OR day_of_week = $3::smallint) \
              ORDER BY day_of_week NULLS LAST LIMIT 1",
        )
        .bind(uid)
        .bind(d)
        .bind(dow)
        .fetch_all(pool)
        .await?;
        if std.first().copied() == shift {
            continue;
        }
        let shift_id = shift.unwrap_or_default();
        let id = format!("pattern|{d}|{shift_id}|{uid}");
        if decided.contains(&id) {
            continue;
        }
        let Some(p) = staff.iter().find(|p| p.user_id == uid) else {
            continue;
        };
        let name = work_shifts
            .iter()
            .find(|w| Some(w.id) == shift)
            .map(|w| w.name.clone());
        out.push(Suggestion {
            id,
            date: d,
            work_shift_id: shift_id,
            shift_name: name.clone().unwrap_or_default(),
            user_id: uid,
            user_name: p.name.clone(),
            from_user_id: None,
            from_user_name: None,
            reason_key: if shift.is_some() {
                "staff.sg_pattern"
            } else {
                "staff.sg_pattern_off"
            }
            .into(),
            reason_args: json!({ "name": p.name, "shift": name, "weeks": 4 }),
            confidence: 80,
            by_default: false,
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
    let claims = extract_claims(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    crate::authz::scope::org_read_branches(pool.get_ref(), &claims, org_id, Some(query.branch_id))
        .await?;
    crate::authz::require::require(
        pool.get_ref(),
        &claims,
        Cap::HrScheduleEdit,
        Some(query.branch_id),
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

/// The precomputed week when it is still current (any roster input changing
/// drops it), else computed now and kept. Decided ones are left out.
async fn cached_suggestions(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    ws: NaiveDate,
) -> Result<Vec<Suggestion>, AppError> {
    let cached: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT payload FROM staff_suggestion_cache WHERE branch_id = $1 AND week_start = $2",
    )
    .bind(branch_id)
    .bind(ws)
    .fetch_optional(pool)
    .await?;
    let all: Vec<Suggestion> = match cached.and_then(|v| serde_json::from_value(v).ok()) {
        Some(v) => v,
        None => precompute(pool, org_id, branch_id, ws).await?,
    };
    let decided: HashSet<String> = sqlx::query_scalar(
        "SELECT suggestion FROM staff_suggestion_events WHERE branch_id = $1 \
            AND on_date BETWEEN $2 AND $2 + 6",
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
pub(crate) async fn precompute(
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
             computed_at = now()",
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
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    d: NaiveDate,
    shift: Option<Uuid>,
) -> Result<(), AppError> {
    let dow = d.weekday().num_days_from_sunday() as i16;
    let mut tx = pool.begin().await?;
    type Row = (Uuid, Uuid, Option<i16>, NaiveDate, Option<NaiveDate>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, work_shift_id, day_of_week, effective_from, effective_to \
           FROM staff_schedules WHERE user_id = $1 AND org_id = $2 \
            AND (effective_to IS NULL OR effective_to >= $3) \
            AND (day_of_week IS NULL OR day_of_week = $4) FOR UPDATE",
    )
    .bind(user_id)
    .bind(org_id)
    .bind(d)
    .bind(dow)
    .fetch_all(&mut *tx)
    .await?;
    for (id, ws_id, day, from, until) in rows {
        if day.is_none() {
            let start = from.max(d);
            for other in (0..7i16).filter(|x| *x != dow) {
                sqlx::query(
                    "INSERT INTO staff_schedules \
                         (org_id, user_id, work_shift_id, day_of_week, effective_from, effective_to) \
                     SELECT $1, $2, $3, $4, $5, $6 WHERE NOT EXISTS ( \
                         SELECT 1 FROM staff_schedules WHERE user_id = $2 AND day_of_week = $4 \
                            AND (effective_to IS NULL OR effective_to >= $5) \
                            AND ($6::date IS NULL OR effective_from <= $6))",
                )
                .bind(org_id)
                .bind(user_id)
                .bind(ws_id)
                .bind(other)
                .bind(start)
                .bind(until)
                .execute(&mut *tx)
                .await?;
            }
        }
        if from >= d {
            sqlx::query("DELETE FROM staff_schedules WHERE id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        } else {
            sqlx::query("UPDATE staff_schedules SET effective_to = $2 WHERE id = $1")
                .bind(id)
                .bind(d - Duration::days(1))
                .execute(&mut *tx)
                .await?;
        }
    }
    if let Some(shift) = shift {
        sqlx::query(
            "INSERT INTO staff_schedules (org_id, user_id, work_shift_id, day_of_week, effective_from) \
             SELECT $1, $2, id, $4, $5 FROM work_shifts WHERE id = $3 AND org_id = $1",
        )
        .bind(org_id)
        .bind(user_id)
        .bind(shift)
        .bind(dow)
        .bind(d)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[derive(Deserialize, ToSchema)]
pub struct DecideSuggestion {
    pub branch_id: Uuid,
    pub id: String,
    pub accept: bool,
}

/// Accept (changes that date only) or reject; either way it is remembered.
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
    let claims = extract_claims(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let caller = claims.user_id_safe()?;
    let pool = pool.get_ref();
    crate::authz::scope::org_read_branches(pool, &claims, org_id, Some(body.branch_id)).await?;
    crate::authz::require::require(pool, &claims, Cap::HrScheduleEdit, Some(body.branch_id))
        .await?;
    let parts: Vec<&str> = body.id.split('|').collect();
    let bad = || AppError::BadRequest("Unknown suggestion".into());
    let date: NaiveDate = parts.get(1).and_then(|d| d.parse().ok()).ok_or_else(bad)?;
    let shift: Uuid = parts.get(2).and_then(|d| d.parse().ok()).ok_or_else(bad)?;
    let pattern = parts.first() == Some(&"pattern");
    let (from, to): (Option<Uuid>, Uuid) = match parts.first() {
        Some(&"add" | &"pattern") if parts.len() == 4 => {
            (None, parts[3].parse().map_err(|_| bad())?)
        }
        Some(&"move") if parts.len() == 5 => (
            Some(parts[3].parse().map_err(|_| bad())?),
            parts[4].parse().map_err(|_| bad())?,
        ),
        _ => return Err(bad()),
    };
    crate::staff::require_user_in_org(pool, org_id, to).await?;
    if body.accept && pattern {
        // The standing pattern itself: the one suggestion that changes it,
        // and only once a manager with pattern rights says so.
        crate::permissions::checker::check_permission(pool, &claims, "work_shifts", "update")
            .await?;
        set_pattern_day(pool, org_id, to, date, (!shift.is_nil()).then_some(shift)).await?;
    } else if body.accept {
        if let Some(from) = from {
            set_day(
                pool,
                org_id,
                from,
                date,
                None,
                "Suggestion accepted",
                caller,
            )
            .await?;
        }
        set_day(
            pool,
            org_id,
            to,
            date,
            Some(shift),
            "Suggestion accepted",
            caller,
        )
        .await?;
    }
    sqlx::query(
        "INSERT INTO staff_suggestion_events (org_id, branch_id, suggestion, user_id, on_date, \
            accepted, decided_by, work_shift_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, (SELECT id FROM work_shifts WHERE id = $8))",
    )
    .bind(org_id)
    .bind(body.branch_id)
    .bind(&body.id)
    .bind(to)
    .bind(date)
    .bind(body.accept)
    .bind(caller)
    .bind((!pattern).then_some(shift))
    .execute(pool)
    .await?;
    // A rejection asks for the next best, so the kept week is recomputed.
    sqlx::query("DELETE FROM staff_suggestion_cache WHERE branch_id = $1 AND week_start = $2")
        .bind(body.branch_id)
        .bind(week_start(date))
        .execute(pool)
        .await?;
    Ok(HttpResponse::NoContent().finish())
}

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
    let claims = extract_claims(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    crate::authz::scope::org_read_branches(pool, &claims, org_id, Some(query.branch_id)).await?;
    crate::authz::require::require(pool, &claims, Cap::HrScheduleRead, Some(query.branch_id))
        .await?;
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
    let claims = extract_claims(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    crate::authz::scope::org_read_branches(pool, &claims, org_id, Some(body.branch_id)).await?;
    crate::authz::require::require(pool, &claims, Cap::HrScheduleEdit, Some(body.branch_id))
        .await?;
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

// ── Fairness audit (SC-13 guardrail) ──────────────────────────────────────

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FairnessQuery {
    /// Any day of the month.
    pub month: NaiveDate,
}

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub struct FairnessRow {
    /// `m` · `f` · null (not set)
    pub gender: Option<String>,
    pub people: i64,
    /// Said they prefer evenings.
    pub willing: i64,
    pub shifts: i64,
    pub night_shifts: i64,
}

#[derive(Serialize, ToSchema)]
pub struct FairnessView {
    pub month: NaiveDate,
    /// Night share by gender against stated willingness.
    pub rows: Vec<FairnessRow>,
    /// Suggestions managers decided in the last 4 weeks, and how many they accepted.
    pub decided_4w: i64,
    pub accepted_4w: i64,
    /// Learning is paused: under 40% accepted over 4 weeks.
    pub learning_frozen: bool,
}

/// Owner only, monthly: who works the nights, by gender, against who said
/// they want them.
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
    let claims = extract_claims(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    crate::authz::require::require(pool, &claims, Cap::HrRosterSettings, None).await?;
    let settings = crate::staff::attendance::load_settings(pool, org_id, None).await?;
    let month = query.month.with_day(1).unwrap_or(query.month);
    let rows: Vec<FairnessRow> = sqlx::query_as(
        "SELECT p.gender, COUNT(DISTINCT p.user_id) AS people, \
                COUNT(DISTINCT p.user_id) FILTER (WHERE p.pref_time = 'evening') AS willing, \
                COUNT(a.id) AS shifts, \
                COUNT(a.id) FILTER (WHERE dawam_night_minutes(a.scheduled_start_at, \
                    a.scheduled_end_at, br.timezone::text, $3, $4) > 0) AS night_shifts \
           FROM staff_profiles p \
           JOIN users u ON u.id = p.user_id AND u.deleted_at IS NULL \
           LEFT JOIN attendance_records a ON a.user_id = p.user_id \
                AND a.business_date >= $2 AND a.business_date < ($2 + INTERVAL '1 month')::date \
                AND a.scheduled_start_at IS NOT NULL AND a.status <> 'on_leave' \
           LEFT JOIN branches br ON br.id = a.branch_id \
          WHERE p.org_id = $1 AND p.employment_status = 'active' \
          GROUP BY p.gender ORDER BY p.gender NULLS LAST",
    )
    .bind(org_id)
    .bind(month)
    .bind(settings.night_start)
    .bind(settings.night_end)
    .fetch_all(pool)
    .await?;
    let (accepted, decided): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*) FILTER (WHERE accepted), COUNT(*) FROM staff_suggestion_events \
          WHERE org_id = $1 AND created_at > now() - INTERVAL '28 days'",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await?;
    Ok(HttpResponse::Ok().json(FairnessView {
        month,
        rows,
        decided_4w: decided,
        accepted_4w: accepted,
        learning_frozen: engine::frozen(accepted, decided),
    }))
}
