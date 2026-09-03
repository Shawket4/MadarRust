//! The attendance sweep — one background task spawned once from `main` (NOT per
//! worker), mirroring `reservations::nudge`.
//!
//! Every tick it does four things, in order, for every org:
//!
//!   1. **Close forgotten checkouts.** A record still open past its scheduled end
//!      plus the org's buffer is closed AT the scheduled end with
//!      `check_out_method = 'auto'`. It accrues NO overtime — the system knows
//!      when the shift was supposed to finish, not when the person actually left,
//!      and paying overtime for a forgotten button is how payroll leaks money.
//!      An operator who knows better corrects the row by hand.
//!
//!   2. **Mark absences.** An employee rostered for a shift that has finished, with
//!      no attendance record at all, gets one: `on_leave` when approved leave or
//!      an approved mission covers the day, otherwise `absent`.
//!
//!   3. **Apply late penalties.** Closed late records get their tier deduction,
//!      idempotently — the partial unique index on
//!      `(attendance_record_id, source)` turns a re-run into an update.
//!
//!   4. **Purge stale attendance coordinates.** Latitude/longitude older than
//!      `COORD_RETENTION_DAYS` are nulled. See `purge_stale_coordinates`.
//!
//! Runs on the OWNER pool, which bypasses RLS. That is the sanctioned path for
//! cross-tenant background work (see `src/db.rs`); every query below is explicitly
//! keyed by `org_id` regardless.

use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::staff::attendance::load_settings;
use crate::staff::penalties;

/// Spawn the sweep. No-op when `ATTENDANCE_SWEEP_ENABLED` is falsy.
pub fn spawn(pool: PgPool) {
    let disabled = std::env::var("ATTENDANCE_SWEEP_ENABLED")
        .map(|v| matches!(v.as_str(), "0" | "false" | "no" | "off"))
        .unwrap_or(false);
    if disabled {
        tracing::info!("Attendance sweep disabled (ATTENDANCE_SWEEP_ENABLED)");
        return;
    }
    let secs = std::env::var("ATTENDANCE_SWEEP_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(600)
        .max(60);

    tracing::info!("Attendance sweep started ({secs}s tick)");
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(secs));
        loop {
            ticker.tick().await;
            // Guarded at the job boundary: a returned error used to be a log
            // line nobody saw, and a panic killed this loop for the life of the
            // process while reporting nothing at all.
            crate::observability::report::guarded_tick("attendance_sweep", || run_tick(&pool))
                .await;
        }
    });
}

async fn run_tick(pool: &PgPool) -> Result<(), crate::errors::AppError> {
    close_forgotten_checkouts(pool).await?;
    mark_absences(pool).await?;
    apply_pending_penalties(pool).await?;
    purge_stale_coordinates(pool).await?;
    Ok(())
}

// ── 1. Forgotten checkouts ────────────────────────────────────

async fn close_forgotten_checkouts(pool: &PgPool) -> Result<(), crate::errors::AppError> {
    #[derive(sqlx::FromRow)]
    struct Open {
        id: Uuid,
        org_id: Uuid,
        branch_id: Uuid,
        check_in_at: DateTime<Utc>,
        scheduled_start_at: DateTime<Utc>,
        scheduled_end_at: DateTime<Utc>,
        break_minutes: i32,
        paid_break: bool,
        half_day_threshold_minutes: Option<i32>,
    }

    // Only rows with a known scheduled end can be auto-closed: an unrostered
    // check-in has no "supposed to finish" to close it at, so it stays open for a
    // human to resolve.
    let stale: Vec<Open> = sqlx::query_as(
        "SELECT a.id, a.org_id, a.branch_id, a.check_in_at, a.scheduled_start_at, \
                a.scheduled_end_at, \
                COALESCE(ws.break_minutes, 0) AS break_minutes, \
                COALESCE(ws.paid_break, TRUE) AS paid_break, \
                ws.half_day_threshold_minutes \
           FROM attendance_records a \
           LEFT JOIN work_shifts ws ON ws.id = a.work_shift_id \
          WHERE a.check_in_at IS NOT NULL \
            AND a.check_out_at IS NULL \
            AND a.scheduled_end_at IS NOT NULL \
            AND a.scheduled_start_at IS NOT NULL \
            AND now() > a.scheduled_end_at + make_interval(mins => COALESCE(( \
                    SELECT s.auto_checkout_buffer_minutes FROM attendance_settings s \
                     WHERE s.org_id = a.org_id \
                       AND (s.branch_id = a.branch_id OR s.branch_id IS NULL) \
                     ORDER BY s.branch_id NULLS LAST LIMIT 1), 120)) \
          LIMIT 500",
    )
    .fetch_all(pool)
    .await?;

    for row in stale {
        let worked = crate::staff::rules::worked_minutes(
            row.check_in_at,
            row.scheduled_end_at,
            row.break_minutes,
            row.paid_break,
        );
        let span = (row.scheduled_end_at - row.scheduled_start_at)
            .num_minutes()
            .max(0);
        // Late minutes were settled at check-in; only the closing figures move.
        let late: i32 =
            sqlx::query_scalar("SELECT late_minutes FROM attendance_records WHERE id = $1")
                .bind(row.id)
                .fetch_one(pool)
                .await?;
        // These rows all have a check-in by definition (that is what makes them
        // "still open"), so they can never come out of this as absent.
        let status = crate::staff::rules::classify(
            true,
            worked,
            span,
            row.half_day_threshold_minutes,
            late as i64,
        );

        sqlx::query(
            "UPDATE attendance_records SET \
                 check_out_at = scheduled_end_at, check_out_method = 'auto', \
                 worked_minutes = $2, overtime_minutes = 0, early_leave_minutes = 0, \
                 status = $3, \
                 edit_reason = COALESCE(edit_reason, 'Auto-closed: no checkout recorded'), \
                 updated_at = now() \
               WHERE id = $1 AND check_out_at IS NULL",
        )
        .bind(row.id)
        .bind(worked as i32)
        .bind(status.as_str())
        .execute(pool)
        .await?;

        tracing::debug!(
            record = %row.id, org = %row.org_id, branch = %row.branch_id,
            "auto-closed a forgotten checkout"
        );
    }
    Ok(())
}

