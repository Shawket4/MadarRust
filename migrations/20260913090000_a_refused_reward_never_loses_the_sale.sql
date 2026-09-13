-- Loyalty redemptions, made robust end to end.
--
-- 1. A replayed sale whose reward can no longer be paid for (the balance was
--    spent at another till, the programme was switched off, the catalogue
--    moved) is RECORDED, not refused: the till already collected the reduced
--    amount and the item is gone. No points move; the order says why.
-- 2. A line remembers how many of its units a reward covered, and the
--    redemption row names the line it paid for, so a refund of that line can
--    give the points back in proportion.

ALTER TABLE orders ADD COLUMN loyalty_redemption_refused text;
COMMENT ON COLUMN orders.loyalty_redemption_refused IS
    'Set when a replayed sale claimed rewards the points could not pay for.
     The covered lines stay covered (the drawer holds the reduced amount), no
     redeem rows were written, and price_flagged is true. NULL otherwise.';

ALTER TABLE order_items
    ADD COLUMN reward_units integer NOT NULL DEFAULT 0,
    ADD CONSTRAINT order_items_reward_units_within_quantity
        CHECK (reward_units >= 0 AND reward_units <= GREATEST(quantity, 0));
COMMENT ON COLUMN order_items.reward_units IS
    'How many of this line''s units a loyalty reward covered. 0 for a paid line.';

ALTER TABLE loyalty_transactions
    ADD COLUMN order_item_id uuid REFERENCES order_items(id) ON DELETE RESTRICT;
CREATE INDEX idx_loyalty_txn_order_item ON loyalty_transactions (order_item_id)
    WHERE order_item_id IS NOT NULL;
COMMENT ON COLUMN loyalty_transactions.order_item_id IS
    'For a redeem row, the order line it paid for. NULL on rows written before
     20260913090000 and on every other kind.';
