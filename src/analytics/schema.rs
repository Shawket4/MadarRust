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
///
/// `refunded` is the status of a FULLY refunded order only (the trigger in
/// 20260912090000 flips it when the cumulative refund reaches the total). A
/// partially refunded order stays `completed`, stays inside `sold`, and the
/// money that went back is subtracted by the measures themselves through the
/// `refunds` join — status alone can no longer tell you what was kept.
const F_ORDER_STATUS: Filter = Filter {
    id: "status",
    label: "Order status",
    help: "Which orders count. 'sold' (default) excludes voided and fully refunded orders; \
           partially refunded orders stay in and the money measures net their refunds.",
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
    help: "Dine-in (the only kind that carries a service charge), takeaway rung up at the \
           counter, or delivery. Rows before 2026-09 say dine_in for every till sale.",
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
            value: "takeaway",
            sql: "AND o.order_type = 'takeaway'",
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
    // Money returned against the order. One row per order (the view groups by
    // order_id), so it cannot fan out either. `refunded_amount` is NULL for an
    // order nothing was returned on — every consumer COALESCEs it.
    Join {
        id: "refunds",
        sql: "LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id",
    },
];

/// The merchant's own take on an order, after refunds, as a SUM over the group.
///
/// Per order: `(total − tax − delivery_fee) × (total − refunded) ÷ total`. The
/// first factor is what the shop keeps of a bill — the tax is the state's and
/// the delivery fee is the courier's — and it is right under both tax policies:
/// `total_amount` has an exclusive tax added on and an inclusive one inside,
/// and subtracting `tax_amount` lands on the same net-of-tax figure either way.
/// The second factor is the share of the bill the customer did not get back. A
/// refund is recorded in "what the customer paid" units (its tax rides inside
/// it, the way it rode inside the bill), so pro-rating is the only honest
/// split: subtracting the whole refund would charge the merchant for tax that
/// is no longer owed.
///
/// THE SERVICE CHARGE IS IN HERE, and that is a decision, not an accident. It
/// could be read as a pass-through to staff, like a tip; it is not treated as
/// one. A tip never enters `total_amount` or the tax base and belongs to the
/// person who was tipped. The service charge is on the bill, is priced by the
/// shop's own policy, enters the tax base by default
/// (`organizations.service_charge_taxable`), and the shop decides what to do
/// with it — paying it out to staff is payroll, a cost against revenue, not a
/// deduction from it. So `net_revenue` = goods + service charge, net of tax,
/// delivery fee and refunds; `service_charge_total` reports the charge on its
/// own for a shop that wants to see it apart.
///
/// A zero-total bill divides by NULL and drops out of the SUM, which is right:
/// its net take is zero. Rounded once, after the SUM, so a hundred bills do not
/// each contribute half a piastre of rounding.
macro_rules! net_revenue_sql {
    () => {
        "SUM((o.total_amount - o.tax_amount - o.delivery_fee) \
          * (o.total_amount - COALESCE(rf.refunded_amount,0))::numeric \
          / NULLIF(o.total_amount,0))"
    };
}

