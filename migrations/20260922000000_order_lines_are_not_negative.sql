-- A line of an order may never be negative (owner, 2026-09-18).
--
-- `orders` has carried `orders_money_is_not_negative` since
-- 20260912030000, so a negative BILL could never be stored. Its LINES could:
-- nothing constrained `order_items.unit_price` or `line_total`, and the header
-- check only sees the sum — so a line of -500 beside one of +1500 stored a
-- negative line under a perfectly valid order. That is the row every report,
-- every menu-engineering rollup and every cost snapshot then reads.
--
-- NOT VALID, deliberately. The server runs `sqlx::migrate!` on boot, so a
-- constraint that fails its initial scan does not fail a deploy — it
-- CRASH-LOOPS PRODUCTION. Whether any negative line exists in a live database
-- is exactly what we cannot check from here. NOT VALID applies the rule to
-- every new and updated row immediately, which is the whole protection needed,
-- and leaves history alone.
--
-- To adopt the existing rows later, once they have been looked at:
--   SELECT count(*) FROM order_items WHERE unit_price < 0 OR line_total < 0;
--   -- if that is 0:
--   ALTER TABLE order_items VALIDATE CONSTRAINT order_items_money_is_not_negative;
-- VALIDATE takes only a SHARE UPDATE EXCLUSIVE lock, so it does not block
-- sales while it scans.

ALTER TABLE order_items
    ADD CONSTRAINT order_items_money_is_not_negative
    CHECK (unit_price >= 0 AND line_total >= 0) NOT VALID;

-- The same for a line's MODIFIERS, and for the same reason one step down: a
-- -5.00 modifier on a 20.00 coffee leaves the line at 15.00 — positive, valid,
-- and hiding a negative add-on row that the add-on revenue reports sum.
ALTER TABLE order_item_addons
    ADD CONSTRAINT order_item_addons_money_is_not_negative
    CHECK (unit_price >= 0 AND line_total >= 0) NOT VALID;
