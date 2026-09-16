-- Seed an org's ingredient catalog from drops_clean/ingredients.csv.
-- Catalog ONLY: no branch_stock rows, no inventory_movements, so book stock is
-- 0 everywhere and the first stocktake sets reality (INVENTORY_V2.md: stock
-- rows are created lazily and on_hand is derived from the ledger).
--
-- Driven by scripts/import-ingredients.sh, which prepends the \copy and :org.

SELECT set_config('app.org_id', :'org', true) AS rls_org \gset

-- ── Categories ────────────────────────────────────────────────────────────
-- `milk` and `coffee_bean` are the two slugs the swap logic keys on
-- (MadarRust src/orders/component_resolve.rs): a milk_type option swaps the
-- drink recipe's `milk` ingredient, a coffee_type option swaps its
-- `coffee_bean` one. The rest are for grouping in the dashboard and the count.
INSERT INTO ingredient_categories (org_id, slug, name, sort_order)
VALUES (:'org'::uuid, 'milk', 'Milk', 0),
       (:'org'::uuid, 'coffee_bean', 'Coffee Bean', 1),
       (:'org'::uuid, 'dairy', 'Dairy', 2),
       (:'org'::uuid, 'syrup', 'Syrup', 3),
       (:'org'::uuid, 'sauce', 'Sauce', 4),
       (:'org'::uuid, 'dry_goods', 'Dry Goods', 5),
       (:'org'::uuid, 'tea', 'Tea', 6),
       (:'org'::uuid, 'beverage', 'Beverage', 7),
       (:'org'::uuid, 'bakery', 'Bakery', 8),
       (:'org'::uuid, 'food', 'Food', 9),
       (:'org'::uuid, 'packaging', 'Packaging', 10),
       (:'org'::uuid, 'merch', 'Merchandise', 11)
ON CONFLICT DO NOTHING;

CREATE TEMP TABLE map_cat AS
SELECT slug, id FROM ingredient_categories WHERE org_id = :'org'::uuid;

-- ── Ingredients ───────────────────────────────────────────────────────────
-- Idempotent on (org, name). The Arabic name goes in `description` —
-- org_ingredients has no translations column — so nothing from the export is
-- lost and a counter who reads Arabic still recognises the row.
CREATE TEMP TABLE new_ing AS
SELECT DISTINCT ON (btrim(s.name))
       btrim(s.name) AS name,
       btrim(s.name_ar) AS name_ar,
       btrim(s.unit)::inventory_unit AS unit,
       c.id AS category_id
FROM stg_ing s
JOIN map_cat c ON c.slug = btrim(s.category)
WHERE nullif(btrim(s.name), '') IS NOT NULL
  AND NOT EXISTS (SELECT 1 FROM org_ingredients e
                  WHERE e.org_id = :'org'::uuid AND e.deleted_at IS NULL
                    AND e.name = btrim(s.name));

INSERT INTO org_ingredients (org_id, name, description, unit, category_id, is_active)
SELECT :'org'::uuid, name, nullif(name_ar, ''), unit, category_id, true
FROM new_ing;

-- ── Swap wiring ───────────────────────────────────────────────────────────
-- A modifier option swaps to an ingredient through a recipe line owned by the
-- option (owner_type 'modifier_option'), which is what `addon_item_ingredients`
-- reads. Quantity is a placeholder of 1 in the ingredient's own unit: the real
-- pour belongs in each drink's recipe, which nobody has authored yet.
CREATE TEMP TABLE swap AS
SELECT btrim(s.swap_key) AS opt_name, e.id AS ingredient_id, e.unit::text AS unit
FROM stg_ing s
JOIN org_ingredients e ON e.org_id = :'org'::uuid AND e.deleted_at IS NULL
                      AND e.name = btrim(s.name)
WHERE nullif(btrim(s.swap_key), '') IS NOT NULL;

INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit)
SELECT 'modifier_option', o.id, w.ingredient_id, 1, w.unit
FROM swap w
JOIN modifier_options o ON o.name = w.opt_name
JOIN modifier_groups g ON g.id = o.group_id
                      AND g.org_id = :'org'::uuid
                      AND g.legacy_addon_type IN ('milk_type', 'coffee_type')
WHERE NOT EXISTS (SELECT 1 FROM recipe_lines r
                  WHERE r.owner_type = 'modifier_option' AND r.owner_id = o.id);

UPDATE modifier_options o
SET replaces_ingredient_id = w.ingredient_id, updated_at = now()
FROM swap w, modifier_groups g
WHERE g.id = o.group_id AND g.org_id = :'org'::uuid
  AND g.legacy_addon_type IN ('milk_type', 'coffee_type')
  AND o.name = w.opt_name
  AND o.replaces_ingredient_id IS DISTINCT FROM w.ingredient_id;

-- ── Report ────────────────────────────────────────────────────────────────
\echo
\echo '== Ingredient import =='
SELECT (SELECT count(*) FROM new_ing)                                              AS inserted,
       (SELECT count(*) FROM stg_ing) - (SELECT count(*) FROM new_ing)             AS already_present,
       (SELECT count(*) FROM org_ingredients WHERE org_id = :'org'::uuid
                                               AND deleted_at IS NULL)             AS catalog_total,
       (SELECT count(*) FROM swap)                                                 AS swap_ingredients;

\echo '-- Options now wired to an ingredient:'
SELECT g.name AS grp, o.name AS option, e.name AS ingredient, e.unit
FROM modifier_options o
JOIN modifier_groups g ON g.id = o.group_id AND g.org_id = :'org'::uuid
JOIN org_ingredients e ON e.id = o.replaces_ingredient_id
ORDER BY g.name, o.name;

\echo '-- Swap keys in the CSV with no matching option (check the spelling):'
SELECT DISTINCT w.opt_name
FROM swap w
WHERE NOT EXISTS (SELECT 1 FROM modifier_options o
                  JOIN modifier_groups g ON g.id = o.group_id
                                        AND g.org_id = :'org'::uuid
                                        AND g.legacy_addon_type IN ('milk_type','coffee_type')
                  WHERE o.name = w.opt_name);

\echo '-- Catalog by category:'
SELECT c.name, count(e.id) AS ingredients
FROM ingredient_categories c
LEFT JOIN org_ingredients e ON e.category_id = c.id AND e.deleted_at IS NULL
WHERE c.org_id = :'org'::uuid
GROUP BY c.name, c.sort_order ORDER BY c.sort_order;

\echo '-- Stock levels: none by design. Book stock is 0 until the first stocktake.'

:final;
