//! [`QuerySpec`] → SQL.
//!
//! This is the only place in the system that assembles an analytics query, and
//! every fragment it concatenates is an author-written `&'static str` from
//! [`super::schema`]. Caller-supplied text is used exclusively to *look up*
//! fragments; caller-supplied values (dates, limits, thresholds) become bound
//! parameters. There is no code path from input text to SQL text.
//!
//! Errors are deliberately verbose and carry the valid alternatives, because
//! they are fed straight back to the AI agent as a tool result so it can correct
//! itself — a rejected spec should teach the model, not end the conversation.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

use super::execute::Bound;
use super::schema::{self, Dataset, Dim, Meas};
use super::spec::{DEFAULT_LIMIT, MAX_LIMIT, QuerySpec, ResolvedPeriod};
use super::types::{Column, ColumnKind, Compare, Dir, Grain, Viz};

/// A spec that could not be compiled, with enough context to retry.
#[derive(Debug, Clone)]
pub struct SpecError {
    pub message: String,
    /// Valid ids for whatever was wrong, when there is a closed set.
    pub valid: Vec<String>,
}

impl SpecError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            valid: Vec::new(),
        }
    }
    fn with_valid<I, S>(message: impl Into<String>, valid: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            message: message.into(),
            valid: valid.into_iter().map(Into::into).collect(),
        }
    }
    /// The full text handed back to the model (or the API client): what was
    /// wrong plus what would have been right.
    pub fn detail(&self) -> String {
        if self.valid.is_empty() {
            self.message.clone()
        } else {
            format!("{}. Valid options: {}", self.message, self.valid.join(", "))
        }
    }
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail())
    }
}

impl From<SpecError> for crate::errors::AppError {
    fn from(e: SpecError) -> Self {
        crate::errors::AppError::BadRequest(e.detail())
    }
}

/// Everything compilation needs that is not in the spec itself.
pub struct CompileCtx {
    /// The merchant's timezone — periods and time buckets resolve in it.
    pub tz: Tz,
    /// "Now", injected so period resolution is deterministic in tests.
    pub now: DateTime<Utc>,
}

/// A compiled, ready-to-run query.
#[derive(Debug)]
pub struct CompiledQuery {
    /// One read-only SELECT with named parameters (`:from`, `:branch_ids`, …).
    pub sql: String,
    pub columns: Vec<Column>,
    /// Values for the spec-controlled named parameters. The system parameters
    /// (`:branch_ids`, `:locale`, `:tz`) are injected by [`super::execute`] and
    /// can never appear here.
    pub binds: HashMap<String, Bound>,
    pub grain: Grain,
    pub viz: Viz,
    /// When set, the client renders one section per distinct value of this
    /// column — "a table per branch".
    pub facet_by: Option<&'static str>,
    /// The resolved window, echoed back so a client can label the answer
    /// ("last month" → actual dates) without re-deriving it.
    pub period: ResolvedPeriod,
    pub dataset: &'static Dataset,
}

