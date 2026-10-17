-- Phase 5 verification (DB_ARCHITECTURE_AUDIT_2026-10-16 §8 Phase 5). Read-only;
-- safe on live prod. Every row it prints is a problem: empty = Phase 5 is in and
-- every expand reconciles with what it mirrors.
--
--   psql "$DB" -X -At -F $'\t' -f scripts/db_phase5_check.sql
\set ON_ERROR_STOP 1
\set QUIET on
BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY;

-- 5.1 relational ticket lines == the jsonb, per ticket.
SELECT 'ticket_lines_per_ticket', i.org_id::text,
       format('ticket %s: jsonb (n=%s qty=%s total=%s) columns (n=%s qty=%s total=%s)', i.open_ticket_id,
              count(*), sum((i.line ->> 'qty')::int), sum((i.line ->> 'line_total')::int),
              count(i.quantity), sum(i.quantity), sum(i.line_total))
  FROM open_ticket_items i
 GROUP BY i.org_id, i.open_ticket_id
HAVING count(*) <> count(i.quantity)
    OR sum((i.line ->> 'qty')::int) IS DISTINCT FROM sum(i.quantity)
    OR sum((i.line ->> 'line_total')::int) IS DISTINCT FROM sum(i.line_total)
UNION ALL
SELECT 'ticket_lines_columns', i.org_id::text, i.id::text
  FROM open_ticket_items i
 WHERE i.item_name IS DISTINCT FROM (i.line ->> 'name')
    OR i.unit_price IS DISTINCT FROM (i.line ->> 'unit_price')::int
    OR i.size_label IS DISTINCT FROM (i.line ->> 'size_label')
UNION ALL
SELECT 'ticket_lines_addons', i.org_id::text,
       format('%s: jsonb %s rows %s', i.id,
              jsonb_array_length(COALESCE(i.line -> 'input' -> 'addons', '[]')),
              (SELECT count(*) FROM open_ticket_item_addons a WHERE a.item_id = i.id))
  FROM open_ticket_items i
 WHERE jsonb_array_length(COALESCE(i.line -> 'input' -> 'addons', '[]'))
       <> (SELECT count(*) FROM open_ticket_item_addons a WHERE a.item_id = i.id)
UNION ALL
SELECT 'ticket_lines_optionals', i.org_id::text, i.id::text
  FROM open_ticket_items i
 WHERE jsonb_array_length(COALESCE(i.line -> 'input' -> 'optional_field_ids', '[]'))
       <> (SELECT count(*) FROM open_ticket_item_optionals o WHERE o.item_id = i.id)
UNION ALL
-- 5.3 ref_counters == the three old counters (max(last_seq) per key, both ways).
SELECT 'ref_counters_' || x.scope, x.branch_id::text, x.business_date || ' old=' || x.old || ' new=' || x.new
  FROM (
    SELECT coalesce(o.scope, n.scope) AS scope, coalesce(o.branch_id, n.branch_id) AS branch_id,
           coalesce(o.business_date, n.business_date) AS business_date, o.last_seq AS old, n.last_seq AS new
      FROM (SELECT 'order' AS scope, branch_id, business_date, last_seq FROM order_ref_counters
            UNION ALL SELECT 'ticket', branch_id, business_date, last_seq FROM ticket_ref_counters
            UNION ALL SELECT 'delivery', branch_id, business_date, last_seq FROM delivery_ref_counters) o
      FULL JOIN ref_counters n
        ON n.scope = o.scope AND n.branch_id = o.branch_id AND n.business_date = o.business_date) x
 WHERE x.old IS DISTINCT FROM x.new
UNION ALL
-- 5.6 every _wide column equals its int4 column.
SELECT 'money_wide_tills', org_id::text, id::text FROM tills
 WHERE opening_cash_wide IS DISTINCT FROM opening_cash
    OR opening_cash_original_wide IS DISTINCT FROM opening_cash_original
    OR closing_cash_declared_wide IS DISTINCT FROM closing_cash_declared
    OR closing_cash_system_wide IS DISTINCT FROM closing_cash_system
    OR held_orders_left_open_total_wide IS DISTINCT FROM held_orders_left_open_total
UNION ALL
SELECT 'money_wide_till_reconciliations', org_id::text, id::text FROM till_reconciliations
 WHERE system_total_wide IS DISTINCT FROM system_total
    OR current_system_total_wide IS DISTINCT FROM current_system_total
    OR declared_amount_wide IS DISTINCT FROM declared_amount
UNION ALL
SELECT 'money_wide_loyalty_memberships', org_id::text, id::text FROM loyalty_memberships
 WHERE lifetime_points_wide IS DISTINCT FROM lifetime_points
    OR lifetime_visits_wide IS DISTINCT FROM lifetime_visits
UNION ALL
-- 5.5 the rename and its compat view.
SELECT 'loyalty_rename', 'public', 'loyalty_memberships is not a table'
 WHERE (SELECT relkind FROM pg_class WHERE oid = to_regclass('public.loyalty_memberships')) IS DISTINCT FROM 'r'
UNION ALL
SELECT 'loyalty_rename', 'public', 'loyalty_customers is not the compat view'
 WHERE (SELECT relkind FROM pg_class WHERE oid = to_regclass('public.loyalty_customers')) IS DISTINCT FROM 'v'
UNION ALL
-- 5.7 no ledger FK from organizations / branches cascades any more.
SELECT 'ledger_fk_cascades', conrelid::regclass::text, conname
  FROM pg_constraint
 WHERE contype = 'f' AND confdeltype = 'c'
   AND confrelid IN ('organizations'::regclass, 'branches'::regclass)
   AND conrelid::regclass::text IN ('orders', 'order_items', 'order_payments', 'order_refunds',
       'order_refund_lines', 'tills', 'till_cash_movements', 'inventory_movements',
       'loyalty_transactions', 'open_tickets', 'kitchen_tickets', 'delivery_orders')
ORDER BY 1, 2, 3;

COMMIT;
