//! Semantic layer + the `analytics_query` builder — the flexible counterpart to
//! the fixed [`super::catalog`] menu.
//!
//! Instead of one frozen SQL per question, the model composes a query from a
//! **whitelist** of datasets × dimensions × measures × filters. Every fragment
//! the builder can emit is an author-written `&'static str`; the model only
//! supplies *ids* that select fragments, so — exactly like the catalog — a
//! hostile or hallucinated argument can only ever pick a pre-approved fragment,
//! never inject SQL. The assembled query runs through the same hardened
//! [`super::executor::run_resolved`] (read-only, timed, row-capped, RLS-scoped,
//! `:branch_ids`-fenced), and uses the same named params (`:from` / `:to` /
//! `:limit` / `:tz` / `:locale` / `:branch_ids`).
//!
//! **Grain.** A dataset fixes the query grain (one row per order vs per line
//! item vs per payment) and lists only the dimensions/measures valid at that
//! grain, with grain-correct SQL — so e.g. revenue is never fanned out by an
//! item join. Cross-grain item counts on the order grain come from a `LATERAL`
//! per-order sum (the pattern proven in `reports/handlers.rs::branch_waiter_stats`).

use serde_json::{Map, Value};

use super::catalog::{ChartHint, Column, ColumnKind};
use super::executor::{ExecError, ResolvedQuery};

/// A GROUP BY axis.
struct Dim {
    id: &'static str,
    label: &'static str,
    /// SQL expression; may reference `:tz` / `:locale`.
    expr: &'static str,
    kind: ColumnKind,
    /// Join ids this dimension needs (see [`join_clause`]); deduped per query.
    joins: &'static [&'static str],
    /// True for time axes (day/week/month/hour/weekday) — a Line-chart default.
    time: bool,
}

/// An aggregate.
struct Meas {
    id: &'static str,
    label: &'static str,
    expr: &'static str,
    kind: ColumnKind,
    joins: &'static [&'static str],
}

/// A dataset = a grain, its base FROM, and the dims/measures valid at that grain.
struct Dataset {
    id: &'static str,
    /// Base FROM clause; exposes the alias the columns below reference.
    from: &'static str,
    /// Column the branch fence binds to (`o.branch_id`, `im.branch_id`, …).
    branch_col: &'static str,
    /// Timestamp column the period filter (`:from`/`:to`) buckets on.
    time_col: &'static str,
    /// Whether the `status` / `order_type` filters apply — they reference
    /// `orders.status`/`order_type`, so `false` for non-order datasets (waste).
    orders_based: bool,
    /// Extra always-on predicate (e.g. `AND im.type = 'waste'`); "" for none.
    base_pred: &'static str,
    dims: &'static [Dim],
    measures: &'static [Meas],
    default_measures: &'static [&'static str],
}

/// Whitelisted JOIN clauses, keyed by a stable id so the same clause dedupes to
/// one appearance regardless of how many dims/measures request it.
fn join_clause(id: &str) -> &'static str {
    match id {
        "branch" => "LEFT JOIN branches b ON b.id = o.branch_id",
        "waiter" => "LEFT JOIN users w ON w.id = o.waiter_id",
        "cashier" => "LEFT JOIN users t ON t.id = o.teller_id",
        // Per-order item rollup — keeps the order grain (no revenue fan-out).
        "items" => "LEFT JOIN LATERAL (SELECT COALESCE(SUM(oi.quantity),0) AS units, \
                    COUNT(oi.id) AS lines FROM order_items oi WHERE oi.order_id = o.id) it ON true",
        // Item grain (order_items dataset): product → category. `menu_item`
        // must precede `category` (the latter references `mi`).
        "menu_item" => "LEFT JOIN menu_items mi ON mi.id = oi.menu_item_id",
        "category" => "LEFT JOIN categories c ON c.id = mi.category_id",
        // Delivery channel of a delivered order (null for dine-in).
        "delivery" => "LEFT JOIN delivery_orders d ON d.id = o.delivery_order_id",
        // Payments dataset: localized payment-method label.
        "pay_method" => "LEFT JOIN org_payment_methods pm ON pm.name = op.method",
        // Waste dataset (inventory_movements im): branch + ingredient name.
        "branch_im" => "LEFT JOIN branches b ON b.id = im.branch_id",
        "ingredient" => "JOIN org_ingredients ing ON ing.id = im.org_ingredient_id",
        _ => "",
    }
}

