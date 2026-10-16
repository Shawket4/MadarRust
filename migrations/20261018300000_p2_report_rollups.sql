-- DB architecture Phase 2.6 (DB_ARCHITECTURE_AUDIT_2026-10-16 §8 Phase 2.6):
-- closed-day rollups the sales reports can be served from.
--
-- ADDITIVE ONLY: new tables and functions, statement-level triggers that only
-- write a marker row. Nothing here reads or moves ledger data at boot.
--
-- Why new tables and not the Phase 1 `sales_daily_*` ones: those hold the
-- LEDGER's view of a day (voided excluded, refunds on the day they were
-- issued, items by size). The reports have their own, older contract: a "sold"
-- order excludes voided AND refunded; refunds are netted against the SALE and
-- so land on the sale's day; items rank by (menu_item_id, item_name) without
-- combo headers; the payment and service-mode mixes cover sold orders only.
-- `sales_daily_sold*` hold exactly those figures, per (branch, business day),
-- so a report over whole closed days can add them up instead of re-reading
-- the ledger.
--
-- When a rollup day may be read (sales_daily_plan): the day is before the
-- branch's local today, within the branch's coverage (`sales_daily_coverage`:
-- every day from `since` to `through` has been computed), and NOT dirty.
-- Everything else in a report's window is computed live, so a report is
-- always exactly what the ledger says:
--   * Any write to a closed day (a late offline replay, a refund or void of an
--     old sale, a payment correction) marks the SALE's (branch, day) dirty in
--     the same transaction (`sales_daily_dirty`, triggers below). A dirty day
--     is served live until the worker has recomputed it.
--   * Coverage advances only over days that closed more than an hour ago, and
--     a day enters coverage dirty: the worker computes it afterwards.
--   * A timezone change on a branch or org drops that branch's rollups and
--     coverage (its business days moved); the worker rebuilds them.
--   * The worker (src/rollups.rs) recomputes dirty days under a row lock on
--     the marker, deleting it only if no write bumped its `gen` meanwhile.
--
-- Backfill of history: `cargo run --bin sales-rollup-backfill` (or the worker,
-- a batch per tick). Validation: `cargo run --bin report-rollup-reconcile`.
SET LOCAL lock_timeout = '15s';

CREATE TABLE IF NOT EXISTS sales_daily_sold (
    org_id               uuid    NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    branch_id            uuid    NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    business_date        date    NOT NULL,
    sold_orders          bigint  NOT NULL,
    voided_orders        bigint  NOT NULL,
    subtotal             bigint  NOT NULL,
    discount             bigint  NOT NULL,
    tax_net              bigint  NOT NULL,
    revenue_net          bigint  NOT NULL,
    gross_sales          bigint  NOT NULL,
    refunded_amount      bigint  NOT NULL,
    service_charge_net   bigint  NOT NULL,
    sc_waived_count      bigint  NOT NULL,
    sc_waived_amount     bigint  NOT NULL,
    delivery_fees        bigint  NOT NULL,
    tips                 bigint  NOT NULL,
    cash_tips            bigint  NOT NULL,
    line_items           bigint  NOT NULL,
    addons               bigint  NOT NULL,
    computed_at          timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, business_date)
);
CREATE TABLE IF NOT EXISTS sales_daily_sold_method (
    org_id        uuid   NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    branch_id     uuid   NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    business_date date   NOT NULL,
    method        text   NOT NULL,
    amount        bigint NOT NULL,
    PRIMARY KEY (branch_id, business_date, method)
);
CREATE TABLE IF NOT EXISTS sales_daily_sold_mode (
    org_id        uuid   NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    branch_id     uuid   NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    business_date date   NOT NULL,
    service_mode  text   NOT NULL,
    orders        bigint NOT NULL,
    revenue       bigint NOT NULL,
    PRIMARY KEY (branch_id, business_date, service_mode)
);
CREATE TABLE IF NOT EXISTS sales_daily_sold_item (
    org_id            uuid   NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    branch_id         uuid   NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    business_date     date   NOT NULL,
    menu_item_id      uuid   NULL,
    item_name         text   NOT NULL,
    quantity          bigint NOT NULL,
    revenue           bigint NOT NULL,
    -- The greatest of the lines' translations (jsonb order): a rule that can
    -- be merged across days. The live report picks the same way.
    name_translations jsonb  NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_sales_daily_sold_item
    ON sales_daily_sold_item (branch_id, business_date, menu_item_id, item_name) NULLS NOT DISTINCT;
CREATE TABLE IF NOT EXISTS sales_daily_coverage (
    org_id     uuid NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    branch_id  uuid PRIMARY KEY REFERENCES branches (id) ON DELETE CASCADE,
    since      date NOT NULL,
    through    date NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS sales_daily_dirty (
    org_id        uuid   NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    branch_id     uuid   NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    business_date date   NOT NULL,
    gen           bigint NOT NULL DEFAULT 1,
    marked_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, business_date)
);
CREATE INDEX IF NOT EXISTS idx_sales_daily_sold_org ON sales_daily_sold (org_id, business_date);

DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['sales_daily_sold', 'sales_daily_sold_method', 'sales_daily_sold_mode',
                             'sales_daily_sold_item', 'sales_daily_coverage', 'sales_daily_dirty'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON %I', t);
        EXECUTE format($p$CREATE POLICY tenant_isolation ON %I FOR ALL
            USING ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE)
            WITH CHECK ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE)$p$, t);
        EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %I TO madar_app', t);
    END LOOP;
