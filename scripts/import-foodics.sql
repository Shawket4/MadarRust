-- Foodics → Madar menu import. Driven by scripts/import-foodics.sh, which
-- prepends the \copy lines that fill the stg_* tables and sets :org / :final.
-- Every step is idempotent: re-running over the same org inserts nothing new.

SELECT set_config('app.org_id', :'org', true) AS rls_org \gset

-- ── Hard wipes (--replace-menu / --reset-org) ─────────────────────────────
-- pg_temp.purge(table, where) deletes the matching rows after first deleting,
-- depth-first, every row in any table whose foreign key points at them —
-- whatever the FK's ON DELETE rule. Nothing is soft-deleted; new tables that
-- reference the menu or the org are picked up without editing this script.
CREATE FUNCTION pg_temp.purge(tbl regclass, cond text, path regclass[] DEFAULT '{}')
RETURNS void LANGUAGE plpgsql AS $fn$
DECLARE
  fk record;
  n bigint;
BEGIN
  FOR fk IN
    SELECT c.conrelid AS child,
           (SELECT string_agg(format('%I', a.attname), ',' ORDER BY k.ord)
              FROM unnest(c.conkey) WITH ORDINALITY k(att, ord)
              JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.att) AS child_cols,
           (SELECT string_agg(format('%I', a.attname), ',' ORDER BY k.ord)
              FROM unnest(c.confkey) WITH ORDINALITY k(att, ord)
              JOIN pg_attribute a ON a.attrelid = c.confrelid AND a.attnum = k.att) AS parent_cols
    FROM pg_constraint c
    WHERE c.contype = 'f' AND c.confrelid = tbl
      AND c.conrelid <> tbl AND NOT c.conrelid = ANY (path)
  LOOP
    PERFORM pg_temp.purge(
      fk.child,
      format('(%s) IN (SELECT %s FROM %s WHERE %s)', fk.child_cols, fk.parent_cols, tbl, cond),
      path || tbl);
  END LOOP;
  EXECUTE format('DELETE FROM %s WHERE %s', tbl, cond);
  GET DIAGNOSTICS n = ROW_COUNT;
  IF n > 0 THEN
    INSERT INTO purge_log AS p (table_name, rows) VALUES (tbl::text, n)
    ON CONFLICT (table_name) DO UPDATE SET rows = p.rows + EXCLUDED.rows;
  END IF;
END
$fn$;
CREATE TEMP TABLE purge_log (table_name text PRIMARY KEY, rows bigint NOT NULL);

-- Append-only history guards (loyalty_transactions, refunds) permit deletes only
-- for demo orgs; treat the org as demo for the wipe and restore the flag after.
CREATE TEMP TABLE keep_org AS SELECT * FROM organizations WHERE id = :'org'::uuid;
\if :reset_org
UPDATE organizations SET is_demo = true WHERE id = :'org'::uuid;
\elif :replace_menu
UPDATE organizations SET is_demo = true WHERE id = :'org'::uuid;
\endif

\if :reset_org
-- Whole org: every row anywhere that hangs off it, then the org row itself,
-- recreated with the same id and settings so --org still points at it.
\o /dev/null
SELECT pg_temp.purge('organizations', format('id = %L', :'org'));
\o
-- The org row's own references (e.g. logo_group_id → asset_groups) pointed at
-- rows that were just purged; blank them so the recreated org is clean.
DO $$
DECLARE col text;
BEGIN
  FOR col IN
    SELECT a.attname FROM pg_constraint c
    JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = ANY (c.conkey)
    WHERE c.contype = 'f' AND c.conrelid = 'organizations'::regclass
  LOOP
    EXECUTE format('UPDATE keep_org SET %I = NULL', col);
  END LOOP;
END $$;
INSERT INTO organizations SELECT * FROM keep_org;
\echo '== Reset org: rows deleted =='
SELECT table_name, rows FROM purge_log ORDER BY rows DESC, table_name;
\elif :replace_menu
-- Menu and everything referencing it (orders, tickets, recipes, overrides …).
\o /dev/null
SELECT pg_temp.purge('modifier_groups', format('org_id = %L', :'org'));
SELECT pg_temp.purge('menu_items', format('org_id = %L', :'org'));
SELECT pg_temp.purge('categories', format('org_id = %L', :'org'));
SELECT CASE WHEN to_regclass('bundles') IS NOT NULL
             AND EXISTS (SELECT 1 FROM information_schema.columns
                         WHERE table_name = 'bundles' AND column_name = 'org_id')
            THEN pg_temp.purge('bundles', format('org_id = %L', :'org')) END;