// ── Shared time dimensions (both grains hang off `o.created_at`) ──────────────
const D_DAY: Dim = Dim { id: "day", label: "Day", expr: "(o.created_at AT TIME ZONE :tz)::date", kind: ColumnKind::Date, joins: &[], time: true };
const D_WEEK: Dim = Dim { id: "week", label: "Week", expr: "date_trunc('week', o.created_at AT TIME ZONE :tz)::date", kind: ColumnKind::Date, joins: &[], time: true };
const D_MONTH: Dim = Dim { id: "month", label: "Month", expr: "date_trunc('month', o.created_at AT TIME ZONE :tz)::date", kind: ColumnKind::Date, joins: &[], time: true };
const D_HOUR: Dim = Dim { id: "hour", label: "Hour", expr: "to_char(o.created_at AT TIME ZONE :tz, 'HH24:00')", kind: ColumnKind::Label, joins: &[], time: true };
const D_WEEKDAY: Dim = Dim { id: "weekday", label: "Weekday", expr: "trim(to_char(o.created_at AT TIME ZONE :tz, 'Day'))", kind: ColumnKind::Label, joins: &[], time: true };
const D_BRANCH: Dim = Dim { id: "branch", label: "Branch", expr: "b.name", kind: ColumnKind::Label, joins: &["branch"], time: false };
const D_WAITER: Dim = Dim { id: "waiter", label: "Waiter", expr: "w.name", kind: ColumnKind::Label, joins: &["waiter"], time: false };

const ORDERS_DIMS: &[Dim] = &[
    D_DAY, D_WEEK, D_MONTH, D_HOUR, D_WEEKDAY, D_BRANCH, D_WAITER,
    Dim { id: "cashier", label: "Cashier", expr: "t.name", kind: ColumnKind::Label, joins: &["cashier"], time: false },
    Dim { id: "order_type", label: "Order type", expr: "COALESCE(o.order_type, 'unknown')", kind: ColumnKind::Label, joins: &[], time: false },
    Dim { id: "delivery_channel", label: "Channel", expr: "d.channel::text", kind: ColumnKind::Label, joins: &["delivery"], time: false },
    Dim { id: "status", label: "Status", expr: "o.status::text", kind: ColumnKind::Label, joins: &[], time: false },
    Dim { id: "void_reason", label: "Void reason", expr: "COALESCE(o.void_reason::text, 'unspecified')", kind: ColumnKind::Label, joins: &[], time: false },
];

const ORDERS_MEASURES: &[Meas] = &[
    Meas { id: "order_count", label: "Orders", expr: "COUNT(DISTINCT o.id)", kind: ColumnKind::Count, joins: &[] },
    Meas { id: "revenue", label: "Revenue", expr: "COALESCE(SUM(o.total_amount),0)", kind: ColumnKind::Money, joins: &[] },
    Meas { id: "discount_total", label: "Discounts", expr: "COALESCE(SUM(o.discount_amount),0)", kind: ColumnKind::Money, joins: &[] },
    Meas { id: "tax_total", label: "Tax", expr: "COALESCE(SUM(o.tax_amount),0)", kind: ColumnKind::Money, joins: &[] },
    Meas { id: "tip_total", label: "Tips", expr: "COALESCE(SUM(o.tip_amount),0)", kind: ColumnKind::Money, joins: &[] },
    Meas { id: "delivery_fees", label: "Delivery fees", expr: "COALESCE(SUM(o.delivery_fee),0)", kind: ColumnKind::Money, joins: &[] },
    Meas { id: "avg_order_value", label: "Avg order", expr: "COALESCE(AVG(o.total_amount),0)::bigint", kind: ColumnKind::Money, joins: &[] },
    Meas { id: "void_count", label: "Voids", expr: "COUNT(*) FILTER (WHERE o.status = 'voided')", kind: ColumnKind::Count, joins: &[] },
    Meas { id: "line_item_units", label: "Units sold", expr: "COALESCE(SUM(it.units),0)", kind: ColumnKind::Count, joins: &["items"] },
    Meas { id: "distinct_lines", label: "Line items", expr: "COALESCE(SUM(it.lines),0)", kind: ColumnKind::Count, joins: &["items"] },
];

