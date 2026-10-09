-- ============================================================================
-- DROPS PROD DATA: a "Cake" add-on group on every drink (owner, 2026-09-25)
--
-- STATUS: staged on a fresh prod copy (drops_cake_stage, 2026-09-25): run 1
-- inserted group 1 / options 2 / option recipe lines 2 / cake item lines 0 /
-- drink links 81 and moved catalog_revision 149 → 150; run 2 inserted 0 and
-- left the revision alone. Runs on prod only when the owner / orchestrator
-- says go. Deliberately NOT in migrations/, so a deploy never applies it.
-- Org-specific: Drops only.
--
-- The owner, verbatim: "We need to add english cake and marble cake to all
-- drinks in the drops catalog, they are the same as their menu items so same
-- recipe copied over, also make sure they have a recipe anyways, the price is
-- 50 EGP for the addon. [...] I want this as a separate group like we did with
-- redbulls etc."
--
-- WHAT IT DOES — built the way "Red Bull Type" is built on prod, except that a
-- cake is optional:
--   1. ONE modifier group "Cake" / "كيك": legacy_addon_type 'cake' (a custom
--      family, like 'red_bull' / 'flavour'), effect 'adds', OPTIONAL
--      (min 0, max 1, single — see the SELECTION note below).
--   2. TWO options, legacy_source 'addon', 5000 piastres (50 EGP) each:
--      English Cake / إنجليش كيك (sort 0), Marble Cake / ماربل كيك (sort 1).
--      The Arabic is the shop's own, from the ingredient rows' descriptions.
--   3. Each option's recipe is COPIED from its menu item's recipe
--      (recipe_lines owner_type 'modifier_option', no size_label) — today
--      1 pcs of the "English Cake" / "Marble Cake" stock row. The sale
--      deducts it additively through the addon_item_ingredients view.
--   4. Every live English / Marble cake MENU ITEM keeps its recipe; any size
--      found without one gets 1 pcs of its cake (a no-op on prod 2026-09-25:
--      all four already carry it).
--   5. Attaches the group to every DRINK: every live item in the eight drink
--      categories below, minus "Ice Cup" (a cup of ice, not a drink). Each
--      link is legacy_origin 'allowlist' with included_option_ids listing both
--      options — a link without them sits behind "show all" on the POS — and
--      takes the item's last sort position (after Extras).
--   Nothing is deleted, merged or renamed. Other orgs are never touched.
--
-- SELECTION: single, 0..1 — a drink takes at most one cake; the teller can
--   leave it empty, and tapping the chosen cake again clears it (POS
--   toggleSingle, for a group that is neither required nor a swap family).
--   The Foodics original was a REQUIRED 1-of-3 with a "بدون كيك" escape
--   hatch; required is gone, so no "none" option is needed. To allow one of
--   each, make it multi with max 2 (not max NULL: that lets the stepper sell
--   3× a cake, and the server does not cap it).
--
-- IDEMPOTENT: every insert is guarded (NOT EXISTS / ON CONFLICT DO NOTHING).
--   A second run inserts 0 rows and ends with the same verification.
--
-- DEVICES: every table touched carries the catalog_revision_bump (deferred,
--   one bump per transaction) and sync_emit triggers. At COMMIT the org's
--   catalog_revision moves by one, and the changefeed re-emits the 81 drinks
--   and the 2 new add-on rows. No backend restart is needed (the only menu
--   cache, MENU_CACHE_TTL_SECS, is off unless set). A till takes the option
--   sheet's groups from /catalog/sync, which it re-reads on Settings → Sync
--   now (or a long-press full sync), so tap Sync now on each till after this.
--
-- IMPORT: like "Red Bull Type", this group is hand-modelled and NOT in the
--   Foodics import (scripts/import-foodics.sql only mirrors the slot/allowlist
--   rule). A plain re-import leaves it alone (the import only inserts what is
--   missing); --replace-menu / --reset-org purge it with every hand-made group
--   and recipe, and the id guards below then refuse to guess — re-derive them.
--
-- HOW TO RUN (as the postgres superuser, on the host):
--   dry run — default, ends in ROLLBACK:
--     sudo -u postgres psql -d madar -X -f 2026-09-25-drops-cake-addon.sql
--   apply — ends in COMMIT:
--     sudo -u postgres psql -d madar -X -v apply=true -f 2026-09-25-drops-cake-addon.sql
--   On prod, take a pg_dump -Fc FIRST.
-- ============================================================================

