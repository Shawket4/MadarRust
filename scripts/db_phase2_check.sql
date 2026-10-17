-- Phase 2 verification (DB_ARCHITECTURE_AUDIT_2026-10-16 §8 Phase 2). Read-only;
-- safe on live prod. Every row it prints is a problem: empty = Phase 2 is in.
--
--   psql "$DB" -X -At -F $'\t' -f scripts/db_phase2_check.sql
\set ON_ERROR_STOP 1
\set QUIET on
BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY;

-- 2.1 A tenant policy that still reaches its org through another table
-- (left out on purpose: the authz and sync tables other phases own).
SELECT 'policy_not_flat', tablename::text, policyname::text
  FROM pg_policies
 WHERE schemaname = 'public'
   AND (qual ~* '\mFROM\M' OR with_check ~* '\mFROM\M')
   AND tablename NOT IN ('permissions', 'user_branch_assignments', 'sync_feed_watermarks',
                         'sync_sweep_state')
UNION ALL
-- 2.2 org_id still nullable on a table whose policy is the flat one.
SELECT 'org_id_nullable', c.table_name::text, ''
  FROM information_schema.columns c
  JOIN pg_policies p ON p.schemaname = 'public' AND p.tablename = c.table_name
                    AND p.policyname = 'tenant_isolation' AND p.qual LIKE '%IS TRUE%'
 WHERE c.table_schema = 'public' AND c.column_name = 'org_id' AND c.is_nullable = 'YES'
UNION ALL
-- 2.3 An index a failed CONCURRENTLY build left INVALID, or a global unique
-- that the per-org one replaced and that is still there.
SELECT 'invalid_index', indrelid::regclass::text, indexrelid::regclass::text
  FROM pg_index WHERE NOT indisvalid
UNION ALL
SELECT 'global_unique_left', 'public', relname::text
  FROM pg_class
 WHERE relnamespace = 'public'::regnamespace AND relkind = 'i'
   AND relname IN ('orders_idempotency_key_idx', 'uq_delivery_orders_idem', 'uq_open_tickets_idem',
                   'uq_open_ticket_rounds_idem', 'uq_till_cash_movements_client_ref',
                   'uq_order_refunds_client_ref', 'orders_order_ref_key', 'uq_open_tickets_ref',
                   'uq_delivery_orders_ref', 'branches_org_code_key')
UNION ALL
-- 2.4 A legacy catalog name that is not prod's view (or the dead table, still in public).
SELECT 'legacy_not_view', 'public', relname::text
  FROM pg_class
 WHERE relnamespace = 'public'::regnamespace AND relkind <> 'v'
   AND relname IN ('addon_item_ingredients', 'addon_items', 'branch_addon_overrides',
                   'branch_channel_addon_overrides', 'branch_channel_menu_overrides',
                   'branch_menu_overrides', 'branch_menu_size_overrides', 'item_sizes',
                   'menu_item_addon_overrides', 'menu_item_addon_slots',
                   'menu_item_allowed_addons', 'menu_item_optional_fields', 'menu_item_recipes')
ORDER BY 1, 2, 3;

COMMIT;
