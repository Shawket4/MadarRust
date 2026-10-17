//! Per-(branch, business day) reference sequences: the `NNNN` in
//! `T-<CODE>-<YYMMDD>-NNNN`, `D-…` and the order counters
//! (DB_ARCHITECTURE_AUDIT_2026-10-16 §8 Phase 5.3).
//!
//! One table, `ref_counters (scope, branch_id, business_date)`, replaces the
//! three identical `order_ref_counters` / `ticket_ref_counters` /
//! `delivery_ref_counters`. For one release both are written: the old table's
//! row is bumped to `GREATEST(old, unified) + 1` — its row lock is what an
//! OLDER binary takes too, so during a rolling deploy the two serialize and can
//! never hand out the same number — and the unified row is set to the same
//! value. The parked contract (`migrations/parked/p5_ref_counters_contract.sql`)
//! drops the old half once no older binary runs.

use chrono::NaiveDate;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::errors::AppError;

/// Which sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Order,
    Ticket,
    Delivery,
}

/// The next number of `scope` at `branch_id` on `business_date` (1 for the
/// first), inside the caller's transaction.
pub async fn next(
    conn: &mut PgConnection,
    scope: Scope,
    branch_id: Uuid,
    business_date: NaiveDate,
) -> Result<i32, AppError> {
    // One static statement per scope (the old table's name differs). Each
    // bumps the old row (the shared lock), then mirrors the value.
    let seq = match scope {
        Scope::Order => {
            sqlx::query_scalar!(
                r#"WITH unified AS (
                       SELECT last_seq FROM ref_counters
                        WHERE scope = 'order' AND branch_id = $1 AND business_date = $2),
                   legacy AS (
                       INSERT INTO order_ref_counters (branch_id, business_date, last_seq)
                       VALUES ($1, $2, COALESCE((SELECT last_seq FROM unified), 0) + 1)
                       ON CONFLICT (branch_id, business_date) DO UPDATE
                          SET last_seq = GREATEST(order_ref_counters.last_seq,
                                                  COALESCE((SELECT last_seq FROM unified), 0)) + 1
                       RETURNING last_seq)
                   INSERT INTO ref_counters (scope, branch_id, business_date, last_seq)
                   SELECT 'order', $1, $2, last_seq FROM legacy
                   ON CONFLICT (scope, branch_id, business_date) DO UPDATE
                      SET last_seq = GREATEST(ref_counters.last_seq, EXCLUDED.last_seq)
                   RETURNING last_seq"#,
                branch_id,
                business_date
            )
            .fetch_one(&mut *conn)
            .await?
        }
        Scope::Ticket => {
            sqlx::query_scalar!(
                r#"WITH unified AS (
                       SELECT last_seq FROM ref_counters
                        WHERE scope = 'ticket' AND branch_id = $1 AND business_date = $2),
                   legacy AS (
                       INSERT INTO ticket_ref_counters (branch_id, business_date, last_seq)
                       VALUES ($1, $2, COALESCE((SELECT last_seq FROM unified), 0) + 1)
                       ON CONFLICT (branch_id, business_date) DO UPDATE
                          SET last_seq = GREATEST(ticket_ref_counters.last_seq,
                                                  COALESCE((SELECT last_seq FROM unified), 0)) + 1
                       RETURNING last_seq)
                   INSERT INTO ref_counters (scope, branch_id, business_date, last_seq)
                   SELECT 'ticket', $1, $2, last_seq FROM legacy
                   ON CONFLICT (scope, branch_id, business_date) DO UPDATE
                      SET last_seq = GREATEST(ref_counters.last_seq, EXCLUDED.last_seq)
                   RETURNING last_seq"#,
                branch_id,
                business_date
            )
            .fetch_one(&mut *conn)
            .await?
        }
        Scope::Delivery => {
            sqlx::query_scalar!(
                r#"WITH unified AS (
                       SELECT last_seq FROM ref_counters
                        WHERE scope = 'delivery' AND branch_id = $1 AND business_date = $2),
                   legacy AS (
                       INSERT INTO delivery_ref_counters (branch_id, business_date, last_seq)
                       VALUES ($1, $2, COALESCE((SELECT last_seq FROM unified), 0) + 1)
                       ON CONFLICT (branch_id, business_date) DO UPDATE
                          SET last_seq = GREATEST(delivery_ref_counters.last_seq,
                                                  COALESCE((SELECT last_seq FROM unified), 0)) + 1
                       RETURNING last_seq)
                   INSERT INTO ref_counters (scope, branch_id, business_date, last_seq)
                   SELECT 'delivery', $1, $2, last_seq FROM legacy
                   ON CONFLICT (scope, branch_id, business_date) DO UPDATE
                      SET last_seq = GREATEST(ref_counters.last_seq, EXCLUDED.last_seq)
                   RETURNING last_seq"#,
                branch_id,
                business_date
            )
            .fetch_one(&mut *conn)
            .await?
        }
    };
    Ok(seq)
}