\set ON_ERROR_STOP on
\if :{?apply}
\else
  \set apply false
\endif
\set org 27b8f8db-fec2-4909-b9f6-9fffbd860a1a

BEGIN;
SELECT set_config('app.org_id', :'org', true) AS rls_org \gset

-- ── 0. Inputs, each pinned by id AND name so a typo can't pick another row ──
CREATE TEMP TABLE cake_drink_categories (id uuid PRIMARY KEY, name text NOT NULL) ON COMMIT DROP;
INSERT INTO cake_drink_categories VALUES
  ('abe7910d-06e6-4a1e-bfc8-e7d2c4e2a81f', 'Hot coffee'),
  ('ed7973a4-7ad7-4965-8ef1-6a2587d1f5b1', 'Iced coffee'),
  ('52dac778-a3b7-41a0-899e-c82e62d16cce', 'Blended coffee'),
  ('72f843e7-7bdf-43d5-9ad5-35617fa249f2', 'Refreshrs'),
  ('d4b8d7af-5e4b-4899-8c3d-fe2e54725d77', 'Hot matcha'),
  ('2d803ba4-e81a-4433-87a9-db69e8348217', 'Iced matcha'),
  ('2c614f6e-54fb-4044-be28-f59162070818', 'Blended matcha'),
  ('8f5455c7-d7c2-42db-8e5e-4a017e698d10', 'Non coffee');

-- Items inside those categories that are NOT drinks.
CREATE TEMP TABLE cake_not_drinks (id uuid PRIMARY KEY, name text NOT NULL) ON COMMIT DROP;
INSERT INTO cake_not_drinks VALUES
  ('ceefac31-fd95-40be-968e-178dd7c48c6b', 'Ice Cup');

-- The two options, and the menu item each copies its recipe from. The two
-- Dessert items are the source; the Bites pair carries the identical recipe
-- (1 pcs of the same stock row), so the choice changes nothing today.
CREATE TEMP TABLE cake_opt_spec (
  name text PRIMARY KEY, ar text NOT NULL, price int NOT NULL, sort int NOT NULL,
  ingredient text NOT NULL, source_item_id uuid NOT NULL, source_item_name text NOT NULL
) ON COMMIT DROP;
INSERT INTO cake_opt_spec VALUES
  ('English Cake', 'إنجليش كيك', 5000, 0, 'English Cake',
   '04d314f3-20dd-4562-9fd0-f0022e7e07f5', 'English Cake'),
  ('Marble Cake',  'ماربل كيك',  5000, 1, 'Marble Cake',
   'dd461664-e509-4266-8886-edfa09108b77', 'Marbel cake');

CREATE TEMP TABLE cake_log (step text PRIMARY KEY, inserted int NOT NULL) ON COMMIT DROP;

-- ── 0b. Guards before any write ────────────────────────────────────────────
DO $$
DECLARE
  o uuid := current_setting('app.org_id')::uuid;
  r record;
  n int;