const ITEM_DIMS: &[Dim] = &[
    D_DAY, D_WEEK, D_MONTH, D_BRANCH, D_WAITER,
    Dim { id: "product", label: "Product", expr: "COALESCE(oi.name_translations->>:locale, oi.item_name)", kind: ColumnKind::Label, joins: &[], time: false },
    Dim { id: "category", label: "Category", expr: "COALESCE(NULLIF(c.name_translations->>:locale,''), c.name, 'Uncategorized')", kind: ColumnKind::Label, joins: &["menu_item", "category"], time: false },
    Dim { id: "size", label: "Size", expr: "COALESCE(oi.size_label, '—')", kind: ColumnKind::Label, joins: &[], time: false },
];

const ITEM_MEASURES: &[Meas] = &[
    Meas { id: "line_item_units", label: "Units sold", expr: "COALESCE(SUM(oi.quantity),0)", kind: ColumnKind::Count, joins: &[] },
    Meas { id: "distinct_lines", label: "Line items", expr: "COUNT(oi.id)", kind: ColumnKind::Count, joins: &[] },
    Meas { id: "item_revenue", label: "Revenue", expr: "COALESCE(SUM(oi.line_total),0)", kind: ColumnKind::Money, joins: &[] },
    // Cost / profit / margin — NULL when any line in the group lacks a cost
    // snapshot (matches the product_profit report's honest-null convention).
    Meas { id: "item_cost", label: "Cost", expr: "(CASE WHEN bool_or(oi.line_cost IS NULL) THEN NULL ELSE SUM(oi.line_cost) END)::bigint", kind: ColumnKind::Money, joins: &[] },
    Meas { id: "item_profit", label: "Profit", expr: "(CASE WHEN bool_or(oi.line_cost IS NULL) THEN NULL ELSE SUM(oi.line_total) - SUM(oi.line_cost) END)::bigint", kind: ColumnKind::Money, joins: &[] },
    Meas { id: "margin_pct", label: "Margin %", expr: "(CASE WHEN bool_or(oi.line_cost IS NULL) THEN NULL ELSE ROUND(100.0 * (SUM(oi.line_total) - SUM(oi.line_cost)) / NULLIF(SUM(oi.line_total),0), 1) END)::float8", kind: ColumnKind::Number, joins: &[] },
    Meas { id: "order_count", label: "Orders", expr: "COUNT(DISTINCT o.id)", kind: ColumnKind::Count, joins: &[] },
];

// Payments grain — one row per payment line (`order_payments`). A split-tender
// order contributes several rows; `paid_amount` sums the tender, not order totals.
const PAYMENT_DIMS: &[Dim] = &[
    D_DAY, D_WEEK, D_MONTH, D_BRANCH,
    Dim { id: "payment_method", label: "Method", expr: "COALESCE(NULLIF(pm.label_translations->>:locale,''), pm.name, op.method)", kind: ColumnKind::Label, joins: &["pay_method"], time: false },
];

const PAYMENT_MEASURES: &[Meas] = &[
    Meas { id: "payment_count", label: "Payments", expr: "COUNT(*)", kind: ColumnKind::Count, joins: &[] },
    Meas { id: "paid_amount", label: "Amount", expr: "COALESCE(SUM(op.amount),0)", kind: ColumnKind::Money, joins: &[] },
];

// Waste grain — `inventory_movements` of type 'waste'; the "item" is an
// ingredient. Its own alias (`im`) + time/branch columns, so it needs the
// generalized WHERE (no `o` alias, no status/order_type filters).
const WASTE_DIMS: &[Dim] = &[
    Dim { id: "day", label: "Day", expr: "(im.created_at AT TIME ZONE :tz)::date", kind: ColumnKind::Date, joins: &[], time: true },
    Dim { id: "week", label: "Week", expr: "date_trunc('week', im.created_at AT TIME ZONE :tz)::date", kind: ColumnKind::Date, joins: &[], time: true },
    Dim { id: "month", label: "Month", expr: "date_trunc('month', im.created_at AT TIME ZONE :tz)::date", kind: ColumnKind::Date, joins: &[], time: true },
    Dim { id: "branch", label: "Branch", expr: "b.name", kind: ColumnKind::Label, joins: &["branch_im"], time: false },
    Dim { id: "ingredient", label: "Item", expr: "ing.name", kind: ColumnKind::Label, joins: &["ingredient"], time: false },
];

