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
//!
//! ONLY LIVE DAWAM ORGS (PS-7, SA-3, audit B3). Every step that writes
//! attendance or money skips an org that is suspended, deleted or has Dawam
//! switched off, and an employee who is not active: switching Dawam back on
//! must not show absences and penalties for the time it was off. The
//! coordinate purge (step 4) is a privacy duty and runs for everyone.

/// The SQL test for "this org's Dawam runs": join `organizations o` on it.
const LIVE_ORG: &str = "o.is_active AND o.deleted_at IS NULL AND 'dawam' = ANY(o.modules)";

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

/// One pass of the sweep (the tests drive it directly).
#[doc(hidden)]
pub async fn run_tick(pool: &PgPool) -> Result<(), crate::errors::AppError> {
    close_forgotten_checkouts(pool).await?;
    mark_absences(pool).await?;
    apply_pending_penalties(pool).await?;
    purge_stale_coordinates(pool).await?;
    precompute_suggestions(pool).await?;
    crate::staff::dawam::suggest::monthly_fairness(pool).await?;
    phones_that_died(pool).await?;
    Ok(())
}

/// A shift gone quiet after a low battery reads "phone likely died" — never
/// "left the branch" (CL-12). Pings come every 15 minutes; three missed is
/// quiet.
#[doc(hidden)]
pub async fn phones_that_died(pool: &PgPool) -> Result<(), crate::errors::AppError> {
    let quiet: Vec<(Uuid, Uuid, Uuid, Uuid)> = sqlx::query_as(&format!(
        "SELECT a.org_id, a.employee_id, a.branch_id, a.id FROM attendance_records a \
           JOIN organizations o ON o.id = a.org_id AND {LIVE_ORG} \
           JOIN employees e ON e.id = a.employee_id AND e.employment_status = 'active' \
           JOIN LATERAL (SELECT at, battery_percent FROM attendance_pings p \
                          WHERE p.attendance_record_id = a.id ORDER BY at DESC LIMIT 1) last ON true \
          WHERE a.check_in_at IS NOT NULL AND a.check_out_at IS NULL \
            AND last.at < now() - INTERVAL '45 minutes' \
            AND last.battery_percent <= $1 \
            AND NOT EXISTS (SELECT 1 FROM attendance_flags f \
                             WHERE f.attendance_record_id = a.id AND f.kind = 'phone_died') \
          LIMIT 500"
    ))
    .bind(crate::staff::dawam::presence::LOW_BATTERY)
    .fetch_all(pool)
    .await?;
    for (org_id, employee_id, branch_id, record_id) in quiet {
        crate::staff::dawam::presence::raise_flag(
            pool,
            org_id,
            employee_id,
            Some(branch_id),
            Some(record_id),
            "phone_died",
            0,
        )
        .await?;
    }
    Ok(())
}

