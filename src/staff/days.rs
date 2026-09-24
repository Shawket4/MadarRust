//! A date's assignments: the writes behind every date-level roster change
//! (SC-5, SC-11), and telling people when a published week changes (SC-4).
//!
//! A date follows the standing pattern until something changes it; from then on
//! the date holds its own SET of assignments in `staff_schedule_overrides`, one
//! row per block (each optionally with its own from/to), or a single NULL-shift
//! row for a day off. Every writer here works on that set, never on "the" shift
//! of a day, so a swap, a claim, an accepted suggestion or a manager's edit
//! touches one block and leaves the rest of a split day alone.
//!
//! Every writer runs inside the caller's transaction and the caller finishes
//! with [`check_overlaps`] before committing: two assignments of one person may
//! not overlap, including a night shift running into the next morning.

use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, Utc};
use serde_json::json;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::errors::AppError;
use crate::staff::access::Subject;
use crate::staff::dawam::{notify, week_start};
use crate::staff::schedules::{ResolvedShift, resolve_range};

const NIL: &str = "'00000000-0000-0000-0000-000000000000'::uuid";

/// One block on a date, with this assignment's own from/to if it has one.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Block {
    pub work_shift_id: Uuid,
    pub times: Option<(NaiveTime, NaiveTime)>,
}

/// What a block allows.
#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct BlockInfo {
    pub name: String,
    pub branch_id: Option<Uuid>,
    pub valid_days: Vec<i16>,
    pub is_active: bool,
}

/// Postgres DOW (0 = Sunday) of a date.
pub(crate) fn dow(d: NaiveDate) -> i16 {
    d.weekday().num_days_from_sunday() as i16
}

pub(crate) async fn block_info(
    conn: &mut PgConnection,
    org_id: Uuid,
    work_shift_id: Uuid,
) -> Result<BlockInfo, AppError> {
    sqlx::query_as(
        "SELECT name, branch_id, valid_days, is_active FROM work_shifts \
          WHERE id = $1 AND org_id = $2",
    )
    .bind(work_shift_id)
    .bind(org_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| AppError::NotFound("Work shift not found".into()))
}

/// May `block` be rostered for `subject` on `date`? The block must be live,
/// valid on that weekday, and belong to one of the person's branches (or the
/// whole business); its own times must make a shift.
pub(crate) async fn validate_block(
    conn: &mut PgConnection,
    subject: &Subject,
    date: NaiveDate,
    block: &Block,
) -> Result<BlockInfo, AppError> {
    let info = block_info(conn, subject.org_id, block.work_shift_id).await?;
    if !info.is_active {
        return Err(AppError::Coded {
            status: 400,
            code: "SHIFT_INACTIVE",
            reason: format!("{} is switched off.", info.name),
        });
    }
    if !info.valid_days.contains(&dow(date)) {
        return Err(AppError::Coded {
            status: 400,
            code: "SHIFT_NOT_ON_DAY",
            reason: format!("{} isn't a shift on {}.", info.name, date.format("%A")),
        });
    }
    if let Some(b) = info.branch_id
        && !subject.branches.contains(&b)
    {
        return Err(AppError::Coded {
            status: 400,
            code: "SHIFT_OTHER_BRANCH",
            reason: format!("{} doesn't work at {}'s branch.", info.name, subject.name),
        });
    }
    if let Some((s, e)) = block.times
        && s == e
    {
        return Err(AppError::Coded {
            status: 400,
            code: "SHIFT_EMPTY",
            reason: "A shift can't start and end at the same time.".into(),
        });
    }
    Ok(info)
}

type Row = (Option<Uuid>, Option<NaiveTime>, Option<NaiveTime>);

async fn day_rows(
    conn: &mut PgConnection,
    employee_id: Uuid,
    date: NaiveDate,
) -> Result<Vec<Row>, AppError> {
    Ok(sqlx::query_as(
        "SELECT work_shift_id, start_time, end_time FROM staff_schedule_overrides \
          WHERE employee_id = $1 AND on_date = $2 FOR UPDATE",
    )
    .bind(employee_id)
    .bind(date)
    .fetch_all(&mut *conn)
    .await?)
}

