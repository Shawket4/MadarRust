# Madar Dashboard — Agent Implementation Prompt

**Working directory:** `/Users/shawket/Desktop/MadarDashboard`
**Backend:** Already complete at `/Users/shawket/Desktop/MadarRust` — zero backend changes required or allowed.
**Scope:** Onboarding wizard (new), cost-engine integration, piastres/form audits, UX streamlining.

---

## Stack

- React 19, TypeScript, Vite
- React Router v7 (`src/app/router/index.tsx`)
- TanStack Query v5 + Orval-generated API client (`src/shared/api/generated/`)
- Zustand v5 for auth (`src/shared/auth/store.ts`) and app state (`src/shared/auth/app-store.ts`)
- Radix UI primitives + Tailwind CSS (shadcn-style component kit in `src/shared/ui/`)
- react-i18next — bilingual EN/AR, RTL is a first-class requirement
- Recharts for all charts

## Project layout (actual conventions — follow these exactly)

```
src/
  app/         → router, providers
  pages/       → one folder per page/feature (e.g. src/pages/branches/)
  widgets/     → layout, header, sidebar, command-palette
  features/    → reusable feature segments (e.g. features/orders-export)
  entities/    → query wrappers, schemas, models (e.g. entities/branch/)
  shared/      → ui/, api/, hooks/, lib/, config/, i18n/, auth/
```

Key hooks already in place:
- `useCurrentContext()` (`src/shared/hooks/use-current-context.ts`) → `{ user, role, orgId, branchId, isSuperAdmin, isOrgAdmin, canManageOrg, isReady }`
- `usePermissions().can(resource, action)` (`src/shared/hooks/use-permissions.ts`)

---

## Step 0 — Regenerate the API client (do this first, blocks everything)

A fresh `openapi.json` is already exported at `/Users/shawket/Desktop/MadarRust/openapi.json`. Copy it into this project (wherever the orval config points) and run:

```bash
npm run generate:api
```

New operations that will appear: `get_onboarding`, `complete_onboarding` (orgs tag), `resolve_branch` (auth tag), `costing` operations, `branch_menu_engineering` (reports tag). Branch schemas gain `latitude`, `longitude`, `geo_radius_meters`.

---

## CRITICAL AUDIT 1 — Piastres ↔ EGP correctness

**Complete this before writing new features.**

The backend stores and transmits ALL monetary values as integer piastres (minor units). Frontend must always display EGP (÷100) and send piastres (×100, truncated). `cost: null` = unknown, NOT free — never render as "0.00 EGP"; always render an em-dash or "cost missing" badge.

Audit tasks:
1. Run `grep -rn "price\|cost\|sales\|profit\|margin\|/ 100\|* 100\|fmtMoney" src/` and classify every result as correct, missing conversion, or double-conversion.
2. Every display site (StatCard, DataTable columns, chart axes, tooltips, Badge) — confirm divide by 100.
3. Every form/input site — confirm multiply by 100 before the API call, display current value ÷100.
4. Menu Engineering report (`GET /reports/branches/{id}/menu-engineering`): `total_sales`, `total_cost`, `total_profit`, `item_profit`, `sales` are all piastres — ÷100 for display.
5. Costing endpoints (`GET /costing/menu-items`, `/costing/addon-items`): `price` and `cost` are piastres — ÷100.
6. Order line DTOs: `line_cost`, `unit_cost` are piastres — ÷100 in the order detail drawer and exports.
7. Fix all mismatches. Comment each fix: `// FIX: was displaying raw piastres, now divides by 100`
8. Unit-test fmtMoney: `fmtMoney(100) === "1.00 EGP"`, `fmtMoney(0) === "0.00 EGP"`, `fmtMoney(null) === "—"` (never "0.00 EGP").

---

## CRITICAL AUDIT 2 — Form reliability

**Complete alongside the piastres audit.**

