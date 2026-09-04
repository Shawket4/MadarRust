//! Curated metrics — named [`QuerySpec`]s with a title, a description and a
//! sensible default window.
//!
//! A preset is **not** a second query engine. It is a spec like any other, and
//! it compiles and executes through exactly the same path as one the AI agent
//! composed. That is the whole point: there is one place where a metric can be
//! wrong, and adding a metric is one entry in [`PRESETS`].
//!
//! These serve three consumers at once:
//!
//!   * the **dashboard widget picker** — this list *is* the widget catalog;
//!   * the **AI agent** — as a one-call shortcut for the common questions, and
//!     as worked examples of what a good spec looks like;
//!   * **default dashboards** — assembled from preset ids in [`DEFAULT_BOARDS`].

use std::collections::BTreeMap;

use super::spec::{Period, PeriodPreset, QuerySpec, Sort, TopPer, Transform};
use super::types::{Dir, Viz};

/// A named, ready-to-run metric.
pub struct Preset {
    pub id: &'static str,
    pub title: &'static str,
    /// What it measures, in one sentence. Shown in the widget picker and handed
    /// to the model, so it must be precise about what is included.
    pub description: &'static str,
    /// Grouping for the picker UI.
    pub category: &'static str,
    pub dataset: &'static str,
    pub dimensions: &'static [&'static str],
    pub measures: &'static [&'static str],
    pub filters: &'static [(&'static str, &'static str)],
    pub sort: Option<(&'static str, Dir)>,
    /// Rank within each value of this dimension, keeping N rows per group.
    pub top_per: Option<(&'static str, u32)>,
    pub limit: u32,
    pub viz: Viz,
    pub share: bool,
    /// The window used when a caller names none. A widget stores its own.
    pub default_period: PeriodPreset,
    /// Permission resource required to see it, checked with the `read` action.
    pub permission: &'static str,
}

impl Preset {
    /// Materialize as a spec. `period` overrides the preset's default — which is
    /// what lets one preset back a "today" KPI card and a "last quarter" report.
    pub fn to_spec(&self, period: Option<Period>) -> QuerySpec {
        QuerySpec {
            dataset: self.dataset.to_string(),
            dimensions: self.dimensions.iter().map(|s| s.to_string()).collect(),
            measures: self.measures.iter().map(|s| s.to_string()).collect(),
            filters: self
                .filters
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
            period: period.unwrap_or_else(|| Period::preset(self.default_period)),
            sort: self.sort.map(|(m, d)| Sort {
                measure: m.to_string(),
                dir: d,
            }),
            limit: Some(self.limit),
            compare: Default::default(),
            transform: Transform {
                share: self.share,
                cumulative: false,
                top_per: self.top_per.map(|(dimension, n)| TopPer {
                    dimension: dimension.to_string(),
                    n,
                }),
            },
            having_min: None,
            viz: Some(self.viz),
            branch: None,
        }
    }
}

pub fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

/// Shorthand for the common shape, so the table below stays readable.
macro_rules! preset {
    (
        $id:literal, $title:literal, $cat:literal, $perm:literal,
        $ds:literal, dims: [$($d:literal),*], measures: [$($m:literal),*],
        filters: [$(($fk:literal, $fv:literal)),*],
        sort: $sort:expr, limit: $limit:expr, viz: $viz:expr,
        period: $period:expr, share: $share:expr,
        $desc:literal
    ) => {
        Preset {
            id: $id, title: $title, description: $desc, category: $cat,
            dataset: $ds, dimensions: &[$($d),*], measures: &[$($m),*],
            filters: &[$(($fk, $fv)),*], sort: $sort, top_per: None, limit: $limit, viz: $viz,
            share: $share, default_period: $period, permission: $perm,
        }
    };
}

pub const PRESETS: &[Preset] = &[
    // ── Sales headlines ──────────────────────────────────────────────────────
    preset!("revenue_total", "Revenue", "Sales", "reports", "orders",
        dims: [], measures: ["revenue"], filters: [],
        sort: None, limit: 1, viz: Viz::Kpi, period: PeriodPreset::Today, share: false,
        "Total revenue for the period. Order totals after discount, including tax and delivery fees. Excludes voided and refunded orders."),
    preset!("order_count_total", "Orders", "Sales", "reports", "orders",
        dims: [], measures: ["order_count"], filters: [],
        sort: None, limit: 1, viz: Viz::Kpi, period: PeriodPreset::Today, share: false,
        "Number of orders taken in the period, excluding voided and refunded ones."),
    preset!("avg_ticket", "Average ticket", "Sales", "reports", "orders",
        dims: [], measures: ["avg_order_value"], filters: [],
        sort: None, limit: 1, viz: Viz::Kpi, period: PeriodPreset::Today, share: false,
        "Average order total — the average amount a customer spends per visit."),
    preset!("sales_summary", "Sales summary", "Sales", "reports", "orders",
        dims: [], measures: ["order_count", "revenue", "avg_order_value", "discount_total", "tip_total"],
        filters: [], sort: None, limit: 1, viz: Viz::Table, period: PeriodPreset::Today, share: false,
        "Headline sales figures for the period: orders, revenue, average ticket, discounts given and tips collected."),
    preset!("sales_by_day", "Revenue by day", "Sales", "reports", "orders",
        dims: ["day"], measures: ["revenue", "order_count"], filters: [],
        sort: None, limit: 180, viz: Viz::Line, period: PeriodPreset::Last30Days, share: false,
        "Daily revenue and order count — the sales trend over time."),
    preset!("sales_by_hour", "Revenue by hour", "Sales", "reports", "orders",
        dims: ["hour"], measures: ["revenue", "order_count"], filters: [],
        sort: None, limit: 24, viz: Viz::Bar, period: PeriodPreset::Last30Days, share: false,
        "Revenue by hour of day — when the rushes are, for staffing and prep."),
    preset!("sales_by_weekday", "Revenue by weekday", "Sales", "reports", "orders",
        dims: ["weekday"], measures: ["revenue", "order_count"], filters: [],
        sort: None, limit: 7, viz: Viz::Bar, period: PeriodPreset::Last30Days, share: false,
        "Revenue by day of the week — which days carry the business."),
    preset!("sales_by_branch", "Revenue by branch", "Sales", "reports", "orders",
        dims: ["branch"], measures: ["revenue", "order_count", "avg_order_value"], filters: [],
        sort: Some(("revenue", Dir::Desc)), limit: 50, viz: Viz::Bar, period: PeriodPreset::Last30Days, share: false,
        "Revenue, orders and average ticket for every branch, ranked."),
    preset!("peak_hours_heatmap", "Busiest times", "Sales", "reports", "orders",
        dims: ["weekday", "hour"], measures: ["revenue"], filters: [],
        sort: None, limit: 200, viz: Viz::Heatmap, period: PeriodPreset::Last30Days, share: false,
        "Revenue by weekday and hour as a grid — the full picture of when the restaurant is busy."),
    preset!("sales_by_order_type", "Dine-in vs delivery", "Sales", "reports", "orders",
        dims: ["order_type"], measures: ["revenue", "order_count", "avg_order_value"], filters: [],
        sort: Some(("revenue", Dir::Desc)), limit: 10, viz: Viz::Donut, period: PeriodPreset::Last30Days, share: true,
        "Revenue split between dine-in and delivery orders."),
    preset!("sales_by_channel", "Delivery channels", "Sales", "reports", "orders",
        dims: ["delivery_channel"], measures: ["revenue", "order_count", "delivery_fees"],
        filters: [("order_type", "delivery")], sort: Some(("revenue", Dir::Desc)), limit: 10,
        viz: Viz::Donut, period: PeriodPreset::Last30Days, share: true,
        "Delivery revenue by channel: in-mall, outside, umbrella aggregators and pickup."),
    // ── Tips ─────────────────────────────────────────────────────────────────
    preset!("tips_total", "Tips", "Sales", "reports", "orders",
        dims: [], measures: ["tip_total", "tip_rate", "avg_tip"], filters: [],
        sort: None, limit: 1, viz: Viz::Kpi, period: PeriodPreset::Today, share: false,
        "Tips collected in the period, what share of revenue they represent, and the average tip on the orders that were tipped."),
    preset!("tips_by_day", "Tips over time", "Sales", "reports", "orders",
        dims: ["day"], measures: ["tip_total", "tip_rate"], filters: [],
        sort: None, limit: 180, viz: Viz::Line, period: PeriodPreset::Last30Days, share: false,
        "Daily tips and the tip rate — whether service is being rewarded consistently or only on busy days."),
    preset!("tips_by_waiter", "Tips by waiter", "Staff", "reports", "orders",
        dims: ["waiter"], measures: ["tip_total", "tip_rate", "tipped_order_count", "order_count"], filters: [],
        sort: Some(("tip_total", Dir::Desc)), limit: 50, viz: Viz::Bar, period: PeriodPreset::Last30Days, share: false,
        "Tips earned per waiter, with how many of their orders were tipped at all."),
    preset!("cash_vs_card_tips", "Cash tips", "Cash control", "reports", "orders",
        dims: ["day"], measures: ["tip_total", "cash_tip_total"], filters: [],
        sort: None, limit: 90, viz: Viz::Line, period: PeriodPreset::Last30Days, share: false,
        "Total tips against the cash portion — cash tips leave the drawer, so they affect the shift count as well as payroll."),
    // ── Refunds ──────────────────────────────────────────────────────────────
    preset!("refunds_by_day", "Refunds", "Control", "reports", "orders",
        dims: ["day"], measures: ["refund_count", "refund_amount"],
        filters: [("status", "refunded")], sort: None, limit: 90, viz: Viz::Line,
        period: PeriodPreset::Last30Days, share: false,
        "Refunded orders and their value per day."),
    // ── Products ─────────────────────────────────────────────────────────────
    preset!("top_products", "Top products", "Products", "reports", "order_items",
        dims: ["product"], measures: ["units_sold", "item_revenue"], filters: [],
        sort: Some(("item_revenue", Dir::Desc)), limit: 10, viz: Viz::Bar, period: PeriodPreset::Last30Days, share: false,
        "Best-selling products by revenue, with units sold."),
    preset!("worst_products", "Slowest products", "Products", "reports", "order_items",
        dims: ["product"], measures: ["units_sold", "item_revenue"], filters: [],
        sort: Some(("units_sold", Dir::Asc)), limit: 10, viz: Viz::Row, period: PeriodPreset::Last30Days, share: false,
        "Products selling the fewest units — menu-pruning candidates."),
    preset!("top_categories", "Top categories", "Products", "reports", "order_items",
        dims: ["category"], measures: ["item_revenue", "units_sold"], filters: [],
        sort: Some(("item_revenue", Dir::Desc)), limit: 20, viz: Viz::Donut, period: PeriodPreset::Last30Days, share: true,
        "Revenue by menu category, with each category's share of the total."),
    preset!("product_profit", "Most profitable products", "Products", "reports", "order_items",
        dims: ["product"], measures: ["item_revenue", "item_cost", "item_profit", "margin_pct"], filters: [],
        sort: Some(("item_profit", Dir::Desc)), limit: 20, viz: Viz::Table, period: PeriodPreset::Last30Days, share: false,
        "Products ranked by gross profit, with cost and margin. Cost is NULL where a line has no cost snapshot."),
    preset!("thin_margin_products", "Thinnest margins", "Products", "reports", "order_items",
        dims: ["product"], measures: ["margin_pct", "item_revenue", "item_profit"], filters: [],
        sort: Some(("margin_pct", Dir::Asc)), limit: 20, viz: Viz::Table, period: PeriodPreset::Last30Days, share: false,
        "Products with the lowest gross margin — repricing or recipe-cost candidates."),
    // Written out rather than via the macro because it is the one preset that
    // needs `top_per`: rank products *within* each branch and keep the winner.
    Preset {
        id: "best_seller_per_branch",
        title: "Best seller per branch",
        description: "The top-selling product in each branch, ranked by revenue within that branch.",
        category: "Products",
        dataset: "order_items",
        dimensions: &["branch", "product"],
        measures: &["item_revenue", "units_sold"],
        filters: &[],
        sort: Some(("item_revenue", Dir::Desc)),
        top_per: Some(("branch", 1)),
        limit: 200,
        viz: Viz::Table,
        share: false,
        default_period: PeriodPreset::Last30Days,
        permission: "reports",
    },
    // ── Payments & cash ──────────────────────────────────────────────────────
    preset!("payment_mix", "Payment methods", "Payments", "reports", "payments",
        dims: ["payment_method"], measures: ["paid_amount", "payment_count"], filters: [],
        sort: Some(("paid_amount", Dir::Desc)), limit: 20, viz: Viz::Donut, period: PeriodPreset::Today, share: true,
        "How customers paid, by amount tendered. A split-tender order appears under each method used."),
    preset!("cash_vs_card", "Cash vs non-cash", "Payments", "reports", "payments",
        dims: ["tender_kind"], measures: ["paid_amount"], filters: [],
        sort: Some(("paid_amount", Dir::Desc)), limit: 5, viz: Viz::Donut, period: PeriodPreset::Last30Days, share: true,
        "The cash share of takings — the figure that drives drawer and banking load."),
    preset!("drawer_variance", "Drawer variance", "Cash control", "shifts", "shifts",
        dims: ["teller"], measures: ["shift_count", "abs_discrepancy", "short_count"],
        filters: [("shift_status", "closed")], sort: Some(("abs_discrepancy", Dir::Desc)), limit: 30,
        viz: Viz::Bar, period: PeriodPreset::Last30Days, share: false,
        "Cash variance by teller. Uses absolute variance so overs and shorts do not cancel out."),
    preset!("shift_cash_summary", "Shift cash summary", "Cash control", "shifts", "shifts",
        dims: ["day"], measures: ["shift_count", "declared_cash", "system_cash", "discrepancy"],
        filters: [("shift_status", "closed")], sort: None, limit: 90, viz: Viz::Line,
        period: PeriodPreset::Last30Days, share: false,
        "Declared versus expected cash per day, and the resulting discrepancy."),
    // ── Discounts, voids and control ─────────────────────────────────────────
    preset!("discount_usage", "Discounts given", "Control", "reports", "orders",
        dims: ["discount_name"], measures: ["discount_total", "order_count", "discount_rate"],
        filters: [("discounted", "yes")], sort: Some(("discount_total", Dir::Desc)), limit: 20,
        viz: Viz::Bar, period: PeriodPreset::Last30Days, share: false,
        "Which discounts are being used, how much they give away, and at what rate."),
    preset!("voids_by_reason", "Voids by reason", "Control", "reports", "orders",
        dims: ["void_reason"], measures: ["void_count", "void_amount"],
        filters: [("status", "voided")], sort: Some(("void_amount", Dir::Desc)), limit: 10,
        viz: Viz::Bar, period: PeriodPreset::Last30Days, share: false,
        "Voided orders grouped by the reason given, with the value voided."),
    preset!("voids_by_cashier", "Voids by cashier", "Control", "reports", "orders",
        dims: ["cashier"], measures: ["void_count", "void_amount"],
        filters: [("status", "voided")], sort: Some(("void_amount", Dir::Desc)), limit: 30,
        viz: Viz::Bar, period: PeriodPreset::Last30Days, share: false,
        "Which staff void the most, by count and value — a standard loss-prevention check."),
    // ── Staff performance ────────────────────────────────────────────────────
    preset!("waiter_performance", "Waiter performance", "Staff", "reports", "orders",
        dims: ["waiter"], measures: ["order_count", "revenue", "avg_order_value", "tip_total"], filters: [],
        sort: Some(("revenue", Dir::Desc)), limit: 50, viz: Viz::Table, period: PeriodPreset::Last30Days, share: false,
        "Orders, revenue, average ticket and tips per waiter."),
    preset!("cashier_performance", "Cashier performance", "Staff", "reports", "orders",
        dims: ["cashier"], measures: ["order_count", "revenue", "avg_order_value"], filters: [],
        sort: Some(("revenue", Dir::Desc)), limit: 50, viz: Viz::Table, period: PeriodPreset::Last30Days, share: false,
        "Orders and revenue rung up per cashier."),
    // ── Attendance & payroll drivers ─────────────────────────────────────────
    preset!("lateness_by_employee", "Lateness", "People", "attendance", "attendance",
        dims: ["employee"], measures: ["late_count", "late_minutes"], filters: [],
        sort: Some(("late_minutes", Dir::Desc)), limit: 50, viz: Viz::Bar, period: PeriodPreset::LastMonth, share: false,
        "Minutes late and late arrivals per employee, after the grace window."),
    preset!("overtime_by_employee", "Overtime", "People", "attendance", "attendance",
        dims: ["employee"], measures: ["overtime_minutes", "worked_minutes"], filters: [],
        sort: Some(("overtime_minutes", Dir::Desc)), limit: 50, viz: Viz::Bar, period: PeriodPreset::LastMonth, share: false,
        "Overtime minutes per employee — the main variable cost in payroll."),
    preset!("attendance_by_day", "Attendance trend", "People", "attendance", "attendance",
        dims: ["day"], measures: ["present_count", "absent_count", "late_count"], filters: [],
        sort: None, limit: 120, viz: Viz::Line, period: PeriodPreset::Last30Days, share: false,
        "Daily present, absent and late counts across the workforce."),
    // ── Inventory ────────────────────────────────────────────────────────────
    preset!("waste_by_ingredient", "Waste by ingredient", "Inventory", "inventory_waste", "inventory",
        dims: ["ingredient"], measures: ["movement_cost", "qty"],
        filters: [("movement_type", "waste")], sort: Some(("movement_cost", Dir::Desc)), limit: 25,
        viz: Viz::Bar, period: PeriodPreset::Last30Days, share: false,
        "Value and quantity of stock wasted, by ingredient. Quantities are in each ingredient's own unit."),
    preset!("waste_trend", "Waste over time", "Inventory", "inventory_waste", "inventory",
        dims: ["day"], measures: ["movement_cost"], filters: [("movement_type", "waste")],
        sort: None, limit: 120, viz: Viz::Line, period: PeriodPreset::Last30Days, share: false,
        "The daily cost of wasted stock."),
    preset!("consumption_by_ingredient", "Consumption", "Inventory", "inventory", "inventory",
        dims: ["ingredient"], measures: ["qty", "movement_cost"], filters: [("movement_type", "sale")],
        sort: Some(("movement_cost", Dir::Desc)), limit: 30, viz: Viz::Bar, period: PeriodPreset::Last30Days, share: false,
        "Stock consumed by sales, by ingredient — what the kitchen actually goes through."),
    preset!("shrinkage_by_ingredient", "Shrinkage", "Inventory", "stocktakes", "stocktakes",
        dims: ["ingredient"], measures: ["shrink_cost", "variance_lines"], filters: [],
        sort: Some(("shrink_cost", Dir::Desc)), limit: 25, viz: Viz::Bar, period: PeriodPreset::Last90Days, share: false,
        "Value of stock found missing at finalized counts, by ingredient."),
    preset!("shrinkage_by_reason", "Shrinkage reasons", "Inventory", "stocktakes", "stocktakes",
        dims: ["variance_reason"], measures: ["shrink_cost", "variance_lines"], filters: [],
        sort: Some(("shrink_cost", Dir::Desc)), limit: 12, viz: Viz::Bar, period: PeriodPreset::Last90Days, share: false,
        "Why stock goes missing: theft, spoilage, breakage, miscounts and the rest."),
    // ── Purchasing ───────────────────────────────────────────────────────────
    preset!("spend_by_supplier", "Spend by supplier", "Purchasing", "purchase_orders", "purchasing",
        dims: ["supplier"], measures: ["purchase_cost", "po_count", "fill_rate"],
        filters: [("po_status", "received")], sort: Some(("purchase_cost", Dir::Desc)), limit: 25,
        viz: Viz::Bar, period: PeriodPreset::Last90Days, share: false,
        "Money spent per supplier on goods actually received, with their fill rate."),
    preset!("spend_by_ingredient", "Spend by ingredient", "Purchasing", "purchase_orders", "purchasing",
        dims: ["ingredient"], measures: ["purchase_cost", "qty_received", "avg_unit_cost"],
        filters: [("po_status", "received")], sort: Some(("purchase_cost", Dir::Desc)), limit: 30,
        viz: Viz::Bar, period: PeriodPreset::Last90Days, share: false,
        "Purchase spend by ingredient, with quantity received and average unit cost."),
    // ── Reservations ─────────────────────────────────────────────────────────
];

/// A default dashboard: a title and the preset ids it lays out, in order.
///
/// These are compiled in rather than seeded as rows so they stay upgradable —
/// improving a default board improves it for every merchant who has not forked
/// it, which seeded rows could never do.
pub struct BoardTemplate {
    pub key: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub widgets: &'static [&'static str],
}

