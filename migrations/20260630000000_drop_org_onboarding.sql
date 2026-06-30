-- ═══════════════════════════════════════════════════════════════════
-- Teardown for the onboarding rebuild — ZERO DATA LOSS.
--
-- The rebuilt onboarding derives `completed` from the timestamp column,
-- so the separate boolean flag is now redundant and is removed. The
-- `onboarding_completed_at` column is deliberately KEPT (dropping it would
-- discard every org's completion record). In the old schema the timestamp
-- was always set whenever the boolean was true, so deriving
-- `completed = (onboarding_completed_at IS NOT NULL)` exactly reproduces
-- the previous state for every existing org.
--
-- No business data (orders, branches, menu, etc.) is touched.
-- ═══════════════════════════════════════════════════════════════════

ALTER TABLE organizations
    DROP COLUMN IF EXISTS onboarding_completed;