const WASTE_MEASURES: &[Meas] = &[
    Meas { id: "waste_qty", label: "Quantity", expr: "ROUND(SUM(ABS(im.quantity)),2)::float8", kind: ColumnKind::Number, joins: &[] },
    Meas { id: "waste_cost", label: "Cost", expr: "COALESCE(ROUND(SUM(ABS(im.quantity) * COALESCE(im.unit_cost,0))),0)::bigint", kind: ColumnKind::Money, joins: &[] },
];

const DATASETS: &[Dataset] = &[
    Dataset {
        id: "orders",
        from: "orders o",
        branch_col: "o.branch_id",
        time_col: "o.created_at",
        orders_based: true,
        base_pred: "",
        dims: ORDERS_DIMS,
        measures: ORDERS_MEASURES,
        default_measures: &["order_count", "revenue"],
    },
    Dataset {
        id: "order_items",
        from: "order_items oi JOIN orders o ON o.id = oi.order_id",
        branch_col: "o.branch_id",
        time_col: "o.created_at",
        orders_based: true,
        base_pred: "",
        dims: ITEM_DIMS,
        measures: ITEM_MEASURES,
        default_measures: &["line_item_units", "item_revenue"],
    },
    Dataset {
        id: "payments",
        from: "order_payments op JOIN orders o ON o.id = op.order_id",
        branch_col: "o.branch_id",
        time_col: "o.created_at",
        orders_based: true,
        base_pred: "",
        dims: PAYMENT_DIMS,
        measures: PAYMENT_MEASURES,
        default_measures: &["payment_count", "paid_amount"],
    },
    Dataset {
        id: "waste",
        from: "inventory_movements im",
        branch_col: "im.branch_id",
        time_col: "im.created_at",
        orders_based: false,
        base_pred: "AND im.type = 'waste'",
        dims: WASTE_DIMS,
        measures: WASTE_MEASURES,
        default_measures: &["waste_cost", "waste_qty"],
    },
];

// ── Schema whitelists exposed to the `analytics_query` tool declaration ───────
pub const DATASET_IDS: &[&str] = &["orders", "order_items", "payments", "waste"];
pub const DIMENSION_IDS: &[&str] = &[
    "day", "week", "month", "hour", "weekday", "branch", "waiter", "cashier",
    "order_type", "delivery_channel", "status", "void_reason", "product",
    "category", "size", "payment_method", "ingredient",
];
pub const MEASURE_IDS: &[&str] = &[
    "order_count", "revenue", "discount_total", "tax_total", "tip_total",
    "delivery_fees", "avg_order_value", "void_count", "line_item_units",
    "distinct_lines", "item_revenue", "item_cost", "item_profit", "margin_pct",
    "payment_count", "paid_amount", "waste_qty", "waste_cost",
];
pub const OUTPUT_IDS: &[&str] = &["auto", "table", "bar", "line", "pie"];
pub const STATUS_IDS: &[&str] = &["completed", "not_voided", "all"];
pub const ORDER_TYPE_IDS: &[&str] = &["any", "dine_in", "delivery"];
pub const SORT_DIR_IDS: &[&str] = &["desc", "asc"];
pub const COMPARE_IDS: &[&str] = &["none", "previous_period", "previous_year"];
pub const PER_IDS: &[&str] = &[
    "none", "branch", "waiter", "cashier", "product", "category", "size",
    "payment_method", "ingredient", "order_type", "day", "week", "month",
];

fn bad(msg: impl Into<String>) -> ExecError {
    ExecError::BadArg(msg.into())
}

