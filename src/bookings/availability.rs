//! Availability: which slots a party can book, and which tables best fit it.
//!
//! Pure functions over loaded rows (unit-tested without a DB) plus thin loaders.
//! Capacity is seats-based: a slot is bookable when a single table, or two
//! tables in one section, with enough seats are free for the whole window.
//! Nothing is stored — every answer is computed from `booking_tables` claims
//! (the exclusion constraint makes those the truth) and the live floor.

use std::collections::HashSet;

use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use serde::Serialize;
use sqlx::PgExecutor;
use utoipa::ToSchema;
use uuid::Uuid;

use super::settings::BookingSettings;
use crate::errors::AppError;

#[derive(Debug, Clone)]
pub struct TableCap {
    pub id: Uuid,
    pub label: String,
    pub seats: i32,
    pub section_id: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub struct Claim {
    pub table_id: Uuid,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SlotAvailability {
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub available: bool,
    /// The tables the auto-assigner would pick (host view only; empty when
    /// unavailable).
    pub table_ids: Vec<Uuid>,
}

/// Every slot start on `date` (branch-local) inside the day's window. The last
/// start leaves room for the default duration before close (a 23:00 close with
/// a 90-minute duration offers 21:30 last). Empty on a blackout / closed day.
pub fn slot_starts(settings: &BookingSettings, tz: Tz, date: NaiveDate) -> Vec<DateTime<Utc>> {
    if settings.is_blackout(date) {
        return Vec::new();
    }
    let dow = chrono::Datelike::weekday(&date).num_days_from_sunday() as u8;
    let Some((open, close)) = settings.window_for(dow) else {
        return Vec::new();
    };
    let Some(open_dt) = tz.from_local_datetime(&date.and_time(open)).single() else {
        return Vec::new();
    };
    let close_date = if close <= open {
        date + Duration::days(1)
    } else {
        date
    };
    let Some(close_dt) = tz.from_local_datetime(&close_date.and_time(close)).single() else {
        return Vec::new();
    };
    let last_start = close_dt - Duration::minutes(settings.default_duration_minutes as i64);
    let step = Duration::minutes(settings.slot_minutes as i64);
    let mut out = Vec::new();
    let mut t = open_dt;
    while t <= last_start {
        out.push(t.with_timezone(&Utc));
        t += step;
    }
    out
}

/// Tables with no active claim overlapping `[start, end)` and not physically
/// blocked right now (a seated/dirty table cannot be promised to a party due
/// within the next few minutes).
pub fn free_tables<'a>(
    tables: &'a [TableCap],
    claims: &[Claim],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    blocked_now: &HashSet<Uuid>,
) -> Vec<&'a TableCap> {
    tables
        .iter()
        .filter(|t| !blocked_now.contains(&t.id))
        .filter(|t| {
            !claims
                .iter()
                .any(|c| c.table_id == t.id && c.starts_at < end && c.ends_at > start)
        })
        .collect()
}

/// Best-fit assignment: the smallest single table that seats the party (a
/// preferred section wins ties, then fewer wasted seats), else the pair of
/// tables in one section with the least combined waste. `None` when nothing
/// fits — the caller decides whether a host may force the booking anyway.
pub fn best_fit(free: &[&TableCap], party: i32, section_pref: Option<Uuid>) -> Option<Vec<Uuid>> {
    let mut singles: Vec<&&TableCap> = free.iter().filter(|t| t.seats >= party).collect();
    singles.sort_by_key(|t| {
        let pref_miss = section_pref.is_some_and(|s| t.section_id != Some(s));
        (pref_miss, t.seats - party, t.label.clone())
    });
    if let Some(t) = singles.first() {
        return Some(vec![t.id]);
    }
    // Pairs within one section (a merged table only makes sense side by side).
    let mut best: Option<(bool, i32, [Uuid; 2])> = None;
    for (i, a) in free.iter().enumerate() {
        for b in free.iter().skip(i + 1) {
            if a.section_id != b.section_id {
                continue;
            }
            let seats = a.seats + b.seats;
            if seats < party {
                continue;
            }
            let pref_miss = section_pref.is_some_and(|s| a.section_id != Some(s));
            let key = (pref_miss, seats - party, [a.id, b.id]);
            if best.as_ref().is_none_or(|k| (key.0, key.1) < (k.0, k.1)) {
                best = Some(key);
            }
        }
    }
    best.map(|(_, _, ids)| ids.to_vec())
}

