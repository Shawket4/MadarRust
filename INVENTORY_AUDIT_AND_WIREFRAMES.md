# Inventory — Audit + Workflow Wireframes

> Deliverable 1 of the Foodics-grade inventory effort (see `INVENTORY_FOODICS_BRIEF.md`).
> Lead with **how a cafe manager uses this**, then map each journey to the backend.
> The phased build plan is a separate, approval-gated deliverable (comes next).

---

## Part A — Audit (what's actually true today)

### A.1 The big surprise: the dashboard inventory UI was never built

The brief assumed the dashboard UI was "broken" because it lagged the backend overhaul. The
reality is cleaner and better:

- **`/inventory` is a `coming soon` placeholder.** `src/routes/_app/inventory.tsx` renders
  `makeComingSoon(...)`. There is **no `src/features/inventory/` directory** and **zero inventory
  feature components**. This is greenfield, not repair.
- **The Orval client is already regenerated** against the new backend spec
  (`orval.config.ts` reads `../MadarRust/openapi.json` directly). Every new hook already exists:
  stocktakes, waste, movements, purchasing, inventory reports. No regen needed to start.
- **The breaking changes are already absorbed.** Close-shift no longer sends `inventory_counts`
  (`src/features/shifts/close-shift-dialog.tsx`); the void dialog already sends `reason` + `note`
  with note-required-for-`other` (`src/features/orders/void-order-dialog.tsx`); `display_order` is
  gone from all components.
- **All en/ar strings for inventory already exist** in `src/i18n/locales/{en,ar}.json` under
  `inventory.*` (catalog, stock, movements, waste, stocktakes, purchasing, reports, etc.). RTL is
  wired (`applyHtmlDir`, logical Tailwind props).

**What this means:** we are not fixing a broken screen — we are building the inventory product on
top of a backend that's ~90% there and a frontend whose plumbing (API client, i18n, design system,
branch/org scope stores) is ready. The wireframes below are the spec for that build.

Two minor cleanups (non-blocking): delete 3 orphan generated model files
(`inventoryCountInput.ts`, `inventoryCountRow.ts`, `inventoryDiscrepancy.ts` — on disk, not
re-exported), and the dashboard's stale root `openapi.json` copy (Orval doesn't read it).

### A.2 Backend: solid foundation, with a short list of real gaps

The ledger, stocktakes, waste, transfers, valuation, consumption, waste-report, COGS/margin
(menu-engineering), and weighted-average-cost on receive all exist and are internally consistent.
NULL-cost (= unknown, never 0) handling is genuinely well done and propagates through every valued
report. The gaps below are what stop a journey from being served *cleanly*:

