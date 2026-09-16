-- Menu modeling phases 4-5 (B7-B10), part 1: recipe_lines learns WHERE a line came
-- from and, for option lines, WHICH SIZE it is for.
--
-- source: bases, packaging rules and linked copies are dashboard-only authoring
-- concepts. They are EXPANDED at write time into ordinary recipe_lines rows, so the
-- resolver, the shim views, the changefeed and every old till keep seeing plain
-- lines. `source` only tells the editor (and the next re-expansion) which rows it
-- owns:
--   NULL / 'own'  typed by the owner on this size (every existing row is NULL)
--   'base'        expanded from menu_item_sizes.base_id  (recipe_bases)
--   'rule'        expanded from a packaging_rules match
--   'linked'      copied from menu_items.recipe_source_item_id
--
-- size_label: an OPTION line may carry a size label ("Can" = 40 g, "Cup" = 25 g).
-- The resolver prefers the exact label and falls back to the NULL-size line; the
-- legacy shim views only ever show NULL-size lines, so old tills keep the generic
-- amount. Size rows (owner_type='item_size') never carry one: the owner IS a size.
--
-- SOURCE TABLES: recipe_lines (already emits; a column change needs no trigger).
ALTER TABLE recipe_lines
    ADD COLUMN IF NOT EXISTS source     text NULL,
    ADD COLUMN IF NOT EXISTS size_label text NULL;

ALTER TABLE recipe_lines ADD CONSTRAINT recipe_lines_source_check
    CHECK (source IS NULL OR source IN ('own', 'base', 'rule', 'linked'));
ALTER TABLE recipe_lines ADD CONSTRAINT recipe_lines_size_label_only_options
    CHECK (size_label IS NULL OR (owner_type = 'modifier_option' AND length(size_label) >= 1));

-- One line per (owner, ingredient) per size. NULLS NOT DISTINCT keeps the old
-- guarantee for every NULL-size row (all rows today).
ALTER TABLE recipe_lines DROP CONSTRAINT IF EXISTS recipe_lines_owner_ingredient_key;
CREATE UNIQUE INDEX IF NOT EXISTS recipe_lines_owner_ingredient_size_key
    ON recipe_lines (owner_type, owner_id, ingredient_id, size_label) NULLS NOT DISTINCT;

-- The legacy shim (deploy/menu_unification_shim.sql) is applied by hand where the
-- unified tables are the source of truth. Where it is, re-create the two views that
-- read option lines so a per-size line never leaks into the legacy shapes (an old
-- till would otherwise deduct BOTH the Cup and the Can amount). Where it is not
-- (fresh dev/test databases) these relations are still base tables: nothing to do.
DO $shim$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_class WHERE relname = 'addon_item_ingredients' AND relkind = 'v') THEN
        CREATE OR REPLACE VIEW addon_item_ingredients AS
        SELECT rl.id, rl.owner_id AS addon_item_id, rl.quantity AS quantity_used, rl.created_at, rl.updated_at,
               oi.name AS ingredient_name, rl.unit AS ingredient_unit, rl.ingredient_id AS org_ingredient_id
        FROM recipe_lines rl
        JOIN modifier_options mo ON mo.id = rl.owner_id AND mo.legacy_source = 'addon'
        JOIN org_ingredients oi ON oi.id = rl.ingredient_id
        WHERE rl.owner_type = 'modifier_option' AND rl.size_label IS NULL;
        IF current_setting('server_version_num')::int >= 150000 THEN
            ALTER VIEW addon_item_ingredients SET (security_invoker = true);
        END IF;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_class WHERE relname = 'menu_item_optional_fields' AND relkind = 'v') THEN
        CREATE OR REPLACE VIEW menu_item_optional_fields AS
        SELECT mo.id, mimg.menu_item_id, mo.name, mo.price,
               rl.ingredient_id AS org_ingredient_id, oi.name AS ingredient_name,
               rl.unit AS ingredient_unit, rl.quantity AS quantity_used,
               NULL::text AS size_label, mo.is_active, mo.created_at, mo.updated_at, mo.name_translations
        FROM modifier_options mo
        JOIN menu_item_modifier_groups mimg ON mimg.group_id = mo.group_id AND mimg.legacy_origin = 'options'
        LEFT JOIN recipe_lines rl ON rl.owner_type = 'modifier_option' AND rl.owner_id = mo.id AND rl.size_label IS NULL
        LEFT JOIN org_ingredients oi ON oi.id = rl.ingredient_id
        WHERE mo.legacy_source = 'optional';
        IF current_setting('server_version_num')::int >= 150000 THEN
            ALTER VIEW menu_item_optional_fields SET (security_invoker = true);
        END IF;
    END IF;
END
$shim$;
