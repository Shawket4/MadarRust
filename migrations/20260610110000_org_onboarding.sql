-- ═══════════════════════════════════════════════════════════════════
-- Organization onboarding state.
--
-- `onboarding_completed` is the single source of truth the dashboard
-- reads to decide whether to route a fresh org into the setup wizard.
-- Step-level progress is NOT stored — it is derived live from data
-- presence by GET /orgs/{id}/onboarding, so the checklist can never
-- drift from reality (e.g. a deleted branch un-checks the step).
-- ═══════════════════════════════════════════════════════════════════

ALTER TABLE organizations
    ADD COLUMN IF NOT EXISTS onboarding_completed    boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS onboarding_completed_at timestamptz;

-- Existing orgs that have already taken orders are live businesses, not
-- onboarding candidates — never show them the wizard.
UPDATE organizations o
SET onboarding_completed    = true,
    onboarding_completed_at = now()
WHERE o.onboarding_completed = false
  AND EXISTS (
      SELECT 1
      FROM orders ord
      JOIN branches b ON b.id = ord.branch_id
      WHERE b.org_id = o.id
  );