| # | Gap | Where | Impact on a journey | Severity |
|---|-----|-------|---------------------|----------|
| G1 | **Partial multi-shipment receive is impossible.** `receive_order` always sets PO `status='received'`, and a second receive is blocked by the status guard. No `partially_received` status in the enum. | `src/purchasing/handlers.rs:477,557-561`; `migrations/20260613004000_purchasing.sql:20` | Receive a delivery (#3) — works for one-shot full/partial, breaks for "rest arrives tomorrow." | **High** |
| G2 | **No ingredient → supplier link.** Suppliers attach to POs, not to ingredients. No `org_ingredients.supplier_id` / preferred supplier. | data model | Reorder view (#7) can't pre-fill a supplier; one-click "create PO" can't group by supplier. | **High** |
| G3 | **Low-stock noise from default-0 thresholds.** Low-stock uses `current_stock <= reorder_threshold` with both defaulting to 0, so every zero/zero item flags as low. No `reorder_threshold > 0` guard. | `src/reports/handlers.rs:952`; `src/inventory/handlers.rs:497` | Morning dashboard (#1) + reorder (#7) flood with false alerts on real data. | **High** |
| G4 | **No "items needing a count" signal.** No last-counted-at per item, no stale-count endpoint. | data model | Morning dashboard (#1) "items needing a count" tile can't be built. | **Medium** |
| G5 | **No org-wide PO list / no status filter / no "pending deliveries."** POs are per-branch list only. | `src/purchasing/routes.rs:15` | Morning dashboard (#1) "pending deliveries"; multi-branch purchasing overview. | **Medium** |
| G6 | **Reorder level is per-branch only** (`branch_inventory.reorder_threshold`); no org default. | data model | Setup (#2): manager must set par per branch; fine, but no "set once" default. | **Low** |
| G7 | **Consumption & waste reports are per-branch only** (no org rollup). | `src/reports/routes.rs` | Reports (#8) multi-branch view needs client-side fan-out or a new org endpoint. | **Low** |
| G8 | **Stocktake finalize is an absolute set** — overwrites any sales between count and finalize; "one open per branch" guard is non-transactional (no DB unique constraint). | `src/stocktakes/handlers.rs:137-148,345-353` | Count stock (#4): long counts on a busy branch can swallow in-flight sales. Acceptable Foodics behavior, but flag it in UI. | **Low** |
| G9 | **Permission inconsistency:** the 4 new inventory reports check `reports/read`, while `/reports/.../stock` and `menu-engineering` check `inventory`/`orders` read. | `src/reports/handlers.rs:861,900,940,980,1025` vs `465,1421` | A manager with inventory-read but not reports-read sees a partial reports tab. | **Low** |

Full endpoint inventory and data-model detail are in the backend audit (captured in the plan
deliverable). The journey→backend map (Part C) marks every gap inline.

---

## Part B — Information Architecture (manager language)

`Inventory` becomes a top-level nav item (under the existing **Operations** group, next to Orders /
Shifts / Analytics). Plain words, never engineering terms. Internal term → manager term:

- movement ledger → **Stock history**
- weighted average cost → just **Cost** (one number)
- variance → **Difference**
- reorder threshold / par → **Low-stock level**
- stocktake → **Stock count**
- below_zero / oversold → **Sold past zero** (a warning badge)

```
Operations ▸ Inventory
│
├─ Today            ← the morning home: alerts, deliveries, counts due, stock value
├─ Items            ← every ingredient: cost, stock per branch, low-stock level, supplier, "used in"
│     └─ (item)     ← detail drawer: Stock history, recipes it's used in, adjust/edit
├─ Purchasing
│     ├─ Orders     ← purchase orders: draft → ordered → receiving (full/partial) → done
│     └─ Suppliers  ← supplier list + contacts
├─ Stock counts     ← start a count → enter quantities → see differences → finalize
├─ Waste            ← log spoilage/loss with a reason; today's & period waste
├─ Transfers        ← move stock between branches
└─ Reports          ← stock value, COGS & margins, usage over time, waste by reason
```

**Connections to the rest of the app**
- **Menu ▸ Recipes** stays where it is. An ingredient's detail drawer shows "Used in N recipes"
  and deep-links into the recipe builder. Setting up "ingredient + its recipe" (journey #2) spans
  Inventory (create the item) → Menu/Recipes (link it to a menu item/size/addon).
- **Reports ▸ Menu engineering** (COGS & margin per item) is reachable both from Inventory ▸
  Reports and from the Menu area — same backend endpoint.
- **Branch/org scope:** the existing scope-bar picks org + branch. Catalog/Items and Suppliers are
  **org-scoped**; Stock, Movements, Waste, Counts, Transfers, POs are **branch-scoped** and show a
  "pick a branch" gate when none is selected (`inventory.pickBranch` already in i18n).

---

## Part C — Workflow wireframes (the 8 journeys)

Low-fidelity on purpose. Each: layout, data shown, primary action, empty/edge states, click-path,
backend mapping. `✅` = endpoint exists. `⚠️ GAP` = backend work needed (ref Part A).

---

### 1) "What do I do this morning?" — Inventory ▸ Today (home)

```
┌─ Inventory · Today ───────────────────────  [Branch: Maadi ▾] [⟳] ─┐
│                                                                     │
│  ┌── Stock value ──┐ ┌── Low stock ──┐ ┌─ Deliveries ─┐ ┌─ Counts ┐│
│  │  EGP 48,250     │ │   7 items     │ │  2 arriving  │ │ 3 due   ││
│  │  ▸ 5 unknown    │ │  ▸ 2 critical │ │   today      │ │ >14 days││
│  │    cost         │ │               │ │              │ │         ││
│  └─────────────────┘ └──────────────┘ └──────────────┘ └─────────┘│
│                                                                     │
│  ⚠ Low stock — reorder soon                          [View all →] │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │ Item         On hand   Low-stock level   Status   ▸ Reorder  │ │
│  │ Milk          2.0 L          10 L         ●critical [+ PO]    │ │
│  │ Espresso      0.8 kg          3 kg        ●critical [+ PO]    │ │
│  │ Cups (12oz)   140 pcs        200 pcs      ●low      [+ PO]    │ │
│  └─────────────────────────────────────────────────────────────┘ │
│                                                                     │
│  📦 Arriving today              🗑 Today's waste     [Log waste +] │
│  ┌──────────────────────────┐  ┌────────────────────────────────┐│
│  │ PO-1043  Cairo Dairy      │  │ Croissants  6 pcs  overproduce ││
│  │   exp. today  [Receive →] │  │ Milk        1.2 L  spoiled      ││
│  │ PO-1044  Bakery Co        │  │ ─ EGP 240 today                ││
│  │   exp. today  [Receive →] │  │                                ││
│  └──────────────────────────┘  └────────────────────────────────┘│
└─────────────────────────────────────────────────────────────────┘
```

- **Data:** stock value (Σ stock×cost, piastres→EGP, with unknown-cost count), low-stock list,
  POs arriving today, today's waste, count-due count.
- **Primary action:** triage — click a low-stock row's **[+ PO]**, **[Receive →]** a delivery,
  **[Log waste +]**.
- **Empty states:** "All good — nothing below its low-stock level 🎉"; "No deliveries scheduled";
  "No waste logged today."
- **Click-path:** open Inventory → lands on Today → scan four tiles → act.
- **Backend:**
  - Stock value → `GET /reports/branches/{id}/inventory-valuation` ✅
  - Low stock → `GET /reports/orgs/{id}/low-stock` ✅ **but ⚠️ GAP G3** (filter `reorder_threshold>0`)
  - Arriving today → ⚠️ **GAP G5** (need PO list filtered by status+date; today only per-branch list, no status filter)
  - Counts due → ⚠️ **GAP G4** (no last-counted-at / stale-count signal)
  - Today's waste → `GET /inventory/branches/{id}/waste` ✅

---

### 2) Set up an ingredient + its recipe — Inventory ▸ Items → Menu ▸ Recipes

```
┌─ Add item ─────────────────────────────────────────┐
│  Name        [ Whole Milk ............ ]            │
│  Category    [ Dairy ▾ ]                            │
│  Unit        ( g  kg  ml  ●l  pcs )   ← base unit   │
│  Cost        [ 32.00 ] EGP per l   (blank = unknown)│
│  Supplier    [ Cairo Dairy ▾ ]          ⚠️ GAP G2   │
│  ─────────────────────────────────────────────────  │
│  ▸ Advanced                                         │
│     Description       [ ............... ]           │
│     Low-stock level   [ 10 ] l   (per branch below) │
│  ─────────────────────────────────────────────────  │
│  Per-branch starting stock & low-stock level         │
│   Maadi    on hand [ 24 ] l   low-stock [ 10 ] l    │
│   Zamalek  on hand [ 18 ] l   low-stock [  8 ] l    │
│                                  [ Cancel ] [ Save ] │
└─────────────────────────────────────────────────────┘

then, to make it deduct on sale:  Menu ▸ Recipes ▸ (pick item/size)
┌─ Recipe: Latte · Large ────────────────────────────┐
│  Whole Milk     [ 250 ] [ ml ▾ ]   ← saved as base  │
│  Espresso       [  18 ] [ g  ▾ ]      unit (l), auto │
│  + Add ingredient                      converted     │
└─────────────────────────────────────────────────────┘
```

- **Data:** ingredient fields (name, category, base unit, cost-or-unknown, supplier), then
  per-branch stock + low-stock level; recipe = ingredient + quantity + unit (any unit in the same
  family; backend normalizes to base).
- **Primary action:** Save item → optionally jump to recipe builder.
- **Progressive disclosure:** description + default low-stock under "Advanced"; recipe unit picker
  stays (manager thinks in ml; backend stores base l).
- **Edge:** cross-family unit (e.g. g for a liquid) → inline error "can't convert g to l."
  Cost blank → stored as unknown (never 0), shown as "—".
- **Click-path:** Items → **[+ Add item]** → fill → Save → "Add a recipe?" → Recipes builder.
- **Backend:**
  - Create item → `POST /inventory/orgs/{id}/catalog` ✅ (name, unit, category, cost)
  - Per-branch stock + low-stock → `POST /inventory/branches/{id}/stock` (`current_stock`, `reorder_threshold`) ✅
  - Supplier on item → ⚠️ **GAP G2** (no linkage column)
  - Recipe link → `POST /recipes/drinks/{id}` / `POST /recipes/addons/{id}` ✅ (normalizes units)

---

### 3) Receive a delivery — Inventory ▸ Purchasing ▸ Orders

```
Step A — create/pick a PO                  Step B — receive
┌─ New purchase order ──────────────┐      ┌─ Receive PO-1043 · Cairo Dairy ──────────┐
│ Supplier  [ Cairo Dairy ▾ ]       │      │ Item        Ordered   Already  Receiving │
│ Branch    [ Maadi ]               │      │ Whole Milk   10 ×1L     0       [ 10 ]    │
│ Expected  [ 2026-06-14 ]          │      │ Espresso     2 ×1kg     0       [  2 ]    │
│ Ref/Note  [ ............ ]        │      │ Sugar        1 ×25kg    0       [  1 ]    │
│ ───────── Lines ───────────────── │      │ ──────────────────────────────────────── │
│ Item        Pack      Qty   Cost  │      │ ◉ Receive full   ○ Partial (rest later)  │
│ Whole Milk  [1 L ▾]  [10]  [32.0] │      │            ⚠️ GAP G1: partial=2nd receive │
│ Espresso    [1 kg▾]  [ 2]  [900 ] │      │            blocked (PO closes on receive) │
│ Sugar       [25kg▾]  [ 1]  [600 ] │      │                       [ Cancel ] [Receive]│
│  + Add line        Total EGP 2,840│      └──────────────────────────────────────────┘
│              [ Save draft ][Order]│       → stock ↑, Cost recomputed (weighted avg),
└───────────────────────────────────┘         a "purchase" row added to Stock history
```

- **Data:** supplier, branch, expected date, lines (item, pack/purchase unit, qty, cost per pack).
  Receive screen shows ordered vs already-received vs receiving-now.
- **Primary action:** **[Receive]** → increments stock, posts `purchase_in`, recomputes cost
  (weighted moving average).
- **Progressive disclosure:** pack factor (`units_per_purchase_unit`) auto-derived when purchase
  unit is a known stock unit; only shown for custom packs (e.g. "24-pack") under Advanced.
- **Edge / GAP:** "Partial (rest later)" is the natural flow but **⚠️ GAP G1** — backend closes the
  PO on any receive. Until fixed, partial means the PO can't be received again. Plan must add a
  `partially_received` status + allow remaining receives.
- **Click-path:** Purchasing ▸ Orders → **[+ New PO]** → add lines → **[Order]** → later
  **[Receive →]** → confirm quantities → done.
- **Backend:**
  - Create → `POST /purchasing/branches/{id}/orders` ✅ · list → `GET .../orders` ✅ · get → `GET /purchasing/orders/{id}` ✅
  - Receive → `POST /purchasing/orders/{id}/receive` ✅ (WAC + `purchase_in`) — **⚠️ partial multi-receive GAP G1**
  - Suppliers → `POST/GET /purchasing/orgs/{id}/suppliers`, `PATCH/DELETE /purchasing/suppliers/{id}` ✅

---

### 4) Count stock (stocktake) — Inventory ▸ Stock counts

```
┌─ Stock count · Maadi · started 09:14 ──────────  [In progress] ─┐
│  Filter [ All categories ▾ ]   [ Show only counted ]  3/42 done │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ Item        Expected   Counted        Difference    Value  │  │
│  │ Whole Milk    24.0 l   [ 22.0 ] l       -2.0 l    -EGP 64  │  │
│  │ Espresso       3.2 kg  [  3.2 ] kg       0.0       —       │  │
│  │ Sugar         12.0 kg  [      ] kg     (not counted)       │  │
│  │ Cups 12oz    420 pcs   [ 400  ]        -20 pcs    -EGP 30  │  │
│  └──────────────────────────────────────────────────────────┘  │
│  Net difference so far:  -EGP 94 shrinkage                      │
│  ⚠ Sales keep deducting while you count (finalize sets stock    │
│    to your counts).                              ⓘ GAP G8       │
│                          [ Cancel count ]   [ Review & finalize ]│
└─────────────────────────────────────────────────────────────────┘
   finalize → stock = counted; "count" rows added to Stock history;
   → Variance report: shrinkage EGP 94 · overage EGP 0 · 1 unknown-cost item
```

- **Data:** expected (snapshot at start) vs counted vs difference vs valued difference; running
  net shrinkage/overage; unknown-cost count.
- **Primary action:** enter counts → **[Review & finalize]** (sets stock to counted, posts
  `stock_count` movements for non-zero differences).
- **Progressive disclosure:** partial count by category (filter); "show only counted."
- **Empty/edge:** "No open count — **[Start a count]**." 409 if one already open → "A count is
  already in progress." Finalize warning re: in-flight sales (**GAP G8**, absolute set — flag, not
  block). Cancel discards.
- **Click-path:** Stock counts → **[Start a count]** (optional note) → enter quantities (autosave
  via `PUT /items`) → **[Review & finalize]** → see Variance report.
- **Backend (all ✅):** start `POST /stocktakes/branches/{id}`, save `PUT /stocktakes/{id}/items`,
  finalize `POST /stocktakes/{id}/finalize`, cancel `POST /stocktakes/{id}/cancel`, variance
  `GET /stocktakes/{id}/variance-report`. Caveat G8 (absolute set + non-tx open-guard).

---

### 5) Log waste/spoilage — Inventory ▸ Waste (dialog reachable from Today & item detail)

```
┌─ Log waste ──────────────────────────┐   ┌─ Waste · this week ─────────────────┐
│  Item      [ Croissants ▾ ]          │   │ By reason            Qty      Value  │
│  Quantity  [ 6 ] pcs                 │   │ Overproduction      18 pcs   EGP 90  │
│  Reason    ( ●Expired  Spoiled       │   │ Spoiled            3.2 l    EGP 102  │
│             Damaged  Overproduction  │   │ Expired             6 pcs    EGP 72  │
│             Theft  Other )           │   │ ─────────────────────────────────── │
│  Note      [ left out overnight ]    │   │ Total this week            EGP 264   │
│              [ Cancel ] [ Log waste ] │   │           [ Day | Week | Month ]    │
└───────────────────────────────────────┘   └─────────────────────────────────────┘
```

- **Data:** item, quantity (in base unit), reason (enum), optional note; period waste grouped by
  reason + item, valued.
- **Primary action:** **[Log waste]** → deducts stock, posts `waste` movement.
- **Edge:** rejects waste > on-hand ("Only 4 pcs on hand"). Reason `other` → note recommended.
- **Click-path:** Waste → **[Log waste +]** → pick item, qty, reason → Log. Also from Today tile
  and item detail.
- **Backend (✅):** `POST /inventory/branches/{id}/waste`, list `GET .../waste`, period grouping
  `GET /reports/branches/{id}/waste-report?from&to` (per-branch only — **GAP G7** for org rollup).

---

### 6) Transfer stock between branches — Inventory ▸ Transfers

```
┌─ New transfer ───────────────────────────────────┐
│  From   [ Maadi ▾ ]      →     To  [ Zamalek ▾ ]  │
│  Item   [ Whole Milk ▾ ]   (Maadi on hand: 22 l)  │
│  Qty    [ 6 ] l                                   │
│  Note   [ cover weekend ]                         │
│                          [ Cancel ] [ Transfer ]  │
└───────────────────────────────────────────────────┘
   → Maadi −6 l, Zamalek +6 l; two Stock-history rows (out/in)
┌─ Recent transfers ────────────────────────────────┐
│ Today 10:12  Whole Milk  6 l  Maadi → Zamalek  ⋯  │
│ Yest. 16:40  Cups 12oz  100   Zamalek → Maadi  ⋯  │
└────────────────────────────────────────────────────┘
```

- **Data:** source/destination branch, item, quantity, note; recent transfers (direction filter).
- **Primary action:** **[Transfer]** (atomic; reversible via row menu → Delete).
- **Edge:** rejects qty > source on-hand; same source/dest disallowed.
- **Click-path:** Transfers → **[+ New transfer]** → pick from/to/item/qty → Transfer.
- **Backend (✅):** `POST /inventory/transfers`, list `GET /inventory/branches/{id}/transfers?direction=`,
  edit note `PATCH`, reverse `DELETE`.

---

### 7) Reorder view — Inventory ▸ Today "View all" / Items filtered "Low stock"

```
┌─ Reorder — below low-stock level (all branches) ──────────────────┐
│  [ Branch: All ▾ ]  [ Supplier: All ▾ ]      ☑ select to order    │
│  ┌──────────────────────────────────────────────────────────────┐│
│  │ ☑  Item        Branch    On hand   Level   Suggest  Supplier  ││
│  │ ☑  Milk        Maadi      2.0 l    10 l    +8 l    Cairo Dairy││
│  │ ☑  Espresso    Maadi      0.8 kg    3 kg   +2.2kg  Beans Co   ││
│  │ ☐  Cups 12oz   Zamalek   140 pcs  200 pcs +60 pcs  Pack Ltd   ││
│  └──────────────────────────────────────────────────────────────┘│
│  2 selected · groups into 1 PO (Cairo Dairy)   [ Create PO(s) → ] │
└────────────────────────────────────────────────────────────────────┘
```

- **Data:** every item below its low-stock level across branches, with deficit ("Suggest" = how
  much to bring back to level) and — ideally — its supplier.
- **Primary action:** select rows → **[Create PO(s)]** (grouped by supplier, pre-filled qty).
- **Edge / GAPs:** supplier column + supplier-grouping needs **GAP G2** (ingredient→supplier link);
  noise control needs **GAP G3** (`reorder_threshold>0`).
- **Click-path:** Today ▸ "View all" (or Items ▸ filter "Low stock") → select → Create PO → lands in
  the PO draft (journey #3).
- **Backend:** below-par list → `GET /reports/orgs/{id}/low-stock` ✅ (carries deficit); create PO →
  `POST /purchasing/branches/{id}/orders` ✅. **Glue is the gap:** no supplier on low-stock rows
  (G2), and a multi-branch select-to-PO needs client orchestration (one PO per branch+supplier).

---

### 8) Reports a manager cares about — Inventory ▸ Reports

```
┌─ Inventory reports ──────  [Scope: Org ▾ / Branch ▾]  [Date: This month ▾] ─┐
│  ( Stock value | What's selling vs costing | Usage | Waste )   ← tabs        │
│                                                                              │
│  Stock value                          What's selling vs costing (COGS)       │
│  ┌────────────────────────────┐       ┌──────────────────────────────────┐  │
│  │ Total  EGP 48,250          │       │ Item     Sold  Revenue  Cost  Margin│
│  │ 5 items unknown cost (—)   │       │ Latte    320  9,600   3,100   66% │  │
│  │ ┌ by category (bar) ─────┐ │       │ Cappucc. 210  6,300   2,050   67% │  │
│  │ │ Dairy ███████ 21k      │ │       │ Croissant 90  2,700   1,500   44% │  │
│  │ │ Coffee ████  12k       │ │       │  (cost basis: snapshot | current) │  │
│  │ └────────────────────────┘ │       └──────────────────────────────────┘  │
│  └────────────────────────────┘                                             │
│  Usage over time (line)               Waste by reason (donut + table)        │
│  └ consumed qty/value by item         └ reason → qty/value, period            │
└──────────────────────────────────────────────────────────────────────────────┘
```

- **Data:** (a) stock value total + unknown-cost count + by-category bar; (b) COGS & margin per
  SKU; (c) consumption/usage over a date range; (d) waste by reason.
- **Primary action:** read; change scope (org/branch) + date range; export.
- **Edge:** unknown-cost items excluded from totals and counted explicitly (never treated as 0).
- **Backend:**
  - Stock value → `GET /reports/{branch|org}/inventory-valuation` ✅
  - COGS & margins → `GET /reports/branches/{id}/menu-engineering?cost_basis=` ✅
  - Usage → `GET /reports/branches/{id}/consumption?from&to` ✅ (per-branch — **GAP G7** org rollup)
  - Waste → `GET /reports/branches/{id}/waste-report?from&to` ✅ (per-branch — **GAP G7**)
  - ⚠️ **GAP G9:** the 4 inventory reports check `reports/read` while stock/menu-engineering check
    `inventory`/`orders` read — reconcile so a manager doesn't see a half-populated tab.

---

## Part D — Wireframe → backend summary (gap rollup)

| Journey | Backend status | Gaps to close |
|---------|----------------|---------------|
| 1 Today (home) | Partial | G3 (low-stock noise), G4 (counts-due), G5 (pending deliveries) |
| 2 Setup item + recipe | Mostly ✅ | G2 (supplier on item), G6 (org-default level) |
| 3 Receive delivery | Partial | **G1 (partial multi-receive)** |
| 4 Stock count | ✅ | G8 (flag absolute-set; harden open-guard) |
| 5 Waste | ✅ | — (G7 for org rollup in reports) |
| 6 Transfers | ✅ | — |
| 7 Reorder view | Partial | **G2**, G3 (depends on both) |
| 8 Reports | ✅ | G7 (org rollup), G9 (permission consistency) |

**Backend work, prioritized for the build plan:** G1 (partial receive) and G2 (ingredient→supplier)
are the two that block a *clean* Foodics flow; G3 (low-stock guard) is a one-line fix with high
visible payoff; G4/G5 enable the morning dashboard; G6/G7/G8/G9 are polish.

---

## Next deliverable
A **prioritized, phased build plan** (backend gap-closing + dashboard screens, data-preserving),
presented in plan mode for approval before any code is written.
