//! The fixed menu of analytics reports the AI may choose from.
//!
//! The model NEVER writes SQL. Each [`Report`] pairs a natural-language
//! description + typed parameters (what the model fills in) with a *pre-written,
//! parameterized* SQL statement (what the backend runs). The model's entire job
//! is to pick a report id and supply values; those values are always sent to
//! Postgres as bound parameters, so a hostile or hallucinated argument can only
//! ever be a value, never executable SQL.
//!
//! Expansiveness comes from a large menu of authored multi-table reports (and
//! several pre-grouped variants — by branch / day / hour / category / channel /
//! staff), NOT from letting the model compose queries. Adding a report is a
//! single entry in [`REPORTS`]; nothing else changes.
//!
//! Every query runs on the caller's RLS-scoped tenant pool (`src/db.rs`) — so no
//! `org_id` filter is needed — AND is fenced to the caller's accessible branch
//! set via the system-injected `:branch_ids` (see `handlers::accessible_branches`).
//! `:locale` picks translated labels; `:tz` buckets time in the org's timezone.
//! Money columns are integer piastres, matching the rest of the system.

use serde::Serialize;
use utoipa::ToSchema;

/// A typed parameter the model fills in when choosing a report.
#[derive(Clone, Copy)]
pub struct Param {
    pub name: &'static str,
    pub kind: ParamKind,
    pub required: bool,
    pub description: &'static str,
}

#[derive(Clone, Copy, PartialEq)]
pub enum ParamKind {
    /// ISO-8601 date/timestamp, bound as `timestamptz`; guarded `(:x IS NULL OR …)`.
    Date,
    /// A bounded positive integer (e.g. a top-N limit).
    Int { min: i64, max: i64, default: i64 },
}

/// The renderable kind of an output column (money vs count vs label vs a time
/// axis) so the frontend can format it and pick a chart.
#[derive(Clone, Copy, Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ColumnKind {
    /// Integer piastres — render as currency.
    Money,
    /// Integer count.
    Count,
    /// Free text / category label (a natural chart category axis).
    Label,
    /// A date/day bucket (a natural time axis).
    Date,
    /// A ratio/decimal (e.g. a percentage).
    Number,
}

/// One output column: its SQL alias (also the JSON key) and how to render it.
#[derive(Clone, Copy, Debug, Serialize, ToSchema)]
pub struct Column {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: ColumnKind,
}

/// A hint for how the result is best visualized. The frontend may override.
#[derive(Clone, Copy, Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChartHint {
    Table,
    Bar,
    Line,
    Pie,
}

/// A single pre-written report.
pub struct Report {
    pub id: &'static str,
    pub title: &'static str,
    /// Natural-language description handed to the model as the function's
    /// description — what it matches the user's question against. Includes
    /// Arabic hints so Arabic/Egyptian-dialect questions route correctly.
    pub description: &'static str,
    pub params: &'static [Param],
    /// Parameterized SQL with NAMED params (`:from`, `:limit`, plus the
    /// system-injected `:branch_ids` / `:locale` / `:tz`). A single read-only
    /// SELECT; must carry its own `LIMIT`.
    pub sql: &'static str,
    pub columns: &'static [Column],
    pub chart: ChartHint,
}

/// Look a report up by id.
pub fn find(id: &str) -> Option<&'static Report> {
    REPORTS.iter().find(|r| r.id == id)
}

// ── Shared params ───────────────────────────────────────────────────────────

const FROM: Param = Param {
    name: "from",
    kind: ParamKind::Date,
    required: false,
    description: "Start of the period (ISO-8601). Omit for all time.",
};
const TO: Param = Param {
    name: "to",
    kind: ParamKind::Date,
    required: false,
    description: "End of the period (ISO-8601). Omit for up to now.",
};
const LIMIT: Param = Param {
    name: "limit",
    kind: ParamKind::Int {
        min: 1,
        max: 100,
        default: 10,
    },
    required: false,
    description: "How many rows to return (top-N). Default 10.",
};

const PERIOD: &[Param] = &[FROM, TO];
const PERIOD_LIMIT: &[Param] = &[FROM, TO, LIMIT];

// ── Column helpers (const arrays) ───────────────────────────────────────────

const C_REVENUE: Column = Column {
    key: "revenue",
    label: "Revenue",
    kind: ColumnKind::Money,
};
const C_ORDERS: Column = Column {
    key: "orders",
    label: "Orders",
    kind: ColumnKind::Count,
};
const C_QTY: Column = Column {
    key: "quantity",
    label: "Qty sold",
    kind: ColumnKind::Count,
};