pub const DEFAULT_BOARDS: &[BoardTemplate] = &[
    BoardTemplate {
        key: "today",
        title: "Today",
        description: "What is happening right now: takings, orders, ticket size, payment mix and the day's shape.",
        widgets: &[
            "revenue_total",
            "order_count_total",
            "avg_ticket",
            "sales_by_hour",
            "payment_mix",
            "top_products",
        ],
    },
    BoardTemplate {
        key: "sales",
        title: "Sales",
        description: "The trading picture over the last month: trend, branches, categories and when the rushes fall.",
        widgets: &[
            "sales_by_day",
            "sales_by_branch",
            "tips_total",
            "tips_by_day",
            "top_categories",
            "peak_hours_heatmap",
            "sales_by_order_type",
            "top_products",
        ],
    },
    BoardTemplate {
        key: "profit",
        title: "Profitability",
        description: "Where the margin is and where it leaks: product profit, thin margins, discounts and waste.",
        widgets: &[
            "product_profit",
            "thin_margin_products",
            "discount_usage",
            "waste_by_ingredient",
            "waste_trend",
            "shrinkage_by_reason",
        ],
    },
    BoardTemplate {
        key: "control",
        title: "Cash & control",
        description: "Loss prevention: drawer variance, voids by reason and by cashier, and the cash share of takings.",
        widgets: &[
            "drawer_variance",
            "shift_cash_summary",
            "voids_by_reason",
            "voids_by_cashier",
            "refunds_by_day",
            "cash_vs_card",
        ],
    },
    BoardTemplate {
        key: "people",
        title: "People",
        description: "Workforce: lateness, overtime, attendance trend and who is selling.",
        widgets: &[
            "attendance_by_day",
            "lateness_by_employee",
            "overtime_by_employee",
            "waiter_performance",
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::compile::{CompileCtx, compile};
    use crate::analytics::spec::parse_flexible_date;
    use std::collections::HashSet;

    fn ctx() -> CompileCtx {
        CompileCtx {
            tz: chrono_tz::Africa::Cairo,
            now: parse_flexible_date("2026-09-02T10:00:00Z").unwrap(),
        }
    }

    #[test]
    fn preset_ids_are_unique() {
        let mut seen = HashSet::new();
        for p in PRESETS {
            assert!(seen.insert(p.id), "duplicate preset id {}", p.id);
        }
    }

    #[test]
    fn every_preset_compiles() {
        // The strongest guarantee this file can offer: a preset that names a
        // dimension, measure or filter that does not exist cannot reach a build.
        for p in PRESETS {
            compile(&p.to_spec(None), &ctx())
                .unwrap_or_else(|e| panic!("preset '{}' does not compile: {e}", p.id));
        }
    }

    #[test]
    fn every_preset_is_documented_and_permissioned() {
        for p in PRESETS {
            assert!(p.description.len() > 30, "{}: description too thin", p.id);
            assert!(!p.category.is_empty(), "{}: no category", p.id);
            assert!(!p.permission.is_empty(), "{}: no permission", p.id);
        }
    }

    #[test]
    fn kpi_presets_really_are_scalars() {
        // A Kpi widget that returns 40 rows renders as a wrong single number.
        for p in PRESETS.iter().filter(|p| p.viz == Viz::Kpi) {
            assert!(
                p.dimensions.is_empty(),
                "{} is a KPI but groups by {:?}",
                p.id,
                p.dimensions
            );
        }
    }

    #[test]
    fn default_boards_only_reference_real_presets() {
        for b in DEFAULT_BOARDS {
            assert!(!b.widgets.is_empty(), "board {} is empty", b.key);
            for w in b.widgets {
                assert!(
                    preset(w).is_some(),
                    "board {} references unknown preset {w}",
                    b.key
                );
            }
        }
    }

    #[test]
    fn board_keys_are_unique() {
        let mut seen = HashSet::new();
        for b in DEFAULT_BOARDS {
            assert!(seen.insert(b.key), "duplicate board key {}", b.key);
        }
    }
}
