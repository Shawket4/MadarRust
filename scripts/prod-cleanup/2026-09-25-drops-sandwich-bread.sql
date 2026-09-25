-- ============================================================================
-- DROPS PROD DATA: sandwiches take a REQUIRED Brown / White bread choice
-- (owner, 2026-09-25)
--
-- STATUS: staged on a fresh prod copy; runs on prod only when the owner /
-- orchestrator says go. Deliberately NOT in migrations/. Drops only.
--
-- The owner, verbatim: "Sandwiches need brown bread and white bread instead of
-- bread in the inventory and it needs to be a required selection not an
-- optional."
--
-- STATE BEFORE (prod, 2026-09-25):
--   * "Bread" (pcs, food) is 1 pcs in the recipe of Chicken honey masterd,
--     chicken sweet chili and Turkey — its only uses. Book stock -7 (seven
--     sandwiches sold, nothing ever received or counted).
--   * The shop's inventory ALREADY holds the two breads, from the Foodics
--     stock list: "Brown Ciabatta" / خبز شباتة بني (-5: the retired
--     "brown bread" item's sales) and "White Ciabatta" / خبز شباتة أبيض
--     (never used). This script uses them rather than adding look-alikes.
--   * Each of the three sandwiches carries a private "Options" group holding
--     one OPTIONAL "Brown Bread" / خبز أسمر at +30 EGP, with no recipe (so
--     brown was charged but plain "Bread" was deducted). Never sold.
--
-- WHAT IT DOES (idempotent; a second run changes nothing):
--   1. Makes sure both bread ingredients exist (by name; created as pcs in
--      Bread's category only if missing — on prod they already exist).
--   2. ONE shared group "Bread" / "الخبز": legacy_addon_type 'bread', effect
--      'adds', single, REQUIRED, exactly 1 (min 1, max 1) — built like
--      "Red Bull Type".
--   3. Two options, legacy_source 'addon', no default (the teller must pick,
--      as with Red Bull): White Bread / خبز أبيض +0 (sort 0) and Brown Bread /
--      خبز أسمر +:brown_price piastres (sort 1; default 3000 = today's +30).
--   4. Each option deducts 1 pcs of its bread — the quantity every sandwich
--      used for "Bread" (a guard refuses to run if any sandwich used another).
--   5. Attaches the group to the three sandwiches as a SLOT with both options
--      listed, FIRST (sort 0; the item's other links move down one).
--   6. Retires the old optional: its three options and their private
--      "Options" groups become inactive (rows kept, nothing deleted).
--   7. Removes the plain "Bread" line from the three sandwich recipes — only
--      once the bread group is attached there with both option recipes — so
--      bread is never deducted twice. These are the ONLY deletes (3 recipe
--      lines, listed in the report; the pre-run backup holds them).
--   8. Archives "Bread": is_active = false. The row, its -7 book stock and
--      its 5 sale movements stay, untouched.
--   ceaser salad (also in Sandwiches) takes no bread and is left alone.
--
-- DEVICES: every table touched fires the deferred catalog_revision bump (one
--   per transaction) and the sync_emit changefeed. Tills show the group after
--   Settings -> Sync now; a sandwich then opens its sheet on tap (it has a
--   required group) and cannot be added until a bread is picked.
--
-- HOW TO RUN (as the postgres superuser, on the host):
--   dry run (default, ends in ROLLBACK):
--     sudo -u postgres psql -d madar -X -f 2026-09-25-drops-sandwich-bread.sql
--   apply:   ... -v apply=true
--   brown bread free instead of +30:  ... -v brown_price=0
--   On prod, take a pg_dump -Fc FIRST.
-- ============================================================================

\set ON_ERROR_STOP on
\if :{?apply}
\else
  \set apply false
\endif
\if :{?brown_price}
\else
  \set brown_price 3000
\endif
\set org 27b8f8db-fec2-4909-b9f6-9fffbd860a1a

BEGIN;
SELECT set_config('app.org_id', :'org', true) AS rls_org \gset
SELECT set_config('bread.brown_price', :'brown_price', true) AS brown_price_cfg \gset

-- ── 0. Inputs, pinned by id AND name ───────────────────────────────────────
CREATE TEMP TABLE bread_sandwiches (id uuid PRIMARY KEY, name text NOT NULL) ON COMMIT DROP;
INSERT INTO bread_sandwiches VALUES
  ('cab67e84-eb50-444a-b5d8-f9f35139a269', 'Chicken honey masterd'),
  ('dc21b61c-7f8b-4645-b780-d034a0c300f9', 'chicken sweet chili'),
  ('b4dbbb89-ab3f-4893-be05-0f2e38ab62cd', 'Turkey');

-- The old generic bread.
CREATE TEMP TABLE bread_old (id uuid PRIMARY KEY, name text NOT NULL) ON COMMIT DROP;
INSERT INTO bread_old VALUES ('935e3b1a-f1be-4385-9c88-88112fe0b4f3', 'Bread');

-- The retired optional: option id, its private group, the sandwich.
CREATE TEMP TABLE bread_old_optionals (option_id uuid PRIMARY KEY, group_id uuid NOT NULL,
                                       item_id uuid NOT NULL) ON COMMIT DROP;
INSERT INTO bread_old_optionals VALUES
  ('436f96e3-4ded-4c7e-aa4f-8fd1306fc194', '2dbfe57d-ed3f-4a48-9337-9b55a400a5e9', 'cab67e84-eb50-444a-b5d8-f9f35139a269'),
  ('d55a9cbe-14fe-4394-a626-cc336327fe2e', 'c30e9909-af27-4495-8e7b-e85afeebd585', 'dc21b61c-7f8b-4645-b780-d034a0c300f9'),
  ('d6bdd35b-7292-4825-ac7f-7f3fa0bf1bcf', '4b22696d-2090-47f2-9471-a31e69272e99', 'b4dbbb89-ab3f-4893-be05-0f2e38ab62cd');

-- The two options and the stock row each deducts.
CREATE TEMP TABLE bread_spec (
  name text PRIMARY KEY, ar text NOT NULL, price int NOT NULL, sort int NOT NULL,
  ingredient text NOT NULL, ingredient_ar text NOT NULL
) ON COMMIT DROP;
INSERT INTO bread_spec VALUES
  ('White Bread', 'خبز أبيض', 0, 0, 'White Ciabatta', 'خبز شباتة أبيض'),
  ('Brown Bread', 'خبز أسمر', current_setting('bread.brown_price')::int, 1,
   'Brown Ciabatta', 'خبز شباتة بني');

CREATE TEMP TABLE bread_log (step text PRIMARY KEY, n int NOT NULL) ON COMMIT DROP;
CREATE TEMP TABLE bread_removed_lines ON COMMIT DROP AS
SELECT NULL::text AS item, NULL::text AS ingredient, NULL::numeric AS quantity,
       NULL::text AS unit, NULL::uuid AS recipe_line_id WHERE false;

-- ── 0b. Guards ─────────────────────────────────────────────────────────────
DO $$
DECLARE
  o uuid := current_setting('app.org_id')::uuid;
  r record;
  n int;
BEGIN
  IF NOT EXISTS (SELECT 1 FROM organizations WHERE id = o AND name = 'Drops') THEN
    RAISE EXCEPTION 'guard: org % is not Drops', o;
  END IF;
  FOR r IN SELECT * FROM bread_sandwiches LOOP
    IF NOT EXISTS (SELECT 1 FROM menu_items m WHERE m.id = r.id AND m.org_id = o
                     AND m.name = r.name AND m.deleted_at IS NULL) THEN
      RAISE EXCEPTION 'guard: sandwich % (%) is missing, renamed or deleted', r.name, r.id;
    END IF;
  END LOOP;
  IF NOT EXISTS (SELECT 1 FROM org_ingredients i JOIN bread_old b ON b.id = i.id
                  WHERE i.org_id = o AND i.name = b.name AND i.unit::text = 'pcs') THEN
    RAISE EXCEPTION 'guard: the old Bread ingredient is missing or renamed';
  END IF;
  -- Each option must map to at most one live row, in pcs.
  FOR r IN SELECT * FROM bread_spec LOOP
    SELECT count(*) INTO n FROM org_ingredients i
     WHERE i.org_id = o AND i.name = r.ingredient AND i.deleted_at IS NULL;
    IF n > 1 THEN RAISE EXCEPTION 'guard: % live rows named %', n, r.ingredient; END IF;
    IF EXISTS (SELECT 1 FROM org_ingredients i WHERE i.org_id = o AND i.name = r.ingredient
                 AND i.deleted_at IS NULL AND i.unit::text <> 'pcs') THEN
      RAISE EXCEPTION 'guard: % is not counted in pcs', r.ingredient;
    END IF;
  END LOOP;
  -- One shared quantity is only right if every sandwich used the same.
  IF EXISTS (SELECT 1 FROM recipe_lines rl
               JOIN menu_item_sizes z ON z.id = rl.owner_id AND rl.owner_type = 'item_size'
               JOIN bread_sandwiches s ON s.id = z.menu_item_id
               JOIN bread_old b ON b.id = rl.ingredient_id
              WHERE rl.quantity <> 1 OR rl.unit <> 'pcs') THEN
    RAISE EXCEPTION 'guard: a sandwich uses Bread in a quantity other than 1 pcs; a shared group would deduct the wrong amount';
  END IF;
  -- The private groups being retired hold nothing but the brown optional.
  FOR r IN SELECT * FROM bread_old_optionals LOOP
    IF NOT EXISTS (SELECT 1 FROM modifier_options mo JOIN modifier_groups g ON g.id = mo.group_id
                    WHERE mo.id = r.option_id AND g.id = r.group_id AND g.org_id = o
                      AND mo.name = 'Brown Bread' AND mo.legacy_source = 'optional'
                      AND g.legacy_addon_type IS NULL) THEN
      RAISE EXCEPTION 'guard: optional % is not the Brown Bread optional of group %', r.option_id, r.group_id;
    END IF;
    IF EXISTS (SELECT 1 FROM modifier_options mo WHERE mo.group_id = r.group_id AND mo.id <> r.option_id) THEN
      RAISE EXCEPTION 'guard: private group % holds more than the Brown Bread optional', r.group_id;
    END IF;
  END LOOP;
  SELECT count(*) INTO n FROM modifier_groups WHERE org_id = o AND legacy_addon_type = 'bread';
  IF n > 1 THEN RAISE EXCEPTION 'guard: % groups already carry legacy_addon_type bread', n; END IF;
  IF EXISTS (SELECT 1 FROM modifier_groups WHERE org_id = o AND lower(name) = 'bread'
               AND legacy_addon_type IS DISTINCT FROM 'bread') THEN
    RAISE EXCEPTION 'guard: a group named Bread exists under another family; resolve by hand';
  END IF;
END $$;

-- ── 1. The two bread ingredients (created only if missing) ─────────────────
WITH ins AS (
  INSERT INTO org_ingredients (org_id, name, unit, description, category_id)
  SELECT :'org'::uuid, s.ingredient, 'pcs', s.ingredient_ar, old.category_id
    FROM bread_spec s
    CROSS JOIN (SELECT i.category_id FROM org_ingredients i JOIN bread_old b ON b.id = i.id) old
   WHERE NOT EXISTS (SELECT 1 FROM org_ingredients i WHERE i.org_id = :'org'::uuid
                       AND i.name = s.ingredient AND i.deleted_at IS NULL)
  RETURNING id)
INSERT INTO bread_log SELECT '1 bread ingredients created', count(*) FROM ins;

-- ── 2. The group ───────────────────────────────────────────────────────────
WITH ins AS (
  INSERT INTO modifier_groups (org_id, name, name_translations, selection_type,
                               min_selections, max_selections, is_required, sort,
                               is_active, legacy_addon_type, effect)
  SELECT :'org'::uuid, 'Bread', jsonb_build_object('ar', 'الخبز'), 'single',
         1, 1, true, 0, true, 'bread', 'adds'
   WHERE NOT EXISTS (SELECT 1 FROM modifier_groups
                      WHERE org_id = :'org'::uuid AND legacy_addon_type = 'bread')
  RETURNING id)
INSERT INTO bread_log SELECT '2 group', count(*) FROM ins;

SELECT id AS bread_group FROM modifier_groups
 WHERE org_id = :'org'::uuid AND legacy_addon_type = 'bread' \gset

-- ── 3. The options ─────────────────────────────────────────────────────────
WITH ins AS (
  INSERT INTO modifier_options (group_id, name, name_translations, price, sort,
                                is_default, is_active, legacy_source)
  SELECT :'bread_group'::uuid, s.name, jsonb_build_object('ar', s.ar), s.price, s.sort,
         false, true, 'addon'
    FROM bread_spec s
   WHERE NOT EXISTS (SELECT 1 FROM modifier_options o
                      WHERE o.group_id = :'bread_group'::uuid AND o.name = s.name)
   ORDER BY s.sort
  RETURNING id)
INSERT INTO bread_log SELECT '3 options', count(*) FROM ins;

-- ── 4. Option recipes: 1 pcs of its bread ──────────────────────────────────
WITH ins AS (
  INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit)
  SELECT 'modifier_option', o.id, i.id, 1, 'pcs'
    FROM bread_spec s
    JOIN modifier_options o ON o.group_id = :'bread_group'::uuid AND o.name = s.name
    JOIN org_ingredients i ON i.org_id = :'org'::uuid AND i.name = s.ingredient
                          AND i.deleted_at IS NULL
   WHERE NOT EXISTS (SELECT 1 FROM recipe_lines x
                      WHERE x.owner_type = 'modifier_option' AND x.owner_id = o.id)
  ON CONFLICT DO NOTHING
  RETURNING id)
INSERT INTO bread_log SELECT '4 option recipe lines', count(*) FROM ins;

-- ── 5. Attach FIRST, as a required slot listing both options ───────────────
WITH opts AS (
  SELECT array_agg(o.id ORDER BY o.sort, o.name) AS ids
    FROM modifier_options o
   WHERE o.group_id = :'bread_group'::uuid AND o.is_active AND o.legacy_source = 'addon'),
ins AS (
  INSERT INTO menu_item_modifier_groups (menu_item_id, group_id, sort, legacy_origin,
                                         included_option_ids)
  SELECT s.id, :'bread_group'::uuid, 0, 'slot', opts.ids
    FROM bread_sandwiches s CROSS JOIN opts
  ON CONFLICT (menu_item_id, group_id) DO NOTHING
  RETURNING id)
INSERT INTO bread_log SELECT '5 sandwich links', count(*) FROM ins;

-- Everything else on those items moves below the bread (only rows that are
-- not already after it, so a re-run moves nothing).
WITH upd AS (
  UPDATE menu_item_modifier_groups l
     SET sort = l.sort + 1
    FROM menu_item_modifier_groups b
   WHERE b.menu_item_id = l.menu_item_id AND b.group_id = :'bread_group'::uuid
     AND l.group_id <> :'bread_group'::uuid
     AND l.menu_item_id IN (SELECT id FROM bread_sandwiches)
     AND l.sort <= b.sort
  RETURNING l.id)
INSERT INTO bread_log SELECT '5b other links moved down', count(*) FROM upd;

-- ── 6. Retire the old +30 optional (inactive, nothing deleted) ─────────────
WITH upd AS (
  UPDATE modifier_options mo SET is_active = false, updated_at = now()
    FROM bread_old_optionals x
   WHERE mo.id = x.option_id AND mo.is_active
  RETURNING mo.id)
INSERT INTO bread_log SELECT '6 old optionals deactivated', count(*) FROM upd;
WITH upd AS (
  UPDATE modifier_groups g SET is_active = false, updated_at = now()
   WHERE g.id IN (SELECT group_id FROM bread_old_optionals) AND g.is_active
  RETURNING g.id)
INSERT INTO bread_log SELECT '6b old private groups deactivated', count(*) FROM upd;

-- ── 7. Remove the plain Bread line — only where the group now covers it ────
WITH covered AS (
  SELECT s.id AS item_id
    FROM bread_sandwiches s
    JOIN menu_item_modifier_groups l ON l.menu_item_id = s.id AND l.group_id = :'bread_group'::uuid
   WHERE l.legacy_origin = 'slot'
     AND (SELECT count(*) FROM modifier_options o
           WHERE o.group_id = :'bread_group'::uuid AND o.is_active AND o.id = ANY (l.included_option_ids)
             AND EXISTS (SELECT 1 FROM recipe_lines x
                          WHERE x.owner_type = 'modifier_option' AND x.owner_id = o.id)) = 2),
del AS (
  DELETE FROM recipe_lines rl
   USING menu_item_sizes z, covered c, bread_old b
   WHERE rl.owner_type = 'item_size' AND rl.owner_id = z.id
     AND z.menu_item_id = c.item_id AND rl.ingredient_id = b.id
  RETURNING z.menu_item_id, rl.id, rl.quantity, rl.unit),
kept AS (
  INSERT INTO bread_removed_lines
  SELECT m.name, 'Bread', d.quantity, d.unit, d.id FROM del d JOIN menu_items m ON m.id = d.menu_item_id
  RETURNING 1)
INSERT INTO bread_log SELECT '7 Bread recipe lines removed', count(*) FROM kept;

-- ── 8. Archive the old Bread (only once nothing uses it) ───────────────────
DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM recipe_lines rl JOIN bread_old b ON b.id = rl.ingredient_id) THEN
    RAISE EXCEPTION 'guard: Bread is still in a recipe; not archiving';
  END IF;
END $$;
WITH upd AS (
  UPDATE org_ingredients i SET is_active = false
    FROM bread_old b WHERE i.id = b.id AND i.is_active
  RETURNING i.id)
INSERT INTO bread_log SELECT '8 Bread archived', count(*) FROM upd;

-- ── 9. Verify (any failure rolls everything back) ──────────────────────────
DO $$
DECLARE
  o uuid := current_setting('app.org_id')::uuid;
  g uuid;
  n int;
BEGIN
  SELECT id INTO g FROM modifier_groups
   WHERE org_id = o AND legacy_addon_type = 'bread' AND is_active AND is_required
     AND selection_type = 'single' AND min_selections = 1 AND max_selections = 1;
  IF g IS NULL THEN RAISE EXCEPTION 'verify: no active required 1..1 bread group'; END IF;

  SELECT count(*) INTO n
    FROM bread_spec s
    JOIN modifier_options mo ON mo.group_id = g AND mo.name = s.name AND mo.is_active
                            AND mo.legacy_source = 'addon'
    JOIN addon_item_ingredients a ON a.addon_item_id = mo.id
    JOIN org_ingredients i ON i.id = a.org_ingredient_id
   WHERE i.name = s.ingredient AND i.is_active AND a.quantity_used = 1 AND a.ingredient_unit = 'pcs';
  IF n <> 2 THEN RAISE EXCEPTION 'verify: % correct bread option lines in addon_item_ingredients, want 2', n; END IF;
  SELECT count(*) INTO n FROM recipe_lines rl JOIN modifier_options mo ON mo.id = rl.owner_id
   WHERE rl.owner_type = 'modifier_option' AND mo.group_id = g;
  IF n <> 2 THEN RAISE EXCEPTION 'verify: % option recipe lines, want exactly 2', n; END IF;

  -- a required slot on each sandwich, listing both options, first in order
  SELECT count(*) INTO n
    FROM bread_sandwiches s
    JOIN menu_item_modifier_groups l ON l.menu_item_id = s.id AND l.group_id = g
   WHERE l.legacy_origin = 'slot' AND cardinality(l.included_option_ids) = 2
     AND l.sort < ALL (SELECT x.sort FROM menu_item_modifier_groups x
                        WHERE x.menu_item_id = s.id AND x.group_id <> g);
  IF n <> 3 THEN RAISE EXCEPTION 'verify: % of 3 sandwiches carry the bread slot first', n; END IF;
  SELECT count(*) INTO n FROM menu_item_addon_slots sl JOIN bread_sandwiches s ON s.id = sl.menu_item_id
   WHERE sl.addon_type = 'bread' AND sl.is_required AND sl.min_selections = 1 AND sl.max_selections = 1;
  IF n <> 3 THEN RAISE EXCEPTION 'verify: % required bread slots in menu_item_addon_slots, want 3', n; END IF;

  -- bread is deducted once: no plain Bread left, and the rest of each recipe intact
  IF EXISTS (SELECT 1 FROM recipe_lines rl JOIN bread_old b ON b.id = rl.ingredient_id) THEN
    RAISE EXCEPTION 'verify: a recipe still uses the old Bread';
  END IF;
  SELECT count(*) INTO n
    FROM bread_sandwiches s JOIN menu_item_sizes z ON z.menu_item_id = s.id AND z.is_active
   WHERE (SELECT count(*) FROM recipe_lines x WHERE x.owner_type = 'item_size' AND x.owner_id = z.id) <> 3;
  IF n <> 0 THEN RAISE EXCEPTION 'verify: % sandwich sizes do not have their 3 other lines', n; END IF;

  -- the old optional is gone from every till path
  IF EXISTS (SELECT 1 FROM modifier_options mo JOIN bread_old_optionals x ON x.option_id = mo.id WHERE mo.is_active)
     OR EXISTS (SELECT 1 FROM modifier_groups mg WHERE mg.id IN (SELECT group_id FROM bread_old_optionals) AND mg.is_active) THEN
    RAISE EXCEPTION 'verify: the old optional is still active';
  END IF;
  IF EXISTS (SELECT 1 FROM org_ingredients i JOIN bread_old b ON b.id = i.id WHERE i.is_active) THEN
    RAISE EXCEPTION 'verify: old Bread still active';
  END IF;
END $$;

-- ── Report ─────────────────────────────────────────────────────────────────
\echo
\echo '== Rows changed by this run (a re-run shows 0 everywhere) =='
SELECT step, n FROM bread_log ORDER BY step;

\echo '== Plain Bread lines removed from the sandwich recipes (this run) =='
SELECT item, ingredient, quantity, unit, recipe_line_id FROM bread_removed_lines ORDER BY item;

\echo '== The group =='
SELECT id, name, name_translations->>'ar' AS ar, selection_type, min_selections AS min,
       max_selections AS max, is_required, legacy_addon_type, effect, is_active
  FROM modifier_groups WHERE id = :'bread_group'::uuid;

\echo '== Options and what a sale deducts (addon_item_ingredients) =='
SELECT mo.sort, mo.name, mo.name_translations->>'ar' AS ar, mo.price AS price_piastres,
       mo.is_default, a.ingredient_name, a.quantity_used, a.ingredient_unit
  FROM modifier_options mo
  LEFT JOIN addon_item_ingredients a ON a.addon_item_id = mo.id
 WHERE mo.group_id = :'bread_group'::uuid ORDER BY mo.sort;

\echo '== Sandwiches: groups in order, and the recipe left on the item =='
SELECT m.name, l.sort, g.name AS grp, g.is_active AS grp_active, l.legacy_origin,
       coalesce(l.is_required_override, g.is_required) AS required
  FROM bread_sandwiches s JOIN menu_items m ON m.id = s.id
  JOIN menu_item_modifier_groups l ON l.menu_item_id = m.id
  JOIN modifier_groups g ON g.id = l.group_id
 ORDER BY m.name, l.sort;
SELECT m.name, string_agg(i.name || ' ' || rl.quantity || rl.unit, ', ' ORDER BY i.name) AS item_recipe
  FROM bread_sandwiches s JOIN menu_items m ON m.id = s.id
  JOIN menu_item_sizes z ON z.menu_item_id = m.id AND z.is_active
  JOIN recipe_lines rl ON rl.owner_type = 'item_size' AND rl.owner_id = z.id
  JOIN org_ingredients i ON i.id = rl.ingredient_id
 GROUP BY m.name ORDER BY m.name;

\echo '== Bread stock rows (book stock is never rewritten by this script) =='
SELECT i.name, i.description AS ar, i.unit, i.is_active,
       coalesce(bs.on_hand, 0) AS on_hand,
       (SELECT count(*) FROM inventory_movements mv WHERE mv.org_ingredient_id = i.id) AS movements
  FROM org_ingredients i
  LEFT JOIN branch_stock bs ON bs.org_ingredient_id = i.id
 WHERE i.org_id = :'org'::uuid
   AND (i.id IN (SELECT id FROM bread_old) OR i.name IN (SELECT ingredient FROM bread_spec))
 ORDER BY i.name;

\if :apply
COMMIT;
\echo '== COMMITTED =='
SELECT revision AS catalog_revision, updated_at
  FROM catalog_revision WHERE org_id = :'org'::uuid;
\else
ROLLBACK;
\echo '== DRY RUN: rolled back. Re-run with -v apply=true to commit. =='
\endif
