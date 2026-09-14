-- Baseline invariants on the PRE-rename schema (shifts / tills entity).
-- Output is canonical "section|key|values" lines, fully ORDERed, so the
-- post-migration file (invariants_after.sql) must print byte-identical lines.
\pset format unaligned
\pset tuples_only on
\pset footer off

-- 1. every till session (today: shift) — identity + declared cash
SELECT 'session', s.id, concat_ws(',', s.branch_id, s.teller_id, s.status, s.opening_cash,
       coalesce(s.closing_cash_declared::text,'-'), coalesce(s.closing_cash_system::text,'-'),
       coalesce(s.cash_discrepancy::text,'-'), s.opened_at, coalesce(s.closed_at::text,'-'))
FROM shifts s ORDER BY s.id;

-- 2. orders per session: count, voided, sum total, sum tip, distinct order numbers
SELECT 'orders', o.shift_id, concat_ws(',', count(*), count(*) FILTER (WHERE o.status::text = 'voided'),
       coalesce(sum(o.total_amount),0), coalesce(sum(o.tip_amount),0), count(DISTINCT o.order_number))
FROM orders o GROUP BY o.shift_id ORDER BY o.shift_id;

-- 3. payments per session (order_payments has no session column today; joined via orders)
SELECT 'payments', o.shift_id, concat_ws(',', count(p.*), coalesce(sum(p.amount),0),
       coalesce(sum(p.amount) FILTER (WHERE p.is_cash),0))
FROM order_payments p JOIN orders o ON o.id = p.order_id GROUP BY o.shift_id ORDER BY o.shift_id;

-- 4. cash movements per session per kind
SELECT 'cash_movements', m.shift_id || ':' || coalesce(m.kind::text,'-'), concat_ws(',', count(*), sum(m.amount))
FROM shift_cash_movements m GROUP BY m.shift_id, m.kind ORDER BY 2;

-- 5. refunds per session
SELECT 'refunds', r.shift_id, concat_ws(',', count(*), sum(r.amount), coalesce(sum(r.amount) FILTER (WHERE r.is_cash),0))
FROM order_refunds r GROUP BY r.shift_id ORDER BY r.shift_id;

-- 6. open-ticket settles per session
SELECT 'ticket_settles', t.settled_shift_id, count(*)
FROM open_tickets t WHERE t.settled_shift_id IS NOT NULL GROUP BY 2 ORDER BY 2;

-- 7. open sessions
SELECT 'open_session', s.id, concat_ws(',', s.branch_id, s.teller_id)
FROM shifts s WHERE s.status::text = 'open' ORDER BY s.id;

-- 8. permission rows for the session resource (+ dead shift_counts)
SELECT 'role_permissions', concat_ws(':', rp.role, rp.resource, rp.action), rp.granted
FROM role_permissions rp WHERE rp.resource::text IN ('shifts','shift_counts') ORDER BY 2;
SELECT 'user_permissions', concat_ws(':', p.user_id, p.resource, p.action), p.granted
FROM permissions p WHERE p.resource::text IN ('shifts','shift_counts') ORDER BY 2;

-- 9. table occupancy till refs (today -> tills ENTITY; decision: remap to the covering session)
SELECT 'occupancy_till_refs', 'totals', concat_ws(',', count(*), count(started_till_id), count(ended_till_id))
FROM table_occupancies;

-- 10. the tills ENTITY rows (to be archived, never lost)
SELECT 'till_entity', t.id, concat_ws(',', t.org_id, t.branch_id, t.name, t.is_default, t.is_active, coalesce(t.deleted_at::text,'-'))
FROM tills t ORDER BY t.id;

-- 11. global totals
SELECT 'totals', 'rows', concat_ws(',',
  (SELECT count(*) FROM shifts), (SELECT count(*) FROM orders), (SELECT count(*) FROM order_payments),
  (SELECT count(*) FROM shift_cash_movements), (SELECT count(*) FROM order_refunds),
  (SELECT count(*) FROM open_tickets WHERE settled_shift_id IS NOT NULL), (SELECT count(*) FROM tills));
SELECT 'totals', 'money', concat_ws(',',
  (SELECT coalesce(sum(total_amount),0) FROM orders), (SELECT coalesce(sum(amount),0) FROM order_payments),
  (SELECT coalesce(sum(amount),0) FROM shift_cash_movements), (SELECT coalesce(sum(amount),0) FROM order_refunds));