Every form that creates or edits data must satisfy all of these. Audit: menu, recipes, categories, bundles, inventory, and all shared dialogs.

1. **Forms must actually fire the request.** Audit for: `onSubmit` not calling the mutation; `handleSubmit` not wired to `<form onSubmit>`; submit buttons with `type="button"` instead of `type="submit"`; early returns that silently discard the submission.
2. **Optimistic updates must roll back on error.** Every `useMutation` with `onMutate` must have a matching `onError` rollback.
3. **After a successful mutation, invalidate the relevant queries.** Check every `onSuccess` for `queryClient.invalidateQueries` on affected keys:
   - Menu item CRUD → `['menu-items', orgId]`
   - Recipe save → `['recipes', ...]` and `['costing', 'menu-items', orgId]`
   - Category CRUD → `['categories', orgId]`
   - Ingredient create/edit → `['catalog', orgId]` and `['costing', ...]`
   - Stock edit → `['stock', branchId]`
   - Bundle CRUD → `['bundles', orgId]`
4. **Dialogs must close after successful save** (open state set to false in `onSuccess`).
5. **Errors must surface** — toast or inline message, never silently swallowed.
6. **Loading states** — submit buttons disabled + spinner while `isPending`.
7. **react-hook-form wiring:** `<form onSubmit={handleSubmit(onSubmit)}>`, register/Controller on every field, `formState.errors` displayed, `reset()` after success where the form should clear.

---

## GROUND RULES (apply to every change)

1. **Never remove a feature.** Make the better way the default; keep the old way as a visible option.
2. **Zero backend modifications.** No new endpoints, no changed shapes. Bulk = client-side fan-out over existing single-record endpoints.
3. Use the shadcn-style kit in `src/shared/ui/` — PageShell, StatCard, Card, Badge, Button, EmptyState, Skeleton, DataTable, DateRangePicker, Tabs, SearchableSelect, Tooltip. Do not introduce a second component vocabulary.
4. Styling: Tailwind + project tokens. Semantic classes only (`text-muted-foreground`, `bg-muted`, `text-warning`, `text-primary`) — never raw hex.
5. i18n: every user-facing string through `t(...)` with keys in `src/shared/i18n`. Bilingual EN/AR + RTL is non-negotiable.
6. RBAC: gate with `usePermissions().can(resource, action)` and `useCurrentContext().isSuperAdmin`, exactly as existing pages do.
7. No new heavy dependencies. Use what's installed.
8. Definition of done per phase: `tsc --noEmit` adds no new errors; `vitest run` green for touched areas; `vite build` succeeds; flows verified in EN (LTR) and AR (RTL).

---

## PART 1 — Onboarding wizard (new feature, highest priority)

### Backend contract (already live)

**`GET /orgs/{id}/onboarding` → `OnboardingStatus`**

```ts
{
  org_id: string,
  completed: boolean,        // THE routing flag — persisted, never regresses
  completed_at: string | null,
  can_complete: boolean,     // all required steps done → enables Finish button
  recipe_coverage: number,   // 0..1 ratio of active items with ≥1 recipe row
  steps: Array<{
    key: "branch" | "payment_methods" | "categories" | "menu_items"
       | "ingredients" | "recipes" | "addons" | "team" | "first_order",
    done: boolean,
    count: number,
    required: boolean,  // branch / payment_methods / categories / menu_items
  }>
}
```

**`POST /orgs/{id}/onboarding/complete` → `OnboardingStatus`**
Idempotent. Sets `completed = true` permanently. Call ONLY from Finish, or from Skip when `can_complete` is true. Orgs with existing orders were backfilled to `completed = true` — they must never see the wizard.

Permissions: GET needs `orgs:read`, POST needs `orgs:update` (org admins). Don't render onboarding UI for non-admin roles.

**CRITICAL design fact:** step progress is derived server-side from data presence on EVERY read. There is no "mark step done" client call and there must never be one — creating a branch IS completing the branch step. Never store step state client-side.

