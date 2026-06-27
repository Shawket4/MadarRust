-- A waiter can apply a discount when taking the order; it's stored on the open
-- ticket and used at settle unless the cashier overrides it. Mirrors the order
-- discount columns (type is a free string: 'percentage' | 'fixed', resolved by
-- create_order_inner at settle).
ALTER TABLE open_tickets ADD COLUMN discount_id    uuid REFERENCES discounts(id);
ALTER TABLE open_tickets ADD COLUMN discount_type  text;
ALTER TABLE open_tickets ADD COLUMN discount_value integer;
