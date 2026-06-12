-- ─────────────────────────────────────────────────────────────────────────
-- Teller PIN branch-scoping + branch geofencing
--
-- PRE-MIGRATION DIAGNOSTIC (run on production before applying):
--   SELECT org_id, LOWER(name) AS lower_name, COUNT(*)
--   FROM users
--   WHERE role = 'teller' AND deleted_at IS NULL
--   GROUP BY org_id, LOWER(name)
--   HAVING COUNT(*) > 1;
-- If any rows return, rename the duplicate tellers first.
-- ─────────────────────────────────────────────────────────────────────────

-- Geofencing columns on branches (all nullable — existing rows unaffected)
ALTER TABLE branches
    ADD COLUMN IF NOT EXISTS latitude          DOUBLE PRECISION,
    ADD COLUMN IF NOT EXISTS longitude         DOUBLE PRECISION,
    ADD COLUMN IF NOT EXISTS geo_radius_meters INTEGER DEFAULT 200;

-- One teller name (case-insensitive) per org. Eliminates cross-org PIN
-- collisions when combined with the branch-scoped login query.
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_teller_unique_name_per_org
    ON users (org_id, LOWER(name))
    WHERE role = 'teller' AND deleted_at IS NULL;

-- Supports the new branch-scoped PIN lookup join
CREATE INDEX IF NOT EXISTS idx_uba_branch_user
    ON user_branch_assignments (branch_id, user_id);

-- Supports resolve-branch geofence queries
CREATE INDEX IF NOT EXISTS idx_branches_org_geo
    ON branches (org_id)
    WHERE latitude IS NOT NULL AND longitude IS NOT NULL AND deleted_at IS NULL;
