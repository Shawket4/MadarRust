-- B2: explicit "what choosing does" on a modifier group.
--
-- `effect`:
--   none  : the choice touches no stock (a note / a price)
--   adds  : each option deducts its own recipe lines (syrups, extra shot)
--   swaps : each option REPLACES the drink's recipe line whose ingredient is in
--           `swap_category_id` (milk, beans, or any custom family)
--
-- Until now swaps were inferred from `legacy_addon_type` (`milk_type` / `coffee_type`)
-- and the magic category slugs `milk` / `coffee_bean`. The resolver now reads the
-- explicit pair and falls back to that inference when it is absent, so old writers
-- (and old tills, which read `legacy_addon_type` through the shim) keep working.

ALTER TABLE modifier_groups
    ADD COLUMN effect text NOT NULL DEFAULT 'adds',
    ADD COLUMN swap_category_id uuid NULL REFERENCES ingredient_categories(id) ON DELETE SET NULL;

ALTER TABLE modifier_groups
    ADD CONSTRAINT modifier_groups_effect_ck CHECK (effect IN ('none', 'adds', 'swaps')),
    ADD CONSTRAINT modifier_groups_swap_category_ck CHECK (effect = 'swaps' OR swap_category_id IS NULL);

-- ── Backfill ────────────────────────────────────────────────────────────────
-- 1. Swap families: effect swaps, target = the org's milk / coffee_bean category
--    (NULL when the org has no such category: the resolver keeps inferring).
UPDATE modifier_groups g
   SET effect = 'swaps',
       swap_category_id = (SELECT c.id FROM ingredient_categories c
                            WHERE c.org_id = g.org_id
                              AND c.slug = CASE g.legacy_addon_type WHEN 'milk_type' THEN 'milk'
                                                                    ELSE 'coffee_bean' END),
       updated_at = now()
 WHERE g.legacy_addon_type IN ('milk_type', 'coffee_type');

-- 2. Groups none of whose options carries a recipe line deduct nothing.
UPDATE modifier_groups g
   SET effect = 'none', updated_at = now()
 WHERE g.effect = 'adds'
   AND NOT EXISTS (SELECT 1 FROM modifier_options o
                     JOIN recipe_lines rl ON rl.owner_type = 'modifier_option' AND rl.owner_id = o.id
                    WHERE o.group_id = g.id);

-- 3. Everything else stays `adds` (the column default).

-- ── Invariants, whichever writer inserts or edits a group ───────────────────
-- Replaces the body of the trigger function from 20260912130000 (same trigger):
--  * a writer that only knows `legacy_addon_type` (old dashboard, demo seed, SQL)
--    and names a swap family gets `effect = swaps` + the family's category;
--  * the swap category must belong to the group's org;
--  * any swap group (magic type or explicit effect) is single choice, max 1: an
--    explicit `swaps` group with `legacy_addon_type = 'extra'` is accepted and forced
--    single too, since a line can only replace its base ingredient once.
CREATE OR REPLACE FUNCTION modifier_groups_swap_family_single() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.legacy_addon_type IN ('milk_type', 'coffee_type')
       AND NEW.effect <> 'swaps'
       AND (TG_OP = 'INSERT' OR OLD.legacy_addon_type IS DISTINCT FROM NEW.legacy_addon_type) THEN
        NEW.effect := 'swaps';
    END IF;

    IF NEW.effect = 'swaps' AND NEW.swap_category_id IS NULL
       AND NEW.legacy_addon_type IN ('milk_type', 'coffee_type') THEN
        NEW.swap_category_id := (
            SELECT c.id FROM ingredient_categories c
             WHERE c.org_id = NEW.org_id
               AND c.slug = CASE NEW.legacy_addon_type WHEN 'milk_type' THEN 'milk'
                                                       ELSE 'coffee_bean' END);
    END IF;

    IF NEW.effect <> 'swaps' THEN
        NEW.swap_category_id := NULL;
    END IF;

    IF NEW.swap_category_id IS NOT NULL AND NOT EXISTS (
           SELECT 1 FROM ingredient_categories c
            WHERE c.id = NEW.swap_category_id AND c.org_id = NEW.org_id) THEN
        RAISE EXCEPTION 'swap category % is not in the group''s organization', NEW.swap_category_id
            USING ERRCODE = '23503';
    END IF;

    IF NEW.legacy_addon_type IN ('milk_type', 'coffee_type') OR NEW.effect = 'swaps' THEN
        NEW.selection_type := 'single';
        NEW.max_selections := 1;
        NEW.min_selections := LEAST(NEW.min_selections, 1);
    END IF;
    RETURN NEW;
END $$;