fn str_arg<'a>(raw: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    raw.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn list_arg<'a>(raw: &'a Map<String, Value>, key: &str) -> Vec<&'a str> {
    raw.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn bool_arg(raw: &Map<String, Value>, key: &str) -> bool {
    match raw.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => false,
    }
}

/// Assemble an `analytics_query` spec (from the model's validated args) into a
/// [`ResolvedQuery`]. Every id is checked against the dataset's whitelist; the
/// SQL is composed only from author-written fragments.
pub fn build(raw: &Map<String, Value>) -> Result<ResolvedQuery, ExecError> {
    let dataset_id = str_arg(raw, "dataset").unwrap_or("orders");
    let ds = DATASETS
        .iter()
        .find(|d| d.id == dataset_id)
        .ok_or_else(|| bad(format!("unknown dataset '{dataset_id}'")))?;

    // Dimensions (deduped, order preserved), each valid for this dataset.
    let mut dims: Vec<&Dim> = Vec::new();
    for id in list_arg(raw, "dimensions") {
        let d = ds
            .dims
            .iter()
            .find(|d| d.id == id)
            .ok_or_else(|| bad(format!("dimension '{id}' isn't available for dataset '{}'", ds.id)))?;
        if !dims.iter().any(|x| x.id == d.id) {
            dims.push(d);
        }
    }

    // Measures — default to the dataset's headline pair when none are named.
    let requested = list_arg(raw, "measures");
    let meas_ids: Vec<&str> = if requested.is_empty() {
        ds.default_measures.to_vec()
    } else {
        requested
    };
    let mut measures: Vec<&Meas> = Vec::new();
    for id in meas_ids {
        let m = ds
            .measures
            .iter()
            .find(|m| m.id == id)
            .ok_or_else(|| bad(format!("measure '{id}' isn't available for dataset '{}'", ds.id)))?;
        if !measures.iter().any(|x| x.id == m.id) {
            measures.push(m);
        }
    }
    if measures.is_empty() {
        return Err(bad("choose at least one measure"));
    }

    // Faceting: `per` must be one of the chosen dimensions.
    let per = str_arg(raw, "per").filter(|s| *s != "none");
    if let Some(p) = per {
        if !dims.iter().any(|d| d.id == p) {
            return Err(bad(format!(
                "'per' ({p}) must be one of the chosen dimensions"
            )));
        }
    }

    // Status + order-type filters — the value only *selects* a whitelisted
    // predicate. They reference `o.*`, so they apply only to order-based datasets
    // (still validated regardless, so a bad value is always rejected).
    let status_raw = match str_arg(raw, "status").unwrap_or("completed") {
        "completed" => "AND o.status = 'completed'",
        "not_voided" => "AND o.status <> 'voided'",
        "all" => "",
        other => return Err(bad(format!("unknown status '{other}'"))),
    };
    let order_type_raw = match str_arg(raw, "order_type").unwrap_or("any") {
        "dine_in" => "AND o.order_type = 'dine_in'",
        "delivery" => "AND o.order_type = 'delivery'",
        "any" => "",
        other => return Err(bad(format!("unknown order_type '{other}'"))),
    };
    let (status_pred, order_type_pred) = if ds.orders_based {
        (status_raw, order_type_raw)
    } else {
        ("", "")
    };

    // Collect joins from dims + measures (ordered, deduped by id).
    let mut join_ids: Vec<&str> = Vec::new();
    for src in dims.iter().flat_map(|d| d.joins).chain(measures.iter().flat_map(|m| m.joins)) {
        if !join_ids.contains(src) {
            join_ids.push(*src);
        }
    }
    let joins = join_ids
        .iter()
        .map(|id| join_clause(id))
        .collect::<Vec<_>>()
        .join(" ");

    // SELECT list + output columns (dims first, then measures).
    let mut select: Vec<String> = Vec::new();
    let mut columns: Vec<Column> = Vec::new();
    for d in &dims {
        select.push(format!("{} AS {}", d.expr, d.id));
        columns.push(Column { key: d.id, label: d.label, kind: d.kind });
    }
    for m in &measures {
        select.push(format!("{} AS {}", m.expr, m.id));
        columns.push(Column { key: m.id, label: m.label, kind: m.kind });
    }

    // Branch fence + period, keyed off the dataset's own columns, plus any
    // always-on base predicate (e.g. waste's `im.type = 'waste'`).
    let where_clause = format!(
        "WHERE {branch} = ANY(:branch_ids) \
         AND (:from::timestamptz IS NULL OR {time} >= :from) \
         AND (:to::timestamptz IS NULL OR {time} <= :to) {base}",
        branch = ds.branch_col,
        time = ds.time_col,
        base = ds.base_pred,
    );
    // Ordinal GROUP BY over the dimension columns (positions 1..=dims.len()).
    let group_by = if dims.is_empty() {
        String::new()
    } else {
        format!(
            "GROUP BY {}",
            (1..=dims.len()).map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
        )
    };
    let first_meas = measures[0];

    // Sort: which measure orders the result + direction. `asc` unlocks "least /
    // worst / slowest / cheapest" questions. Defaults to the first measure, desc.
    let sort_meas: &Meas = match str_arg(raw, "sort") {
        Some(id) => measures
            .iter()
            .copied()
            .find(|m| m.id == id)
            .ok_or_else(|| bad(format!("'sort' ({id}) must be one of the chosen measures")))?,
        None => first_meas,
    };
    let dir = if str_arg(raw, "sort_dir") == Some("asc") {
        "ASC"
    } else {
        "DESC"
    };
    // Optional HAVING threshold on the sort measure ("products selling ≥ 100").
    let having_min = raw
        .get("having_min")
        .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
        .unwrap_or(0);
    let having = if having_min > 0 {
        format!("HAVING {} >= :having_min", sort_meas.expr)
    } else {
        String::new()
    };

    let share = bool_arg(raw, "share");
    let cumulative = bool_arg(raw, "cumulative");

    // ── Period-over-period comparison — its own SQL shape (cur/prev CTEs). ──
    // Compares the SORT measure this period vs the previous equal-length window
    // (or the same window a year earlier). Not a time series: the breakdown must
    // be by entity (branch/product/…) or overall, so the join keys align.
    if let Some(cmp) = str_arg(raw, "compare").filter(|s| *s != "none") {
        if per.is_some() {
            return Err(bad("'compare' can't combine with per-group ranking (per)"));
        }
        if dims.iter().any(|d| d.time) {
            return Err(bad(
                "'compare' is for totals or entity breakdowns, not a time series — drop the time dimension",
            ));
        }
        if str_arg(raw, "from").is_none() || str_arg(raw, "to").is_none() {
            return Err(bad("'compare' needs an explicit date range (both from and to)"));
        }
        let prev_period = match cmp {
            "previous_period" => {
                format!("AND {t} >= (:from - (:to - :from)) AND {t} < :from", t = ds.time_col)
            }
            "previous_year" => format!(
                "AND {t} >= (:from - interval '1 year') AND {t} <= (:to - interval '1 year')",
                t = ds.time_col
            ),
            other => return Err(bad(format!("unknown compare '{other}'"))),
        };
        let cur_period = format!("AND {t} >= :from AND {t} <= :to", t = ds.time_col);
        let cte = |period: &str| {
            format!(
                "SELECT {sel} FROM {from} {joins} WHERE {branch} = ANY(:branch_ids) {period} {status} {ot} {base} {group_by}",
                sel = select.join(", "),
                from = ds.from,
                branch = ds.branch_col,
                status = status_pred,
                ot = order_type_pred,
                base = ds.base_pred,
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
        let sql = format!(
            "WITH cur AS ({cur}), prev AS ({prev}) \
             SELECT cur.*, prev.{s} AS prev, \
                    ROUND(100.0 * (cur.{s} - prev.{s}) / NULLIF(prev.{s}, 0), 1) AS change_pct \
             FROM {join} ORDER BY {s} {dir} LIMIT :limit",
            cur = cte(&cur_period),
            prev = cte(&prev_period),
            s = sort_meas.id,
        );
        let mut cols = columns.clone();
        cols.push(Column { key: "prev", label: "Previous", kind: sort_meas.kind });
        cols.push(Column { key: "change_pct", label: "Change %", kind: ColumnKind::Number });
        return Ok(ResolvedQuery { sql, columns: cols, chart: ChartHint::Table, facet_by: None });
    }

    let (sql, facet_by) = if let Some(p) = per {
        if share || cumulative {
            return Err(bad("'share'/'cumulative' can't combine with per-group ranking (per)"));
        }
        let per_dim = dims.iter().find(|d| d.id == p).expect("validated above");
        let mut sel = select.clone();
        sel.push(format!(
            "ROW_NUMBER() OVER (PARTITION BY {} ORDER BY {} {dir}) AS rank",
            per_dim.expr, sort_meas.expr,
        ));
        columns.push(Column { key: "rank", label: "Rank", kind: ColumnKind::Count });
        let sql = format!(
            "SELECT * FROM (SELECT {sel} FROM {from} {joins} {where_clause} {status} {ot} {group_by} {having}) ranked \
             WHERE rank <= :top_per ORDER BY {p}, rank LIMIT :limit",
            sel = sel.join(", "),
            from = ds.from,
            status = status_pred,
            ot = order_type_pred,
        );
        (sql, Some(p.to_string()))
    } else if share || cumulative {
        // Window transforms over the full aggregation: share = each row's % of
        // the grand total; cumulative = running total in time order.
        let inner = format!(
            "SELECT {sel} FROM {from} {joins} {where_clause} {status} {ot} {group_by} {having}",
            sel = select.join(", "),
            from = ds.from,
            status = status_pred,
            ot = order_type_pred,
        );
        let mut extra: Vec<String> = Vec::new();
        if share {
            extra.push(format!(
                "ROUND(100.0 * base.{pm} / NULLIF(SUM(base.{pm}) OVER (), 0), 1) AS share_pct",
                pm = sort_meas.id
            ));
            columns.push(Column { key: "share_pct", label: "% of total", kind: ColumnKind::Number });
        }
        let time_dim = dims.iter().find(|d| d.time);
        if cumulative {
            let td = time_dim
                .ok_or_else(|| bad("'cumulative' needs a time dimension (day / week / month)"))?;
            extra.push(format!(
                "SUM(base.{pm}) OVER (ORDER BY base.{tid}) AS cumulative",
                pm = sort_meas.id,
                tid = td.id
            ));
            columns.push(Column { key: "cumulative", label: "Cumulative", kind: sort_meas.kind });
        }
        // A running total reads in time order; otherwise keep the requested sort.
        let order = match (cumulative, time_dim) {
            (true, Some(td)) => format!("{} ASC", td.id),
            _ => format!("{} {dir}", sort_meas.id),
        };
        let sql = format!(
            "SELECT base.*, {extra} FROM ({inner}) base ORDER BY {order} LIMIT :limit",
            extra = extra.join(", "),
        );
        (sql, None)
    } else {
        let sql = format!(
            "SELECT {sel} FROM {from} {joins} {where_clause} {status} {ot} {group_by} {having} ORDER BY {order} {dir} LIMIT :limit",
            sel = select.join(", "),
            from = ds.from,
            status = status_pred,
            ot = order_type_pred,
            order = sort_meas.id,
        );
        (sql, None)
    };

    // Chart: an explicit `output` always wins. Otherwise: a faceted query
    // defaults to one table per group (what "…in each branch" usually wants);
    // a single time axis → Line; a single categorical axis → Bar; anything
    // multi-dimensional → Table.
    let non_facet: Vec<&&Dim> = dims.iter().filter(|d| Some(d.id) != per).collect();
    let chart = match str_arg(raw, "output").unwrap_or("auto") {
        "table" => ChartHint::Table,
        "bar" => ChartHint::Bar,
        "line" => ChartHint::Line,
        "pie" => ChartHint::Pie,
        _ if per.is_some() => ChartHint::Table,
        _ if non_facet.len() == 1 => {
            if non_facet[0].time {
                ChartHint::Line
            } else {
                ChartHint::Bar
            }
        }
        _ => ChartHint::Table,
    };

    Ok(ResolvedQuery { sql, columns, chart, facet_by })
}

#[cfg(test)]
mod tests {
    use super::build;

    fn args(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn rejects_unknown_ids_never_injecting() {
        // Hallucinated / hostile ids are errors, never interpolated into SQL.
        assert!(
            build(&args(serde_json::json!({
                "dataset": "orders", "dimensions": ["day; DROP TABLE orders"]
            })))
            .is_err()
        );
        assert!(
            build(&args(serde_json::json!({
                "dataset": "orders", "measures": ["revenue) FROM users --"]
            })))
            .is_err()
        );
        assert!(build(&args(serde_json::json!({ "dataset": "users" }))).is_err());
        // `product` is item-grain only — invalid on the `orders` dataset.
        assert!(
            build(&args(serde_json::json!({
                "dataset": "orders", "dimensions": ["product"]
            })))
            .is_err()
        );
    }

    #[test]
    fn faceting_sets_facet_by_and_rank() {
        let r = build(&args(serde_json::json!({
            "dataset": "order_items",
            "dimensions": ["branch", "product"],
            "measures": ["line_item_units"],
            "per": "branch",
            "top_per": 1
        })))
        .unwrap();
        assert_eq!(r.facet_by.as_deref(), Some("branch"));
        assert!(r.columns.iter().any(|c| c.key == "rank"));
        assert!(r.sql.contains("ROW_NUMBER() OVER"));
    }

    #[test]
    fn per_must_be_a_chosen_dimension() {
        // Faceting by a dimension that wasn't selected is rejected.
        assert!(
            build(&args(serde_json::json!({
                "dataset": "orders",
                "dimensions": ["day"],
                "measures": ["order_count"],
                "per": "branch"
            })))
            .is_err()
        );
    }

    #[test]
    fn sort_and_threshold_shape_the_sql() {
        let r = build(&args(serde_json::json!({
            "dataset": "order_items",
            "dimensions": ["product"],
            "measures": ["line_item_units"],
            "sort": "line_item_units",
            "sort_dir": "asc",
            "having_min": 5
        })))
        .unwrap();
        assert!(r.sql.contains("ORDER BY line_item_units ASC"));
        assert!(r.sql.contains("HAVING"));
        assert!(r.sql.contains(":having_min"));
        // 'sort' must be one of the chosen measures.
        assert!(
            build(&args(serde_json::json!({
                "dataset": "orders",
                "measures": ["revenue"],
                "sort": "tip_total"
            })))
            .is_err()
        );
    }

    #[test]
    fn waste_dataset_uses_its_own_alias_no_order_filters() {
        let r = build(&args(serde_json::json!({
            "dataset": "waste",
            "dimensions": ["ingredient"],
            "measures": ["waste_cost"]
        })))
        .unwrap();
        assert!(r.sql.contains("inventory_movements im"));
        assert!(r.sql.contains("im.type = 'waste'"));
        assert!(r.sql.contains("im.branch_id = ANY(:branch_ids)"));
        // Not order-based → the status/order_type predicates must be absent.
        assert!(!r.sql.contains("o.status"));
        assert!(!r.sql.contains("o.order_type"));
    }

    #[test]
    fn compare_builds_cur_prev_ctes() {
        let r = build(&args(serde_json::json!({
            "dataset": "orders",
            "dimensions": ["branch"],
            "measures": ["revenue"],
            "compare": "previous_period",
            "from": "2026-07-01",
            "to": "2026-07-07"
        })))
        .unwrap();
        assert!(r.sql.contains("WITH cur AS"));
        assert!(r.sql.contains("prev AS"));
        assert!(r.sql.contains("LEFT JOIN prev USING (branch)"));
        assert!(r.columns.iter().any(|c| c.key == "prev"));
        assert!(r.columns.iter().any(|c| c.key == "change_pct"));
    }

    #[test]
    fn compare_rejects_time_dim_and_missing_range() {
        // A time breakdown can't be period-compared (the buckets shift).
        assert!(
            build(&args(serde_json::json!({
                "dataset": "orders", "dimensions": ["day"], "measures": ["revenue"],
                "compare": "previous_period", "from": "2026-07-01", "to": "2026-07-07"
            })))
            .is_err()
        );
        // Comparison needs an explicit window.
        assert!(
            build(&args(serde_json::json!({
                "dataset": "orders", "measures": ["revenue"], "compare": "previous_year"
            })))
            .is_err()
        );
    }

    #[test]
    fn share_and_cumulative_add_window_columns() {
        let s = build(&args(serde_json::json!({
            "dataset": "orders", "dimensions": ["branch"], "measures": ["revenue"], "share": true
        })))
        .unwrap();
        assert!(s.columns.iter().any(|c| c.key == "share_pct"));
        assert!(s.sql.contains("OVER ()"));

        let c = build(&args(serde_json::json!({
            "dataset": "orders", "dimensions": ["day"], "measures": ["revenue"], "cumulative": true
        })))
        .unwrap();
        assert!(c.columns.iter().any(|c| c.key == "cumulative"));
        assert!(c.sql.contains("OVER (ORDER BY base.day)"));

        // Cumulative needs a time axis to run along.
        assert!(
            build(&args(serde_json::json!({
                "dataset": "orders", "dimensions": ["branch"], "measures": ["revenue"], "cumulative": true
            })))
            .is_err()
        );
    }
}
