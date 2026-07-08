# AI Analytics — Expansion Roadmap

How to take the `analytics_query` builder (`src/ai/`) from a strong self-serve
analytics layer to a full analytics platform — **much** further — without ever
loosening the safety model.

---

## Where we are today

The AI chat picks one tool and fills typed params; the backend assembles SQL from
a **whitelisted semantic layer** (`src/ai/semantic.rs`) and runs it read-only,
timed, row-capped, RLS-scoped. The model never writes SQL.

Current surface of the flexible `analytics_query` tool:

- **4 datasets** — `orders`, `order_items`, `payments`, `waste` (each fixes a
  grain + its own `branch_col`/`time_col`/`orders_based`/`base_pred`).
- **17 dimensions** — time (day/week/month/hour/weekday), branch, waiter,
  cashier, order_type, delivery_channel, status, void_reason, product, category,
  size, payment_method, ingredient.
- **19 measures** — counts, revenue, discounts, tax, tips, delivery fees, AOV,
  voids, units, line items, item revenue/cost/profit/margin %, payments, waste.
- **Filters** — status, order_type, branch, date range.
- **Shaping** — `sort` (any measure, asc/desc), `having_min` threshold,
  top‑N‑`per`‑facet (one table per group), `compare` (previous_period /
  previous_year → Previous + Change % columns), `share` (% of total),
  `cumulative` (running total), `output` (auto/table/bar/line/pie).

Verified by `cargo test --lib 'ai::'` (41 tests): the exhaustive
dataset×dimension×measure compile-against-live-schema test, every transform, and
the full security battery (injection / RLS / read-only / timeout). The dashboard
(`ai-result-view.tsx`) renders faceted, multi-measure, and %/money/date columns
with no per-feature code.

---

## Invariants (never regress these)

1. **The model supplies ids + typed values, never SQL.** Every SQL string is an
   author-written `&'static` fragment; args only *select* fragments.
2. **Every query is read-only, `statement_timeout`-bounded, `MAX_ROWS`-capped,
   RLS-scoped, and `:branch_ids`-fenced.** System params (`:branch_ids`,
   `:locale`, `:tz`) are backend-injected and non-overridable.
3. **Every new fragment is proven against the live schema** by
   `builder_composes_valid_sql_for_every_dataset_dim_measure` (or a sibling), so
   a stale column can never ship.
4. **New response shapes stay backward-compatible** (additive fields, `#[serde(
   skip_serializing_if)]`), and the dashboard degrades gracefully.

---

## The extension recipe (why this is cheap now)

- **Add a measure/dimension** → one `Meas`/`Dim` row in `semantic.rs` (+ its id in
  the whitelist + description). The exhaustive test auto-covers it.
- **Add a dataset** → a `Dataset` row (base FROM + `branch_col`/`time_col`/
  `orders_based`/`base_pred`) + its dims/measures. Non-order grains already work
  (see `waste`).
- **Add a filter** → a whitelisted `(value → predicate)` match arm + an `Enum`
  param.
- **Add a transform** → a branch in `build()` that wraps/reshapes the aggregation
  (see `compare`/`share`/`cumulative`) + fixed-key output columns.
- **Add a param kind** → extend `ParamKind` + `prompt::report_parameters_schema`
  + `executor::resolve_model_args` (both providers pick it up for free).

Copy SQL fragments from the matching curated report in `catalog.rs` — they are
already schema-correct.

---

## Roadmap

### A. New datasets
- **`shifts`** — cash variance (over/short), shift duration, sales & orders per
  shift, per teller/till. Grain: `shifts`. *High value, low risk.*
- **`refunds`** — refund reasons, refunded value, refund rate. Grain: refunded
  orders / a refunds table.
- **`inventory_movements` (general)** — generalize `waste` into all movement
  types (purchase / consumption / waste / transfer / adjustment) with a
  `movement_type` dimension + filter; unlocks consumption & purchase-cost.
- **`stock_levels`** (snapshot, `branch_inventory`) — on-hand value, days-of-cover,
  reorder needs. No time axis → a snapshot dataset variant.
- **`open_tickets` / dine-in service** — table turnover, dwell time, covers, tips
  per table/section.
- **`customers`** (delivery) — new vs returning, repeat rate, AOV per customer,
  top customers, simple LTV. *Needs a customer identity key; check PII rules.*
- **`menu_engineering`** — the popularity × margin matrix (star / plowhorse /
  puzzle / dog) as a first-class report.

### B. New dimensions
- `day_part` (breakfast/lunch/dinner/late — derived from hour), `is_weekend`,
  `hour_of_week`.
- `table` / `zone` (dine-in), `customer` (delivery), `supplier`, `movement_type`.
- `bundle` (bundle vs à la carte), `addon` (via `order_item_addons`),
  `discount_name` / promo.
- `price_band` / order-value band (bucketed), `staff_role`.

