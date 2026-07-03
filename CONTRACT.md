# Menu / Recipe / Modifier Unification — CONTRACT (authoritative)

Status: **Wave 1 frozen.** This is the source of truth for Waves 2–5 (backend rewrite, rust-core,
dashboard Menu Studio, storefront, Flutter app). New tables + backfill + shim are implemented and
validated against `~/Desktop/madar_fresh_20260628_184507.dump` and
`~/Desktop/madar_pre_squash_20260628_173930.dump`. If code and this document disagree, this document
wins until amended.

Money is **integer piastres** end to end. **Unknown cost = NULL, never 0.** Order history is
immutable. `(menu_item_id, size_label)` stays resolvable everywhere.

---

## 1. Chosen migration strategy — Expand / Contract (parallel-change), and why

A single big-bang rename/rewrite would break the deployed Flutter fleet (old `madar-core`/`madar-api`
hit old endpoints) and every backend module at once. Instead:

| Phase | Artifact | When | Effect |
|---|---|---|---|
| **EXPAND** | `migrations/20260703100000_menu_unification_expand.sql` | Wave 1 (now) | Additive only: creates the 7 new tables + reconstructs `menu_item_addon_overrides`. Legacy tables stay the live source of truth. Build stays green (no old ref broken). |
| **BACKFILL** | bin `backfill-menu-unification` (`src/menu_unification/backfill.rs`) | after EXPAND, per org | Populates new tables from old with **stable ids**; idempotent; `--dry-run` first; emits an unmigratable-rows report. Order history untouched. |
| **FLIP + SHIM** | `deploy/menu_unification_shim.sql` + Wave-2 handler rewrite | Wave 2 | Drops the legacy catalog/override tables and recreates them as **read-only compat VIEWS** over the new tables; handlers now read/write the new tables; legacy write endpoints translate old→new. |
| **TEARDOWN** | `SHIM_TEARDOWN_PROMPT.md` | Wave 5+, after Flutter fleet updated | Removes the shim views, legacy endpoints, dead columns. |

Rationale: additive EXPAND is reversible and non-breaking; the backfill can be dry-run and re-run
freely; the FLIP is one coordinated deploy with the code that needs it; deployed clients never see a
gap because the shim reprojects the exact old shapes.

> The shim file lives in `deploy/` (NOT `migrations/`) **on purpose** — `sqlx::migrate!` must not
> apply it in Wave 1. It is applied by the Wave-2 deploy, after the backfill has run for every org.

---

## 2. New tables (authoritative shapes in the EXPAND migration)

All created by `20260703100000_menu_unification_expand.sql`. Summary + the reconciliation decisions:

1. **`menu_item_sizes`** `(id, menu_item_id→menu_items CASCADE, label, price int, sort, is_active)`,
   `UNIQUE(menu_item_id,label)`. Authoritative per-item size dictionary. Absolute price in piastres.
   **DEVIATION (documented):** the locked target named this `menu_item_sizes`; the real DB already had
   `item_sizes(id, menu_item_id, label, price_override, is_active)` with **no `sort`** (dropped earlier).
   We create `menu_item_sizes` (adds `sort`, renames `price_override`→`price`), copy `item_sizes`
   verbatim preserving `id`, and **synthesize a `'one_size'` row** (id = `md5(menu_item_id||':one_size')::uuid`,
   price = `menu_items.base_price`) for every item with no size rows. The shim exposes `item_sizes`
   back (minus synthetic `one_size`).

2. **`modifier_groups`** `(id, org_id, name, name_translations, selection_type['single'|'multi'],
   min_selections, max_selections NULL, is_required, sort, is_active, legacy_addon_type NULL, timestamps)`.
   Reusable. `legacy_addon_type` stores the old `addon_items.type` — it is **both** the shim join key
   **and** the milk/coffee swap contract (`'milk_type'→'milk'`, `'coffee_type'→'coffee_bean'`).

