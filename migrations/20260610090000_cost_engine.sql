-- ═══════════════════════════════════════════════════════════════════
-- Cost engine: order-line cost snapshots, epoch/history baselines,
-- backfill from deductions_snapshot, and dead-table removal.
--
-- Money convention (unchanged):
--   * prices            → integer piastres (minor units)
--   * ingredient costs  → numeric(15,2) EGP per unit in org_ingredients /
--                         ingredient_cost_history
--   * NEW cost columns  → bigint piastres. NULL = unknown, never 0-as-unknown.
-- ═══════════════════════════════════════════════════════════════════

-- ── 1. Cost columns on order lines ──────────────────────────────────

-- line_cost  : full COGS of the line in piastres — recipe (incl. addon swaps)
--              + additive addons + optionals + bundle components, × quantity.
-- unit_cost  : recipe-only cost per unit in piastres (incl. swaps, excl.
--              additive addons/optionals). This is the cost the Menu Advisor
--              compares against unit_price. NULL ⟺ recipe cost unknown.
-- cost_missing: TRUE ⟺ any component of line_cost could not be resolved
--              (unlinked ingredient, no cost, or no recipe at all).
ALTER TABLE order_items
    ADD COLUMN IF NOT EXISTS line_cost    bigint,
    ADD COLUMN IF NOT EXISTS unit_cost    bigint,
    ADD COLUMN IF NOT EXISTS cost_missing boolean NOT NULL DEFAULT true;

ALTER TABLE order_item_addons
    ADD COLUMN IF NOT EXISTS line_cost bigint;

ALTER TABLE order_item_optionals
    ADD COLUMN IF NOT EXISTS cost bigint;

ALTER TABLE order_line_bundle_components
    ADD COLUMN IF NOT EXISTS line_cost bigint;

CREATE INDEX IF NOT EXISTS idx_order_items_menu_item
    ON order_items (menu_item_id) WHERE menu_item_id IS NOT NULL;

-- ── 2. Baseline ingredient cost history ─────────────────────────────
-- Ingredients created before history maintenance existed have no rows;
-- point-in-time lookups would come back empty. Seed a baseline epoch at
-- the ingredient's creation time with its current cost.
INSERT INTO ingredient_cost_history (org_ingredient_id, cost_per_unit, effective_from, note)
SELECT oi.id, oi.cost_per_unit, oi.created_at, 'baseline (cost-engine migration)'
FROM org_ingredients oi
WHERE NOT EXISTS (
    SELECT 1 FROM ingredient_cost_history h WHERE h.org_ingredient_id = oi.id
);

-- Repair history rows that drifted from the live cost: if the open epoch
-- disagrees with org_ingredients.cost_per_unit, close it and open a fresh one.
WITH open_epochs AS (
    SELECT h.id, h.org_ingredient_id, h.cost_per_unit AS hist_cost,
           oi.cost_per_unit AS live_cost
    FROM ingredient_cost_history h
    JOIN org_ingredients oi ON oi.id = h.org_ingredient_id
    WHERE h.effective_until IS NULL
      AND h.cost_per_unit <> oi.cost_per_unit
),
closed AS (
    UPDATE ingredient_cost_history h
    SET effective_until = now()
    FROM open_epochs oe
    WHERE h.id = oe.id
    RETURNING oe.org_ingredient_id, oe.live_cost
)
INSERT INTO ingredient_cost_history (org_ingredient_id, cost_per_unit, effective_from, note)
SELECT org_ingredient_id, live_cost, now(), 'drift repair (cost-engine migration)'
FROM closed;

-- ── 3. Baseline price epochs ─────────────────────────────────────────
-- The advisor's "recently repriced" suppression reads these tables; items
-- predating epoch maintenance need a baseline so point-in-time price is
-- always resolvable.
INSERT INTO menu_item_price_epochs (menu_item_id, size_label, price, effective_from)
SELECT mi.id, NULL, mi.base_price, mi.created_at
FROM menu_items mi
WHERE NOT EXISTS (
    SELECT 1 FROM menu_item_price_epochs e
    WHERE e.menu_item_id = mi.id AND e.size_label IS NULL
);

INSERT INTO menu_item_price_epochs (menu_item_id, size_label, price, effective_from)
SELECT sz.menu_item_id, sz.label::text, sz.price_override, mi.created_at
FROM item_sizes sz
JOIN menu_items mi ON mi.id = sz.menu_item_id
WHERE NOT EXISTS (
    SELECT 1 FROM menu_item_price_epochs e
    WHERE e.menu_item_id = sz.menu_item_id AND e.size_label = sz.label::text
);

INSERT INTO bundle_price_epochs (bundle_id, price, effective_from)
SELECT b.id, b.price, b.created_at
FROM bundles b
WHERE NOT EXISTS (
    SELECT 1 FROM bundle_price_epochs e WHERE e.bundle_id = b.id
);

