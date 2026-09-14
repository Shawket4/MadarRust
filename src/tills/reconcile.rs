//! Close-till reconciliation, one line per payment method used on a till.
//!
//! Contract: TILLS_CONTRACT.md §1.2 `till_reconciliations`, §2.1
//! `TillReconciliationLine`, §2.2 close rules, §3 R6, §9 (B3 → B2 boundary).
//!
//! Closing is NEVER blocked by reconciliation. Live closes validate the input
//! (a disagreement needs an amount and a note); replayed closes never fail on
//! it (a missing note is stored as `(no note)`, a missing amount as the system
//! total) so an offline till's queued close can never dead-letter.
//!
//! Every statement is a runtime `sqlx::query*` (not a macro) against the
//! post-rework names (`tills`, `orders.till_id`, `order_refunds.till_id`,
//! `till_reconciliations`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::errors::AppError;

/// What the system says one method took on a till.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, ToSchema)]
pub struct MethodTotal {
    pub method: String,
    pub payment_method_id: Option<Uuid>,
    pub is_cash: bool,
    pub system_total: i64,
    pub order_count: i64,
}

/// What the teller said about one method at close.
#[derive(Deserialize, Serialize, Clone, Debug, ToSchema)]
pub struct ReconciliationInput {
    pub method: String,
    /// `checked` | `disagreed`
    pub status: String,
    #[serde(default)]
    pub declared_amount: Option<i32>,
    #[serde(default)]
    pub note: Option<String>,
}

/// One stored reconciliation line (§2.1).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, sqlx::FromRow, ToSchema)]
pub struct TillReconciliationLine {
    pub method: String,
    pub payment_method_id: Option<Uuid>,
    pub is_cash: bool,
    pub system_total: i32,
    pub current_system_total: i32,
    pub order_count: i32,
    /// `checked` | `disagreed` | `unreviewed`
    pub status: String,
    pub declared_amount: Option<i32>,
    pub note: Option<String>,
    pub reconciled_by: Option<Uuid>,
    pub reconciled_at: DateTime<Utc>,
    /// `current_system_total <> system_total` — a late replay moved the total.
    pub changed_after_close: bool,
}

pub const STATUS_CLEAN: &str = "clean";
pub const STATUS_DISAGREED: &str = "disagreed";
pub const STATUS_UNREVIEWED: &str = "unreviewed";
pub const REPLAY_MISSING_NOTE: &str = "(no note)";
pub const CODE_NOTE_REQUIRED: &str = "RECONCILIATION_NOTE_REQUIRED";
pub const CODE_AMOUNT_REQUIRED: &str = "RECONCILIATION_AMOUNT_REQUIRED";

const CASH_FALLBACK_NAME: &str = "cash";

#[derive(sqlx::FromRow)]
struct UsedRow {
    method: String,
    is_cash: bool,
    total: i64,
    order_count: i64,
    payment_method_id: Option<Uuid>,
}