3. **`modifier_options`** `(id, group_id→modifier_groups CASCADE, name, name_translations, price int,
   sort, is_default, is_active, replaces_ingredient_id NULL→org_ingredients, legacy_source['addon'|'optional'],
   timestamps)`. **UNIFIES `addon_items` AND `menu_item_optional_fields`.**
   **NON-NEGOTIABLE:** `option.id == old addon_item.id / optional_field.id` (§4).
   **DEVIATION (documented):** added `legacy_source` so the shim can route an option back to the right
   legacy shape and so order-create translation knows addon-vs-optional.

4. **`menu_item_modifier_groups`** `(id, menu_item_id→menu_items CASCADE, group_id→modifier_groups CASCADE,
   sort, min_override NULL, max_override NULL, is_required_override NULL, included_option_ids uuid[] NULL,
   legacy_origin['slot'|'allowlist'|'options'] NULL)`, `UNIQUE(menu_item_id,group_id)`.
   **REPLACES `menu_item_addon_slots` AND `menu_item_allowed_addons`.** `included_option_ids` NULL = all
   options; else the allowlist subset. **DEVIATION (documented):** added `legacy_origin` so the shim can
   reproject `menu_item_addon_slots` (origin `'slot'`) separately from `menu_item_allowed_addons`
   (derived from `included_option_ids`) with full fidelity.

5. **`recipe_lines`** `(id, owner_type['item_size'|'modifier_option'], owner_id uuid, ingredient_id
   NOT NULL→org_ingredients RESTRICT, quantity numeric, unit text, timestamps)`,
   `UNIQUE(owner_type,owner_id,ingredient_id)`. **id-keyed → renames no longer orphan.** REPLACES
   `menu_item_recipes` (owner=item_size), `addon_item_ingredients` (owner=modifier_option), the inline
   optional recipe, and `menu_item_addon_overrides` swaps. `quantity`/`unit` are the base-unit,
   yield-normalized values (unchanged from today; normalization stays in `recipes::handlers::normalize_recipe_unit`
   with `org_ingredients.density_g_per_ml` + `yield_pct`).
   **DEVIATION (documented):** `CHECK (quantity >= 0)` (not `> 0`) — `quantity = 0` is a legitimate
   **swap marker** for `milk_type`/`coffee_type` options (the deducted amount comes from the base item
   recipe at order time; the legacy data carries these as 0-qty `addon_item_ingredients` rows).

6. **`menu_price_overrides`** `(id, scope['branch'|'channel'|'branch_channel'], branch_id NULL→branches,
   channel NULL delivery_channel, target_type['menu_item_size'|'modifier_option'], target_id uuid,
   price int NULL, is_available bool NULL, updated_at)`. **MERGES** `branch_menu_overrides`,
   `branch_menu_size_overrides`, `branch_addon_overrides`, `branch_channel_menu_overrides`,
   `branch_channel_addon_overrides`. Per-size branch availability is now expressible (was not). One
   partial unique key per scope shape. A CHECK enforces the scope↔(branch_id,channel) shape and that a
   row sets at least one of price/is_available.

7. **`catalog_revision`** `(org_id PK→organizations, revision bigint default 1, updated_at)`. Per-org
   monotonic version; Wave-2 catalog writes bump it; the offline POS compares its cached revision.

8. **`menu_item_addon_overrides`** — **reconstructed DDL** (per-item ingredient swaps). This table was
   referenced only by `src/menu/handlers.rs` (list/upsert/delete override endpoints) and had **no
   migration** — it does not exist in any dump; those endpoints 500 at runtime today. Reconstructed
   from the handler's SELECT (`handlers.rs:1769-1794`) / INSERT (`:1948-1975`) as
   `(id, menu_item_id, addon_item_id, size_label NULL, ingredient_name, org_ingredient_id NULL,
   ingredient_unit, quantity_used, replaces_org_ingredient_id NULL, combo_addon_item_id NULL, timestamps)`
   with 4 partial unique indexes mirroring the handler's manual 4-way upsert. Created `IF NOT EXISTS`
   for schema consistency; empty in practice; any rows are reported for manual review (no reusable-option
   home). Wave 2 deletes the dead endpoints; TEARDOWN drops the table.

