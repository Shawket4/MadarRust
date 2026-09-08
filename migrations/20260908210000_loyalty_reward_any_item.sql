-- Let a reward be spent on ANY menu item, not only a curated few.
--
-- Off by default and deliberately so: a catalogue is the safer shape, because
-- it lets a shop offer an espresso for five stamps without also offering the
-- steak. This is for shops whose programme is "collect five, get anything" —
-- a real model, and one the catalogue cannot express without listing the whole
-- menu and keeping that list in step with it forever.
--
-- The cost is the scope's `default_reward_cost`; per-item pricing is what the
-- catalogue is for, and the two are alternatives rather than layers.
ALTER TABLE loyalty_settings
    ADD COLUMN IF NOT EXISTS reward_any_item boolean NOT NULL DEFAULT false;
