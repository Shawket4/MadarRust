-- Catalog depth: pack-size conversion + per-branch par levels.
--
-- pack_unit / pack_size: a named purchase pack and how many BASE STOCK units it
-- yields (e.g. a "case" = 24 pcs; a "sack" = 25000 g). Lets purchasing receive
-- in packs the supplier actually sells, beyond the built-in measure conversions
-- (g/kg/ml/l/pcs). NULL = no named pack (use the built-in unit conversions).
ALTER TABLE org_ingredients
    ADD COLUMN pack_unit text,
    ADD COLUMN pack_size numeric(12,4);

-- Per-branch min/max par levels for usage-based reordering:
--   par_min = reorder point   (order when on-hand drops to/below this)
--   par_max = order-up-to qty  (how much to bring stock back up to)
-- `reorder_threshold` is retained as the legacy reorder point; par_min takes
-- precedence when set. Both nullable (opt-in per ingredient/branch).
ALTER TABLE branch_inventory
    ADD COLUMN par_min numeric(12,3),
    ADD COLUMN par_max numeric(12,3);
