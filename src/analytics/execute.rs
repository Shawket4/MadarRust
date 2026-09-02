//! The single execution choke point.
//!
//! Every analytics query in the system — a dashboard widget, a curated preset,
//! or something the AI agent composed — runs through [`run`] and nothing else.
//! Concentrating execution in one function is what makes the safety envelope
//! auditable, because it holds regardless of who produced the SQL:
//!
//!   * **RLS.** The query runs on the caller's tenant pool ([`crate::db::Db`]),
//!     so Postgres itself fences every row to the caller's organization.
//!   * **Branch fence.** `:branch_ids` is injected *here* from the caller's
//!     verified access ([`super::scope`]) — never from the request body — so no
//!     spec can widen scope beyond what the user may see.
//!   * **Read only.** `SET TRANSACTION READ ONLY` means this path cannot write,
//!     even though the `madar_app` role can in general.
//!   * **Bounded.** A `LOCAL statement_timeout` caps runtime and rows are hard
//!     capped, so a pathological grouping degrades into a truncated answer
//!     rather than a stalled worker on a one-vCPU box.
//!
//! Queries are authored with **named** parameters (`:from`, `:branch_ids`, …)
//! and rewritten to positional binds immediately before execution, so a fragment
//! can reference the branch scope or the locale wherever it needs to without any
//! author tracking `$1`/`$2` offsets across composed SQL.

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{Map, Value};
use sqlx::{Column as _, Row};
use uuid::Uuid;

use crate::db::Db;

use super::compile::CompiledQuery;
use super::spec::ResolvedPeriod;
use super::types::{Column, ColumnKind, Grain, Viz};

/// Hard ceiling on returned rows, whatever the query's own LIMIT says.
pub const MAX_ROWS: usize = 1000;
/// Per-query statement timeout. Generous enough for a year-long grouping on the
/// production box, short enough that a runaway cannot hold a connection.
pub const STATEMENT_TIMEOUT_MS: i64 = 8_000;

/// A validated value ready to bind. There is no `Raw`/`Sql` variant, and there
/// never should be: everything that reaches Postgres is either an author-written
/// fragment or one of these.
#[derive(Debug, Clone)]
pub enum Bound {
    Int(i64),
    Text(String),
    Ts(Option<DateTime<Utc>>),
    Uuids(Vec<Uuid>),
}

#[derive(Debug)]
pub enum ExecError {
    Db(sqlx::Error),
    /// The SQL referenced a named parameter nothing supplied — a registry bug,
    /// not a caller error.
    MissingBind(String),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::Db(e) => write!(f, "query failed: {e}"),
            ExecError::MissingBind(n) => write!(f, "query references unknown parameter :{n}"),
        }
    }
}

impl From<ExecError> for crate::errors::AppError {
    fn from(e: ExecError) -> Self {
        match e {
            ExecError::Db(e) => crate::errors::AppError::Db(e),
            // A missing bind means the registry emitted SQL it cannot satisfy.
            // That is ours, not the caller's.
            ExecError::MissingBind(_) => crate::errors::AppError::Internal,
        }
    }
}

/// System-injected execution context. Every field is backend-derived; none of it
/// is reachable from a request body or a model's output.
pub struct ExecCtx<'a> {
    /// The branches this caller may see. Resolved by [`super::scope`] from the
    /// verified JWT claims.
    pub branch_ids: &'a [Uuid],
    /// Locale key for translated labels.
    pub locale: &'a str,
    /// IANA timezone name for time bucketing.
    pub tz: &'a str,
}

/// The tabular result of a query, plus everything a client needs to render it
/// without knowing what was asked.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<Column>,
    pub rows: Vec<Map<String, Value>>,
    pub row_count: usize,
    /// True when the row cap trimmed the result.
    pub truncated: bool,
    pub grain: Grain,
    pub viz: Viz,
    pub facet_by: Option<&'static str>,
    pub period: ResolvedPeriod,
}

impl QueryResult {
    /// A compact JSON rendering for handing back to a language model: the
    /// column metadata plus the rows, truncated to a sample. The model needs
    /// enough to state a takeaway, not the whole table.
    pub fn to_model_json(&self, sample: usize) -> Value {
        serde_json::json!({
            "columns": self.columns.iter().map(|c| serde_json::json!({
                "key": c.key, "label": c.label, "kind": c.kind
            })).collect::<Vec<_>>(),
            "row_count": self.row_count,
            "truncated": self.truncated || self.rows.len() > sample,
            "rows": self.rows.iter().take(sample).collect::<Vec<_>>(),
        })
    }