const ORDERS_MEASURES: &[Meas] = &[
    Meas {
        id: "order_count",
        label: "Orders",
        expr: "COUNT(DISTINCT o.id)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Number of orders.",
    },
    // `revenue` was SUM(total_amount) until 2026-09: a partially refunded sale
    // counted in full, because only a FULL refund changes the order's status
    // and the status filter was the only thing subtracting anything. It is now
    // net of the money returned. `gross_sales` keeps the old figure under a
    // name that says what it is, and `revenue = gross_sales − refund_amount`
    // holds under every status filter.
    Meas {
        id: "revenue",
        label: "Revenue",
        // `::bigint` is load-bearing: `refunded_amount` is a bigint, so the SUM
        // comes back numeric, and the executor decodes a Money column as i64 —
        // a numeric would read as NULL on every dashboard. Any measure that
        // mixes the view's columns in needs the same cast.
        expr: "COALESCE(SUM(o.total_amount - COALESCE(rf.refunded_amount,0)),0)::bigint",
        kind: ColumnKind::Money,
        joins: &["refunds"],
        help: "What the shop kept: order totals after discount (tax, service charge and \
               delivery fee included), less any money refunded against those orders. \
               Fully refunded orders are already out under the default 'sold' filter.",
    },
    Meas {
        id: "gross_sales",
        label: "Gross sales",
        expr: "COALESCE(SUM(o.total_amount),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Order totals as rung up, before any refund. revenue = gross_sales − refund_amount.",
    },
    Meas {
        id: "net_revenue",
        label: "Net revenue",
        expr: concat!("COALESCE(ROUND(", net_revenue_sql!(), "),0)::bigint"),
        kind: ColumnKind::Money,
        joins: &["refunds"],
        help: "The merchant's own take: revenue less tax and delivery fees, net of refunds \
               (a refund's tax share is pro-rated out, not charged to the merchant). \
               The service charge stays IN — it is the shop's income, not a tip; see \
               service_charge_total to view it apart.",
    },
    Meas {
        id: "service_charge_total",
        label: "Service charge",
        expr: "COALESCE(SUM(o.service_charge_amount),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Service charge added to the bills, as rung up. Dine-in only by rule; taxable \
               or not according to the policy the order was priced under. Counted inside \
               revenue and net_revenue as the shop's income.",
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
        help: "Tax on the bills as rung up (inclusive or exclusive, per the order's policy). \
               Not reduced by partial refunds — net_revenue carries the refund's tax share.",
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
    // Refunds on the ORDER grain are attributed to the sale they were against,
    // whenever they were issued — the same restatement a full refund makes
    // through the status flip. That is the right view for "what did we keep of
    // September's sales"; for "how much did we hand back in September" use the
    // `refunds` dataset, whose grain is the refund and whose clock is issued_at.
    Meas {
        id: "refund_count",
        label: "Orders refunded",
        expr: "COUNT(*) FILTER (WHERE rf.refund_count > 0)",
        kind: ColumnKind::Count,
        joins: &["refunds"],
        help: "Orders with at least one refund against them. Under the default 'sold' \
               filter this is the PARTIALLY refunded ones only — a fully refunded order \
               is out of 'sold'. Set status to 'all' for every order that returned money.",
    },
    Meas {
        id: "refund_amount",
        label: "Refunded",
        expr: "COALESCE(SUM(rf.refunded_amount),0)::bigint",
        kind: ColumnKind::Money,
        joins: &["refunds"],
        help: "Money returned against these orders, attributed to the sale it was against. \
               Under 'sold' this is partial refunds only; under 'all' it is everything. \
               For refunds by the day they were ISSUED, use the refunds dataset.",
    },
    Meas {
        id: "delivery_fees",
        label: "Delivery fees",
        expr: "COALESCE(SUM(o.delivery_fee),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Delivery fees charged, as rung up. Outside the tax base and not food revenue: \
               inside revenue (the customer paid it) but excluded from net_revenue.",
    },
    Meas {
        id: "avg_order_value",
        label: "Avg order",
        expr: "COALESCE(AVG(o.total_amount),0)::bigint",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Average order total as rung up (average ticket). A refund does not shrink \
               the bill that was ordered, so this is gross_sales ÷ orders, not revenue ÷ orders.",
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
    // Cost stays whole when revenue is refunded: a plate sent back was still
    // cooked. So a refund lowers profit by its full net share, which is the
    // truth of it.
    Meas {
        id: "profit",
        label: "Profit",
        expr: concat!(
            "(CASE WHEN bool_or(it.cost_missing) THEN NULL ELSE ROUND(COALESCE(",
            net_revenue_sql!(),
            ",0) - SUM(it.cost)) END)::bigint"
        ),
        kind: ColumnKind::Money,
        joins: &["items", "refunds"],
        help: "Net revenue (after refunds) minus cost of goods. NULL if any cost is missing.",
    },
    Meas {
        id: "margin_pct",
        label: "Margin %",
        expr: concat!(
            "(CASE WHEN bool_or(it.cost_missing) THEN NULL ELSE ROUND(100.0 * (COALESCE(",
            net_revenue_sql!(),
            ",0) - SUM(it.cost)) / NULLIF(",
            net_revenue_sql!(),
            ",0), 1) END)::float8"
        ),
        kind: ColumnKind::Number,
        joins: &["items", "refunds"],
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
        // A sale that came through no delivery order is labelled by its own
        // kind. This used to say 'dine_in' for anything without a channel,
        // which was true until takeaway became expressible.
        Dim {
            id: "delivery_channel",
            label: "Channel",
            expr: "COALESCE(d.channel::text, o.order_type)",
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

// ── Dataset: tables (one row per settled sale that was eaten at a table) ─────

const TABLE_JOINS: &[Join] = &[
    Join {
        id: "branch",
        sql: "LEFT JOIN branches b ON b.id = o.branch_id",
    },
    Join {
        id: "section",
        sql: "LEFT JOIN floor_sections fs ON fs.id = tb.section_id",
    },
    Join {
        id: "refunds",
        sql: "LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id",
    },
];

/// Minutes from the party sitting down to the bill being paid.
macro_rules! dwell_minutes_sql {
    () => {
        "GREATEST(EXTRACT(EPOCH FROM (o.created_at - COALESCE(o.seated_at, o.created_at))) / 60.0, 0)"
    };
}

const TABLE_MEASURES: &[Meas] = &[
    Meas {
        id: "turns",
        label: "Turns",
        expr: "COUNT(DISTINCT o.id)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Parties served: settled bills eaten at a table.",
    },
    Meas {
        id: "turns_per_day",
        label: "Turns per day",
        expr: "ROUND(COUNT(DISTINCT o.id)::numeric \
               / NULLIF(COUNT(DISTINCT (o.table_id, (o.created_at AT TIME ZONE :tz)::date)), 0), 2)::float8",
        kind: ColumnKind::Number,
        joins: &[],
        help: "Parties per table per trading day (a day the table sat at least one party). \
               Grouped by table it is that table's turns on the days it was used.",
    },
    Meas {
        id: "covers",
        label: "Covers",
        expr: "COALESCE(SUM(o.covers),0)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Guests seated, from the bill's guest count (bills with no count add none).",
    },
    Meas {
        id: "table_revenue",
        label: "Revenue",
        expr: "COALESCE(SUM(o.total_amount - COALESCE(rf.refunded_amount,0)),0)::bigint",
        kind: ColumnKind::Money,
        joins: &["refunds"],
        help: "What the table's bills brought in after refunds (tax and service charge included).",
    },
    Meas {
        id: "revenue_per_table",
        label: "Revenue per table",
        expr: "COALESCE(ROUND(SUM(o.total_amount - COALESCE(rf.refunded_amount,0))::numeric \
               / NULLIF(COUNT(DISTINCT o.table_id),0)),0)::bigint",
        kind: ColumnKind::Money,
        joins: &["refunds"],
        help: "Revenue divided by the number of distinct tables that took money in the group.",
    },
    Meas {
        id: "revenue_per_cover",
        label: "Revenue per cover",
        expr: "COALESCE(ROUND(SUM(o.total_amount - COALESCE(rf.refunded_amount,0))::numeric \
               / NULLIF(SUM(o.covers),0)),0)::bigint",
        kind: ColumnKind::Money,
        joins: &["refunds"],
        help: "Revenue divided by covers. Bills with no guest count still add revenue, so \
               record covers for this to be fair.",
    },
    Meas {
        id: "avg_dwell_minutes",
        label: "Avg minutes seated",
        expr: concat!(
            "COALESCE(ROUND(AVG(",
            dwell_minutes_sql!(),
            ")::numeric, 1),0)::float8"
        ),
        kind: ColumnKind::Minutes,
        joins: &[],
        help: "Mean minutes from the party sitting down (the seat, else the bill opening) to \
               paying.",
    },
    Meas {
        id: "active_tables",
        label: "Tables used",
        expr: "COUNT(DISTINCT o.table_id)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Distinct tables that sat at least one paying party.",
    },
];

const TABLE_DIMS: &[Dim] = dims_with_time!(
    "o.created_at",
    [
        Dim {
            id: "table",
            label: "Table",
            expr: "tb.label",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
        Dim {
            id: "section",
            label: "Section",
            expr: "COALESCE(fs.name, 'No section')",
            kind: ColumnKind::Label,
            joins: &["section"],
            time: false
        },
        Dim {
            id: "branch",
            label: "Branch",
            expr: "b.name",
            kind: ColumnKind::Label,
            joins: &["branch"],
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
        help: "Amount tendered — money IN, by the method it came in. Sums tender, not order \
               totals, and is not reduced by refunds: money going back out is its own event \
               in the refunds dataset, with its own tender.",
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

// ── Dataset: refunds (one row per refund) ────────────────────────────────────
//
// The refund's own grain and its own clock. On the orders dataset a refund is
// folded into the sale it was against, whenever it happened; here it sits on
// the day it was ISSUED, out of the drawer it left. "How much did we give back
// last month" is this dataset; "what did we keep of last month's sales" is
// the other one. Both are true and they are not the same number.

const REFUND_JOINS: &[Join] = &[
    Join {
        id: "branch",
        sql: "LEFT JOIN branches b ON b.id = r.branch_id",
    },
    // Whoever issued it. A person — `cashier` is claimed by the cashier
    // EntityKind, so the name is pseudonymised before it reaches a model.
    Join {
        id: "cashier",
        sql: "LEFT JOIN users t ON t.id = r.issued_by",
    },
];

const REFUND_MEASURES: &[Meas] = &[
    Meas {
        id: "refund_count",
        label: "Refunds",
        expr: "COUNT(*)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Number of refunds issued. One tender per row, so a refund split across two \
               tenders counts twice here and once in orders_refunded.",
    },
    Meas {
        id: "refund_amount",
        label: "Refunded",
        expr: "COALESCE(SUM(r.amount),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Money handed back, by the day it was issued. Tax rides inside it the way it \
               rode inside the bill.",
    },
    Meas {
        id: "cash_refund_amount",
        label: "Cash refunded",
        expr: "COALESCE(SUM(r.amount) FILTER (WHERE r.is_cash),0)",
        kind: ColumnKind::Money,
        joins: &[],
        help: "The cash slice of refund_amount — what physically left a drawer, and what \
               the shift's expected cash is short by.",
    },
    Meas {
        id: "avg_refund",
        label: "Avg refund",
        expr: "COALESCE(AVG(r.amount),0)::bigint",
        kind: ColumnKind::Money,
        joins: &[],
        help: "Average amount per refund.",
    },
    Meas {
        id: "orders_refunded",
        label: "Orders",
        expr: "COUNT(DISTINCT r.order_id)",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Distinct orders that had money returned against them.",
    },
    Meas {
        id: "fully_refunded_orders",
        label: "Fully refunded",
        expr: "COUNT(DISTINCT r.order_id) FILTER (WHERE o.status = 'refunded')",
        kind: ColumnKind::Count,
        joins: &[],
        help: "Orders whose refunds reached the whole bill — the ones no longer counted as \
               sales anywhere.",
    },
];

const REFUND_DIMS: &[Dim] = dims_with_time!(
    "r.issued_at",
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
            id: "cashier",
            label: "Issued by",
            expr: "COALESCE(t.name, 'Unknown')",
            kind: ColumnKind::Label,
            joins: &["cashier"],
            time: false
        },
        Dim {
            id: "refund_reason",
            label: "Reason",
            expr: "r.reason",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
        Dim {
            id: "refund_method",
            label: "Method",
            expr: "r.method",
            kind: ColumnKind::Label,
            joins: &[],
            time: false
        },
        Dim {
            id: "tender_kind",
            label: "Cash or card",
            expr: "CASE WHEN r.is_cash THEN 'Cash' ELSE 'Non-cash' END",
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

const F_REFUND_TENDER: Filter = Filter {
    id: "tender",
    label: "Refund tender",
    help: "How the money went back: cash out of the drawer, or onto a card or wallet.",
    options: &[
        FilterOpt {
            value: "any",
            sql: "",
        },
        FilterOpt {
            value: "cash",
            sql: "AND r.is_cash",
        },
        FilterOpt {
            value: "non_cash",
            sql: "AND NOT r.is_cash",
        },
    ],
    default: "any",
};

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
        id: "ingredient_category",
        sql: "LEFT JOIN ingredient_categories ingc ON ingc.id = ing.category_id",
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
            expr: "COALESCE(ingc.name,\'Uncategorized\')",
            kind: ColumnKind::Label,
            joins: &["ingredient", "ingredient_category"],
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
    Join {
        id: "ingredient_category",
        sql: "LEFT JOIN ingredient_categories ingc ON ingc.id = ing.category_id",
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
            expr: "COALESCE(ingc.name,\'Uncategorized\')",
            kind: ColumnKind::Label,
            joins: &["ingredient", "ingredient_category"],
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
    Join {
        id: "ingredient_category",
        sql: "LEFT JOIN ingredient_categories ingc ON ingc.id = ing.category_id",
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
            expr: "COALESCE(ingc.name,\'Uncategorized\')",
            kind: ColumnKind::Label,
            joins: &["ingredient", "ingredient_category"],
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
               discounts, tips, voids, service charge, and anything counted per order. \
               Revenue here is net of refunds against each sale. Do NOT use for \
               per-product questions — use order_items — or for refunds by the day they \
               were issued — use refunds.",
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
        id: "tables",
        title: "Tables",
        help: "One row per settled dine-in bill that was eaten at a table. Use for table \
               turns, covers, dwell time (seated to paid), revenue per table and per cover, \
               and busiest tables, sections and hours. Counter and delivery sales are not \
               here — use orders.",
        from: "orders o JOIN branch_tables tb ON tb.id = o.table_id",
        branch_col: "o.branch_id",
        time_col: "o.created_at",
        time_is_date: false,
        base_pred: "",
        joins: TABLE_JOINS,
        dims: TABLE_DIMS,
        measures: TABLE_MEASURES,
        filters: &[F_ORDER_STATUS],
        default_measures: &["turns", "covers", "table_revenue"],
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
        id: "refunds",
        title: "Refunds",
        help: "One row per refund — money handed back against a settled order, on the day \
               it was issued. Use for how much was returned, in cash or otherwise, by \
               reason, by who issued it. NOT for revenue: the orders dataset already \
               nets refunds against the sales they were for.",
        from: "order_refunds r JOIN orders o ON o.id = r.order_id",
        branch_col: "r.branch_id",
        time_col: "r.issued_at",
        time_is_date: false,
        base_pred: "",
        joins: REFUND_JOINS,
        dims: REFUND_DIMS,
        measures: REFUND_MEASURES,
        filters: &[F_ORDER_TYPE, F_REFUND_TENDER],
        default_measures: &["refund_count", "refund_amount"],
        default_viz: Viz::Bar,
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
