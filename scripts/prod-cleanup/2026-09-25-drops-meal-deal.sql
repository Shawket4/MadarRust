-- DROPS-MEAL-DEAL
-- ============================================================================
-- DROPS PROD DATA: the "Sandwich & Drink" combo (owner, 2026-09-25)
--
-- STATUS: staged on a fresh prod copy (prod-2026-09-25.dump, 19:47 UTC,
-- migrated through 20261005100100_combos_capabilities): the dry run rolled
-- back; run 1 inserted category 1 / combo 1 / anchor 1 / slots 2 / choices 11
-- and moved catalog_revision 153 -> 154; run 2 inserted 0 and left the
-- revision alone. A v1.6.0 backend on that copy showed the combo on the QR
-- menu and cart-quote priced it 255.00 (base), 285.00 (Brown bread) and
-- 285.00 (Iced latte Can). Deliberately NOT in migrations/, so a deploy never
-- applies it. Org-specific: Drops (27b8f8db-fec2-4909-b9f6-9fffbd860a1a).
--
-- The owner, verbatim: "any sandwich including the caesar salads, base white
-- bread of course where it exists; drinks flat white, latte, cappuccino, iced
-- latte, orange juice, strawberry juice, mango juice… all base as well, with
-- full add-ons or size selections adding their extra cost, same for the
-- sandwiches for bread." Price 255 EGP.
--
-- WHAT IT DOES: the rows POST /combos writes (combos::handlers::create_combo),
-- nothing else:
--   1. A category "Combos" / "كومبو" (reused if one already exists), placed
--      after every existing category, so no category moves.
--   2. menu_items: kind 'combo', "Sandwich & Drink" / "ساندوتش ومشروب",
--      base_price 25500, active. The menu_items_ensure_one_size trigger adds
--      its one_size size; its price is set to 25500 (as the handler does).
--   3. menu_item_combos: the anchor.
--   4. Two slots, each pick exactly one (min 1, max 1), no default:
--        "Sandwich" / "ساندوتش": ceaser salad, Chicken honey masterd,
--          chicken sweet chili, Turkey.
--        "Drink" / "مشروب": Flat white, Latte, Cappuccino, Iced latte,
--          Orange juice, Strawberry juce, mango jucie.
--      Every choice: surcharge 0, included_size_label NULL (= the item's
--      cheapest active size), no size-surcharge rows. So (C9) a bigger size
--      costs its usual price difference: Iced latte Cup is included and Can
--      adds 3000 (17500 - 14500). (C10) Add-ons keep their normal prices:
--      the sandwiches' required Bread group is White +0 / Brown +3000.
--   5. No sale window (always available). No combo_channel_settings row, so
--      every channel follows the org default (all on, C3).
--   Nothing is deleted, merged or renamed. Other orgs are never touched.
--
-- IDEMPOTENT: if Drops already has a live combo named "Sandwich & Drink",
--   the script writes nothing (and still verifies). A re-run inserts 0 rows.
--
-- DEVICES: menu_items, categories and the combo tables carry the
--   catalog_revision_bump (deferred, one bump per transaction) and sync_emit
--   triggers, so at COMMIT the org's catalog_revision moves by one and the
--   changefeed emits the new rows. Tills show it after Settings -> Sync now,
--   and only POS 0.9.0+ sells combos (older tills never see it).
--
-- HOW TO RUN (as the postgres superuser, on the host):
--   dry run — default, ends in ROLLBACK:
--     sudo -u postgres psql -d madar -X -f 2026-09-25-drops-meal-deal.sql
--   apply — ends in COMMIT:
--     sudo -u postgres psql -d madar -X -v ON_ERROR_STOP=1 -v apply=true -f 2026-09-25-drops-meal-deal.sql
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
CREATE TEMP TABLE meal_spec (
  slot_sort int NOT NULL, choice_sort int NOT NULL,
  id uuid PRIMARY KEY, name text NOT NULL
) ON COMMIT DROP;
INSERT INTO meal_spec VALUES
  (0, 0, '0f3595ef-836e-4746-a606-1abfa40d9896', 'ceaser salad'),
  (0, 1, 'cab67e84-eb50-444a-b5d8-f9f35139a269', 'Chicken honey masterd'),
  (0, 2, 'dc21b61c-7f8b-4645-b780-d034a0c300f9', 'chicken sweet chili'),
  (0, 3, 'b4dbbb89-ab3f-4893-be05-0f2e38ab62cd', 'Turkey'),
  (1, 0, 'a59144b4-c613-446c-8d94-e2b97e558b28', 'Flat white'),
  (1, 1, '9748f4df-986e-4fda-9a65-3c761af97fb1', 'Latte'),
  (1, 2, '177b8e43-897c-4c4e-8bca-2d3e3938559b', 'Cappuccino'),
  (1, 3, 'f6e7275d-8e76-4ec7-912c-9cf0fd3934da', 'Iced latte'),
  (1, 4, 'ba8fd477-a75b-4328-82d4-aebf9f14a6db', 'Orange juice'),
  (1, 5, 'df6befef-fbe7-42b6-bbe5-1e702e9d4289', 'Strawberry juce'),
  (1, 6, '451fed3b-48b7-4d35-82bd-9c145342087a', 'mango jucie');