### Routing & gating

1. After auth resolves, fetch onboarding status once: `staleTime: 30_000`.
2. If `completed === false` AND the user can `("orgs","update")` → redirect to `/onboarding`. Full-screen route OUTSIDE the normal app shell — no sidebar, no header scope switcher; the wizard is the shell.
3. A quiet "Skip setup, take me to the dashboard →" link lives in the wizard footer at all times.
4. If `completed === true`, `/onboarding` redirects to the overview. The wizard is unreachable after completion — re-runs are not a feature.
5. Invalidate `['onboarding', orgId]` after every create mutation fired inside the wizard so the stepper reflects reality immediately.
6. Session-scoped "skipped" flag (sessionStorage): after Skip, don't bounce the user back to the wizard for the rest of the session — only on next sign-in.
7. Skip semantics: `POST .../complete` ONLY if `can_complete` is true; otherwise route to the overview WITHOUT completing. NEVER mark an org complete when it has no branch — a "completed" org that can't take an order is a lie the rest of the app trips over.
8. `orgId` source: use `orgId` from `useCurrentContext()` (it merges org-admin's own org and super-admin's selected org).

### Wizard structure

Single route `/onboarding` with `?step=` URL sync (refresh/back must work).

| # | Step key(s)             | Title (EN)                       | Required |
|---|-------------------------|----------------------------------|----------|
| 0 | —                       | Welcome                          | —        |
| 1 | `branch`                | Create your first branch         | ✅       |
| 2 | `payment_methods`       | How do customers pay?            | ✅       |
| 3 | `categories`+`menu_items` | Build your menu                | ✅       |
| 4 | `ingredients`+`recipes` | Ingredient costs (recommended)   | ⬜       |
| 5 | `addons`                | Addons & extras                  | ⬜       |
| 6 | `team`                  | Invite your team                 | ⬜       |
| 7 | —                       | Review & finish                  | —        |

**REUSE, DON'T REBUILD.** Every step embeds the existing feature's form/list components in a narrowed layout. If a form is page-coupled, extract the form into its feature folder so both the page and the wizard import it — do not fork markup. The wizard contributes only: framing copy, the stepper, progress logic, empty-state illustrations.

**Step behaviors:**
- **Welcome (0):** org name + logo (reuse org settings form fields). If the org already has partial data (admin left mid-way), say "Picking up where you left off" and jump to the first incomplete required step.
- **Branch (1):** single branch form. After create, success card + "Add another / Continue". The branch form now also has optional `latitude`, `longitude`, `geo_radius_meters` fields (see Part 2.6) — keep them in an "advanced" collapsible here.
- **Payments (2):** the backend PRE-SEEDS 6 payment methods (cash, card, digital_wallet, mixed, talabat_online, talabat_cash) on org creation, so `payment_methods.done` is almost always ALREADY TRUE. Frame as "These are on — confirm and customize", not "add your first method". Note: the step counts ACTIVE methods — if the user deactivates all of them the step regresses; surface that inline.
- **Menu (3):** two-pane — categories left, items right (reuse existing components). Gate "Continue" on ≥1 category AND ≥1 active item; the authoritative check is the refetched `steps`.
- **Costs (4):** sell the cost engine — "Add ingredient costs and Madar computes profit per item, classifies your menu, and suggests prices." Show `recipe_coverage` as a live progress ring. Fully skippable, never guilt-block.
- **Addons (5) / Team (6):** thin wrappers over existing managers, both skippable.
- **Review (7):** render `steps` as a checklist (done = green check, optional-undone = gray "later"), the coverage ring, and the **Finish** button bound to `can_complete`. If false, disable Finish with "Finish requires: <missing required steps>" — each missing item deep-links to its step. On Finish → `POST .../complete` → light success state → route to overview.

### Files to create

