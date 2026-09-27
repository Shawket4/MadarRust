-- ============================================================================
-- DROPS PROD DATA: the "Talabat" group becomes MULTI with a stepper, and never
-- reaches a customer-facing channel (owner, 2026-09-27)
--
-- STATUS: staged on a fresh prod snapshot; runs on prod only when the owner
-- says go. Deliberately NOT in migrations/. Drops only. Follows
-- 2026-09-26-drops-talabat-group.sql (applied 2026-09-26 15:40 UTC).
--
-- The owner, verbatim: "the talabat addon work is done but it made it one
-- selection not a stepper approach It needs to be multi not single."
--
-- WHY IT WAS A PICK: the group was created single, 0..1. The till shows a group
--   as one choice whenever its maximum is 1 (coreGroupIsSingle), so Talabat
--   could only be on or off.
--
-- WHAT IT DOES (idempotent; a second run changes nothing):
--   1. The group becomes selection 'multi', min 0, NO maximum — exactly the
--      shape of "Extras" — so the till draws the option as a chip with a
--      stepper (1×, 2×, …) and prices each unit (+10.00 each). No link
--      carries a min/max/required override (checked; any found is cleared).
--   2. The option is made UNAVAILABLE on every customer channel (in_mall,
--      outside, umbrella, pickup: org-wide 'channel' overrides). Until
--      2026-09-26 no item listed Talabat, so the online menu never showed it;
--      linking it to every item made it customer-visible. The till resolves
--      its menu with no channel, so these overrides never touch it.
--   Nothing is deleted; the option keeps its id and price.
--
-- NOT COVERED BY DATA: the read-only dine-in menu (a table's / branch QR
--   "preview", channel none) is by design exactly the till's menu, so it still
--   lists Talabat for browsing. Hiding it there needs a backend rule (e.g.
--   skip the 'talabat' family in load_public_menu). Drops takes no online
--   orders today (every channel off), so nothing can be ordered with it.
--
-- DEVICES: modifier_groups / menu_price_overrides fire catalog_revision_bump
--   and sync_emit; tills pick it up on Settings → Sync now.
--
-- HOW TO RUN (as the postgres superuser, on the host):
--   dry run (default, ends in ROLLBACK):
--     sudo -u postgres psql -d madar -X -f 2026-09-27-drops-talabat-multi.sql
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

SELECT id AS talabat_group FROM modifier_groups
 WHERE org_id = :'org'::uuid AND legacy_addon_type = 'talabat' \gset
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM organizations WHERE id = current_setting('app.org_id')::uuid AND name = 'Drops') THEN
    RAISE EXCEPTION 'not Drops';
  END IF;
  IF NOT EXISTS (SELECT 1 FROM modifier_options o JOIN modifier_groups g ON g.id = o.group_id
                  WHERE o.id = '4fc98e3a-502b-42b7-9af8-e655d45d013e' AND o.name = 'Talabat'
                    AND g.legacy_addon_type = 'talabat' AND g.org_id = current_setting('app.org_id')::uuid) THEN
    RAISE EXCEPTION 'the Talabat option is not in its own group (run 2026-09-26-drops-talabat-group.sql first)';
  END IF;
END $$;

\echo '== BEFORE =='
SELECT name, selection_type, min_selections, max_selections, is_required FROM modifier_groups
 WHERE id = :'talabat_group'::uuid;
SELECT scope, channel, is_available, price FROM menu_price_overrides WHERE target_id = :'talabat'::uuid;

-- 1. Multi, like Extras.
UPDATE modifier_groups
   SET selection_type = 'multi', min_selections = 0, max_selections = NULL, is_required = false
 WHERE id = :'talabat_group'::uuid
   AND (selection_type <> 'multi' OR min_selections <> 0 OR max_selections IS NOT NULL OR is_required);
UPDATE menu_item_modifier_groups
   SET min_override = NULL, max_override = NULL, is_required_override = NULL
 WHERE group_id = :'talabat_group'::uuid
   AND (min_override IS NOT NULL OR max_override IS NOT NULL OR is_required_override IS NOT NULL);

-- 2. Never on a customer channel.
INSERT INTO menu_price_overrides (scope, channel, target_type, target_id, is_available)
SELECT 'channel', ch::delivery_channel, 'modifier_option', :'talabat'::uuid, false
  FROM unnest(ARRAY['in_mall', 'outside', 'umbrella', 'pickup']) ch
 WHERE NOT EXISTS (SELECT 1 FROM menu_price_overrides p
                    WHERE p.scope = 'channel' AND p.channel = ch::delivery_channel
                      AND p.target_type = 'modifier_option' AND p.target_id = :'talabat'::uuid);
UPDATE menu_price_overrides SET is_available = false
 WHERE scope = 'channel' AND target_type = 'modifier_option' AND target_id = :'talabat'::uuid
   AND is_available IS DISTINCT FROM false;

-- Verify inside the transaction.
DO $$
DECLARE g record; n int;
BEGIN
  SELECT * INTO g FROM modifier_groups
   WHERE org_id = current_setting('app.org_id')::uuid AND legacy_addon_type = 'talabat';
  IF g.selection_type <> 'multi' OR g.max_selections IS NOT NULL OR g.min_selections <> 0 OR g.is_required THEN
    RAISE EXCEPTION 'verify: the Talabat group is not multi 0..unbounded';
  END IF;
  SELECT count(*) INTO n FROM menu_price_overrides
   WHERE scope = 'channel' AND target_type = 'modifier_option'
     AND target_id = '4fc98e3a-502b-42b7-9af8-e655d45d013e' AND is_available = false;
  IF n <> 4 THEN RAISE EXCEPTION 'verify: % of 4 customer channels hide Talabat', n; END IF;
  IF EXISTS (SELECT 1 FROM menu_price_overrides WHERE target_id = '4fc98e3a-502b-42b7-9af8-e655d45d013e'
              AND scope IN ('branch', 'branch_channel')) THEN
    RAISE EXCEPTION 'verify: a branch-level override would reach the till';
  END IF;
END $$;

\echo '== AFTER =='
SELECT name, selection_type, min_selections, max_selections, is_required,
       (SELECT count(*) FROM menu_item_modifier_groups l WHERE l.group_id = g.id) AS items
  FROM modifier_groups g WHERE id = :'talabat_group'::uuid;
SELECT scope, channel, is_available FROM menu_price_overrides WHERE target_id = :'talabat'::uuid ORDER BY channel;

\if :apply
COMMIT;
\echo 'APPLIED.'
\else
ROLLBACK;
\echo 'DRY RUN: rolled back. Re-run with -v apply=true to apply.'
\endif