---

## 3. Override resolution order (documented COALESCE precedence)

For a target (a `menu_item_size` or a `modifier_option`), a branch `B`, and a channel `C`, resolve
**each field independently**, most-specific-scope first:

```
effective_price(target, B, C) = COALESCE(
    (branch_channel override where branch=B, channel=C).price,
    (branch         override where branch=B           ).price,
    (channel        override where channel=C           ).price,   -- org-wide per-channel; new, no legacy source
    catalog_default_price(target)                                 -- menu_item_sizes.price | modifier_options.price
)

effective_available(target, B, C) = COALESCE(
    (branch_channel override where branch=B, channel=C).is_available,
    (branch         override where branch=B           ).is_available,
    (channel        override where channel=C           ).is_available,
    TRUE
)
```

Price **always** resolves (falls back to the catalog default) — it is never NULL. Availability defaults
to TRUE. `is_available` and `price` are resolved separately (a row may set one and inherit the other).
This subsumes the legacy per-item / per-size / per-addon / per-channel tables.

---

## 4. Stable-id rules (invariant — never break)

Immutable order history keeps these FK **values** and they must keep resolving:

| Order-history column | Was FK → | New resolution |
|---|---|---|
| `order_item_addons.addon_item_id` | `addon_items(id)` | `modifier_options(id)` — **same uuid** |
| `order_item_optionals.optional_field_id` | `menu_item_optional_fields(id)` | `modifier_options(id)` — **same uuid** |
| `order_item_optionals.org_ingredient_id` | `org_ingredients(id)` | unchanged |