/// Compile a spec into SQL. See the module docs for the security argument.
pub fn compile(spec: &QuerySpec, ctx: &CompileCtx) -> Result<CompiledQuery, SpecError> {
    let ds = schema::dataset(&spec.dataset).ok_or_else(|| {
        SpecError::with_valid(
            format!("Unknown dataset '{}'", spec.dataset),
            schema::DATASETS.iter().map(|d| d.id),
        )
    })?;

    let dims = resolve_dims(ds, spec)?;
    let measures = resolve_measures(ds, spec)?;
    let filter_preds = resolve_filters(ds, spec)?;
    let sort_meas = resolve_sort(&measures, spec)?;
    let period = spec.period.resolve(ctx.tz, ctx.now);

    let grain = grain_of(&dims);
    let limit = spec.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    let mut binds: HashMap<String, Bound> = HashMap::new();
    binds.insert("from".into(), Bound::Ts(period.from));
    binds.insert("to".into(), Bound::Ts(period.to));
    binds.insert("limit".into(), Bound::Int(limit as i64));

    // SELECT list and output columns: dimensions first, then measures. The
    // dimension order is also the GROUP BY ordinal order.
    let mut select: Vec<String> = Vec::with_capacity(dims.len() + measures.len());
    let mut columns: Vec<Column> = Vec::with_capacity(dims.len() + measures.len());
    for d in &dims {
        select.push(format!("{} AS {}", d.expr, d.id));
        columns.push(Column {
            key: d.id,
            label: d.label,
            kind: d.kind,
        });
    }
    for m in &measures {
        select.push(format!("{} AS {}", m.expr, m.id));
        columns.push(Column {
            key: m.id,
            label: m.label,
            kind: m.kind,
        });
    }

    let joins = joins_for(ds, &dims, &measures);
    let where_clause = where_clause(ds, &filter_preds);
    let group_by = if dims.is_empty() {
        String::new()
    } else {
        format!(
            "GROUP BY {}",
            (1..=dims.len())
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };

    // HAVING must repeat the aggregate expression: Postgres does not resolve
    // output aliases there.
    let having = match spec.having_min {
        Some(min) if min > 0 => {
            binds.insert("having_min".into(), Bound::Int(min));
            format!("HAVING {} >= :having_min", sort_meas.expr)
        }
        _ => String::new(),
    };

    // Ordering. A time series is ordered by time ascending — sorting a trend by
    // magnitude produces a chart nobody can read — everything else by the sort
    // measure, descending unless asked otherwise.
    let explicit_dir = spec.sort.as_ref().map(|s| s.dir);
    let time_dim = dims.iter().find(|d| d.time);
    let order_by = match (time_dim, spec.sort.as_ref()) {
        (Some(td), None) => format!("ORDER BY {} ASC", td.id),
        (Some(td), Some(_)) if dims.len() == 1 => {
            format!(
                "ORDER BY {} {}",
                td.id,
                explicit_dir.unwrap_or(Dir::Asc).sql()
            )
        }
        _ => format!(
            "ORDER BY {} {} NULLS LAST",
            sort_meas.id,
            explicit_dir.unwrap_or(Dir::Desc).sql()
        ),
    };
    let rank_dir = explicit_dir.unwrap_or(Dir::Desc);

    let base = |extra_where: &str| {
        format!(
            "SELECT {sel} FROM {from} {joins} {where_clause} {extra_where} {group_by} {having}",
            sel = select.join(", "),
            from = ds.from,
        )
    };

    // ── Period-over-period comparison ────────────────────────────────────────
    if spec.compare != Compare::None {
        return compile_compare(
            ds,
            spec,
            &dims,
            sort_meas,
            &select,
            &joins,
            &filter_preds,
            &group_by,
            columns,
            binds,
            period,
            grain,
            rank_dir,
        );
    }

    // ── Top-N within each group ("best seller per branch") ───────────────────
    if let Some(tp) = &spec.transform.top_per {
        if spec.transform.share || spec.transform.cumulative {
            return Err(SpecError::new(
                "'top_per' cannot be combined with 'share' or 'cumulative'",
            ));
        }
        let per = dims.iter().find(|d| d.id == tp.dimension).ok_or_else(|| {
            SpecError::with_valid(
                format!(
                    "'top_per.dimension' ({}) must be one of the dimensions you selected",
                    tp.dimension
                ),
                dims.iter().map(|d| d.id),
            )
        })?;
        if dims.len() < 2 {
            return Err(SpecError::new(
                "'top_per' needs at least two dimensions: the one to rank within, and the one being ranked",
            ));
        }
        binds.insert("top_per".into(), Bound::Int(tp.n.clamp(1, 50) as i64));
        let mut sel = select.clone();
        sel.push(format!(
            "ROW_NUMBER() OVER (PARTITION BY {} ORDER BY {} {}) AS rank",
            per.expr,
            sort_meas.expr,
            rank_dir.sql()
        ));
        let mut cols = columns;
        cols.push(Column {
            key: "rank",
            label: "Rank",
            kind: ColumnKind::Count,
        });
        let sql = format!(
            "SELECT * FROM (SELECT {sel} FROM {from} {joins} {where_clause} {group_by} {having}) ranked \
             WHERE rank <= :top_per ORDER BY {per_id}, rank LIMIT :limit",
            sel = sel.join(", "),
            from = ds.from,
            per_id = per.id,
        );
        return Ok(CompiledQuery {
            sql,
            columns: cols,
            binds,
            grain: Grain::Table,
            viz: Viz::Table,
            facet_by: Some(per.id),
            period,
            dataset: ds,
        });
    }

    // ── Window transforms: share of total and running total ──────────────────
    if spec.transform.share || spec.transform.cumulative {
        let mut cols = columns;
        let mut extra: Vec<String> = Vec::new();
        if spec.transform.share {
            extra.push(format!(
                "ROUND(100.0 * base.{m} / NULLIF(SUM(base.{m}) OVER (), 0), 1)::float8 AS share_pct",
                m = sort_meas.id
            ));
            cols.push(Column {
                key: "share_pct",
                label: "% of total",
                kind: ColumnKind::Number,
            });
        }
        if spec.transform.cumulative {
            let td = time_dim.ok_or_else(|| {
                SpecError::new("'cumulative' needs a time dimension (day, week or month)")
            })?;
            extra.push(format!(
                "SUM(base.{m}) OVER (ORDER BY base.{t}) AS cumulative",
                m = sort_meas.id,
                t = td.id
            ));
            cols.push(Column {
                key: "cumulative",
                label: "Running total",
                kind: sort_meas.kind,
            });
        }
        let sql = format!(
            "SELECT base.*, {extra} FROM ({inner}) base {order_by} LIMIT :limit",
            extra = extra.join(", "),
            inner = base(""),
            order_by = order_by.replace("ORDER BY ", "ORDER BY base."),
        );
        let viz = spec.viz.unwrap_or(Viz::Auto).resolve(grain, dims.len());
        return Ok(CompiledQuery {
            sql,
            columns: cols,
            binds,
            grain,
            viz,
            facet_by: None,
            period,
            dataset: ds,
        });
    }

    // ── The plain case ───────────────────────────────────────────────────────
    let sql = format!("{} {order_by} LIMIT :limit", base(""));
    let viz = spec
        .viz
        .unwrap_or(Viz::Auto)
        .resolve(grain, dims.len())
        .pick_default(ds, grain);

    Ok(CompiledQuery {
        sql,
        columns,
        binds,
        grain,
        viz,
        facet_by: None,
        period,
        dataset: ds,
    })
}

impl Viz {
    /// After [`Viz::resolve`], let a dataset express a house preference for its
    /// breakdowns (payments read better as a donut than a bar).
    fn pick_default(self, ds: &Dataset, grain: Grain) -> Viz {
        if self == Viz::Bar && grain == Grain::Categorical {
            ds.default_viz
        } else {
            self
        }
    }
}

/// Comparison has its own SQL shape: the current window and the shifted window
/// are aggregated separately and joined on the dimension keys, so the result
/// carries `prev` and `change_pct` alongside each row.
#[allow(clippy::too_many_arguments)]
fn compile_compare(
    ds: &'static Dataset,
    spec: &QuerySpec,
    dims: &[&'static Dim],
    sort_meas: &'static Meas,
    select: &[String],
    joins: &str,
    filter_preds: &str,
    group_by: &str,
    mut columns: Vec<Column>,
    binds: HashMap<String, Bound>,
    period: ResolvedPeriod,
    grain: Grain,
    dir: Dir,
) -> Result<CompiledQuery, SpecError> {
    if spec.transform.top_per.is_some() || spec.transform.share || spec.transform.cumulative {
        return Err(SpecError::new(
            "'compare' cannot be combined with 'top_per', 'share' or 'cumulative'",
        ));
    }
    if dims.iter().any(|d| d.time) {
        return Err(SpecError::new(
            "'compare' contrasts two windows, so it cannot also break down by time — \
             drop the day/week/month dimension, or drop 'compare'",
        ));
    }
    if !period.is_bounded() {
        return Err(SpecError::new(
            "'compare' needs a bounded period — use a preset such as 'last_month', \
             or set both period.from and period.to",
        ));
    }

    let t = ds.time_col;
    let (cur, prev) = if ds.time_is_date {
        // A date column compares against the period bounds converted to local
        // dates, matching how the main period predicate treats this dataset.
        (
            format!(
                "AND {t} >= (:from AT TIME ZONE :tz)::date AND {t} <= (:to AT TIME ZONE :tz)::date"
            ),
            match spec.compare {
                Compare::PreviousPeriod => format!(
                    "AND {t} >= ((:from - (:to - :from)) AT TIME ZONE :tz)::date \
                     AND {t} < (:from AT TIME ZONE :tz)::date"
                ),
                _ => format!(
                    "AND {t} >= ((:from - interval '1 year') AT TIME ZONE :tz)::date \
                     AND {t} <= ((:to - interval '1 year') AT TIME ZONE :tz)::date"
                ),
            },
        )
    } else {
        (
            format!("AND {t} >= :from AND {t} <= :to"),
            match spec.compare {
                Compare::PreviousPeriod => {
                    format!("AND {t} >= (:from - (:to - :from)) AND {t} < :from")
                }
                _ => format!(
                    "AND {t} >= (:from - interval '1 year') AND {t} <= (:to - interval '1 year')"
                ),
            },
        )
    };

    // Note the period predicate is supplied per-CTE here rather than by the
    // shared WHERE builder, which is why the fence and filters are inlined.
    let cte = |window: &str| {
        format!(
            "SELECT {sel} FROM {from} {joins} WHERE {branch} = ANY(:branch_ids) {window} {base_pred} {filters} {group_by}",
            sel = select.join(", "),
            from = ds.from,
            branch = ds.branch_col,
            base_pred = ds.base_pred,
            filters = filter_preds,
        )
    };

    let join = if dims.is_empty() {
        "cur CROSS JOIN prev".to_string()
    } else {
        format!(
            "cur LEFT JOIN prev USING ({})",
            dims.iter().map(|d| d.id).collect::<Vec<_>>().join(", ")
        )
    };

    let m = sort_meas.id;
    let sql = format!(
        "WITH cur AS ({cur_q}), prev AS ({prev_q}) \
         SELECT cur.*, prev.{m} AS previous, \
                ROUND(100.0 * (cur.{m} - prev.{m}) / NULLIF(prev.{m}, 0), 1)::float8 AS change_pct \
         FROM {join} ORDER BY cur.{m} {dir} NULLS LAST LIMIT :limit",
        cur_q = cte(&cur),
        prev_q = cte(&prev),
        dir = dir.sql(),
    );

    columns.push(Column {
        key: "previous",
        label: "Previous",
        kind: sort_meas.kind,
    });
    columns.push(Column {
        key: "change_pct",
        label: "Change %",
        kind: ColumnKind::Number,
    });

    Ok(CompiledQuery {
        sql,
        columns,
        binds,
        grain,
        viz: spec.viz.unwrap_or(Viz::Auto).resolve(grain, dims.len()),
        facet_by: None,
        period,
        dataset: ds,
    })
}

// ── Resolution helpers ───────────────────────────────────────────────────────

fn resolve_dims(ds: &'static Dataset, spec: &QuerySpec) -> Result<Vec<&'static Dim>, SpecError> {
    let mut out: Vec<&'static Dim> = Vec::new();
    for id in &spec.dimensions {
        let d = ds.dim(id).ok_or_else(|| {
            SpecError::with_valid(
                format!("Dimension '{id}' is not available on dataset '{}'", ds.id),
                ds.dims.iter().map(|d| d.id),
            )
        })?;
        if !out.iter().any(|x| x.id == d.id) {
            out.push(d);
        }
    }
    // Three grouping axes make a result nobody can read and a cardinality
    // explosion; the row cap would then silently truncate the interesting part.
    if out.len() > 2 {
        return Err(SpecError::new(
            "At most 2 dimensions. For a deeper breakdown, use 'top_per' to rank \
             within a group, or filter to narrow the question",
        ));
    }
    Ok(out)
}

fn resolve_measures(
    ds: &'static Dataset,
    spec: &QuerySpec,
) -> Result<Vec<&'static Meas>, SpecError> {
    let ids: Vec<&str> = if spec.measures.is_empty() {
        ds.default_measures.to_vec()
    } else {
        spec.measures.iter().map(String::as_str).collect()
    };
    let mut out: Vec<&'static Meas> = Vec::new();
    for id in ids {
        let m = ds.measure(id).ok_or_else(|| {
            SpecError::with_valid(
                format!("Measure '{id}' is not available on dataset '{}'", ds.id),
                ds.measures.iter().map(|m| m.id),
            )
        })?;
        if !out.iter().any(|x| x.id == m.id) {
            out.push(m);
        }
    }
    if out.is_empty() {
        return Err(SpecError::with_valid(
            "Choose at least one measure",
            ds.measures.iter().map(|m| m.id),
        ));
    }
    if out.len() > 8 {
        return Err(SpecError::new("At most 8 measures in one query"));
    }
    Ok(out)
}

/// Every filter the dataset declares contributes a predicate — the caller's
/// chosen value if valid, otherwise the dataset's default. Applying defaults
/// unconditionally is what makes "revenue" mean revenue: omitting the status
/// filter still excludes voided orders.
fn resolve_filters(ds: &'static Dataset, spec: &QuerySpec) -> Result<String, SpecError> {
    for id in spec.filters.keys() {
        if ds.filter(id).is_none() {
            return Err(SpecError::with_valid(
                format!("Filter '{id}' is not available on dataset '{}'", ds.id),
                ds.filters.iter().map(|f| f.id),
            ));
        }
    }
    let mut preds = String::new();
    for f in ds.filters {
        let sql = match spec.filters.get(f.id) {
            Some(v) => {
                f.option(v)
                    .ok_or_else(|| {
                        SpecError::with_valid(
                            format!("'{v}' is not a valid value for filter '{}'", f.id),
                            f.values(),
                        )
                    })?
                    .sql
            }
            None => f.default_sql(),
        };
        if !sql.is_empty() {
            preds.push(' ');
            preds.push_str(sql);
        }
    }
    Ok(preds)
}

fn resolve_sort(measures: &[&'static Meas], spec: &QuerySpec) -> Result<&'static Meas, SpecError> {
    match spec.sort.as_ref() {
        None => Ok(measures[0]),
        Some(s) => measures
            .iter()
            .copied()
            .find(|m| m.id == s.measure)
            .ok_or_else(|| {
                SpecError::with_valid(
                    format!(
                        "'sort.measure' ({}) must be one of the measures you selected",
                        s.measure
                    ),
                    measures.iter().map(|m| m.id),
                )
            }),
    }
}

/// Emit only the joins the selected columns need, in the dataset's declared
/// order. Declaration order is dependency order (`category` references the alias
/// `menu_item` introduces), so preserving it is what keeps the SQL valid no
/// matter what order the caller listed columns in.
fn joins_for(ds: &'static Dataset, dims: &[&'static Dim], measures: &[&'static Meas]) -> String {
    let needed: Vec<&str> = dims
        .iter()
        .flat_map(|d| d.joins.iter().copied())
        .chain(measures.iter().flat_map(|m| m.joins.iter().copied()))
        .collect();
    ds.joins
        .iter()
        .filter(|j| needed.contains(&j.id))
        .map(|j| j.sql)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The branch fence, the reporting period, the dataset's always-on predicate,
/// and the resolved filters.
///
/// `:branch_ids` is injected by the executor from the caller's verified access,
/// never from the spec, so no spec can widen scope.
fn where_clause(ds: &'static Dataset, filter_preds: &str) -> String {
    let period = if ds.time_is_date {
        format!(
            "AND (:from::timestamptz IS NULL OR {t} >= (:from AT TIME ZONE :tz)::date) \
             AND (:to::timestamptz IS NULL OR {t} <= (:to AT TIME ZONE :tz)::date)",
            t = ds.time_col
        )
    } else {
        format!(
            "AND (:from::timestamptz IS NULL OR {t} >= :from) \
             AND (:to::timestamptz IS NULL OR {t} <= :to)",
            t = ds.time_col
        )
    };
    format!(
        "WHERE {branch} = ANY(:branch_ids) {period} {base}{filters}",
        branch = ds.branch_col,
        base = ds.base_pred,
        filters = filter_preds,
    )
}

fn grain_of(dims: &[&'static Dim]) -> Grain {
    match dims.len() {
        0 => Grain::Scalar,
        1 if dims[0].time => Grain::Series,
        1 => Grain::Categorical,
        _ => Grain::Table,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::spec::{Period, PeriodPreset, Sort, TopPer};

    fn ctx() -> CompileCtx {
        CompileCtx {
            tz: chrono_tz::Africa::Cairo,
            now: crate::analytics::spec::parse_flexible_date("2026-09-02T10:00:00Z").unwrap(),
        }
    }

    fn spec(dataset: &str) -> QuerySpec {
        QuerySpec {
            dataset: dataset.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_bare_spec_compiles_to_totals_with_the_default_measures() {
        let q = compile(&spec("orders"), &ctx()).unwrap();
        assert_eq!(q.grain, Grain::Scalar);
        assert_eq!(q.viz, Viz::Kpi);
        assert!(q.sql.contains("COUNT(DISTINCT o.id) AS order_count"));
        // The status default applies even though the spec named no filter.
        assert!(q.sql.contains("o.status NOT IN ('voided','refunded')"));
        assert!(!q.sql.contains("GROUP BY"));
    }

    #[test]
    fn the_branch_fence_is_always_present() {
        // Every dataset, every shape: there is no compiled query without it.
        for ds in schema::DATASETS {
            let q = compile(&spec(ds.id), &ctx()).unwrap();
            assert!(
                q.sql.contains("= ANY(:branch_ids)"),
                "{} compiled without a branch fence",
                ds.id
            );
        }
    }

    #[test]
    fn a_time_dimension_produces_a_series_ordered_by_time() {
        let mut s = spec("orders");
        s.dimensions = vec!["day".into()];
        let q = compile(&s, &ctx()).unwrap();
        assert_eq!(q.grain, Grain::Series);
        assert_eq!(q.viz, Viz::Line);
        assert!(q.sql.contains("ORDER BY day ASC"));
        assert!(q.sql.contains("GROUP BY 1"));
    }

    #[test]
    fn a_category_breakdown_ranks_by_the_first_measure_descending() {
        let mut s = spec("order_items");
        s.dimensions = vec!["product".into()];
        s.measures = vec!["item_revenue".into()];
        let q = compile(&s, &ctx()).unwrap();
        assert_eq!(q.grain, Grain::Categorical);
        assert!(q.sql.contains("ORDER BY item_revenue DESC NULLS LAST"));
    }

    #[test]
    fn joins_are_emitted_in_dependency_order_regardless_of_request_order() {
        let mut s = spec("order_items");
        // `category` depends on the alias `menu_item` introduces. Requesting
        // them in this order must still emit menu_items before categories.
        s.dimensions = vec!["category".into()];
        let q = compile(&s, &ctx()).unwrap();
        let mi = q.sql.find("menu_items mi").expect("menu_items join");
        let cat = q.sql.find("categories c").expect("categories join");
        assert!(mi < cat, "join order would produce invalid SQL");
    }

    #[test]
    fn unneeded_joins_are_not_emitted() {
        let q = compile(&spec("orders"), &ctx()).unwrap();
        assert!(!q.sql.contains("LEFT JOIN users"));
        assert!(!q.sql.contains("LATERAL"));
    }

    #[test]
    fn unknown_ids_are_rejected_with_the_valid_alternatives() {
        let mut s = spec("orders");
        s.dimensions = vec!["nonsense".into()];
        let e = compile(&s, &ctx()).unwrap_err();
        assert!(e.message.contains("nonsense"));
        // The retry hint is what lets the agent correct itself.
        assert!(e.valid.contains(&"branch".to_string()));

        let e = compile(&spec("no_such_dataset"), &ctx()).unwrap_err();
        assert!(e.valid.contains(&"orders".to_string()));

        let mut s = spec("orders");
        s.measures = vec!["revenoo".into()];
        assert!(
            compile(&s, &ctx())
                .unwrap_err()
                .valid
                .contains(&"revenue".to_string())
        );

        let mut s = spec("orders");
        s.filters.insert("status".into(), "sould".into());
        let e = compile(&s, &ctx()).unwrap_err();
        assert!(e.valid.contains(&"sold".to_string()));
    }

    #[test]
    fn filters_select_only_authored_predicates() {
        let mut s = spec("orders");
        s.filters.insert("status".into(), "all".into());
        s.filters.insert("order_type".into(), "delivery".into());
        let q = compile(&s, &ctx()).unwrap();
        assert!(!q.sql.contains("NOT IN ('voided'"));
        assert!(!q.sql.contains("o.order_type = 'dine_in'"));
        assert!(q.sql.contains("o.order_type = 'delivery'"));
    }

    #[test]
    fn hostile_filter_values_cannot_reach_the_sql() {
        let mut s = spec("orders");
        s.filters
            .insert("status".into(), "sold'; DROP TABLE orders; --".into());
        let e = compile(&s, &ctx()).unwrap_err();
        assert!(e.message.contains("not a valid value"));
        // And nothing of the payload survives into any emitted SQL.
        let ok = compile(&spec("orders"), &ctx()).unwrap();
        assert!(!ok.sql.contains("DROP"));
    }

    #[test]
    fn the_period_is_bound_never_interpolated() {
        let mut s = spec("orders");
        s.period = Period::preset(PeriodPreset::LastMonth);
        let q = compile(&s, &ctx()).unwrap();
        assert!(q.sql.contains(":from") && q.sql.contains(":to"));
        // 2026-08-01 00:00 Cairo, which is UTC+3 in summer.
        match q.binds.get("from") {
            Some(Bound::Ts(Some(t))) => assert_eq!(t.to_rfc3339(), "2026-07-31T21:00:00+00:00"),
            other => panic!("expected a bound timestamp, got {other:?}"),
        }
        assert!(!q.sql.contains("2026-08-01"));
    }

    #[test]
    fn limits_are_clamped_not_trusted() {
        let mut s = spec("orders");
        s.dimensions = vec!["branch".into()];
        s.limit = Some(999_999);
        let q = compile(&s, &ctx()).unwrap();
        assert!(matches!(q.binds.get("limit"), Some(Bound::Int(n)) if *n == MAX_LIMIT as i64));
    }

    #[test]
    fn compare_needs_a_bounded_period_and_refuses_a_time_axis() {
        let mut s = spec("orders");
        s.compare = Compare::PreviousPeriod;
        s.dimensions = vec!["day".into()];
        s.period = Period::preset(PeriodPreset::LastMonth);
        assert!(
            compile(&s, &ctx())
                .unwrap_err()
                .message
                .contains("break down by time")
        );

        let mut s = spec("orders");
        s.compare = Compare::PreviousPeriod;
        s.period = Period::preset(PeriodPreset::AllTime);
        assert!(
            compile(&s, &ctx())
                .unwrap_err()
                .message
                .contains("bounded period")
        );
    }

    #[test]
    fn compare_emits_both_windows_and_a_change_column() {
        let mut s = spec("orders");
        s.dimensions = vec!["branch".into()];
        s.compare = Compare::PreviousPeriod;
        s.period = Period::preset(PeriodPreset::LastMonth);
        let q = compile(&s, &ctx()).unwrap();
        assert!(q.sql.contains("WITH cur AS") && q.sql.contains("prev AS"));
        assert!(q.sql.contains("change_pct"));
        assert!(q.columns.iter().any(|c| c.key == "previous"));
        // Both windows carry the branch fence — the previous period is not a
        // hole in tenancy.
        assert_eq!(q.sql.matches("= ANY(:branch_ids)").count(), 2);
    }

    #[test]
    fn top_per_ranks_within_a_group_and_facets() {
        let mut s = spec("order_items");
        s.dimensions = vec!["branch".into(), "product".into()];
        s.transform.top_per = Some(TopPer {
            dimension: "branch".into(),
            n: 3,
        });
        let q = compile(&s, &ctx()).unwrap();
        assert!(q.sql.contains("ROW_NUMBER() OVER (PARTITION BY b.name"));
        assert!(q.sql.contains("WHERE rank <= :top_per"));
        assert_eq!(q.facet_by, Some("branch"));
        assert!(matches!(q.binds.get("top_per"), Some(Bound::Int(3))));
    }

    #[test]
    fn top_per_needs_two_dimensions() {
        let mut s = spec("order_items");
        s.dimensions = vec!["branch".into()];
        s.transform.top_per = Some(TopPer {
            dimension: "branch".into(),
            n: 1,
        });
        assert!(
            compile(&s, &ctx())
                .unwrap_err()
                .message
                .contains("two dimensions")
        );
    }

    #[test]
    fn cumulative_requires_a_time_dimension() {
        let mut s = spec("orders");
        s.dimensions = vec!["branch".into()];
        s.transform.cumulative = true;
        assert!(
            compile(&s, &ctx())
                .unwrap_err()
                .message
                .contains("time dimension")
        );

        let mut s = spec("orders");
        s.dimensions = vec!["day".into()];
        s.transform.cumulative = true;
        let q = compile(&s, &ctx()).unwrap();
        assert!(
            q.sql
                .contains("SUM(base.order_count) OVER (ORDER BY base.day)")
        );
    }

    #[test]
    fn share_adds_a_percentage_of_total_column() {
        let mut s = spec("order_items");
        s.dimensions = vec!["category".into()];
        s.transform.share = true;
        let q = compile(&s, &ctx()).unwrap();
        assert!(q.columns.iter().any(|c| c.key == "share_pct"));
        assert!(q.sql.contains("OVER ()"));
    }

    #[test]
    fn three_dimensions_are_refused() {
        let mut s = spec("orders");
        s.dimensions = vec!["day".into(), "branch".into(), "waiter".into()];
        assert!(
            compile(&s, &ctx())
                .unwrap_err()
                .message
                .contains("At most 2 dimensions")
        );
    }

    #[test]
    fn sort_must_name_a_selected_measure() {
        let mut s = spec("orders");
        s.measures = vec!["revenue".into()];
        s.sort = Some(Sort {
            measure: "tip_total".into(),
            dir: Dir::Desc,
        });
        let e = compile(&s, &ctx()).unwrap_err();
        assert!(e.valid.contains(&"revenue".to_string()));
    }

    #[test]
    fn a_date_grain_dataset_compares_against_local_dates() {
        let mut s = spec("attendance");
        s.dimensions = vec!["employee".into()];
        let q = compile(&s, &ctx()).unwrap();
        assert!(q.sql.contains("(:from AT TIME ZONE :tz)::date"));
    }

    #[test]
    fn every_dataset_compiles_with_every_dimension_and_measure() {
        // A registry-wide smoke test: any authored fragment that cannot be
        // assembled is a bug in the registry, caught here rather than in prod.
        for ds in schema::DATASETS {
            for d in ds.dims {
                for m in ds.measures {
                    let s = QuerySpec {
                        dataset: ds.id.into(),
                        dimensions: vec![d.id.into()],
                        measures: vec![m.id.into()],
                        ..Default::default()
                    };
                    let q = compile(&s, &ctx())
                        .unwrap_or_else(|e| panic!("{}/{}/{}: {e}", ds.id, d.id, m.id));
                    assert!(q.sql.contains("SELECT"));
                }
            }
        }
    }
}
