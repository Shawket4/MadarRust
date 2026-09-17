//! The effective timezone — ONE answer to "what wall-clock does this branch
//! read": the branch's zone, else its org's, else [`DEFAULT_TZ`].
//!
//! Every date shown, printed or bucketed for a branch is that instant read in
//! this zone. It travels on order, shift and ticket payloads as `timezone`, so a
//! client formats a payload in the zone the server meant rather than a cached
//! guess. SQL reads it through `effective_timezone(branch_id)` (same rule).

use chrono_tz::Tz;
use sqlx::PgExecutor;
use uuid::Uuid;

use crate::errors::AppError;

/// Product-home default and the DB column default.
pub const DEFAULT_TZ: &str = "Africa/Cairo";

/// The first day of a week, everywhere (owner rule, 2026-09-17): SATURDAY.
/// Every "this week"/"last week" window and every weekly bucket starts on it,
/// read on the scope's wall clock. The POS core (`timefmt::WEEK_START`) and the
/// dashboard (`lib/week.ts`) carry the same rule.
pub const WEEK_START: chrono::Weekday = chrono::Weekday::Sat;

/// Days Postgres's ISO week start (Monday) lies AFTER [`WEEK_START`]. The SQL
/// helpers shift by it; `week_sql_shift_matches_week_start` pins the two.
pub const WEEK_SQL_SHIFT_DAYS: i64 = 2;

/// The local date the week containing `day` starts on.
pub fn week_start(day: chrono::NaiveDate) -> chrono::NaiveDate {
    use chrono::Datelike;
    let back = (7 + day.weekday().num_days_from_monday() as i64
        - WEEK_START.num_days_from_monday() as i64)
        % 7;
    day - chrono::Duration::days(back)
}

/// SQL: the start of the [`WEEK_START`] week holding `local` — a wall-clock
/// `timestamp` (e.g. `x AT TIME ZONE $tz`) or a `date`; the result is a
/// `timestamp`. Postgres's `date_trunc('week', …)` cuts on Monday, so the value
/// is shifted forward, truncated, and shifted back. Use this, never a bare
/// `date_trunc('week', …)`.
pub fn week_start_sql(local: &str) -> String {
    format!(
        "(date_trunc('week', ({local}) + interval '{WEEK_SQL_SHIFT_DAYS} days') - interval '{WEEK_SQL_SHIFT_DAYS} days')"
    )
}

/// [`week_start_sql`] for `&'static str` SQL built with `concat!` (the
/// analytics schema). `$suffix` is appended, e.g. `"::date"`.
#[macro_export]
macro_rules! week_start_sql {
    ($local:expr, $suffix:expr) => {
        concat!(
            "(date_trunc('week', (",
            $local,
            ") + interval '2 days') - interval '2 days')",
            $suffix
        )
    };
}

/// Parse an IANA name, falling back to [`DEFAULT_TZ`] — loudly, because a
/// branch shown in the wrong zone is otherwise invisible.
pub fn parse(name: &str) -> Tz {
    name.parse().unwrap_or_else(|_| {
        tracing::warn!(
            timezone = name,
            "unknown timezone; falling back to {DEFAULT_TZ}"
        );
        chrono_tz::Africa::Cairo
    })
}

/// The branch's effective IANA name. `NotFound` for an unknown branch.
pub async fn effective_tz_name<'e, E: PgExecutor<'e>>(
    exec: E,
    branch_id: Uuid,
) -> Result<String, AppError> {
    sqlx::query_scalar::<_, Option<String>>("SELECT effective_timezone($1)")
        .bind(branch_id)
        .fetch_one(exec)
        .await?
        .ok_or_else(|| AppError::NotFound("Branch not found".into()))
}

/// [`effective_tz_name`], parsed.
pub async fn effective_tz<'e, E: PgExecutor<'e>>(exec: E, branch_id: Uuid) -> Result<Tz, AppError> {
    Ok(parse(&effective_tz_name(exec, branch_id).await?))
}