pub async fn load_tables<'e, E>(exec: E, branch_id: Uuid) -> Result<Vec<TableCap>, AppError>
where
    E: PgExecutor<'e>,
{
    let rows: Vec<(Uuid, String, i16, Option<Uuid>)> = sqlx::query_as(
        "SELECT id, label, seats, section_id FROM branch_tables \
         WHERE branch_id = $1 AND is_active ORDER BY lower(label)",
    )
    .bind(branch_id)
    .fetch_all(exec)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, label, seats, section_id)| TableCap {
            id,
            label,
            seats: seats as i32,
            section_id,
        })
        .collect())
}

/// Active claims on the branch overlapping `[from, to)`, minus one booking's own
/// (so a move can re-check against everyone else).
pub async fn load_claims<'e, E>(
    exec: E,
    branch_id: Uuid,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    exclude_booking: Option<Uuid>,
) -> Result<Vec<Claim>, AppError>
where
    E: PgExecutor<'e>,
{
    let rows: Vec<(Uuid, DateTime<Utc>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT bt.table_id, lower(bt.during), upper(bt.during) \
         FROM booking_tables bt JOIN bookings b ON b.id = bt.booking_id \
         WHERE b.branch_id = $1 AND bt.active \
           AND bt.during && tstzrange($2, $3, '[)') \
           AND ($4::uuid IS NULL OR bt.booking_id <> $4)",
    )
    .bind(branch_id)
    .bind(from)
    .bind(to)
    .bind(exclude_booking)
    .fetch_all(exec)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(table_id, starts_at, ends_at)| Claim {
            table_id,
            starts_at,
            ends_at,
        })
        .collect())
}

/// Tables that are physically taken right now (seated or waiting to be bussed).
pub async fn occupied_now<'e, E>(exec: E, branch_id: Uuid) -> Result<HashSet<Uuid>, AppError>
where
    E: PgExecutor<'e>,
{
    let rows: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM branch_tables WHERE branch_id = $1 AND status IN ('seated', 'dirty')",
    )
    .bind(branch_id)
    .fetch_all(exec)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Guests already booked to START at exactly `starts_at` (for the covers cap).
pub async fn covers_at<'e, E>(
    exec: E,
    branch_id: Uuid,
    starts_at: DateTime<Utc>,
    exclude_booking: Option<Uuid>,
) -> Result<i64, AppError>
where
    E: PgExecutor<'e>,
{
    let n: Option<i64> = sqlx::query_scalar(
        "SELECT SUM(party_size)::bigint FROM bookings \
         WHERE branch_id = $1 AND starts_at = $2 AND status IN ('confirmed', 'seated') \
           AND ($3::uuid IS NULL OR id <> $3)",
    )
    .bind(branch_id)
    .bind(starts_at)
    .bind(exclude_booking)
    .fetch_one(exec)
    .await?;
    Ok(n.unwrap_or(0))
}

/// Everything one availability answer needs, loaded once per request.
pub struct Ground {
    pub tables: Vec<TableCap>,
    pub claims: Vec<Claim>,
    pub occupied: HashSet<Uuid>,
}

