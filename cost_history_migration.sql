--
-- Add ingredient_cost_history table
-- Run this against the live database ONCE.
--

-- ── 1. History table ────────────────────────────────────────────────────────
CREATE TABLE public.ingredient_cost_history (
    id                uuid          DEFAULT gen_random_uuid() NOT NULL PRIMARY KEY,
    org_ingredient_id uuid          NOT NULL REFERENCES public.org_ingredients(id) ON DELETE CASCADE,
    cost_per_unit     numeric(15,2) NOT NULL,
    effective_from    timestamptz   NOT NULL DEFAULT now(),
    effective_until   timestamptz,           -- NULL  → still the active cost
    changed_by        uuid          REFERENCES public.users(id) ON DELETE SET NULL,
    note              text
);

ALTER TABLE public.ingredient_cost_history OWNER TO rue;

-- Only one open row per ingredient allowed
CREATE UNIQUE INDEX idx_ich_active
    ON public.ingredient_cost_history (org_ingredient_id)
    WHERE (effective_until IS NULL);

CREATE INDEX idx_ich_ingredient
    ON public.ingredient_cost_history (org_ingredient_id, effective_from DESC);

-- ── 2. Seed backfill ────────────────────────────────────────────────────────
-- For every existing ingredient, seed one "open" history row dated to its
-- creation time so that any historical order created after that date can
-- resolve a cost.
INSERT INTO public.ingredient_cost_history
    (org_ingredient_id, cost_per_unit, effective_from, effective_until, note)
SELECT
    id,
    cost_per_unit,
    created_at,
    NULL,
    'Seeded from initial ingredient cost'
FROM public.org_ingredients
WHERE deleted_at IS NULL
ON CONFLICT DO NOTHING;