BEGIN
  IF NOT EXISTS (SELECT 1 FROM organizations WHERE id = o AND name = 'Drops') THEN
    RAISE EXCEPTION 'guard: org % is not Drops', o;
  END IF;

  FOR r IN SELECT * FROM cake_drink_categories LOOP
    IF NOT EXISTS (SELECT 1 FROM categories c WHERE c.id = r.id AND c.org_id = o
                     AND c.name = r.name AND c.deleted_at IS NULL) THEN
      RAISE EXCEPTION 'guard: drink category % (%) is missing, renamed or deleted', r.name, r.id;
    END IF;
  END LOOP;

  FOR r IN SELECT * FROM cake_not_drinks LOOP
    IF NOT EXISTS (SELECT 1 FROM menu_items m WHERE m.id = r.id AND m.org_id = o AND m.name = r.name) THEN
      RAISE EXCEPTION 'guard: excluded item % (%) not found', r.name, r.id;
    END IF;
  END LOOP;

  FOR r IN SELECT * FROM cake_opt_spec LOOP
    SELECT count(*) INTO n FROM org_ingredients i
     WHERE i.org_id = o AND i.name = r.ingredient AND i.deleted_at IS NULL
       AND i.is_active AND i.unit::text = 'pcs';
    IF n <> 1 THEN
      RAISE EXCEPTION 'guard: expected one live pcs ingredient "%", found %', r.ingredient, n;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM menu_items m WHERE m.id = r.source_item_id AND m.org_id = o
                     AND m.name = r.source_item_name AND m.deleted_at IS NULL) THEN
      RAISE EXCEPTION 'guard: recipe source item "%" (%) is missing or deleted',
        r.source_item_name, r.source_item_id;
    END IF;
  END LOOP;

  -- The family key must be ours alone: no second 'cake' group, and no group
  -- already called "Cake" under another family (a dashboard-made one).
  SELECT count(*) INTO n FROM modifier_groups WHERE org_id = o AND legacy_addon_type = 'cake';
  IF n > 1 THEN
    RAISE EXCEPTION 'guard: % groups already carry legacy_addon_type cake', n;
  END IF;
  IF EXISTS (SELECT 1 FROM modifier_groups WHERE org_id = o AND lower(name) = 'cake'
               AND legacy_addon_type IS DISTINCT FROM 'cake') THEN
    RAISE EXCEPTION 'guard: a group named Cake exists under another family; resolve by hand';
  END IF;
END $$;

-- The drinks: live items of the drink categories, minus the exclusions.
CREATE TEMP TABLE cake_drinks ON COMMIT DROP AS
SELECT m.id, m.name, c.name AS category
  FROM menu_items m
  JOIN cake_drink_categories c ON c.id = m.category_id
 WHERE m.org_id = :'org'::uuid
   AND m.deleted_at IS NULL
   AND m.id NOT IN (SELECT id FROM cake_not_drinks);

-- ── 1. The group ───────────────────────────────────────────────────────────
WITH ins AS (
  INSERT INTO modifier_groups (org_id, name, name_translations, selection_type,
                               min_selections, max_selections, is_required, sort,
                               is_active, legacy_addon_type, effect)
  SELECT :'org'::uuid, 'Cake', jsonb_build_object('ar', 'كيك'), 'single',
         0, 1, false, 0, true, 'cake', 'adds'
   WHERE NOT EXISTS (SELECT 1 FROM modifier_groups
                      WHERE org_id = :'org'::uuid AND legacy_addon_type = 'cake')
  RETURNING id)
INSERT INTO cake_log SELECT '1 group', count(*) FROM ins;

SELECT id AS cake_group FROM modifier_groups
 WHERE org_id = :'org'::uuid AND legacy_addon_type = 'cake' \gset

-- ── 2. The options ─────────────────────────────────────────────────────────
WITH ins AS (
  INSERT INTO modifier_options (group_id, name, name_translations, price, sort,
                                is_default, is_active, legacy_source)
  SELECT :'cake_group'::uuid, s.name, jsonb_build_object('ar', s.ar), s.price, s.sort,
         false, true, 'addon'
    FROM cake_opt_spec s
   WHERE NOT EXISTS (SELECT 1 FROM modifier_options o
                      WHERE o.group_id = :'cake_group'::uuid AND o.name = s.name)
   ORDER BY s.sort
  RETURNING id)
INSERT INTO cake_log SELECT '2 options', count(*) FROM ins;

-- ── 3. Option recipes: a copy of the source item's recipe ─────────────────
-- Copied from the item's first active size (the cakes have one, `one_size`);
-- an option already holding any line is left as it is.
WITH src AS (
  SELECT DISTINCT ON (s.name, rl.ingredient_id)
         o.id AS option_id, rl.ingredient_id, rl.quantity, rl.unit
    FROM cake_opt_spec s
    JOIN modifier_options o ON o.group_id = :'cake_group'::uuid AND o.name = s.name
    JOIN LATERAL (SELECT z.id FROM menu_item_sizes z
                   WHERE z.menu_item_id = s.source_item_id AND z.is_active
                   ORDER BY z.sort, z.label LIMIT 1) sz ON true
    JOIN recipe_lines rl ON rl.owner_type = 'item_size' AND rl.owner_id = sz.id
   WHERE NOT EXISTS (SELECT 1 FROM recipe_lines x
                      WHERE x.owner_type = 'modifier_option' AND x.owner_id = o.id)
   ORDER BY s.name, rl.ingredient_id),
