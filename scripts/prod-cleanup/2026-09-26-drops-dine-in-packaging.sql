-- ============================================================================
-- DROPS PROD DATA: dine-in skips ONLY the 4 / 8 / 12 oz cups and their lids
-- (owner, 2026-09-26)
--
-- STATUS: staged on a prod copy; runs on prod only when the owner says go.
-- Deliberately NOT in migrations/. Drops only.
--
-- The owner, verbatim: "on drops for the dine in or packaging that gets
-- skipped, I want only 4, 8, 12 oz cups and lids to be there nothing else."
--
-- HOW DINE-IN WORKS: a dine-in sale skips every recipe line whose ingredient's
-- category is flagged is_packaging, or has the slug 'packaging'
-- (orders/handlers.rs, menu/packaging.rs). The skip is per CATEGORY, so "only
-- these six are skipped" means only these six may sit in such a category.
--
-- STATE BEFORE (the 2026-09-25 prod copy): Drops' one packaging category
-- ('packaging', flagged) holds 16 ingredients: Cup 4oz / 8oz / 12oz, Lid 4oz /
-- 8oz / 12oz / 16oz, Plastic Cup 16oz, Straw, Can, Craft Bag, Coffee Bean Bag,
-- V60 Paper Filter, Coffee Cup Small / Medium / Large. No other Drops category
-- is flagged. Drops has no packaging rules.
--
-- WHAT IT DOES (idempotent; a second run changes nothing):
--   1. Checks the six are in the packaging category (refuses otherwise; names
--      matched case- and space-insensitively).
--   2. Creates the category "Supplies" (slug 'supplies', NOT packaging, sorted
--      right after packaging) if Drops has none.
--   3. Moves every OTHER ingredient out of the packaging category:
--        Coffee Cup Small / Medium / Large -> Merchandise ('merch'): they are
--          the reusable cups sold to customers, not paper cups;
--        everything else (16oz plastic cup + lid, straw, can, bags, V60
--          filter, and anything added since) -> Supplies.
--      Only category_id changes: no recipe, stock, cost or movement is touched.
--   4. Un-flags any other Drops category that is flagged is_packaging (none on
--      the copy), so nothing else is skipped.
--
-- EFFECT: a dine-in drink still deducts its 16oz plastic cup, lid, straw, can
--   and filter; only the 4/8/12oz cups and lids are skipped. Takeaway is
--   unchanged. The dashboard's menu checks will warn about the moved cups,
--   lids and straws (F14: "dine-in orders still deduct it", which is now the
--   intent) and may flag iced sizes (F15), whose cup is no longer 'packaging'.
--
-- DEVICES: org_ingredients / ingredient_categories fire catalog_revision_bump
--   and sync_emit; nothing to do on the tills.
--
-- HOW TO RUN (as the postgres superuser, on the host):
--   dry run (default, ends in ROLLBACK):
--     sudo -u postgres psql -d madar -X -f 2026-09-26-drops-dine-in-packaging.sql
--   apply:   ... -v apply=true
--   On prod, take a pg_dump -Fc FIRST.
-- ============================================================================

\set ON_ERROR_STOP on
\if :{?apply}
\else
  \set apply false
\endif
\set drops '27b8f8db-fec2-4909-b9f6-9fffbd860a1a'

BEGIN;
SELECT set_config('drops.org', :'drops', true);

\echo '== BEFORE: Drops ingredient categories that dine-in skips, with their ingredients =='
SELECT c.slug, c.is_packaging, i.name
  FROM ingredient_categories c
  LEFT JOIN org_ingredients i ON i.category_id = c.id
 WHERE c.org_id = :'drops' AND (c.is_packaging OR c.slug = 'packaging')
 ORDER BY c.slug, i.name;

DO $$
DECLARE
  o        uuid := current_setting('drops.org')::uuid;
  pack     uuid;
  supplies uuid;
  merch    uuid;
  keep     text[] := ARRAY['cup 4oz','cup 8oz','cup 12oz','lid 4oz','lid 8oz','lid 12oz'];
  missing  text;
  n        int;
