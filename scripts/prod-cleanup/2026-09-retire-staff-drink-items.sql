-- ============================================================================
-- PROD DATA CLEANUP: retire the duplicate zero-priced "… staff" menu items
-- (DROPS_SIZES_AND_PACKAGING.md §4c, superseded by the staff drinks pool).
--
-- STATUS: FOR OWNER REVIEW. NOT RUN ANYWHERE. This file is deliberately NOT in
-- migrations/ so a deploy never applies it.
--
-- WHY: a staff drink used to be rung as its own zero-priced twin of the real
-- item ("Latte staff" beside "Latte"). The staff drinks pool replaces that: the
-- real item is rung, at zero, against the branch's daily allowance, with a note.
-- The twins then only split the menu and hide staff consumption inside a second
-- item that has no recipe.
--
-- WHAT IT DOES: it NEVER deletes a row. Each twin is archived, then
--   * soft-deleted (`deleted_at = now()`) and deactivated, so it leaves the
--     menu and the POS grid but every historical `order_items` line that names
--     it keeps working (order_items stores `item_name` and `menu_item_id`, and
--     the FK is not cascaded);
--   * its real counterpart is collected into a suggested eligible-items list
--     for `staff_pool_settings.eligible_item_ids`.
-- Order history, reports and receipts are untouched.
--
-- How to run (after review, on a prod COPY first):
--   createdb -T template0 madar_staffpool_rehearsal
--   pg_restore -d madar_staffpool_rehearsal --no-owner --no-privileges <prod dump>
--   psql -d madar_staffpool_rehearsal -v ON_ERROR_STOP=1 \
--        -v org="'27b8f8db-fec2-4909-b9f6-9fffbd860a1a'" \
--        -f 2026-09-retire-staff-drink-items.sql
--   (read the three reports it prints; it ends in ROLLBACK)
--   On prod: take a pg_dump first, then change the final ROLLBACK to COMMIT.
--
-- :org is the organisation to retire for. Run it one org at a time on purpose.
-- ============================================================================

\set ON_ERROR_STOP on
BEGIN;

CREATE SCHEMA IF NOT EXISTS archive;

-- ── 0. The candidates ────────────────────────────────────────────────────────
-- A twin is: zero base price, not already deleted, and named "… staff" /
-- "… STAFF DRINK" (case-insensitive, trailing). The name test is deliberately
-- anchored at the END so a real item that merely contains the word is safe.
CREATE TEMP TABLE staff_twins ON COMMIT DROP AS
SELECT m.id,
       m.name,
       m.is_active,
       m.category_id,
       regexp_replace(m.name, '[[:space:]]*(staff[[:space:]]+drink|staff)[[:space:]]*$', '', 'i') AS base_name
  FROM menu_items m
 WHERE m.org_id = :org::uuid
   AND m.deleted_at IS NULL
   AND m.base_price = 0
   AND m.name ~* '[[:space:]](staff[[:space:]]+drink|staff)[[:space:]]*$';

-- Its real counterpart, matched by the name with the suffix stripped.
CREATE TEMP TABLE staff_twin_map ON COMMIT DROP AS
SELECT t.id          AS twin_id,
       t.name        AS twin_name,
       t.is_active   AS twin_active,
       r.id          AS real_id,
       r.name        AS real_name,
       r.base_price  AS real_price,
       (SELECT count(*) FROM order_items oi WHERE oi.menu_item_id = t.id)            AS history_lines,
       (SELECT coalesce(sum(oi.quantity),0) FROM order_items oi WHERE oi.menu_item_id = t.id) AS history_qty,
       (SELECT count(*) FROM menu_item_recipes x WHERE x.menu_item_id = t.id)        AS twin_recipe_rows,
       (SELECT count(*) FROM menu_item_recipes x WHERE x.menu_item_id = r.id)        AS real_recipe_rows
  FROM staff_twins t
  LEFT JOIN menu_items r
         ON r.org_id = :org::uuid
        AND r.deleted_at IS NULL
        AND r.id <> t.id
        AND lower(r.name) = lower(t.base_name)
        AND r.name !~* '[[:space:]](staff[[:space:]]+drink|staff)[[:space:]]*$';