Enforcement: the backfill copies the source id verbatim (`INSERT INTO modifier_options SELECT
old.id, …`). addon ids and optional ids never collide (disjoint uuid populations). After the FLIP the
hard FKs are dropped (a table can't be a view); the values become **soft references** validated against
the compat views / new tables. Reports (`reports::handlers` addon report) and the cost-repricing
backfill (`costing::backfill`) that re-join `addon_item_id` → catalog therefore keep resolving.

Group ids and item↔group attachment ids are **new** (deterministic `md5(...)::uuid` for idempotency) —
nothing in order history references them, so they need not be stable.

Validation (fresh dump, all 4 orgs, committed): `order_item_addons` unresolved = **0**,
`order_item_optionals` unresolved = **0**, no orphans in any new table, option count parity exact
(75 = 54 addons + 21 optionals).

---

## 5. NEW endpoints (Wave-2 API — JSON shapes are authoritative)

Money fields are integer piastres; `*_cost_piastres` is `int|null` (null = unknown, never 0). Names carry
`*_translations` jsonb. All under the existing auth/permission model.

### 5.1 Dashboard Menu Studio (one screen per item)

**`GET /menu-items/{id}/studio`** → the full item aggregate the one-page editor renders:
```jsonc
{
  "id": "uuid", "org_id": "uuid", "name": "…", "name_translations": {}, "description": "…",
  "image_url": "…", "category_id": "uuid|null", "is_active": true, "catalog_revision": 42,
  "sizes": [
    { "id": "uuid", "label": "small", "price": 4500, "sort": 0, "is_active": true,
      "recipe": [ { "id":"uuid","ingredient_id":"uuid","ingredient_name":"Milk","quantity":"0.200","unit":"l",
                    "line_cost_piastres": 1200 } ],
      "cost_piastres": 1200, "cost_incomplete": false } ],
  "modifier_groups": [                      // reusable groups attached to this item
    { "attachment_id":"uuid","group_id":"uuid","name":"milk_type","name_translations":{},
      "selection_type":"single","legacy_addon_type":"milk_type",
      "min":1,"max":1,"is_required":true,"sort":0,
      "options": [
        { "id":"uuid","name":"Oat Milk","price":1000,"is_default":false,"is_active":true,
          "included":true,                  // false = offered by group but not on this item
          "replaces_ingredient_id":"uuid|null",
          "recipe":[{ "ingredient_id":"uuid","quantity":"0.000","unit":"l" }],  // 0 = swap marker
          "cost_piastres": null } ] } ],
  "options": [                              // priced optionals (item-private 'Options' group)
    { "id":"uuid","name":"Extra shot","price":500,"is_active":true,
      "recipe":[{ "ingredient_id":"uuid","quantity":"0.007","unit":"kg" }],
      "cost_piastres": 320 } ],
  "availability": {
    "org_active": true,
    "branches": [ { "branch_id":"uuid",
      "sizes":[ { "size_id":"uuid","price":null,"is_available":false } ],   // null = inherit
      "channels":[ { "channel":"outside","sizes":[ {"size_id":"uuid","price":5000,"is_available":null} ] } ] } ] },
  "used_in_bundles": [ { "bundle_id":"uuid","name":"Combo A" } ]
}
```

Mutations (each bumps `catalog_revision`):
- **`PUT /menu-items/{id}`** — basics `{name,name_translations,description,image_url,category_id,is_active}`.
- **`PUT /menu-items/{id}/sizes`** — replace-set `[{label,price,sort,is_active}]` (adds/updates/deactivates; never deletes a size that has order history — soft-deactivate).
- **`PUT /menu-item-sizes/{size_id}/recipe`** — replace lines `[{ingredient_id,quantity,unit}]`; server normalizes to base unit; returns live `cost_piastres`/`cost_incomplete`.
- **`GET/POST/PATCH/DELETE /modifier-groups`**, **`…/modifier-groups/{gid}/options`**, **`PUT /modifier-options/{oid}/recipe`** — reusable group + option CRUD (addon_type picked from a managed dropdown = `legacy_addon_type` value).
- **`PUT /menu-items/{id}/modifier-groups`** — attach-set `[{group_id,sort,min_override,max_override,is_required_override,included_option_ids}]`.
- **`PUT /menu-items/{id}/options`** — the priced optionals `[{id?,name,price,recipe:[{ingredient_id,quantity,unit}]|null,is_active}]` (server owns the per-item `Options` group).
- **`PUT /menu-price-overrides`** / **`DELETE /menu-price-overrides`** — upsert/delete one override `{scope,branch_id,channel,target_type,target_id,price,is_available}` (the merged Pricing & Availability view).
- **`POST /menu-items/{id}/duplicate`** — deep copy: basics + sizes + size recipes + group attachments (+ `included_option_ids`) + the item's options (+ their recipes) + overrides. Option copies get **new** ids (a duplicate is a new item, no order history). Fixes the "Duplicate silently drops slots/optionals/allowlist" bug.
- **`GET /menu-items/{id}/cost`** — live per-size cost via `costing::service` for the inline "fix cost" flow (no navigate-away).

### 5.2 POS catalog sync

**`GET /catalog/sync?branch_id={uuid}&channel={ch}&since={revision}`** →
```jsonc
{ "catalog_revision": 42,
  "changed": true,                          // false ⇒ client is current (else full/delta payload)
  "items":[ { "id","name","name_translations","category_id",
    "sizes":[{"id","label","price": <effective>, "is_available": <effective>}],
    "modifier_groups":[{ "group_id","selection_type","min","max","is_required","legacy_addon_type",
        "options":[{"id","name","price": <effective>,"is_available": <effective>,
                    "replaces_ingredient_id","recipe":[{ingredient_id,quantity,unit}]}] }] } ],
  "ingredients":[…] }
```
Prices/availability are already resolved for `(branch_id, channel)` per §3. The POS resyncs when its
cached `catalog_revision` < server.

---

## 6. LEGACY shim endpoints (deployed Flutter must not break)

After the FLIP, the OLD endpoints keep their **exact JSON shapes**, served from the compat views
(`deploy/menu_unification_shim.sql`) + thin write-translation. Deployed clients change nothing.

**Reads** (served from views): `GET /menu-items`, `…/{id}`, `GET /addon-items`,
`GET /menu-items/{id}/addon-slots`, `…/optional-fields`, `…/allowed-addons`, `GET /branch-menu-overrides`,
`…/branch-menu-size-overrides`, `…/branch-addon-overrides`, channel-override GETs, recipe GETs. Each view
(`addon_items`, `addon_item_ingredients`, `menu_item_addon_slots`, `menu_item_optional_fields`,
`menu_item_allowed_addons`, `menu_item_recipes`, `item_sizes`, and the 5 branch/channel override views)
reprojects the new tables into the legacy columns. Round-trip fidelity validated (see §8).

**Writes** — deployed clients only write **order history**:
- **`POST /orders`** (and delivery order create) with `addons:[{addon_item_id,…}]`,
  `optionals:[{optional_field_id,…}]`, `size_label`. Translation is a **stable-id pass-through**:
  `addon_item_id`/`optional_field_id` already equal `modifier_options.id`; `size_label` resolves via
  `menu_item_sizes`. Rows are written to the untouched `order_item_addons` / `order_item_optionals` /
  `order_items` tables. Addon/optional price + cost are resolved from the new tables (via §3 + `costing::service`).
- Legacy **catalog** writes (old dashboard) are **not** re-supported — catalog authoring moves to the new
  Menu Studio (Wave 3). The deployed POS does not write catalog.

The shim stays until the new Flutter build is fleet-wide; then `SHIM_TEARDOWN_PROMPT.md` removes it.

---

## 7. Backfill behavior + unmigratable categories

Bin: `cargo run --bin backfill-menu-unification -- (--org <uuid> | --branch <uuid>) [--dry-run]`.
Operator-only, never HTTP. Idempotent (clears the org's new-table rows, then rebuilds). One transaction;
`--dry-run` rolls back. Mirrors `recipes::backfill` structure (`BackfillScope`, counters + `Vec` report).

Mapping (per org): `item_sizes`→`menu_item_sizes` (+ synth `one_size`); each `(org,addon_items.type)`→a
`modifier_group` (+ `legacy_addon_type`); each `addon_items` row→a `modifier_option` (id preserved) in
that group; each item with optionals→a per-item `Options` group; each `menu_item_optional_fields`
row→a `modifier_option` (id preserved); `menu_item_recipes`/`addon_item_ingredients`/inline-optional
recipes→`recipe_lines` (ingredient resolved by id else by `(org,name)`); `menu_item_addon_slots`→
`menu_item_modifier_groups` (`legacy_origin='slot'`); allowlist-only types→attaches
(`legacy_origin='allowlist'`); `Options` group attaches (`legacy_origin='options'`);
`included_option_ids` = the allowlisted subset; the 5 legacy override tables→`menu_price_overrides`;
seed `catalog_revision=1`.

**Report categories** (`kind`): `recipe.ingredient_unresolved`, `recipe.size_unmatched`,
`recipe.negative_qty`, `addon.ingredient_unresolved`, `optional.ingredient_unresolved`,
`optional.size_scoped` (size-scoped optional deduction — size scoping is not representable on an option;
deduction now applies to all sizes), `branch_menu.price_on_sized_item` /
`branch_channel_menu.price_on_sized_item` (a base-price override on a multi-size item has no `one_size`
target; not applied), `branch_menu_size.size_unmatched`, `size.negative_price_clamped`,
`addon_override.manual_review`, and the informational `info.implicit_all_addons`.

**`info.implicit_all_addons` — documented semantic decision:** items with **no** slots and **no**
allowlist relied on the legacy implicit "offer all org addons" default. The new model is
**explicit-attachment** (the dashboard authors offered options), so these items get **no** auto-attached
groups. Deployed clients keep their behavior via the shim (empty slots/allowlist ⇒ their own default).
The count is reported per org, not treated as a failure.

---

## 8. Dry-run results (both dumps) + validation

Both dumps are the same data at different squash states; both reach head via `sqlx migrate run` (incl.
the enum→text migration and this EXPAND) and backfill **identically**.

Per-org (fresh dump, committed): sizes copied 4, `one_size` synth 163, groups 12, options 75,
attaches 56, recipe_lines 514, price_overrides 111. **Unmigratable failures: 0** (only two
`info.implicit_all_addons` notes: 83 items in First Crack, 44 in Rue).

Integrity (committed, all 4 orgs):
- `addon_items`/`optional_fields` preserved as options: **0 missing**; option parity **75 = 54 + 21**.
- **Stable-id**: `order_item_addons.addon_item_id` unresolved **0**; `order_item_optionals.optional_field_id`
  unresolved **0**.
- No orphans: recipe_lines owners, mpo targets all resolve; every item has ≥1 size.
- `(menu_item_id, size_label)` SKU key: **39** historical order lines reference a retired size
  (`small`/`medium` on items now size-less). **Benign & pre-existing**: order history is immutable and
  self-contained (it snapshots its own price/cost/size_label); current-cost for a retired size resolves
  to **NULL** (unknown), never 0. Not a backfill defect.

Shim round-trip fidelity (views vs pre-flip snapshots): `addon_items`, `item_sizes` (minus synthetic
`one_size`), all **1109** `allowed_addons`, `optional_fields`, `addon_item_ingredients`,
`branch_menu_size_overrides`, `branch_addon_overrides`, `branch_menu_overrides` (meaningful),
`menu_item_addon_slots` (correctly **0** — dumps had 0 slots): **0 diff**. `menu_item_recipes`: stable
identity `(item,size,ingredient_id,qty,unit)` **0 diff**; the only differences are 4 rows where the view
shows the **canonical current** ingredient name vs a stale denormalized copy — i.e. the rename-orphan bug
**fixed**.

---

## 9. Bundle cost re-route (Wave 2, tracked here)

`bundles::compute_item_cost` (`bundles/handlers.rs:219`) is the **only** divergent cost engine. It costs
one arbitrary size (`ORDER BY size_label LIMIT 1`, `:231`), is `org_ingredients`-only (no per-branch
actual), uses `float8`, treats recipe-less as free (`COUNT(*)=0 THEN 0`), and the display path
`.unwrap_or(0)` (`:281`) zeros unknown cost — an invariant violation. **Wave 2 retires it and routes
bundle cost through `costing::service`** (correct per-size, per-branch, NULL-aware). Because
`bundle_components` has no size dimension, the component→SKU size choice must be made explicitly (spec:
cost each component at its default/one_size size unless the bundle pins a size). Wave 5 produces the
**margin-flip diff**: enumerate active bundles, compute `sum_costs` old vs new, flag every bundle whose
`price ≥ 1.20 × sum_costs` pass/fail flips (floor at `bundles/handlers.rs:383`).

---

## 10. Invariants honored (checklist)

- [x] Money = integer piastres end to end (new `price`/override columns are `int`; costs `bigint`).
- [x] Unknown cost = NULL, never 0 (recipe_lines qty=0 is a swap marker, not unknown cost; cost rollups keep `costing::service` NULL-tolerance).
- [x] Order history immutable (backfill never writes `order_*`; shim never rewrites history; stable ids).
- [x] `(menu_item_id, size_label)` resolvable (size dictionary + synth `one_size`; retired sizes handled per §8).
- [x] Soft-delete discipline (`is_active` on groups/options/sizes; `deleted_at` on items/ingredients unchanged).
- [x] Deployed Flutter unaffected (shim views + stable-id order-create translation).
