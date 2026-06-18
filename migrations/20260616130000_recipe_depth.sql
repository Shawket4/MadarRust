-- Recipe depth: per-ingredient yield (waste/shrinkage) factor + density bridge.
--
-- yield_pct: usable percentage after trim/cook loss (e.g. 70 = 70% usable). A
-- recipe that needs N usable units consumes N / (yield_pct/100) of the PURCHASED
-- ingredient. NULL = 100% (no loss). Applied at recipe-save time, so the stored
-- quantity_used is the yield-adjusted consumption in the base unit — every
-- deduction and cost rollup then stays correct with no runtime change.
--
-- density_g_per_ml: grams per millilitre, bridging weight↔volume so a recipe can
-- be authored in ml against a kg-purchased ingredient (oils, syrups, milk) or
-- vice-versa. NULL = no cross-family conversion (kept strict, as before).
ALTER TABLE org_ingredients
    ADD COLUMN yield_pct        numeric(6,3),
    ADD COLUMN density_g_per_ml numeric(10,4);