impl Ground {
    pub async fn load<'e, E>(
        exec: E,
        branch_id: Uuid,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        exclude_booking: Option<Uuid>,
    ) -> Result<Self, AppError>
    where
        E: PgExecutor<'e> + Copy,
    {
        Ok(Self {
            tables: load_tables(exec, branch_id).await?,
            claims: load_claims(exec, branch_id, from, to, exclude_booking).await?,
            occupied: occupied_now(exec, branch_id).await?,
        })
    }

    /// The auto-assigner's pick for one window, or `None` when nothing fits.
    /// Currently-occupied tables are excluded only for windows starting within
    /// the branch's hold horizon (a table seated now will be free in two hours).
    pub fn pick(
        &self,
        settings: &BookingSettings,
        now: DateTime<Utc>,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        party: i32,
        section_pref: Option<Uuid>,
    ) -> Option<Vec<Uuid>> {
        let horizon = Duration::minutes((settings.hold_minutes as i64).max(15));
        let blocked = if start <= now + horizon {
            self.occupied.clone()
        } else {
            HashSet::new()
        };
        let free = free_tables(&self.tables, &self.claims, start, end, &blocked);
        best_fit(&free, party, section_pref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(label: &str, seats: i32, section: Option<Uuid>) -> TableCap {
        TableCap {
            id: Uuid::new_v4(),
            label: label.into(),
            seats,
            section_id: section,
        }
    }

    #[test]
    fn best_fit_prefers_smallest_single_table() {
        let s = Uuid::new_v4();
        let tables = [
            t("T1", 2, Some(s)),
            t("T2", 4, Some(s)),
            t("T3", 6, Some(s)),
        ];
        let free: Vec<&TableCap> = tables.iter().collect();
        assert_eq!(best_fit(&free, 3, None), Some(vec![tables[1].id]));
        assert_eq!(best_fit(&free, 6, None), Some(vec![tables[2].id]));
    }

    #[test]
    fn best_fit_pairs_tables_in_one_section_when_no_single_fits() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let tables = [
            t("A1", 4, Some(a)),
            t("A2", 4, Some(a)),
            t("B1", 4, Some(b)),
        ];
        let free: Vec<&TableCap> = tables.iter().collect();
        let pick = best_fit(&free, 7, None).unwrap();
        assert_eq!(pick.len(), 2);
        assert!(pick.contains(&tables[0].id) && pick.contains(&tables[1].id));
        assert_eq!(best_fit(&free, 9, None), None, "no pair seats nine");
    }

    #[test]
    fn best_fit_honours_section_preference_before_waste() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let tables = [t("A1", 6, Some(a)), t("B1", 4, Some(b))];
        let free: Vec<&TableCap> = tables.iter().collect();
        assert_eq!(best_fit(&free, 4, Some(a)), Some(vec![tables[0].id]));
        assert_eq!(best_fit(&free, 4, None), Some(vec![tables[1].id]));
    }

    #[test]
    fn free_tables_drops_overlapping_claims_and_blocked() {
        let tables = [t("T1", 4, None), t("T2", 4, None), t("T3", 4, None)];
        let base = Utc.with_ymd_and_hms(2026, 9, 10, 18, 0, 0).unwrap();
        let claims = vec![
            Claim {
                table_id: tables[0].id,
                starts_at: base,
                ends_at: base + Duration::minutes(90),
            },
            // Ends exactly when our window starts: no overlap.
            Claim {
                table_id: tables[1].id,
                starts_at: base - Duration::minutes(90),
                ends_at: base + Duration::minutes(30),
            },
        ];
        let blocked: HashSet<Uuid> = [tables[2].id].into_iter().collect();
        let free = free_tables(
            &tables,
            &claims,
            base + Duration::minutes(30),
            base + Duration::minutes(120),
            &blocked,
        );
        assert_eq!(free.len(), 1);
        assert_eq!(free[0].id, tables[1].id);
    }

    #[test]
    fn slot_starts_follow_the_window_and_duration() {
        let mut s = BookingSettings::defaults(Uuid::new_v4());
        s.slot_minutes = 30;
        s.default_duration_minutes = 90;
        s.hours = vec![HoursEntry {
            dow: 4, // Thursday
            open: "18:00".into(),
            close: "21:00".into(),
        }];
        let tz: Tz = "Africa/Cairo".parse().unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(); // a Thursday
        let slots = slot_starts(&s, tz, date);
        // 18:00, 18:30, 19:00, 19:30 (last start = 21:00 - 90m).
        assert_eq!(slots.len(), 4);
        assert_eq!(
            slots[0],
            tz.with_ymd_and_hms(2026, 9, 10, 18, 0, 0)
                .unwrap()
                .with_timezone(&Utc)
        );
        assert!(
            slot_starts(&s, tz, date + Duration::days(1)).is_empty(),
            "friday closed"
        );
        s.blackout_dates = vec!["2026-09-10".into()];
        assert!(slot_starts(&s, tz, date).is_empty(), "blackout");
    }

    #[test]
    fn slot_starts_handle_a_close_after_midnight() {
        let mut s = BookingSettings::defaults(Uuid::new_v4());
        s.slot_minutes = 60;
        s.default_duration_minutes = 60;
        s.hours = vec![HoursEntry {
            dow: 5,
            open: "22:00".into(),
            close: "02:00".into(),
        }];
        let tz: Tz = "Africa/Cairo".parse().unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 9, 11).unwrap(); // a Friday
        let slots = slot_starts(&s, tz, date);
        assert_eq!(slots.len(), 4, "22, 23, 00, 01");
    }

    use super::super::settings::HoursEntry;
}