```
src/pages/onboarding/
  index.tsx                  → page entry, completed-flag guard
  wizard-shell.tsx           → full-screen frame, stepper, footer (Skip link)
  stepper.tsx                → step indicator (direction-aware for RTL)
  step-frame.tsx             → title/description wrapper per step
  step-welcome.tsx
  step-branch.tsx            → imports existing branch form
  step-payments.tsx          → imports existing payment-methods manager
  step-menu.tsx              → imports existing category + item forms
  step-costs.tsx             → imports existing ingredient/recipe forms
  step-addons.tsx            → imports existing addon manager
  step-team.tsx              → imports existing user form
  step-review.tsx            → checklist + CoverageRing + Finish
  coverage-ring.tsx          → circular progress ring for recipe_coverage
  use-onboarding.ts          → TanStack Query hooks (status + complete)

src/widgets/setup-checklist/
  setup-checklist.tsx        → dismissible overview card (post-wizard)
```

### Files to edit

- `src/app/router/index.tsx` — add `/onboarding` route (outside shell) + redirect guard for org admins with `completed === false`
- `src/pages/dashboard/dashboard.tsx` — mount SetupChecklist widget

### Post-wizard: setup checklist widget

On the overview, render a dismissible "Setup checklist" card while any optional step has `done === false` OR `recipe_coverage < 0.8`:
- Optional steps only, each row deep-links to its normal screen.
- `first_order` row bridges to the POS: "Open a shift on the iPad and ring up your first order."
- Dismiss state in localStorage (existing persistence util). Card disappears on its own when everything is done.
- The card must tolerate regression — `done` can flip back to false (admin deletes their only branch later).

### i18n

Full Egyptian-Arabic coverage. Suggested AR step titles: الفرع، طرق الدفع، المنيو، تكلفة المكونات، الإضافات، الفريق، المراجعة والإنهاء. Stepper connector lines must mirror correctly in RTL.

### Pitfalls

- Do NOT store step completion client-side. Only client state: current step index + session "skipped" flag.
- Do NOT gate on `steps` array order — gate on `required`/`done`.
- Do NOT call complete when `can_complete` is false.
- Test the backfill path: an org with historical orders lands on the overview, never the wizard.

---

## PART 2 — Backend integration (cost engine + new endpoints)

### 2.1 Menu Engineering screen (new)

Route: `/menu/engineering` (or a tab under the merged `/menu` section — see Part 4.1). Permission: `can("orders", "read")`.

**Endpoint:** `GET /reports/branches/{branch_id}/menu-engineering?from=&to=&limit=` — default picker to last 30 days.

```ts
MenuEngineeringReport {
  branch_id, from, to,
  rows: MenuEngineeringRow[],
  total_sales, total_cost, total_profit,  // piastres → ÷100
  rows_cost_missing: number,
}
MenuEngineeringRow {
  menu_item_id, size_label, item_name, category_id, category_name,
  quantity_sold,
  sales,              // piastres → ÷100
  total_cost,         // piastres | null (null ⟺ ≥1 line had unknown cost)
  item_profit,        // per-unit, piastres | null → ÷100
  total_profit,       // piastres | null → ÷100
  popularity_pct,     // 0..1 ratio — no conversion
  cost_missing_lines: number,
  profit_category: "high" | "low" | null,
  popularity_category: "high" | "low",
  class: "star" | "workhorse" | "challenge" | "dog" | null,
}
```

**Two views, one toggle:**

*Scatter/quadrant (default):* Recharts ScatterChart. X = `popularity_pct`, Y = `item_profit ÷ 100`. ReferenceLines at popularity threshold (`0.70 / rows.length`) and weighted-average per-unit profit (`Σ total_profit / Σ quantity_sold` over cost-tracked rows). Color by class: star=green, workhorse=blue, challenge=amber, dog=red; `class: null` = hollow gray dots pinned to X axis, tooltip "Add ingredient costs to classify". Quadrant labels: Stars / Workhorses / Challenges / Dogs. Tooltip: name + size, sales EGP, qty, profit EGP, popularity %.

