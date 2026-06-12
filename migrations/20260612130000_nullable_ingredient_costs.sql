-- Never-entered ingredient costs must read as UNKNOWN, not free.
--
-- The costing convention is "NULL = unknown, never zero; explicit 0 =
-- genuinely free" — but cost_per_unit was NOT NULL DEFAULT 0, so every
-- ingredient created without a cost landed at 0 and flowed through every
-- rollup as a 0-cost recipe (e.g. items showing 100% margin in menu
-- engineering's current-cost basis). Make the column nullable, drop the
-- default, and convert the existing zeros — all of which are unentered
-- defaults, not deliberate "free" entries — to NULL. Cost-history epochs
-- carrying those zeros are noise from the same default and are removed
-- (point-in-time lookups then fall back to the catalog value, now NULL).
--
-- From here on: the API stores NULL when no cost is supplied, and an
-- explicit 0 entered through the dashboard means genuinely free.

ALTER TABLE org_ingredients
    ALTER COLUMN cost_per_unit DROP NOT NULL,
    ALTER COLUMN cost_per_unit DROP DEFAULT;

UPDATE org_ingredients SET cost_per_unit = NULL WHERE cost_per_unit = 0;

DELETE FROM ingredient_cost_history WHERE cost_per_unit = 0;