CREATE TEMP TABLE meal_slots (sort int PRIMARY KEY, name text NOT NULL, ar text NOT NULL) ON COMMIT DROP;
INSERT INTO meal_slots VALUES (0, 'Sandwich', 'ساندوتش'), (1, 'Drink', 'مشروب');

CREATE TEMP TABLE meal_log (step text PRIMARY KEY, inserted int NOT NULL) ON COMMIT DROP;

-- ── 0b. Guards before any write ────────────────────────────────────────────
DO $$
DECLARE
  o uuid := current_setting('app.org_id')::uuid;
  r record;
BEGIN
  IF NOT EXISTS (SELECT 1 FROM organizations WHERE id = o AND name = 'Drops') THEN
    RAISE EXCEPTION 'guard: org % is not Drops', o;
  END IF;
  -- The combos schema must be there (migration 20261005100000_combos).
  IF to_regclass('public.menu_item_combos') IS NULL THEN
    RAISE EXCEPTION 'guard: the combos migration is not applied (no menu_item_combos)';
  END IF;
  FOR r IN SELECT * FROM meal_spec LOOP
    IF NOT EXISTS (SELECT 1 FROM menu_items m
                    WHERE m.id = r.id AND m.org_id = o AND m.name = r.name
                      AND m.kind = 'item' AND m.deleted_at IS NULL) THEN
      RAISE EXCEPTION 'guard: item % (%) is missing, renamed, deleted, not an item or not Drops''', r.name, r.id;
    END IF;
  END LOOP;
  IF (SELECT count(*) FROM menu_items WHERE org_id = o AND kind = 'combo'
         AND name = 'Sandwich & Drink' AND deleted_at IS NULL) > 1 THEN
    RAISE EXCEPTION 'guard: more than one live "Sandwich & Drink" combo; resolve by hand';
  END IF;
END $$;

SELECT EXISTS (SELECT 1 FROM menu_items WHERE org_id = :'org'::uuid AND kind = 'combo'
                 AND name = 'Sandwich & Drink' AND deleted_at IS NULL) AS already \gset
\if :already
  \echo '== "Sandwich & Drink" already exists in Drops: nothing to write =='
  INSERT INTO meal_log VALUES ('0 skipped (already there)', 0);
\else

-- ── 1. The category ────────────────────────────────────────────────────────
WITH ins AS (
  INSERT INTO categories (org_id, name, name_translations, display_order, is_active)
  SELECT :'org'::uuid, 'Combos', jsonb_build_object('ar', 'كومبو'),
         coalesce((SELECT max(display_order) + 1 FROM categories
                    WHERE org_id = :'org'::uuid AND deleted_at IS NULL), 0),
         true
   WHERE NOT EXISTS (SELECT 1 FROM categories
                      WHERE org_id = :'org'::uuid AND name = 'Combos' AND deleted_at IS NULL)
  RETURNING id)