-- ── 4. Backfill order_items costs from deductions_snapshot ───────────
-- Point-in-time ingredient cost at the order's created_at (falls back to the
-- baseline seeded above, i.e. current cost, for legacy data — approximate by
-- design, agreed). Costs convert EGP → piastres (× 100).
--
-- unit_cost  ← entries with source 'drink_recipe' or 'addon_swap:%' (the
--              serving recipe, swaps included), ÷ quantity.
-- line_cost  ← all entries.
-- cost_missing ← any entry whose ingredient or cost could not be resolved,
--              OR no entries at all (no recipe).
WITH entry_costs AS (
    SELECT
        oi.id AS order_item_id,
        oi.quantity,
        (e.entry->>'quantity')::numeric                    AS qty,
        NULLIF(e.entry->>'org_ingredient_id', '')::uuid    AS ing_id,
        COALESCE(e.entry->>'source', '')                   AS source,
        c.cost_per_unit                                    AS unit_cost_egp
    FROM order_items oi
    JOIN orders o ON o.id = oi.order_id
    CROSS JOIN LATERAL jsonb_array_elements(oi.deductions_snapshot) AS e(entry)
    LEFT JOIN LATERAL (
        SELECT h.cost_per_unit
        FROM ingredient_cost_history h
        WHERE h.org_ingredient_id = NULLIF(e.entry->>'org_ingredient_id', '')::uuid
          AND h.effective_from <= o.created_at
          AND (h.effective_until IS NULL OR h.effective_until > o.created_at)
        ORDER BY h.effective_from DESC
        LIMIT 1
    ) c ON TRUE
    WHERE oi.line_cost IS NULL
      AND jsonb_typeof(oi.deductions_snapshot) = 'array'
),
rollup AS (
    SELECT
        order_item_id,
        bool_or(ing_id IS NULL OR unit_cost_egp IS NULL)            AS any_missing,
        round(SUM(qty * COALESCE(unit_cost_egp, 0)) * 100)::bigint  AS line_cost_pst,
        round(SUM(qty * COALESCE(unit_cost_egp, 0))
              FILTER (WHERE source = 'drink_recipe' OR source LIKE 'addon_swap:%')
              * 100)::bigint                                        AS recipe_cost_pst,
        bool_or((ing_id IS NULL OR unit_cost_egp IS NULL)
                AND (source = 'drink_recipe' OR source LIKE 'addon_swap:%'))
                                                                    AS recipe_missing,
        COUNT(*) FILTER (WHERE source = 'drink_recipe' OR source LIKE 'addon_swap:%')
                                                                    AS recipe_entries,
        MAX(quantity)                                               AS quantity
    FROM entry_costs
    GROUP BY order_item_id
)
UPDATE order_items oi
SET line_cost    = CASE WHEN r.any_missing THEN NULL ELSE r.line_cost_pst END,
    unit_cost    = CASE
                       WHEN r.recipe_missing OR r.recipe_entries = 0 THEN NULL
                       ELSE round(r.recipe_cost_pst::numeric / GREATEST(r.quantity, 1))::bigint
                   END,
    cost_missing = r.any_missing
FROM rollup r
WHERE r.order_item_id = oi.id;

-- ── 5. Backfill order_item_optionals.cost ────────────────────────────
-- Optionals carry their own ingredient linkage on the row.
UPDATE order_item_optionals oo
SET cost = sub.cost_pst
FROM (
    SELECT oo2.id,
           round(oo2.quantity_deducted * c.cost_per_unit * 100)::bigint AS cost_pst
    FROM order_item_optionals oo2
    JOIN order_items oi ON oi.id = oo2.order_item_id
    JOIN orders o       ON o.id = oi.order_id
    JOIN LATERAL (
        SELECT h.cost_per_unit
        FROM ingredient_cost_history h
        WHERE h.org_ingredient_id = oo2.org_ingredient_id
          AND h.effective_from <= o.created_at
          AND (h.effective_until IS NULL OR h.effective_until > o.created_at)
        ORDER BY h.effective_from DESC
        LIMIT 1
    ) c ON TRUE
    WHERE oo2.cost IS NULL
      AND oo2.org_ingredient_id IS NOT NULL
      AND oo2.quantity_deducted IS NOT NULL
) sub
WHERE sub.id = oo.id;

-- ── 6. Backfill order_item_addons.line_cost ──────────────────────────
-- Approximate: current addon recipe quantities × point-in-time ingredient
-- cost. Swap-type addons (whose deduction replaced the base recipe entry)
-- keep NULL — their cost lives inside the item's recipe cost.
UPDATE order_item_addons oa
SET line_cost = sub.cost_pst
FROM (
    SELECT oa2.id,
           CASE WHEN bool_or(aii.org_ingredient_id IS NULL OR c.cost_per_unit IS NULL)
                THEN NULL
                ELSE round(SUM(aii.quantity_used * c.cost_per_unit)
                           * oa2.quantity * oi.quantity * 100)::bigint
           END AS cost_pst
    FROM order_item_addons oa2
    JOIN order_items oi ON oi.id = oa2.order_item_id
    JOIN orders o       ON o.id = oi.order_id
    JOIN addon_items ai ON ai.id = oa2.addon_item_id
    JOIN addon_item_ingredients aii ON aii.addon_item_id = oa2.addon_item_id
    LEFT JOIN LATERAL (
        SELECT h.cost_per_unit
        FROM ingredient_cost_history h
        WHERE h.org_ingredient_id = aii.org_ingredient_id
          AND h.effective_from <= o.created_at
          AND (h.effective_until IS NULL OR h.effective_until > o.created_at)
        ORDER BY h.effective_from DESC
        LIMIT 1
    ) c ON TRUE
    WHERE oa2.line_cost IS NULL
      AND ai.type NOT IN ('milk_type', 'coffee_type')
    GROUP BY oa2.id, oa2.quantity, oi.quantity
) sub
WHERE sub.id = oa.id;

-- ── 7. Drop dead branch-level price overrides ────────────────────────
DROP TABLE IF EXISTS branch_menu_overrides;