-- ── REPORT 1: exactly what would be retired ─────────────────────────────────
\echo ''
\echo '=== REPORT 1: twins that would be retired (soft-deleted + deactivated) ==='
SELECT twin_name, twin_active, history_lines, history_qty, twin_recipe_rows,
       coalesce(real_name, '(NO MATCH — map by hand)') AS real_item,
       real_price, real_recipe_rows
  FROM staff_twin_map
 ORDER BY history_qty DESC, twin_name;

-- ── REPORT 2: the suggested eligible-items list for the new pool ────────────
-- Paste these ids into the branch's / org's staff pool settings. Twins with no
-- match are listed in report 1 and need a hand-picked real item.
\echo ''
\echo '=== REPORT 2: suggested staff_pool_settings.eligible_item_ids ==='
SELECT string_agg(DISTINCT real_id::text, ',' ORDER BY real_id::text) AS eligible_item_ids
  FROM staff_twin_map WHERE real_id IS NOT NULL;

SELECT DISTINCT real_id, real_name FROM staff_twin_map WHERE real_id IS NOT NULL ORDER BY real_name;

-- ── REPORT 3: anything that would be left behind ────────────────────────────
\echo ''
\echo '=== REPORT 3: twins with NO real counterpart (retire only after mapping) ==='
SELECT twin_name, history_lines, history_qty FROM staff_twin_map WHERE real_id IS NULL ORDER BY twin_name;

-- ── 1. Archive, then retire ─────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS archive.menu_items_staff_twins (LIKE menu_items INCLUDING ALL);
ALTER TABLE archive.menu_items_staff_twins ADD COLUMN IF NOT EXISTS archived_at timestamptz DEFAULT now();
ALTER TABLE archive.menu_items_staff_twins ADD COLUMN IF NOT EXISTS retired_for text;

INSERT INTO archive.menu_items_staff_twins
SELECT m.*, now(), 'staff drinks pool (DROPS §4c retired)'
  FROM menu_items m JOIN staff_twin_map t ON t.twin_id = m.id;

-- Sizes and recipes of the twin are archived too, so the whole item can be put
-- back by hand if the owner changes their mind.
CREATE TABLE IF NOT EXISTS archive.menu_item_sizes_staff_twins (LIKE menu_item_sizes INCLUDING ALL);
ALTER TABLE archive.menu_item_sizes_staff_twins ADD COLUMN IF NOT EXISTS archived_at timestamptz DEFAULT now();
INSERT INTO archive.menu_item_sizes_staff_twins
SELECT s.*, now() FROM menu_item_sizes s JOIN staff_twin_map t ON t.twin_id = s.menu_item_id;

CREATE TABLE IF NOT EXISTS archive.menu_item_recipes_staff_twins (LIKE menu_item_recipes INCLUDING ALL);
ALTER TABLE archive.menu_item_recipes_staff_twins ADD COLUMN IF NOT EXISTS archived_at timestamptz DEFAULT now();
INSERT INTO archive.menu_item_recipes_staff_twins
SELECT r.*, now() FROM menu_item_recipes r JOIN staff_twin_map t ON t.twin_id = r.menu_item_id;

-- The retirement itself. NO DELETE: the row stays, soft-deleted and inactive,
-- so historical order lines, receipts and reports keep resolving it.
UPDATE menu_items m
   SET is_active = false,
       deleted_at = now(),
       updated_at = now()
  FROM staff_twin_map t
 WHERE m.id = t.twin_id;

-- A retired twin must never sit in a bundle or a loyalty reward list.
\echo ''
\echo '=== REPORT 4: references that would dangle (resolve before COMMIT) ==='
SELECT 'bundle_component' AS kind, b.id AS ref_id, t.twin_name
  FROM bundle_components b JOIN staff_twin_map t ON t.twin_id = b.item_id
UNION ALL
SELECT 'loyalty_reward_item', l.id, t.twin_name
  FROM loyalty_reward_items l JOIN staff_twin_map t ON t.twin_id = l.menu_item_id;

\echo ''
\echo '=== counts ==='
SELECT (SELECT count(*) FROM staff_twin_map) AS twins_retired,
       (SELECT count(*) FROM staff_twin_map WHERE real_id IS NOT NULL) AS mapped,
       (SELECT count(*) FROM staff_twin_map WHERE real_id IS NULL) AS unmapped,
       (SELECT coalesce(sum(history_qty),0) FROM staff_twin_map) AS historical_units_kept;

-- Change to COMMIT only after the four reports above have been reviewed.
ROLLBACK;
