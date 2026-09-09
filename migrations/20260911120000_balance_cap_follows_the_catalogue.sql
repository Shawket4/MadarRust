-- The balance cap gets a switch of its own, so "no number" can mean something.
--
-- As first built, `balance_cap` carried two meanings in one column: NULL was
-- "no ceiling" and a number was the ceiling. That left nowhere to say the thing
-- a shop actually wants, which is "stop when they can afford the best thing on
-- the list" — a ceiling that follows the catalogue instead of being retyped
-- every time a reward is added or repriced.
--
-- So the switch is separate from the figure:
--
--   enabled = false                → collects without end (unchanged default)
--   enabled = true,  cap IS NULL   → the most expensive reward on offer
--   enabled = true,  cap = 500     → 500, whatever the catalogue says
--
-- The derived case is the one to reach for. Once a customer can claim anything
-- in the programme, further collecting buys them nothing and costs the shop a
-- liability it never chose; and because it reads the catalogue at award time,
-- adding a bigger reward raises the ceiling on its own.

ALTER TABLE loyalty_settings
    ADD COLUMN balance_cap_enabled boolean NOT NULL DEFAULT false;

-- Any row that already names a figure was switched on by whoever set it.
-- (Nothing is live yet — this only matters for a dev database that ran the
-- previous migration before this one existed.)
UPDATE loyalty_settings SET balance_cap_enabled = true WHERE balance_cap IS NOT NULL;

COMMENT ON COLUMN loyalty_settings.balance_cap_enabled IS
    'Whether a ceiling applies at all. With it on, a NULL balance_cap means '
    'the most expensive reward on offer rather than no ceiling.';
COMMENT ON COLUMN loyalty_settings.balance_cap IS
    'The ceiling, when balance_cap_enabled. NULL = derive it from the reward '
    'catalogue (the dearest reward), so it follows what is actually offered.';