#[allow(clippy::too_many_arguments)]
async fn insert_row(
    conn: &mut PgConnection,
    org_id: Uuid,
    employee_id: Uuid,
    date: NaiveDate,
    shift: Option<Uuid>,
    times: Option<(NaiveTime, NaiveTime)>,
    reason: Option<&str>,
    by: Option<Uuid>,
) -> Result<(), AppError> {
    sqlx::query(&format!(
        "INSERT INTO staff_schedule_overrides \
             (org_id, employee_id, on_date, work_shift_id, start_time, end_time, reason, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (employee_id, on_date, COALESCE(work_shift_id, {NIL})) DO UPDATE SET \
             start_time = EXCLUDED.start_time, end_time = EXCLUDED.end_time, \
             reason = EXCLUDED.reason, created_by = EXCLUDED.created_by"
    ))
    .bind(org_id)
    .bind(employee_id)
    .bind(date)
    .bind(shift)
    .bind(times.map(|t| t.0))
    .bind(times.map(|t| t.1))
    .bind(reason)
    .bind(by)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Turn a date that follows the pattern into its own set, as the pattern has it.
async fn materialise(
    conn: &mut PgConnection,
    org_id: Uuid,
    employee_id: Uuid,
    date: NaiveDate,
    reason: Option<&str>,
    by: Option<Uuid>,
) -> Result<(), AppError> {
    if !day_rows(conn, employee_id, date).await?.is_empty() {
        return Ok(());
    }
    let pattern = resolve_range(&mut *conn, &[employee_id], date, date, None).await?;
    if pattern.is_empty() {
        insert_row(conn, org_id, employee_id, date, None, None, reason, by).await?;
    }
    for s in pattern {
        insert_row(
            conn,
            org_id,
            employee_id,
            date,
            Some(s.work_shift_id),
            None,
            reason,
            by,
        )
        .await?;
    }
    Ok(())
}

/// The whole date for one person: exactly these blocks, or a day off when empty.
pub(crate) async fn replace_day(
    conn: &mut PgConnection,
    org_id: Uuid,
    employee_id: Uuid,
    date: NaiveDate,
    blocks: &[Block],
    reason: Option<&str>,
    by: Option<Uuid>,
) -> Result<(), AppError> {
    sqlx::query("DELETE FROM staff_schedule_overrides WHERE employee_id = $1 AND on_date = $2")
        .bind(employee_id)
        .bind(date)
        .execute(&mut *conn)
        .await?;
    if blocks.is_empty() {
        insert_row(conn, org_id, employee_id, date, None, None, reason, by).await?;
    }
    let mut seen = BTreeSet::new();
    for b in blocks {
        if !seen.insert(b.work_shift_id) {
            return Err(AppError::BadRequest(
                "A block can be on a day only once.".into(),
            ));
        }
        insert_row(
            conn,
            org_id,
            employee_id,
            date,
            Some(b.work_shift_id),
            b.times,
            reason,
            by,
        )
        .await?;
    }
    Ok(())
}

/// Back to the pattern: the date's own set is dropped. Rows removed.
pub(crate) async fn reset_day(
    conn: &mut PgConnection,
    employee_id: Uuid,
    date: NaiveDate,
) -> Result<u64, AppError> {
    Ok(
        sqlx::query("DELETE FROM staff_schedule_overrides WHERE employee_id = $1 AND on_date = $2")
            .bind(employee_id)
            .bind(date)
            .execute(&mut *conn)
            .await?
            .rows_affected(),
    )
}

/// Put one more block on a date; the rest of the day stays.
pub(crate) async fn add_block(
    conn: &mut PgConnection,
    org_id: Uuid,
    employee_id: Uuid,
    date: NaiveDate,
    block: &Block,
    reason: Option<&str>,
    by: Option<Uuid>,
) -> Result<(), AppError> {
    materialise(conn, org_id, employee_id, date, reason, by).await?;
    sqlx::query(
        "DELETE FROM staff_schedule_overrides \
          WHERE employee_id = $1 AND on_date = $2 AND work_shift_id IS NULL",
    )
    .bind(employee_id)
    .bind(date)
    .execute(&mut *conn)
    .await?;
    insert_row(
        conn,
        org_id,
        employee_id,
        date,
        Some(block.work_shift_id),
        block.times,
        reason,
        by,
    )
    .await
}

/// Take one block off a date; the rest of the day stays. `None` when the
/// person wasn't on that block that day (nothing changed); otherwise the
/// assignment's own times, if it had any.
pub(crate) async fn remove_block(
    conn: &mut PgConnection,
    org_id: Uuid,
    employee_id: Uuid,
    date: NaiveDate,
    work_shift_id: Uuid,
    reason: Option<&str>,
    by: Option<Uuid>,
) -> Result<Option<Option<(NaiveTime, NaiveTime)>>, AppError> {
    materialise(conn, org_id, employee_id, date, reason, by).await?;
    let gone: Option<(Option<NaiveTime>, Option<NaiveTime>)> = sqlx::query_as(
        "DELETE FROM staff_schedule_overrides \
          WHERE employee_id = $1 AND on_date = $2 AND work_shift_id = $3 \
          RETURNING start_time, end_time",
    )
    .bind(employee_id)
    .bind(date)
    .bind(work_shift_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((s, e)) = gone else {
        return Ok(None);
    };
    if day_rows(conn, employee_id, date).await?.is_empty() {
        insert_row(conn, org_id, employee_id, date, None, None, reason, by).await?;
    }
    Ok(Some(s.zip(e)))
}

/// This one assignment's own from/to (`None` = back to the block's times).
/// False when the person isn't on that block that day.
pub(crate) async fn set_times(
    conn: &mut PgConnection,
    org_id: Uuid,
    employee_id: Uuid,
    date: NaiveDate,
    work_shift_id: Uuid,
    times: Option<(NaiveTime, NaiveTime)>,
    by: Option<Uuid>,
) -> Result<bool, AppError> {
    materialise(conn, org_id, employee_id, date, None, by).await?;
    let n = sqlx::query(
        "UPDATE staff_schedule_overrides SET start_time = $4, end_time = $5, created_by = $6 \
          WHERE employee_id = $1 AND on_date = $2 AND work_shift_id = $3",
    )
    .bind(employee_id)
    .bind(date)
    .bind(work_shift_id)
    .bind(times.map(|t| t.0))
    .bind(times.map(|t| t.1))
    .bind(by)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(n > 0)
}

/// Two assignments of one person may not overlap, a night shift's morning
/// included. Checks every assignment touching `[from, to]`.
pub(crate) async fn check_overlaps(
    conn: &mut PgConnection,
    employee_id: Uuid,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<(), AppError> {
    let rows = resolve_range(
        &mut *conn,
        &[employee_id],
        from - Duration::days(1),
        to + Duration::days(1),
        None,
    )
    .await?;
    if let Some((a, b)) = first_overlap(&rows, from, to) {
        // The two blocks and their dates, for the client's own wording
        // (AT-13, E2E B-ROTA-3): `date` is the first one's.
        return Err(AppError::CodedVars {
            status: 409,
            code: "SHIFTS_OVERLAP",
            reason: format!(
                "{} on {} and {} on {} overlap.",
                a.name, a.on_date, b.name, b.on_date
            ),
            vars: json!({ "a": a.name, "b": b.name, "date": a.on_date,
                          "a_date": a.on_date, "b_date": b.on_date }),
        });
    }
    Ok(())
}

/// The first pair of one person's assignments that overlap, at least one of
/// them dated inside `[from, to]`.
pub(crate) fn first_overlap(
    rows: &[ResolvedShift],
    from: NaiveDate,
    to: NaiveDate,
) -> Option<(&ResolvedShift, &ResolvedShift)> {
    let inside = |d: NaiveDate| d >= from && d <= to;
    for (i, a) in rows.iter().enumerate() {
        for b in &rows[i + 1..] {
            if a.employee_id == b.employee_id
                && (inside(a.on_date) || inside(b.on_date))
                && a.scheduled_start_at < b.scheduled_end_at
                && b.scheduled_start_at < a.scheduled_end_at
            {
                return Some((a, b));
            }
        }
    }
    None
}

/// A manual edit teaches the engine what the manager wanted (SUG-learn):
/// weaker than accepting a suggestion, kept apart by `source`.
pub(crate) async fn log_manual(
    conn: &mut PgConnection,
    org_id: Uuid,
    employee_id: Uuid,
    date: NaiveDate,
    work_shift_id: Uuid,
    by: Option<Uuid>,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO staff_suggestion_events \
             (org_id, branch_id, suggestion, employee_id, on_date, accepted, decided_by, \
              work_shift_id, source) \
         SELECT $1, b, $2, $3, $4, true, $5, $6, 'manual' FROM ( \
             SELECT COALESCE(ws.branch_id, ( \
                 SELECT eb.branch_id FROM employee_branches eb \
                  WHERE eb.employee_id = $3 ORDER BY eb.assigned_at LIMIT 1)) AS b \
               FROM work_shifts ws WHERE ws.id = $6 AND ws.org_id = $1) x \
          WHERE b IS NOT NULL",
    )
    .bind(org_id)
    .bind(format!("manual|{date}|{work_shift_id}|{employee_id}"))
    .bind(employee_id)
    .bind(date)
    .bind(by)
    .bind(work_shift_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

// ── SC-4: who a change touched ────────────────────────────────────────────

/// Each person's rostered assignments per date, as start/end instants.
pub(crate) type Snapshot = HashMap<(Uuid, NaiveDate), Vec<(Uuid, DateTime<Utc>, DateTime<Utc>)>>;

pub(crate) async fn snapshot(
    conn: &mut PgConnection,
    employees: &[Uuid],
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Snapshot, AppError> {
    let mut out: Snapshot = HashMap::new();
    if employees.is_empty() || to < from {
        return Ok(out);
    }
    for s in resolve_range(&mut *conn, employees, from, to, None).await? {
        out.entry((s.employee_id, s.on_date)).or_default().push((
            s.work_shift_id,
            s.scheduled_start_at,
            s.scheduled_end_at,
        ));
    }
    for v in out.values_mut() {
        v.sort();
    }
    Ok(out)
}

/// The (person, date) pairs whose assignments differ.
pub(crate) fn diff(before: &Snapshot, after: &Snapshot) -> BTreeSet<(Uuid, NaiveDate)> {
    let empty = Vec::new();
    before
        .keys()
        .chain(after.keys())
        .filter(|k| before.get(*k).unwrap_or(&empty) != after.get(*k).unwrap_or(&empty))
        .copied()
        .collect()
}

/// From yesterday to the end of the last published week at these people's
/// branches — the part of the roster anyone has been shown. `None` = nothing
/// published ahead, so no change can be "after publish".
pub(crate) async fn published_horizon(
    conn: &mut PgConnection,
    employees: &[Uuid],
) -> Result<Option<(NaiveDate, NaiveDate)>, AppError> {
    let last: Option<NaiveDate> = sqlx::query_scalar(
        "SELECT MAX(p.week_start) FROM staff_week_publications p \
          WHERE p.branch_id IN (SELECT branch_id FROM employee_branches \
                                 WHERE employee_id = ANY($1)) \
            AND p.week_start >= CURRENT_DATE - 8",
    )
    .bind(employees)
    .fetch_one(&mut *conn)
    .await?;
    Ok(last.map(|w| {
        (
            Utc::now().date_naive() - Duration::days(1),
            w + Duration::days(6),
        )
    }))
}

/// A published week changed for these people on these dates: mark each day
/// "changed" (the app shows it) and tell each person once (SC-4). Days in
/// weeks nobody published stay drafts and are silent.
pub(crate) async fn mark_changed(
    pool: &PgPool,
    org_id: Uuid,
    changes: &BTreeSet<(Uuid, NaiveDate)>,
) -> Result<(), AppError> {
    mark_changed_and_tell(pool, org_id, changes, true).await
}

/// [`mark_changed`], saying whether to tell: a swap or a claim decision
/// sends its own notice, so the change is only marked.
pub(crate) async fn mark_changed_and_tell(
    pool: &PgPool,
    org_id: Uuid,
    changes: &BTreeSet<(Uuid, NaiveDate)>,
    tell: bool,
) -> Result<(), AppError> {
    let mut told: BTreeSet<Uuid> = BTreeSet::new();
    for &(employee_id, date) in changes {
        let published: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM staff_week_publications p \
                             JOIN employee_branches eb ON eb.branch_id = p.branch_id \
                            WHERE eb.employee_id = $1 AND p.week_start = $2)",
        )
        .bind(employee_id)
        .bind(week_start(date))
        .fetch_one(pool)
        .await?;
        if !published {
            continue;
        }
        sqlx::query(
            "INSERT INTO staff_roster_changes (org_id, employee_id, on_date) VALUES ($1, $2, $3) \
             ON CONFLICT (employee_id, on_date) DO UPDATE SET changed_at = now()",
        )
        .bind(org_id)
        .bind(employee_id)
        .bind(date)
        .execute(pool)
        .await?;
        if tell && told.insert(employee_id) {
            notify(
                pool,
                org_id,
                employee_id,
                "staff.n_shift_changed",
                json!({ "date": date }),
            )
            .await;
        }
    }
    Ok(())
}
