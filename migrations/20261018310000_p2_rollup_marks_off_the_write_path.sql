-- DB Phase 2.6b: take report-rollup maintenance off the per-order write path.
--
-- k6 (K6_RESULTS_2026-10-17.md, Result 3): the 2.6 statement-level dirty-mark
-- triggers cost ~12 ms of Postgres time per order created (INSERT INTO orders
-- 2.2 -> 4.6 ms, order_payments 1.3 -> 4.8 ms): every INSERT and UPDATE in the
-- create transaction captured a transition table and ran sales_daily_mark(),
-- which only ever finds today's date and writes nothing.
--
-- The rule a mark has to keep is unchanged: a write that changes a day the
-- reports may read from a rollup (a day inside the branch's coverage) marks
-- that SALE's day dirty in the same transaction. What changes is how cheaply
-- "this write cannot touch a covered day" is decided:
--
--   * Coverage only ever reaches days that ended at least `lag` ago, and the
--     lag is now at least 30 minutes (sales_daily_advance clamps it; the
--     worker uses one hour). So a row in a covered day has
--     created_at < now() - 30 minutes. A sale rung up now (created_at = now(),
--     or a POS clock a few minutes off) can never be in one.
--   * orders INSERT: a row trigger whose WHEN is that test. For a live sale
--     the WHEN is false and no function runs at all. A late offline replay
--     (created_at hours or days back) still marks its day.
--   * orders UPDATE (void, discount, table, loyalty, payment-method rename...):
--     still a statement trigger (a bulk rename stays one mark call), but it
--     returns before calling sales_daily_mark when no old or new row is older
--     than 30 minutes, which is every UPDATE inside an order create.
--   * order_items / order_payments / order_item_addons INSERT: no trigger.
--     These rows are only ever inserted in the transaction that inserts their
--     order (orders/handlers.rs create_order_once, delivery/snapshot.rs
--     apply_snapshot, demo/seed.rs); that order's own INSERT marks the day.
--   * UPDATE / DELETE on those three, order_refunds (all events) and orders
--     DELETE keep their statement triggers, with the same early return.
--     None of them runs while an order is created.
--
-- A straggler (a writer whose transaction started more than 30 minutes before
-- the worker advanced coverage and committed after it) is caught by the
-- catch-up below: once a day is two hours closed, D-1 and D-2 are recomputed
-- one more time if their rollup predates that.
--
-- Additive and idempotent: functions replaced, triggers dropped and created.
-- Nothing reads or moves ledger data here.
SET LOCAL lock_timeout = '15s';

-- Statement triggers: return before sales_daily_mark() unless some row is old
-- enough to be in a covered day.
CREATE OR REPLACE FUNCTION sales_daily_mark_orders() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $fn$
DECLARE
    v_old timestamptz := now() - interval '30 minutes';
BEGIN
    IF TG_LEVEL = 'ROW' THEN
        -- The INSERT row trigger; its WHEN already found NEW old enough.
        PERFORM sales_daily_mark(ARRAY[NEW.branch_id], ARRAY[NEW.created_at]);
        RETURN NULL;
    END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') AND EXISTS (SELECT 1 FROM new_rows WHERE created_at < v_old) THEN
        PERFORM sales_daily_mark(array_agg(n.branch_id), array_agg(n.created_at)) FROM new_rows n;
    END IF;
    IF TG_OP IN ('UPDATE', 'DELETE') AND EXISTS (SELECT 1 FROM old_rows WHERE created_at < v_old) THEN
        PERFORM sales_daily_mark(array_agg(o.branch_id), array_agg(o.created_at)) FROM old_rows o;
    END IF;
    RETURN NULL;
END
$fn$;

CREATE OR REPLACE FUNCTION sales_daily_mark_by_order() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $fn$
DECLARE
    v_old timestamptz := now() - interval '30 minutes';
BEGIN
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        PERFORM sales_daily_mark(array_agg(o.branch_id), array_agg(o.created_at))
           FROM (SELECT DISTINCT order_id FROM new_rows) n JOIN orders o ON o.id = n.order_id
          WHERE o.created_at < v_old
         HAVING count(*) > 0;
    END IF;
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        PERFORM sales_daily_mark(array_agg(o.branch_id), array_agg(o.created_at))
           FROM (SELECT DISTINCT order_id FROM old_rows) x JOIN orders o ON o.id = x.order_id
          WHERE o.created_at < v_old
         HAVING count(*) > 0;
    END IF;
    RETURN NULL;
END
$fn$;

CREATE OR REPLACE FUNCTION sales_daily_mark_by_line() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $fn$
DECLARE
    v_old timestamptz := now() - interval '30 minutes';
BEGIN
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        PERFORM sales_daily_mark(array_agg(o.branch_id), array_agg(o.created_at))
           FROM (SELECT DISTINCT order_item_id FROM new_rows) n
           JOIN order_items oi ON oi.id = n.order_item_id JOIN orders o ON o.id = oi.order_id
          WHERE o.created_at < v_old
         HAVING count(*) > 0;
    END IF;
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        PERFORM sales_daily_mark(array_agg(o.branch_id), array_agg(o.created_at))
           FROM (SELECT DISTINCT order_item_id FROM old_rows) x
           JOIN order_items oi ON oi.id = x.order_item_id JOIN orders o ON o.id = oi.order_id
          WHERE o.created_at < v_old
         HAVING count(*) > 0;
    END IF;
    RETURN NULL;
END
$fn$;

-- orders INSERT: a row trigger that does not fire for a sale rung up now.
DROP TRIGGER IF EXISTS sales_daily_mark_ins ON orders;
CREATE TRIGGER sales_daily_mark_ins AFTER INSERT ON orders
    FOR EACH ROW WHEN (NEW.created_at < now() - interval '30 minutes')
    EXECUTE FUNCTION sales_daily_mark_orders();

-- Lines, add-ons and payments are inserted only with their order.
DROP TRIGGER IF EXISTS sales_daily_mark_ins ON order_items;
DROP TRIGGER IF EXISTS sales_daily_mark_ins ON order_payments;
DROP TRIGGER IF EXISTS sales_daily_mark_ins ON order_item_addons;

-- Coverage never comes within 30 minutes of now, whatever lag a caller passes
-- (the guard above relies on it). Also the catch-up: D-1 and D-2, once two
-- hours closed, are marked for one more recompute if their rollup is older
-- than that (a straggling writer, or a day computed before it settled).
CREATE OR REPLACE FUNCTION sales_daily_advance(p_branch uuid, p_lag interval DEFAULT interval '1 hour')
RETURNS integer LANGUAGE plpgsql AS $fn$
DECLARE
    v_org    uuid;
    v_tz     text;
    v_today  date;
    v_target date;
    v_since  date;
    v_thr    date;
    n        integer := 0;
    m        integer := 0;
BEGIN
    SELECT org_id INTO v_org FROM branches WHERE id = p_branch;
    IF v_org IS NULL THEN
        RETURN 0;
    END IF;
    v_tz := effective_timezone(p_branch);
    v_today := (now() AT TIME ZONE v_tz)::date;
    v_target := ((now() - GREATEST(p_lag, interval '30 minutes')) AT TIME ZONE v_tz)::date - 1;
    SELECT since, through INTO v_since, v_thr
      FROM sales_daily_coverage WHERE branch_id = p_branch FOR UPDATE;
    IF NOT FOUND THEN
        SELECT COALESCE((min(created_at) AT TIME ZONE v_tz)::date, v_target + 1) INTO v_since
          FROM orders WHERE branch_id = p_branch;
        v_thr := v_since - 1;
        INSERT INTO sales_daily_coverage (org_id, branch_id, since, through)
        VALUES (v_org, p_branch, v_since, v_thr);
    END IF;
    IF v_target > v_thr THEN
        INSERT INTO sales_daily_dirty (org_id, branch_id, business_date, gen)
        SELECT DISTINCT v_org, p_branch, (o.created_at AT TIME ZONE v_tz)::date, 1
          FROM orders o
         WHERE o.branch_id = p_branch
           AND o.created_at >= (v_thr + 1)::timestamp AT TIME ZONE v_tz
           AND o.created_at <  (v_target + 1)::timestamp AT TIME ZONE v_tz
        ON CONFLICT (branch_id, business_date) DO UPDATE SET gen = sales_daily_dirty.gen + 1;
        GET DIAGNOSTICS n = ROW_COUNT;
        UPDATE sales_daily_coverage SET through = v_target, updated_at = now() WHERE branch_id = p_branch;
        v_thr := v_target;
    END IF;
    -- Catch-up. A day that had a sale and whose rollup was computed (or found
    -- empty) less than two hours after it closed is computed once more.
    INSERT INTO sales_daily_dirty (org_id, branch_id, business_date, gen)
    SELECT v_org, p_branch, d.day, 1
      FROM (SELECT (v_today - k) AS day FROM generate_series(1, 2) k) d
     CROSS JOIN LATERAL (SELECT (d.day + 1)::timestamp AT TIME ZONE v_tz AS day_end) e
     WHERE d.day BETWEEN v_since AND v_thr
       AND now() >= e.day_end + interval '2 hours'
       AND EXISTS (SELECT 1 FROM orders o
                    WHERE o.branch_id = p_branch
                      AND o.created_at >= d.day::timestamp AT TIME ZONE v_tz
                      AND o.created_at <  e.day_end)
       AND NOT EXISTS (SELECT 1 FROM sales_daily_sold s
                        WHERE s.branch_id = p_branch AND s.business_date = d.day
                          AND s.computed_at >= e.day_end + interval '2 hours')
    ON CONFLICT (branch_id, business_date) DO NOTHING;
    GET DIAGNOSTICS m = ROW_COUNT;
    RETURN n + m;
END
$fn$;

-- At a till close: bring the till's branch up to date right away instead of
-- at the worker's next pass (the close is when an offline till's late sales
-- and refunds have just landed). Advance its coverage, then recompute its
-- dirty days (at most 31; the worker takes any rest). Idempotent. SECURITY
-- DEFINER because the close's session sees only its own org's rows, which is
-- all this touches; it reads the ledger and rewrites only the rollup rows.
CREATE OR REPLACE FUNCTION sales_daily_settle_till(p_till uuid)
RETURNS integer LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $fn$
DECLARE
    v_branch uuid;
    r        record;
    n        integer := 0;
BEGIN
    SELECT branch_id INTO v_branch FROM tills WHERE id = p_till;
    IF v_branch IS NULL THEN
        RETURN 0;
    END IF;
    PERFORM sales_daily_advance(v_branch);
    FOR r IN SELECT business_date, gen FROM sales_daily_dirty
              WHERE branch_id = v_branch
              ORDER BY business_date LIMIT 31 FOR UPDATE SKIP LOCKED
    LOOP
        PERFORM sales_daily_sold_recompute(v_branch, r.business_date);
        DELETE FROM sales_daily_dirty
         WHERE branch_id = v_branch AND business_date = r.business_date AND gen = r.gen;
        n := n + 1;
    END LOOP;
    RETURN n;
END
$fn$;
REVOKE ALL ON FUNCTION sales_daily_settle_till(uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION sales_daily_settle_till(uuid) TO madar_app;
