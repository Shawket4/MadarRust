-- Two limits a shop can put on its own programme.
--
-- Both exist because a stamp card that only ever accrues is a liability that
-- only ever grows, and neither of the things that keeps it in check today is
-- under the shop's control.
--
-- 1. `balance_cap` stops a member's balance climbing past a figure the shop
--    chooses. Earning above the cap is not refused — the sale goes through and
--    the customer is served — it simply does not add to a balance that is
--    already at the ceiling. Off (NULL) is the current behaviour: no ceiling.
--
-- 2. `max_rewards_per_order` stops one visit clearing a hoard. A member with
--    thirty stamps and a five-stamp reward can otherwise walk out with six free
--    items in a single order, which is the same giveaway the shop thought it
--    was spreading over six visits. NULL is the current behaviour: unlimited.
--
-- Both are per-scope, like every other setting here: a branch row overrides the
-- org row, and a NULL in a branch row means that branch inherits.

ALTER TABLE loyalty_settings
    ADD COLUMN balance_cap           integer,
    ADD COLUMN max_rewards_per_order integer;

ALTER TABLE loyalty_settings
    ADD CONSTRAINT loyalty_settings_balance_cap_positive
        CHECK (balance_cap IS NULL OR balance_cap > 0),
    ADD CONSTRAINT loyalty_settings_max_rewards_positive
        CHECK (max_rewards_per_order IS NULL OR max_rewards_per_order > 0);

COMMENT ON COLUMN loyalty_settings.balance_cap IS
    'Highest balance a member may hold. NULL = no ceiling. Earning at the cap '
    'is silently dropped rather than refused: the sale is not the customer''s '
    'fault and must not fail.';
COMMENT ON COLUMN loyalty_settings.max_rewards_per_order IS
    'How many rewards one order may claim. NULL = unlimited. 1 stops a large '
    'balance being spent all at once.';

-- Deliberately not backfilled with a value. Every existing programme runs
-- uncapped today, and inventing a ceiling for them would start silently
-- discarding points people had been promised.
