# Madar Backend — Cost Engine & Menu Advisor Rebuild

**Audience:** the agent working on the Madar React dashboard (`React 19, TypeScript, FSD, TanStack Query/Table, shadcn/ui, OpenAPI-generated client`).
**Backend version:** cost-engine rebuild, June 2026. Regenerate the OpenAPI client from the new `openapi.json` before touching any code — every change below is reflected in the spec.

---

## 0. TL;DR

The backend now computes ingredient-based COGS **at order creation** and snapshots it onto every order line, immutably. The Menu Advisor was rebuilt on top of these snapshots (its old cost math was off by ~100× due to an EGP↔piastre unit bug and is not comparable to new output). Three new read surfaces exist for the dashboard:

1. `GET /reports/branches/{branch_id}/menu-engineering` — a Foodics-vocabulary menu engineering table. **This is the centerpiece screen.**
2. `GET /costing/menu-items?org_id=` and `GET /costing/addon-items?org_id=` — current recipe-cost rollups per SKU / addon.
3. Cost fields on every order line DTO, and advisor run failure visibility via `?any_status=true`.

**Universal money/cost semantics (memorize):**
- All money is integer **piastres** (minor units). Divide by 100 for EGP display, as everywhere else in the dashboard.
- `cost: null` means **unknown**, never free. `cost: 0` means genuinely free. Never render `null` as `0.00 EGP` — render a "cost missing" badge/em-dash.
- `cost_missing: true` on a line/SKU means at least one component couldn't be resolved (no recipe, unlinked ingredient, or ingredient without a cost).

---

## 1. New endpoint: Menu Engineering report

`GET /reports/branches/{branch_id}/menu-engineering?from=&to=&limit=`
Permission: same as other reports (`orders:read`). Dates are optional ISO timestamps; default = all time, so the dashboard should default the picker to **last 30 days** (Foodics defaults to last month — keep that familiarity).

Response (`MenuEngineeringReport`):

```ts
{
  branch_id: string,
  from: string | null,
  to: string | null,
  rows: MenuEngineeringRow[],
  total_sales: number,        // piastres, cost-tracked rows only
  total_cost: number,         // piastres
  total_profit: number,       // piastres
  rows_cost_missing: number,  // rows excluded from profit math
}
```

`MenuEngineeringRow` (one row per **SKU** = menu_item × size; `size_label: "one_size"` when sizeless):

```ts
{
  menu_item_id: string,
  size_label: string,
  item_name: string,
  category_id: string | null,
  category_name: string | null,
  quantity_sold: number,            // standalone lines only; bundles excluded
  sales: number,                    // piastres
  total_cost: number | null,        // null ⟺ ≥1 line in window had unknown cost
  item_profit: number | null,       // per-unit profit, piastres
  total_profit: number | null,
  popularity_pct: number,           // 0..1 share of units in this report
  cost_missing_lines: number,
  profit_category: "high" | "low" | null,
  popularity_category: "high" | "low",
  class: "star" | "workhorse" | "challenge" | "dog" | null,
}
```

### How to build the screen (Foodics-familiar, our math)

This is the screen MENA owners migrating from Foodics must recognize instantly. Two views, one toggle:

**A. Scatter/quadrant view (default):**
- Recharts `ScatterChart`. X = `popularity_pct`, Y = `item_profit` (per-unit profit).
- `ReferenceLine` at the popularity threshold (`0.70 / rows.length`) and at the weighted-average per-unit profit (derive: `Σ total_profit / Σ quantity_sold` over cost-tracked rows). These are the lines the backend used for `class`, so dots and quadrant labels will agree.
- Color by `class`: star = green, workhorse = blue, challenge = amber, dog = red. Rows with `class: null` (cost missing) render as hollow/gray dots pinned to the X axis with a tooltip "Add ingredient costs to classify".
- Tooltip: name + size, sales, qty, profit, popularity %.
- Quadrant corner labels: **Stars / Workhorses / Challenges / Dogs** — Foodics console vocabulary, intentionally (not "Plowhorse").