// ── 2. Absences ───────────────────────────────────────────────

async fn mark_absences(pool: &PgPool) -> Result<(), crate::errors::AppError> {
    #[derive(sqlx::FromRow)]
    struct Missing {
        org_id: Uuid,
        user_id: Uuid,
        branch_id: Uuid,
        work_shift_id: Uuid,
        business_date: NaiveDate,
        scheduled_start_at: DateTime<Utc>,
        scheduled_end_at: DateTime<Utc>,
        excused: bool,
    }

    // Yesterday and today only: a sweep that reached back further would resurrect
    // absences an operator had deliberately deleted.
    let missing: Vec<Missing> = sqlx::query_as(
        r#"
        WITH days AS (
            SELECT d::date AS business_date
              FROM generate_series(CURRENT_DATE - 1, CURRENT_DATE, INTERVAL '1 day') d
        ),
        rostered AS (
            SELECT p.org_id,
                   p.user_id,
                   d.business_date,
                   ws.id AS work_shift_id,
                   COALESCE(ws.branch_id, (
                       SELECT uba.branch_id FROM user_branch_assignments uba
                        WHERE uba.user_id = p.user_id LIMIT 1
                   )) AS branch_id,
                   COALESCE(b.timezone::text, o.timezone::text, 'Africa/Cairo') AS tz,
                   ws.start_time, ws.end_time, ws.crosses_midnight
              FROM staff_profiles p
              JOIN users u ON u.id = p.user_id AND u.deleted_at IS NULL
              JOIN organizations o ON o.id = p.org_id
              CROSS JOIN days d
              JOIN staff_schedules s
                ON s.user_id = p.user_id
               AND s.effective_from <= d.business_date
               AND (s.effective_to IS NULL OR s.effective_to >= d.business_date)
               AND (s.day_of_week IS NULL
                    OR s.day_of_week = EXTRACT(DOW FROM d.business_date)::smallint)
              JOIN work_shifts ws ON ws.id = s.work_shift_id AND ws.is_active
              LEFT JOIN branches b ON b.id = ws.branch_id AND b.deleted_at IS NULL
             WHERE p.employment_status = 'active'
               -- An explicit override (including a day off) wins outright; those
               -- days are simply not rostered.
               AND NOT EXISTS (
                   SELECT 1 FROM staff_schedule_overrides ov
                    WHERE ov.user_id = p.user_id AND ov.on_date = d.business_date
               )
        )
        SELECT r.org_id, r.user_id, r.branch_id, r.work_shift_id, r.business_date,
               (r.business_date + r.start_time) AT TIME ZONE r.tz AS scheduled_start_at,
               (r.business_date + r.end_time
                    + CASE WHEN r.crosses_midnight
                           THEN INTERVAL '1 day' ELSE INTERVAL '0 day' END
               ) AT TIME ZONE r.tz AS scheduled_end_at,
               -- One table now covers leave AND missions: both are whole-day
               -- approvals, so a day either is excused or is an absence.
               EXISTS (
                   SELECT 1 FROM staff_requests sr
                    WHERE sr.user_id = r.user_id AND sr.status = 'approved'
                      AND sr.kind IN ('leave', 'mission')
                      AND sr.on_date <= r.business_date
                      AND COALESCE(sr.end_date, sr.on_date) >= r.business_date
               ) AS excused
          FROM rostered r
         WHERE r.branch_id IS NOT NULL
           -- The shift must be over before its absence is a fact.
           AND now() > (r.business_date + r.end_time
                    + CASE WHEN r.crosses_midnight
                           THEN INTERVAL '1 day' ELSE INTERVAL '0 day' END
               ) AT TIME ZONE r.tz
           AND NOT EXISTS (
               SELECT 1 FROM attendance_records a
                WHERE a.user_id = r.user_id
                  AND a.business_date = r.business_date
                  AND COALESCE(a.work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid)
                      = r.work_shift_id
           )
         LIMIT 500
        "#,
    )
    .fetch_all(pool)
    .await?;

    for row in missing {
        sqlx::query(
            "INSERT INTO attendance_records \
                 (org_id, user_id, branch_id, work_shift_id, business_date, status, \
                  scheduled_start_at, scheduled_end_at, is_manual, edit_reason) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, FALSE, 'Marked automatically: no check-in') \
             ON CONFLICT (user_id, business_date, \
                          COALESCE(work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid)) \
             DO NOTHING",
        )
        .bind(row.org_id)
        .bind(row.user_id)
        .bind(row.branch_id)
        .bind(row.work_shift_id)
        .bind(row.business_date)
        .bind(if row.excused { "on_leave" } else { "absent" })
        .bind(row.scheduled_start_at)
        .bind(row.scheduled_end_at)
        .execute(pool)
        .await?;
    }
    Ok(())
}