/// Every method used on the till, with the system's total for it.
///
/// Non-cash `system_total` is what that method's terminal saw for this till:
/// tendered legs (sales not voided, as the drawer counts them) + tips carried
/// on that method − refunds issued FROM this till on that method.
/// `order_count` = distinct orders with a leg on the method. A method appears
/// if it has a leg, a tip or a refund on the till. With no tips and no refunds
/// the non-cash totals equal the till report's `payment_summary`.
///
/// Cash: ONE row, always present and first, `system_total = expected_cash`
/// (the drawer), named after the cash-flagged method used on the till (else the
/// org's first cash method, else `cash`). Other cash-flagged names fold into it.
pub async fn system_totals_by_method(
    conn: &mut sqlx::PgConnection,
    till_id: Uuid,
    expected_cash: i64,
) -> Result<Vec<MethodTotal>, AppError> {
    let sql = format!(
        r#"
        WITH t AS (
            SELECT tl.id, b.org_id FROM tills tl JOIN branches b ON b.id = tl.branch_id WHERE tl.id = $1
        ),
        used AS (
            SELECT op.method::text AS method,
                   COALESCE(op.is_cash, op.method = 'cash') AS is_cash,
                   op.amount::bigint AS amount, op.order_id
            FROM order_payments op JOIN orders o ON o.id = op.order_id
            WHERE o.till_id = $1 AND o.{tendered}
          UNION ALL
            SELECT COALESCE(o.tip_payment_method, o.payment_method)::text,
                   COALESCE(o.tip_is_cash, COALESCE(o.tip_payment_method, o.payment_method) = 'cash'),
                   o.tip_amount::bigint, NULL::uuid
            FROM orders o
            WHERE o.till_id = $1 AND o.{tendered} AND COALESCE(o.tip_amount, 0) <> 0
          UNION ALL
            SELECT r.method, r.is_cash, -r.amount::bigint, NULL::uuid
            FROM order_refunds r WHERE r.till_id = $1
        )
        SELECT u.method,
               bool_or(u.is_cash) AS is_cash,
               COALESCE(SUM(u.amount), 0)::bigint AS total,
               COUNT(DISTINCT u.order_id)::bigint AS order_count,
               (SELECT m.id FROM org_payment_methods m, t WHERE m.org_id = t.org_id AND m.name = u.method) AS payment_method_id
        FROM used u
        WHERE u.method IS NOT NULL AND btrim(u.method) <> ''
        GROUP BY u.method
        ORDER BY u.method
        "#,
        tendered = crate::orders::TENDERED
    );
    let rows = sqlx::query_as::<_, UsedRow>(&sql)
        .bind(till_id)
        .fetch_all(&mut *conn)
        .await?;

    let mut cash_name: Option<(String, Option<Uuid>)> = None;
    let mut cash_orders = 0i64;
    let mut out = Vec::with_capacity(rows.len() + 1);
    for r in rows {
        if r.is_cash {
            cash_orders += r.order_count;
            // Prefer the literal `cash`, else the first cash-flagged name.
            if cash_name.is_none() || r.method == CASH_FALLBACK_NAME {
                cash_name = Some((r.method, r.payment_method_id));
            }
        } else {
            out.push(MethodTotal {
                method: r.method,
                payment_method_id: r.payment_method_id,
                is_cash: false,
                system_total: r.total,
                order_count: r.order_count,
            });
        }
    }
    let (method, payment_method_id) = match cash_name {
        Some(c) => c,
        None => {
            let org_cash: Option<(String, Uuid)> = sqlx::query_as(
                "SELECT m.name, m.id FROM org_payment_methods m
                 JOIN branches b ON b.org_id = m.org_id JOIN tills tl ON tl.branch_id = b.id
                 WHERE tl.id = $1 AND m.is_cash
                 ORDER BY (m.name = 'cash') DESC, m.is_active DESC, m.created_at LIMIT 1",
            )
            .bind(till_id)
            .fetch_optional(&mut *conn)
            .await?;
            match org_cash {
                Some((n, id)) => (n, Some(id)),
                None => (CASH_FALLBACK_NAME.to_string(), None),
            }
        }
    };
    out.insert(
        0,
        MethodTotal { method, payment_method_id, is_cash: true, system_total: expected_cash, order_count: cash_orders },
    );
    Ok(out)
}

fn clamp_i32(v: i64) -> i32 {
    v.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

fn blank_to_none(s: Option<&str>) -> Option<String> {
    s.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Rollup: any disagreed → `disagreed`, else any unreviewed → `unreviewed`, else `clean`.
pub fn rollup_status<'a>(statuses: impl IntoIterator<Item = &'a str>) -> &'static str {
    let mut unreviewed = false;
    for s in statuses {
        match s {
            "disagreed" => return STATUS_DISAGREED,
            "unreviewed" => unreviewed = true,
            _ => {}
        }
    }
    if unreviewed { STATUS_UNREVIEWED } else { STATUS_CLEAN }
}

/// A line to insert, before it has a timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedLine {
    pub method: String,
    pub payment_method_id: Option<Uuid>,
    pub is_cash: bool,
    pub system_total: i32,
    pub order_count: i32,
    pub status: &'static str,
    pub declared_amount: Option<i32>,
    pub note: Option<String>,
}