ins AS (
  INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit)
  SELECT 'modifier_option', option_id, ingredient_id, quantity, unit FROM src
  ON CONFLICT DO NOTHING
  RETURNING id)
INSERT INTO cake_log SELECT '3 option recipe lines', count(*) FROM ins;

-- ── 4. The cake menu items keep a recipe ("make sure they have a recipe") ──
-- Any live English/Marble cake item size without a line gets 1 pcs of its cake.
WITH need AS (
  SELECT z.id AS size_id, i.id AS ingredient_id
    FROM menu_items m
    JOIN menu_item_sizes z ON z.menu_item_id = m.id AND z.is_active
    JOIN org_ingredients i
      ON i.org_id = m.org_id AND i.deleted_at IS NULL
     AND i.name = CASE WHEN m.name ~* '^\s*english\s+cake' THEN 'English Cake'
                       ELSE 'Marble Cake' END
   WHERE m.org_id = :'org'::uuid AND m.deleted_at IS NULL
     AND m.name ~* '^\s*(english|marble|marbel)\s+cake\s*$'
     AND NOT EXISTS (SELECT 1 FROM recipe_lines x
                      WHERE x.owner_type = 'item_size' AND x.owner_id = z.id)),
ins AS (
  INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit)
  SELECT 'item_size', size_id, ingredient_id, 1, 'pcs' FROM need
  ON CONFLICT DO NOTHING
  RETURNING id)
INSERT INTO cake_log SELECT '4 cake item recipe lines', count(*) FROM ins;

-- ── 5. Attach to every drink, the Red Bull way (explicit origin + list) ────
WITH opts AS (
  SELECT array_agg(o.id ORDER BY o.sort, o.name) AS ids
    FROM modifier_options o
   WHERE o.group_id = :'cake_group'::uuid AND o.is_active AND o.legacy_source = 'addon'),
ins AS (
  INSERT INTO menu_item_modifier_groups (menu_item_id, group_id, sort, legacy_origin,
                                         included_option_ids)
  SELECT d.id, :'cake_group'::uuid,
         coalesce((SELECT max(l.sort) + 1 FROM menu_item_modifier_groups l
                    WHERE l.menu_item_id = d.id), 0),
         'allowlist', opts.ids
    FROM cake_drinks d CROSS JOIN opts
   ORDER BY d.category, d.name
  ON CONFLICT (menu_item_id, group_id) DO NOTHING
  RETURNING id)
INSERT INTO cake_log SELECT '5 drink attachments', count(*) FROM ins;

-- ── 6. Verify (inside the transaction; any failure rolls everything back) ──
DO $$
DECLARE
  o uuid := current_setting('app.org_id')::uuid;
  g uuid;
  n int;
  n_drinks int;
BEGIN
  SELECT id INTO g FROM modifier_groups WHERE org_id = o AND legacy_addon_type = 'cake';
  IF g IS NULL THEN RAISE EXCEPTION 'verify: no cake group'; END IF;

  SELECT count(*) INTO n FROM modifier_options
   WHERE group_id = g AND is_active AND legacy_source = 'addon';
  IF n <> 2 THEN RAISE EXCEPTION 'verify: expected 2 active options, found %', n; END IF;

  -- each option: exactly one line, 1 pcs of its own cake, seen by the view the
  -- sale reads (addon_item_ingredients).
  SELECT count(*) INTO n
    FROM cake_opt_spec s
    JOIN modifier_options mo ON mo.group_id = g AND mo.name = s.name
    JOIN addon_item_ingredients a ON a.addon_item_id = mo.id
    JOIN org_ingredients i ON i.id = a.org_ingredient_id
   WHERE i.name = s.ingredient AND a.quantity_used = 1 AND a.ingredient_unit = 'pcs';
  IF n <> 2 THEN RAISE EXCEPTION 'verify: addon_item_ingredients shows % correct cake lines, want 2', n; END IF;
  SELECT count(*) INTO n FROM recipe_lines rl JOIN modifier_options mo ON mo.id = rl.owner_id
   WHERE rl.owner_type = 'modifier_option' AND mo.group_id = g;
  IF n <> 2 THEN RAISE EXCEPTION 'verify: % option recipe lines, want exactly 2', n; END IF;

  SELECT count(*) INTO n_drinks FROM cake_drinks;
  SELECT count(*) INTO n
    FROM menu_item_modifier_groups l JOIN cake_drinks d ON d.id = l.menu_item_id
   WHERE l.group_id = g AND l.legacy_origin = 'allowlist'
     AND cardinality(l.included_option_ids) = 2
     AND l.included_option_ids @> (SELECT array_agg(id) FROM modifier_options WHERE group_id = g);
  IF n <> n_drinks THEN
    RAISE EXCEPTION 'verify: % of % drinks carry a visible cake link', n, n_drinks;
  END IF;

  -- every cake menu item has a recipe on each active size
  SELECT count(*) INTO n
    FROM menu_items m JOIN menu_item_sizes z ON z.menu_item_id = m.id AND z.is_active
   WHERE m.org_id = o AND m.deleted_at IS NULL
     AND m.name ~* '^\s*(english|marble|marbel)\s+cake\s*$'
     AND NOT EXISTS (SELECT 1 FROM recipe_lines x
                      WHERE x.owner_type = 'item_size' AND x.owner_id = z.id);
  IF n <> 0 THEN RAISE EXCEPTION 'verify: % cake item sizes still have no recipe', n; END IF;
