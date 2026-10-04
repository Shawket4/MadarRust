# Warehouses: design

> Status: **draft for review** · 2026-10-04 · repos: MadarRust, madar-shared, MadarDashboard
> Builds on [INVENTORY_V2.md](INVENTORY_V2.md) (ledger-derived stock). Decisions below were
> taken with the owner. Open items are in §10.

## 1. What a warehouse is

A **warehouse** is a stock location that belongs to an org, like a branch, but it **never sells**.
It holds inventory, receives supplier deliveries, counts, logs waste, and sends stock to branches
(and takes stock back). It has no POS, menu, orders, tills, tables, devices, delivery, loyalty or
customers.

| Decision | Choice |
|---|---|
| Data model | A `branches` row with `kind = 'warehouse'`. Not a new table. |
| Transfer flow | **Draft → dispatched (in transit) → received**, with per-line received quantities. One receive closes a transfer. |
| Who starts it | **Either side.** A branch can *request* stock (a `requested` draft the source reviews), and any source can send without a request. |
| Transfer shape | **Multi-line**: one transfer document, N ingredients. |
| Directions | Any location to any location: W→B, B→W (returns), B→B (as today), W→W. |
| Supplier purchasing | Warehouses **and** branches can receive POs. |
| Costing | **At cost**: the source's weighted average cost, blended into the destination's WAC (today's rule). No markup. |
| People | Reuse role assignments. A person is assigned to a warehouse like a branch (branch_manager kind). |
| Warehouse features | Stock, ledger, stock counts, waste, par levels + reorder suggestions, POs. |
| Branch replenishment | Suggested W→B transfers from branch par levels. |
| Warehouse par | Set by hand (v1). |
| Kind change | **Anytime**, branch ↔ warehouse, with guards (§4.3). |
| Limits | **Separate warehouse limit** per org (`organizations.max_warehouses`, §3). |
| Over-receive | Allowed with a required note, flagged. |
| Quick transfer | One-step UI shortcut when the user can act at both ends. |
| Shared contract | New **`madar-inventory`** crate in madar-shared. |

## 2. Why a `branches` row

Every inventory table is keyed by `branch_id`: `branch_stock`, `inventory_movements` (and its
apply/guard triggers), `stocktakes`, `purchase_orders`, waste, `stock_transfers`. If a warehouse
is a branch row, **the whole inventory stack works at a warehouse with no change**, including the
ledger trigger, WAC, counts, POs, reorder suggestions, low-stock, valuation, and authz branch scope
(`src/authz/scope.rs`).

The cost is the other direction: about 257 queries `FROM/JOIN branches` across selling features
must not treat a warehouse as a shop. §4 handles this with **DB guards first, filters second**, so
a missed filter fails loudly instead of selling from a warehouse.

## 3. Schema

```sql
-- migrations/2026100xxxxxxx_warehouses.sql
CREATE TYPE branch_kind AS ENUM ('branch', 'warehouse');
ALTER TABLE branches ADD COLUMN kind branch_kind NOT NULL DEFAULT 'branch';
CREATE INDEX idx_branches_org_kind ON branches (org_id, kind) WHERE deleted_at IS NULL;

-- Warehouse limit. NULL = unlimited. Set by a super admin; no plan system exists yet.
ALTER TABLE organizations ADD COLUMN max_warehouses int CHECK (max_warehouses >= 0);
-- Enforced in create_branch / kind change: count live kind='warehouse' rows FOR UPDATE on the org row.

-- Selling tables reject a warehouse (see §4.1).
CREATE FUNCTION assert_selling_branch() RETURNS trigger ...
  -- RAISE 'warehouse_cannot_sell' when NEW.branch_id is a kind='warehouse' branch.

-- Transfers become documents with lines.
CREATE TYPE stock_transfer_status AS ENUM ('requested', 'draft', 'dispatched', 'received', 'cancelled');

ALTER TABLE stock_transfers
  ADD COLUMN status        stock_transfer_status NOT NULL DEFAULT 'received',
  ADD COLUMN reference     text,              -- human ref, e.g. TR-1043 (per-org sequence)
  ADD COLUMN requested_at  timestamptz, ADD COLUMN requested_by  uuid REFERENCES users(id),
  ADD COLUMN dispatched_at timestamptz, ADD COLUMN dispatched_by uuid REFERENCES users(id),
  ADD COLUMN received_at   timestamptz, ADD COLUMN received_by   uuid REFERENCES users(id),
  ADD COLUMN cancelled_at  timestamptz, ADD COLUMN cancelled_by  uuid REFERENCES users(id);

CREATE TABLE stock_transfer_lines (
  id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  transfer_id       uuid NOT NULL REFERENCES stock_transfers(id) ON DELETE CASCADE,
  org_ingredient_id uuid NOT NULL REFERENCES org_ingredients(id),
  qty_sent          numeric(12,3) NOT NULL CHECK (qty_sent > 0),
  qty_received      numeric(12,3) CHECK (qty_received >= 0),   -- NULL until received
  unit_cost         bigint,          -- piastres, frozen at dispatch; NULL = unknown (never 0)
  note              text,            -- e.g. "2 bottles broken"
  UNIQUE (transfer_id, org_ingredient_id)
);
```

**Backfill:** every existing `stock_transfers` row becomes a `received` header plus one line
(`qty_sent = qty_received = quantity`, `unit_cost` from its `transfer_out` movement). Then drop
`stock_transfers.org_ingredient_id` and `quantity`. Assert row counts and `SUM(quantity)` inside
the migration (same pattern as inventory v2).

`inventory_movement_type` needs no new values. `transfer_out` / `transfer_in` already exist;
`source_type = 'transfer'`, `source_id = transfer_id` as today.

## 4. Keeping warehouses out of selling

### 4.1 DB guards (the safety net)

A `BEFORE INSERT` trigger `assert_selling_branch()` on the tables that mean "this place sells":

- `orders`, till/shift tables, device pairing (`devices`), branch menu/size/addon/channel
  overrides, bookings/reservations, tables/floor, delivery integration links.

A missed app filter gives a clear `409 warehouse_cannot_sell` instead of a silent bad row.

### 4.2 App filters

| Surface | Change |
|---|---|
| `GET /branches` (`src/branches/handlers.rs:240`) | Return `kind` on `Branch`. Add optional `?kind=branch\|warehouse`; omitted returns both. |
| Dashboard scope bar | Group: **Branches** / **Warehouses**. Selling pages show only `kind=branch`. Inventory pages show both. |
| POS device pairing, `/sync/pull` feed | Exclude warehouses (pairing a till to a warehouse is refused by §4.1 too). |
| Sales reports, analytics, menu engineering, loyalty, public ordering/QR | Filter `kind = 'branch'`. |
| Inventory reports (valuation, waste, consumption, low-stock) | Include warehouses, labelled. |
| Dawam (attendance) | **Allowed** at a warehouse: warehouse staff clock in, and the geofence works the same. |
| Branch create/edit dialog | "Type: Branch / Warehouse" on create and edit (§4.3). |

### 4.3 Changing kind

Allowed anytime, in both directions (`PATCH /branches/{id}` with `kind`, `branches.edit`):

- **Branch → warehouse:** refused while the branch has an open till/shift, open orders or
  tickets, or paired active devices (`409` naming what to close first). The org's
  `max_warehouses` is checked. Menu overrides and other selling settings are **kept, not deleted**,
  so turning it back into a branch restores them. Past sales stay in reports under the location's
  name.
- **Warehouse → branch:** always allowed. Stock, ledger and transfers carry over unchanged. The
  branch then needs its selling setup (menu, tills, devices) like a new branch.
- (Audit trail of kind changes: there is no general audit log yet; `branches.updated_at` moves.)

## 5. Transfers

### 5.1 Lifecycle

```
requested ──accept/edit──▶ draft ──dispatch──▶ dispatched ──receive──▶ received
    │                        │                     │
    └──decline──▶ cancelled ◀┴──cancel─────────────┘ (dispatched cancel returns stock to source)
```

A transfer starts either as a **request** (created by the destination side) or directly as a
**draft** (created by the source side, no request needed).

| Step | Who (capability) | Ledger effect |
|---|---|---|
| **Request** (N lines, quantities wanted) | destination side · `inventory.transfers.create` | none |
| **Accept request** (edit qty/lines → draft) or **decline** (note required) | source side · `inventory.transfers.create` | none |
| **Withdraw request** | destination side · `inventory.transfers.create` | none |
| **Create draft** (any direction, N lines) | source side · `inventory.transfers.create` | none |
| **Edit draft** (lines, qty, note) | source side · `inventory.transfers.create` | none |
| **Dispatch** | source side · `inventory.transfers.create` | Lock each line's source balance (`lock_on_hand`). Reject if any line exceeds on-hand. Freeze `unit_cost` = source WAC (org default fallback, NULL if unknown). Post `transfer_out` −qty_sent per line at the source. |
| **Receive** (per-line `qty_received`, optional note) | destination side · `inventory.transfers.edit` | Blend into destination WAC at the frozen `unit_cost`, then post `transfer_in` +qty_received per line at the destination. Lines with 0 received post nothing. |
| **Cancel draft** | source · `inventory.transfers.create` | none |
| **Cancel dispatched** | source · `inventory.transfers.delete` | Post `transfer_in` +qty_sent back at the **source** (`note = 'Transfer cancelled'`). |
| Received transfers | n/a | **Final.** Mistakes are fixed with a reverse transfer, not by editing history. Replaces today's `DELETE /transfers/{id}` reversal. |

The table is `madar_inventory::transfer::step` (one copy, vector-pinned). All existing capabilities are reused (`inventory.transfers.create/read/edit/delete`, ids in
`capabilities.toml`; `edit` is already labelled "Receive or edit stock transfers"). **No new
capability rows.** Side checks use `require_branch_access` on source (dispatch, cancel) or
destination (receive).

### 5.2 In transit and differences

- **In transit** is not a balance anywhere. It is `SUM(qty_sent)` over `dispatched` transfers,
  read from the transfer tables. The source already lost it and the destination doesn't have it
  yet, so the ledger never double-counts.
- **Short / damaged:** `qty_received < qty_sent`. The gap (`(sent − received) × unit_cost`) is a
  **transit loss**. It is shown on the transfer and in a "Transfer differences" inventory report.
  No extra movement is needed: the source already posted the full send.
- **Over-received:** `qty_received > qty_sent` is allowed **only with a note** (400 without one),
  posted as received and flagged on the report (usually a counting slip at dispatch).

### 5.3 Costing

Unchanged rule ("cost travels with the goods", `src/inventory/handlers.rs:1580`). The only change
is **when** the cost is frozen: at dispatch, so a WAC change at the source while goods are in
transit doesn't revalue them. Destination WAC blends at receive using
`costing::service::apply_weighted_average_cost`.

## 6. Replenishment (warehouse → branch)

`GET /inventory/warehouses/{warehouse_id}/replenishment?branch_id=` (`inventory.transfers.read`):

```
need      = max(max(par_max, par_min) − on_hand − in_transit − open_inbound, 0)   -- only when par_min > 0 and on_hand <= par_min
available = max(warehouse_on_hand − warehouse_drafted_out, 0)
suggested = min(need, available)
```

`in_transit` = dispatched to this branch; `open_inbound` = requested/draft lines to this branch;
`warehouse_drafted_out` = the warehouse's draft lines to anyone. Compared in whole thousandths.
The single source is `madar_inventory::replenish::suggest`, pinned by its vectors.

Rows: ingredient, branch on-hand / par, in transit, warehouse available, suggested. The UI fills
a **draft transfer** from the selected rows. Nothing is automatic: a person dispatches.

Warehouse-side reorder (supplier POs for the warehouse) uses the existing
`/purchasing/branches/{warehouse_id}/reorder-suggestions` as-is. **v1 limitation:** warehouse par
is set by hand. It does not yet roll up branch demand (§10).

## 7. API

New and changed endpoints. Contract types (`StockTransfer`, `StockTransferLine`,
`CreateTransferRequest`, `ReceiveTransferRequest`, `ReplenishmentRow`, `BranchKind`) are defined
in a **new `madar-inventory` crate** in madar-shared, per the "every new API through
madar-shared" rule, together with the pure replenishment math (§6), pinned by
`replenishment_vectors.json`. The handlers live in MadarRust.

| Method | Path | Capability |
|---|---|---|
| POST | `/inventory/transfers` (`CreateTransferRequest`: `lines[]`, `request: bool`; a request is made by the destination side, a draft by the source) | `inventory.transfers.create` |
| POST | `/inventory/transfers/{id}/accept` (`AcceptTransferRequest`: request → draft, optional line edits) | `inventory.transfers.create` |
| POST | `/inventory/transfers/{id}/decline` (`CloseTransferRequest`, note required) | `inventory.transfers.create` |
| PATCH | `/inventory/transfers/{id}` (draft: lines/note; any: note) | `inventory.transfers.create` |
| POST | `/inventory/transfers/{id}/dispatch` | `inventory.transfers.create` |
| POST | `/inventory/transfers/{id}/receive` `{lines:[{line_id, qty_received, note?}], note?}` | `inventory.transfers.edit` |
| POST | `/inventory/transfers/{id}/cancel` (`CloseTransferRequest`; operationId `cancel_stock_transfer`, as `cancel_transfer` is the floor waitlist's) | per status: `.create` before dispatch, `.delete` once in transit |
| GET | `/inventory/transfers/{id}` | `inventory.transfers.read` |
| GET | `/inventory/branches/{id}/transfers?status=&direction=in\|out` (works for warehouses) | `inventory.transfers.read` |
| GET | `/inventory/branches/00000000-0000-0000-0000-000000000000/transfers?status=dispatched` (nil id = org-wide, as today) | `inventory.transfers.read` |
| GET | `/inventory/warehouses/{id}/replenishment?branch_id=` | `inventory.transfers.read` |
| GET | `/inventory/orgs/{org}/transfer-differences?from=&to=` | `inventory.transfers.read` |
| — | `GET /branches` gains `kind` + `?kind=`; `POST`/`PATCH /branches` accept `kind` (§4.3) | existing `branches.*` |
| — | super admin sets `max_warehouses` on the org (existing platform org edit) | super admin |
| removed | `DELETE /inventory/transfers/{id}` (instant reversal) → replaced by cancel / reverse transfer | — |

"Quick transfer" (today's instant B→B UX) = create + dispatch + receive in one dialog for a user
who holds both sides. It's a UI shortcut over the same three calls, not a separate endpoint.

## 8. Dashboard (MadarDashboard)

- **Branches page** (`src/features/branches/`): "Add warehouse" next to "Add branch". The warehouse
  form is just name, address, phone, timezone (no printer, tax, service, tables, float).
- **Scope bar** (`src/components/layout/scope-bar.tsx`): warehouses listed in their own group,
  only on Inventory routes.
- **Inventory ▸ Today** at a warehouse: stock value, low stock, **inbound POs**, **outbound
  transfers to dispatch**, **branch requests (replenishment)**.
- **Inventory ▸ Transfers** (`transfers-page.tsx`, `transfer-dialog.tsx`): list with status tabs
  (Drafts · In transit · Received · Cancelled), direction filter, and a multi-line editor (reuse
  the PO line editor from `purchase-order-dialog.tsx`). Receive dialog mirrors `receive-dialog.tsx`
  (per-line received qty, prefilled with sent).
- **Inventory ▸ Requests**: a branch's "Request stock" (multi-line); at the source, an inbox of
  incoming requests → accept (edit quantities) or decline.
- **Inventory ▸ Replenish** (warehouse only): pick a branch → suggestion table → "Create transfer".
- Selling pages (orders, menu, POS devices, tills, reports) only ever see `kind=branch`.
- i18n en/ar for every new string; RTL as existing.

Flutter (`madar/`): no change for POS/staff, which never pair to a warehouse. `rust-core/madar-api` is
regenerated from `openapi.json`. Flutter `apps/dashboard` transfers nav follows later.

## 9. Rollout

1. madar-shared: new `madar-inventory` crate, contract types + replenishment vectors → tag.
2. MadarRust: migration (kind, guards, transfer lines + backfill with asserts) → handlers →
   `openapi.json`. Rehearse the migration on a restored prod copy (as inventory v2).
3. MadarDashboard: `npm run generate:api`, then branches/scope bar/transfers/replenish UI.
4. Ship backend and dashboard together (the transfer API shape changes, and the dashboard is the
   only real caller).

Tests (smallest set that fails if the logic breaks): dispatch over on-hand is rejected; short
receive leaves the source debited by sent and the destination credited by received; cancel after
dispatch restores the source; an `orders` insert on a warehouse raises; branch → warehouse is
refused with an open till; `max_warehouses` is enforced; over-receive without a note is refused;
the backfill keeps `SUM(quantity)`.

## 10. Decided later / out of scope

1. **Warehouse par from branch demand** (N days of cover from Σ branch consumption). v1: by hand.
2. **Partial receive across days** for one transfer. v1: one receive closes it; send the rest as a
   new transfer.
3. **Central kitchen / production** (warehouse turns ingredients into prepped items). Needs
   sub-recipes, which don't exist yet.
4. **Real plans/billing.** `max_warehouses` is a plain per-org number until a plan system exists.