END $$;

-- ── recompute one (branch, business day) from the ledger ────────────────────
-- DELETE + INSERT in the caller's transaction: rerunning gives the same rows.
-- Each figure is the reports' own SQL (src/reports/handlers.rs, branch_sales /
-- branch_sales_timeseries / org_branch_comparison) restricted to the day.
CREATE OR REPLACE FUNCTION sales_daily_sold_recompute(p_branch uuid, p_date date)
RETURNS void LANGUAGE plpgsql AS $fn$
DECLARE
    v_org  uuid;
    v_tz   text;
    v_from timestamptz;
    v_to   timestamptz;
BEGIN
    SELECT org_id INTO v_org FROM branches WHERE id = p_branch;
    IF v_org IS NULL THEN
        RETURN;
    END IF;
    v_tz   := effective_timezone(p_branch);
    v_from := p_date::timestamp AT TIME ZONE v_tz;
    v_to   := (p_date + 1)::timestamp AT TIME ZONE v_tz;

    DELETE FROM sales_daily_sold        WHERE branch_id = p_branch AND business_date = p_date;
    DELETE FROM sales_daily_sold_method WHERE branch_id = p_branch AND business_date = p_date;
    DELETE FROM sales_daily_sold_mode   WHERE branch_id = p_branch AND business_date = p_date;
    DELETE FROM sales_daily_sold_item   WHERE branch_id = p_branch AND business_date = p_date;

    INSERT INTO sales_daily_sold
        (org_id, branch_id, business_date, sold_orders, voided_orders, subtotal, discount,
         tax_net, revenue_net, gross_sales, refunded_amount, service_charge_net,
         sc_waived_count, sc_waived_amount, delivery_fees, tips, cash_tips, line_items, addons)
    SELECT v_org, p_branch, p_date,
           COUNT(*) FILTER (WHERE o.status NOT IN ('voided', 'refunded')),
           COUNT(*) FILTER (WHERE o.status = 'voided'),
           COALESCE(SUM(o.subtotal)        FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0),
           COALESCE(SUM(o.discount_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0),
           COALESCE(SUM(o.tax_amount - COALESCE(rf.refunded_tax, 0)) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0),
           COALESCE(SUM(o.total_amount - COALESCE(rf.refunded_amount, 0)) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0),
           COALESCE(SUM(o.total_amount)    FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0),
           COALESCE(SUM(rf.refunded_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0),
           COALESCE(SUM(o.service_charge_amount - COALESCE(rf.refunded_service_charge, 0)) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0),
           COUNT(o.id) FILTER (WHERE o.status NOT IN ('voided', 'refunded') AND o.service_charge_waived_by IS NOT NULL),
           COALESCE(SUM(o.service_charge_waived_amount) FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0),
           COALESCE(SUM(o.delivery_fee)    FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0),
           COALESCE(SUM(o.tip_amount)      FILTER (WHERE o.status NOT IN ('voided', 'refunded')), 0),
           COALESCE(SUM(o.tip_amount) FILTER (
               WHERE o.status NOT IN ('voided', 'refunded')
                 AND COALESCE(o.tip_is_cash, COALESCE(o.tip_payment_method, o.payment_method) = 'cash')), 0),
           COALESCE((SELECT SUM(oi.quantity) FROM order_items oi JOIN orders o3 ON o3.id = oi.order_id
                      WHERE o3.branch_id = p_branch AND o3.created_at >= v_from AND o3.created_at < v_to
                        AND o3.status NOT IN ('voided', 'refunded') AND oi.line_kind <> 'combo'), 0),
           COALESCE((SELECT SUM(a.quantity) FROM order_item_addons a
                       JOIN order_items oi ON oi.id = a.order_item_id
                       JOIN orders o4 ON o4.id = oi.order_id
                      WHERE o4.branch_id = p_branch AND o4.created_at >= v_from AND o4.created_at < v_to
                        AND o4.status NOT IN ('voided', 'refunded')), 0)
      FROM orders o
      LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id
     WHERE o.branch_id = p_branch AND o.created_at >= v_from AND o.created_at < v_to
    HAVING COUNT(*) > 0;

    INSERT INTO sales_daily_sold_method (org_id, branch_id, business_date, method, amount)
    SELECT v_org, p_branch, p_date, op.method, SUM(op.amount)
      FROM order_payments op JOIN orders o ON o.id = op.order_id
     WHERE o.branch_id = p_branch AND o.created_at >= v_from AND o.created_at < v_to
       AND o.status NOT IN ('voided', 'refunded')
     GROUP BY op.method;

    INSERT INTO sales_daily_sold_mode (org_id, branch_id, business_date, service_mode, orders, revenue)
    SELECT v_org, p_branch, p_date, o.service_mode, COUNT(*),
           COALESCE(SUM(o.total_amount - COALESCE(rf.refunded_amount, 0)), 0)
      FROM orders o LEFT JOIN v_order_refund_totals rf ON rf.order_id = o.id
     WHERE o.branch_id = p_branch AND o.created_at >= v_from AND o.created_at < v_to
       AND o.status::text NOT IN ('voided', 'refunded')
     GROUP BY o.service_mode;

    INSERT INTO sales_daily_sold_item
        (org_id, branch_id, business_date, menu_item_id, item_name, quantity, revenue, name_translations)
    SELECT v_org, p_branch, p_date, oi.menu_item_id, oi.item_name, SUM(oi.quantity), SUM(oi.line_total),
           (array_agg(oi.name_translations ORDER BY oi.name_translations DESC))[1]
      FROM order_items oi JOIN orders o ON o.id = oi.order_id
     WHERE o.branch_id = p_branch AND o.created_at >= v_from AND o.created_at < v_to
       AND o.status NOT IN ('voided', 'refunded') AND oi.line_kind <> 'combo'
     GROUP BY oi.menu_item_id, oi.item_name;
END
$fn$;

-- ── dirty marking (a write to a CLOSED day) ─────────────────────────────────
-- Marks the business day of each (branch, sale time) that is before the
-- branch's local today. Today's writes mark nothing: today is never read from
-- a rollup, and it is computed once it has closed. SECURITY DEFINER so a
-- tenant write marks its day whatever that session may see of the marker
-- table; it writes nothing else.
CREATE OR REPLACE FUNCTION sales_daily_mark(p_branch uuid[], p_at timestamptz[])
RETURNS void LANGUAGE sql SECURITY DEFINER SET search_path = public AS $fn$
    INSERT INTO sales_daily_dirty (org_id, branch_id, business_date, gen)
    SELECT DISTINCT ON (x.branch_id, x.day) x.org_id, x.branch_id, x.day, 1
      FROM (SELECT b.org_id, b.id AS branch_id, (u.at AT TIME ZONE z.tz)::date AS day,
                   (now() AT TIME ZONE z.tz)::date AS today
              FROM (SELECT DISTINCT * FROM unnest(p_branch, p_at)) u(branch_id, at)
              JOIN branches b ON b.id = u.branch_id
              CROSS JOIN LATERAL (SELECT effective_timezone(b.id) AS tz) z
             WHERE u.at IS NOT NULL) x
     WHERE x.day < x.today
    ON CONFLICT (branch_id, business_date)
    DO UPDATE SET gen = sales_daily_dirty.gen + 1, marked_at = now();
$fn$;

CREATE OR REPLACE FUNCTION sales_daily_mark_orders() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $fn$
BEGIN
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        PERFORM sales_daily_mark(array_agg(n.branch_id), array_agg(n.created_at)) FROM new_rows n;
    END IF;
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        PERFORM sales_daily_mark(array_agg(o.branch_id), array_agg(o.created_at)) FROM old_rows o;
    END IF;
    RETURN NULL;
END
$fn$;

-- Children reach the sale through order_id (order_item_addons through its line).
CREATE OR REPLACE FUNCTION sales_daily_mark_by_order() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $fn$
BEGIN
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        PERFORM sales_daily_mark(array_agg(o.branch_id), array_agg(o.created_at))
           FROM (SELECT DISTINCT order_id FROM new_rows) n JOIN orders o ON o.id = n.order_id;
    END IF;
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        PERFORM sales_daily_mark(array_agg(o.branch_id), array_agg(o.created_at))
           FROM (SELECT DISTINCT order_id FROM old_rows) x JOIN orders o ON o.id = x.order_id;
    END IF;
    RETURN NULL;
END
$fn$;

CREATE OR REPLACE FUNCTION sales_daily_mark_by_line() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $fn$
BEGIN
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        PERFORM sales_daily_mark(array_agg(o.branch_id), array_agg(o.created_at))
           FROM (SELECT DISTINCT order_item_id FROM new_rows) n
           JOIN order_items oi ON oi.id = n.order_item_id JOIN orders o ON o.id = oi.order_id;
    END IF;
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        PERFORM sales_daily_mark(array_agg(o.branch_id), array_agg(o.created_at))
           FROM (SELECT DISTINCT order_item_id FROM old_rows) x
           JOIN order_items oi ON oi.id = x.order_item_id JOIN orders o ON o.id = oi.order_id;
    END IF;
    RETURN NULL;
END
$fn$;

DO $$
DECLARE
    r record;
BEGIN
    FOR r IN SELECT * FROM (VALUES
        ('orders', 'sales_daily_mark_orders'),
        ('order_items', 'sales_daily_mark_by_order'),
        ('order_payments', 'sales_daily_mark_by_order'),
        ('order_refunds', 'sales_daily_mark_by_order'),
        ('order_item_addons', 'sales_daily_mark_by_line')) AS v(t, fn)
    LOOP
        EXECUTE format('DROP TRIGGER IF EXISTS sales_daily_mark_ins ON %I', r.t);
        EXECUTE format('DROP TRIGGER IF EXISTS sales_daily_mark_upd ON %I', r.t);
        EXECUTE format('DROP TRIGGER IF EXISTS sales_daily_mark_del ON %I', r.t);
        EXECUTE format('CREATE TRIGGER sales_daily_mark_ins AFTER INSERT ON %I '
                       'REFERENCING NEW TABLE AS new_rows FOR EACH STATEMENT EXECUTE FUNCTION %I()', r.t, r.fn);
        EXECUTE format('CREATE TRIGGER sales_daily_mark_upd AFTER UPDATE ON %I '
                       'REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows '
                       'FOR EACH STATEMENT EXECUTE FUNCTION %I()', r.t, r.fn);
        EXECUTE format('CREATE TRIGGER sales_daily_mark_del AFTER DELETE ON %I '
                       'REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT EXECUTE FUNCTION %I()', r.t, r.fn);
    END LOOP;
END $$;

-- A branch's (or its org's) timezone moved its business days: its rollups
-- and coverage no longer mean anything. Drop them; the worker rebuilds.
CREATE OR REPLACE FUNCTION sales_daily_tz_changed() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $fn$
DECLARE
    v_branches uuid[];
BEGIN
    IF TG_TABLE_NAME = 'branches' THEN
        v_branches := ARRAY[NEW.id];
    ELSE
        v_branches := ARRAY(SELECT id FROM branches WHERE org_id = NEW.id);
    END IF;
    DELETE FROM sales_daily_coverage    WHERE branch_id = ANY(v_branches);
    DELETE FROM sales_daily_dirty       WHERE branch_id = ANY(v_branches);
    DELETE FROM sales_daily_sold        WHERE branch_id = ANY(v_branches);
    DELETE FROM sales_daily_sold_method WHERE branch_id = ANY(v_branches);
    DELETE FROM sales_daily_sold_mode   WHERE branch_id = ANY(v_branches);
    DELETE FROM sales_daily_sold_item   WHERE branch_id = ANY(v_branches);
    RETURN NULL;
END
$fn$;
DROP TRIGGER IF EXISTS sales_daily_tz_changed ON branches;
CREATE TRIGGER sales_daily_tz_changed AFTER UPDATE OF timezone ON branches
    FOR EACH ROW WHEN (OLD.timezone IS DISTINCT FROM NEW.timezone)
    EXECUTE FUNCTION sales_daily_tz_changed();
DROP TRIGGER IF EXISTS sales_daily_tz_changed ON organizations;
CREATE TRIGGER sales_daily_tz_changed AFTER UPDATE OF timezone ON organizations
    FOR EACH ROW WHEN (OLD.timezone IS DISTINCT FROM NEW.timezone)
    EXECUTE FUNCTION sales_daily_tz_changed();

-- ── the worker's two steps (run as the owner) ───────────────────────────────
-- Extend a branch's coverage over the days that closed at least `p_lag` ago.
-- Each newly covered day with a sale enters DIRTY (served live until
-- recomputed), in the same transaction as the coverage update, so no write can
-- slip between "covered" and "computed". Returns the days marked.
CREATE OR REPLACE FUNCTION sales_daily_advance(p_branch uuid, p_lag interval DEFAULT interval '1 hour')
RETURNS integer LANGUAGE plpgsql AS $fn$
DECLARE
    v_org    uuid;
    v_tz     text;
    v_target date;
    v_since  date;
    v_thr    date;
    n        integer;
BEGIN
    SELECT org_id INTO v_org FROM branches WHERE id = p_branch;
    IF v_org IS NULL THEN
        RETURN 0;
    END IF;
    v_tz := effective_timezone(p_branch);
    v_target := ((now() - p_lag) AT TIME ZONE v_tz)::date - 1;
    SELECT since, through INTO v_since, v_thr
      FROM sales_daily_coverage WHERE branch_id = p_branch FOR UPDATE;
    IF NOT FOUND THEN
        SELECT COALESCE((min(created_at) AT TIME ZONE v_tz)::date, v_target + 1) INTO v_since
          FROM orders WHERE branch_id = p_branch;
        v_thr := v_since - 1;
        INSERT INTO sales_daily_coverage (org_id, branch_id, since, through)
        VALUES (v_org, p_branch, v_since, v_thr);
    END IF;
    IF v_target <= v_thr THEN
        RETURN 0;
    END IF;
    INSERT INTO sales_daily_dirty (org_id, branch_id, business_date, gen)
    SELECT DISTINCT v_org, p_branch, (o.created_at AT TIME ZONE v_tz)::date, 1
      FROM orders o
     WHERE o.branch_id = p_branch
       AND o.created_at >= (v_thr + 1)::timestamp AT TIME ZONE v_tz
       AND o.created_at <  (v_target + 1)::timestamp AT TIME ZONE v_tz
    ON CONFLICT (branch_id, business_date) DO UPDATE SET gen = sales_daily_dirty.gen + 1;
    GET DIAGNOSTICS n = ROW_COUNT;
    UPDATE sales_daily_coverage SET through = v_target, updated_at = now() WHERE branch_id = p_branch;
    RETURN n;
END
$fn$;

-- Recompute up to `p_limit` dirty days. Each marker is row-locked while its
-- day is recomputed (a concurrent write's upsert waits, commits after, and
-- re-marks the day), and deleted only when its gen is the one read.
CREATE OR REPLACE FUNCTION sales_daily_clean(p_limit integer DEFAULT 50)
RETURNS integer LANGUAGE plpgsql AS $fn$
DECLARE
    r record;
    n integer := 0;
BEGIN
    FOR r IN SELECT branch_id, business_date, gen FROM sales_daily_dirty
              ORDER BY business_date, branch_id LIMIT p_limit FOR UPDATE SKIP LOCKED
    LOOP
        PERFORM sales_daily_sold_recompute(r.branch_id, r.business_date);
        DELETE FROM sales_daily_dirty
         WHERE branch_id = r.branch_id AND business_date = r.business_date AND gen = r.gen;
        n := n + 1;
    END LOOP;
    RETURN n;
END
$fn$;

-- ── the read plan ───────────────────────────────────────────────────────────
-- Split a report window [p_from, p_to] (both inclusive, NULL = open) over the
-- given branches into rollup DAYS (day NOT NULL) and LIVE intervals [lo, hi)
-- (day NULL) that together cover it exactly once. With p_rollups = false (or
-- for a branch whose zone is not p_tz, when given) the whole window is live.
-- A rollup day must be entirely inside the window. One allowance: a window
-- ending in the last second before the day's end (the dashboard sends
-- midnight − 1 ms) still counts the day, but only when the ledger has no sale
-- of that branch in that last sliver.
CREATE OR REPLACE FUNCTION sales_daily_plan(
    p_branches uuid[], p_from timestamptz, p_to timestamptz,
    p_rollups boolean, p_tz text DEFAULT NULL)
