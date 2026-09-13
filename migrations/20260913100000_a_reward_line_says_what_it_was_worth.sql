-- What a loyalty reward took off a line, in minor units, so an order's detail
-- and the redemption report can state the value given away without re-pricing
-- the line. 0 for a paid line and for lines written before this migration.
ALTER TABLE order_items
    ADD COLUMN reward_covered integer NOT NULL DEFAULT 0,
    ADD CONSTRAINT order_items_reward_covered_not_negative CHECK (reward_covered >= 0);
COMMENT ON COLUMN order_items.reward_covered IS
    'Minor units a loyalty reward took off this line (charged per unit, modifiers
     included, times reward_units, capped at the line). 0 for a paid line.';