/// Pure planning step (no DB): validate inputs against the used methods.
/// Live: a `disagreed` line without an amount → 400 `RECONCILIATION_AMOUNT_REQUIRED`,
/// without a non-blank note (non-cash) → 400 `RECONCILIATION_NOTE_REQUIRED`, an
/// unknown status → 400. Replay never fails: missing amount → system total,
/// missing note → `(no note)`, unknown status → `unreviewed`.
pub fn plan_lines(
    totals: &[MethodTotal],
    closing_cash_declared: i32,
    closing_cash_system: i32,
    cash_note: Option<&str>,
    inputs: &[ReconciliationInput],
    replay: bool,
) -> Result<Vec<PlannedLine>, AppError> {
    let mut lines = Vec::with_capacity(totals.len() + 1);
    let cash = totals.iter().find(|t| t.is_cash);
    lines.push(PlannedLine {
        method: cash.map(|c| c.method.clone()).unwrap_or_else(|| CASH_FALLBACK_NAME.into()),
        payment_method_id: cash.and_then(|c| c.payment_method_id),
        is_cash: true,
        system_total: closing_cash_system,
        order_count: cash.map(|c| clamp_i32(c.order_count)).unwrap_or(0),
        status: if closing_cash_declared == closing_cash_system { "checked" } else { "disagreed" },
        declared_amount: Some(closing_cash_declared),
        note: blank_to_none(cash_note),
    });
    let cash_method = lines[0].method.clone();

    let find_input = |method: &str| inputs.iter().find(|i| i.method.trim() == method);
    let plan_one = |method: &str, pmid: Option<Uuid>, total: i64, count: i64| -> Result<PlannedLine, AppError> {
        let system_total = clamp_i32(total);
        let base = PlannedLine {
            method: method.to_string(),
            payment_method_id: pmid,
            is_cash: false,
            system_total,
            order_count: clamp_i32(count),
            status: "unreviewed",
            declared_amount: None,
            note: None,
        };
        let Some(input) = find_input(method) else { return Ok(base) };
        match input.status.trim() {
            "checked" => Ok(PlannedLine { status: "checked", note: blank_to_none(input.note.as_deref()), ..base }),
            "disagreed" => {
                let amount = match input.declared_amount {
                    Some(a) => a,
                    None if replay => system_total,
                    None => {
                        return Err(AppError::Coded {
                            status: 400,
                            code: CODE_AMOUNT_REQUIRED,
                            reason: format!("{CODE_AMOUNT_REQUIRED}: enter the amount you see for {method}"),
                        });
                    }
                };
                let note = match blank_to_none(input.note.as_deref()) {
                    Some(n) => n,
                    None if replay => REPLAY_MISSING_NOTE.to_string(),
                    None => {
                        return Err(AppError::Coded {
                            status: 400,
                            code: CODE_NOTE_REQUIRED,
                            reason: format!("{CODE_NOTE_REQUIRED}: add a note for the difference on {method}"),
                        });
                    }
                };
                Ok(PlannedLine { status: "disagreed", declared_amount: Some(amount), note: Some(note), ..base })
            }
            _ if replay => Ok(base),
            other => Err(AppError::BadRequest(format!(
                "Invalid reconciliation status '{other}' for {method}"
            ))),
        }
    };

    for t in totals.iter().filter(|t| !t.is_cash) {
        lines.push(plan_one(&t.method, t.payment_method_id, t.system_total, t.order_count)?);
    }
    // Inputs naming a method not used on the till: stored with system total 0.
    let mut extra: Vec<&ReconciliationInput> = Vec::new();
    for i in inputs {
        let m = i.method.trim();
        if m.is_empty() || m == cash_method || lines.iter().any(|l| l.method == m) || extra.iter().any(|e| e.method.trim() == m) {
            continue;
        }
        extra.push(i);
    }
    for i in extra {
        lines.push(plan_one(i.method.trim(), None, 0, 0)?);
    }
    Ok(lines)
}

const LINE_COLUMNS: &str = "method, payment_method_id, is_cash, system_total, current_system_total, order_count,
    status, declared_amount, note, reconciled_by, reconciled_at,
    (current_system_total <> system_total) AS changed_after_close";