END $$;

-- ── Report ─────────────────────────────────────────────────────────────────
\echo
\echo '== Rows inserted by this run (a re-run shows 0 everywhere) =='
SELECT step, inserted FROM cake_log ORDER BY step;

\echo '== The group =='
SELECT id, name, name_translations->>'ar' AS ar, selection_type, min_selections AS min,
       max_selections AS max, is_required, legacy_addon_type, effect, is_active
  FROM modifier_groups WHERE id = :'cake_group'::uuid;

\echo '== Options and the recipe the sale deducts (addon_item_ingredients) =='
SELECT mo.sort, mo.name, mo.name_translations->>'ar' AS ar, mo.price AS price_piastres,
       a.ingredient_name, a.quantity_used, a.ingredient_unit
  FROM modifier_options mo
  LEFT JOIN addon_item_ingredients a ON a.addon_item_id = mo.id
 WHERE mo.group_id = :'cake_group'::uuid
 ORDER BY mo.sort;

\echo '== Cake menu items and their recipes =='
SELECT m.name, c.name AS category, m.base_price AS price_piastres, z.label AS size,
       i.name AS ingredient, rl.quantity, rl.unit
  FROM menu_items m
  JOIN categories c ON c.id = m.category_id
  JOIN menu_item_sizes z ON z.menu_item_id = m.id AND z.is_active
  LEFT JOIN recipe_lines rl ON rl.owner_type = 'item_size' AND rl.owner_id = z.id
  LEFT JOIN org_ingredients i ON i.id = rl.ingredient_id
 WHERE m.org_id = :'org'::uuid AND m.deleted_at IS NULL
   AND m.name ~* '^\s*(english|marble|marbel)\s+cake\s*$'
 ORDER BY c.name, m.name;

\echo '== Attachments per category =='
SELECT d.category, count(*) AS drinks,
       count(l.id) FILTER (WHERE l.legacy_origin = 'allowlist') AS linked_allowlist
  FROM cake_drinks d
  LEFT JOIN menu_item_modifier_groups l ON l.menu_item_id = d.id AND l.group_id = :'cake_group'::uuid
 GROUP BY d.category ORDER BY d.category;

\echo '== The group on anything that is NOT a counted drink (expect none) =='
SELECT m.name, c.name AS category, l.legacy_origin
  FROM menu_item_modifier_groups l
  JOIN menu_items m ON m.id = l.menu_item_id
  LEFT JOIN categories c ON c.id = m.category_id
 WHERE l.group_id = :'cake_group'::uuid
   AND m.id NOT IN (SELECT id FROM cake_drinks);

\if :apply
COMMIT;
\echo '== COMMITTED =='
SELECT revision AS catalog_revision, updated_at
  FROM catalog_revision WHERE org_id = :'org'::uuid;
\else
ROLLBACK;
\echo '== DRY RUN: rolled back. Re-run with -v apply=true to commit. =='
\endif
