-- Phase 3 verification (DB_ARCHITECTURE_AUDIT_2026-10-16 §8 Phase 3). Read-only;
-- safe on live prod. Every row it prints is a problem: empty = Phase 3 is in.
--
--   psql "$DB" -X -At -F $'\t' -f scripts/db_phase3_check.sql
\set ON_ERROR_STOP 1
\set QUIET on
BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY;

-- 3.4 included_option_ids == menu_item_modifier_group_options, per org.
WITH arr AS (
    SELECT m.org_id, m.id AS mimg_id, u.opt AS option_id
      FROM menu_item_modifier_groups m
     CROSS JOIN LATERAL unnest(m.included_option_ids) u(opt)
     WHERE EXISTS (SELECT 1 FROM modifier_options o WHERE o.id = u.opt)
), j AS (
    SELECT org_id, mimg_id, option_id FROM menu_item_modifier_group_options
)
SELECT 'mimg_options_count', org_id::text, format('array=%s join=%s', a, b)
  FROM (SELECT coalesce(x.org_id, y.org_id) AS org_id, coalesce(x.n, 0) a, coalesce(y.n, 0) b
          FROM (SELECT org_id, count(*) n FROM (SELECT DISTINCT * FROM arr) d GROUP BY 1) x
          FULL JOIN (SELECT org_id, count(*) n FROM j GROUP BY 1) y USING (org_id)) c
 WHERE a <> b
UNION ALL
SELECT 'mimg_options_missing_in_join', org_id::text, mimg_id || ' ' || option_id
  FROM (SELECT * FROM arr EXCEPT SELECT * FROM j) d
UNION ALL
SELECT 'mimg_options_extra_in_join', org_id::text, mimg_id || ' ' || option_id
  FROM (SELECT * FROM j EXCEPT SELECT * FROM arr) d
UNION ALL
SELECT 'mimg_allowlist_flag', org_id::text, id::text
  FROM menu_item_modifier_groups
 WHERE option_allowlist IS DISTINCT FROM (included_option_ids IS NOT NULL)
UNION ALL
-- 3.3 the compat schema holds every compat view.
SELECT 'compat_view_missing', 'compat', v
  FROM unnest(ARRAY['addon_item_ingredients', 'addon_items', 'branch_addon_overrides',
                    'branch_channel_addon_overrides', 'branch_channel_menu_overrides',
                    'branch_menu_overrides', 'branch_menu_size_overrides', 'item_sizes',
                    'menu_item_addon_slots', 'menu_item_allowed_addons',
                    'menu_item_optional_fields', 'menu_item_recipes', 'loyalty_members_v']) v
 WHERE to_regclass('compat.' || v) IS NULL
ORDER BY 1, 2, 3;

COMMIT;