INSERT INTO meal_log SELECT '1 category', count(*) FROM ins;

SELECT id AS combos_category FROM categories
 WHERE org_id = :'org'::uuid AND name = 'Combos' AND deleted_at IS NULL \gset

-- ── 2. The combo item (create_combo's INSERT, then its one_size price) ─────
WITH ins AS (
  INSERT INTO menu_items (org_id, category_id, name, name_translations, description,
                          description_translations, base_price, is_active, kind)
  VALUES (:'org'::uuid, :'combos_category'::uuid, 'Sandwich & Drink',
          jsonb_build_object('ar', 'ساندوتش ومشروب'), NULL, '{}'::jsonb, 25500, true, 'combo')
  RETURNING id)
INSERT INTO meal_log SELECT '2 combo item', count(*) FROM ins;

SELECT id AS combo FROM menu_items
 WHERE org_id = :'org'::uuid AND kind = 'combo' AND name = 'Sandwich & Drink' AND deleted_at IS NULL \gset

UPDATE menu_item_sizes SET price = 25500 WHERE menu_item_id = :'combo'::uuid AND label = 'one_size';

-- ── 3. The anchor ──────────────────────────────────────────────────────────
WITH ins AS (
  INSERT INTO menu_item_combos (menu_item_id, org_id) VALUES (:'combo'::uuid, :'org'::uuid)
  RETURNING menu_item_id)
INSERT INTO meal_log SELECT '3 anchor', count(*) FROM ins;

-- ── 4. The slots and their choices ─────────────────────────────────────────
WITH ins AS (
  INSERT INTO combo_slots (org_id, combo_item_id, name, name_translations, sort,
                           min_picks, max_picks, default_item_id, default_size_label)
  SELECT :'org'::uuid, :'combo'::uuid, s.name, jsonb_build_object('ar', s.ar), s.sort,
         1, 1, NULL, NULL
    FROM meal_slots s ORDER BY s.sort
  RETURNING id)
INSERT INTO meal_log SELECT '4a slots', count(*) FROM ins;

WITH ins AS (
  INSERT INTO combo_slot_choices (org_id, slot_id, menu_item_id, category_id, surcharge,
                                  included_size_label, sort)
  SELECT :'org'::uuid, cs.id, sp.id, NULL, 0, NULL, sp.choice_sort
    FROM meal_spec sp
    JOIN combo_slots cs ON cs.combo_item_id = :'combo'::uuid AND cs.sort = sp.slot_sort
   ORDER BY sp.slot_sort, sp.choice_sort
  RETURNING id)
INSERT INTO meal_log SELECT '4b choices', count(*) FROM ins;

\endif

-- ── 5. Verify (inside the transaction; any failure rolls everything back) ──
SELECT id AS combo FROM menu_items
 WHERE org_id = :'org'::uuid AND kind = 'combo' AND name = 'Sandwich & Drink' AND deleted_at IS NULL \gset
DO $$
DECLARE
  o uuid := current_setting('app.org_id')::uuid;
  c uuid;
  n int;
BEGIN
  SELECT id INTO c FROM menu_items
   WHERE org_id = o AND kind = 'combo' AND name = 'Sandwich & Drink' AND deleted_at IS NULL;
  IF c IS NULL THEN RAISE EXCEPTION 'verify: no combo'; END IF;
  IF NOT EXISTS (SELECT 1 FROM menu_items WHERE id = c AND base_price = 25500 AND is_active
                   AND name_translations->>'ar' = 'ساندوتش ومشروب') THEN
    RAISE EXCEPTION 'verify: the combo item is not 25500 / active / named in Arabic';
  END IF;
  SELECT count(*) INTO n FROM menu_item_sizes WHERE menu_item_id = c AND is_active;
  IF n <> 1 OR NOT EXISTS (SELECT 1 FROM menu_item_sizes WHERE menu_item_id = c AND label = 'one_size'
                             AND price = 25500 AND is_active) THEN
    RAISE EXCEPTION 'verify: the combo must have exactly its one_size size at 25500 (% active sizes)', n;
  END IF;
  IF NOT EXISTS (SELECT 1 FROM menu_item_combos WHERE menu_item_id = c AND org_id = o) THEN
    RAISE EXCEPTION 'verify: no anchor';
  END IF;
  SELECT count(*) INTO n FROM combo_slots WHERE combo_item_id = c AND min_picks = 1 AND max_picks = 1
     AND default_item_id IS NULL;
  IF n <> 2 OR (SELECT count(*) FROM combo_slots WHERE combo_item_id = c) <> 2 THEN
    RAISE EXCEPTION 'verify: want 2 pick-exactly-one slots, found %', n;
  END IF;
  SELECT count(*) INTO n
    FROM meal_spec sp
    JOIN combo_slots cs ON cs.combo_item_id = c AND cs.sort = sp.slot_sort
    JOIN combo_slot_choices ch ON ch.slot_id = cs.id AND ch.menu_item_id = sp.id
   WHERE ch.surcharge = 0 AND ch.included_size_label IS NULL AND ch.category_id IS NULL;
  IF n <> 11 OR (SELECT count(*) FROM combo_slot_choices ch JOIN combo_slots cs ON cs.id = ch.slot_id
                  WHERE cs.combo_item_id = c) <> 11 THEN
    RAISE EXCEPTION 'verify: want exactly the 11 choices as specified, found % matching', n;
  END IF;
  IF EXISTS (SELECT 1 FROM combo_choice_size_surcharges z JOIN combo_slot_choices ch ON ch.id = z.choice_id
               JOIN combo_slots cs ON cs.id = ch.slot_id WHERE cs.combo_item_id = c) THEN
    RAISE EXCEPTION 'verify: unexpected size-surcharge rows';
  END IF;
  IF EXISTS (SELECT 1 FROM sale_windows WHERE combo_item_id = c) THEN
    RAISE EXCEPTION 'verify: unexpected sale window';
  END IF;
END $$;

-- ── Report ─────────────────────────────────────────────────────────────────
\echo
\echo '== Rows inserted by this run (a re-run shows the skip only) =='
SELECT step, inserted FROM meal_log ORDER BY step;

\echo '== The combo =='
SELECT m.id, m.name, m.name_translations->>'ar' AS ar, m.kind, m.base_price AS price_piastres,
       m.is_active, c.name AS category, c.display_order
  FROM menu_items m JOIN categories c ON c.id = m.category_id
 WHERE m.id = :'combo'::uuid;

\echo '== Slots and choices (base size = included; bigger sizes at their difference) =='
SELECT cs.sort AS slot, cs.name AS slot_name, cs.name_translations->>'ar' AS slot_ar,
       cs.min_picks AS min, cs.max_picks AS max, ch.sort, m.name AS item,
       (SELECT string_agg(z.label || '=' || z.price, ', ' ORDER BY z.price)
          FROM menu_item_sizes z WHERE z.menu_item_id = m.id AND z.is_active) AS sizes
  FROM combo_slots cs
  JOIN combo_slot_choices ch ON ch.slot_id = cs.id
  JOIN menu_items m ON m.id = ch.menu_item_id
 WHERE cs.combo_item_id = :'combo'::uuid
 ORDER BY cs.sort, ch.sort;

\if :apply
COMMIT;
\echo '== COMMITTED =='
SELECT revision AS catalog_revision, updated_at
  FROM catalog_revision WHERE org_id = :'org'::uuid;
\else
ROLLBACK;
\echo '== DRY RUN: rolled back. Re-run with -v apply=true to commit. =='
\endif