/// The stored lines of several tills on one connection, keyed by till (sync pull).
pub(crate) async fn stored_lines_by_till(
    conn: &mut sqlx::PgConnection,
    till_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, Vec<TillReconciliationLine>>, AppError> {
    let rows: Vec<(Uuid, sqlx::types::Json<TillReconciliationLine>)> = sqlx::query_as(&format!(
        "SELECT till_id, row_to_json(x) FROM (SELECT till_id, {LINE_COLUMNS} FROM till_reconciliations \
          WHERE till_id = ANY($1) ORDER BY till_id, is_cash DESC, method) x"
    ))
    .bind(till_ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut out: std::collections::HashMap<Uuid, Vec<TillReconciliationLine>> = std::collections::HashMap::new();
    for (till, line) in rows {
        out.entry(till).or_default().push(line.0);
    }
    Ok(out)
}

async fn stored_lines(conn: &mut sqlx::PgConnection, till_id: Uuid) -> Result<Vec<TillReconciliationLine>, AppError> {
    Ok(sqlx::query_as::<_, TillReconciliationLine>(&format!(
        "SELECT {LINE_COLUMNS} FROM till_reconciliations WHERE till_id = $1 ORDER BY is_cash DESC, method"
    ))
    .bind(till_id)
    .fetch_all(&mut *conn)
    .await?)
}

/// Write the reconciliation rows for a close and set `tills.reconciliation_status`.
/// Returns the stored lines and the rollup status (`clean|disagreed|unreviewed`).
///
/// PRECONDITION: the caller holds the till advisory lock and has ALREADY set
/// `tills.status` to `closed`/`force_closed` in the same transaction (the
/// `tills_reconciled_when_closed` CHECK). All live validation happens before
/// any write, so a 400 leaves nothing behind.
///
/// Idempotent: if lines already exist for the till nothing is rewritten and the
/// stored state is returned. Force-close: pass `inputs = &[]`, `replay = true`
/// → non-cash lines are `unreviewed`.
#[allow(clippy::too_many_arguments)]
pub async fn write_close_reconciliation(
    conn: &mut sqlx::PgConnection,
    till_id: Uuid,
    actor: Uuid,
    closing_cash_declared: i32,
    closing_cash_system: i32,
    cash_note: Option<&str>,
    inputs: &[ReconciliationInput],
    replay: bool,
) -> Result<(Vec<TillReconciliationLine>, &'static str), AppError> {
    let existing = stored_lines(conn, till_id).await?;
    if !existing.is_empty() {
        let status = rollup_status(existing.iter().map(|l| l.status.as_str()));
        return Ok((existing, status));
    }
    let totals = system_totals_by_method(conn, till_id, closing_cash_system as i64).await?;
    let planned = plan_lines(&totals, closing_cash_declared, closing_cash_system, cash_note, inputs, replay)?;
    insert_planned(conn, till_id, actor, &planned).await
}

/// The reconciliation a FORCE-close writes: nobody counted anything, so every
/// line — the cash row included — is `unreviewed` with no declared amount
/// (contract §8: `unreviewed` exists for legacy closes and force-closes). The
/// system totals are still snapshotted so a later review has the figures.
/// Same precondition and idempotency as [`write_close_reconciliation`].
pub async fn write_force_close_reconciliation(
    conn: &mut sqlx::PgConnection,
    till_id: Uuid,
    actor: Uuid,
    closing_cash_system: i32,
) -> Result<(Vec<TillReconciliationLine>, &'static str), AppError> {
    let existing = stored_lines(conn, till_id).await?;
    if !existing.is_empty() {
        let status = rollup_status(existing.iter().map(|l| l.status.as_str()));
        return Ok((existing, status));
    }
    let totals = system_totals_by_method(conn, till_id, closing_cash_system as i64).await?;
    let planned = force_close_lines(plan_lines(&totals, closing_cash_system, closing_cash_system, None, &[], true)?);
    insert_planned(conn, till_id, actor, &planned).await
}

/// Force-close planning: no line was counted or checked by anyone.
pub fn force_close_lines(planned: Vec<PlannedLine>) -> Vec<PlannedLine> {
    planned
        .into_iter()
        .map(|l| PlannedLine { status: STATUS_UNREVIEWED, declared_amount: None, note: None, ..l })
        .collect()
}

async fn insert_planned(
    conn: &mut sqlx::PgConnection,
    till_id: Uuid,
    actor: Uuid,
    planned: &[PlannedLine],
) -> Result<(Vec<TillReconciliationLine>, &'static str), AppError> {
    for l in planned {
        sqlx::query(
            "INSERT INTO till_reconciliations
               (till_id, method, payment_method_id, is_cash, system_total, current_system_total,
                order_count, status, declared_amount, note, reconciled_by)
             VALUES ($1, $2, $3, $4, $5, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (till_id, method) DO NOTHING",
        )
        .bind(till_id)
        .bind(&l.method)
        .bind(l.payment_method_id)
        .bind(l.is_cash)
        .bind(l.system_total)
        .bind(l.order_count)
        .bind(l.status)
        .bind(l.declared_amount)
        .bind(&l.note)
        .bind(actor)
        .execute(&mut *conn)
        .await?;
    }
    let lines = stored_lines(conn, till_id).await?;
    let status = rollup_status(lines.iter().map(|l| l.status.as_str()));
    sqlx::query("UPDATE tills SET reconciliation_status = $2 WHERE id = $1")
        .bind(till_id)
        .bind(status)
        .execute(&mut *conn)
        .await?;
    Ok((lines, status))
}

/// After a late replay lands on a closed till: refresh `current_system_total`
/// on every line (cash from `closing_cash_system`); a method newly used after
/// close gets an `unreviewed` line with `system_total 0`. Never changes an
/// existing line's `status`, `system_total`, or the till's
/// `reconciliation_status`. A till with no lines (pre-reconciliation) is left
/// untouched. Safe to call repeatedly.
pub async fn recompute_after_late_replay(
    conn: &mut sqlx::PgConnection,
    till_id: Uuid,
    closing_cash_system: i32,
) -> Result<(), AppError> {
    let existing = stored_lines(conn, till_id).await?;
    if existing.is_empty() {
        return Ok(());
    }
    let totals = system_totals_by_method(conn, till_id, closing_cash_system as i64).await?;
    for t in &totals {
        let current = clamp_i32(t.system_total);
        let target = if t.is_cash {
            existing.iter().find(|l| l.is_cash).map(|l| l.method.clone())
        } else {
            existing.iter().find(|l| !l.is_cash && l.method == t.method).map(|l| l.method.clone())
        };
        match target {
            Some(method) => {
                sqlx::query(
                    "UPDATE till_reconciliations SET current_system_total = $3, order_count = GREATEST(order_count, $4)
                     WHERE till_id = $1 AND method = $2",
                )
                .bind(till_id)
                .bind(method)
                .bind(current)
                .bind(clamp_i32(t.order_count))
                .execute(&mut *conn)
                .await?;
            }
            None => {
                sqlx::query(
                    "INSERT INTO till_reconciliations
                       (till_id, method, payment_method_id, is_cash, system_total, current_system_total, order_count, status)
                     VALUES ($1, $2, $3, $4, 0, $5, $6, 'unreviewed')
                     ON CONFLICT (till_id, method) DO UPDATE SET current_system_total = EXCLUDED.current_system_total",
                )
                .bind(till_id)
                .bind(&t.method)
                .bind(t.payment_method_id)
                .bind(t.is_cash)
                .bind(current)
                .bind(clamp_i32(t.order_count))
                .execute(&mut *conn)
                .await?;
            }
        }
    }
    // Methods whose money all went away after close (e.g. a late void) read 0 now.
    let used: Vec<String> = totals.iter().filter(|t| !t.is_cash).map(|t| t.method.clone()).collect();
    sqlx::query(
        "UPDATE till_reconciliations SET current_system_total = 0
         WHERE till_id = $1 AND NOT is_cash AND method <> ALL($2)",
    )
    .bind(till_id)
    .bind(&used)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Stored lines for a till, cash first then by method name.
pub async fn lines_for_till(
    pool: &PgPool,
    till_id: Uuid,
) -> Result<Vec<TillReconciliationLine>, AppError> {
    let mut conn = pool.acquire().await?;
    stored_lines(&mut conn, till_id).await
}
