-- Phase 5 backfills, rerunnable (DB_ARCHITECTURE_AUDIT_2026-10-16 §10: every
-- data move is idempotent). The migrations already ran each of these once; this
-- runs them again and prints `<backfill>\t<rows changed>`. Run after the
-- migrations, every count must be 0 (scripts/db_verify_phase.sh checks it).
--
--   psql "$DB" -X -At -F $'\t' -f scripts/db_phase5_backfill.sql
\set ON_ERROR_STOP 1
\set QUIET on
BEGIN;
SET LOCAL lock_timeout = '15s';

CREATE TEMP TABLE p5_counts (backfill text, n bigint) ON COMMIT DROP;

-- 5.1 ticket line columns (rows the column trigger has not filled)
WITH u AS (UPDATE open_ticket_items SET line = line WHERE quantity IS NULL RETURNING 1)
INSERT INTO p5_counts SELECT 'ticket_line_columns', count(*) FROM u;
WITH u AS (
    INSERT INTO open_ticket_item_addons (org_id, item_id, position, addon_item_id, quantity, unit_price)
    SELECT i.org_id, i.id, (a.ord - 1)::int, (a.v ->> 'addon_item_id')::uuid,
           COALESCE((a.v ->> 'quantity')::int, 1), COALESCE((a.v ->> 'unit_price')::int, 0)
      FROM open_ticket_items i
     CROSS JOIN LATERAL jsonb_array_elements(COALESCE(i.line -> 'input' -> 'addons', '[]'::jsonb))
           WITH ORDINALITY AS a(v, ord)
     WHERE a.v ? 'addon_item_id'
    ON CONFLICT (item_id, position) DO NOTHING RETURNING 1)
INSERT INTO p5_counts SELECT 'ticket_line_addons', count(*) FROM u;
WITH u AS (
    INSERT INTO open_ticket_item_optionals (org_id, item_id, position, option_id)
    SELECT i.org_id, i.id, (o.ord - 1)::int, (o.v #>> '{}')::uuid
      FROM open_ticket_items i
     CROSS JOIN LATERAL jsonb_array_elements(COALESCE(i.line -> 'input' -> 'optional_field_ids', '[]'::jsonb))
           WITH ORDINALITY AS o(v, ord)
    ON CONFLICT (item_id, position) DO NOTHING RETURNING 1)
INSERT INTO p5_counts SELECT 'ticket_line_optionals', count(*) FROM u;

-- 5.3 ref_counters (never lowers a counter; changes nothing when in step)
WITH src AS (
    SELECT 'order' AS scope, branch_id, business_date, last_seq FROM order_ref_counters
    UNION ALL SELECT 'ticket', branch_id, business_date, last_seq FROM ticket_ref_counters
    UNION ALL SELECT 'delivery', branch_id, business_date, last_seq FROM delivery_ref_counters),
u AS (
    INSERT INTO ref_counters (scope, org_id, branch_id, business_date, last_seq)
    SELECT s.scope, b.org_id, s.branch_id, s.business_date, s.last_seq
      FROM src s JOIN branches b ON b.id = s.branch_id
    ON CONFLICT (scope, branch_id, business_date) DO UPDATE
       SET last_seq = GREATEST(ref_counters.last_seq, EXCLUDED.last_seq)
     WHERE ref_counters.last_seq < EXCLUDED.last_seq
    RETURNING 1)
INSERT INTO p5_counts SELECT 'ref_counters', count(*) FROM u;

-- 5.6 money _wide twins (rows still out of step)
WITH u AS (UPDATE tills SET opening_cash = opening_cash
            WHERE opening_cash_wide IS DISTINCT FROM opening_cash
               OR opening_cash_original_wide IS DISTINCT FROM opening_cash_original
               OR closing_cash_declared_wide IS DISTINCT FROM closing_cash_declared
               OR closing_cash_system_wide IS DISTINCT FROM closing_cash_system
                          OR held_orders_left_open_total_wide IS DISTINCT FROM held_orders_left_open_total
            RETURNING 1)
INSERT INTO p5_counts SELECT 'money_wide_tills', count(*) FROM u;
WITH u AS (UPDATE till_reconciliations SET system_total = system_total
            WHERE system_total_wide IS DISTINCT FROM system_total
               OR current_system_total_wide IS DISTINCT FROM current_system_total
               OR declared_amount_wide IS DISTINCT FROM declared_amount
            RETURNING 1)
INSERT INTO p5_counts SELECT 'money_wide_till_reconciliations', count(*) FROM u;
WITH u AS (UPDATE loyalty_memberships SET lifetime_points = lifetime_points
            WHERE lifetime_points_wide IS DISTINCT FROM lifetime_points
               OR lifetime_visits_wide IS DISTINCT FROM lifetime_visits
            RETURNING 1)
INSERT INTO p5_counts SELECT 'money_wide_loyalty_memberships', count(*) FROM u;

SELECT backfill, n FROM p5_counts ORDER BY backfill;
COMMIT;
