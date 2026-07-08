//! Runs a chosen [`Report`] against the caller's RLS-scoped tenant pool.
//!
//! Reports are authored with **named parameters** (`:from`, `:limit`, and the
//! system-injected `:branch_ids` / `:locale`) rather than positional `$n`. The
//! executor rewrites them to positional binds just before running, which lets a
//! report reference the branch scope and locale wherever it needs them without
//! the authoring pain of tracking `$1`/`$2` offsets. Postgres `::type` casts are
//! left untouched.
//!
//! Every execution is defense-in-depth hardened, independent of what the model
//! asked for:
//!   * the model can only name a report id + typed values — never SQL, never a
//!     branch it lacks access to (`:branch_ids` is injected by the backend from
//!     the caller's claims, not by the model);
//!   * arguments are validated and coerced against the report's declared params;
//!   * the query runs inside a `READ ONLY` transaction — this path cannot write
//!     even though `madar_app` can in general;
//!   * a `LOCAL statement_timeout` caps runtime, and rows are hard-capped;
//!   * RLS scopes every row to the caller's org (`src/db.rs`).

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{Map, Value};
use sqlx::{Column as _, Row};
use uuid::Uuid;

use crate::db::Db;

use super::catalog::{Column, ColumnKind, ParamKind, Report};

/// Hard ceiling on rows returned regardless of a report's own LIMIT.
const MAX_ROWS: usize = 1000;
/// Per-report statement timeout.
const STATEMENT_TIMEOUT_MS: i64 = 8000;

#[derive(Debug)]
pub enum ExecError {
    BadArg(String),
    Db(sqlx::Error),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::BadArg(m) => write!(f, "{m}"),
            ExecError::Db(e) => write!(f, "query failed: {e}"),
        }
    }
}

impl From<ExecError> for crate::errors::AppError {
    fn from(e: ExecError) -> Self {
        match e {
            ExecError::BadArg(m) => crate::errors::AppError::BadRequest(m),
            ExecError::Db(e) => crate::errors::AppError::Db(e),
        }
    }
}

/// A validated value ready to bind.
enum Bound {
    Int(i64),
    Text(String),
    Ts(Option<DateTime<Utc>>),
    Uuids(Vec<Uuid>),
}

/// The tabular result of a report.
pub struct QueryResult {
    pub columns: &'static [Column],
    pub rows: Vec<Map<String, Value>>,
    pub row_count: usize,
    pub truncated: bool,
}

/// Validate the model's raw args against the report's declared params, keyed by
/// name. Missing optionals become NULL (dates) or their default (ints); unknown
/// keys are ignored; malformed / out-of-bounds values are rejected.
fn resolve_model_args(
    report: &Report,
    raw: &Map<String, Value>,
) -> Result<HashMap<String, Bound>, ExecError> {
    let mut out = HashMap::new();
    for p in report.params {
        let v = raw.get(p.name);
        let bound = match p.kind {
            ParamKind::Date => {
                let parsed = match v {
                    None | Some(Value::Null) => None,
                    Some(Value::String(s)) => Some(parse_date(s).ok_or_else(|| {
                        ExecError::BadArg(format!("'{}' is not a valid ISO-8601 date", p.name))
                    })?),
                    Some(_) => {
                        return Err(ExecError::BadArg(format!(
                            "'{}' must be a date string",
                            p.name
                        )));
                    }
                };
                if parsed.is_none() && p.required {
                    return Err(ExecError::BadArg(format!("'{}' is required", p.name)));
                }
                Bound::Ts(parsed)
            }
            ParamKind::Int { min, max, default } => {
                let n = match v {
                    None | Some(Value::Null) => default,
                    Some(Value::Number(n)) => n.as_i64().ok_or_else(|| {
                        ExecError::BadArg(format!("'{}' must be a whole number", p.name))
                    })?,
                    Some(Value::String(s)) => s.trim().parse::<i64>().map_err(|_| {
                        ExecError::BadArg(format!("'{}' must be a whole number", p.name))
                    })?,
                    Some(_) => {
                        return Err(ExecError::BadArg(format!("'{}' must be a number", p.name)));
                    }
                };
                Bound::Int(n.clamp(min, max))
            }
        };
        out.insert(p.name.to_string(), bound);
    }
    Ok(out)
}

/// Accept a full RFC-3339 timestamp or a bare `YYYY-MM-DD` (midnight UTC).
fn parse_date(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
}

