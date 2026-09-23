//! Public holidays (RU-10): suggested, never applied on their own.
//!
//! Egypt's fixed-date holidays and its Islamic ones are SUGGESTED for every
//! year; the owner decides each (`holiday` = nobody is marked absent and
//! working it pays the holiday multiplier, `dismissed` = a normal day).
//!
//! - The Islamic dates come from the arithmetic (tabular) Hijri calendar, so
//!   the suggestions never run out (audit B15: nothing was suggested after
//!   2027). They are the EXPECTED dates; the moon can move them a day, which
//!   is why every one waits for a person to confirm it. Where the published
//!   expectation is known it wins over the arithmetic.
//! - Reading never writes (audit B15): a GET merges the suggestions with the
//!   stored decisions in memory. Only a decision stores a row.
//! - A decision re-prices the day (audit RU-10): declaring a past day a
//!   holiday takes back the absences the sweep had marked on it, with their
//!   deductions; dismissing it again marks the rostered no-shows absent.
//!   An approved month is frozen, so a decision inside one is refused
//!   (`PERIOD_CLOSED`).

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::authz::Cap;
use crate::errors::{AppError, AppErrorResponse};
use crate::staff::access;
use crate::staff::principal::caller;

#[derive(Serialize, ToSchema, sqlx::FromRow, Clone, Debug, PartialEq)]
pub struct HolidayView {
    pub on_date: NaiveDate,
    pub name_en: String,
    pub name_ar: String,
    /// null = not decided yet: a normal day unless set up (RU-10).
    pub decision: Option<String>,
}

// ── The calendar ────────────────────────────────────────────────────────────

/// Julian day number (at noon) of a date in the arithmetic Islamic calendar
/// (the civil / Friday epoch, 16 July 622 CE Julian). The same formula the
/// usual converters use: months alternate 30/29 days, and 11 of every 30
/// years have a leap day at the end of Dhu al-Hijjah.
fn hijri_to_jdn(year: i64, month: i64, day: i64) -> i64 {
    // ceil(29.5 × (month − 1)) without floating point.
    let month_days = (59 * (month - 1) + 1) / 2;
    day + month_days + (year - 1) * 354 + (3 + 11 * year).div_euclid(30) + ISLAMIC_EPOCH_JDN - 1
}

/// Julian day number of 1 Muharram 1 AH (16 July 622, Julian calendar).
const ISLAMIC_EPOCH_JDN: i64 = 1_948_440;

/// The Gregorian date of a Julian day number.
fn jdn_to_date(jdn: i64) -> Option<NaiveDate> {
    // 2440588 = 1970-01-01.
    NaiveDate::from_ymd_opt(1970, 1, 1)?.checked_add_signed(chrono::Duration::days(jdn - 2_440_588))
}

/// The expected Gregorian date of a Hijri date.
pub fn hijri_to_gregorian(year: i64, month: i64, day: i64) -> Option<NaiveDate> {
    jdn_to_date(hijri_to_jdn(year, month, day))
}

/// Islamic holidays Egypt keeps, as (Hijri month, day, EN, AR).
const ISLAMIC: [(i64, i64, &str, &str); 4] = [
    (10, 1, "Eid al-Fitr", "عيد الفطر"),
    (12, 10, "Eid al-Adha", "عيد الأضحى"),
    (1, 1, "Islamic New Year", "رأس السنة الهجرية"),
    (3, 12, "Prophet's Birthday", "المولد النبوي"),
];

/// Published expectations that differ from the arithmetic calendar, by
/// (Gregorian year, English name). Kept where known; the arithmetic answers
/// every other year.
const PUBLISHED: [(i32, &str, u32, u32); 8] = [
    (2026, "Eid al-Fitr", 3, 20),
    (2026, "Eid al-Adha", 5, 27),
    (2026, "Islamic New Year", 6, 16),
    (2026, "Prophet's Birthday", 8, 25),
    (2027, "Eid al-Fitr", 3, 10),
    (2027, "Eid al-Adha", 5, 16),
    (2027, "Islamic New Year", 6, 6),
    (2027, "Prophet's Birthday", 8, 15),
];