**B. Table view:**
- TanStack Table with the exact Foodics column order: Item, Size, Sales, Quantity, Total Cost, Item Profit, Total Profit, Popularity %, Profit Category, Popularity Category, Class.
- Category filter (client-side from `category_id`), search by name, Excel export (the dashboard's existing export util; include EGP-converted columns).
- `class` as a colored shadcn `Badge`. `total_cost: null` rows: em-dash in cost/profit cells + a subtle "N lines missing cost" hint from `cost_missing_lines`, linking to the recipes screen for that item.

**Header strip:** four stat cards — Total Sales, Total COGS, Gross Profit, and "Items missing costs: N" (from `rows_cost_missing`; clicking filters the table to those rows). The missing-costs card is the call-to-action that drives recipe completion.

Note: this report is the *simple single-window* view (Foodics-equivalent). The **Menu Advisor** screen remains the statistically serious layer (recency weighting, confidence, suggestions). Don't merge them; link from a quadrant/row to the advisor's suggestion for that SKU.

---

## 2. New endpoints: current cost rollups

`GET /costing/menu-items?org_id={org_id}` → `SkuCost[]`:

```ts
{
  menu_item_id: string,
  size_label: string,          // "one_size" when sizeless
  item_name: string,
  category_id: string | null,
  price: number,               // current price, piastres
  cost: number | null,         // current recipe rollup, piastres
  cost_missing: boolean,
  margin_pct: number | null,   // (price-cost)/price
  food_cost_pct: number | null,
}
```

`GET /costing/addon-items?org_id={org_id}` → `AddonCost[]` (same shape minus size/category, plus `addon_type`).

**Dashboard application:**
- **Menu items screen:** add a "Cost / Margin" column pair to the items table. Fetch `/costing/menu-items` once per org scope (TanStack Query, key `['costing','menu-items',orgId]`, staleTime ~60s) and join client-side on `(menu_item_id, size_label)`. Show `food_cost_pct` as a small colored chip (green <30%, amber 30–40%, red >40%). `cost_missing` rows get a warning icon linking to the recipe editor.
- **Addon items screen:** same treatment with `/costing/addon-items`.
- **Recipe editor:** after a recipe save, invalidate the costing queries so the margin chips refresh.

These reflect *current* ingredient prices. Historical truth lives on order lines (next section) — don't mix the two in one widget without labeling.

---

## 3. Cost fields on order DTOs (additive, non-breaking)

`OrderItem` gained:
- `line_cost: number | null` — full line COGS in piastres (recipe + addons + optionals + bundle components, × quantity), snapshotted at sale time.
- `unit_cost: number | null` — recipe-only cost per unit (includes milk/coffee swaps; excludes additive addons/optionals). `null` for bundle lines.
- `cost_missing: boolean`.

`OrderItemAddon` gained `line_cost: number | null` (null also for swap-type addons — their cost is folded into the item's recipe cost, by design).
`OrderItemOptional` gained `cost: number | null` — per **parent-item unit**, matching its `quantity_deducted` semantics.
`deductions_snapshot` entries now carry `cost_per_unit` (EGP float, audit) and `line_cost` (piastres), plus attribution ids (`addon_item_id` / `optional_field_id` / `component_item_id`).

**Dashboard application:**
- **Order detail drawer:** under the existing line breakdown, show a muted "Cost" column and a footer row `COGS: X EGP · Gross profit: Y EGP (Z%)` computed from `line_cost` vs `line_total`. If any line has `cost_missing`, show the profit as `≥` lower-bound with a tooltip, or omit — never silently treat missing as zero.
- **Orders export:** the existing export already ships `ingredient_costs`; prefer the new per-line `line_cost` for the COGS column going forward (true sale-time cost, not current cost).
- Historical orders were **backfilled** by migration using point-in-time cost history (approximate for pre-history data — current costs were used as the baseline). Treat backfilled and new lines identically.

---

## 4. Menu Advisor changes

The advisor's data layer was rebuilt; persisted run/suggestion schemas are unchanged, so existing advisor UI types still deserialize. Behavioral changes the dashboard must handle:

1. **`POST .../runs` now returns `409 Conflict`** (was `400`) when a run is already in progress. Update the mutation's error handling to treat 409 as "run in flight — start polling" rather than a generic error toast.
2. **Stale-run takeover:** an `in_progress` run older than 15 minutes is automatically failed and superseded when a new run is requested. The UI no longer needs a "stuck run" escape hatch.
3. **Failed-run visibility:** `GET .../runs/latest?any_status=true` returns the most recent run regardless of status, including `error_message`. The advisor screen's empty state must become a three-way switch:
   - latest run `completed` → render report (as today)
   - latest run `in_progress` → progress state + poll
   - latest run `failed` → error card showing `error_message` (it's prefixed with the failing stage, e.g. `[adapter] …`) with a "Run again" button.
   This replaces the old behavior where `/latest` and `/active` both returned null after a failure and the screen looked inexplicably empty.
4. **Suggestion content changes** (no shape change): items repriced inside the analysis window now come back as `action: "hold"` with an explanation noting suppression; `price_changed_in_window` is the flag to badge ("recently repriced"). `cost_missing` on a suggestion still drives the revenue-only badge.
5. **Numbers will move.** All CM/margin/food-cost figures from runs created before this rebuild were computed with costs ~100× too small. Do not chart old runs against new runs; if the screen has run-over-run comparisons, gate them to runs created after the deploy date.

---

## 5. Fixed report endpoints (previously 500-ing)

`GET /reports/branches/{id}/addons` and `GET /reports/branches/{id}/items-combined` were broken in production (SQL missing the translations columns their row types declare). They now return what their TS types always claimed:
- `AddonSalesRow.addon_name_translations: object`
- `CombinedItemSalesRow.item_name_translations: object`

If the dashboard had workarounds/disabled widgets for these, re-enable them.

---

## 6. Client regeneration & sequencing

1. Pull the new `openapi.json` from the backend repo (`cargo run --bin export-openapi`) and regenerate the TS client.
2. New tags/operations to expect: `costing` (2 ops), `reports` (+`branch_menu_engineering`).
3. Apply in this order: regenerate client → 409/`any_status` advisor handling (small, unblocks ops) → order-detail cost rows → costing columns on menu/addon screens → the Menu Engineering screen (largest piece).
4. FSD placement suggestion: `features/menu-engineering/` for the new screen (api hook + quadrant chart + table as segments), `entities/costing/` for the SkuCost/AddonCost query hooks shared by menu screens.

## 7. Pitfalls

- Never coalesce `cost ?? 0`. Anywhere. `null` is a first-class "unknown" state with its own UI.
- Join costing data on the **pair** `(menu_item_id, size_label)` — item id alone collides across sizes.
- `quantity_sold`/`sales` in menu-engineering exclude bundle lines by design; don't reconcile them against the combined-items report and file a bug — use `/reports/branches/{id}/items-combined` when bundle attribution matters.
- `popularity_pct` sums to 1 across the report's rows, not across the whole menu (unsold SKUs aren't rows).
- RTL: the new screen must respect the existing Arabic i18n setup; class names (Star/Workhorse/Challenge/Dog) need Arabic strings — suggest نجم / حصان عمل / لغز / عبء for star/workhorse/challenge/dog, but defer to the existing glossary if one exists.
