-- ============================================================================
-- PROD DATA CLEANUP: permissions Phase 0 (ORG_ONBOARDING_PERMISSIONS_AUDIT §5.2)
--
-- STATUS: FOR OWNER REVIEW. NOT RUN ANYWHERE. This file is deliberately NOT in
-- migrations/ so a deploy never applies it.
--
-- How to run (after review, on a prod COPY first):
--   createdb -T madar_prodcopy madar_cleanup_rehearsal
--   psql -d madar_cleanup_rehearsal -v ON_ERROR_STOP=1 -f 2026-09-permissions-phase0.sql
--   (read the report it prints; it ends in ROLLBACK)
--   On prod: change the final ROLLBACK to COMMIT, take a pg_dump first.
--
-- Every change is archived before it is made, into schema `archive`, so it can
-- be put back by hand.
-- ============================================================================

\set ON_ERROR_STOP on
BEGIN;

CREATE SCHEMA IF NOT EXISTS archive;

-- ── 1. Overrides that belong to soft-deleted users (audit S4: 180 rows) ──────
-- They come back if the user is restored. Archive, then delete.
CREATE TABLE IF NOT EXISTS archive.permissions_deleted_users (LIKE permissions INCLUDING ALL);
ALTER TABLE archive.permissions_deleted_users ADD COLUMN IF NOT EXISTS archived_at timestamptz DEFAULT now();

INSERT INTO archive.permissions_deleted_users
SELECT p.*, now() FROM permissions p
  JOIN users u ON u.id = p.user_id
 WHERE u.deleted_at IS NOT NULL;

DELETE FROM permissions p USING users u
 WHERE u.id = p.user_id AND u.deleted_at IS NOT NULL;

-- ── 2. Admin powers granted to till accounts (audit S1/S4: Rue teller "Test") ─
-- Any GRANTED override on orgs / users / permissions for a teller, waiter or
-- kitchen account is removed (archived first). Those roles never hold them by
-- default, so removing the override restores the role default (denied).
CREATE TABLE IF NOT EXISTS archive.permissions_admin_on_floor_roles (LIKE permissions INCLUDING ALL);
ALTER TABLE archive.permissions_admin_on_floor_roles ADD COLUMN IF NOT EXISTS archived_at timestamptz DEFAULT now();

INSERT INTO archive.permissions_admin_on_floor_roles
SELECT p.*, now() FROM permissions p
  JOIN users u ON u.id = p.user_id
 WHERE u.role IN ('teller', 'waiter', 'kitchen')
   AND p.granted
   AND p.resource IN ('orgs', 'users', 'permissions');

DELETE FROM permissions p USING users u
 WHERE u.id = p.user_id
   AND u.role IN ('teller', 'waiter', 'kitchen')
   AND p.granted
   AND p.resource IN ('orgs', 'users', 'permissions');

-- ── 3. REVIEW ONLY: everything else unusual, printed, not changed ────────────
\echo '--- Remaining overrides per active user (review; Rue teller "Test" had 64) ---'
SELECT o.name AS org, u.name AS user_name, u.role, count(*) AS overrides,
       count(*) FILTER (WHERE p.granted) AS allows,
       count(*) FILTER (WHERE NOT p.granted) AS denies
  FROM permissions p JOIN users u ON u.id = p.user_id
  JOIN organizations o ON o.id = u.org_id
 WHERE u.deleted_at IS NULL
 GROUP BY o.name, u.name, u.role
 ORDER BY overrides DESC;

\echo '--- Branch managers denied till work (Rue "One Ninety" had 24 denies) ---'
SELECT o.name AS org, u.name, p.resource::text || ':' || p.action::text AS denied
  FROM permissions p JOIN users u ON u.id = p.user_id
  JOIN organizations o ON o.id = u.org_id
 WHERE u.role = 'branch_manager' AND NOT p.granted AND u.deleted_at IS NULL
 ORDER BY o.name, u.name, denied;

\echo '--- Active staff with no branch assignment (Rue had 10) ---'
SELECT o.name AS org, u.name, u.role
  FROM users u JOIN organizations o ON o.id = u.org_id
 WHERE u.deleted_at IS NULL AND NOT u.is_guest_principal
   AND u.role IN ('branch_manager', 'teller', 'waiter', 'kitchen')
   AND NOT EXISTS (SELECT 1 FROM user_branch_assignments a WHERE a.user_id = u.id)
 ORDER BY o.name, u.name;

-- ── 4. The fake "mixed" tender (audit B3) ────────────────────────────────────
-- `mixed` is the label a split sale carries, not a way to pay. Deactivating the
-- method row changes no historical order (orders keep their text label) and
-- stops tills offering "Mixed" as a tender. Archived first.
CREATE TABLE IF NOT EXISTS archive.org_payment_methods_mixed (LIKE org_payment_methods INCLUDING ALL);
INSERT INTO archive.org_payment_methods_mixed
SELECT * FROM org_payment_methods WHERE name = 'mixed' AND is_active;

UPDATE org_payment_methods SET is_active = false, updated_at = now()
 WHERE name = 'mixed' AND is_active;

\echo '--- NOT CHANGED, owner decision: talabat_cash counted as drawer cash ---'
SELECT o.name AS org, m.name, m.is_cash, m.is_active
  FROM org_payment_methods m JOIN organizations o ON o.id = m.org_id
 WHERE m.name IN ('talabat_cash', 'talabat_online')
 ORDER BY o.name, m.name;

-- ── 5. Dead role-default rows (audit §3.2: 18 rows, dead resource labels) ────
CREATE TABLE IF NOT EXISTS archive.role_permissions_dead (LIKE role_permissions INCLUDING ALL);
INSERT INTO archive.role_permissions_dead
SELECT * FROM role_permissions WHERE resource IN ('reservations');
DELETE FROM role_permissions WHERE resource IN ('reservations');
-- `inventory_adjustments` is still a listed resource; left as is.

-- ── 6. Missing swap ingredient categories (audit B5), additive ───────────────
INSERT INTO ingredient_categories (org_id, slug, name, sort_order)
SELECT o.id, c.slug, c.name, c.ord
  FROM organizations o
 CROSS JOIN (VALUES ('milk', 'Milk', 10), ('coffee_bean', 'Coffee beans', 11)) AS c(slug, name, ord)
 WHERE o.deleted_at IS NULL
   AND NOT EXISTS (SELECT 1 FROM ingredient_categories ic WHERE ic.org_id = o.id AND ic.slug = c.slug);

\echo '--- Summary of archived rows ---'
SELECT 'overrides of deleted users' AS what, count(*) FROM archive.permissions_deleted_users
UNION ALL SELECT 'admin overrides on floor roles', count(*) FROM archive.permissions_admin_on_floor_roles
UNION ALL SELECT 'mixed tenders deactivated', count(*) FROM archive.org_payment_methods_mixed
UNION ALL SELECT 'dead role rows', count(*) FROM archive.role_permissions_dead;

-- Change to COMMIT only on the reviewed run.
ROLLBACK;