/// The zone a report scope reads in: the branch's effective zone, or — for an
/// "all branches" scope (no such branch) — the org's, else the default.
pub async fn scope_tz_name<'e, E: PgExecutor<'e>>(
    exec: E,
    branch_id: Uuid,
    org_id: Uuid,
) -> Result<String, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(effective_timezone($1),
                         (SELECT timezone::text FROM organizations WHERE id = $2),
                         $3)",
    )
    .bind(branch_id)
    .bind(org_id)
    .bind(DEFAULT_TZ)
    .fetch_one(exec)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn a_branch_zone_overrides_its_orgs(pool: sqlx::PgPool) {
        let org: Uuid = sqlx::query_scalar(
            "INSERT INTO organizations (name, slug, timezone) VALUES ('Tz', $1, 'Europe/London') RETURNING id",
        )
        .bind(format!("tz-{}", Uuid::new_v4()))
        .fetch_one(&pool)
        .await
        .unwrap();
        let branch = |tz: Option<&'static str>| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, Uuid>(
                    "INSERT INTO branches (org_id, name, timezone) VALUES ($1, $3, $2::timezone_name) RETURNING id",
                )
                .bind(org)
                .bind(tz)
                .bind(tz.unwrap_or("inherits"))
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        let own = branch(Some("Africa/Cairo")).await;
        let inherits = branch(None).await;
        assert_eq!(effective_tz_name(&pool, own).await.unwrap(), "Africa/Cairo");
        assert_eq!(
            effective_tz_name(&pool, inherits).await.unwrap(),
            "Europe/London"
        );
        assert!(effective_tz_name(&pool, Uuid::new_v4()).await.is_err());
        assert_eq!(
            scope_tz_name(&pool, Uuid::nil(), org).await.unwrap(),
            "Europe/London"
        );
    }

    #[test]
    fn week_sql_shift_matches_week_start() {
        use chrono::Weekday;
        assert_eq!(
            WEEK_SQL_SHIFT_DAYS,
            (7 - WEEK_START.num_days_from_monday() as i64) % 7
        );
        assert_eq!(week_start_sql("x"), crate::week_start_sql!("x", ""));
        assert_eq!(WEEK_START, Weekday::Sat);
        let d = |y, m, dd| chrono::NaiveDate::from_ymd_opt(y, m, dd).unwrap();
        // 2026-09-18 is a Friday, 09-19 a Saturday.
        assert_eq!(week_start(d(2026, 9, 18)), d(2026, 9, 12));
        assert_eq!(week_start(d(2026, 9, 19)), d(2026, 9, 19));
        assert_eq!(week_start(d(2026, 9, 25)), d(2026, 9, 19));
    }

    /// Friday 23:30 in Cairo closes a week; Saturday 00:30 opens the next —
    /// cut on the wall clock, not UTC (both are Friday in UTC).
    #[sqlx::test]
    async fn sql_weeks_start_saturday_on_the_local_clock(pool: sqlx::PgPool) {
        let sql = format!(
            "SELECT {}::date FROM (VALUES ($1::timestamptz)) v(ts)",
            week_start_sql("v.ts AT TIME ZONE 'Africa/Cairo'")
        );
        let week = |ts: &'static str| {
            let (pool, sql) = (pool.clone(), sql.clone());
            async move {
                sqlx::query_scalar::<_, chrono::NaiveDate>(&sql)
                    .bind(ts.parse::<chrono::DateTime<chrono::Utc>>().unwrap())
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        let d = |y, m, dd| chrono::NaiveDate::from_ymd_opt(y, m, dd).unwrap();
        // Cairo is UTC+3 in September 2026.
        assert_eq!(week("2026-09-18T20:30:00Z").await, d(2026, 9, 12));
        assert_eq!(week("2026-09-18T21:30:00Z").await, d(2026, 9, 19));
        // A plain date shifts the same way.
        let on_date: chrono::NaiveDate = sqlx::query_scalar(&format!(
            "SELECT {}::date",
            week_start_sql("DATE '2026-09-18'")
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(on_date, d(2026, 9, 12));
    }

    #[test]
    fn parses_or_falls_back_to_the_default() {
        assert_eq!(parse("Europe/Paris"), chrono_tz::Europe::Paris);
        assert_eq!(parse("Not/AZone"), chrono_tz::Africa::Cairo);
        assert_eq!(DEFAULT_TZ.parse::<Tz>().unwrap(), chrono_tz::Africa::Cairo);
    }
}
