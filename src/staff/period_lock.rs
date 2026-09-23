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
    if is_closed(conn, org_id, day).await? {
        return Err(closed(what, day));
    }
    Ok(())
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
    if any_closed_in(conn, org_id, from, to).await? {
        return Err(closed(what, from));
    }
    Ok(())
}

/// 409 with the machine code `PERIOD_CLOSED`, so every client branches on
/// one code whichever handler refused (the rules module's month guard
/// delegates here too).
fn closed(what: &str, day: NaiveDate) -> AppError {
    AppError::Coded {
        status: 409,
        code: "PERIOD_CLOSED",
        reason: format!(
            "PERIOD_CLOSED: that month's payroll is approved — {what} dated {day} can't change it. \
             Add it to the next open month instead."
        ),
    }
}
