-- Ingredient cost unit convention flip: EGP → PIASTRES.
--
-- `org_ingredients.cost_per_unit` and `ingredient_cost_history.cost_per_unit`
-- are now defined as PIASTRES (fractional allowed). The dashboard already
-- writes piastres (it multiplies typed EGP ×100 on save), so the catalog and
-- history tables need NO data change — this migration only repairs the order
-- cost snapshots that the old backend computed by multiplying those stored
-- values by 100 a second time (uniform 100× inflation, verified: average
-- unit_cost was 16.5× unit_price before repair).
--
-- Dividing by 100 exactly undoes the old code path, independent of when each
-- catalog row was entered. `deductions_snapshot` JSONB line_cost values are
-- write-only history (no reader) and are left as-is.

UPDATE order_items
SET unit_cost = CASE WHEN unit_cost IS NULL THEN NULL
                     ELSE round(unit_cost / 100.0)::bigint END,
    line_cost = CASE WHEN line_cost IS NULL THEN NULL
                     ELSE round(line_cost / 100.0)::bigint END
WHERE unit_cost IS NOT NULL OR line_cost IS NOT NULL;

UPDATE order_item_addons
SET line_cost = round(line_cost / 100.0)::bigint
WHERE line_cost IS NOT NULL;

UPDATE order_item_optionals
SET cost = round(cost / 100.0)::bigint
WHERE cost IS NOT NULL;

UPDATE order_line_bundle_components
SET line_cost = round(line_cost / 100.0)::bigint
WHERE line_cost IS NOT NULL;
