-- Org-level timezone + branch-tz inheritance.
--
-- Until now timezone lived only on branches (NOT NULL DEFAULT 'Africa/Cairo').
-- We add an org-level default and let a branch INHERIT it: branch.timezone NULL
-- means "use the org's timezone". Effective tz is resolved everywhere as
--   COALESCE(branch.timezone, org.timezone)
-- and org.timezone is itself NOT NULL DEFAULT 'Africa/Cairo', so the chain
-- branch -> org -> 'Africa/Cairo' always yields a valid IANA name.
--
-- IANA names are validated against pg_timezone_names in the application layer
-- (orgs/branches handlers) on every write, mirroring the existing branch check.

-- 1) Org-level timezone (NOT NULL with the historical default so every existing
--    org gets a sane value immediately).
ALTER TABLE organizations
    ADD COLUMN IF NOT EXISTS timezone text NOT NULL DEFAULT 'Africa/Cairo';

-- 2) Branch timezone becomes optional. Existing branches keep their explicit
--    'Africa/Cairo' (honored as-is); going forward, omitting a branch timezone
--    stores NULL = "inherit the org default".
ALTER TABLE branches
    ALTER COLUMN timezone DROP NOT NULL,
    ALTER COLUMN timezone DROP DEFAULT;
