-- Where the drink is going: takeaway (default) or dine-in.
--
-- NOT `order_type`. That is DERIVED — a sale settled from a waiter's ticket is
-- dine_in, anything else takeaway — and it decides the service charge. A
-- counter shop with no floor module can therefore never say "drinking in",
-- which is the gap this closes. `service_mode` is CHOSEN by the teller and
-- changes exactly one thing: a dine-in sale is served in the shop's own cup,
-- so nothing in the `packaging` ingredient category comes off stock.
--
-- Deliberately does NOT touch pricing: a dine-in counter sale attracts no
-- service charge (owner decision 2026-09-16), so this can never alter a total.
--
-- SOURCE TABLES: orders (already emits; an added column rides the projection).
ALTER TABLE orders
    ADD COLUMN IF NOT EXISTS service_mode text NOT NULL DEFAULT 'takeaway';
ALTER TABLE orders DROP CONSTRAINT IF EXISTS orders_service_mode_check;
ALTER TABLE orders ADD CONSTRAINT orders_service_mode_check
    CHECK (service_mode IN ('takeaway', 'dine_in'));