*Table:* TanStack Table, columns in Foodics order: Item, Size, Sales, Quantity, Total Cost, Item Profit, Total Profit, Popularity %, Profit Category, Popularity Category, Class. Category filter (client-side), name search, Excel export (existing util, EGP-converted). `class` as colored Badge. `total_cost: null` rows: em-dash cells + "N lines missing cost" hint linking to the recipe editor.

**Header strip:** 4 StatCards — Total Sales, Total COGS, Gross Profit (all EGP), "Items missing costs: N". Clicking the last one filters the table to those rows.

**AR class names:** نجم / حصان عمل / لغز / عبء.

**Note:** this report is the simple Foodics-equivalent single-window view. The Menu Advisor stays the statistically serious layer — don't merge them; link from quadrant/row to the advisor's suggestion for that SKU.

FSD: `src/features/menu-engineering/` for the screen, `src/entities/costing/queries.ts` for shared SkuCost/AddonCost hooks.

### 2.2 Cost columns on menu and addon screens

`GET /costing/menu-items?org_id={org_id}` → `SkuCost[]`: `price`/`cost` piastres → ÷100; `margin_pct`/`food_cost_pct` are ratios; `cost_missing: boolean`. `GET /costing/addon-items` same minus size/category, plus `addon_type`.

- Menu items screen: "Cost / Margin" column pair. Query key `['costing','menu-items',orgId]`, `staleTime: 60_000`. **Join on the pair `(menu_item_id, size_label)` — never item id alone.** `food_cost_pct` chip: green <30%, amber 30–40%, red >40%. `cost_missing` rows get a warning icon linking to the recipe editor.
- Addon items screen: same with `/costing/addon-items`.
- After recipe save: invalidate `['costing','menu-items',orgId]` and `['costing','addon-items',orgId]`.

### 2.3 Order detail cost rows

`OrderItem` gained `line_cost` (piastres|null), `unit_cost` (piastres|null), `cost_missing`. `OrderItemAddon` gained `line_cost`. `OrderItemOptional` gained `cost`.

Order detail drawer: muted "Cost" column + footer row `COGS: X EGP · Gross profit: Y EGP (Z%)`. If any line has `cost_missing`: show profit as `≥` lower-bound with tooltip, or omit — NEVER silently treat missing as zero.

Orders export: use `line_cost` for the COGS column (true sale-time cost, ÷100).

### 2.4 Menu Advisor behavioral changes

1. `POST .../runs` now returns **409** (was 400) when a run is in progress → treat as "run in flight — start polling", not an error toast.
2. Stale `in_progress` runs >15 min are auto-failed server-side → remove any "stuck run" escape-hatch UI.
3. `GET .../runs/latest?any_status=true` returns the latest run regardless of status. Advisor empty state becomes a three-way switch: `completed` → report; `in_progress` → progress + poll; `failed` → error card with `error_message` (prefixed with failing stage) + "Run again" button.
4. `price_changed_in_window` flag → badge as "recently repriced" (these return `action: "hold"`).
5. Do NOT chart old runs against new runs — pre-rebuild runs have costs ~100× too small. Gate run-over-run comparisons to runs created after the deploy date.

### 2.5 Fixed report endpoints

`GET /reports/branches/{id}/addons` and `/items-combined` were broken (missing translation columns) and now work — `addon_name_translations` / `item_name_translations` are real objects. Re-enable any disabled widgets or workarounds.

### 2.6 Branch geofencing fields (new)

`POST /branches` and `PATCH /branches/{id}` now accept:
- `latitude` (number, nullable — null clears it on PATCH)
- `longitude` (number, nullable — null clears it on PATCH)
- `geo_radius_meters` (integer, backend defaults to 200)