/// Egypt's public holidays falling in `year`, suggested — never applied
/// automatically. An Islamic holiday can fall twice in one Gregorian year
/// (the lunar year is 11 days shorter); both are suggested.
pub fn egypt_holidays(year: i32) -> Vec<(NaiveDate, &'static str, &'static str)> {
    let mut out: Vec<(NaiveDate, &'static str, &'static str)> = [
        (1, 7, "Coptic Christmas", "عيد الميلاد المجيد"),
        (1, 25, "Revolution Day (25 January)", "عيد ثورة 25 يناير"),
        (4, 25, "Sinai Liberation Day", "عيد تحرير سيناء"),
        (5, 1, "Labour Day", "عيد العمال"),
        (6, 30, "30 June Revolution", "ذكرى ثورة 30 يونيو"),
        (7, 23, "Revolution Day (23 July)", "عيد ثورة 23 يوليو"),
        (10, 6, "Armed Forces Day", "عيد القوات المسلحة"),
    ]
    .into_iter()
    .filter_map(|(m, d, en, ar)| Some((NaiveDate::from_ymd_opt(year, m, d)?, en, ar)))
    .collect();

    // Hijri years that can touch this Gregorian year.
    let approx = ((i64::from(year) - 622) * 33) / 32;
    for hy in (approx - 1)..=(approx + 2) {
        for (m, d, en, ar) in ISLAMIC {
            let Some(date) = hijri_to_gregorian(hy, m, d) else {
                continue;
            };
            if date.year() != year {
                continue;
            }
            let date = PUBLISHED
                .iter()
                .find(|(y, name, _, _)| *y == year && *name == en)
                .and_then(|(_, _, pm, pd)| NaiveDate::from_ymd_opt(year, *pm, *pd))
                // The published date replaces the arithmetic one only when
                // they are the same holiday (a day or two apart).
                .filter(|p| (*p - date).num_days().abs() <= 2)
                .unwrap_or(date);
            if !out.iter().any(|(o, _, _)| *o == date) {
                out.push((date, en, ar));
            }
        }
    }
    out.sort_by_key(|(d, _, _)| *d);
    out
}

/// The suggestions in `[from, to]`.
fn suggested_between(from: NaiveDate, to: NaiveDate) -> Vec<(NaiveDate, &'static str, &'static str)> {
    (from.year()..=to.year())
        .flat_map(egypt_holidays)
        .filter(|(d, _, _)| *d >= from && *d <= to)
        .collect()
}

/// The holidays of `[from, to]`: every suggestion, with the owner's decision
/// where there is one, plus any decided day no longer suggested. Read-only.
pub async fn holidays_in(
    pool: &PgPool,
    org_id: Uuid,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<HolidayView>, AppError> {
    let stored: Vec<HolidayView> = sqlx::query_as(
        "SELECT on_date, name_en, name_ar, decision FROM staff_holidays \
          WHERE org_id = $1 AND on_date BETWEEN $2 AND $3 ORDER BY on_date",
    )
    .bind(org_id)
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;
    Ok(merge(suggested_between(from, to), stored))
}

fn merge(
    suggested: Vec<(NaiveDate, &'static str, &'static str)>,
    stored: Vec<HolidayView>,
) -> Vec<HolidayView> {
    let mut out: Vec<HolidayView> = suggested
        .into_iter()
        .map(|(on_date, en, ar)| HolidayView {
            on_date,
            name_en: en.into(),
            name_ar: ar.into(),
            decision: None,
        })
        .collect();
    for row in stored {
        match out.iter_mut().find(|h| h.on_date == row.on_date) {
            Some(h) => h.decision = row.decision,
            None => out.push(row),
        }
    }
    out.sort_by_key(|h| h.on_date);
    out
}

// ── Deciding ────────────────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct HolidayDecision {
    /// `holiday` (nobody marked absent; working it pays the multiplier) or
    /// `dismissed` (a normal day).
    pub decision: String,
}

