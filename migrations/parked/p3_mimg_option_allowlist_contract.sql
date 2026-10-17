-- PARKED (DB audit 2026-10-16 Phase 3.4, CONTRACT). NOT APPLIED: sqlx reads
-- only the files directly in migrations/.
--
-- Retires menu_item_modifier_groups.included_option_ids, whose content lives in
-- menu_item_modifier_group_options + option_allowlist since 20261019100000.
--
-- Unpark (rename to a timestamp after the newest applied migration, move into
-- migrations/) only when ALL hold:
--   1. scripts/db_phase3_check.sql run READ-ONLY on live prod prints no
--      `mimg_options_*` row (the join table equals the array, per org, both ways),
--      and has for at least one full release.
--   2. The same PR makes every writer write the join table and the flag directly
--      (src/menu/studio.rs inserts/updates and its copy path, src/menu/modifiers.rs,
--      src/menu_unification/backfill.rs) and stops naming the column; the compile-
--      time macros then prove no query reads it (`cargo sqlx prepare --check`).
--   3. compat.menu_item_allowed_addons (and the public view, if still there)
--      reads the join table, not the array.
-- No data is lost: the array is copied to archive first.
--
-- Owner sign-off: ______________________  date: __________

CREATE SCHEMA IF NOT EXISTS archive;
CREATE TABLE IF NOT EXISTS archive.mimg_included_option_ids AS
    SELECT id AS mimg_id, org_id, included_option_ids, now() AS archived_at
      FROM menu_item_modifier_groups WHERE included_option_ids IS NOT NULL;
DROP TRIGGER IF EXISTS mimg_option_allowlist_sync ON menu_item_modifier_groups;
DROP TRIGGER IF EXISTS mimg_option_allowlist_flag ON menu_item_modifier_groups;
DROP FUNCTION IF EXISTS mimg_option_allowlist_sync();
DROP FUNCTION IF EXISTS mimg_option_allowlist_flag();
ALTER TABLE menu_item_modifier_groups DROP COLUMN IF EXISTS included_option_ids;
