-- ═══════════════════════════════════════════════════════════════════
-- Per-branch ingredient costing.
--
-- Until now a single `org_ingredients.cost_per_unit` (piastres) was the cost
-- everywhere, and receiving a purchase blended a weighted-average straight onto
-- that org-wide column — so a receipt at one branch silently re-priced every
-- other branch's COGS, margins and inventory valuation.
--
-- New model (standard-cost vs actual-WAC):
--   * org_ingredients.cost_per_unit  → the ORG DEFAULT / standard cost. Set by
--     the catalog editor. Used as the fallback when a branch has no actual cost
--     yet, and for genuinely org-wide rollups that have no branch context.
--   * branch_inventory.cost_per_unit → the BRANCH's actual moving-average cost
--     (piastres/stock unit). Moved only by that branch's receipts. NULL ⇒ no
--     branch cost yet → fall back to the org default.
--
-- Cost is resolved as: branch actual → org default (NULL = unknown, never 0).
-- Everything additive and data-preserving; safe to run on production.
-- ═══════════════════════════════════════════════════════════════════

-- ── 1. Branch-level actual cost ─────────────────────────────────────
ALTER TABLE branch_inventory
    ADD COLUMN cost_per_unit numeric(15,2);

-- Seed every existing branch row from the current org cost, so valuation/COGS
-- are unchanged on day one. Each branch's cost then moves independently as it
-- receives stock. NULL org costs (unknown) stay NULL (unknown) per branch.
UPDATE branch_inventory bi
SET cost_per_unit = oi.cost_per_unit
FROM org_ingredients oi
WHERE oi.id = bi.org_ingredient_id;

-- ── 2. Branch-scope the cost-history epochs ─────────────────────────
-- NULL branch_id  = org-level (standard) epoch — existing rows + catalog edits.
-- non-null        = a branch's actual-WAC epoch, written on receipt.
-- Point-in-time resolution: branch epoch covering `at` → org epoch → org default.
ALTER TABLE ingredient_cost_history
    ADD COLUMN branch_id uuid REFERENCES branches(id) ON DELETE CASCADE;

CREATE INDEX idx_ingredient_cost_history_branch
    ON ingredient_cost_history (org_ingredient_id, branch_id, effective_from DESC);

-- At most one OPEN epoch per scope. Closes the "two open epochs silently
-- double-count in cost rollups" gap (audit). NULLs compare distinct in a plain
-- unique index, so the org-level and branch-level scopes get one partial index
-- each.
CREATE UNIQUE INDEX idx_ich_one_open_org
    ON ingredient_cost_history (org_ingredient_id)
    WHERE effective_until IS NULL AND branch_id IS NULL;

CREATE UNIQUE INDEX idx_ich_one_open_branch
    ON ingredient_cost_history (org_ingredient_id, branch_id)
    WHERE effective_until IS NULL AND branch_id IS NOT NULL;
