-- Plan harness (DB_ARCHITECTURE_AUDIT_2026-10-16 §9.2). Runs each hot query
-- three times as the owner and three times as `madar_app` with the org
-- stamped (the RLS path the app takes), and prints the median execution time
-- in ms per query and role. Read-only (EXPLAIN ANALYZE of SELECTs only).
--
--   psql "$DB" -X -At -F $'\t' -f scripts/explain_hot_paths.sql
--
-- Picks the org and the branch with the most orders, and anchors every date
-- window at that org's latest order (restored dumps are weeks old).
\set ON_ERROR_STOP 1
\set QUIET on
SET client_min_messages = warning;
BEGIN;

SELECT b.org_id AS org, o.branch_id AS branch, max(o.created_at) AS anchor
  FROM orders o JOIN branches b ON b.id = o.branch_id
 GROUP BY 1, 2 ORDER BY count(*) DESC LIMIT 1 \gset
SELECT id AS till FROM tills WHERE branch_id = :'branch' ORDER BY opened_at DESC OFFSET 5 LIMIT 1 \gset
SELECT coalesce(max(seq), 0) - 500 AS seq FROM sync_changes WHERE branch_id = :'branch' \gset
SELECT teller_id AS teller FROM orders WHERE branch_id = :'branch' LIMIT 1 \gset

CREATE FUNCTION pg_temp.ms(q text) RETURNS numeric
LANGUAGE plpgsql AS $$
DECLARE j json; t numeric[] := '{}';
BEGIN
    FOR i IN 1..3 LOOP
        EXECUTE 'EXPLAIN (ANALYZE, FORMAT JSON) ' || q INTO j;
        t := t || (j -> 0 ->> 'Execution Time')::numeric;
    END LOOP;
    RETURN (SELECT round(percentile_cont(0.5) WITHIN GROUP (ORDER BY x)::numeric, 2) FROM unnest(t) x);
END $$;

CREATE TEMP TABLE hot(name text, q text);
INSERT INTO hot VALUES
('orders_page_30d', format($q$SELECT o.id, o.order_number, o.total_amount, o.created_at FROM orders o
   WHERE o.branch_id = %L AND o.created_at >= %L::timestamptz - interval '30 days'
   ORDER BY o.created_at DESC LIMIT 50$q$, :'branch', :'anchor')),
('orders_summary_30d', format($q$SELECT count(*), sum(o.total_amount),
   sum((SELECT sum(oi.quantity) FROM order_items oi WHERE oi.order_id = o.id)) FROM orders o
   WHERE o.branch_id = %L AND o.created_at >= %L::timestamptz - interval '30 days'$q$, :'branch', :'anchor')),
('till_fold', format($q$SELECT o.status, sum(p.amount) FILTER (WHERE p.is_cash), count(*) FROM orders o
   JOIN order_payments p ON p.order_id = o.id WHERE o.till_id = %L GROUP BY 1$q$, :'till')),
('branch_sales_all', format($q$SELECT count(*), sum(total_amount) FROM orders
   WHERE branch_id = %L AND status <> 'voided'$q$, :'branch')),
('top_items_30d_org', format($q$SELECT oi.menu_item_id, sum(oi.quantity), sum(oi.line_total) FROM order_items oi
   JOIN orders o ON o.id = oi.order_id JOIN branches b ON b.id = o.branch_id
   WHERE b.org_id = %L AND o.created_at >= %L::timestamptz - interval '30 days' AND o.status <> 'voided'
   GROUP BY 1 ORDER BY 3 DESC LIMIT 20$q$, :'org', :'anchor')),
('addon_revenue_30d_org', format($q$SELECT a.addon_item_id, sum(a.line_total) FROM order_item_addons a
   JOIN order_items oi ON oi.id = a.order_item_id JOIN orders o ON o.id = oi.order_id
   JOIN branches b ON b.id = o.branch_id
   WHERE b.org_id = %L AND o.created_at >= %L::timestamptz - interval '30 days' AND o.status <> 'voided'
   GROUP BY 1$q$, :'org', :'anchor')),
('ledger_window_48h', format($q$SELECT o.id FROM orders o WHERE o.branch_id = %L
   AND o.created_at >= %L::timestamptz - interval '48 hours'
   AND EXISTS (SELECT 1 FROM tills t WHERE t.id = o.till_id)$q$, :'branch', :'anchor')),
('incremental_pull', format($q$SELECT * FROM sync_changes WHERE branch_id = %L AND seq > %s
   ORDER BY seq LIMIT 2000$q$, :'branch', :'seq')),
('movements_page', format($q$SELECT * FROM inventory_movements WHERE branch_id = %L
   ORDER BY created_at DESC LIMIT 50$q$, :'branch')),
-- FK probes a user / branch_stock delete runs (Phase 1.1 indexes these).
('fk_probe_orders_voided_by', format($q$SELECT 1 FROM orders WHERE voided_by = %L LIMIT 1$q$, :'teller')),
('fk_probe_orders_started_by', format($q$SELECT 1 FROM orders WHERE started_by = %L LIMIT 1$q$, :'teller')),
('fk_probe_tills_closed_by', format($q$SELECT 1 FROM tills WHERE closed_by = %L LIMIT 1$q$, :'teller')),
('fk_probe_movements_branch_stock', $q$SELECT 1 FROM inventory_movements WHERE branch_stock_id = '00000000-0000-0000-0000-000000000001' LIMIT 1$q$);
GRANT SELECT ON hot TO madar_app;

SELECT name, 'owner', pg_temp.ms(q) FROM hot ORDER BY name;

SET LOCAL ROLE madar_app;
SELECT set_config('app.org_id', :'org', true) \gset
SELECT name, 'madar_app', pg_temp.ms(q) FROM hot ORDER BY name;

ROLLBACK;
