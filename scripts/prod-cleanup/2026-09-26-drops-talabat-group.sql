-- ============================================================================
-- DROPS PROD DATA: "Talabat" becomes its own add-on group, on every item
-- (owner, 2026-09-26)
--
-- STATUS: staged on a prod copy; runs on prod only when the owner says go.
-- Deliberately NOT in migrations/. Drops only.
--
-- The owner, verbatim: "I want the talabat addon to be separate from the
-- extras and to be available on all items."
--
-- STATE BEFORE (prod, 2026-09-26): the option "Talabat" / طلبات, +10.00 EGP,
--   no recipe, lives in the shared "Extras" group (multi, legacy 'extra').
--   Extras is linked to 73 items, and NO link lists Talabat among its visible
--   options, so a till only shows it behind "Show all add-ons". Sold twice.
--
-- WHAT IT DOES (idempotent; a second run changes nothing):
--   1. ONE group "Talabat" / "طلبات": legacy_addon_type 'talabat' (its own
--      family, like 'cake' / 'red_bull'), effect 'adds', OPTIONAL, single,
--      0..1 — tap to add, tap again to clear.
--   2. MOVES the existing option row into it (same id, name, price, Arabic):
--      the two sales that carry it keep pointing at it, and the price stays
--      10.00. It leaves Extras; any Extras link that listed it drops it.
--   3. Links the group to EVERY Drops item that is not deleted (active or
--      not, so an item switched back on has it), as legacy_origin
--      'allowlist' with the option listed — so it shows without "Show all" —
--      in the item's last position. Combos are skipped: a combo carries no
--      modifier groups of its own (combo_no_recipe_guard).
--   Nothing is deleted. Other orgs are never touched.
--
-- DEVICES: modifier_groups / modifier_options / menu_item_modifier_groups
--   fire catalog_revision_bump and sync_emit; tills pick it up on Settings →
--   Sync now.
--
-- HOW TO RUN (as the postgres superuser, on the host):
--   dry run (default, ends in ROLLBACK):
--     sudo -u postgres psql -d madar -X -f 2026-09-26-drops-talabat-group.sql
--   apply:   ... -v apply=true
--   On prod, take a pg_dump -Fc FIRST.
-- ============================================================================

\set ON_ERROR_STOP on
\if :{?apply}
\else
  \set apply false
\endif
\set org     '27b8f8db-fec2-4909-b9f6-9fffbd860a1a'
\set talabat '4fc98e3a-502b-42b7-9af8-e655d45d013e'

BEGIN;
SELECT set_config('app.org_id', :'org', true) \gset

DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM organizations WHERE id = current_setting('app.org_id')::uuid AND name = 'Drops') THEN
    RAISE EXCEPTION 'not Drops';
  END IF;
END $$;
-- The option, pinned by id: it must be Drops' "Talabat".
SELECT count(*) AS talabat_option FROM modifier_options o JOIN modifier_groups g ON g.id = o.group_id
 WHERE o.id = :'talabat'::uuid AND o.name = 'Talabat' AND g.org_id = :'org'::uuid \gset
\if :talabat_option
\else
  \echo 'The Talabat option is not where it was on 2026-09-26: nothing done.'
  ROLLBACK;
  \quit
\endif

\echo '== BEFORE =='
SELECT g.name AS "group", o.name, o.price FROM modifier_options o JOIN modifier_groups g ON g.id = o.group_id
 WHERE o.id = :'talabat'::uuid;

-- 1. The group.
INSERT INTO modifier_groups (org_id, name, name_translations, selection_type,
                             min_selections, max_selections, is_required, sort,
                             is_active, legacy_addon_type, effect)
SELECT :'org'::uuid, 'Talabat', jsonb_build_object('ar', 'طلبات'), 'single',
       0, 1, false, 0, true, 'talabat', 'adds'
 WHERE NOT EXISTS (SELECT 1 FROM modifier_groups
                    WHERE org_id = :'org'::uuid AND legacy_addon_type = 'talabat');