    /// True when the query succeeded but matched nothing — a distinct outcome
    /// from an error, and one the agent must be able to explain rather than
    /// render as a blank table.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Execute a compiled query under the full safety envelope.
pub async fn run(
    db: &Db,
    query: &CompiledQuery,
    ctx: &ExecCtx<'_>,
) -> Result<QueryResult, ExecError> {
    let mut binds = query.binds.clone();
    // System parameters are inserted last and unconditionally, so they overwrite
    // anything of the same name a compiled query might carry. Scope cannot be
    // spoofed by construction.
    binds.insert("branch_ids".into(), Bound::Uuids(ctx.branch_ids.to_vec()));
    binds.insert("locale".into(), Bound::Text(ctx.locale.to_string()));
    binds.insert("tz".into(), Bound::Text(ctx.tz.to_string()));

    let (positional_sql, order) = rewrite_named(&query.sql);

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
        let bound = binds
            .get(name)
            .ok_or_else(|| ExecError::MissingBind(name.clone()))?;
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
        .map(|row| map_row(row, &query.columns))
        .collect();

    Ok(QueryResult {
        columns: query.columns.clone(),
        row_count: rows.len(),
        rows,
        truncated,
        grain: query.grain,
        viz: query.viz,
        facet_by: query.facet_by,
        period: query.period,
    })
}

/// Rewrite `:name` placeholders to positional `$n`, preserving `::type` casts
/// and never touching the inside of a string literal or a quoted identifier.
/// A repeated name reuses its first position, so `:from` can appear as often as
/// a fragment needs it and still bind once.
fn rewrite_named(sql: &str) -> (String, Vec<String>) {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 16);
    let mut order: Vec<String> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        // Copy quoted literals/identifiers verbatim: a ':' inside `':00'` is
        // text, not a parameter.
        if c == b'\'' || c == b'"' {
            let quote = c;
            out.push(quote as char);
            i += 1;
            while i < bytes.len() {
                out.push(bytes[i] as char);
                if bytes[i] == quote {
                    // A doubled quote is an escaped quote — stay inside.
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
            // `::` is a cast, not a parameter.
            if i + 1 < bytes.len() && bytes[i + 1] == b':' {
                out.push_str("::");
                i += 2;
                continue;
            }
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

/// Decode one row into a JSON object keyed by column key, using the declared
/// [`ColumnKind`]. Decoding is by name, so the SELECT alias order is free.
///
/// A column the query did not actually produce decodes as `null` rather than
/// panicking: a partially-shaped result is still useful, and a hard failure here
/// would take down an entire dashboard for one bad widget.
fn map_row(row: &sqlx::postgres::PgRow, columns: &[Column]) -> Map<String, Value> {
    let mut obj = Map::with_capacity(columns.len());
    for col in columns {
        let present = row.columns().iter().any(|c| c.name() == col.key);
        let value = if !present {
            Value::Null
        } else {
            match col.kind {
                ColumnKind::Money | ColumnKind::Count => row
                    .try_get::<Option<i64>, _>(col.key)
                    .ok()
                    .flatten()
                    .map(Value::from)
                    .unwrap_or(Value::Null),
                ColumnKind::Number | ColumnKind::Minutes => row
                    .try_get::<Option<f64>, _>(col.key)
                    .ok()
                    .flatten()
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
mod tests {
    use super::rewrite_named;

    #[test]
    fn casts_literals_and_repeated_names_survive_the_rewrite() {
        let (sql, order) = rewrite_named(
            "SELECT lpad(x, 2, '0') || ':00' AS h FROM orders \
             WHERE branch_id = ANY(:branch_ids) \
             AND (:from::timestamptz IS NULL OR created_at >= :from) LIMIT :limit",
        );
        assert_eq!(order, vec!["branch_ids", "from", "limit"]);
        assert!(sql.contains("ANY($1)"));
        // A repeated name binds once and reuses its position.
        assert!(sql.contains("$2::timestamptz IS NULL OR created_at >= $2"));
        assert!(sql.trim_end().ends_with("LIMIT $3"));
        // The ':00' inside the string literal is untouched.
        assert!(sql.contains("'0') || ':00'"));
        assert!(!sql.contains(":branch_ids") && !sql.contains(":from"));
    }

    #[test]
    fn a_colon_inside_a_quoted_identifier_is_not_a_parameter() {
        let (sql, order) = rewrite_named(r#"SELECT 1 AS "a:b" WHERE x = :y"#);
        assert_eq!(order, vec!["y"]);
        assert!(sql.contains(r#""a:b""#));
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_literal_early() {
        let (sql, order) = rewrite_named("SELECT 'it''s :not_a_param' , :real");
        assert_eq!(order, vec!["real"]);
        assert!(sql.contains("'it''s :not_a_param'"));
    }
}