/// Reusable SQL fragment: only orders in the caller's accessible branches and
/// the optional period. Every orders-based report inlines this shape.
///
/// (Kept as a doc reference; the fragment is written inline per report because
/// `const` string concatenation isn't available in this position.)
///   o.branch_id = ANY(:branch_ids)
///   AND (:from::timestamptz IS NULL OR o.created_at >= :from)
///   AND (:to::timestamptz   IS NULL OR o.created_at <= :to)
pub static REPORTS: &[Report] = &[
    // ── Sales headline & splits ─────────────────────────────────────────────
    Report {
        id: "sales_summary",
        title: "Sales summary",
        description: "Headline totals across the merchant's branches for a period: \
            completed orders, gross revenue, discounts, tips, and average order value. \
            Use for 'how were sales', 'total revenue', 'مبيعات', 'إجمالي المبيعات', \
            'إيرادات الأسبوع'. For a per-branch split use sales_by_branch instead.",
        params: PERIOD,
        sql: "SELECT \
                COUNT(*) FILTER (WHERE o.status <> 'voided')::bigint AS orders, \
                COALESCE(SUM(o.total_amount)    FILTER (WHERE o.status='completed'),0)::bigint AS revenue, \
                COALESCE(SUM(o.discount_amount) FILTER (WHERE o.status='completed'),0)::bigint AS discounts, \
                COALESCE(SUM(o.tip_amount)      FILTER (WHERE o.status='completed'),0)::bigint AS tips, \
                COALESCE(ROUND(AVG(o.total_amount) FILTER (WHERE o.status='completed')),0)::bigint AS avg_order_value \
              FROM orders o \
              WHERE o.branch_id = ANY(:branch_ids) \
                AND (:from::timestamptz IS NULL OR o.created_at >= :from) \
                AND (:to::timestamptz   IS NULL OR o.created_at <= :to) \
              LIMIT 1",
        columns: &[
            C_ORDERS,
            C_REVENUE,
            Column {
                key: "discounts",
                label: "Discounts",
                kind: ColumnKind::Money,
            },
            Column {
                key: "tips",
                label: "Tips",
                kind: ColumnKind::Money,
            },
            Column {
                key: "avg_order_value",
                label: "Avg order value",
                kind: ColumnKind::Money,
            },
        ],
        chart: ChartHint::Table,
    },
    Report {
        id: "sales_by_branch",
        title: "Sales by branch",
        description: "Revenue and order count PER BRANCH for a period, across every \
            branch the user can access — the default way to compare locations. Use for \
            'sales by branch', 'compare branches/stores', 'which branch is best', \
            'المبيعات حسب الفرع', 'مقارنة الفروع', 'أي فرع الأفضل'.",
        params: PERIOD,
        sql: "SELECT b.name AS branch, \
                     COUNT(o.id) FILTER (WHERE o.status <> 'voided')::bigint AS orders, \
                     COALESCE(SUM(o.total_amount) FILTER (WHERE o.status='completed'),0)::bigint AS revenue \
              FROM branches b \
              LEFT JOIN orders o ON o.branch_id = b.id \
                AND (:from::timestamptz IS NULL OR o.created_at >= :from) \
                AND (:to::timestamptz   IS NULL OR o.created_at <= :to) \
              WHERE b.id = ANY(:branch_ids) \
              GROUP BY b.id, b.name \
              ORDER BY revenue DESC \
              LIMIT 200",
        columns: &[
            Column {
                key: "branch",
                label: "Branch",
                kind: ColumnKind::Label,
            },
            C_ORDERS,
            C_REVENUE,
        ],
        chart: ChartHint::Bar,
    },
    Report {
        id: "sales_by_day",
        title: "Daily sales",
        description: "Day-by-day revenue and order-count time series (merchant timezone). \
            Use for 'sales per day', 'daily revenue', 'trend', 'المبيعات اليومية', \
            'الاتجاه', 'مبيعات كل يوم'.",
        params: PERIOD,
        sql: "SELECT (o.created_at AT TIME ZONE :tz)::date AS day, \
                     COUNT(*)::bigint AS orders, \
                     COALESCE(SUM(o.total_amount),0)::bigint AS revenue \
              FROM orders o \
              WHERE o.status='completed' AND o.branch_id = ANY(:branch_ids) \
                AND (:from::timestamptz IS NULL OR o.created_at >= :from) \
                AND (:to::timestamptz   IS NULL OR o.created_at <= :to) \
              GROUP BY day ORDER BY day LIMIT 400",
        columns: &[
            Column {
                key: "day",
                label: "Day",
                kind: ColumnKind::Date,
            },
            C_ORDERS,
            C_REVENUE,
        ],
        chart: ChartHint::Line,
    },
    Report {
        id: "sales_by_month",
        title: "Monthly sales",
        description: "Month-by-month revenue and order count (merchant timezone). Use \
            for 'monthly sales', 'revenue by month', 'مبيعات شهرية', 'الإيراد الشهري'.",
        params: PERIOD,
        sql: "SELECT to_char(date_trunc('month', o.created_at AT TIME ZONE :tz), 'YYYY-MM') AS month, \
                     COUNT(*)::bigint AS orders, \
                     COALESCE(SUM(o.total_amount),0)::bigint AS revenue \
              FROM orders o \
              WHERE o.status='completed' AND o.branch_id = ANY(:branch_ids) \
                AND (:from::timestamptz IS NULL OR o.created_at >= :from) \
                AND (:to::timestamptz   IS NULL OR o.created_at <= :to) \
              GROUP BY month ORDER BY month LIMIT 120",
        columns: &[
            Column {
                key: "month",
                label: "Month",
                kind: ColumnKind::Label,
            },
            C_ORDERS,
            C_REVENUE,
        ],
        chart: ChartHint::Bar,
    },
    Report {
        id: "sales_by_hour",
        title: "Sales by hour (peak hours)",
        description: "Revenue and orders by hour of day (0–23, merchant timezone) to find \
            peak/busy hours. Use for 'peak hours', 'busiest time', 'sales by hour', \
            'ساعات الذروة', 'أكثر الأوقات ازدحاما', 'المبيعات بالساعة'.",
        params: PERIOD,
        sql: "SELECT lpad(EXTRACT(HOUR FROM o.created_at AT TIME ZONE :tz)::int::text, 2, '0') || ':00' AS hour, \
                     COUNT(*)::bigint AS orders, \
                     COALESCE(SUM(o.total_amount),0)::bigint AS revenue \
              FROM orders o \
              WHERE o.status='completed' AND o.branch_id = ANY(:branch_ids) \
                AND (:from::timestamptz IS NULL OR o.created_at >= :from) \
                AND (:to::timestamptz   IS NULL OR o.created_at <= :to) \
              GROUP BY 1 ORDER BY 1 LIMIT 24",
        columns: &[
            Column {
                key: "hour",
                label: "Hour",
                kind: ColumnKind::Label,
            },
            C_ORDERS,
            C_REVENUE,
        ],
        chart: ChartHint::Bar,
    },
    Report {
        id: "sales_by_weekday",
        title: "Sales by weekday",
        description: "Revenue and orders by day of week (merchant timezone). Use for \
            'best day of the week', 'sales by weekday', 'أفضل يوم', 'المبيعات حسب اليوم'.",
        params: PERIOD,
        sql: "SELECT to_char(o.created_at AT TIME ZONE :tz, 'Dy') AS weekday, \
                     EXTRACT(DOW FROM o.created_at AT TIME ZONE :tz)::int AS dow, \
                     COUNT(*)::bigint AS orders, \
                     COALESCE(SUM(o.total_amount),0)::bigint AS revenue \
              FROM orders o \
              WHERE o.status='completed' AND o.branch_id = ANY(:branch_ids) \
                AND (:from::timestamptz IS NULL OR o.created_at >= :from) \
                AND (:to::timestamptz   IS NULL OR o.created_at <= :to) \
              GROUP BY weekday, dow ORDER BY dow LIMIT 7",
        columns: &[
            Column {
                key: "weekday",
                label: "Weekday",
                kind: ColumnKind::Label,
            },
            C_ORDERS,
            C_REVENUE,
        ],
        chart: ChartHint::Bar,
    },
    // ── Products & categories ───────────────────────────────────────────────
    Report {
        id: "top_products",
        title: "Top products",
        description: "Best-selling menu items ranked by revenue, with quantity. Names are \
            localized. Use for 'top products', 'best sellers', 'what sells most', \
            'أفضل المنتجات', 'الأكثر مبيعا', 'أعلى المنتجات مبيعا'.",
        params: PERIOD_LIMIT,
        sql: "SELECT COALESCE(NULLIF(oi.name_translations->>:locale,''), oi.item_name) AS product, \
                     SUM(oi.quantity)::bigint   AS quantity, \
                     SUM(oi.line_total)::bigint AS revenue \
              FROM order_items oi JOIN orders o ON o.id = oi.order_id \
              WHERE o.status='completed' AND o.branch_id = ANY(:branch_ids) \
                AND (:from::timestamptz IS NULL OR o.created_at >= :from) \
                AND (:to::timestamptz   IS NULL OR o.created_at <= :to) \
              GROUP BY product ORDER BY revenue DESC LIMIT :limit",
        columns: &[
            Column {
                key: "product",
                label: "Product",
                kind: ColumnKind::Label,
            },
            C_QTY,
            C_REVENUE,
        ],
        chart: ChartHint::Bar,
    },
    Report {
        id: "worst_products",
        title: "Slowest products",
        description: "Worst-selling menu items (lowest revenue) over a period — candidates \
            to cut. Localized names. Use for 'worst sellers', 'slowest products', \
            'items that don't sell', 'الأقل مبيعا', 'المنتجات الراكدة', 'الأبطأ مبيعا'.",
        params: PERIOD_LIMIT,
        sql: "SELECT COALESCE(NULLIF(oi.name_translations->>:locale,''), oi.item_name) AS product, \
                     SUM(oi.quantity)::bigint   AS quantity, \
                     SUM(oi.line_total)::bigint AS revenue \
              FROM order_items oi JOIN orders o ON o.id = oi.order_id \
              WHERE o.status='completed' AND o.branch_id = ANY(:branch_ids) \
                AND (:from::timestamptz IS NULL OR o.created_at >= :from) \
                AND (:to::timestamptz   IS NULL OR o.created_at <= :to) \
              GROUP BY product ORDER BY revenue ASC LIMIT :limit",
        columns: &[
            Column {
                key: "product",
                label: "Product",
                kind: ColumnKind::Label,
            },
            C_QTY,
            C_REVENUE,
        ],
        chart: ChartHint::Bar,
    },
    Report {
        id: "top_categories",
        title: "Top categories",
        description: "Revenue and quantity by menu category, ranked. Localized names. Use \
            for 'best category', 'sales by category', 'أفضل قسم', 'المبيعات حسب الفئة', \
            'الأقسام الأعلى'.",
        params: PERIOD_LIMIT,
        sql: "SELECT COALESCE(NULLIF(c.name_translations->>:locale,''), c.name, 'Uncategorized') AS category, \
                     SUM(oi.quantity)::bigint   AS quantity, \
                     SUM(oi.line_total)::bigint AS revenue \
              FROM order_items oi JOIN orders o ON o.id = oi.order_id \
              LEFT JOIN menu_items mi ON mi.id = oi.menu_item_id \
              LEFT JOIN categories c ON c.id = mi.category_id \
              WHERE o.status='completed' AND o.branch_id = ANY(:branch_ids) \
                AND (:from::timestamptz IS NULL OR o.created_at >= :from) \
                AND (:to::timestamptz   IS NULL OR o.created_at <= :to) \
              GROUP BY category ORDER BY revenue DESC LIMIT :limit",
        columns: &[
            Column {
                key: "category",
                label: "Category",
                kind: ColumnKind::Label,
            },
            C_QTY,
            C_REVENUE,
        ],
        chart: ChartHint::Bar,
    },
    Report {
        id: "top_addons",
        title: "Top add-ons",
        description: "Most-sold order add-ons/modifiers by revenue. Localized names. Use \
            for 'top add-ons', 'popular extras/modifiers', 'أشهر الإضافات', 'الإضافات الأكثر مبيعا'.",
        params: PERIOD_LIMIT,
        sql: "SELECT COALESCE(NULLIF(a.name_translations->>:locale,''), a.addon_name) AS addon, \
                     SUM(a.quantity)::bigint AS quantity, \
                     SUM(a.line_total)::bigint AS revenue \
              FROM order_item_addons a \
              JOIN order_items oi ON oi.id = a.order_item_id \
              JOIN orders o ON o.id = oi.order_id \
              WHERE o.status='completed' AND o.branch_id = ANY(:branch_ids) \
                AND (:from::timestamptz IS NULL OR o.created_at >= :from) \
                AND (:to::timestamptz   IS NULL OR o.created_at <= :to) \
              GROUP BY addon ORDER BY revenue DESC LIMIT :limit",
        columns: &[
            Column {
                key: "addon",
                label: "Add-on",
                kind: ColumnKind::Label,
            },
            C_QTY,
            C_REVENUE,
        ],
        chart: ChartHint::Bar,
    },
    // ── Profitability (cost engine) ─────────────────────────────────────────
    Report {
        id: "product_profit",
        title: "Product profitability",
        description: "Top products by PROFIT (revenue minus cost) using the cost engine, \
            with margin %. Localized names. A product whose cost is not fully known \
            shows blank cost/profit/margin (never a guessed zero), matching the \
            profitability page. Use for 'most profitable products', 'margins', \
            'أعلى ربحية', 'هامش الربح', 'المنتجات الأكثر ربحا'.",
        params: PERIOD_LIMIT,
        // Mirrors the menu-profitability engine: non-voided orders, and cost is
        // NULL (unknown) — not 0 — if ANY line for the product lacks a cost, so a
        // partially-costed product is never graded on an understated cost.
        sql: "SELECT COALESCE(NULLIF(oi.name_translations->>:locale,''), oi.item_name) AS product, \
                     SUM(oi.line_total)::bigint AS revenue, \
                     (CASE WHEN bool_or(oi.line_cost IS NULL) THEN NULL ELSE SUM(oi.line_cost) END)::bigint AS cost, \
                     (CASE WHEN bool_or(oi.line_cost IS NULL) THEN NULL ELSE SUM(oi.line_total) - SUM(oi.line_cost) END)::bigint AS profit, \
                     (CASE WHEN bool_or(oi.line_cost IS NULL) THEN NULL \
                           ELSE ROUND(100.0 * (SUM(oi.line_total) - SUM(oi.line_cost)) / NULLIF(SUM(oi.line_total),0), 1) END)::float8 AS margin_pct \
              FROM order_items oi JOIN orders o ON o.id = oi.order_id \
              WHERE o.status <> 'voided' AND o.branch_id = ANY(:branch_ids) \
                AND (:from::timestamptz IS NULL OR o.created_at >= :from) \
                AND (:to::timestamptz   IS NULL OR o.created_at <= :to) \
              GROUP BY product ORDER BY profit DESC NULLS LAST LIMIT :limit",
        columns: &[
            Column {
                key: "product",
                label: "Product",
                kind: ColumnKind::Label,
            },
            C_REVENUE,
            Column {
                key: "cost",
                label: "Cost",
                kind: ColumnKind::Money,
            },
            Column {
                key: "profit",
                label: "Profit",
                kind: ColumnKind::Money,
            },
            Column {
                key: "margin_pct",
                label: "Margin %",
                kind: ColumnKind::Number,
            },
        ],
        chart: ChartHint::Bar,
    },
    Report {
        id: "repricing_opportunities",
        title: "Repricing opportunities",
        description: "Menu items whose current price gives a margin BELOW the org's \
            target, with a SUGGESTED price that restores the target margin. Costs come \
            from the current recipe rollup; an item without a complete recipe cost is \
            skipped (no guessed cost), so with fewer costed recipes it simply covers \
            fewer items — never a wrong suggestion. Use for 'where should I raise \
            prices', 'repricing', 'underpriced items', 'suggest new prices', 'pricing \
            suggestions', 'أين أرفع الأسعار', 'اقترح تسعير', 'المنتجات المنخفضة السعر', \
            'تحسين التسعير'.",
        params: &[LIMIT],
        // Org-level target-restoring suggestion. Mirrors costing::org_sku_costs
        // (never-default-to-0: an uncosted/unlinked ingredient is EXCLUDED and the
        // SKU marked incomplete → skipped) and margin_targets (branch NULL = org
        // default, builtin 60%). suggested = ceil(cost / (1 - target)) to whole EGP.
        sql: "WITH expanded AS ( \
                 SELECT mi.id AS menu_item_id, \
                        COALESCE(NULLIF(mi.name_translations->>:locale,''), mi.name) AS product, \
                        COALESCE(sz.label, 'one_size') AS size_label, \
                        COALESCE(sz.price_override, mi.base_price)::bigint AS price \
                 FROM menu_items mi \
                 LEFT JOIN item_sizes sz ON sz.menu_item_id = mi.id AND sz.is_active = true \
                 WHERE mi.deleted_at IS NULL AND mi.is_active = true \
              ), costed AS ( \
                 SELECT e.product, e.price, r.cost, r.incomplete \
                 FROM expanded e CROSS JOIN LATERAL ( \
                   SELECT SUM(rc.quantity_used * ing.cost_per_unit) FILTER (WHERE ing.cost_per_unit IS NOT NULL) AS cost, \
                          bool_or(rc.org_ingredient_id IS NULL OR ing.cost_per_unit IS NULL) AS incomplete \
                   FROM menu_item_recipes rc \
                   LEFT JOIN org_ingredients ing ON ing.id = rc.org_ingredient_id \
                   WHERE rc.menu_item_id = e.menu_item_id AND COALESCE(rc.size_label,'one_size') = e.size_label \
                 ) r \
                 WHERE r.cost IS NOT NULL AND NOT COALESCE(r.incomplete, true) \
              ), tgt AS ( \
                 SELECT COALESCE((SELECT target_pct FROM margin_targets WHERE branch_id IS NULL LIMIT 1), 60)::numeric AS target_pct \
              ) \
              SELECT c.product, \
                     c.price AS current_price, \
                     ROUND(c.cost)::bigint AS cost, \
                     ROUND(100.0 * (c.price - c.cost) / NULLIF(c.price,0), 1)::float8 AS margin_pct, \
                     t.target_pct::float8 AS target_pct, \
                     (CEIL(c.cost / (1 - t.target_pct/100.0) / 100.0) * 100)::bigint AS suggested_price, \
                     ((CEIL(c.cost / (1 - t.target_pct/100.0) / 100.0) * 100) - c.price)::bigint AS uplift \
              FROM costed c CROSS JOIN tgt t \
              WHERE c.price > 0 AND (100.0 * (c.price - c.cost) / c.price) < t.target_pct \
              ORDER BY uplift DESC LIMIT :limit",
        columns: &[
            Column {
                key: "product",
                label: "Product",
                kind: ColumnKind::Label,
            },
            Column {
                key: "current_price",
                label: "Current price",
                kind: ColumnKind::Money,
            },
            Column {
                key: "cost",
                label: "Cost",
                kind: ColumnKind::Money,
            },
            Column {
                key: "margin_pct",
                label: "Margin %",
                kind: ColumnKind::Number,
            },
            Column {
                key: "target_pct",
                label: "Target %",
                kind: ColumnKind::Number,
            },
            Column {
                key: "suggested_price",
                label: "Suggested price",
                kind: ColumnKind::Money,
            },
            Column {
                key: "uplift",
                label: "Price uplift",
                kind: ColumnKind::Money,
            },
        ],
        chart: ChartHint::Table,
    },
    // ── Payments, discounts, tips ───────────────────────────────────────────
    Report {
        id: "payment_method_breakdown",
        title: "Payment methods",
        description: "Revenue split by payment method (cash, card, …) — localized labels. \
            Use for 'cash vs card', 'how customers pay', 'payment breakdown', \
            'كاش ولا فيزا', 'طرق الدفع', 'الدفع نقدا أم بالبطاقة'.",
        params: PERIOD,
        sql: "SELECT COALESCE(NULLIF(pm.label_translations->>:locale,''), pm.name, op.method) AS method, \
                     COUNT(*)::bigint AS payments, \
                     COALESCE(SUM(op.amount),0)::bigint AS revenue \
              FROM order_payments op JOIN orders o ON o.id = op.order_id \
              LEFT JOIN org_payment_methods pm ON pm.name = op.method \
              WHERE o.status='completed' AND o.branch_id = ANY(:branch_ids) \
                AND (:from::timestamptz IS NULL OR o.created_at >= :from) \
                AND (:to::timestamptz   IS NULL OR o.created_at <= :to) \
              GROUP BY 1 ORDER BY revenue DESC LIMIT 30",
        columns: &[
            Column {
                key: "method",
                label: "Method",
                kind: ColumnKind::Label,
            },
            Column {
                key: "payments",
                label: "Payments",
                kind: ColumnKind::Count,
            },
            C_REVENUE,
        ],
        chart: ChartHint::Pie,
    },
    Report {
        id: "discount_summary",
        title: "Discounts",
        description: "How much was given away in discounts: discounted order count, total \
            discount, and share of revenue. Use for 'discounts given', 'how much discount', \
            'الخصومات', 'إجمالي الخصم', 'قيمة الخصومات'.",
        params: PERIOD,
        sql: "SELECT COUNT(*) FILTER (WHERE o.discount_amount > 0)::bigint AS discounted_orders, \
                     COALESCE(SUM(o.discount_amount),0)::bigint AS total_discount, \
                     COALESCE(SUM(o.total_amount),0)::bigint AS net_revenue \
              FROM orders o \
              WHERE o.status='completed' AND o.branch_id = ANY(:branch_ids) \
                AND (:from::timestamptz IS NULL OR o.created_at >= :from) \
                AND (:to::timestamptz   IS NULL OR o.created_at <= :to) \
              LIMIT 1",
        columns: &[
            Column {
                key: "discounted_orders",
                label: "Discounted orders",
                kind: ColumnKind::Count,
            },
            Column {
                key: "total_discount",
                label: "Total discount",
                kind: ColumnKind::Money,
            },
            Column {
                key: "net_revenue",
                label: "Net revenue",
                kind: ColumnKind::Money,
            },
        ],
        chart: ChartHint::Table,
    },
    Report {
        id: "top_discounts",
        title: "Top discounts used",
        description: "Which named discounts/promos were used most, by total amount given. \
            Localized names. Use for 'most used discount', 'promo usage', 'العروض الأكثر استخداما', \
            'الكوبونات'.",
        params: PERIOD_LIMIT,
        sql: "SELECT COALESCE(NULLIF(d.name_translations->>:locale,''), d.name) AS discount, \
                     COUNT(*)::bigint AS times_used, \
                     COALESCE(SUM(o.discount_amount),0)::bigint AS total_given \
              FROM orders o JOIN discounts d ON d.id = o.discount_id \
              WHERE o.status='completed' AND o.branch_id = ANY(:branch_ids) \
                AND (:from::timestamptz IS NULL OR o.created_at >= :from) \
                AND (:to::timestamptz   IS NULL OR o.created_at <= :to) \
              GROUP BY discount ORDER BY total_given DESC LIMIT :limit",
        columns: &[
            Column {
                key: "discount",
                label: "Discount",
                kind: ColumnKind::Label,
            },
            Column {
                key: "times_used",
                label: "Times used",
                kind: ColumnKind::Count,
            },
            Column {
                key: "total_given",
                label: "Total given",
                kind: ColumnKind::Money,
            },
        ],
        chart: ChartHint::Bar,
    },
    // ── Order type & delivery ───────────────────────────────────────────────
    Report {
        id: "order_type_breakdown",
        title: "Order types",
        description: "Revenue and orders by order type (dine-in, takeaway, delivery). Use \
            for 'dine-in vs delivery', 'order types', 'صالة أم توصيل', 'أنواع الطلبات', \
            'محلي أم دليفري'.",
        params: PERIOD,
        sql: "SELECT COALESCE(o.order_type, 'unknown') AS order_type, \
                     COUNT(*)::bigint AS orders, \
                     COALESCE(SUM(o.total_amount),0)::bigint AS revenue \
              FROM orders o \
              WHERE o.status='completed' AND o.branch_id = ANY(:branch_ids) \
                AND (:from::timestamptz IS NULL OR o.created_at >= :from) \
                AND (:to::timestamptz   IS NULL OR o.created_at <= :to) \
              GROUP BY order_type ORDER BY revenue DESC LIMIT 20",
        columns: &[
            Column {
                key: "order_type",
                label: "Order type",
                kind: ColumnKind::Label,
            },
            C_ORDERS,
            C_REVENUE,
        ],
        chart: ChartHint::Pie,
    },
    Report {
        id: "delivery_channel_breakdown",
        title: "Delivery channels",
        description: "Delivery orders split by channel (in-mall, outside, umbrella, pickup): \
            count, revenue, and total delivery fees. Use for 'delivery channels', \
            'delivery breakdown', 'قنوات التوصيل', 'الدليفري حسب القناة'.",
        params: PERIOD,
        sql: "SELECT d.channel::text AS channel, \
                     COUNT(*)::bigint AS orders, \
                     COALESCE(SUM(o.total_amount),0)::bigint AS revenue, \
                     COALESCE(SUM(o.delivery_fee),0)::bigint AS delivery_fees \
              FROM orders o JOIN delivery_orders d ON d.id = o.delivery_order_id \
              WHERE o.status='completed' AND o.branch_id = ANY(:branch_ids) \
                AND (:from::timestamptz IS NULL OR o.created_at >= :from) \
                AND (:to::timestamptz   IS NULL OR o.created_at <= :to) \
              GROUP BY d.channel ORDER BY revenue DESC LIMIT 20",
        columns: &[
            Column {
                key: "channel",
                label: "Channel",
                kind: ColumnKind::Label,
            },
            C_ORDERS,
            C_REVENUE,
            Column {
                key: "delivery_fees",
                label: "Delivery fees",
                kind: ColumnKind::Money,
            },
        ],
        chart: ChartHint::Bar,
    },
    // ── Staff performance ───────────────────────────────────────────────────
    Report {
        id: "waiter_performance",
        title: "Waiter performance",
        description: "Sales attributed to each waiter: orders and revenue. Use for \
            'waiter performance', 'sales by waiter', 'أداء الويتر', 'مبيعات كل ويتر', \
            'الجرسون الأفضل'.",
        params: PERIOD_LIMIT,
        sql: "SELECT w.name AS waiter, \
                     COUNT(*)::bigint AS orders, \
                     COALESCE(SUM(o.total_amount),0)::bigint AS revenue \
              FROM orders o JOIN users w ON w.id = o.waiter_id \
              WHERE o.status='completed' AND o.branch_id = ANY(:branch_ids) \
                AND (:from::timestamptz IS NULL OR o.created_at >= :from) \
                AND (:to::timestamptz   IS NULL OR o.created_at <= :to) \
              GROUP BY w.name ORDER BY revenue DESC LIMIT :limit",
        columns: &[
            Column {
                key: "waiter",
                label: "Waiter",
                kind: ColumnKind::Label,
            },
            C_ORDERS,
            C_REVENUE,
        ],
        chart: ChartHint::Bar,
    },
    Report {
        id: "cashier_performance",
        title: "Cashier performance",
        description: "Sales rung up by each cashier/teller: completed orders, revenue, and \
            voids. Use for 'cashier performance', 'sales by cashier', 'teller stats', \
            'أداء الكاشير', 'مبيعات كل كاشير'.",
        params: PERIOD_LIMIT,
        sql: "SELECT u.name AS cashier, \
                     COUNT(*) FILTER (WHERE o.status='completed')::bigint AS orders, \
                     COALESCE(SUM(o.total_amount) FILTER (WHERE o.status='completed'),0)::bigint AS revenue, \
                     COUNT(*) FILTER (WHERE o.status='voided')::bigint AS voids \
              FROM orders o JOIN users u ON u.id = o.teller_id \
              WHERE o.branch_id = ANY(:branch_ids) \
                AND (:from::timestamptz IS NULL OR o.created_at >= :from) \
                AND (:to::timestamptz   IS NULL OR o.created_at <= :to) \
              GROUP BY u.name ORDER BY revenue DESC LIMIT :limit",
        columns: &[
            Column {
                key: "cashier",
                label: "Cashier",
                kind: ColumnKind::Label,
            },
            C_ORDERS,
            C_REVENUE,
            Column {
                key: "voids",
                label: "Voids",
                kind: ColumnKind::Count,
            },
        ],
        chart: ChartHint::Bar,
    },
    // ── Voids & operations ──────────────────────────────────────────────────
    Report {
        id: "void_summary",
        title: "Voided orders",
        description: "Voided/cancelled orders by reason: count and value. Use for \
            'voids', 'cancelled orders', 'why orders were voided', 'الأوردرات الملغية', \
            'أسباب الإلغاء', 'المرتجعات'.",
        params: PERIOD,
        sql: "SELECT COALESCE(o.void_reason::text, 'unspecified') AS reason, \
                     COUNT(*)::bigint AS voids, \
                     COALESCE(SUM(o.total_amount),0)::bigint AS value \
              FROM orders o \
              WHERE o.status='voided' AND o.branch_id = ANY(:branch_ids) \
                AND (:from::timestamptz IS NULL OR o.created_at >= :from) \
                AND (:to::timestamptz   IS NULL OR o.created_at <= :to) \
              GROUP BY reason ORDER BY voids DESC LIMIT 30",
        columns: &[
            Column {
                key: "reason",
                label: "Reason",
                kind: ColumnKind::Label,
            },
            Column {
                key: "voids",
                label: "Voids",
                kind: ColumnKind::Count,
            },
            Column {
                key: "value",
                label: "Value",
                kind: ColumnKind::Money,
            },
        ],
        chart: ChartHint::Bar,
    },
    // ── Inventory ───────────────────────────────────────────────────────────
    Report {
        id: "inventory_valuation",
        title: "Inventory valuation by branch",
        description: "Current stock value per branch (sum of stock × unit cost). Use for \
            'inventory value', 'stock on hand value', 'قيمة المخزون', 'قيمة المخزون بالفرع'.",
        params: &[],
        sql: "SELECT b.name AS branch, \
                     COALESCE(ROUND(SUM(bi.current_stock * bi.cost_per_unit)),0)::bigint AS stock_value, \
                     COUNT(*)::bigint AS items \
              FROM branch_inventory bi JOIN branches b ON b.id = bi.branch_id \
              WHERE bi.branch_id = ANY(:branch_ids) \
              GROUP BY b.name ORDER BY stock_value DESC LIMIT 200",
        columns: &[
            Column {
                key: "branch",
                label: "Branch",
                kind: ColumnKind::Label,
            },
            Column {
                key: "stock_value",
                label: "Stock value",
                kind: ColumnKind::Money,
            },
            Column {
                key: "items",
                label: "Items",
                kind: ColumnKind::Count,
            },
        ],
        chart: ChartHint::Bar,
    },
    Report {
        id: "low_stock",
        title: "Low stock items",
        description: "Items at or below their reorder threshold — what to restock. Use for \
            'low stock', 'what to reorder', 'running out', 'المخزون المنخفض', 'نواقص المخزون', \
            'اللي قرب يخلص'.",
        params: &[LIMIT],
        sql: "SELECT ing.name AS item, b.name AS branch, \
                     ROUND(bi.current_stock, 2)::float8 AS current_stock, \
                     ROUND(bi.reorder_threshold, 2)::float8 AS reorder_at \
              FROM branch_inventory bi \
              JOIN branches b ON b.id = bi.branch_id \
              JOIN org_ingredients ing ON ing.id = bi.org_ingredient_id \
              WHERE bi.branch_id = ANY(:branch_ids) \
                AND bi.reorder_threshold IS NOT NULL \
                AND bi.current_stock <= bi.reorder_threshold \
              ORDER BY (bi.current_stock - bi.reorder_threshold) ASC LIMIT :limit",
        columns: &[
            Column {
                key: "item",
                label: "Item",
                kind: ColumnKind::Label,
            },
            Column {
                key: "branch",
                label: "Branch",
                kind: ColumnKind::Label,
            },
            Column {
                key: "current_stock",
                label: "Current",
                kind: ColumnKind::Number,
            },
            Column {
                key: "reorder_at",
                label: "Reorder at",
                kind: ColumnKind::Number,
            },
        ],
        chart: ChartHint::Table,
    },
    Report {
        id: "waste_summary",
        title: "Waste by item",
        description: "Wasted/spoiled inventory over a period, by item: quantity and cost. \
            Use for 'waste', 'spoilage', 'wasted stock', 'الهدر', 'التالف', 'الفاقد'.",
        params: PERIOD_LIMIT,
        sql: "SELECT ing.name AS item, \
                     ROUND(SUM(ABS(im.quantity)), 2)::float8 AS quantity, \
                     COALESCE(ROUND(SUM(ABS(im.quantity) * COALESCE(im.unit_cost,0))),0)::bigint AS cost \
              FROM inventory_movements im \
              JOIN org_ingredients ing ON ing.id = im.org_ingredient_id \
              WHERE im.type = 'waste' AND im.branch_id = ANY(:branch_ids) \
                AND (:from::timestamptz IS NULL OR im.created_at >= :from) \
                AND (:to::timestamptz   IS NULL OR im.created_at <= :to) \
              GROUP BY ing.name ORDER BY cost DESC LIMIT :limit",
        columns: &[
            Column {
                key: "item",
                label: "Item",
                kind: ColumnKind::Label,
            },
            Column {
                key: "quantity",
                label: "Qty wasted",
                kind: ColumnKind::Number,
            },
            Column {
                key: "cost",
                label: "Cost",
                kind: ColumnKind::Money,
            },
        ],
        chart: ChartHint::Bar,
    },
];