SELECT id AS talabat_group FROM modifier_groups
 WHERE org_id = :'org'::uuid AND legacy_addon_type = 'talabat' \gset

-- 2. The option moves (same row, same id).
UPDATE modifier_options SET group_id = :'talabat_group'::uuid, sort = 0, is_default = false
 WHERE id = :'talabat'::uuid AND group_id <> :'talabat_group'::uuid;
UPDATE menu_item_modifier_groups
   SET included_option_ids = array_remove(included_option_ids, :'talabat'::uuid)
 WHERE group_id <> :'talabat_group'::uuid AND :'talabat'::uuid = ANY (included_option_ids);

-- 3. On every item (not combos), visible, last.
INSERT INTO menu_item_modifier_groups (menu_item_id, group_id, sort, legacy_origin, included_option_ids)
SELECT m.id, :'talabat_group'::uuid,
       coalesce((SELECT max(l.sort) + 1 FROM menu_item_modifier_groups l WHERE l.menu_item_id = m.id), 0),
       'allowlist', ARRAY[:'talabat'::uuid]
  FROM menu_items m
 WHERE m.org_id = :'org'::uuid AND m.deleted_at IS NULL AND m.kind = 'item'
ON CONFLICT (menu_item_id, group_id) DO NOTHING;

-- Verify inside the transaction; any failure rolls everything back.
DO $$
DECLARE
  o uuid := current_setting('app.org_id')::uuid;
  g uuid := (SELECT id FROM modifier_groups WHERE org_id = o AND legacy_addon_type = 'talabat');
  n_items int; n_linked int;
BEGIN
  IF (SELECT group_id FROM modifier_options WHERE id = '4fc98e3a-502b-42b7-9af8-e655d45d013e') <> g THEN
    RAISE EXCEPTION 'verify: Talabat is not in its own group';
  END IF;
  IF (SELECT count(*) FROM modifier_options WHERE group_id = g) <> 1 THEN
    RAISE EXCEPTION 'verify: the Talabat group holds more than the one option';
  END IF;
  SELECT count(*) INTO n_items FROM menu_items WHERE org_id = o AND deleted_at IS NULL AND kind = 'item';
  SELECT count(*) INTO n_linked FROM menu_item_modifier_groups l JOIN menu_items m ON m.id = l.menu_item_id
   WHERE l.group_id = g AND m.deleted_at IS NULL AND m.kind = 'item'
     AND '4fc98e3a-502b-42b7-9af8-e655d45d013e'::uuid = ANY (l.included_option_ids);
  IF n_linked <> n_items THEN
    RAISE EXCEPTION 'verify: % of % items carry a visible Talabat link', n_linked, n_items;
  END IF;
  IF EXISTS (SELECT 1 FROM menu_item_modifier_groups l
              WHERE l.group_id <> g AND '4fc98e3a-502b-42b7-9af8-e655d45d013e'::uuid = ANY (l.included_option_ids)) THEN
    RAISE EXCEPTION 'verify: another group still lists Talabat';
  END IF;
END $$;

\echo '== AFTER =='
SELECT g.name AS "group", g.legacy_addon_type, g.selection_type, g.min_selections, g.max_selections,
       o.name, o.price, o.name_translations
  FROM modifier_options o JOIN modifier_groups g ON g.id = o.group_id
 WHERE o.id = :'talabat'::uuid;
SELECT count(*) AS items_with_talabat,
       count(*) FILTER (WHERE m.is_active) AS of_which_active
  FROM menu_item_modifier_groups l JOIN menu_items m ON m.id = l.menu_item_id
 WHERE l.group_id = :'talabat_group'::uuid;
SELECT count(*) AS extras_options_left FROM modifier_options
 WHERE group_id = (SELECT id FROM modifier_groups WHERE org_id = :'org'::uuid AND legacy_addon_type = 'extra');

\if :apply
COMMIT;
\echo 'APPLIED.'
\else
ROLLBACK;
\echo 'DRY RUN: rolled back. Re-run with -v apply=true to apply.'
\endif