/// Roster suggestions for the week starting Saturday are computed from
/// Wednesday 22:00 branch time, so the button is instant (SC-13). A branch
/// already holding that week is skipped; the cache drops itself when the
/// roster changes. Learning history is kept 24 months.
#[doc(hidden)]
pub async fn precompute_suggestions(pool: &PgPool) -> Result<(), crate::errors::AppError> {
    let due: Vec<(Uuid, Uuid, NaiveDate)> = sqlx::query_as(
        "WITH b AS ( \
             SELECT b.org_id, b.id, \
                    (now() AT TIME ZONE COALESCE(b.timezone::text, o.timezone::text)) AS local \
               FROM branches b JOIN organizations o ON o.id = b.org_id \
              WHERE o.is_active AND o.deleted_at IS NULL AND 'dawam' = ANY(o.modules) \
                AND b.deleted_at IS NULL \
         ) \
         SELECT b.org_id, b.id, \
                (b.local::date + (6 - EXTRACT(DOW FROM b.local)::int))::date AS week \
           FROM b \
          WHERE ((EXTRACT(DOW FROM b.local) = 3 AND b.local::time >= '22:00') \
                 OR EXTRACT(DOW FROM b.local) IN (4, 5)) \
            AND EXISTS (SELECT 1 FROM employee_branches a WHERE a.branch_id = b.id) \
            AND NOT EXISTS (SELECT 1 FROM staff_suggestion_cache c \
                             WHERE c.branch_id = b.id AND NOT c.stale \
                               AND c.week_start = (b.local::date + (6 - EXTRACT(DOW FROM b.local)::int))) \
          LIMIT 50",
    )
    .fetch_all(pool)
    .await?;
    for (org_id, branch_id, week) in due {
        if let Err(e) = crate::staff::dawam::roster::precompute(pool, org_id, branch_id, week).await
        {
            tracing::warn!(%branch_id, "suggestion precompute failed: {e}");
        }
    }
    sqlx::query(
        "DELETE FROM staff_suggestion_events WHERE created_at < now() - INTERVAL '24 months'",
    )
    .execute(pool)
    .await?;
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
    let stale: Vec<Open> = sqlx::query_as(&format!(
        "SELECT a.id, a.org_id, a.branch_id, a.check_in_at, a.scheduled_start_at, \
                a.scheduled_end_at, \
                COALESCE(ws.break_minutes, 0) AS break_minutes, \
                COALESCE(ws.paid_break, TRUE) AS paid_break, \
                ws.half_day_threshold_minutes \
           FROM attendance_records a \
           JOIN organizations o ON o.id = a.org_id AND {LIVE_ORG} \
           JOIN employees e ON e.id = a.employee_id AND e.employment_status = 'active' \
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
          LIMIT 500"
    ))
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

#[doc(hidden)]
pub async fn mark_absences(pool: &PgPool) -> Result<(), crate::errors::AppError> {
    #[derive(sqlx::FromRow)]
    struct Missing {
        org_id: Uuid,
        employee_id: Uuid,
        branch_id: Uuid,
        work_shift_id: Uuid,
        business_date: NaiveDate,
        scheduled_start_at: DateTime<Utc>,
        scheduled_end_at: DateTime<Utc>,
        excused: bool,
    }

    // Yesterday and today only, in each shift's BRANCH-local calendar (AT-1):
    // a sweep that reached back further would resurrect absences an operator
    // had deliberately deleted. The roster comes from the one roster function
    // (SC-6, AT-9): a date change beats the weekday row beats the every-day
    // row, so a shift given by a date change (a day edit, swap, claim or
    // accepted suggestion) is missed like any other, a split day's shifts are
    // each their own, and a day off marks nobody.
    let missing: Vec<Missing> = sqlx::query_as(&format!(
        r#"
        WITH people AS (
            SELECT p.id
              FROM employees p
              JOIN organizations o ON o.id = p.org_id AND {LIVE_ORG}
             WHERE p.employment_status = 'active'
        ),
        rostered AS (
            SELECT e.org_id, r.employee_id, r.branch_id, r.work_shift_id,
                   r.on_date AS business_date, r.start_at, r.end_at, r.tz
              FROM dawam_roster(ARRAY(SELECT id FROM people),
                                CURRENT_DATE - 2, CURRENT_DATE + 1) r
              JOIN employees e ON e.id = r.employee_id
        )
        SELECT r.org_id, r.employee_id, r.branch_id, r.work_shift_id, r.business_date,
               r.start_at AS scheduled_start_at, r.end_at AS scheduled_end_at,
               -- One table covers leave AND missions: both are whole-day
               -- approvals, so a day either is excused or is an absence.
               EXISTS (
                   SELECT 1 FROM staff_requests sr
                    WHERE sr.employee_id = r.employee_id AND sr.status = 'approved'
                      AND sr.kind IN ('leave', 'mission')
                      AND sr.on_date <= r.business_date
                      AND COALESCE(sr.end_date, sr.on_date) >= r.business_date
               ) AS excused
          FROM rostered r
         WHERE r.branch_id IS NOT NULL
           AND r.business_date >= (now() AT TIME ZONE r.tz)::date - 1
           AND r.business_date <= (now() AT TIME ZONE r.tz)::date
           -- The shift must be over before its absence is a fact.
           AND now() > r.end_at
           -- A confirmed public holiday marks nobody absent (RU-10).
           AND NOT EXISTS (
               SELECT 1 FROM staff_holidays h
                WHERE h.org_id = r.org_id AND h.on_date = r.business_date
                  AND h.decision = 'holiday'
           )
           AND NOT EXISTS (
               SELECT 1 FROM attendance_records a
                WHERE a.employee_id = r.employee_id
                  AND a.business_date = r.business_date
                  AND a.covered_employee_id IS NULL
                  AND COALESCE(a.work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid)
                      = r.work_shift_id
           )
         ORDER BY r.business_date, r.employee_id
         LIMIT 500
        "#
    ))
    .fetch_all(pool)
    .await?;

    for row in missing {
        sqlx::query(
            "INSERT INTO attendance_records \
                 (org_id, employee_id, branch_id, work_shift_id, business_date, status, \
                  scheduled_start_at, scheduled_end_at, is_manual, edit_reason) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, FALSE, 'Marked automatically: no check-in') \
             ON CONFLICT (employee_id, business_date, \
                          COALESCE(work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid)) WHERE covered_employee_id IS NULL \
             DO NOTHING",
        )
        .bind(row.org_id)
        .bind(row.employee_id)
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
    let rows: Vec<Pending> = sqlx::query_as(&format!(
        "SELECT a.id, a.org_id, a.branch_id \
           FROM attendance_records a \
           JOIN organizations o ON o.id = a.org_id AND {LIVE_ORG} \
           JOIN employees e ON e.id = a.employee_id AND e.employment_status = 'active' \
          WHERE a.business_date >= CURRENT_DATE - 7 \
            AND (a.check_out_at IS NOT NULL OR a.status IN ('absent', 'on_leave')) \
            AND (a.late_minutes > 0 OR a.status IN ('absent', 'on_leave')) \
            AND NOT EXISTS ( \
                SELECT 1 FROM payroll_deductions d \
                 WHERE d.attendance_record_id = a.id AND d.source <> 'manual' \
            ) \
          LIMIT 500"
    ))
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
pub async fn purge_stale_coordinates(pool: &PgPool) -> Result<(), crate::errors::AppError> {
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
