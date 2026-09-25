//! ONE notion of "closed" for a pay period (Dawam AD-10, PAY-5, PAY-6).
//!
//! A period is closed once it is approved (`generated`), paid or closed: its
//! payslips are frozen snapshots. From then on nothing dated inside it moves
//! money any more — no new pay line, no waive, no override, no deletion, no
//! penalty recompute, no request decision that would re-price a day. Fixes go
//! into the next open month as new lines (AD-10). Only a reopen (before anyone
//! is paid) makes it a draft again.
//!
//! Every handler that touches money on a date asks here, so the rule lives in
//! one place and every path — including the ones other modules own — answers
//! the same way.

use chrono::NaiveDate;
use uuid::Uuid;

use crate::errors::AppError;

/// Statuses under which a period's figures are frozen.
pub const CLOSED: &str = "'generated', 'paid', 'closed'";

/// Is `day` inside an approved, paid or closed period of `org`?
pub async fn is_closed<'e, E>(conn: E, org_id: Uuid, day: NaiveDate) -> Result<bool, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    Ok(sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM payroll_periods \
          WHERE org_id = $1 AND status IN ({CLOSED}) AND start_date <= $2 AND end_date >= $2)"
    ))
    .bind(org_id)
    .bind(day)
    .fetch_one(conn)
    .await?)
}

/// The first day on or after `day` that is not inside an approved, paid or
/// closed period: where a new pay line lands by default (minor default M27),
/// so a line added after an early approval goes to next month's pay instead
/// of being refused.
pub async fn first_open_day(
    pool: &sqlx::PgPool,
    org_id: Uuid,
    day: NaiveDate,
) -> Result<NaiveDate, AppError> {
    let mut d = day;
    // Closed periods never overlap, so this walks at most a few of them.
    for _ in 0..24 {
        let end: Option<NaiveDate> = sqlx::query_scalar(&format!(
            "SELECT end_date FROM payroll_periods \
              WHERE org_id = $1 AND status IN ({CLOSED}) AND start_date <= $2 AND end_date >= $2 \
              ORDER BY end_date DESC LIMIT 1"
        ))
        .bind(org_id)
        .bind(d)
        .fetch_optional(pool)
        .await?;
        match end {
            Some(e) => d = e + chrono::Duration::days(1),
            None => break,
        }
    }
    Ok(d)
}

/// Does any approved, paid or closed period overlap `[from, to]`?
pub async fn any_closed_in<'e, E>(
    conn: E,
    org_id: Uuid,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<bool, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    Ok(sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM payroll_periods \
          WHERE org_id = $1 AND status IN ({CLOSED}) AND start_date <= $3 AND end_date >= $2)"
    ))
    .bind(org_id)
    .bind(from)
    .bind(to)
    .fetch_one(conn)
    .await?)
}

/// 409 when `day` falls in a closed period. `what` names the act for the
/// message ("a pay line", "this waiver").
pub async fn assert_open<'e, E>(
    conn: E,
    org_id: Uuid,
    day: NaiveDate,
    what: &str,
) -> Result<(), AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    closed_as(conn, org_id, day, day, what).await
}

/// 409 when any day of `[from, to]` falls in a closed period.
pub async fn assert_range_open<'e, E>(
    conn: E,
    org_id: Uuid,
    from: NaiveDate,
    to: NaiveDate,
    what: &str,
) -> Result<(), AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    closed_as(conn, org_id, from, to, what).await
}

/// 409 with the machine code `PERIOD_CLOSED` when a closed period overlaps
/// `[from, to]`, so every client branches on one code whichever handler
/// refused (the rules module's month guard delegates here too). `vars.paid`
/// says the month is PAID — it can never be reopened, so a client offers only
/// "pick a day in an open month", not "reopen it first".
async fn closed_as<'e, E>(
    conn: E,
    org_id: Uuid,
    from: NaiveDate,
    to: NaiveDate,
    what: &str,
) -> Result<(), AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    // NULL = nothing closed overlaps; else whether any overlapping one is paid.
    let paid: Option<bool> = sqlx::query_scalar(&format!(
        "SELECT bool_or(status IN ('paid', 'closed')) FROM payroll_periods \
          WHERE org_id = $1 AND status IN ({CLOSED}) AND start_date <= $3 AND end_date >= $2"
    ))
    .bind(org_id)
    .bind(from)
    .bind(to)
    .fetch_one(conn)
    .await?;
    let Some(paid) = paid else { return Ok(()) };
    Err(AppError::CodedVars {
        status: 409,
        code: "PERIOD_CLOSED",
        reason: if paid {
            format!(
                "That month is paid — {what} dated {from} can't change it. \
                 Add it to the next open month instead."
            )
        } else {
            format!(
                "That month's payroll is approved — {what} dated {from} can't change it. \
                 Reopen it first, or add it to the next open month."
            )
        },
        vars: serde_json::json!({ "date": from, "paid": paid }),
    })
}
