//! The semantic layer: the complete, authoritative description of what this
//! system can measure.
//!
//! Everything downstream — the metrics HTTP API, dashboard widgets, and the AI
//! agent's tool surface — is generated from this registry. Nothing else in the
//! codebase is allowed to describe a metric.
//!
//! # The security property
//!
//! Every SQL fragment here is an author-written `&'static str`. A caller (a
//! merchant, or a language model) supplies only **ids** that *select* fragments.
//! There is no path by which caller input becomes SQL text: a hostile or
//! hallucinated argument can at worst name a fragment that already exists, or
//! name nothing and be rejected. Values that genuinely vary (dates, limits,
//! thresholds) travel as bound parameters. See [`super::compile`] for assembly
//! and [`super::execute`] for the runtime envelope.
//!
//! # Grain
//!
//! A [`Dataset`] fixes the grain of a query — one row per order, per line item,
//! per tender, per stock movement — and publishes only the dimensions and
//! measures that are *correct at that grain*. This is what stops the classic
//! analytics bug where revenue is fanned out by a line-item join and silently
//! multiplied. Cross-grain figures (units sold on the order grain) come from a
//! `LATERAL` per-order rollup instead of a fan-out join.

use super::types::{ColumnKind, Viz};

/// A whitelisted JOIN, emitted only when a selected dimension or measure needs
/// it. Ordering within a dataset's `joins` list is the *dependency* order (a
/// join may reference an alias introduced by an earlier one), and the compiler
/// preserves it regardless of the order ids are requested in.
#[derive(Debug)]
pub struct Join {
    pub id: &'static str,
    pub sql: &'static str,
}

/// A GROUP BY axis.
#[derive(Debug)]
pub struct Dim {
    pub id: &'static str,
    pub label: &'static str,
    /// SQL expression. May reference `:tz` (bucket in the merchant's timezone)
    /// and `:locale` (pick a translated label).
    pub expr: &'static str,
    pub kind: ColumnKind,
    /// Ids from the dataset's `joins` this expression depends on.
    pub joins: &'static [&'static str],
    /// True for time axes (day/week/month/hour/weekday). Drives [`Viz`]
    /// selection, ordering, and the `cumulative` transform.
    pub time: bool,
}

/// An aggregate.
#[derive(Debug)]
pub struct Meas {
    pub id: &'static str,
    pub label: &'static str,
    pub expr: &'static str,
    pub kind: ColumnKind,
    pub joins: &'static [&'static str],
    /// One line explaining exactly what it counts — shown in the widget picker
    /// and handed to the model, so "revenue" is never guessed at.
    pub help: &'static str,
}

/// One allowed value of a [`Filter`], paired with the predicate it selects.
#[derive(Debug)]
pub struct FilterOpt {
    pub value: &'static str,
    /// Predicate fragment, ANDed into the WHERE clause. Empty = no restriction.
    pub sql: &'static str,
}

/// A dataset-scoped filter with a closed set of values. Because the value only
/// ever *selects* a fragment, filters are as injection-proof as dimensions.
#[derive(Debug)]
pub struct Filter {
    pub id: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub options: &'static [FilterOpt],
    /// Applied when the caller names no value. Chosen so the safe, expected
    /// reading is the default (e.g. sales exclude voids unless asked otherwise).
    pub default: &'static str,
}

impl Filter {
    pub fn option(&self, value: &str) -> Option<&'static FilterOpt> {
        self.options.iter().find(|o| o.value == value)
    }
    pub fn default_sql(&self) -> &'static str {
        self.option(self.default).map(|o| o.sql).unwrap_or("")
    }
    pub fn values(&self) -> Vec<&'static str> {
        self.options.iter().map(|o| o.value).collect()
    }
}

/// A dataset = a grain, its base FROM, and everything valid at that grain.
#[derive(Debug)]
pub struct Dataset {
    pub id: &'static str,
    pub title: &'static str,
    /// What one row of the underlying relation *is*. Handed to the model
    /// verbatim; ambiguity here is the main cause of wrong routing.
    pub help: &'static str,
    /// Base FROM clause, exposing the aliases the expressions below reference.
    pub from: &'static str,
    /// Column the branch fence binds against.
    pub branch_col: &'static str,
    /// Column the reporting period filters on.
    pub time_col: &'static str,
    /// True when `time_col` is a `date`, not a `timestamptz` — the period
    /// bounds are then converted to local dates before comparison.
    pub time_is_date: bool,
    /// Always-on predicate for this dataset (e.g. only finalized stocktakes).
    pub base_pred: &'static str,
    pub joins: &'static [Join],
    pub dims: &'static [Dim],
    pub measures: &'static [Meas],
    pub filters: &'static [Filter],
    /// The headline measures used when a caller names none.
    pub default_measures: &'static [&'static str],
    /// Visualization to fall back to for this dataset's breakdowns.
    pub default_viz: Viz,
}

