-- Per-delivery-channel discounts, reusing the existing `discounts` table.
--
-- A manager attaches an optional discount to each delivery channel of a branch
-- (in-mall / outside). When a customer places a delivery order the active
-- channel discount is resolved + FROZEN onto the delivery order at intake, then
-- replayed verbatim into the real `orders` row at finalize. It applies to the
-- item subtotal only — the delivery fee is always charged in full — mirroring
-- the POS order discount semantics (discount before tax).

-- ── Per-branch, per-channel config ──────────────────────────────────────────
ALTER TABLE branch_delivery_settings
    ADD COLUMN in_mall_discount_id uuid REFERENCES discounts(id) ON DELETE SET NULL,
    ADD COLUMN outside_discount_id uuid REFERENCES discounts(id) ON DELETE SET NULL;

-- ── Frozen discount on the delivery order ───────────────────────────────────
ALTER TABLE delivery_orders
    ADD COLUMN discount_id     uuid REFERENCES discounts(id) ON DELETE SET NULL,
    ADD COLUMN discount_type   discount_type,
    ADD COLUMN discount_value  integer NOT NULL DEFAULT 0,
    ADD COLUMN discount_amount integer NOT NULL DEFAULT 0;

ALTER TABLE delivery_orders
    ADD CONSTRAINT delivery_orders_discount_nonneg
        CHECK (discount_value >= 0 AND discount_amount >= 0 AND discount_amount <= subtotal);
