# Staff drink comp — the wire contract

Owner rule, 2026-09-21. Background: `../STAFF_POOL.md` (the pool: a branch's
daily allowance, the eligible list, the REQUIRED note, overspend recorded and
flagged, never refused).

Until now putting a line "on the pool" only wrote a `staff_drinks` side record;
nothing linked it to the sale's price. From this change **a staff drink is a
normal order whose pooled line is comped, and the server prices the comp**.

Everything here is ADDITIVE. POS v0.5.0–v0.7.12 never send the new field and
keep using `POST /staff-pool/drinks` / the `record_staff_drink` replay op, which
are unchanged.

## 1. The rule, in plain words

A staff drink gets a 100% discount on its **base configuration** only.

1. **Size.** The free amount is the price of the item's **cheapest active
   size**, at this branch's prices. A bigger size pays the difference. An item
   with no sizes (or a line that names no size, which rings at the item's
   "from" price): that price is the free amount.
2. **Optional add-ons and optionals are never free.** They ring at their normal
   price.
3. **Required choice groups** (the group's minimum is ≥ 1). Per group, the free
   allowance is the price of the group's **default option × the minimum**. With
   no active default, the **cheapest active option × the minimum**. What was
   actually picked in that group pays only the amount by which it exceeds the
   allowance: an equal or cheaper alternative is free, a pricier one pays the
   difference. Never negative — a cheaper pick creates no credit for another
   group or for the size.
4. **Per unit:** `comp = size part + Σ group parts`, each part capped by what
   that part actually rang at; `charged = normal − comp ≥ 0`. A line of `n`
   units is `n` drinks off the allowance and `n ×` the per-unit comp.
5. **Tax and service apply to the charged part only.** The comp comes off the
   line *before* the subtotal is formed, exactly as a loyalty reward does; the
   bill engine (`tax_vectors.json`) then runs on what is left.
6. **An order-level discount applies after the comp**, to what remains.
7. **The backend owns the money.** Live, the server computes the comp and any
   figure the client sent is ignored. On offline replay the till's figure is
   accepted (the sale happened), the server recomputes, stores ITS OWN verdict
   beside the till's, and flags a difference. A replayed sale is never refused.
8. **A bundle line is never on the pool** (`item_not_eligible`). **A table's
   bill never carries a staff drink** (§6).

The arithmetic is `src/staff_pool/comp.rs` — a pure function, integers only, no
rounding anywhere. It is pinned by **`tests/fixtures/staff_comp_vectors.json`**
(33 cases), which is copied VERBATIM into
`madar/rust-core/crates/madar-core/tests/fixtures/`. Change the rule in that
file first and let both repos fail until they agree.

### Interpretations of this codebase's menu model (owner: please confirm)

* **"Required".** A group is required for an item when its effective minimum
  (`menu_item_modifier_groups.min_override`, else `modifier_groups.min_selections`)
  is ≥ 1. A group flagged `is_required` (or `is_required_override`) with a
  minimum of 0 is read as minimum **1**.
* **"Default".** `modifier_options.is_default`, the first active one in display
  order (`sort, name, id`). An inactive default, an option the attachment's
  allow-list (`included_option_ids`) leaves out, or one this branch has switched
  off (`branch_addon_overrides.is_available = false`) does not count; the
  cheapest active option then sets the allowance.
* **Option prices** are what the order resolver charges for a pick:
  `addon_items.default_price` under `branch_addon_overrides.price_override`.
* **Swap groups are not "required groups" for the comp.** Milk and bean groups
  (`effect = 'swaps'`, or legacy type `milk_type` / `coffee_type`) are already
  priced by the resolver as the DIFFERENCE over the recipe's own ingredient —
  the base milk rings at 0, oat rings at `oat − base`. That is the rule's
  "pricier alternative pays the difference" already, so their picks reach the
  engine as plain extras. Passing them as required groups too would discount
  the difference a second time. The outcome equals the rule whenever the
  group's default option is the recipe's own ingredient (the normal
  modelling). Where a catalogue marks a DIFFERENT, priced milk as default, this
  reading charges its difference and the literal rule would not.
* **Optionals** (`optional_field_ids`, the item-private "Options" group) are
  never free, whatever that group's minimum says — they have no per-pick price
  row an allowance could land on.
* **Size.** Hot / iced / blended modelled as sizes of one item
  (DROPS_SIZES_AND_PACKAGING) are sizes like any other: the cheapest active one
  is the free amount and an iced drink pays its difference over it.
* **A reward and the pool on one line:** refused live (400); on replay the comp
  is applied first and the reward covers what is left.

## 2. Requests

### `POST /orders` — each element of `items` may carry

```jsonc
"staff_drink": {
  "id":   "<uuid>",     // client-minted. THE idempotency key, and the id of the staff_drinks row
  "note": "for Sara",   // REQUIRED, non-blank after trim
  "comp_minor": 7000,   // OPTIONAL. The till's comp for the WHOLE line. Read on replay only
  "overspent": false    // OPTIONAL. What the till believed. Read on replay only
}
```

Live behaviour, all inside the order's transaction:

| Step | Outcome |
|---|---|
| Actor lacks `orders.staff_drink.record` (id 223, approval = true) | `403`, unless `live_approval` on the request is a valid manager approval for that capability (same object and same verification a live discount uses); the approval is then recorded in `approvals` |
| Pool off / empty list / item not on the list / blank note / bundle line | `400`, body `{"code": "<token>", …}` with the engine's tokens: `pool_off`, `no_eligible_items`, `item_not_eligible`, `note_required`. The whole sale is refused — nothing was rung |
| Line also named in `loyalty_redemptions` | `400` |
| `staff_drink.id` already attached to a DIFFERENT order | `409` |
| Past the allowance | **allowed**: the row is `overspent`, the response `warnings` says so |
| Otherwise | comp computed by the server; `comp_minor`, `subtotal`, `discount_amount`, `tax_amount`, `total_amount` sent by the client are NOT compared (as with a loyalty reward, the server's bill stands). `payment_splits` must still sum to the server's total |

A `staff_drinks` row with that `id` that already exists (an older flow recorded
it first, or a retry) is **reused**: the order and the money are attached to it
and it is NOT counted off the allowance a second time.

### `/sync/replay` — `create_order`

Same `request` shape. The till prices offline with its mirror of the rule and
sends, per pooled line, normal `unit_price` / addon `unit_price`s and
`staff_drink.comp_minor`; and, per order, `subtotal` / `total_amount` NET of the
comp, as it charged them.

* The till's `comp_minor` (clamped to what the line rang at) is what comes off
  the stored money — the sale already happened.
* The server recomputes and stores its verdict in `staff_drinks.comp_minor`,
  the till's in `staff_drinks.comp_minor_reported`.
* `comp_minor` omitted on a pooled line ⇒ the server's comp is applied and the
  server's bill stands for that order.
* Nothing refuses. A blank note is stored as `(no note given)` and flagged.
* The existing total-drift check still applies to the bill the till states
  (`subtotal − discount` → tax → total under the branch policy, ±1).

Flags written to `authz_replay_flags` (`op = 'CreateOrder'`, `subject_id` = the
order):

| `capability` | Meaning |
|---|---|
| `orders.staff_drink.record` | the author does not hold the act and no valid approval rode the envelope |
| `orders.staff_drink.record:comp_mismatch` | till comp ≠ server comp |
| `orders.staff_drink.record:overspent` | the server's count makes it an overspend the till did not report |
| `orders.staff_drink.record:device_overcounted` | the till said over, the server does not |
| `orders.staff_drink.record:pool_off` / `:no_eligible_items` / `:item_not_eligible` / `:note_required` | the pool would have refused it live (server comp verdict is then 0) |
| `orders.staff_drink.record:duplicate_id` | the drink id is already attached to another order |

### Unchanged

`POST /staff-pool/drinks` and the `record_staff_drink` replay op: same body,
same behaviour, same flags. Rows they write have `comp_minor = null`.

## 3. Responses (additive)

* **Order line** (`OrderFull.items[]`, order export, `/sync/pull` `order`
  rows): `staff_comp_minor: int` (total comp on the line, 0 on a paid line),
  `staff_drink_id: uuid | null`.
* **Order line add-on** (`items[].addons[]`): `staff_comp_minor: int` — the
  part of the comp that pick absorbed.
* **`StaffDrink`** (`POST|GET /staff-pool/drinks`, replay answer):
  `comp_minor`, `extras_minor`, `comp_minor_reported` (all nullable).
* **`GET /staff-pool/drinks/summary`** (new; same query as the list):
  `drinks, quantity, overspent, comp_minor, extras_minor, cost_minor,
  comp_mismatches, unpriced`.
* **`/sync/pull` `staff_drink` rows:** `comp_minor`, `extras_minor`.

### How the money is stored — read this before printing a receipt

The comp is **already taken off** the stored figures; never subtract it again.

| Field | Value |
|---|---|
| `items[].unit_price`, `addons[].unit_price` | the NORMAL price |
| `items[].line_total` | `unit_price × quantity − size part of the comp` |
| `addons[].line_total` | `unit_price × quantity × line qty − addons[].staff_comp_minor` |
| `items[].staff_comp_minor` | size part + Σ add-on parts |
| order `subtotal`, `discount_amount`, `tax_amount`, `total_amount`, payments | over the CHARGED part only |
| `line_cost`, `unit_cost`, `staff_drinks.cost_minor` | the FULL cost — the drink was made |

Receipt: print each line at its normal price, then one line-discount row
"Staff drink −`staff_comp_minor`".

Because the stored figures are net, every report that sums `line_total`,
`subtotal` or `total_amount` — sales, item sales, add-on sales, the Z / till
report, POS metrics, `compute_system_cash` — counts only what was charged with
**no formula change**, so `till_report_vectors.json` is NOT regenerated.

## 4. Schema — `20260926090000_staff_drink_comp.sql`

`order_items.staff_comp_minor int NOT NULL DEFAULT 0`,
`order_items.staff_drink_id uuid NULL` (soft link, no FK),
`order_item_addons.staff_comp_minor int NOT NULL DEFAULT 0`,
`staff_drinks.comp_minor / extras_minor / comp_minor_reported int NULL`.

## 5. What the POS core must implement

1. Copy `tests/fixtures/staff_comp_vectors.json` verbatim and make a
   `staff_comp::comp(input) -> result` pass it: same input/output field names
   (`eligible, unit_price, sizes[], groups[], picks[], optionals_per_unit,
   quantity` → `free_per_unit, charged_per_unit, line_comp, line_charged,
   breakdown{normal_per_unit, free_size, size_comp, groups[], picks[]}`).
2. Build the input exactly as §1 "Interpretations" says: sizes only when the
   line names a size; required non-swap groups from the item's attachments;
   picks at the prices the cart already rings (swap picks at their difference).
3. On a pooled cart line: run the existing pool decision first (`decide`); if
   refused, do not comp. Otherwise show the line at normal price with a
   "Staff drink −comp" row; take the comp off the line BEFORE the bill engine,
   and apply any order discount to the remainder.
4. Send `staff_drink { id, note, comp_minor: line_comp, overspent }` on the
   order line — live and queued alike — with `subtotal` / `total_amount` net of
   the comp. **Stop sending the separate `record_staff_drink` op for that
   drink**; if an older queue already did, reuse the same `id` so the server
   attaches instead of double-counting.
5. Never offer the pool on a bundle line or on a table ticket's line.
6. Read `staff_comp_minor` / `staff_drink_id` from order rows and
   `comp_minor` / `extras_minor` from `staff_drink` rows on the feed; all
   optional, decode leniently.

## 6. Paths that do NOT carry it

* **Open tickets (fire / settle).** A table's bill is priced when a round is
  fired and settled hours later under a frozen policy; the pool is counted per
  business day at the moment of sale. Live fire with a `staff_drink` line →
  `400 staff_drink_not_on_ticket`. A round fired OFFLINE already reached the
  kitchen, so on replay the field is stripped and the line rings in full.
* **Delivery finalize / public ordering.** Customer-authored carts; no staff
  line exists there.
* **Service charge** is dine-in (ticket) only, so a pooled line never meets
  one; rule 5 holds trivially for service and is enforced for tax.
