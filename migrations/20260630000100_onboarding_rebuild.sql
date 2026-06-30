-- ═══════════════════════════════════════════════════════════════════
-- Rebuilt organization onboarding state.
--
-- Single source of truth: a nullable completion timestamp.
--   NULL  → the owner hasn't finished (or dismissed) the first-run wizard
--   set   → the wizard is done; the dashboard never routes back into it
--
-- `onboarding_completed_at` already exists (it predates this rebuild and is
-- preserved by the teardown migration), so the ADD below is a no-op safety
-- net for any environment that somehow lacks it. Step-level progress is
-- never stored — it is derived live from data presence by
-- GET /orgs/{id}/onboarding, so the checklist can't drift from reality.
-- ═══════════════════════════════════════════════════════════════════

ALTER TABLE organizations
    ADD COLUMN IF NOT EXISTS onboarding_completed_at timestamptz;

-- Safety net: any org that has already taken orders is a live business, not
-- an onboarding candidate — make sure it's marked done. (No-op on existing
-- data, where completed orgs already carry a timestamp.)
UPDATE organizations o
SET onboarding_completed_at = now()
WHERE o.onboarding_completed_at IS NULL
  AND EXISTS (
      SELECT 1
      FROM orders ord
      JOIN branches b ON b.id = ord.branch_id
      WHERE b.org_id = o.id
  );