#[utoipa::path(
    put, path = "/staff/holidays/{date}", tag = "staff", request_body = HolidayDecision,
    params(("date" = NaiveDate, Path)),
    responses(
        (status = 200, body = HolidayView),
        (status = 409, description = "PERIOD_CLOSED: the day is in an approved month"),
        AppErrorResponse,
    ),
    security(("bearer_jwt" = []))
)]
pub async fn decide_holiday(
    req: HttpRequest,
    pool: crate::db::Db,
    date: web::Path<NaiveDate>,
    body: web::Json<HolidayDecision>,
) -> Result<HttpResponse, AppError> {
    let claims = caller(&req)?;
    let org_id = crate::staff::scope_org(&req, &claims)?;
    let pool = pool.get_ref();
    let date = *date;
    // A public holiday is the business's, every branch at once (audit B-3).
    access::require_everywhere(pool, &claims, org_id, Cap::HrSchedulePublish).await?;
    if body.decision != "holiday" && body.decision != "dismissed" {
        return Err(AppError::BadRequest(
            "decision is holiday or dismissed".into(),
        ));
    }
    let known = holidays_in(pool, org_id, date, date)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::NotFound("No public holiday on that date.".into()))?;
    // The day's pay changes with the decision: an approved month is frozen.
    crate::staff::period_lock::assert_open(pool, org_id, date, "a public holiday").await?;

    let row: HolidayView = sqlx::query_as(
        "INSERT INTO staff_holidays (org_id, on_date, name_en, name_ar, decision, decided_by) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (org_id, on_date) DO UPDATE \
            SET decision = EXCLUDED.decision, decided_by = EXCLUDED.decided_by \
         RETURNING on_date, name_en, name_ar, decision",
    )
    .bind(org_id)
    .bind(date)
    .bind(&known.name_en)
    .bind(&known.name_ar)
    .bind(&body.decision)
    .bind(claims.user_id_safe().ok())
    .fetch_one(pool)
    .await?;

    if body.decision == "holiday" {
        forgive_absences(pool, org_id, date).await?;
    } else {
        restore_absences(pool, org_id, date).await?;
    }
    Ok(HttpResponse::Ok().json(row))
}

