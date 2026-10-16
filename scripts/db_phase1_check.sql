-- Phase 1 verification (DB_ARCHITECTURE_AUDIT_2026-10-16 §8 "Phase 1
-- verification"). Read-only; safe on live prod. Every row it prints is a
-- problem: an empty result means Phase 1 is complete.
--
--   psql "$DB" -X -At -F $'\t' -f scripts/db_phase1_check.sql
\set ON_ERROR_STOP 1
\set QUIET on
BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY;

-- 1. Every Phase 1 constraint is validated.
SELECT 'not_validated', conrelid::regclass::text, conname
  FROM pg_constraint
 WHERE NOT convalidated
   AND (conname LIKE '%\_org\_id\_present' OR conname LIKE '%\_org\_%\_fk'
        OR conname IN ('inventory_movements_source_type_check', 'sync_changes_type_check'))
UNION ALL
-- 2. Every Phase 1 index is valid (a failed CONCURRENTLY build leaves one INVALID).
SELECT 'invalid_index', indrelid::regclass::text, indexrelid::regclass::text
  FROM pg_index WHERE NOT indisvalid
ORDER BY 1, 2, 3;

-- 3. No NULL org_id, and 4. no child whose org differs from its parent's.
WITH paths(t, col, parent) AS (VALUES
    ('orders', 'branch_id', 'branches'), ('tills', 'branch_id', 'branches'),
    ('inventory_movements', 'branch_id', 'branches'), ('branch_stock', 'branch_id', 'branches'),
    ('sync_changes', 'branch_id', 'branches'),
    ('order_items', 'order_id', 'orders'), ('order_payments', 'order_id', 'orders'),
    ('till_cash_movements', 'till_id', 'tills'),
    ('open_ticket_rounds', 'open_ticket_id', 'open_tickets'),
    ('open_ticket_items', 'open_ticket_id', 'open_tickets'),
    ('open_ticket_items', 'round_id', 'open_ticket_rounds'),
    ('kitchen_ticket_items', 'kitchen_ticket_id', 'kitchen_tickets'),
    ('menu_item_sizes', 'menu_item_id', 'menu_items'),
    ('modifier_options', 'group_id', 'modifier_groups'),
    ('recipe_lines', 'ingredient_id', 'org_ingredients'),
    ('stocktake_items', 'stocktake_id', 'stocktakes'),
    ('stocktake_items', 'org_ingredient_id', 'org_ingredients'),
    ('purchase_order_lines', 'purchase_order_id', 'purchase_orders'),
    ('goods_receipt_lines', 'goods_receipt_id', 'goods_receipts'),
    ('stock_transfer_lines', 'transfer_id', 'stock_transfers'),
    ('packaging_rule_lines', 'rule_id', 'packaging_rules'),
    ('recipe_base_lines', 'base_id', 'recipe_bases'),
    ('order_item_addons', 'order_item_id', 'order_items'),
    ('order_item_optionals', 'order_item_id', 'order_items'),
    ('inventory_movements', 'org_ingredient_id', 'org_ingredients'),
    ('branch_stock', 'org_ingredient_id', 'org_ingredients'),
    ('kitchen_tickets', 'branch_id', 'branches'), ('open_tickets', 'branch_id', 'branches'))
SELECT r.* FROM paths p,
LATERAL query_to_xml(format(
    'SELECT count(*) FILTER (WHERE c.org_id IS NULL) AS nulls, '
    '       count(*) FILTER (WHERE c.org_id IS DISTINCT FROM p.org_id) AS mismatched '
    '  FROM %I c JOIN %I p ON p.id = c.%I', p.t, p.parent, p.col), false, true, '') AS x(doc),
LATERAL (SELECT 'org_id', p.t || '.' || p.col || ' -> ' || p.parent,
                'nulls=' || (xpath('/row/nulls/text()', x.doc))[1]::text
                || ' mismatched=' || (xpath('/row/mismatched/text()', x.doc))[1]::text) r(kind, path, counts)
WHERE r.counts <> 'nulls=0 mismatched=0'
ORDER BY 2;

COMMIT;
