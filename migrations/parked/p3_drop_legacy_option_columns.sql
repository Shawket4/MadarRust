-- PARKED (DB audit 2026-10-16 Phase 3.5, CONTRACT). NOT APPLIED.
--
-- Retires the menu-unification provenance columns:
--   modifier_options.legacy_source, modifier_groups.legacy_addon_type,
--   menu_item_modifier_groups.legacy_origin.
-- Their values are copied to archive first (no data is lost).
--
-- Unpark only when ALL hold:
--   1. The compat shims `legacy_catalog_reads` (src/compat) are gone: no route
--      serves the old addon / optional-field / slot shapes, and the compat views
--      that project these columns are dropped (they depend on them).
--   2. The milk/coffee swap logic keys on modifier_groups.effect /
--      swap_category_id and ingredient category slugs only: no query reads
--      legacy_addon_type / legacy_source / legacy_origin (`cargo sqlx prepare
--      --check` against a database with this file applied proves it), and the
--      catalog pricing vectors (tests/catalog_pricing_tests.rs) are unchanged.
--   3. Offline replays of old sales still resolve: an order line's
--      addon_item_id is a modifier_options id (stable-id invariant), which does
--      not depend on these columns.
--
-- Owner sign-off: ______________________  date: __________

CREATE SCHEMA IF NOT EXISTS archive;
CREATE TABLE IF NOT EXISTS archive.modifier_options_legacy_source AS
    SELECT id, org_id, legacy_source, now() AS archived_at FROM modifier_options WHERE legacy_source IS NOT NULL;
CREATE TABLE IF NOT EXISTS archive.modifier_groups_legacy_addon_type AS
    SELECT id, org_id, legacy_addon_type, now() AS archived_at FROM modifier_groups WHERE legacy_addon_type IS NOT NULL;
CREATE TABLE IF NOT EXISTS archive.mimg_legacy_origin AS
    SELECT id, org_id, legacy_origin, now() AS archived_at FROM menu_item_modifier_groups WHERE legacy_origin IS NOT NULL;
ALTER TABLE menu_item_modifier_groups DROP COLUMN IF EXISTS legacy_origin;
ALTER TABLE modifier_groups DROP COLUMN IF EXISTS legacy_addon_type;
ALTER TABLE modifier_options DROP COLUMN IF EXISTS legacy_source;