/// A day declared a holiday marks nobody absent (RU-10): the absences the
/// sweep wrote on it go, with the automatic deductions they carried. A day a
/// person set or punched is left alone (AT-7); a waived or overridden line is
/// a decision and stays (detached from the record).
async fn forgive_absences(pool: &PgPool, org_id: Uuid, date: NaiveDate) -> Result<(), AppError> {
    let mut tx = pool.begin().await?;
    let gone: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM attendance_records \
          WHERE org_id = $1 AND business_date = $2 AND status = 'absent' \
            AND check_in_at IS NULL AND NOT is_manual AND created_by IS NULL \
            AND NOT status_overridden AND covered_employee_id IS NULL \
          FOR UPDATE",
    )
    .bind(org_id)
    .bind(date)
    .fetch_all(&mut *tx)
    .await?;
    if !gone.is_empty() {
        sqlx::query(
            "DELETE FROM payroll_deductions \
              WHERE attendance_record_id = ANY($1) AND source <> 'manual' \
                AND created_by IS NULL AND waived_at IS NULL AND overridden_at IS NULL",
        )
        .bind(&gone)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM attendance_records WHERE id = ANY($1)")
            .bind(&gone)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// A past day dismissed as a normal day again: the rostered shifts nobody
/// clocked are absences, as the sweep would have marked them (a whole-day
/// leave or mission makes them `on_leave`). A day a manager deleted stays
/// deleted (the tombstone trigger).
async fn restore_absences(pool: &PgPool, org_id: Uuid, date: NaiveDate) -> Result<(), AppError> {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        r#"
        WITH people AS (
            SELECT id FROM employees
             WHERE org_id = $1 AND employment_status = 'active'
        ),
        missing AS (
            SELECT r.employee_id, r.branch_id, r.work_shift_id, r.on_date,
                   r.start_at, r.end_at,
                   EXISTS (
                       SELECT 1 FROM staff_requests sr
                        WHERE sr.employee_id = r.employee_id AND sr.status = 'approved'
                          AND sr.kind IN ('leave', 'mission')
                          AND sr.on_date <= r.on_date
                          AND COALESCE(sr.end_date, sr.on_date) >= r.on_date
                   ) AS excused
              FROM dawam_roster(ARRAY(SELECT id FROM people), $2, $2) r
             WHERE r.branch_id IS NOT NULL
               AND now() > r.end_at
               AND NOT EXISTS (
                   SELECT 1 FROM attendance_records a
                    WHERE a.employee_id = r.employee_id
                      AND a.business_date = r.on_date
                      AND a.covered_employee_id IS NULL
                      AND COALESCE(a.work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid)
                          = r.work_shift_id)
        )
        INSERT INTO attendance_records
            (org_id, employee_id, branch_id, work_shift_id, business_date, status,
             scheduled_start_at, scheduled_end_at, is_manual, edit_reason)
        SELECT $1, m.employee_id, m.branch_id, m.work_shift_id, m.on_date,
               CASE WHEN m.excused THEN 'on_leave' ELSE 'absent' END,
               m.start_at, m.end_at, FALSE, 'Marked automatically: no check-in'
          FROM missing m
        ON CONFLICT (employee_id, business_date,
                     COALESCE(work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid))
           WHERE covered_employee_id IS NULL
        DO NOTHING
        RETURNING id
        "#,
    )
    .bind(org_id)
    .bind(date)
    .fetch_all(pool)
    .await?;
    for id in ids {
        crate::staff::attendance::reprice_record(pool, org_id, id).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn the_arithmetic_calendar_matches_known_dates() {
        // 1 Shawwal 1447 and 10 Dhu al-Hijjah 1447 (expected 2026 dates).
        assert_eq!(hijri_to_gregorian(1447, 10, 1), Some(d(2026, 3, 20)));
        let adha = hijri_to_gregorian(1447, 12, 10).unwrap();
        assert!((adha - d(2026, 5, 27)).num_days().abs() <= 1, "{adha}");
        // 1 Muharram 1 = 16 July 622 (Julian) = 19 July 622 (proleptic Gregorian).
        assert_eq!(hijri_to_gregorian(1, 1, 1), Some(d(622, 7, 19)));
    }

    #[test]
    fn the_published_dates_win_where_known() {
        let y2026 = egypt_holidays(2026);
        for (m, day, name) in [(3, 20, "Eid al-Fitr"), (5, 27, "Eid al-Adha"), (6, 16, "Islamic New Year"), (8, 25, "Prophet's Birthday")] {
            assert!(y2026.iter().any(|(date, en, _)| *date == d(2026, m, day) && *en == name), "{name}");
        }
    }

    #[test]
    fn every_year_has_its_islamic_holidays() {
        for year in 2028..=2040 {
            let names: Vec<&str> = egypt_holidays(year).iter().map(|(_, en, _)| *en).collect();
            for (_, _, en, _) in ISLAMIC {
                assert!(names.contains(&en), "{year} has no {en}");
            }
        }
        // 2028's Eid al-Fitr is expected late February.
        let fitr = egypt_holidays(2028).into_iter().find(|(_, en, _)| *en == "Eid al-Fitr").unwrap().0;
        assert!(fitr >= d(2028, 2, 25) && fitr <= d(2028, 2, 28), "{fitr}");
    }

    #[test]
    fn a_holiday_can_fall_twice_in_one_year() {
        // The lunar year is 11 days short: Eid al-Fitr comes twice in 2033
        // (early January and late December).
        let fitr = egypt_holidays(2033).into_iter().filter(|(_, en, _)| *en == "Eid al-Fitr").count();
        assert_eq!(fitr, 2);
    }

    #[test]
    fn a_decision_overlays_its_suggestion_and_a_stray_decision_stays() {
        let merged = merge(
            vec![(d(2026, 5, 1), "Labour Day", "عيد العمال")],
            vec![
                HolidayView { on_date: d(2026, 5, 1), name_en: "Labour Day".into(), name_ar: "عيد العمال".into(), decision: Some("holiday".into()) },
                HolidayView { on_date: d(2026, 5, 2), name_en: "Old".into(), name_ar: "قديم".into(), decision: Some("dismissed".into()) },
            ],
        );
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].decision.as_deref(), Some("holiday"));
        assert_eq!(merged[1].name_en, "Old");
    }
}
