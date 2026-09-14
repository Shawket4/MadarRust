-- POST-migration invariants (NEW names, TILLS_CONTRACT.md §1.2). MUST print
-- byte-identical lines to invariants_before.sql on a correct migration
-- (section labels are neutral). Extra checks that have no "before" twin print
-- NOTHING when they pass and a 'VIOLATION|…' line when they fail, so any
-- failure shows up in the diff.
\pset format unaligned
\pset tuples_only on
\pset footer off

-- 1. every till (formerly shift) — identity + declared cash
SELECT 'session', s.id, concat_ws(',', s.branch_id, s.teller_id, s.status, s.opening_cash,
       coalesce(s.closing_cash_declared::text,'-'), coalesce(s.closing_cash_system::text,'-'),
       coalesce(s.cash_discrepancy::text,'-'), s.opened_at, coalesce(s.closed_at::text,'-'))
FROM tills s ORDER BY s.id;

-- 2. orders per till
SELECT 'orders', o.till_id, concat_ws(',', count(*), count(*) FILTER (WHERE o.status::text = 'voided'),
       coalesce(sum(o.total_amount),0), coalesce(sum(o.tip_amount),0), count(DISTINCT o.order_number))
FROM orders o GROUP BY o.till_id ORDER BY o.till_id;

-- 3. payments per till — grouped by the NEW order_payments.till_id
SELECT 'payments', p.till_id, concat_ws(',', count(p.*), coalesce(sum(p.amount),0),
       coalesce(sum(p.amount) FILTER (WHERE p.is_cash),0))
FROM order_payments p GROUP BY p.till_id ORDER BY p.till_id;
SELECT 'VIOLATION', 'payment_till_differs_from_order', count(*)
FROM order_payments p JOIN orders o ON o.id = p.order_id
WHERE p.till_id IS DISTINCT FROM o.till_id HAVING count(*) > 0;

-- 4. cash movements per till per kind
SELECT 'cash_movements', m.till_id || ':' || coalesce(m.kind::text,'-'), concat_ws(',', count(*), sum(m.amount))
FROM till_cash_movements m GROUP BY m.till_id, m.kind ORDER BY 2;

-- 5. refunds per till
SELECT 'refunds', r.till_id, concat_ws(',', count(*), sum(r.amount), coalesce(sum(r.amount) FILTER (WHERE r.is_cash),0))
FROM order_refunds r GROUP BY r.till_id ORDER BY r.till_id;

-- 6. open-ticket settles per till
SELECT 'ticket_settles', t.settled_till_id, count(*)
FROM open_tickets t WHERE t.settled_till_id IS NOT NULL GROUP BY 2 ORDER BY 2;

-- 7. open tills
SELECT 'open_session', s.id, concat_ws(',', s.branch_id, s.teller_id)
FROM tills s WHERE s.status::text = 'open' ORDER BY s.id;

-- 8. permission rows: resource 'shifts' was renamed to 'tills' (mapped back in the
--    key so lines match); 'shift_counts' is kept as a dead enum value, rows unchanged.
SELECT 'role_permissions', concat_ws(':', rp.role, CASE WHEN rp.resource::text = 'tills' THEN 'shifts' ELSE rp.resource::text END, rp.action), rp.granted
FROM role_permissions rp WHERE rp.resource::text IN ('tills','shift_counts') ORDER BY 2;
SELECT 'user_permissions', concat_ws(':', p.user_id, CASE WHEN p.resource::text = 'tills' THEN 'shifts' ELSE p.resource::text END, p.action), p.granted
FROM permissions p WHERE p.resource::text IN ('tills','shift_counts') ORDER BY 2;

-- 9. occupancy refs now reference the covering SESSION: same counts, and every
--    ref is a till of the same teller/branch covering the moment.
SELECT 'occupancy_till_refs', 'totals', concat_ws(',', count(*), count(started_till_id), count(ended_till_id))
FROM table_occupancies;
SELECT 'VIOLATION', 'occupancy_ref_not_covering_session', o.id
FROM table_occupancies o
WHERE (o.started_till_id IS NOT NULL AND NOT EXISTS (
         SELECT 1 FROM tills s WHERE s.id = o.started_till_id AND s.teller_id = o.started_by AND s.branch_id = o.branch_id
            AND s.opened_at <= o.started_at AND (s.closed_at IS NULL OR s.closed_at >= o.started_at)))
   OR (o.ended_till_id IS NOT NULL AND NOT EXISTS (
         SELECT 1 FROM tills s WHERE s.id = o.ended_till_id AND s.teller_id = o.ended_by AND s.branch_id = o.branch_id
            AND s.opened_at <= o.ended_at AND (s.closed_at IS NULL OR s.closed_at >= o.ended_at)))
ORDER BY o.id;

-- 10. the drawer ENTITY rows, from their archive
SELECT 'till_entity', t.id, concat_ws(',', t.org_id, t.branch_id, t.name, t.is_default, t.is_active, coalesce(t.deleted_at::text,'-'))
FROM archive.till_entities t ORDER BY t.id;

-- 11. global totals
SELECT 'totals', 'rows', concat_ws(',',
  (SELECT count(*) FROM tills), (SELECT count(*) FROM orders), (SELECT count(*) FROM order_payments),
  (SELECT count(*) FROM till_cash_movements), (SELECT count(*) FROM order_refunds),
  (SELECT count(*) FROM open_tickets WHERE settled_till_id IS NOT NULL), (SELECT count(*) FROM archive.till_entities));
SELECT 'totals', 'money', concat_ws(',',
  (SELECT coalesce(sum(total_amount),0) FROM orders), (SELECT coalesce(sum(amount),0) FROM order_payments),
  (SELECT coalesce(sum(amount),0) FROM till_cash_movements), (SELECT coalesce(sum(amount),0) FROM order_refunds));