\o
UPDATE organizations o SET is_demo = k.is_demo FROM keep_org k WHERE o.id = k.id;
\echo '== Replace menu: rows deleted =='
SELECT table_name, rows FROM purge_log ORDER BY rows DESC, table_name;
\endif

-- ── Categories ────────────────────────────────────────────────────────────
-- Foodics image URLs are deliberately not copied into image_url: images are
-- uploaded through Madar's asset pipeline (scripts/upload-menu-images.sh) so
-- nothing keeps pointing at Foodics' S3.
INSERT INTO categories (org_id, name, name_translations)
SELECT DISTINCT ON (btrim(c.name))
       :'org'::uuid, btrim(c.name),
       CASE WHEN nullif(btrim(c.name_localized), '') IS NULL THEN '{}'::jsonb
            ELSE jsonb_build_object('ar', btrim(c.name_localized)) END
FROM stg_categories c
WHERE nullif(btrim(c.name), '') IS NOT NULL
ON CONFLICT (org_id, name) WHERE deleted_at IS NULL DO NOTHING;

-- Items without a category land here (Madar's API requires one).
INSERT INTO categories (org_id, name)
SELECT :'org'::uuid, 'Uncategorized'
WHERE EXISTS (SELECT 1 FROM stg_items i
              WHERE nullif(btrim(i.category_reference), '') IS NULL
                 OR NOT EXISTS (SELECT 1 FROM stg_categories c
                                WHERE c.reference = i.category_reference))
ON CONFLICT (org_id, name) WHERE deleted_at IS NULL DO NOTHING;

CREATE TEMP TABLE map_category AS
SELECT c.reference, k.id
FROM stg_categories c
JOIN categories k ON k.org_id = :'org'::uuid AND k.deleted_at IS NULL
                 AND k.name = btrim(c.name)
WHERE nullif(btrim(c.reference), '') IS NOT NULL;

-- ── Menu items (matched by exact live name within the org) ────────────────
CREATE TEMP TABLE new_items AS
SELECT DISTINCT ON (btrim(i.name))
       btrim(i.name) AS name,
       coalesce(m.id, (SELECT id FROM categories
                       WHERE org_id = :'org'::uuid AND deleted_at IS NULL
                         AND name = 'Uncategorized')) AS category_id,
       CASE WHEN nullif(btrim(i.name_localized), '') IS NULL THEN '{}'::jsonb
            ELSE jsonb_build_object('ar', btrim(i.name_localized)) END AS name_tr,
       nullif(btrim(i.description), '') AS description,
       CASE WHEN nullif(btrim(i.description_localized), '') IS NULL THEN '{}'::jsonb
            ELSE jsonb_build_object('ar', btrim(i.description_localized)) END AS desc_tr,
       round(coalesce(nullif(btrim(i.price), ''), '0')::numeric * 100)::int AS price,
       lower(btrim(i.is_active)) = 'yes' AS is_active
FROM stg_items i
LEFT JOIN map_category m ON m.reference = i.category_reference
WHERE nullif(btrim(i.name), '') IS NOT NULL
  AND NOT EXISTS (SELECT 1 FROM menu_items e
                  WHERE e.org_id = :'org'::uuid AND e.deleted_at IS NULL
                    AND e.name = btrim(i.name));

INSERT INTO menu_items (org_id, category_id, name, name_translations, description,
                        description_translations, base_price, is_active)
SELECT :'org'::uuid, category_id, name, name_tr, description, desc_tr, price, is_active
FROM new_items;

-- Every item carries at least one size row. It is NOT cosmetic: the dashboard's
-- recipe builder runs one column PER SIZE, so an item with no sizes has nowhere
-- to hang its ingredients. The POS hides a lone `one_size` rather than drawing
-- a one-option size chip.
INSERT INTO menu_item_sizes (menu_item_id, label, price)
SELECT e.id, 'one_size', e.base_price
FROM menu_items e
WHERE e.org_id = :'org'::uuid AND e.deleted_at IS NULL
  AND e.name IN (SELECT btrim(name) FROM stg_items)
  AND NOT EXISTS (SELECT 1 FROM menu_item_sizes s WHERE s.menu_item_id = e.id);

CREATE TEMP TABLE map_item AS
SELECT DISTINCT ON (i.sku) i.sku, e.id
FROM stg_items i
JOIN menu_items e ON e.org_id = :'org'::uuid AND e.deleted_at IS NULL
                 AND e.name = btrim(i.name)
WHERE nullif(btrim(i.sku), '') IS NOT NULL
ORDER BY i.sku, e.created_at;

-- ── Modifier groups (one per distinct name; defaults = most common limits) ─
-- A group is keyed by its `reference` when the export sets one (the cleaned
-- drops do), else by its name — which is all the raw Foodics exports carry.
CREATE TEMP TABLE link AS
SELECT coalesce(nullif(btrim(a.modifier_reference), ''), btrim(a.modifier_name)) AS gkey,
       btrim(a.modifier_name) AS gname,
       nullif(btrim(a.modifier_name_localized), '') AS gname_ar,
       a.product_sku AS sku,
       coalesce(nullif(btrim(a.minimum_options), ''), '0')::int AS mn,
       nullif(btrim(a.maximum_options), '')::int AS mx,
       row_number() OVER (PARTITION BY a.product_sku ORDER BY a.ordinal) - 1 AS sort
FROM stg_addons a
WHERE nullif(btrim(a.modifier_name), '') IS NOT NULL;

-- modifier_groups.csv is the group catalog (incl. groups no product uses yet);
-- product_modifiers.csv supplies the limits. Unlinked groups default to optional multi.
CREATE TEMP TABLE group_default AS
SELECT DISTINCT ON (gkey) gkey, gname, gname_ar, mn, mx
FROM (SELECT l.gkey,
             coalesce(max(nullif(btrim(g.name), '')) OVER (PARTITION BY l.gkey),
                      max(l.gname) OVER (PARTITION BY l.gkey)) AS gname,
             coalesce(max(nullif(btrim(g.name_localized), '')) OVER (PARTITION BY l.gkey),
                      max(l.gname_ar) OVER (PARTITION BY l.gkey)) AS gname_ar,
             coalesce(l.mn, 0) AS mn, l.mx,
             count(l.sku) OVER (PARTITION BY l.gkey, l.mn, l.mx) AS n
      FROM (SELECT gkey, gname, gname_ar, sku, mn, mx FROM link
            UNION ALL
            SELECT coalesce(nullif(btrim(reference), ''), btrim(name)),
                   btrim(name), NULL, NULL, NULL, NULL FROM stg_groups
            WHERE nullif(btrim(name), '') IS NOT NULL) l
      LEFT JOIN stg_groups g
        ON coalesce(nullif(btrim(g.reference), ''), btrim(g.name)) = l.gkey) t
ORDER BY gkey, n DESC, mn, mx;

-- `size` is not a modifier group: Madar models sizes as menu_item_sizes, with
-- their own price, rendered as chips above the option cards. Its options become
-- the linked items' sizes further down, so it never reaches modifier_groups.
CREATE TEMP TABLE size_key AS
SELECT gkey FROM group_default WHERE gkey = 'size';
DELETE FROM group_default WHERE gkey IN (SELECT gkey FROM size_key);

-- legacy_addon_type must be set: options default to legacy_source='addon', which
-- surfaces them through the addon_items view, and GET /addon-items fails on a
-- NULL type. Milk / bean groups get the swap-family types, the rest 'extra'
-- (the DB trigger forces milk_type/coffee_type groups to single-select).
-- The family drives the POS sheet order (milk → coffee type → extras) and the
-- DB trigger that forces milk/coffee groups to single-select. A keyed export
-- states it outright; a raw Foodics export is guessed from the name.
INSERT INTO modifier_groups (org_id, name, name_translations, selection_type,
                             min_selections, max_selections, is_required, legacy_addon_type)
SELECT :'org'::uuid, d.gname,
       CASE WHEN d.gname_ar IS NULL THEN '{}'::jsonb ELSE jsonb_build_object('ar', d.gname_ar) END,
       CASE WHEN d.mx = 1 THEN 'single' ELSE 'multi' END,
       d.mn, d.mx, d.mn > 0,
       CASE WHEN d.gkey IN ('milk_type', 'coffee_type', 'extra') THEN d.gkey
            WHEN d.gname ~* 'milk' THEN 'milk_type'
            WHEN d.gname ~* '(bean|حبوب)' THEN 'coffee_type'
            ELSE 'extra' END
FROM group_default d
WHERE NOT EXISTS (SELECT 1 FROM modifier_groups g
                  WHERE g.org_id = :'org'::uuid AND g.name = d.gname);

CREATE TEMP TABLE map_group AS
SELECT DISTINCT ON (d.gkey) d.gkey, g.name, g.id, g.min_selections, g.max_selections
FROM group_default d
JOIN modifier_groups g ON g.org_id = :'org'::uuid AND g.name = d.gname
ORDER BY d.gkey, g.created_at;

-- ── Modifier options (matched to groups by name; idempotent by group + name) ─
CREATE TEMP TABLE new_options AS
SELECT DISTINCT ON (mg.id, btrim(o.name))
       mg.id AS group_id, btrim(o.name) AS name,
       CASE WHEN nullif(btrim(o.name_localized), '') IS NULL THEN '{}'::jsonb
            ELSE jsonb_build_object('ar', btrim(o.name_localized)) END AS name_tr,
       round(coalesce(nullif(btrim(o.price), ''), '0')::numeric * 100)::int AS price,
       coalesce(lower(btrim(o.is_active)), 'yes') = 'yes' AS is_active,
       o.ordinal
FROM stg_options o
JOIN map_group mg
  ON mg.gkey = coalesce(nullif(btrim(o.modifier_reference), ''), btrim(o.modifier_name))
WHERE nullif(btrim(o.name), '') IS NOT NULL
  AND NOT EXISTS (SELECT 1 FROM modifier_options x
                  WHERE x.group_id = mg.id AND x.name = btrim(o.name))
ORDER BY mg.id, btrim(o.name), o.ordinal;

INSERT INTO modifier_options (group_id, name, name_translations, price, is_active, sort)
SELECT group_id, name, name_tr, price, is_active,
       row_number() OVER (PARTITION BY group_id ORDER BY ordinal) - 1
FROM new_options;

INSERT INTO menu_item_modifier_groups (menu_item_id, group_id, sort, min_override,
                                       max_override, is_required_override)
SELECT DISTINCT ON (mi.id, mg.id) mi.id, mg.id, l.sort,
       CASE WHEN l.mn IS DISTINCT FROM mg.min_selections THEN l.mn END,
       CASE WHEN l.mx IS DISTINCT FROM mg.max_selections THEN l.mx END,
       CASE WHEN l.mn IS DISTINCT FROM mg.min_selections THEN l.mn > 0 END
FROM link l
JOIN map_item mi ON mi.sku = l.sku
JOIN map_group mg ON mg.gkey = l.gkey
ON CONFLICT (menu_item_id, group_id) DO NOTHING;

-- ── Sizes from the `size` family ──────────────────────────────────────────
-- Each linked item gets one row per size option, priced base + the option's
-- price, replacing the single 'one_size' row seeded above.
CREATE TEMP TABLE size_opt AS
SELECT btrim(o.name) AS label,
       round(coalesce(nullif(btrim(o.price), ''), '0')::numeric * 100)::int AS add_price,
       o.ordinal
FROM stg_options o
WHERE coalesce(nullif(btrim(o.modifier_reference), ''), btrim(o.modifier_name))
      IN (SELECT gkey FROM size_key)
  AND nullif(btrim(o.name), '') IS NOT NULL;

DELETE FROM menu_item_sizes s
WHERE s.label = 'one_size'
  AND EXISTS (SELECT 1 FROM size_opt)
  AND s.menu_item_id IN (SELECT mi.id FROM link l JOIN map_item mi ON mi.sku = l.sku
                         WHERE l.gkey IN (SELECT gkey FROM size_key));

INSERT INTO menu_item_sizes (menu_item_id, label, price)
SELECT DISTINCT mi.id, so.label, e.base_price + so.add_price
FROM link l
JOIN map_item mi ON mi.sku = l.sku
JOIN menu_items e ON e.id = mi.id
CROSS JOIN size_opt so
WHERE l.gkey IN (SELECT gkey FROM size_key)
  AND NOT EXISTS (SELECT 1 FROM menu_item_sizes x
                  WHERE x.menu_item_id = mi.id AND x.label = so.label);

-- ── Item-private optionals ────────────────────────────────────────────────
-- `menu_item_optional_fields` is a VIEW (not insertable): an optional is a
-- per-item group named "Options" with legacy_addon_type NULL, linked with
-- legacy_origin = 'options', holding modifier_options with
-- legacy_source = 'optional'. Same shape the matcha drinks already use for
-- Honey / Condensed Milk / Vanilla Syrup.
CREATE TEMP TABLE optional_item AS
SELECT DISTINCT mi.id AS menu_item_id
FROM stg_optionals t
JOIN map_item mi ON mi.sku = btrim(t.product_sku)
WHERE nullif(btrim(t.name), '') IS NOT NULL;

-- One "Options" group per item, created only where the item has none yet.
CREATE TEMP TABLE optional_group (menu_item_id uuid PRIMARY KEY, group_id uuid NOT NULL);

DO $$
DECLARE
  it uuid;
  gid uuid;
  nxt int;
BEGIN
  FOR it IN SELECT menu_item_id FROM optional_item LOOP
    SELECT l.group_id INTO gid
      FROM menu_item_modifier_groups l
      JOIN modifier_groups g ON g.id = l.group_id
                            AND g.org_id = current_setting('app.org_id')::uuid
     WHERE l.menu_item_id = it AND l.legacy_origin = 'options'
     LIMIT 1;
    IF gid IS NULL THEN
      INSERT INTO modifier_groups (org_id, name, selection_type, min_selections,
                                   max_selections, is_required, legacy_addon_type)
      VALUES (current_setting('app.org_id')::uuid, 'Options', 'multi', 0, NULL, false, NULL)
      RETURNING id INTO gid;
      SELECT coalesce(max(sort) + 1, 0) INTO nxt
        FROM menu_item_modifier_groups WHERE menu_item_id = it;
      INSERT INTO menu_item_modifier_groups (menu_item_id, group_id, sort, legacy_origin)
      VALUES (it, gid, nxt, 'options');
    END IF;
    INSERT INTO optional_group VALUES (it, gid);
  END LOOP;
END $$;

INSERT INTO modifier_options (group_id, name, name_translations, price, is_active,
                              sort, legacy_source)
SELECT DISTINCT ON (p.group_id, btrim(t.name))
       p.group_id, btrim(t.name),
       CASE WHEN nullif(btrim(t.name_localized), '') IS NULL THEN '{}'::jsonb
            ELSE jsonb_build_object('ar', btrim(t.name_localized)) END,
       round(coalesce(nullif(btrim(t.price), ''), '0')::numeric * 100)::int, true, 0,
       'optional'
FROM stg_optionals t
JOIN map_item mi ON mi.sku = btrim(t.product_sku)
JOIN optional_group p ON p.menu_item_id = mi.id
WHERE nullif(btrim(t.name), '') IS NOT NULL
  AND NOT EXISTS (SELECT 1 FROM modifier_options x
                  WHERE x.group_id = p.group_id AND x.name = btrim(t.name));

-- ── Report ────────────────────────────────────────────────────────────────
\echo
\echo '== Import summary =='
SELECT (SELECT count(*) FROM categories WHERE org_id = :'org'::uuid AND deleted_at IS NULL) AS categories_in_org,
       (SELECT count(*) FROM new_items)                                                   AS items_inserted,
       (SELECT count(*) FROM stg_items) - (SELECT count(*) FROM new_items)                AS items_already_present,
       (SELECT count(*) FROM map_group)                                                   AS modifier_groups,
       (SELECT count(*) FROM new_options)                                                 AS options_inserted,
       (SELECT count(*) FROM size_opt)                                                    AS size_labels,
       (SELECT count(*) FROM menu_item_modifier_groups x JOIN map_item mi ON mi.id = x.menu_item_id) AS item_group_links;

\echo '-- product_modifiers.csv rows whose SKU is not in products.csv (skipped):'
SELECT DISTINCT l.sku, l.gname FROM link l LEFT JOIN map_item mi ON mi.sku = l.sku WHERE mi.id IS NULL;

\echo '-- modifier_options.csv rows whose group is not in modifier_groups.csv (skipped):'
SELECT btrim(o.modifier_name) AS group_name, btrim(o.name) AS option
FROM stg_options o
LEFT JOIN map_group mg
  ON mg.gkey = coalesce(nullif(btrim(o.modifier_reference), ''), btrim(o.modifier_name))
WHERE mg.id IS NULL
  AND coalesce(nullif(btrim(o.modifier_reference), ''), btrim(o.modifier_name))
      NOT IN (SELECT gkey FROM size_key);

\echo '-- product_optionals.csv rows whose SKU is not in products.csv (skipped):'
SELECT DISTINCT btrim(t.product_sku) AS sku, btrim(t.name) AS optional
FROM stg_optionals t LEFT JOIN map_item mi ON mi.sku = btrim(t.product_sku)
WHERE mi.id IS NULL AND nullif(btrim(t.name), '') IS NOT NULL;

\echo '-- Items placed in Uncategorized:'
SELECT btrim(i.name) AS item, nullif(i.category_reference, '') AS foodics_ref
FROM stg_items i LEFT JOIN map_category m ON m.reference = i.category_reference
WHERE m.id IS NULL ORDER BY 1;

\echo '-- Modifier groups with no options (required ones block selling the item):'
SELECT g.name, g.min_selections AS min, g.max_selections AS max
FROM map_group m JOIN modifier_groups g ON g.id = m.id
WHERE NOT EXISTS (SELECT 1 FROM modifier_options o WHERE o.group_id = g.id) ORDER BY 1;

\echo '-- Not imported (no Madar column): sku, barcode, tax_group_reference, calories, is_sold_by_weight, preparation_time, free_options, ereceipt_*, option sku/calories/tax_group'

:final;