BEGIN
  IF NOT EXISTS (SELECT 1 FROM organizations WHERE id = o AND name = 'Drops') THEN
    RAISE EXCEPTION 'org % is not Drops', o;
  END IF;

  SELECT id INTO pack FROM ingredient_categories WHERE org_id = o AND slug = 'packaging';
  IF pack IS NULL THEN RAISE EXCEPTION 'Drops has no packaging category'; END IF;

  SELECT string_agg(k, ', ') INTO missing
    FROM unnest(keep) k
   WHERE NOT EXISTS (SELECT 1 FROM org_ingredients i
                      WHERE i.org_id = o AND i.category_id = pack
                        AND regexp_replace(lower(i.name), '\s+', ' ', 'g') = k);
  IF missing IS NOT NULL THEN
    RAISE EXCEPTION 'not in Drops'' packaging category: %', missing;
  END IF;

  -- 2. Supplies: deducted on every sale, dine-in included.
  INSERT INTO ingredient_categories (org_id, slug, name, sort_order, is_packaging)
  SELECT o, 'supplies', 'Supplies',
         (SELECT sort_order FROM ingredient_categories WHERE id = pack), false
  WHERE NOT EXISTS (SELECT 1 FROM ingredient_categories WHERE org_id = o AND slug = 'supplies');
  SELECT id INTO supplies FROM ingredient_categories WHERE org_id = o AND slug = 'supplies';
  IF (SELECT is_packaging FROM ingredient_categories WHERE id = supplies) THEN
    RAISE EXCEPTION 'Drops'' supplies category is flagged as packaging';
  END IF;
  SELECT id INTO merch FROM ingredient_categories
   WHERE org_id = o AND slug = 'merch' AND NOT is_packaging;

  -- 3. The merchandise cups go to Merchandise (when it exists), the rest to Supplies.
  UPDATE org_ingredients i SET category_id = merch
   WHERE merch IS NOT NULL AND i.org_id = o AND i.category_id = pack
     AND regexp_replace(lower(i.name), '\s+', ' ', 'g')
         IN ('coffee cup small', 'coffee cup medium', 'coffee cup large');
  GET DIAGNOSTICS n = ROW_COUNT;
  RAISE NOTICE 'moved to Merchandise: %', n;

  UPDATE org_ingredients i SET category_id = supplies
   WHERE i.org_id = o AND i.category_id = pack
     AND regexp_replace(lower(i.name), '\s+', ' ', 'g') <> ALL (keep);
  GET DIAGNOSTICS n = ROW_COUNT;
  RAISE NOTICE 'moved to Supplies: %', n;

  -- 4. Nothing else skipped on dine-in.
  UPDATE ingredient_categories SET is_packaging = false
   WHERE org_id = o AND id <> pack AND is_packaging;
  GET DIAGNOSTICS n = ROW_COUNT;
  RAISE NOTICE 'other categories un-flagged: %', n;

  -- The guarantee this script exists for.
  SELECT count(*) INTO n
    FROM org_ingredients i JOIN ingredient_categories c ON c.id = i.category_id
   WHERE i.org_id = o AND (c.is_packaging OR c.slug = 'packaging')
     AND regexp_replace(lower(i.name), '\s+', ' ', 'g') <> ALL (keep);
  IF n > 0 THEN RAISE EXCEPTION '% ingredient(s) still skipped on dine-in besides the six', n; END IF;
END $$;

\echo '== AFTER: what dine-in skips (must be exactly the six) =='
SELECT c.slug, i.name
  FROM org_ingredients i JOIN ingredient_categories c ON c.id = i.category_id
 WHERE i.org_id = :'drops' AND (c.is_packaging OR c.slug = 'packaging')
 ORDER BY i.name;
\echo '== AFTER: where the rest went =='
SELECT c.slug, i.name
  FROM org_ingredients i JOIN ingredient_categories c ON c.id = i.category_id
 WHERE i.org_id = :'drops' AND c.slug IN ('supplies', 'merch')
 ORDER BY c.slug, i.name;

\if :apply
COMMIT;
\echo 'APPLIED.'
\else
ROLLBACK;
\echo 'DRY RUN: rolled back. Re-run with -v apply=true to apply.'
\endif