/// Rewrite `:name` placeholders to positional `$n`, preserving `::type` casts.
/// Returns the positional SQL and the parameter names in bind order (first
/// appearance; a repeated name reuses its earlier position).
fn rewrite_named(sql: &str) -> (String, Vec<String>) {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 16);
    let mut order: Vec<String> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        // Copy quoted literals/identifiers verbatim so a `:` inside them (e.g.
        // the string ':00') is never mistaken for a parameter.
        if c == b'\'' || c == b'"' {
            let quote = c;
            out.push(quote as char);
            i += 1;
            while i < bytes.len() {
                out.push(bytes[i] as char);
                if bytes[i] == quote {
                    // A doubled quote ('') is an escaped quote — stay in-string.
                    if i + 1 < bytes.len() && bytes[i + 1] == quote {
                        out.push(quote as char);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c == b':' {
            // `::` is a cast — copy both bytes verbatim.
            if i + 1 < bytes.len() && bytes[i + 1] == b':' {
                out.push_str("::");
                i += 2;
                continue;
            }
            // `:name` — read the identifier.
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j > start {
                let name = &sql[start..j];
                let pos = order.iter().position(|n| n == name).unwrap_or_else(|| {
                    order.push(name.to_string());
                    order.len() - 1
                });
                out.push('$');
                out.push_str(&(pos + 1).to_string());
                i = j;
                continue;
            }
            out.push(':');
            i += 1;
        } else {
            out.push(c as char);
            i += 1;
        }
    }
    (out, order)
}

/// System-injected execution context — all backend-controlled, none of it
/// model-fillable. `branch_ids` is the caller's accessible branch set (RLS
/// already fences the org; this fences the branch subset the user may see);
/// `locale` selects translated labels; `tz` buckets time in the org's timezone.
pub struct ExecCtx<'a> {
    pub branch_ids: &'a [Uuid],
    pub locale: &'a str,
    pub tz: &'a str,
}

/// Execute `report` for the caller with the system-injected [`ExecCtx`].
pub async fn run(
    db: &Db,
    report: &'static Report,
    raw: &Map<String, Value>,
    ctx: &ExecCtx<'_>,
) -> Result<QueryResult, ExecError> {
    let mut values = resolve_model_args(report, raw)?;
    // System-injected params — authoritative, cannot be overridden by the model.
    values.insert("branch_ids".into(), Bound::Uuids(ctx.branch_ids.to_vec()));
    values.insert("locale".into(), Bound::Text(ctx.locale.to_string()));
    values.insert("tz".into(), Bound::Text(ctx.tz.to_string()));

    let (positional_sql, order) = rewrite_named(report.sql);

    let mut tx = db.begin().await.map_err(ExecError::Db)?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(ExecError::Db)?;
    sqlx::query(&format!(
        "SET LOCAL statement_timeout = {STATEMENT_TIMEOUT_MS}"
    ))
    .execute(&mut *tx)
    .await
    .map_err(ExecError::Db)?;

    let mut q = sqlx::query(&positional_sql);
    for name in &order {
        let bound = values.get(name).ok_or_else(|| {
            ExecError::BadArg(format!("report '{}' references unknown :{name}", report.id))
        })?;
        q = match bound {
            Bound::Int(n) => q.bind(*n),
            Bound::Text(s) => q.bind(s.clone()),
            Bound::Ts(t) => q.bind(*t),
            Bound::Uuids(v) => q.bind(v.clone()),
        };
    }

    let pg_rows = q.fetch_all(&mut *tx).await.map_err(ExecError::Db)?;
    tx.commit().await.map_err(ExecError::Db)?;

    let truncated = pg_rows.len() > MAX_ROWS;
    let rows: Vec<Map<String, Value>> = pg_rows
        .iter()
        .take(MAX_ROWS)
        .map(|row| map_row(row, report.columns))
        .collect();

    Ok(QueryResult {
        columns: report.columns,
        row_count: rows.len(),
        rows,
        truncated,
    })
}

/// Map one Postgres row into a JSON object keyed by column key, decoded by the
/// declared [`ColumnKind`]. Decoding is by-name, so SELECT alias order is free.
fn map_row(row: &sqlx::postgres::PgRow, columns: &[Column]) -> Map<String, Value> {
    let mut obj = Map::with_capacity(columns.len());
    for col in columns {
        let present = row.columns().iter().any(|c| c.name() == col.key);
        let value = if !present {
            Value::Null
        } else {
            match col.kind {
                ColumnKind::Money | ColumnKind::Count => row
                    .try_get::<i64, _>(col.key)
                    .ok()
                    .map(Value::from)
                    .unwrap_or(Value::Null),
                ColumnKind::Number => row
                    .try_get::<f64, _>(col.key)
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
                ColumnKind::Label => row
                    .try_get::<Option<String>, _>(col.key)
                    .ok()
                    .flatten()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
                ColumnKind::Date => row
                    .try_get::<Option<NaiveDate>, _>(col.key)
                    .ok()
                    .flatten()
                    .map(|d| Value::String(d.to_string()))
                    .unwrap_or(Value::Null),
            }
        };
        obj.insert(col.key.to_string(), value);
    }
    obj
}

#[cfg(test)]
mod rewrite_tests {
    use super::rewrite_named;

    #[test]
    fn preserves_casts_and_numbers_params() {
        let (sql, order) = rewrite_named(
            "SELECT lpad(x, 2, '0') || ':00' AS h FROM orders \
             WHERE branch_id = ANY(:branch_ids) \
             AND (:from::timestamptz IS NULL OR created_at >= :from) LIMIT :limit",
        );
        assert_eq!(order, vec!["branch_ids", "from", "limit"]);
        // :from reused → same $2; ::timestamptz cast intact.
        assert!(sql.contains("ANY($1)"));
        assert!(sql.contains("$2::timestamptz IS NULL OR created_at >= $2"));
        assert!(sql.trim_end().ends_with("LIMIT $3"));
        // The string literal ':00' is untouched; no named `:ident` remains.
        assert!(sql.contains("'0') || ':00'"));
        assert!(!sql.contains(":branch_ids") && !sql.contains(":from") && !sql.contains(":limit"));
    }
}
