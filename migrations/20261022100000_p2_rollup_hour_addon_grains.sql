-- Report rollups, two more grains (QUERY_SURVEY_2026-10-17 #8): the hour of
-- day and the add-on, per (branch, business day), so the peak-hours and
-- add-on reports can add up closed days the way branch sales does. (Peak days
-- need no new grain: a weekday is a sum of business days, all in
-- sales_daily_sold.)
--
-- ADDITIVE, on the 2.6 design unchanged:
--   * two new tables, filled by sales_daily_sold_recompute next to the others
--     (DELETE + INSERT of the same (branch, day), so a rerun gives the same
--     rows) and dropped with them by sales_daily_tz_changed;
--   * no new trigger and nothing on the order path: a write to a closed day
--     already marks that DAY dirty (orders, order_items, order_payments,
--     order_refunds, order_item_addons), and that recompute now rewrites these
--     rows too;
--   * the read plan (sales_daily_plan) is unchanged: a day it serves from the
--     rollups is clean, and every clean day is recomputed by THIS function.
--
-- Days computed before this migration have no hour or add-on rows. They are
-- marked dirty here (the marker rows only; no ledger read at boot), so every
-- report reads them live until the worker's next tick recomputes them (a few
-- hundred days per second; `sales-rollup-backfill` does the same at once).
-- Exactly the state a newly covered day is in.
--
-- The add-on report's name translations are the greatest of the lines'
-- (jsonb order), like the items' (a rule that merges across days); the live
-- report picks the same way.
SET LOCAL lock_timeout = '15s';

CREATE TABLE IF NOT EXISTS sales_daily_sold_hour (
    org_id        uuid     NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    branch_id     uuid     NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    business_date date     NOT NULL,
    -- Local hour of the sale (EXTRACT(hour FROM created_at AT TIME ZONE the
    -- branch's zone)), 0..23.
    hour          smallint NOT NULL,
    sold_orders   bigint   NOT NULL,
    voided_orders bigint   NOT NULL,
    revenue_net   bigint   NOT NULL,
    discount      bigint   NOT NULL,
    tax_net       bigint   NOT NULL,
    line_items    bigint   NOT NULL,
    addons        bigint   NOT NULL,
    PRIMARY KEY (branch_id, business_date, hour)
);
CREATE TABLE IF NOT EXISTS sales_daily_sold_addon (
    org_id            uuid   NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    branch_id         uuid   NOT NULL REFERENCES branches (id) ON DELETE CASCADE,
    business_date     date   NOT NULL,
    addon_item_id     uuid   NOT NULL,
    addon_name        text   NOT NULL,
    quantity          bigint NOT NULL,
    revenue           bigint NOT NULL,
    name_translations jsonb  NOT NULL,
    PRIMARY KEY (branch_id, business_date, addon_item_id, addon_name)
);

DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['sales_daily_sold_hour', 'sales_daily_sold_addon'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON %I', t);
        EXECUTE format($p$CREATE POLICY tenant_isolation ON %I FOR ALL
            USING ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE)
            WITH CHECK ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE)$p$, t);
        EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %I TO madar_app', t);
    END LOOP;
END $$;

-- The 2.6 function, unchanged above the two new blocks at its end.
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
    DELETE FROM sales_daily_sold_hour   WHERE branch_id = p_branch AND business_date = p_date;
    DELETE FROM sales_daily_sold_addon  WHERE branch_id = p_branch AND business_date = p_date;

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

    -- The peak-hours report's figures (src/reports/sales.rs, peak_hours), per
    -- local hour of the day. An hour row exists for every hour with an order.
    INSERT INTO sales_daily_sold_hour
        (org_id, branch_id, business_date, hour, sold_orders, voided_orders, revenue_net,
         discount, tax_net, line_items, addons)
    WITH oh AS MATERIALIZED (
        SELECT o.id, o.status, EXTRACT(hour FROM o.created_at AT TIME ZONE v_tz)::smallint AS hour,
               o.total_amount, o.discount_amount, o.tax_amount
          FROM orders o
         WHERE o.branch_id = p_branch AND o.created_at >= v_from AND o.created_at < v_to
    )
    SELECT v_org, p_branch, p_date, a.hour, a.sold, a.voided, a.revenue, a.discount, a.tax,
           COALESCE(li.q, 0), COALESCE(ad.q, 0)
      FROM (SELECT oh.hour,
                   COUNT(*) FILTER (WHERE oh.status NOT IN ('voided', 'refunded')) AS sold,
                   COUNT(*) FILTER (WHERE oh.status = 'voided') AS voided,
                   COALESCE(SUM(oh.total_amount - COALESCE(rf.refunded_amount, 0)) FILTER (WHERE oh.status NOT IN ('voided', 'refunded')), 0) AS revenue,
                   COALESCE(SUM(oh.discount_amount) FILTER (WHERE oh.status NOT IN ('voided', 'refunded')), 0) AS discount,
                   COALESCE(SUM(oh.tax_amount - COALESCE(rf.refunded_tax, 0)) FILTER (WHERE oh.status NOT IN ('voided', 'refunded')), 0) AS tax
              FROM oh LEFT JOIN v_order_refund_totals rf ON rf.order_id = oh.id
             GROUP BY oh.hour) a
      LEFT JOIN (SELECT oh.hour, SUM(oi.quantity) AS q
                   FROM oh JOIN order_items oi ON oi.order_id = oh.id
                  WHERE oh.status NOT IN ('voided', 'refunded') AND oi.line_kind <> 'combo'
                  GROUP BY oh.hour) li ON li.hour = a.hour
      LEFT JOIN (SELECT oh.hour, SUM(x.quantity) AS q
                   FROM oh JOIN order_items oi ON oi.order_id = oh.id
                   JOIN order_item_addons x ON x.order_item_id = oi.id
                  WHERE oh.status NOT IN ('voided', 'refunded')
                  GROUP BY oh.hour) ad ON ad.hour = a.hour;

    -- The add-on report's figures (src/reports/sales.rs, addon_sales).
    INSERT INTO sales_daily_sold_addon
        (org_id, branch_id, business_date, addon_item_id, addon_name, quantity, revenue, name_translations)
    SELECT v_org, p_branch, p_date, a.addon_item_id, a.addon_name, SUM(a.quantity), SUM(a.line_total),
           (array_agg(a.name_translations ORDER BY a.name_translations DESC))[1]
      FROM order_item_addons a
      JOIN order_items oi ON oi.id = a.order_item_id
      JOIN orders o ON o.id = oi.order_id
     WHERE o.branch_id = p_branch AND o.created_at >= v_from AND o.created_at < v_to
       AND o.status NOT IN ('voided', 'refunded')
     GROUP BY a.addon_item_id, a.addon_name;
END
$fn$;

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
    DELETE FROM sales_daily_sold_hour   WHERE branch_id = ANY(v_branches);
    DELETE FROM sales_daily_sold_addon  WHERE branch_id = ANY(v_branches);
    RETURN NULL;
END
$fn$;

-- Every day computed before the new grains existed: read live until the
-- worker recomputes it (idempotent: a day already dirty just bumps its gen,
-- and a rerun finds those days already marked).
INSERT INTO sales_daily_dirty (org_id, branch_id, business_date, gen)
SELECT s.org_id, s.branch_id, s.business_date, 1
  FROM sales_daily_sold s
 WHERE NOT EXISTS (SELECT 1 FROM sales_daily_sold_hour h
                    WHERE h.branch_id = s.branch_id AND h.business_date = s.business_date)
ON CONFLICT (branch_id, business_date) DO UPDATE SET gen = sales_daily_dirty.gen + 1;