// ── 3. Automatic deductions ───────────────────────────────────

/// Price every recently-closed day that has not been priced yet.
///
/// The live paths (check-out, manual entry, correction) already call
/// `penalties::recompute_record` themselves, so this is the safety net for days
/// the sweep itself closed and for anything a restart interrupted. It is
/// idempotent, and `penalties` refuses to touch a row a human has waived or
/// overridden — so a manager's decision made during the day survives the night.
async fn apply_pending_penalties(pool: &PgPool) -> Result<(), crate::errors::AppError> {
    #[derive(sqlx::FromRow)]
    struct Pending {
        id: Uuid,
        org_id: Uuid,
        branch_id: Uuid,
    }

    // Closed days (or absences) from the last week that carry no deduction row
    // yet. A day whose penalty was already written and then waived is excluded by
    // the EXISTS, so it is never revisited.
    let rows: Vec<Pending> = sqlx::query_as(
        "SELECT a.id, a.org_id, a.branch_id \
           FROM attendance_records a \
          WHERE a.business_date >= CURRENT_DATE - 7 \
            AND (a.check_out_at IS NOT NULL OR a.status IN ('absent', 'on_leave')) \
            AND (a.late_minutes > 0 OR a.status IN ('absent', 'on_leave')) \
            AND NOT EXISTS ( \
                SELECT 1 FROM payroll_deductions d \
                 WHERE d.attendance_record_id = a.id AND d.source <> 'manual' \
            ) \
          LIMIT 500",
    )
    .fetch_all(pool)
    .await?;

    for row in rows {
        let settings = load_settings(pool, row.org_id, Some(row.branch_id)).await?;
        let mut conn = pool.acquire().await?;
        penalties::recompute_record(&mut conn, row.id, &settings).await?;
    }
    Ok(())
}

// ── 4. Attendance coordinate retention ────────────────────────

/// How long a punch's GPS coordinates are kept. After this, the latitude and
/// longitude are nulled and the record keeps only *when* the punch happened and
/// whether it passed the geofence.
const COORD_RETENTION_DAYS: i32 = 90;

/// Null out attendance coordinates older than `COORD_RETENTION_DAYS`.
///
/// Attendance *times* must live as long as payroll does — they are the evidence
/// for what someone was paid. The *coordinates* do not: once a punch is settled
/// and no longer disputed, latitude and longitude have served their only purpose
/// (proving the person was at the branch when they clocked in). Keeping precise
/// employee locations for years to support a payslip is hard to defend as
/// proportionate, so we stop keeping them.
///
/// Deliberately preserved:
///   * `check_in_at` / `check_out_at` and every derived minute count — payroll.
///   * `check_in_distance_meters` / `check_out_distance_meters` — the geofence
///     RESULT. It records that the punch was N metres from the branch, which is
///     the auditable fact, without recording WHERE the employee was.
///   * `check_in_method` / `check_out_method`.
///
/// Idempotent: rows already purged fail the `IS NOT NULL` test, so a re-run is a
/// no-op. Bounded per tick so a first run over a large backlog cannot hold long
/// locks — the remainder is picked up on the next tick.
pub(super) async fn purge_stale_coordinates(pool: &PgPool) -> Result<(), crate::errors::AppError> {
    let purged = sqlx::query(
        "UPDATE attendance_records SET \
             check_in_latitude = NULL, check_in_longitude = NULL, \
             check_out_latitude = NULL, check_out_longitude = NULL, \
             updated_at = now() \
           WHERE id IN ( \
               SELECT id FROM attendance_records \
                WHERE business_date < CURRENT_DATE - $1 \
                  AND (check_in_latitude IS NOT NULL OR check_in_longitude IS NOT NULL \
                       OR check_out_latitude IS NOT NULL OR check_out_longitude IS NOT NULL) \
                ORDER BY business_date \
                LIMIT 1000 \
           )",
    )
    .bind(COORD_RETENTION_DAYS)
    .execute(pool)
    .await?
    .rows_affected();

    if purged > 0 {
        tracing::info!(
            records = purged,
            retention_days = COORD_RETENTION_DAYS,
            "purged stale attendance coordinates"
        );
    }
    Ok(())
}