### C. New & derived measures
- Ratios: `discount_rate`, `void_rate`, `refund_rate`, `attach_rate`
  (addons/order), `tips_pct`, `margin_pct` at order level.
- Basket: `avg_items_per_order`, `avg_basket_value`, `covers`.
- Customers: `unique_customers`, `new_customers`, `repeat_rate`.
- Net: `net_sales` (gross − refunds − discounts), `net_profit`.
- Distribution: `p50/p90 order value` (percentile via `percentile_cont`).
- **Derived-measure engine** — let a measure be composed from other whitelisted
  measures (a small dependency graph), so ratios don't each need bespoke SQL.

### D. Query transforms & analytics
- **Moving average / rolling window** (7-day MA, rolling baseline) via framed
  window functions.
- **% vs a rolling baseline** (lightweight anomaly flagging).
- **Bucketing / binning** — order-value bands, basket-size distribution
  (`width_bucket`).
- **Cross-tab / pivot** — two dimensions as rows × columns (a `pivot_on` param).
- **Multi-level faceting** — `per` a list (e.g. per branch **and** category).
- **Contribution-to-change** — which entity drove a period delta (compare +
  ranked Δ).
- **Cohort / retention** — first-order cohort × subsequent activity (needs
  customer identity).
- **Market-basket / associations** — items frequently bought together
  (self-join on order, support/lift; guard cost).
- **Relative filters** — "top 20% of products by revenue" (percentile HAVING),
  HAVING max/range, multiple conditions.
- **Forecast (naive)** — seasonal-naive / linear-trend projection for the next
  period; clearly labeled as an estimate.

### E. Presentation & UX (dashboard)
- **Sparklines / mini-trends** per row (a compact per-entity time series).
- **Heatmap** output (hour × weekday) as a new `ChartHint`.
- **Pivot table** rendering for cross-tabs.
- **Drill-down** — click a row → an auto-composed follow-up query.
- **Saved queries** — pin an `analytics_query` spec to a dashboard tile.
- **Scheduled digests** — a daily/weekly spec run + emailed (reuses the routine
  infra).
- **CSV / Excel export** of any result.
- **Auto coverage notes** — surface caveats automatically (e.g. waiter
  attribution only covers dine-in tickets).

### F. Platform & architecture
- **Declarative semantic model** — move datasets/dims/measures to a data-driven
  registry (or a tiny config), so non-Rust contributors can extend it and the
  schema is self-documenting.
- **Localized labels** — resolve column labels via i18n (`:locale`) instead of
  the current English literals.
- **Materialized daily rollups** — pre-aggregate for large orgs; the builder
  targets a rollup table when the grain allows (huge latency win at scale).
- **Query-cost guard** — estimate complexity (joins × grain × range) and refuse
  or downsample beyond a budget.
- **Multi-step orchestration** — let the model chain queries (find the top
  branch → break *it* down) behind a controlled tool loop.
- **Compare two entities** — `compare_dimensions` (branch A vs branch B side by
  side), not just periods.

### G. Governance & safety
- **Per-measure permissions** — gate cost/profit/labor to manager roles; the
  whitelist filters by the caller's permissions before the model sees it.
- **PII handling** — customer datasets behind explicit scopes + minimization.
- **Audit log** — persist (user, question, chosen tool + params, row count) for
  every AI query.
- **Rate / budget limits** per user/org.

### H. Evaluation & quality
- **Eval harness** — a fixture of NL questions (EN + AR/dialect) → expected tool
  + params; run in CI to measure routing accuracy and catch regressions as the
  catalog grows.
- **Fallback-rate metric** — log when the model can't compose a fitting query;
  drive catalog/prompt improvements from it.
- **Property/fuzz tests** — random valid specs must always assemble to runnable
  SQL; random invalid ids must always be rejected (never SQL).
- **Golden SQL snapshots** — pin the assembled SQL for representative specs.

---

## Suggested phasing

- **Phase 5 — deepen the core (near-term).** Derived-measure engine (C) + the
  ratio/basket measures; `refunds` + generalized `inventory_movements` datasets
  (A); `day_part`/`bundle`/`addon` dimensions (B); moving-average + bucketing
  transforms (D). All are pure `semantic.rs` additions covered by the exhaustive
  test. Add the eval harness (H).
- **Phase 6 — new surfaces (mid-term).** `shifts`, `customers` (with PID/PII
  guards), `menu_engineering` (A); cross-tab/pivot + multi-level faceting (D);
  pivot/heatmap/sparkline rendering + drill-down (E); per-measure permissions
  (G).
- **Phase 7 — platform (long-term).** Declarative semantic registry + localized
  labels + materialized rollups + query-cost guard + multi-step orchestration
  (F); saved queries + scheduled digests + export (E); audit log (G).

Each item is a bounded, independently shippable change on the same tested
foundation — extend the whitelist, prove it against the live schema, keep the
invariants.