Add these to the branch create/edit form, ideally as a "Geofencing" section with helper text: "Set coordinates to let POS devices auto-detect this branch by GPS." Optional nicety: a "use my current location" button via the browser geolocation API.

### 2.7 Permission resources renamed

- Discounts now use the `"discounts"` resource (was checked against `"menu_items"`).
- Payment methods now use `"payment_methods"` (was `"orgs"`).
Any permission matrix UI that lists resources must include both (already seeded backend-side). Update `usePermissions` role defaults if they enumerate resources.

---

## PART 3 — Dashboard UX streamlining

### 3.1 Global scope context (the keystone — backbone for later phases)

New Zustand store `src/shared/scope/scope-store.ts` (same pattern as `src/shared/auth/app-store.ts`):

```ts
interface ScopeState {
  branchId: string | null;     // null = "all branches" where supported
  from: string | null;         // ISO (Cairo day-start)
  to: string | null;           // ISO (Cairo day-end)
  preset: "today" | "yesterday" | "7d" | "30d" | "mtd" | "custom";
  setBranch(id: string | null): void;
  setRange(from: string | null, to: string | null, preset: ScopeState["preset"]): void;
}
```

- Persist `branchId` + `preset` to localStorage (not from/to — recompute presets on load).
- `src/shared/scope/use-scope-url-sync.ts`: mounted once in Layout; mirrors store to `?branch=&from=&to=&preset=`, hydrates from URL on first load (URL wins).
- Default branchId: exactly one accessible branch → select it; else null. Default preset: `today`.
- Scope bar in the header (`src/widgets/header/header.tsx` → `src/widgets/scope-bar/scope-bar.tsx`): branch SearchableSelect (gated by `can("branches","read")`, hidden if only one branch — show static text) + preset chips (Today / 7d / 30d / MTD / Custom) + DateRangePicker for custom. Collapse into a popover on `< lg`.
- `src/shared/scope/use-scoped-params.ts`: returns `{ branchId, from, to }` for query hooks.
- Refactor analytics, orders, shifts, inventory, recipes, bundles, users, permissions, and all other scoped pages to read `useScopedParams()`. Delete bespoke branch selects and per-page date pickers.
- Single-branch pages with `branchId === null` show an EmptyState prompting branch selection.

Acceptance: header scope updates every open page and the URL; reload/share restores scope; `grep -rn "DateRangePicker" src/pages` returns empty.

### 3.2 Overview redesign (`src/pages/dashboard/dashboard.tsx`)