RETURNS TABLE (branch_id uuid, day date, lo timestamptz, hi timestamptz)
LANGUAGE plpgsql STABLE AS $fn$
DECLARE
    b      uuid;
    v_tz   text;
    v_lo   timestamptz := COALESCE(p_from, '-infinity'::timestamptz);
    v_hi   timestamptz := CASE WHEN p_to IS NULL THEN 'infinity'::timestamptz
                               ELSE p_to + interval '1 microsecond' END;
    v_since date;
    v_thr  date;
    v_cur  timestamptz;
    d      date;
    ds     timestamptz;
    de     timestamptz;
BEGIN
    IF v_lo >= v_hi THEN
        RETURN;
    END IF;
    FOREACH b IN ARRAY COALESCE(p_branches, '{}'::uuid[]) LOOP
        v_tz := effective_timezone(b);
        IF v_tz IS NULL THEN
            CONTINUE;  -- not a branch this session can see: the live read finds nothing either
        END IF;
        v_since := NULL;
        v_thr := NULL;
        IF p_rollups AND (p_tz IS NULL OR p_tz = v_tz) THEN
            SELECT c.since, LEAST(c.through, (now() AT TIME ZONE v_tz)::date - 1)
              INTO v_since, v_thr
              FROM sales_daily_coverage c WHERE c.branch_id = b;
        END IF;
        v_cur := v_lo;
        IF v_thr IS NOT NULL AND v_since <= v_thr THEN
            FOR d IN
                SELECT g::date FROM generate_series(
                    GREATEST(v_since, CASE WHEN p_from IS NULL THEN v_since
                                           ELSE (p_from AT TIME ZONE v_tz)::date END)::timestamp,
                    v_thr::timestamp, interval '1 day') g
            LOOP
                ds := d::timestamp AT TIME ZONE v_tz;
                de := (d + 1)::timestamp AT TIME ZONE v_tz;
                CONTINUE WHEN ds < v_lo;
                EXIT WHEN de > v_hi + interval '1 second';
                CONTINUE WHEN EXISTS (SELECT 1 FROM sales_daily_dirty x
                                       WHERE x.branch_id = b AND x.business_date = d);
                IF de > v_hi THEN
                    EXIT WHEN EXISTS (SELECT 1 FROM orders o WHERE o.branch_id = b
                                         AND o.created_at >= v_hi AND o.created_at < de);
                END IF;
                IF v_cur < ds THEN
                    branch_id := b; day := NULL; lo := v_cur; hi := ds;
                    RETURN NEXT;
                END IF;
                branch_id := b; day := d; lo := ds; hi := de;
                RETURN NEXT;
                v_cur := de;
            END LOOP;
        END IF;
        IF v_cur < v_hi THEN
            branch_id := b; day := NULL; lo := v_cur; hi := v_hi;
            RETURN NEXT;
        END IF;
    END LOOP;
END
$fn$;