impl Dataset {
    // These take `&'static self` rather than `&self`: every `Dataset` reachable
    // at runtime is an element of the `DATASETS` static, so the borrow is
    // genuinely 'static and the returned fragments can be held for the life of a
    // request without cloning — no lifetime laundering required.
    pub fn dim(&'static self, id: &str) -> Option<&'static Dim> {
        self.dims.iter().find(|d| d.id == id)
    }
    pub fn measure(&'static self, id: &str) -> Option<&'static Meas> {
        self.measures.iter().find(|m| m.id == id)
    }
    pub fn filter(&'static self, id: &str) -> Option<&'static Filter> {
        self.filters.iter().find(|f| f.id == id)
    }
    pub fn join_sql(&'static self, id: &str) -> Option<&'static str> {
        self.joins.iter().find(|j| j.id == id).map(|j| j.sql)
    }
}

/// Look up a dataset by id.
pub fn dataset(id: &str) -> Option<&'static Dataset> {
    DATASETS.iter().find(|d| d.id == id)
}

/// Joins that resolve a row to a PERSON — they all reach `users`.
///
/// A dimension hanging off one of these produces a staff member's real name,
/// which is why [`PERSONAL_DIMENSIONS`] exists and why a test below derives one
/// list from the other rather than trusting them to stay in step by hand.
pub const PERSON_JOINS: &[&str] = &["waiter", "cashier", "teller", "employee"];

/// Dimensions whose values are a person's name.
///
/// Derived from [`crate::analytics::entities::ENTITY_KINDS`] rather than
/// maintained here. This used to be a hand-written list beside the join graph,
/// which meant two places had to agree about which dimensions name people — and
/// the failure mode of them disagreeing is staff names reaching a language
/// model.
pub fn personal_dimensions() -> Vec<&'static str> {
    super::entities::ENTITY_KINDS
        .iter()
        .filter(|k| k.personal)
        .flat_map(|k| k.dimensions.iter().copied())
        .collect()
}

/// True when a result column carries a person's name.
pub fn is_personal_dimension(column_key: &str) -> bool {
    super::entities::is_personal_dimension(column_key)
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared time dimensions
//
// Declared per family because the bucketed column differs by dataset alias.
// `AT TIME ZONE :tz` is what makes "yesterday" mean the merchant's yesterday
// rather than UTC's — the same convention `src/reports` uses.
// ─────────────────────────────────────────────────────────────────────────────

/// Splices the standard time dimensions in front of a dataset's own dimensions.
macro_rules! dims_with_time {
    ($col:expr, [$($rest:expr),* $(,)?]) => {
        &[
            Dim { id: "day", label: "Day",
                  expr: concat!("(", $col, " AT TIME ZONE :tz)::date"),
                  kind: ColumnKind::Date, joins: &[], time: true },
            Dim { id: "week", label: "Week",
                  expr: concat!("date_trunc('week', ", $col, " AT TIME ZONE :tz)::date"),
                  kind: ColumnKind::Date, joins: &[], time: true },
            Dim { id: "month", label: "Month",
                  expr: concat!("date_trunc('month', ", $col, " AT TIME ZONE :tz)::date"),
                  kind: ColumnKind::Date, joins: &[], time: true },
            Dim { id: "hour", label: "Hour",
                  expr: concat!("to_char(", $col, " AT TIME ZONE :tz, 'HH24:00')"),
                  kind: ColumnKind::Label, joins: &[], time: true },
            Dim { id: "weekday", label: "Weekday",
                  expr: concat!("trim(to_char(", $col, " AT TIME ZONE :tz, 'Day'))"),
                  kind: ColumnKind::Label, joins: &[], time: true },
            $($rest),*
        ]
    };
}

// ── Shared filter option sets ────────────────────────────────────────────────

/// Order status. `sold` is the default everywhere: a voided or refunded order
/// is not revenue, and defaulting to "all" is how naive dashboards overstate.
const F_ORDER_STATUS: Filter = Filter {
    id: "status",
    label: "Order status",
    help: "Which orders count. 'sold' (default) excludes voided and refunded orders.",
    options: &[
        FilterOpt {
            value: "sold",
            sql: "AND o.status NOT IN ('voided','refunded')",
        },
        FilterOpt {
            value: "completed",
            sql: "AND o.status = 'completed'",
        },
        FilterOpt {
            value: "voided",
            sql: "AND o.status = 'voided'",
        },
        FilterOpt {
            value: "refunded",
            sql: "AND o.status = 'refunded'",
        },
        FilterOpt {
            value: "open",
            sql: "AND o.status IN ('pending','preparing','ready')",
        },
        FilterOpt {
            value: "all",
            sql: "",
        },
    ],
    default: "sold",
};

const F_ORDER_TYPE: Filter = Filter {
    id: "order_type",
    label: "Order type",
    help: "Dine-in versus delivery orders.",
    options: &[
        FilterOpt {
            value: "any",
            sql: "",
        },
        FilterOpt {
            value: "dine_in",
            sql: "AND o.order_type = 'dine_in'",
        },
        FilterOpt {
            value: "delivery",
            sql: "AND o.order_type = 'delivery'",
        },
    ],
    default: "any",
};

const F_DELIVERY_CHANNEL: Filter = Filter {
    id: "channel",
    label: "Delivery channel",
    help: "For delivery orders: in-mall, outside, an umbrella aggregator, or pickup.",
    options: &[
        FilterOpt {
            value: "any",
            sql: "",
        },
        FilterOpt {
            value: "in_mall",
            sql: "AND d.channel = 'in_mall'",
        },
        FilterOpt {
            value: "outside",
            sql: "AND d.channel = 'outside'",
        },
        FilterOpt {
            value: "umbrella",
            sql: "AND d.channel = 'umbrella'",
        },
        FilterOpt {
            value: "pickup",
            sql: "AND d.channel = 'pickup'",
        },
    ],
    default: "any",
};

const F_DISCOUNTED: Filter = Filter {
    id: "discounted",
    label: "Discounted only",
    help: "Restrict to orders that carried a discount.",
    options: &[
        FilterOpt {
            value: "any",
            sql: "",
        },
        FilterOpt {
            value: "yes",
            sql: "AND o.discount_amount > 0",
        },
        FilterOpt {
            value: "no",
            sql: "AND o.discount_amount = 0",
        },
    ],
    default: "any",
};

// ── Dataset: orders (one row per order) ──────────────────────────────────────

const ORDERS_JOINS: &[Join] = &[
    Join {
        id: "branch",
        sql: "LEFT JOIN branches b ON b.id = o.branch_id",
    },
    Join {
        id: "waiter",
        sql: "LEFT JOIN users w ON w.id = o.waiter_id",
    },
    Join {
        id: "cashier",
        sql: "LEFT JOIN users t ON t.id = o.teller_id",
    },
    Join {
        id: "delivery",
        sql: "LEFT JOIN delivery_orders d ON d.id = o.delivery_order_id",
    },
    Join {
        id: "discount",
        sql: "LEFT JOIN discounts dc ON dc.id = o.discount_id",
    },
    // Per-order line rollup. A LATERAL keeps the order grain intact, so revenue
    // is never multiplied by the number of lines — the fan-out bug this whole
    // grain system exists to prevent.
    Join {
        id: "items",
        sql: "LEFT JOIN LATERAL (SELECT COALESCE(SUM(oi.quantity),0) AS units, \
              COUNT(oi.id) AS lines, SUM(oi.line_cost) AS cost, \
              bool_or(oi.line_cost IS NULL) AS cost_missing \
              FROM order_items oi WHERE oi.order_id = o.id) it ON true",
    },
];

const ORDERS_MEASURES: &[Meas] = &[
    Meas {
        id: "order_count",
        label: "Orders",
        expr: "COUNT(DISTINCT o.id)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Number of orders.",
    },
    Meas {
        id: "revenue",
        label: "Revenue",
        expr: "COALESCE(SUM(o.total_amount),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Gross revenue: order totals after discount, including tax and delivery fee.",
    },
    Meas {
        id: "net_revenue",
        label: "Net revenue",
        expr: "COALESCE(SUM(o.total_amount - o.tax_amount - o.delivery_fee),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Revenue excluding tax and delivery fees — the merchant's own take.",
    },
    Meas {
        id: "subtotal",
        label: "Subtotal",
        expr: "COALESCE(SUM(o.subtotal),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Sum of line totals before discount and tax.",
    },
    Meas {
        id: "discount_total",
        label: "Discounts",
        expr: "COALESCE(SUM(o.discount_amount),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Total discount given away.",
    },
    Meas {
        id: "discount_rate",
        label: "Discount %",
        expr: "ROUND(100.0 * SUM(o.discount_amount) / NULLIF(SUM(o.subtotal),0), 1)::float8",
        kind: ColumnKind::Number,
        joins: &[],
        help: "Discounts as a percentage of subtotal.",
    },
    Meas {
        id: "tax_total",
        label: "Tax",
        expr: "COALESCE(SUM(o.tax_amount),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Tax collected.",
    },
    Meas {
        id: "tip_total",
        label: "Tips",
        expr: "COALESCE(SUM(o.tip_amount),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Tips collected.",
    },
    Meas {
        id: "tip_rate",
        label: "Tip %",
        expr: "ROUND(100.0 * SUM(o.tip_amount) / NULLIF(SUM(o.total_amount),0), 1)::float8",
        kind: ColumnKind::Number,
        joins: &[],
        help: "Tips as a percentage of revenue — whether service is actually being rewarded, \
               independent of how busy the period was.",
    },
    Meas {
        id: "tipped_order_count",
        label: "Tipped orders",
        expr: "COUNT(*) FILTER (WHERE COALESCE(o.tip_amount,0) > 0)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Orders that left a tip at all.",
    },
    Meas {
        id: "avg_tip",
        label: "Avg tip",
        // Averaged over TIPPED orders only. Dividing by every order answers a
        // different question and reads as a collapse in tipping whenever a
        // quiet untipped shift lands in the period.
        expr: "COALESCE(AVG(o.tip_amount) FILTER (WHERE COALESCE(o.tip_amount,0) > 0),0)::bigint",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Average tip on the orders that were tipped — not diluted by the ones that \
               were not.",
    },
    Meas {
        id: "cash_tip_total",
        label: "Cash tips",
        expr: "COALESCE(SUM(o.tip_amount) FILTER (WHERE o.tip_is_cash),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Tips taken in cash. These leave the drawer rather than the bank, so they \
               matter to the shift count as well as to payroll.",
    },
    Meas {
        id: "refund_count",
        label: "Refunds",
        expr: "COUNT(*) FILTER (WHERE o.status = 'refunded')",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Orders refunded. Needs the status filter set to 'all' or 'refunded' to be \
               non-zero, because 'sold' excludes them.",
    },
    Meas {
        id: "refund_amount",
        label: "Refunded value",
        expr: "COALESCE(SUM(o.total_amount) FILTER (WHERE o.status = 'refunded'),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Value of refunded orders.",
    },
    Meas {
        id: "delivery_fees",
        label: "Delivery fees",
        expr: "COALESCE(SUM(o.delivery_fee),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Delivery fees charged.",
    },
    Meas {
        id: "avg_order_value",
        label: "Avg order",
        expr: "COALESCE(AVG(o.total_amount),0)::bigint",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Average order total (average ticket).",
    },
    Meas {
        id: "void_count",
        label: "Voids",
        expr: "COUNT(*) FILTER (WHERE o.status = 'voided')",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Orders voided. Needs status filter 'all' or 'voided' to be non-zero.",
    },
    Meas {
        id: "void_amount",
        label: "Voided value",
        expr: "COALESCE(SUM(o.total_amount) FILTER (WHERE o.status = 'voided'),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Value of voided orders.",
    },
    Meas {
        id: "units_sold",
        label: "Units sold",
        expr: "COALESCE(SUM(it.units),0)",
        kind: ColumnKind::Count,
        joins: &["items"],
        help: "Total item quantity across these orders.",
    },
    Meas {
        id: "line_count",
        label: "Line items",
        expr: "COALESCE(SUM(it.lines),0)",
        kind: ColumnKind::Count,
        joins: &["items"],
        help: "Number of order lines.",
    },
    Meas {
        id: "basket_size",
        label: "Items / order",
        expr: "ROUND(AVG(it.units), 2)::float8",
        kind: ColumnKind::Number,
        joins: &["items"],
        help: "Average number of items per order.",
    },
    // Cost/profit are NULL for any group containing a line with no cost
    // snapshot — an honest null beats a silently understated cost.
    Meas {
        id: "cogs",
        label: "Cost",
        expr: "(CASE WHEN bool_or(it.cost_missing) THEN NULL ELSE SUM(it.cost) END)::bigint",
        kind: ColumnKind::Money,
        joins: &["items"],
        help: "Cost of goods sold. NULL if any line lacks a cost snapshot.",
    },
    Meas {
        id: "profit",
        label: "Profit",
        expr: "(CASE WHEN bool_or(it.cost_missing) THEN NULL ELSE SUM(o.total_amount - o.tax_amount - o.delivery_fee) - SUM(it.cost) END)::bigint",
        kind: ColumnKind::Money,
        joins: &["items"],
        help: "Net revenue minus cost of goods. NULL if any cost is missing.",
    },
    Meas {
        id: "margin_pct",
        label: "Margin %",
        expr: "(CASE WHEN bool_or(it.cost_missing) THEN NULL ELSE ROUND(100.0 * (SUM(o.total_amount - o.tax_amount - o.delivery_fee) - SUM(it.cost)) / NULLIF(SUM(o.total_amount - o.tax_amount - o.delivery_fee),0), 1) END)::float8",
        kind: ColumnKind::Number,
        joins: &["items"],
        help: "Profit as a percentage of net revenue.",
    },
    Meas {
        id: "unique_customers",
        label: "Customers",
        expr: "COUNT(DISTINCT NULLIF(o.customer_name,''))",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Distinct named customers (only orders that captured a name).",
    },
];

const ORDERS_DIMS: &[Dim] = dims_with_time!(
    "o.created_at",
    [
        Dim {
            id: "branch",
            label: "Branch",
            expr: "b.name",
            kind: ColumnKind::Label,
            joins: &["branch"],
            time: false
        },
        Dim {
            id: "waiter",
            label: "Waiter",
            expr: "COALESCE(w.name, 'Unassigned')",
            kind: ColumnKind::Label,
            joins: &["waiter"],
            time: false
        },
        Dim {
            id: "cashier",
            label: "Cashier",
            expr: "COALESCE(t.name, 'Unknown')",
            kind: ColumnKind::Label,
            joins: &["cashier"],
            time: false
        },
        Dim {
            id: "order_type",
            label: "Order type",
            expr: "COALESCE(o.order_type,'unknown')",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
        Dim {
            id: "delivery_channel",
            label: "Channel",
            expr: "COALESCE(d.channel::text,'dine_in')",
            kind: ColumnKind::Label,
            joins: &["delivery"],
            time: false
        },
        Dim {
            id: "payment_method",
            label: "Payment method",
            expr: "o.payment_method",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
        Dim {
            id: "status",
            label: "Status",
            expr: "o.status::text",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
        Dim {
            id: "void_reason",
            label: "Void reason",
            expr: "COALESCE(o.void_reason::text,'unspecified')",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
        Dim {
            id: "discount_name",
            label: "Discount",
            expr: "COALESCE(NULLIF(dc.name_translations->>:locale,''), dc.name, 'No discount')",
            kind: ColumnKind::Label,
            joins: &["discount"],
            time: false
        },
    ]
);

// ── Dataset: order_items (one row per order line) ────────────────────────────

const ITEM_JOINS: &[Join] = &[
    Join {
        id: "branch",
        sql: "LEFT JOIN branches b ON b.id = o.branch_id",
    },
    Join {
        id: "waiter",
        sql: "LEFT JOIN users w ON w.id = o.waiter_id",
    },
    Join {
        id: "menu_item",
        sql: "LEFT JOIN menu_items mi ON mi.id = oi.menu_item_id",
    },
    // Depends on `mi` — declared after it, and the compiler preserves this order.
    Join {
        id: "category",
        sql: "LEFT JOIN categories c ON c.id = mi.category_id",
    },
    Join {
        id: "bundle",
        sql: "LEFT JOIN bundles bn ON bn.id = oi.bundle_id",
    },
];

const ITEM_MEASURES: &[Meas] = &[
    Meas {
        id: "units_sold",
        label: "Units sold",
        expr: "COALESCE(SUM(oi.quantity),0)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Total quantity sold.",
    },
    Meas {
        id: "line_count",
        label: "Line items",
        expr: "COUNT(oi.id)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Number of order lines.",
    },
    Meas {
        id: "item_revenue",
        label: "Revenue",
        expr: "COALESCE(SUM(oi.line_total),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Line revenue before order-level discount and tax.",
    },
    Meas {
        id: "avg_unit_price",
        label: "Avg price",
        expr: "COALESCE(AVG(oi.unit_price),0)::bigint",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Average selling price per unit.",
    },
    Meas {
        id: "item_cost",
        label: "Cost",
        expr: "(CASE WHEN bool_or(oi.line_cost IS NULL) THEN NULL ELSE SUM(oi.line_cost) END)::bigint",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Cost of goods for these lines. NULL if any line lacks a cost snapshot.",
    },
    Meas {
        id: "item_profit",
        label: "Profit",
        expr: "(CASE WHEN bool_or(oi.line_cost IS NULL) THEN NULL ELSE SUM(oi.line_total) - SUM(oi.line_cost) END)::bigint",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Line revenue minus line cost.",
    },
    Meas {
        id: "margin_pct",
        label: "Margin %",
        expr: "(CASE WHEN bool_or(oi.line_cost IS NULL) THEN NULL ELSE ROUND(100.0 * (SUM(oi.line_total) - SUM(oi.line_cost)) / NULLIF(SUM(oi.line_total),0), 1) END)::float8",
        kind: ColumnKind::Number,
        joins: &[],
        help: "Profit as a percentage of line revenue.",
    },
    Meas {
        id: "order_count",
        label: "Orders",
        expr: "COUNT(DISTINCT o.id)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Distinct orders containing these lines.",
    },
    Meas {
        id: "attach_rate",
        label: "Attach %",
        expr: "ROUND(100.0 * COUNT(DISTINCT o.id) / NULLIF((SELECT COUNT(*) FROM orders o2 WHERE o2.branch_id = ANY(:branch_ids) AND o2.status NOT IN ('voided','refunded') AND (:from::timestamptz IS NULL OR o2.created_at >= :from) AND (:to::timestamptz IS NULL OR o2.created_at <= :to)),0), 1)::float8",
        kind: ColumnKind::Number,
        joins: &[],
        help: "Share of all orders in the period that contained this item.",
    },
];

const ITEM_DIMS: &[Dim] = dims_with_time!(
    "o.created_at",
    [
        Dim {
            id: "branch",
            label: "Branch",
            expr: "b.name",
            kind: ColumnKind::Label,
            joins: &["branch"],
            time: false
        },
        Dim {
            id: "waiter",
            label: "Waiter",
            expr: "COALESCE(w.name,'Unassigned')",
            kind: ColumnKind::Label,
            joins: &["waiter"],
            time: false
        },
        // Uses the *snapshot* name on the line, so a later rename does not
        // rewrite history; falls back through the translation map.
        Dim {
            id: "product",
            label: "Product",
            expr: "COALESCE(NULLIF(oi.name_translations->>:locale,''), oi.item_name)",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
        Dim {
            id: "category",
            label: "Category",
            expr: "COALESCE(NULLIF(c.name_translations->>:locale,''), c.name, 'Uncategorized')",
            kind: ColumnKind::Label,
            joins: &["menu_item", "category"],
            time: false
        },
        Dim {
            id: "size",
            label: "Size",
            expr: "COALESCE(oi.size_label, 'Regular')",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
        Dim {
            id: "bundle",
            label: "Bundle",
            expr: "COALESCE(bn.name, 'Not in a bundle')",
            kind: ColumnKind::Label,
            joins: &["bundle"],
            time: false
        },
        Dim {
            id: "order_type",
            label: "Order type",
            expr: "COALESCE(o.order_type,'unknown')",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
    ]
);

// ── Dataset: payments (one row per tender line) ──────────────────────────────

const PAYMENT_JOINS: &[Join] = &[
    Join {
        id: "branch",
        sql: "LEFT JOIN branches b ON b.id = o.branch_id",
    },
    Join {
        id: "pay_method",
        sql: "LEFT JOIN org_payment_methods pm ON pm.name = op.method",
    },
];

const PAYMENT_MEASURES: &[Meas] = &[
    Meas {
        id: "payment_count",
        label: "Payments",
        expr: "COUNT(*)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Number of tender lines. A split-tender order contributes several.",
    },
    Meas {
        id: "paid_amount",
        label: "Amount",
        expr: "COALESCE(SUM(op.amount),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Amount tendered. Sums tender, not order totals.",
    },
    Meas {
        id: "avg_payment",
        label: "Avg payment",
        expr: "COALESCE(AVG(op.amount),0)::bigint",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Average tender line.",
    },
    Meas {
        id: "cash_amount",
        label: "Cash",
        expr: "COALESCE(SUM(op.amount) FILTER (WHERE op.is_cash),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Cash portion of the tender.",
    },
    Meas {
        id: "order_count",
        label: "Orders",
        expr: "COUNT(DISTINCT o.id)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Distinct orders paid.",
    },
];

const PAYMENT_DIMS: &[Dim] = dims_with_time!(
    "o.created_at",
    [
        Dim {
            id: "branch",
            label: "Branch",
            expr: "b.name",
            kind: ColumnKind::Label,
            joins: &["branch"],
            time: false
        },
        Dim {
            id: "payment_method",
            label: "Method",
            expr: "COALESCE(NULLIF(pm.label_translations->>:locale,''), pm.name, op.method)",
            kind: ColumnKind::Label,
            joins: &["pay_method"],
            time: false
        },
        Dim {
            id: "tender_kind",
            label: "Cash or card",
            expr: "CASE WHEN op.is_cash THEN 'Cash' ELSE 'Non-cash' END",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
        Dim {
            id: "order_type",
            label: "Order type",
            expr: "COALESCE(o.order_type,'unknown')",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
    ]
);

// ── Dataset: inventory (one row per stock movement) ──────────────────────────

const INV_JOINS: &[Join] = &[
    Join {
        id: "branch",
        sql: "LEFT JOIN branches b ON b.id = im.branch_id",
    },
    Join {
        id: "ingredient",
        sql: "JOIN org_ingredients ing ON ing.id = im.org_ingredient_id",
    },
    Join {
        id: "supplier",
        sql: "LEFT JOIN suppliers sup ON sup.id = ing.supplier_id",
    },
];

const INV_MEASURES: &[Meas] = &[
    Meas {
        id: "movement_count",
        label: "Movements",
        expr: "COUNT(*)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Number of stock movements.",
    },
    Meas {
        id: "qty",
        label: "Quantity",
        expr: "ROUND(SUM(ABS(im.quantity)),3)::float8",
        kind: ColumnKind::Number,
        joins: &[],
        help: "Absolute quantity moved, in each ingredient's stock unit.",
    },
    Meas {
        id: "net_qty",
        label: "Net change",
        expr: "ROUND(SUM(im.quantity),3)::float8",
        kind: ColumnKind::Number,
        joins: &[],
        help: "Signed net change in stock (positive = added).",
    },
    Meas {
        id: "movement_cost",
        label: "Value",
        expr: "COALESCE(ROUND(SUM(ABS(im.quantity) * COALESCE(im.unit_cost,0))),0)::bigint",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Value of the stock moved, at the cost recorded on the movement.",
    },
    Meas {
        id: "below_zero_count",
        label: "Negative-stock events",
        expr: "COUNT(*) FILTER (WHERE im.below_zero)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Movements that drove stock below zero — a counting or recipe problem.",
    },
];

const INV_DIMS: &[Dim] = dims_with_time!(
    "im.created_at",
    [
        Dim {
            id: "branch",
            label: "Branch",
            expr: "b.name",
            kind: ColumnKind::Label,
            joins: &["branch"],
            time: false
        },
        Dim {
            id: "ingredient",
            label: "Ingredient",
            expr: "ing.name",
            kind: ColumnKind::Label,
            joins: &["ingredient"],
            time: false
        },
        Dim {
            id: "ingredient_category",
            label: "Ingredient category",
            expr: "COALESCE(ing.category,'Uncategorized')",
            kind: ColumnKind::Label,
            joins: &["ingredient"],
            time: false
        },
        Dim {
            id: "supplier",
            label: "Supplier",
            expr: "COALESCE(sup.name,'No supplier')",
            kind: ColumnKind::Label,
            joins: &["ingredient", "supplier"],
            time: false
        },
        Dim {
            id: "movement_type",
            label: "Movement type",
            expr: "im.type::text",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
        Dim {
            id: "reason",
            label: "Reason",
            expr: "COALESCE(NULLIF(im.reason,''),'Unspecified')",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
    ]
);

const F_MOVEMENT_TYPE: Filter = Filter {
    id: "movement_type",
    label: "Movement type",
    help: "Which kind of stock movement to include. 'waste' is the spoilage/loss report.",
    options: &[
        FilterOpt {
            value: "all",
            sql: "",
        },
        FilterOpt {
            value: "waste",
            sql: "AND im.type = 'waste'",
        },
        FilterOpt {
            value: "sale",
            sql: "AND im.type = 'sale'",
        },
        FilterOpt {
            value: "purchase",
            sql: "AND im.type IN ('purchase_in','purchase_return')",
        },
        FilterOpt {
            value: "adjustment",
            sql: "AND im.type IN ('adjustment_add','adjustment_remove')",
        },
        FilterOpt {
            value: "transfer",
            sql: "AND im.type IN ('transfer_in','transfer_out')",
        },
        FilterOpt {
            value: "stock_count",
            sql: "AND im.type = 'stock_count'",
        },
        FilterOpt {
            value: "outbound",
            sql: "AND im.quantity < 0",
        },
        FilterOpt {
            value: "inbound",
            sql: "AND im.quantity > 0",
        },
    ],
    default: "all",
};

// ── Dataset: shifts (one row per till shift) ─────────────────────────────────

const SHIFT_JOINS: &[Join] = &[
    Join {
        id: "branch",
        sql: "LEFT JOIN branches b ON b.id = s.branch_id",
    },
    Join {
        id: "teller",
        sql: "LEFT JOIN users u ON u.id = s.teller_id",
    },
    Join {
        id: "till",
        sql: "LEFT JOIN tills tl ON tl.id = s.till_id",
    },
];

const SHIFT_MEASURES: &[Meas] = &[
    Meas {
        id: "shift_count",
        label: "Shifts",
        expr: "COUNT(*)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Number of shifts.",
    },
    Meas {
        id: "opening_cash",
        label: "Opening float",
        expr: "COALESCE(SUM(s.opening_cash),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Cash in the drawer at open.",
    },
    Meas {
        id: "declared_cash",
        label: "Declared cash",
        expr: "COALESCE(SUM(s.closing_cash_declared),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Cash the teller counted at close.",
    },
    Meas {
        id: "system_cash",
        label: "Expected cash",
        expr: "COALESCE(SUM(s.closing_cash_system),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Cash the system expected at close.",
    },
    Meas {
        id: "discrepancy",
        label: "Net discrepancy",
        expr: "COALESCE(SUM(s.cash_discrepancy),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Signed cash over/short. Overs and shorts cancel out.",
    },
    Meas {
        id: "abs_discrepancy",
        label: "Total variance",
        expr: "COALESCE(SUM(ABS(s.cash_discrepancy)),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Absolute cash variance — overs and shorts both count. The honest control metric.",
    },
    Meas {
        id: "short_count",
        label: "Short shifts",
        expr: "COUNT(*) FILTER (WHERE s.cash_discrepancy < 0)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Shifts that closed short.",
    },
    Meas {
        id: "force_closed_count",
        label: "Force-closed",
        expr: "COUNT(*) FILTER (WHERE s.force_closed_at IS NOT NULL)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Shifts closed by a manager rather than the teller.",
    },
    Meas {
        id: "avg_shift_minutes",
        label: "Avg length",
        expr: "ROUND(AVG(EXTRACT(EPOCH FROM (COALESCE(s.closed_at, now()) - s.opened_at))/60))::float8",
        kind: ColumnKind::Minutes,
        joins: &[],
        help: "Average shift length in minutes.",
    },
];

const SHIFT_DIMS: &[Dim] = dims_with_time!(
    "s.opened_at",
    [
        Dim {
            id: "branch",
            label: "Branch",
            expr: "b.name",
            kind: ColumnKind::Label,
            joins: &["branch"],
            time: false
        },
        Dim {
            id: "teller",
            label: "Teller",
            expr: "COALESCE(u.name,'Unknown')",
            kind: ColumnKind::Label,
            joins: &["teller"],
            time: false
        },
        Dim {
            id: "till",
            label: "Till",
            expr: "COALESCE(tl.name,'Unassigned')",
            kind: ColumnKind::Label,
            joins: &["till"],
            time: false
        },
        Dim {
            id: "status",
            label: "Status",
            expr: "s.status::text",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
    ]
);

// ── Dataset: attendance (one row per employee per business date) ─────────────

const ATT_JOINS: &[Join] = &[
    Join {
        id: "branch",
        sql: "LEFT JOIN branches b ON b.id = ar.branch_id",
    },
    Join {
        id: "employee",
        sql: "LEFT JOIN users u ON u.id = ar.user_id",
    },
    Join {
        id: "profile",
        sql: "LEFT JOIN staff_profiles sp ON sp.user_id = ar.user_id",
    },
    Join {
        id: "department",
        sql: "LEFT JOIN departments dep ON dep.id = sp.department_id",
    },
    Join {
        id: "work_shift",
        sql: "LEFT JOIN work_shifts ws ON ws.id = ar.work_shift_id",
    },
];

const ATT_MEASURES: &[Meas] = &[
    Meas {
        id: "record_count",
        label: "Records",
        expr: "COUNT(*)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Attendance records (one per employee per day).",
    },
    Meas {
        id: "present_count",
        label: "Present",
        expr: "COUNT(*) FILTER (WHERE ar.check_in_at IS NOT NULL)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Days an employee actually clocked in.",
    },
    Meas {
        id: "absent_count",
        label: "Absences",
        expr: "COUNT(*) FILTER (WHERE ar.status = 'absent')",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Days recorded as absent.",
    },
    Meas {
        id: "late_count",
        label: "Late arrivals",
        expr: "COUNT(*) FILTER (WHERE COALESCE(ar.late_minutes,0) > 0)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Days an employee arrived after the grace window.",
    },
    Meas {
        id: "late_minutes",
        label: "Late minutes",
        expr: "COALESCE(SUM(ar.late_minutes),0)::float8",
        kind: ColumnKind::Minutes,
        joins: &[],
        help: "Total minutes late.",
    },
    Meas {
        id: "overtime_minutes",
        label: "Overtime",
        expr: "COALESCE(SUM(ar.overtime_minutes),0)::float8",
        kind: ColumnKind::Minutes,
        joins: &[],
        help: "Total overtime minutes.",
    },
    Meas {
        id: "early_leave_minutes",
        label: "Early leave",
        expr: "COALESCE(SUM(ar.early_leave_minutes),0)::float8",
        kind: ColumnKind::Minutes,
        joins: &[],
        help: "Total minutes left early.",
    },
    Meas {
        id: "worked_minutes",
        label: "Worked",
        expr: "COALESCE(SUM(ar.worked_minutes),0)::float8",
        kind: ColumnKind::Minutes,
        joins: &[],
        help: "Total minutes worked.",
    },
    Meas {
        id: "avg_worked_minutes",
        label: "Avg day",
        expr: "ROUND(AVG(ar.worked_minutes))::float8",
        kind: ColumnKind::Minutes,
        joins: &[],
        help: "Average minutes worked per recorded day.",
    },
    Meas {
        id: "manual_count",
        label: "Manual entries",
        expr: "COUNT(*) FILTER (WHERE ar.is_manual)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Records entered or corrected by a manager rather than clocked.",
    },
    Meas {
        id: "employee_count",
        label: "Employees",
        expr: "COUNT(DISTINCT ar.user_id)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Distinct employees.",
    },
];

const ATT_DIMS: &[Dim] = &[
    Dim {
        id: "day",
        label: "Day",
        expr: "ar.business_date",
        kind: ColumnKind::Date,
        joins: &[],
        time: true,
    },
    Dim {
        id: "week",
        label: "Week",
        expr: "date_trunc('week', ar.business_date)::date",
        kind: ColumnKind::Date,
        joins: &[],
        time: true,
    },
    Dim {
        id: "month",
        label: "Month",
        expr: "date_trunc('month', ar.business_date)::date",
        kind: ColumnKind::Date,
        joins: &[],
        time: true,
    },
    Dim {
        id: "weekday",
        label: "Weekday",
        expr: "trim(to_char(ar.business_date, 'Day'))",
        kind: ColumnKind::Label,
        joins: &[],
        time: true,
    },
    Dim {
        id: "branch",
        label: "Branch",
        expr: "COALESCE(b.name,'Unassigned')",
        kind: ColumnKind::Label,
        joins: &["branch"],
        time: false,
    },
    Dim {
        id: "employee",
        label: "Employee",
        expr: "COALESCE(u.name,'Unknown')",
        kind: ColumnKind::Label,
        joins: &["employee"],
        time: false,
    },
    Dim {
        id: "department",
        label: "Department",
        expr: "COALESCE(dep.name,'No department')",
        kind: ColumnKind::Label,
        joins: &["profile", "department"],
        time: false,
    },
    Dim {
        id: "job_title",
        label: "Job title",
        expr: "COALESCE(sp.job_title,'Unspecified')",
        kind: ColumnKind::Label,
        joins: &["profile"],
        time: false,
    },
    Dim {
        id: "work_shift",
        label: "Work shift",
        expr: "COALESCE(ws.name,'Unscheduled')",
        kind: ColumnKind::Label,
        joins: &["work_shift"],
        time: false,
    },
    Dim {
        id: "status",
        label: "Status",
        expr: "COALESCE(ar.status,'unknown')",
        kind: ColumnKind::Label,
        joins: &[],
        time: false,
    },
];

const F_ATT_STATUS: Filter = Filter {
    id: "attendance_status",
    label: "Attendance status",
    help: "Which attendance records to include.",
    options: &[
        FilterOpt {
            value: "all",
            sql: "",
        },
        FilterOpt {
            value: "present",
            sql: "AND ar.check_in_at IS NOT NULL",
        },
        FilterOpt {
            value: "absent",
            sql: "AND ar.status = 'absent'",
        },
        FilterOpt {
            value: "late",
            sql: "AND COALESCE(ar.late_minutes,0) > 0",
        },
    ],
    default: "all",
};

// ── Dataset: bookings (reservations + waitlist) ──────────────────────────────

const BOOK_JOINS: &[Join] = &[Join {
    id: "branch",
    sql: "LEFT JOIN branches b ON b.id = bk.branch_id",
}];

const BOOK_MEASURES: &[Meas] = &[
    Meas {
        id: "booking_count",
        label: "Bookings",
        expr: "COUNT(*)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Number of reservations or waitlist entries.",
    },
    Meas {
        id: "covers",
        label: "Covers",
        expr: "COALESCE(SUM(bk.party_size),0)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Total guests booked.",
    },
    Meas {
        id: "seated_count",
        label: "Seated",
        expr: "COUNT(*) FILTER (WHERE bk.seated_at IS NOT NULL)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Bookings that were actually seated.",
    },
    Meas {
        id: "no_show_count",
        label: "No-shows",
        expr: "COUNT(*) FILTER (WHERE bk.status = 'no_show')",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Bookings that never arrived.",
    },
    Meas {
        id: "cancelled_count",
        label: "Cancelled",
        expr: "COUNT(*) FILTER (WHERE bk.status = 'cancelled')",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Cancelled bookings.",
    },
    Meas {
        id: "no_show_rate",
        label: "No-show %",
        expr: "ROUND(100.0 * COUNT(*) FILTER (WHERE bk.status = 'no_show') / NULLIF(COUNT(*),0), 1)::float8",
        kind: ColumnKind::Number,
        joins: &[],
        help: "Share of bookings that did not arrive.",
    },
    Meas {
        id: "avg_party_size",
        label: "Avg party",
        expr: "ROUND(AVG(bk.party_size), 1)::float8",
        kind: ColumnKind::Number,
        joins: &[],
        help: "Average guests per booking.",
    },
    Meas {
        id: "avg_wait_minutes",
        label: "Avg wait",
        expr: "ROUND(AVG(EXTRACT(EPOCH FROM (bk.seated_at - bk.created_at))/60) FILTER (WHERE bk.seated_at IS NOT NULL))::float8",
        kind: ColumnKind::Minutes,
        joins: &[],
        help: "Average minutes between joining and being seated.",
    },
];

const BOOK_DIMS: &[Dim] = dims_with_time!(
    "COALESCE(bk.reserved_for, bk.created_at)",
    [
        Dim {
            id: "branch",
            label: "Branch",
            expr: "b.name",
            kind: ColumnKind::Label,
            joins: &["branch"],
            time: false
        },
        Dim {
            id: "status",
            label: "Status",
            expr: "bk.status::text",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
        Dim {
            id: "kind",
            label: "Kind",
            expr: "bk.kind",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
        Dim {
            id: "source",
            label: "Source",
            expr: "COALESCE(bk.source,'unknown')",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
        Dim {
            id: "party_bucket",
            label: "Party size",
            expr: "CASE WHEN bk.party_size <= 2 THEN '1-2' WHEN bk.party_size <= 4 THEN '3-4' WHEN bk.party_size <= 6 THEN '5-6' ELSE '7+' END",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
    ]
);

const F_BOOKING_KIND: Filter = Filter {
    id: "booking_kind",
    label: "Booking kind",
    help: "Reservations (booked ahead) versus walk-in waitlist entries.",
    options: &[
        FilterOpt {
            value: "any",
            sql: "",
        },
        FilterOpt {
            value: "reservation",
            sql: "AND bk.kind = 'reservation'",
        },
        FilterOpt {
            value: "waitlist",
            sql: "AND bk.kind = 'waitlist'",
        },
    ],
    default: "any",
};

// ── Dataset: purchasing (one row per purchase-order line) ────────────────────

const PUR_JOINS: &[Join] = &[
    Join {
        id: "branch",
        sql: "LEFT JOIN branches b ON b.id = po.branch_id",
    },
    Join {
        id: "supplier",
        sql: "LEFT JOIN suppliers sup ON sup.id = po.supplier_id",
    },
    Join {
        id: "ingredient",
        sql: "LEFT JOIN org_ingredients ing ON ing.id = pol.org_ingredient_id",
    },
];

const PUR_MEASURES: &[Meas] = &[
    Meas {
        id: "line_count",
        label: "Lines",
        expr: "COUNT(*)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Purchase-order lines.",
    },
    Meas {
        id: "po_count",
        label: "Purchase orders",
        expr: "COUNT(DISTINCT po.id)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Distinct purchase orders.",
    },
    Meas {
        id: "qty_ordered",
        label: "Ordered",
        expr: "ROUND(SUM(pol.quantity_ordered),3)::float8",
        kind: ColumnKind::Number,
        joins: &[],
        help: "Quantity ordered, in purchase units.",
    },
    Meas {
        id: "qty_received",
        label: "Received",
        expr: "ROUND(SUM(COALESCE(pol.quantity_received,0)),3)::float8",
        kind: ColumnKind::Number,
        joins: &[],
        help: "Quantity actually received.",
    },
    Meas {
        id: "fill_rate",
        label: "Fill rate %",
        expr: "ROUND(100.0 * SUM(COALESCE(pol.quantity_received,0)) / NULLIF(SUM(pol.quantity_ordered),0), 1)::float8",
        kind: ColumnKind::Number,
        joins: &[],
        help: "Received as a share of ordered — supplier reliability.",
    },
    Meas {
        id: "purchase_cost",
        label: "Spend",
        expr: "COALESCE(ROUND(SUM(COALESCE(pol.quantity_received,0) * COALESCE(pol.unit_cost,0))),0)::bigint",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Money spent on goods actually received.",
    },
    Meas {
        id: "ordered_cost",
        label: "Committed",
        expr: "COALESCE(ROUND(SUM(pol.quantity_ordered * COALESCE(pol.unit_cost,0))),0)::bigint",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Value of what was ordered, received or not.",
    },
    Meas {
        id: "avg_unit_cost",
        label: "Avg unit cost",
        expr: "COALESCE(ROUND(AVG(pol.unit_cost)),0)::bigint",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Average purchase price per unit.",
    },
];

const PUR_DIMS: &[Dim] = dims_with_time!(
    "po.created_at",
    [
        Dim {
            id: "branch",
            label: "Branch",
            expr: "b.name",
            kind: ColumnKind::Label,
            joins: &["branch"],
            time: false
        },
        Dim {
            id: "supplier",
            label: "Supplier",
            expr: "COALESCE(sup.name,'No supplier')",
            kind: ColumnKind::Label,
            joins: &["supplier"],
            time: false
        },
        Dim {
            id: "ingredient",
            label: "Ingredient",
            expr: "COALESCE(ing.name,'Unknown')",
            kind: ColumnKind::Label,
            joins: &["ingredient"],
            time: false
        },
        Dim {
            id: "ingredient_category",
            label: "Ingredient category",
            expr: "COALESCE(ing.category,'Uncategorized')",
            kind: ColumnKind::Label,
            joins: &["ingredient"],
            time: false
        },
        Dim {
            id: "status",
            label: "PO status",
            expr: "po.status::text",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
    ]
);

const F_PO_STATUS: Filter = Filter {
    id: "po_status",
    label: "Purchase order status",
    help: "Which purchase orders count. 'received' is what actually cost money.",
    options: &[
        FilterOpt {
            value: "all",
            sql: "",
        },
        FilterOpt {
            value: "received",
            sql: "AND po.status IN ('received','partially_received')",
        },
        FilterOpt {
            value: "ordered",
            sql: "AND po.status = 'ordered'",
        },
        FilterOpt {
            value: "draft",
            sql: "AND po.status = 'draft'",
        },
        FilterOpt {
            value: "cancelled",
            sql: "AND po.status = 'cancelled'",
        },
    ],
    default: "all",
};

// ── Dataset: stocktakes (one row per counted ingredient) ─────────────────────

const ST_JOINS: &[Join] = &[
    Join {
        id: "branch",
        sql: "LEFT JOIN branches b ON b.id = st.branch_id",
    },
    Join {
        id: "ingredient",
        sql: "LEFT JOIN org_ingredients ing ON ing.id = si.org_ingredient_id",
    },
];

const ST_MEASURES: &[Meas] = &[
    Meas {
        id: "counted_lines",
        label: "Counted items",
        expr: "COUNT(*)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Ingredient lines counted.",
    },
    Meas {
        id: "variance_qty",
        label: "Net variance",
        expr: "ROUND(SUM(si.variance),3)::float8",
        kind: ColumnKind::Number,
        joins: &[],
        help: "Signed quantity variance (negative = missing stock).",
    },
    Meas {
        id: "shrink_cost",
        label: "Shrinkage",
        expr: "COALESCE(ROUND(SUM(ABS(LEAST(si.variance,0)) * COALESCE(si.unit_cost,0))),0)::bigint",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Value of stock that was missing at the count — the loss figure.",
    },
    Meas {
        id: "overage_cost",
        label: "Overage",
        expr: "COALESCE(ROUND(SUM(GREATEST(si.variance,0) * COALESCE(si.unit_cost,0))),0)::bigint",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Value of stock found in excess of the system figure.",
    },
    Meas {
        id: "variance_lines",
        label: "Items off",
        expr: "COUNT(*) FILTER (WHERE si.variance <> 0)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Lines whose count did not match the system.",
    },
    Meas {
        id: "stocktake_count",
        label: "Stocktakes",
        expr: "COUNT(DISTINCT st.id)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Distinct stocktakes.",
    },
];

const ST_DIMS: &[Dim] = dims_with_time!(
    "COALESCE(st.finalized_at, st.created_at)",
    [
        Dim {
            id: "branch",
            label: "Branch",
            expr: "b.name",
            kind: ColumnKind::Label,
            joins: &["branch"],
            time: false
        },
        Dim {
            id: "ingredient",
            label: "Ingredient",
            expr: "COALESCE(ing.name,'Unknown')",
            kind: ColumnKind::Label,
            joins: &["ingredient"],
            time: false
        },
        Dim {
            id: "ingredient_category",
            label: "Ingredient category",
            expr: "COALESCE(ing.category,'Uncategorized')",
            kind: ColumnKind::Label,
            joins: &["ingredient"],
            time: false
        },
        Dim {
            id: "variance_reason",
            label: "Reason",
            expr: "COALESCE(si.variance_reason::text,'unexplained')",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
    ]
);

// ── The registry ─────────────────────────────────────────────────────────────

pub const DATASETS: &[Dataset] = &[
    Dataset {
        id: "orders",
        title: "Orders",
        help: "One row per order (a completed sale ticket). Use for revenue, ticket size, \
               discounts, tips, voids, and anything counted per order. Do NOT use for \
               per-product questions — use order_items.",
        from: "orders o",
        branch_col: "o.branch_id",
        time_col: "o.created_at",
        time_is_date: false,
        base_pred: "",
        joins: ORDERS_JOINS,
        dims: ORDERS_DIMS,
        measures: ORDERS_MEASURES,
        filters: &[
            F_ORDER_STATUS,
            F_ORDER_TYPE,
            F_DELIVERY_CHANNEL,
            F_DISCOUNTED,
        ],
        default_measures: &["order_count", "revenue"],
        default_viz: Viz::Bar,
    },
    Dataset {
        id: "order_items",
        title: "Order items",
        help: "One row per line on an order. Use for product, category, size and bundle \
               questions, item profitability, and units sold. Line revenue excludes \
               order-level discounts and tax.",
        from: "order_items oi JOIN orders o ON o.id = oi.order_id",
        branch_col: "o.branch_id",
        time_col: "o.created_at",
        time_is_date: false,
        base_pred: "",
        joins: ITEM_JOINS,
        dims: ITEM_DIMS,
        measures: ITEM_MEASURES,
        filters: &[F_ORDER_STATUS, F_ORDER_TYPE],
        default_measures: &["units_sold", "item_revenue"],
        default_viz: Viz::Bar,
    },
    Dataset {
        id: "payments",
        title: "Payments",
        help: "One row per tender line. Use for payment-method mix and cash versus card. \
               A split-tender order contributes several rows, so counts here are tender \
               counts, not order counts.",
        from: "order_payments op JOIN orders o ON o.id = op.order_id",
        branch_col: "o.branch_id",
        time_col: "o.created_at",
        time_is_date: false,
        base_pred: "",
        joins: PAYMENT_JOINS,
        dims: PAYMENT_DIMS,
        measures: PAYMENT_MEASURES,
        filters: &[F_ORDER_STATUS, F_ORDER_TYPE],
        default_measures: &["paid_amount", "payment_count"],
        default_viz: Viz::Donut,
    },
    Dataset {
        id: "inventory",
        title: "Inventory movements",
        help: "One row per stock movement. Use for waste and spoilage (movement_type \
               'waste'), consumption, transfers, and stock value moved. Quantities are in \
               each ingredient's own unit, so only compare within one ingredient.",
        from: "inventory_movements im",
        branch_col: "im.branch_id",
        time_col: "im.created_at",
        time_is_date: false,
        base_pred: "",
        joins: INV_JOINS,
        dims: INV_DIMS,
        measures: INV_MEASURES,
        filters: &[F_MOVEMENT_TYPE],
        default_measures: &["movement_cost", "qty"],
        default_viz: Viz::Bar,
    },
    Dataset {
        id: "shifts",
        title: "Shifts",
        help: "One row per till shift. Use for cash control: drawer variance, short \
               shifts, force-closes, and shift length by teller or branch.",
        from: "shifts s",
        branch_col: "s.branch_id",
        time_col: "s.opened_at",
        time_is_date: false,
        base_pred: "",
        joins: SHIFT_JOINS,
        dims: SHIFT_DIMS,
        measures: SHIFT_MEASURES,
        filters: &[Filter {
            id: "shift_status",
            label: "Shift status",
            help: "Open, closed, or force-closed shifts.",
            options: &[
                FilterOpt {
                    value: "all",
                    sql: "",
                },
                FilterOpt {
                    value: "closed",
                    sql: "AND s.status IN ('closed','force_closed')",
                },
                FilterOpt {
                    value: "open",
                    sql: "AND s.status = 'open'",
                },
                FilterOpt {
                    value: "force_closed",
                    sql: "AND s.status = 'force_closed'",
                },
            ],
            default: "all",
        }],
        default_measures: &["shift_count", "abs_discrepancy"],
        default_viz: Viz::Bar,
    },
    Dataset {
        id: "attendance",
        title: "Attendance",
        help: "One row per employee per business date. Use for lateness, absence, \
               overtime and hours worked, by employee, department or branch.",
        from: "attendance_records ar",
        branch_col: "ar.branch_id",
        time_col: "ar.business_date",
        time_is_date: true,
        base_pred: "",
        joins: ATT_JOINS,
        dims: ATT_DIMS,
        measures: ATT_MEASURES,
        filters: &[F_ATT_STATUS],
        default_measures: &["record_count", "late_minutes"],
        default_viz: Viz::Bar,
    },
    Dataset {
        id: "bookings",
        title: "Reservations & waitlist",
        help: "One row per reservation or waitlist entry. Use for covers booked, \
               no-show rate, party sizes and waiting times.",
        from: "bookings bk",
        branch_col: "bk.branch_id",
        time_col: "COALESCE(bk.reserved_for, bk.created_at)",
        time_is_date: false,
        base_pred: "",
        joins: BOOK_JOINS,
        dims: BOOK_DIMS,
        measures: BOOK_MEASURES,
        filters: &[F_BOOKING_KIND],
        default_measures: &["booking_count", "covers"],
        default_viz: Viz::Bar,
    },
    Dataset {
        id: "purchasing",
        title: "Purchasing",
        help: "One row per purchase-order line. Use for supplier spend, fill rates, \
               and what ingredients cost to buy.",
        from: "purchase_order_lines pol JOIN purchase_orders po ON po.id = pol.purchase_order_id",
        branch_col: "po.branch_id",
        time_col: "po.created_at",
        time_is_date: false,
        base_pred: "",
        joins: PUR_JOINS,
        dims: PUR_DIMS,
        measures: PUR_MEASURES,
        filters: &[F_PO_STATUS],
        default_measures: &["purchase_cost", "po_count"],
        default_viz: Viz::Bar,
    },
    Dataset {
        id: "stocktakes",
        title: "Stocktakes",
        help: "One row per ingredient counted in a finalized stocktake. Use for \
               shrinkage, count accuracy, and which ingredients go missing.",
        from: "stocktake_items si JOIN stocktakes st ON st.id = si.stocktake_id",
        branch_col: "st.branch_id",
        time_col: "COALESCE(st.finalized_at, st.created_at)",
        time_is_date: false,
        // Draft and in-progress counts hold provisional numbers that would read
        // as enormous phantom variance.
        base_pred: "AND st.status = 'finalized'",
        joins: ST_JOINS,
        dims: ST_DIMS,
        measures: ST_MEASURES,
        filters: &[],
        default_measures: &["shrink_cost", "variance_lines"],
        default_viz: Viz::Bar,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn dataset_ids_are_unique() {
        let mut seen = HashSet::new();
        for d in DATASETS {
            assert!(seen.insert(d.id), "duplicate dataset id {}", d.id);
        }
    }

    #[test]
    fn every_dim_and_measure_id_is_unique_within_its_dataset() {
        for d in DATASETS {
            let mut dims = HashSet::new();
            for dim in d.dims {
                assert!(dims.insert(dim.id), "{}: duplicate dim {}", d.id, dim.id);
            }
            let mut ms = HashSet::new();
            for m in d.measures {
                assert!(ms.insert(m.id), "{}: duplicate measure {}", d.id, m.id);
            }
        }
    }

    #[test]
    fn every_referenced_join_exists_in_its_dataset() {
        for d in DATASETS {
            let known: HashSet<&str> = d.joins.iter().map(|j| j.id).collect();
            for dim in d.dims {
                for j in dim.joins {
                    assert!(
                        known.contains(j),
                        "{}: dim {} wants unknown join {j}",
                        d.id,
                        dim.id
                    );
                }
            }
            for m in d.measures {
                for j in m.joins {
                    assert!(
                        known.contains(j),
                        "{}: measure {} wants unknown join {j}",
                        d.id,
                        m.id
                    );
                }
            }
        }
    }

    #[test]
    fn default_measures_exist_and_filters_have_a_valid_default() {
        for d in DATASETS {
            assert!(
                !d.default_measures.is_empty(),
                "{}: no default measures",
                d.id
            );
            for m in d.default_measures {
                assert!(
                    d.measure(m).is_some(),
                    "{}: unknown default measure {m}",
                    d.id
                );
            }
            for f in d.filters {
                assert!(
                    f.option(f.default).is_some(),
                    "{}: filter {} default {} is not an option",
                    d.id,
                    f.id,
                    f.default
                );
            }
        }
    }

    /// The drift guard for pseudonymisation.
    ///
    /// A new dimension that joins to `users` and is not listed as personal
    /// would send staff names to a third-party model silently. Derived from the
    /// join graph rather than maintained by hand, so it cannot be forgotten.
    #[test]
    fn every_person_valued_dimension_is_marked_personal() {
        for d in DATASETS {
            for dim in d.dims {
                if dim.joins.iter().any(|j| PERSON_JOINS.contains(j)) {
                    assert!(
                        is_personal_dimension(dim.id),
                        "{}/{} resolves to a person's name but no EntityKind \
                         claims it — it would reach the model unpseudonymised",
                        d.id,
                        dim.id
                    );
                }
            }
        }
    }

    #[test]
    fn every_personal_dimension_actually_exists() {
        for id in personal_dimensions() {
            assert!(
                DATASETS.iter().any(|d| d.dims.iter().any(|x| x.id == id)),
                "a personal kind names dimension '{id}', which no dataset has"
            );
        }
    }

    #[test]
    fn business_dimensions_are_not_treated_as_personal() {
        // Over-marking is its own failure: the model needs product and branch
        // names to reason ("Latte and Mocha are both drinks"), and replacing
        // them with codes would make answers unusable.
        for id in [
            "branch",
            "product",
            "category",
            "ingredient",
            "supplier",
            "department",
        ] {
            assert!(!is_personal_dimension(id), "{id} must not be pseudonymised");
        }
    }

    #[test]
    fn every_dataset_documents_itself() {
        // The help text is the model's entire basis for routing, and the widget
        // picker's entire basis for describing a metric. An empty one is a bug.
        for d in DATASETS {
            assert!(d.help.len() > 40, "{}: help text too thin", d.id);
            for m in d.measures {
                assert!(!m.help.is_empty(), "{}: measure {} has no help", d.id, m.id);
            }
        }
    }
}