Kill duplicate fetching: lift per-branch current-shift and stock queries to single `useQueries` calls in the page; pass results into BranchCard as props (delete BranchCard's own `useGetCurrentShift` and the raw `apiClient.get(["stock-low", id])`). One `refetchInterval: 60_000` on the lifted shift query only.

New layout (top → bottom):
1. **KPI row** — 4 StatCards with trend vs prior equal period: revenue (EGP), orders, AOV (EGP), active shifts. Add `delta`/`trend` prop to StatCard if absent; show `▲/▼ %`.
2. **Attention panel** — prioritized list: branches with no open shift during business hours, low-stock items, stuck/failed states. Empty → "all clear" EmptyState. Replaces the separate low-stock card.
3. **Branch status grid** — prop-fed BranchCard. Click sets `scope.branchId` then navigates to `/orders`. Shows today's sales (EGP), open-shift teller, running duration, low-stock badge.
4. **Recent activity** — RecentOrdersPanel driven by `scope.branchId`.
5. Remove the quick-actions card (duplicates sidebar); replace with contextual actions.
6. Mount the SetupChecklist widget (Part 1).

Loading: replace `?? "—"` with Skeletons.

### 3.3 Analytics streamlining

Adopt global scope; delete local branch select + DateRangePicker. Keep granularity control and all 6 tabs. Confirm query-key sharing across tabs dedupes through the TanStack cache. Consistent skeletons per tab (ChartCard takes a `loading` prop). Branches tab stays org-scoped — label "All branches comparison". RTL: mirror chart axes/legends via `i18n.dir()`.

### 3.4 Cross-cutting polish

- Deep-linking with scope: every "All →" / row click carries current scope.
- Replace ad-hoc `text-[10px]`/`text-[11px]` with `text-xs`/`text-sm` + tabular class — `grep -rn "text-\[1[01]px\]" src` must return empty.
- Every page renders through PageShell with title/description/action.
- `src/shared/ui/async-boundary.tsx`: shared loading/error/empty wrapper for all list/table pages.

---

## PART 4 — Menu, Recipes, Inventory streamlining

### 4.1 Merge Menu + Recipes + Bundles navigation

- Routing: `/menu` parent with tabbed children `/menu/items`, `/menu/recipes`, `/menu/bundles`, `/menu/add-ons` (+ `/menu/engineering`, `/menu/advisor`). Redirects: `/recipes → /menu/recipes`, `/bundles → /menu/bundles`, `/menu → /menu/items`.
- Sidebar: collapse menu/recipes/bundles/menuAdvisor into one expandable **Menu** entry. Preserve all RBAC.
- `src/pages/menu/menu-layout.tsx`: Tabs strip + Outlet inside one PageShell; strip each tab page's own PageShell to avoid double padding.

### 4.2 Reusable editable grid + bulk runner

`src/shared/ui/editable-grid.tsx` (wraps DataTable): inline cell edit committing on blur/Enter via `onCommitRow(row, patch)`; add-row affordance; bulk-select toolbar; paste-to-create (TSV/CSV → column mapping → preview with per-row validation → "Create N" fan-out).

`src/shared/lib/bulk-runner.ts`:

```ts
async function runBulk<T>(rows: T[], op: (row: T) => Promise<unknown>, {
  concurrency = 4,
  onProgress,  // (done, total, lastError?) => void
}): Promise<{ ok: T[]; failed: { row: T; error: unknown }[] }>
```

Bounded concurrency 4; never aborts the batch on one failure; returns summary → toast "Created 57, 3 failed" with retry-failed action; invalidates query keys once at the end.

### 4.3 Menu items streamlining

- Inline grid as default: edit name, `base_price` (÷100 display, ×100 send), category, `is_active` via `PATCH /menu-items/{id}`. Full dialog stays for sizes/translations/advanced.
- Bulk add via paste/CSV → fan-out `POST /menu-items`.
- Duplicate item: GET → POST (name + " (copy)") → replay sizes → replay recipe lines per size.
- Size templates: named size sets in localStorage; applying → `POST /menu-items/{id}/sizes` calls.
- Bulk price ops: select rows → "+N EGP" / "×N%" → fan-out PATCH (converted to piastres).
- Image flow (chained, not single-call): (1) `POST /menu-items`, (2) `POST /uploads/menu-items/{returnedId}` with the queued file. Item appears instantly; image fills in when the upload resolves.

### 4.4 Recipe single-screen builder

- Single-screen builder as default: `useFieldArray` across all sizes, commit all lines in one `POST /recipes/drinks/{id}` per size.
- Base-size scaling (opt-in toggle); off = manual per-size authoring unchanged.
- Live cost & margin preview while editing (browser-computed: `cost_per_unit ÷ 100 × quantity_used`), labeled "current-cost estimate".
- Copy recipe from another item (GET source → POST lines onto target).
- Inline "create ingredient" in the picker (`POST /inventory/orgs/{org_id}/catalog` → select result) — never leave the screen.
- Add-ons recipes get the same builder.

### 4.5 Cost & margin helpers

`src/entities/menu/cost.ts` — pure, unit-tested:

```ts
// All inputs in piastres; output in piastres; null when any ingredient lacks cost_per_unit
function recipeCost(recipe: DrinkRecipe, catalog: OrgIngredient[]): number | null
function itemMargin(item: MenuItem, recipe: DrinkRecipe, catalog: OrgIngredient[]): number | null
```

Surface: items grid (cost ÷100 + margin% per base size, warning chip on low/negative), recipe builder (live running cost), bundles (per-component `item_cost`/`item_price` — both piastres — and bundle-level margin).

Tests minimum: `recipeCost` with multi-size + missing-cost ingredients; `itemMargin` with zero-cost and null-cost cases.

### 4.6 Inventory streamlining

- "Receive delivery" batch: editable grid → fan-out `PATCH .../stock/{id}` and/or `POST .../adjustments`; progress + partial-failure summary.
- Low-stock → quick restock: multi-select → received qty → fan-out commit.
- Inline stock + reorder-threshold editing; optimistic + undo.
- One-step ingredient onboarding: `POST .../catalog` then `POST .../stock` chained in one gesture.
- Depletion forecast: `min(current_stock / quantity_used)` over recipe lines — pure computation, info panel.
- Transfer-from-alert: one-click "transfer from <branch with surplus>" prefilling the transfer form.
- Stock-take CSV import/export (export client-side; import → fan-out adjustments).
- **Transfer deletion now returns 409** when it would send destination stock negative — show a clear message, not a generic error.

### 4.7 Cross-cutting

- Inline "create new X" in every picker (ingredient in recipes, category in items).
- Command-palette actions: "Add menu item", "New ingredient", "Receive delivery", "Duplicate item".
- Optimistic mutations + undo across create/edit/delete.

---

## SEQUENCING

1. **Step 0** — regenerate API client (blocks everything)
2. **Audits** — piastres + form reliability (before any new feature code)
3. **Part 1** — onboarding wizard (highest visible priority)
4. **2.4** — Menu Advisor 409 fix + failed-run visibility (small, unblocks ops)
5. **3.1** — global scope store + URL sync (backbone)
6. **2.3 + 2.2** — order-detail cost rows, costing columns
7. **2.1 + 3.2** — Menu Engineering screen + overview redesign (parallel)
8. **3.3** — analytics streamlining
9. **4.1 + 4.2** — menu IA merge + editable grid/bulk runner
10. **4.3 + 4.4 + 4.5** — menu items, recipe builder, cost helpers
11. **4.6 + 3.4 + 4.7** — inventory + polish

---

## ABSOLUTE PITFALLS — never do these

- `cost ?? 0` anywhere. `null` is "unknown", not free.
- Render null cost as "0.00 EGP" — use em-dash or "cost missing" badge.
- Join costing data on `menu_item_id` alone — always `(menu_item_id, size_label)`.
- Store onboarding step completion client-side.
- Call `POST .../onboarding/complete` when `can_complete` is false.
- Chart pre-rebuild advisor runs against new ones (old costs ~100× too small).
- `quantity_sold`/`sales` in menu-engineering exclude bundle lines by design — use `/reports/branches/{id}/items-combined` when bundle attribution matters.
- `popularity_pct` sums to 1 across the report's rows, not the whole menu.
- Remove an existing capability, add tsc errors, break AR/RTL, duplicate an existing query key, or invent a request/response shape not in the generated client.

---

## VERIFICATION

```bash
npm install
npx tsc --noEmit                            # no new errors vs baseline
npm run test                                # touched suites green (vitest)
npm run build                               # production build succeeds
grep -rn "cost ?? 0" src                    # must be empty
grep -rn "null.*0\.00" src                  # must be empty
grep -rn "text-\[1[01]px\]" src             # must be empty (after 3.4)
grep -rn "DateRangePicker" src/pages        # must be empty (after 3.1)
# Manual: toggle to AR — verify RTL on overview, analytics, menu engineering, wizard stepper
# Manual: fresh org → wizard appears; org with orders → overview, never the wizard
```
