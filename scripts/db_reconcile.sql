-- Reconciliation harness (DB_ARCHITECTURE_AUDIT_2026-10-16 §10, Phase 0.2).
--
-- One TSV row per (org_id, table, metric, value): per-org counts and money /
-- quantity sums for every ledger table. No customer data. Stable ordering, so
-- two runs can be diffed byte for byte:
--
--   psql "$DB" -X -At -F $'\t' -f scripts/db_reconcile.sql > before.tsv
--   … apply the migrations / run the backfill …
--   psql "$DB" -X -At -F $'\t' -f scripts/db_reconcile.sql > after.tsv
--   diff before.tsv after.tsv          # must be empty
--
-- Read-only (it only SELECTs); safe against live prod. The org of a row is
-- always reached through its parent chain, never through a column added by a
-- later phase, so the numbers mean the same thing before and after a phase.
\set ON_ERROR_STOP 1
\set QUIET on
SET statement_timeout = '10min';
BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY;

WITH
o AS (SELECT o.*, b.org_id AS org FROM orders o JOIN branches b ON b.id = o.branch_id),
oi AS (SELECT oi.*, o.org FROM order_items oi JOIN o ON o.id = oi.order_id),
m AS (
    -- orders
    SELECT org, 'orders' t, 'count' metric, count(*)::numeric v FROM o GROUP BY org
    UNION ALL SELECT org, 'orders', 'count_status_' || status::text, count(*) FROM o GROUP BY org, status
    UNION ALL SELECT org, 'orders', 'sum_subtotal', COALESCE(sum(subtotal), 0) FROM o GROUP BY org
    UNION ALL SELECT org, 'orders', 'sum_discount_amount', COALESCE(sum(discount_amount), 0) FROM o GROUP BY org
    UNION ALL SELECT org, 'orders', 'sum_tax_amount', COALESCE(sum(tax_amount), 0) FROM o GROUP BY org
    UNION ALL SELECT org, 'orders', 'sum_service_charge_amount', COALESCE(sum(service_charge_amount), 0) FROM o GROUP BY org
    UNION ALL SELECT org, 'orders', 'sum_delivery_fee', COALESCE(sum(delivery_fee), 0) FROM o GROUP BY org
    UNION ALL SELECT org, 'orders', 'sum_tip_amount', COALESCE(sum(tip_amount), 0) FROM o GROUP BY org
    UNION ALL SELECT org, 'orders', 'sum_total_amount', COALESCE(sum(total_amount), 0) FROM o GROUP BY org
    UNION ALL SELECT org, 'orders', 'count_idempotency_key_null', count(*) FILTER (WHERE idempotency_key IS NULL) FROM o GROUP BY org
    -- order_items
    UNION ALL SELECT org, 'order_items', 'count', count(*) FROM oi GROUP BY org
    UNION ALL SELECT org, 'order_items', 'sum_quantity', COALESCE(sum(quantity), 0) FROM oi GROUP BY org
    UNION ALL SELECT org, 'order_items', 'sum_line_total', COALESCE(sum(line_total), 0) FROM oi GROUP BY org
    UNION ALL SELECT org, 'order_items', 'sum_unit_cost_x_qty', COALESCE(sum(unit_cost * quantity), 0) FROM oi GROUP BY org
    UNION ALL SELECT org, 'order_items', 'sum_line_cost', COALESCE(sum(line_cost), 0) FROM oi GROUP BY org
    -- order_item_addons / optionals
    UNION ALL SELECT oi.org, 'order_item_addons', 'count', count(*) FROM order_item_addons a JOIN oi ON oi.id = a.order_item_id GROUP BY oi.org
    UNION ALL SELECT oi.org, 'order_item_addons', 'sum_quantity', COALESCE(sum(a.quantity), 0) FROM order_item_addons a JOIN oi ON oi.id = a.order_item_id GROUP BY oi.org
    UNION ALL SELECT oi.org, 'order_item_addons', 'sum_line_total', COALESCE(sum(a.line_total), 0) FROM order_item_addons a JOIN oi ON oi.id = a.order_item_id GROUP BY oi.org
    UNION ALL SELECT oi.org, 'order_item_optionals', 'count', count(*) FROM order_item_optionals x JOIN oi ON oi.id = x.order_item_id GROUP BY oi.org
    UNION ALL SELECT oi.org, 'order_item_optionals', 'sum_price', COALESCE(sum(x.price), 0) FROM order_item_optionals x JOIN oi ON oi.id = x.order_item_id GROUP BY oi.org
    UNION ALL SELECT oi.org, 'order_item_optionals', 'sum_quantity_deducted', COALESCE(sum(x.quantity_deducted), 0) FROM order_item_optionals x JOIN oi ON oi.id = x.order_item_id GROUP BY oi.org
    -- order_payments
    UNION ALL SELECT o.org, 'order_payments', 'count_is_cash_' || p.is_cash::text, count(*) FROM order_payments p JOIN o ON o.id = p.order_id GROUP BY o.org, p.is_cash
    UNION ALL SELECT o.org, 'order_payments', 'sum_amount_is_cash_' || p.is_cash::text, COALESCE(sum(p.amount), 0) FROM order_payments p JOIN o ON o.id = p.order_id GROUP BY o.org, p.is_cash
    -- order_refunds / lines
    UNION ALL SELECT org_id, 'order_refunds', 'count', count(*) FROM order_refunds GROUP BY org_id
    UNION ALL SELECT org_id, 'order_refunds', 'sum_amount', COALESCE(sum(amount), 0) FROM order_refunds GROUP BY org_id
    UNION ALL SELECT org_id, 'order_refunds', 'sum_tax_amount', COALESCE(sum(tax_amount), 0) FROM order_refunds GROUP BY org_id
    UNION ALL SELECT r.org_id, 'order_refund_lines', 'count', count(*) FROM order_refund_lines l JOIN order_refunds r ON r.id = l.refund_id GROUP BY r.org_id
    -- tills
    UNION ALL SELECT b.org_id, 'tills', 'count_status_' || t.status::text, count(*) FROM tills t JOIN branches b ON b.id = t.branch_id GROUP BY b.org_id, t.status
    UNION ALL SELECT b.org_id, 'tills', 'sum_opening_cash', COALESCE(sum(t.opening_cash), 0) FROM tills t JOIN branches b ON b.id = t.branch_id GROUP BY b.org_id
    UNION ALL SELECT b.org_id, 'tills', 'sum_closing_cash_declared', COALESCE(sum(t.closing_cash_declared), 0) FROM tills t JOIN branches b ON b.id = t.branch_id GROUP BY b.org_id
    UNION ALL SELECT b.org_id, 'tills', 'sum_closing_cash_system', COALESCE(sum(t.closing_cash_system), 0) FROM tills t JOIN branches b ON b.id = t.branch_id GROUP BY b.org_id
    -- till_cash_movements
    UNION ALL SELECT b.org_id, 'till_cash_movements', 'count', count(*) FROM till_cash_movements m JOIN tills t ON t.id = m.till_id JOIN branches b ON b.id = t.branch_id GROUP BY b.org_id
    UNION ALL SELECT b.org_id, 'till_cash_movements', 'sum_amount', COALESCE(sum(m.amount), 0) FROM till_cash_movements m JOIN tills t ON t.id = m.till_id JOIN branches b ON b.id = t.branch_id GROUP BY b.org_id
    -- inventory_movements + branch_stock
    UNION ALL SELECT b.org_id, 'inventory_movements', 'count_type_' || im.type::text, count(*) FROM inventory_movements im JOIN branches b ON b.id = im.branch_id GROUP BY b.org_id, im.type
    UNION ALL SELECT b.org_id, 'inventory_movements', 'sum_quantity', COALESCE(sum(im.quantity), 0) FROM inventory_movements im JOIN branches b ON b.id = im.branch_id GROUP BY b.org_id
    UNION ALL SELECT b.org_id, 'branch_stock', 'count', count(*) FROM branch_stock s JOIN branches b ON b.id = s.branch_id GROUP BY b.org_id
    UNION ALL SELECT b.org_id, 'branch_stock', 'sum_on_hand', COALESCE(sum(s.on_hand), 0) FROM branch_stock s JOIN branches b ON b.id = s.branch_id GROUP BY b.org_id
    UNION ALL SELECT b.org_id, 'branch_stock', 'count_on_hand_ne_ledger', count(*) FILTER (WHERE s.on_hand IS DISTINCT FROM COALESCE(l.q, 0))
      FROM branch_stock s JOIN branches b ON b.id = s.branch_id
      LEFT JOIN (SELECT branch_id, org_ingredient_id, sum(quantity) q FROM inventory_movements GROUP BY 1, 2) l
             ON l.branch_id = s.branch_id AND l.org_ingredient_id = s.org_ingredient_id
     GROUP BY b.org_id
    -- loyalty
    UNION ALL SELECT org_id, 'loyalty_transactions', 'count_' || currency::text || '_' || kind::text, count(*) FROM loyalty_transactions GROUP BY org_id, currency, kind
    UNION ALL SELECT org_id, 'loyalty_transactions', 'sum_points_' || currency::text, COALESCE(sum(points), 0) FROM loyalty_transactions GROUP BY org_id, currency
    -- open tickets / items / kitchen
    UNION ALL SELECT org_id, 'open_tickets', 'count_status_' || status::text, count(*) FROM open_tickets GROUP BY org_id, status
    UNION ALL SELECT t.org_id, 'open_ticket_rounds', 'count', count(*) FROM open_ticket_rounds r JOIN open_tickets t ON t.id = r.open_ticket_id GROUP BY t.org_id
    UNION ALL SELECT t.org_id, 'open_ticket_items', 'count', count(*) FROM open_ticket_items i JOIN open_tickets t ON t.id = i.open_ticket_id GROUP BY t.org_id
    UNION ALL SELECT t.org_id, 'open_ticket_items', 'sum_line_total', COALESCE(sum((i.line ->> 'line_total')::numeric), 0) FROM open_ticket_items i JOIN open_tickets t ON t.id = i.open_ticket_id GROUP BY t.org_id
    UNION ALL SELECT org_id, 'kitchen_tickets', 'count', count(*) FROM kitchen_tickets GROUP BY org_id
    UNION ALL SELECT k.org_id, 'kitchen_ticket_items', 'count', count(*) FROM kitchen_ticket_items i JOIN kitchen_tickets k ON k.id = i.kitchen_ticket_id GROUP BY k.org_id
    -- catalog children that gain org_id in Phase 1
    UNION ALL SELECT mi.org_id, 'menu_item_sizes', 'count', count(*) FROM menu_item_sizes s JOIN menu_items mi ON mi.id = s.menu_item_id GROUP BY mi.org_id
    UNION ALL SELECT g.org_id, 'modifier_options', 'count', count(*) FROM modifier_options x JOIN modifier_groups g ON g.id = x.group_id GROUP BY g.org_id
    UNION ALL SELECT i.org_id, 'recipe_lines', 'count', count(*) FROM recipe_lines r JOIN org_ingredients i ON i.id = r.ingredient_id GROUP BY i.org_id
    UNION ALL SELECT i.org_id, 'recipe_lines', 'sum_quantity', COALESCE(sum(r.quantity), 0) FROM recipe_lines r JOIN org_ingredients i ON i.id = r.ingredient_id GROUP BY i.org_id
    -- sync changefeed (a Phase 1 backfill must not re-emit anything)
    UNION ALL SELECT b.org_id, 'sync_changes', 'count', count(*) FROM sync_changes c JOIN branches b ON b.id = c.branch_id GROUP BY b.org_id
    UNION ALL SELECT b.org_id, 'sync_changes', 'max_seq', COALESCE(max(c.seq), 0) FROM sync_changes c JOIN branches b ON b.id = c.branch_id GROUP BY b.org_id
    UNION ALL SELECT b.org_id, 'sync_changes', 'sum_seq', COALESCE(sum(c.seq), 0) FROM sync_changes c JOIN branches b ON b.id = c.branch_id GROUP BY b.org_id
    -- updated_at must not move under a backfill
    UNION ALL SELECT org, 'orders', 'max_updated_at_epoch', COALESCE(extract(epoch FROM max(updated_at)), 0) FROM o GROUP BY org
    UNION ALL SELECT b.org_id, 'tills', 'max_updated_at_epoch', COALESCE(extract(epoch FROM max(t.updated_at)), 0) FROM tills t JOIN branches b ON b.id = t.branch_id GROUP BY b.org_id
    UNION ALL SELECT org_id, 'catalog_revision', 'revision', revision FROM catalog_revision
)
SELECT org, t, metric, v FROM m ORDER BY org, t, metric;

COMMIT;
