-- Reports > Operations > Items: each item's menu price and "avg extra"
-- (owner-approved 2026-10-18). Two figures per sold item, additive on the
-- per-item rows of `GET /reports/branches/{id}/sales` (`top_items[]` and
-- `by_category[].items[]`):
--
--   menu_total   = SUM over the item's lines of unit_price × quantity: the
--                  size's price as rung, AT SALE TIME (order_items.unit_price
--                  is the catalogue + branch price of the chosen size when the
--                  server priced it live, or the till's price for it when an
--                  offline sale replayed; it never carries add-ons, swaps,
--                  optional fields, discounts or rewards).
--   extras_total = SUM over the item's lines of order_line_extras(): what was
--                  charged on top of that size price, before any discount.
--
-- The formula, per order line (see order_line_extras below):
--
--   paid  = unit_price × quantity + extras            (before discounts)
--   extra = paid − unit_price × quantity = extras
--   extras = Σ add-on rows (line_total + staff_comp_minor)
--          + quantity × Σ optional-field rows (price)
--
-- Add-on rows are every option picked on the line: plain add-ons and swaps
-- (a milk/bean swap is an add-on row priced at its delta). Their stored
-- line_total is what that pick was charged on the line (delta × its
-- quantity × the line's quantity), less a staff comp; the comp is added back
-- so a comp, like any discount, never lowers the extra. (Rows from
-- 2026-03-20 stored delta × its quantity without the line's quantity; their
-- line_total is still what the order's subtotal charged, so it is the figure
-- used, not a recomputation.) Optional fields are priced per unit.
--
-- Discounts never enter it: order-level discounts (orders.discount_amount)
-- are not on the line at all, and the line's own reductions — a deal
-- (deal_minor), a loyalty reward (reward_covered), a staff comp
-- (staff_comp_minor) — come off line_total, which is not read. A combo part
-- counts its add-ons and optional fields the same way; its share of the
-- combo price is the combo's deal, not an extra. So an extra is never below
-- zero. Same lines as the rest of the report: orders not voided/refunded,
-- combo headers left out.
--
-- avg_extra (served, not stored) = round(extras_total / quantity_sold) in
-- piastres.
--
-- ADDITIVE on the Phase 2.6 design: two NOT NULL DEFAULT 0 columns
-- (metadata-only), the recompute writes them, nothing on the order path.
-- order_item_optionals now feeds a rollup figure, so it gets the UPDATE and
-- DELETE dirty-mark triggers its siblings have (no INSERT trigger: its rows
-- are only inserted with their order, whose INSERT marks the day). Every day
-- already rolled up has 0 in the new columns, so it is marked dirty here (the
-- marker rows only, no ledger read at boot) and read live until the worker
-- recomputes it; `sales-rollup-backfill` does the same at once. Idempotent.
SET LOCAL lock_timeout = '15s';

ALTER TABLE sales_daily_sold_item
    ADD COLUMN IF NOT EXISTS menu_total   bigint NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS extras_total bigint NOT NULL DEFAULT 0;

-- What one order line was charged on top of its size's price, before any
-- discount (the formula above). One definition, read by the recompute and by
-- the live report alike.
CREATE OR REPLACE FUNCTION order_line_extras(p_order_item uuid, p_quantity integer)
RETURNS bigint LANGUAGE sql STABLE PARALLEL SAFE AS $fn$
    SELECT COALESCE((SELECT SUM(a.line_total::bigint + a.staff_comp_minor)
                       FROM order_item_addons a WHERE a.order_item_id = p_order_item), 0)
         + p_quantity::bigint * COALESCE((SELECT SUM(f.price::bigint)
                       FROM order_item_optionals f WHERE f.order_item_id = p_order_item), 0)
$fn$;

-- The 2.6 function (as of the hour/add-on grains), unchanged but for the two
-- new columns of the item rows.
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
        (org_id, branch_id, business_date, menu_item_id, item_name, quantity, revenue, name_translations,
         menu_total, extras_total)
    SELECT v_org, p_branch, p_date, oi.menu_item_id, oi.item_name, SUM(oi.quantity), SUM(oi.line_total),
           (array_agg(oi.name_translations ORDER BY oi.name_translations DESC))[1],
           SUM(oi.unit_price::bigint * oi.quantity), SUM(order_line_extras(oi.id, oi.quantity))
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

-- Optional fields now change a rollup figure: a late UPDATE or DELETE of one
-- on a closed day marks that day dirty, as add-on rows do.
DROP TRIGGER IF EXISTS sales_daily_mark_upd ON order_item_optionals;
CREATE TRIGGER sales_daily_mark_upd AFTER UPDATE ON order_item_optionals
    REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION sales_daily_mark_by_line();
DROP TRIGGER IF EXISTS sales_daily_mark_del ON order_item_optionals;
CREATE TRIGGER sales_daily_mark_del AFTER DELETE ON order_item_optionals
    REFERENCING OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION sales_daily_mark_by_line();

-- Every day rolled up before the new columns existed: read live until the
-- worker (or sales-rollup-backfill) recomputes it. A rerun marks only days
-- that still show a sold item with no menu total (a free item's day too,
-- which costs one more recompute and changes nothing).
INSERT INTO sales_daily_dirty (org_id, branch_id, business_date, gen)
SELECT DISTINCT si.org_id, si.branch_id, si.business_date, 1
  FROM sales_daily_sold_item si
 WHERE si.menu_total = 0 AND si.quantity <> 0
ON CONFLICT (branch_id, business_date) DO UPDATE SET gen = sales_daily_dirty.gen + 1;
