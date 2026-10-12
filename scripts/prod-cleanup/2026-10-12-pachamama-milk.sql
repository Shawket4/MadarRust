-- ============================================================================
-- PACHA MAMA PROD DATA: milk set up the way Drops has it (owner, 2026-10-12)
--
-- The owner: "we need required selections for milk at least ... you just need
-- to setup milk like it is on other orgs".
--
-- WHY: the menu import made an optional "Milk Alternatives" group (0..1) of the
--   four +45 milks only, so a drink could go out with no milk chosen, and there
--   was no regular milk to pick.
--
-- WHAT IT DOES (idempotent; a second run changes nothing), in place, so every
-- id stays:
--   1. The group becomes "Milk" / الحليب, single, exactly 1, required. It stays
--      legacy_addon_type milk_type, effect swaps, swap category milk.
--   2. Options as Drops names them: Full Cream and Skimmed at 0 (added), then
--      Oat, Almond, Coconut, Soy at +45 (renamed from "Oat Milk" …). None is a
--      default: a required group is never pre-picked on the till.
--   3. Every item link becomes a required slot offering all six, no overrides,
--      the shape of Drops' Milk links.
--   No ingredient links (Pacha Mama runs no inventory); Rue's milk has none either.
--
-- DEVICES: tills pick it up on Settings → Sync now.
--
-- HOW TO RUN: dry run (default, ends in ROLLBACK):
--     psql <prod> -X -f 2026-10-12-pachamama-milk.sql
--   apply: ... -v apply=true   (take a pg_dump -Fc FIRST)
-- ============================================================================
\set ON_ERROR_STOP on
\if :{?apply}
\else
  \set apply false
\endif
\set org 'ff1c5ebe-0d45-4be3-9992-d58bf58448a1'

BEGIN;
SELECT set_config('app.org_id', :'org', true) \gset

SELECT id AS milk FROM modifier_groups
 WHERE org_id = :'org'::uuid AND legacy_addon_type = 'milk_type' AND name IN ('Milk Alternatives', 'Milk') \gset

DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM organizations WHERE id = current_setting('app.org_id')::uuid AND slug = 'pachamama') THEN
    RAISE EXCEPTION 'not Pacha Mama';
  END IF;
  IF (SELECT count(*) FROM modifier_groups WHERE org_id = current_setting('app.org_id')::uuid
        AND legacy_addon_type = 'milk_type') <> 1 THEN
    RAISE EXCEPTION 'expected exactly one milk group';
  END IF;
END $$;

\echo '== BEFORE =='
SELECT name, selection_type, min_selections, max_selections, is_required, effect FROM modifier_groups WHERE id = :'milk'::uuid;
SELECT name, price, sort, is_active FROM modifier_options WHERE group_id = :'milk'::uuid ORDER BY sort;

-- 1. The group: "Milk", exactly one, required.
UPDATE modifier_groups
   SET name = 'Milk', name_translations = '{"ar": "الحليب"}', selection_type = 'single',
       min_selections = 1, max_selections = 1, is_required = true
 WHERE id = :'milk'::uuid
   AND (name <> 'Milk' OR name_translations <> '{"ar": "الحليب"}' OR selection_type <> 'single'
        OR min_selections <> 1 OR max_selections IS DISTINCT FROM 1 OR NOT is_required);

-- 2. The six options, Drops' names and order.
CREATE TEMP TABLE want (old text, name text, ar text, price int, sort int) ON COMMIT DROP;
INSERT INTO want VALUES
  (NULL,           'Full Cream', 'حليب كامل الدسم', 0,    0),
  (NULL,           'Skimmed',    'حليب خالي الدسم', 0,    1),
  ('Oat Milk',     'Oat',        'حليب الشوفان',    4500, 2),
  ('Almond Milk',  'Almond',     'حليب اللوز',      4500, 3),
  ('Coconut Milk', 'Coconut',    'حليب جوز الهند',  4500, 4),
  ('Soy Milk',     'Soy',        'حليب الصويا',     4500, 5);

UPDATE modifier_options o
   SET name = w.name, name_translations = jsonb_build_object('ar', w.ar), sort = w.sort
  FROM want w
 WHERE o.group_id = :'milk'::uuid AND o.name = w.old;

INSERT INTO modifier_options (group_id, name, name_translations, price, sort, is_active)
SELECT :'milk'::uuid, w.name, jsonb_build_object('ar', w.ar), w.price, w.sort, true
  FROM want w
 WHERE NOT EXISTS (SELECT 1 FROM modifier_options o WHERE o.group_id = :'milk'::uuid AND o.name = w.name);

-- 3. Every link: a required slot offering all six.
UPDATE menu_item_modifier_groups l
   SET legacy_origin = 'slot', min_override = NULL, max_override = NULL, is_required_override = NULL,
       included_option_ids = (SELECT array_agg(o.id ORDER BY o.sort) FROM modifier_options o
                               WHERE o.group_id = :'milk'::uuid AND o.is_active)
 WHERE l.group_id = :'milk'::uuid;

-- Verify inside the transaction.
DO $$
DECLARE g record; n int; bad int;
BEGIN
  SELECT * INTO g FROM modifier_groups
   WHERE org_id = current_setting('app.org_id')::uuid AND legacy_addon_type = 'milk_type';
  IF g.name <> 'Milk' OR g.selection_type <> 'single' OR g.min_selections <> 1
     OR g.max_selections IS DISTINCT FROM 1 OR NOT g.is_required THEN
    RAISE EXCEPTION 'verify: the milk group is not single, exactly 1, required';
  END IF;
  SELECT count(*) INTO n FROM modifier_options WHERE group_id = g.id AND is_active;
  IF n <> 6 THEN RAISE EXCEPTION 'verify: % active milk options, want 6', n; END IF;
  SELECT count(*) INTO bad FROM menu_item_modifier_groups
   WHERE group_id = g.id AND (legacy_origin <> 'slot' OR cardinality(included_option_ids) <> 6
         OR min_override IS NOT NULL OR max_override IS NOT NULL OR is_required_override IS NOT NULL);
  IF bad > 0 THEN RAISE EXCEPTION 'verify: % milk links are not a 6-option slot', bad; END IF;
END $$;

\echo '== AFTER =='
SELECT name, name_translations->>'ar' AS ar, selection_type, min_selections, max_selections, is_required, effect,
       (SELECT count(*) FROM menu_item_modifier_groups l WHERE l.group_id = g.id) AS items
  FROM modifier_groups g WHERE id = :'milk'::uuid;
SELECT name, name_translations->>'ar' AS ar, price / 100 AS egp, sort, is_default FROM modifier_options
 WHERE group_id = :'milk'::uuid ORDER BY sort;
SELECT legacy_origin, cardinality(included_option_ids) AS options, count(*) AS links
  FROM menu_item_modifier_groups WHERE group_id = :'milk'::uuid GROUP BY 1, 2;

\if :apply
COMMIT;
\echo 'APPLIED.'
\else
ROLLBACK;
\echo 'DRY RUN: rolled back. Re-run with -v apply=true to apply.'
\endif
