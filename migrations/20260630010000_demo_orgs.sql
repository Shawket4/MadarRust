-- ═══════════════════════════════════════════════════════════════════
-- Throwaway demo orgs (public playground).
--
-- Each demo visitor gets a fresh, isolated org (multi-tenancy is the
-- sandbox boundary). `is_demo` flags it; `demo_expires_at` is its TTL —
-- a background sweeper deletes expired demo orgs and all their child rows.
-- A partial index keeps the sweeper's "find expired" scan cheap and never
-- touches real (non-demo) orgs.
-- ═══════════════════════════════════════════════════════════════════

ALTER TABLE organizations
    ADD COLUMN IF NOT EXISTS is_demo         boolean NOT NULL DEFAULT false,
    ADD COLUMN IF NOT EXISTS demo_expires_at timestamptz;

CREATE INDEX IF NOT EXISTS idx_organizations_demo_expiry
    ON organizations (demo_expires_at)
    WHERE is_demo;
