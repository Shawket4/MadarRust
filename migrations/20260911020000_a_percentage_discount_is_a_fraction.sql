-- A percentage is a FRACTION, the same as every other rate in this schema.
--
-- Tax stores 0.14 for 14%, and so do the service charge and the no-show rate:
-- the convention is that `_rate`-shaped things are fractions between 0 and 1,
-- guarded by a CHECK, and that the engine MULTIPLIES by them. A percentage
-- discount was the one exception — stored as `14`, divided by 100 at every use
-- site, in an `integer` column that could not express 12.5% at all.
--
-- Two conventions for "a percentage" inside one money engine is how a shop ends
-- up typing 14 into a field that wanted 0.14. That already happened once, on
-- the tax rate, and cost a day to find. This removes the second convention
-- rather than documenting it.
--
-- `discounts.value` is polymorphic and stays that way: a FIXED discount is an
-- amount in minor units, a PERCENTAGE is now a fraction. `numeric` holds the
-- integer case exactly, so nothing about fixed discounts changes.

ALTER TABLE discounts
    ALTER COLUMN value TYPE numeric(12,4) USING value::numeric;

UPDATE discounts SET value = value / 100 WHERE type = 'percentage';

ALTER TABLE discounts
    ADD CONSTRAINT discounts_percentage_is_a_fraction
        CHECK (type <> 'percentage' OR (value >= 0 AND value <= 1)),
    ADD CONSTRAINT discounts_fixed_is_not_negative
        CHECK (type <> 'fixed' OR value >= 0);

COMMENT ON COLUMN discounts.value IS
    'Polymorphic by `type`: a FRACTION for percentage (0.14 = 14%, like every
     other rate in this schema), or an amount in minor units for fixed.';

-- The three snapshots that freeze what was applied to a bill. Same reasoning:
-- a snapshot that means something different from the row it was copied from is
-- a snapshot nobody can read back.
ALTER TABLE orders
    ALTER COLUMN discount_value TYPE numeric(12,4) USING discount_value::numeric;
UPDATE orders SET discount_value = discount_value / 100 WHERE discount_type = 'percentage';

ALTER TABLE open_tickets
    ALTER COLUMN discount_value TYPE numeric(12,4) USING discount_value::numeric;
UPDATE open_tickets SET discount_value = discount_value / 100 WHERE discount_type = 'percentage';

ALTER TABLE delivery_orders
    ALTER COLUMN discount_value TYPE numeric(12,4) USING discount_value::numeric;
UPDATE delivery_orders SET discount_value = discount_value / 100 WHERE discount_type = 'percentage';
