-- A drink has one milk (and one coffee).
--
-- `milk_type` / `coffee_type` options are SWAPS: each replaces the recipe's
-- ingredient. The unification backfill made such a group `multi` whenever any
-- legacy slot had no max, and the demo seed put Oat Milk in the multi Extras
-- group, so a till could put two milks on one latte — charged twice, costed as
-- whichever swap ran last. The order path now refuses that line; this makes the
-- menus stop offering it.

-- 1. Existing swap-family groups: single choice, at most one.
UPDATE modifier_groups
   SET selection_type = 'single',
       max_selections = 1,
       min_selections = LEAST(min_selections, 1),
       updated_at     = now()
 WHERE legacy_addon_type IN ('milk_type', 'coffee_type')
   AND (selection_type <> 'single' OR max_selections IS DISTINCT FROM 1 OR min_selections > 1);

-- 2. Milk sitting in a non-swap group (the old demo seed's Extras): move it
--    into its org's milk group when that group exists; create one otherwise.
INSERT INTO modifier_groups (org_id, name, selection_type, min_selections, max_selections,
                             is_required, legacy_addon_type)
SELECT DISTINCT g.org_id, 'Milk', 'single', 0, 1, false, 'milk_type'
  FROM modifier_options o
  JOIN modifier_groups g ON g.id = o.group_id
 WHERE g.legacy_addon_type = 'extras' AND g.name = 'Extras'
   AND o.name = 'Oat Milk'
   AND NOT EXISTS (SELECT 1 FROM modifier_groups m
                    WHERE m.org_id = g.org_id AND m.legacy_addon_type = 'milk_type');

WITH moved AS (
    UPDATE modifier_options o
       SET group_id = (SELECT m.id FROM modifier_groups m
                        WHERE m.org_id = g.org_id AND m.legacy_addon_type = 'milk_type'
                        ORDER BY m.created_at LIMIT 1),
           updated_at = now()
      FROM modifier_groups g
     WHERE g.id = o.group_id
       AND g.legacy_addon_type = 'extras' AND g.name = 'Extras'
       AND o.name = 'Oat Milk'
    RETURNING o.group_id AS milk_group, g.id AS extras_group
)
INSERT INTO menu_item_modifier_groups (menu_item_id, group_id, sort)
SELECT DISTINCT mimg.menu_item_id, moved.milk_group, mimg.sort + 1
  FROM moved
  JOIN menu_item_modifier_groups mimg ON mimg.group_id = moved.extras_group
ON CONFLICT DO NOTHING;

-- 3. And it stays that way, whichever writer inserts or edits a group.
CREATE OR REPLACE FUNCTION modifier_groups_swap_family_single() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.legacy_addon_type IN ('milk_type', 'coffee_type') THEN
        NEW.selection_type := 'single';
        NEW.max_selections := 1;
        NEW.min_selections := LEAST(NEW.min_selections, 1);
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER modifier_groups_swap_family_single
    BEFORE INSERT OR UPDATE ON modifier_groups
    FOR EACH ROW EXECUTE FUNCTION modifier_groups_swap_family_single();
