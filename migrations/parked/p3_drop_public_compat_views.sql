-- PARKED (DB audit 2026-10-16 Phase 3.3, CONTRACT). NOT APPLIED: sqlx reads
-- only the files directly in migrations/.
--
-- Drops the PUBLIC copies of the legacy catalog views. Since 20261019110000
-- the old-shape routes read compat.* and the server's own code reads the
-- unified tables, so nothing current names these. Views hold no rows: no data
-- is lost.
--
-- Unpark (rename to a timestamp after the newest applied migration, move into
-- migrations/) only when ALL hold:
--   1. Phase 3 has been live for one release (no container running an older
--      binary that still reads public.<view> during a rolling deploy).
--   2. `grep -rnwE "(FROM|JOIN)\s+(addon_items|addon_item_ingredients|...)" src`
--      finds only compat.-qualified names (tests/compat_contract_tests.rs
--      enforces it) and `cargo sqlx prepare --check` passes against a database
--      with this file applied.
--   3. sync_source_tables() no longer lists addon_items, addon_item_ingredients
--      and branch_addon_overrides (Phase 6 owns that function; redefine it from
--      its then-current body in the same PR).
--
-- Owner sign-off: ______________________  date: __________

DROP VIEW IF EXISTS public.menu_item_allowed_addons;
DROP VIEW IF EXISTS public.menu_item_addon_slots;
DROP VIEW IF EXISTS public.menu_item_optional_fields;
DROP VIEW IF EXISTS public.branch_channel_addon_overrides;
DROP VIEW IF EXISTS public.branch_channel_menu_overrides;
DROP VIEW IF EXISTS public.branch_addon_overrides;
DROP VIEW IF EXISTS public.branch_menu_size_overrides;
DROP VIEW IF EXISTS public.branch_menu_overrides;
DROP VIEW IF EXISTS public.addon_item_ingredients;
DROP VIEW IF EXISTS public.addon_items;
DROP VIEW IF EXISTS public.menu_item_recipes;
DROP VIEW IF EXISTS public.item_sizes;
