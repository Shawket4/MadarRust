-- Existing negative money on a live database. READ ONLY — every statement
-- here is a SELECT; nothing is modified.
--
-- Run against prod (org Drops is 27b8f8db-fec2-4909-b9f6-9fffbd860a1a; the
-- query covers every org and reports per org so the others are visible too):
--
--   psql "$PROD_DATABASE_URL" -f scripts/negative-money/check_existing_negatives.sql
--
-- What each count means, and what to do with it:
--
--   orders.*            SHOULD be zero. `orders_money_is_not_negative` has
--                       constrained this table since 20260912030000, so a
--                       non-zero count here means a row predates that
--                       migration — the constraint was added without a scan.
--   order_items.*       The real question. Nothing has ever constrained these
--                       columns; migration 20260922000000 adds a NOT VALID
--                       check, which binds new rows only.
--   order_item_addons.* The same, one level down, and the one that matters
--                       most for reports: add-on revenue sums `line_total`.
--
-- If EVERY count below is zero, the new constraints can be adopted for
-- history too (they are NOT VALID until someone confirms this):
--
--   ALTER TABLE order_items       VALIDATE CONSTRAINT order_items_money_is_not_negative;
--   ALTER TABLE order_item_addons VALIDATE CONSTRAINT order_item_addons_money_is_not_negative;
--
-- VALIDATE takes only SHARE UPDATE EXCLUSIVE, so it does not block sales.

\echo '== orders with a negative money figure, by org =='
SELECT b.org_id,
       count(*) FILTER (WHERE o.subtotal < 0)              AS neg_subtotal,
       count(*) FILTER (WHERE o.discount_amount < 0)       AS neg_discount,
       count(*) FILTER (WHERE o.tax_amount < 0)            AS neg_tax,
       count(*) FILTER (WHERE o.service_charge_amount < 0) AS neg_service_charge,
       count(*) FILTER (WHERE o.total_amount < 0)          AS neg_total,
       count(*) FILTER (WHERE o.tip_amount < 0)            AS neg_tip,
       count(*) FILTER (WHERE o.discount_amount > o.subtotal) AS discount_over_subtotal
  FROM orders o JOIN branches b ON b.id = o.branch_id
 GROUP BY b.org_id
HAVING count(*) FILTER (
         WHERE o.subtotal < 0 OR o.discount_amount < 0 OR o.tax_amount < 0
            OR o.service_charge_amount < 0 OR o.total_amount < 0 OR o.tip_amount < 0
            OR o.discount_amount > o.subtotal) > 0
 ORDER BY 1;

\echo '== order lines with a negative price or total, by org =='
SELECT b.org_id,
       count(*) FILTER (WHERE i.unit_price < 0) AS neg_unit_price,
       count(*) FILTER (WHERE i.line_total < 0) AS neg_line_total,
       min(o.created_at) AS earliest,
       max(o.created_at) AS latest
  FROM order_items i JOIN orders o ON o.id = i.order_id
  JOIN branches b ON b.id = o.branch_id
 WHERE i.unit_price < 0 OR i.line_total < 0
 GROUP BY b.org_id
 ORDER BY 1;

\echo '== modifiers with a negative price or total, by org =='
SELECT b.org_id,
       count(*) FILTER (WHERE a.unit_price < 0) AS neg_unit_price,
       count(*) FILTER (WHERE a.line_total < 0) AS neg_line_total,
       min(o.created_at) AS earliest,
       max(o.created_at) AS latest
  FROM order_item_addons a
  JOIN order_items i ON i.id = a.order_item_id
  JOIN orders o ON o.id = i.order_id
  JOIN branches b ON b.id = o.branch_id
 WHERE a.unit_price < 0 OR a.line_total < 0
 GROUP BY b.org_id
 ORDER BY 1;

\echo '== catalogue prices that could author a negative line (no CHECK on these) =='
SELECT 'menu_items.base_price'        AS col, org_id, count(*) FROM menu_items        WHERE base_price    < 0 GROUP BY 2
UNION ALL
SELECT 'addon_items.default_price',        org_id, count(*) FROM addon_items         WHERE default_price < 0 GROUP BY 2
UNION ALL
SELECT 'item_sizes.price_override',        mi.org_id, count(*) FROM item_sizes s
       JOIN menu_items mi ON mi.id = s.menu_item_id WHERE s.price_override < 0 GROUP BY 2
UNION ALL
SELECT 'menu_item_optional_fields.price',  mi.org_id, count(*) FROM menu_item_optional_fields f
       JOIN menu_items mi ON mi.id = f.menu_item_id WHERE f.price < 0 GROUP BY 2
 ORDER BY 1, 2;

\echo '== open tickets and their frozen lines =='
SELECT t.org_id,
       count(*) FILTER (WHERE t.subtotal < 0) AS neg_ticket_subtotal
  FROM open_tickets t
 GROUP BY t.org_id
HAVING count(*) FILTER (WHERE t.subtotal < 0) > 0
 ORDER BY 1;
