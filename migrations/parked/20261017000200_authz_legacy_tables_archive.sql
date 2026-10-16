-- PARKED (DB audit 2026-10-16, Phase 4 teardown). NOT APPLIED: sqlx only reads
-- files directly in migrations/, never this subdirectory.
--
-- Moves the two legacy-only authz tables to the `archive` schema (data kept,
-- never dropped) and removes the mirror triggers that copied their writes into
-- architecture E. Move this file into migrations/ (renamed to a timestamp
-- after the newest applied migration) only when ALL of these hold:
--
--   1. 0 hits of client_seen kind `legacy_permissions_route` for 30
--      consecutive days across all orgs:
--        SELECT count(*) FROM client_seen
--         WHERE last_legacy_kind = 'legacy_permissions_route'
--           AND last_legacy_at > now() - interval '30 days';   -- must be 0
--      (or the per-day `compat_usage` counter once Phase 1 ships it; the
--      journald `madar.legacy` events carry the same kind).
--      and, in the same PR, the /permissions/* routes are retired (410 with a
--      code) or re-served from the single model (they are the last readers and
--      writers of these tables; no POS build ever called them).
--   2. `authz-reconcile` run READ-ONLY against live prod prints "0 differ",
--      with its TSV attached to the PR.
--   3. If MADAR_AUTHZ_MODE=compare was ever set in prod: no row in
--      authz_shadow_disagreements for the last 30 days.
--   4. The owner has reviewed the reconcile's `role_default` rows: after this
--      migration a legacy-template org created outside orgs::provision seeds
--      its roles from the capability spec, not the global table (on the
--      2026-09-05 prod copy the two differ only on inventory_adjustments:*,
--      which the global table grants owners and managers and the spec does
--      not).
--   5. The same PR deletes the code that names these tables (the macros then
--      prove nothing is left): permissions::checker::legacy_decision and the
--      `compare` mode, permissions::seeder, cache::{user_override,role_default},
--      authz::shadow::compare_all + src/bin/authz_parity.rs, the old side of
--      authz::reconcile, and the fixtures that call seed_role_permissions.
--
-- NOT covered here, on purpose: `users.role` and `user_branch_assignments`
-- and their mirror triggers (authz_users_owner, authz_users_sync,
-- authz_branch_assignment_sync). They are not legacy-only: the current users
-- and auth handlers still write them and old tills read `role`, so they are
-- the live write path into role_assignments until those handlers write the
-- new tables directly.
--
-- Owner sign-off: ______________________  date: __________

CREATE SCHEMA IF NOT EXISTS archive;
REVOKE ALL ON SCHEMA archive FROM PUBLIC;

-- The mirror: legacy writes into the new model.
DROP TRIGGER IF EXISTS authz_permissions_sync ON permissions;
DROP TRIGGER IF EXISTS authz_role_permissions_sync ON role_permissions;
DROP FUNCTION IF EXISTS authz_permissions_after();
DROP FUNCTION IF EXISTS authz_role_permissions_after();
DROP FUNCTION IF EXISTS authz_sync_override(uuid, text, text);

-- The changefeed re-emitted tellers on writes to these tables.
DROP TRIGGER IF EXISTS sync_emit ON permissions;
DROP TRIGGER IF EXISTS sync_emit ON role_permissions;
-- AT UNPARK TIME: redefine sync_source_tables() from its then-current
-- definition minus the ('permissions', ...) and ('role_permissions', ...)
-- rows, and drop sync_emit_permissions() / sync_emit_role_permissions() if
-- nothing else uses them. (Not written here: a copy made today would clobber
-- every row added to that function between now and the unpark.)

-- New legacy-template orgs seed their system roles from the spec.
CREATE OR REPLACE FUNCTION authz_ensure_org_roles(p_org uuid) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    k      text;
    v_role uuid;
    names  jsonb := '{"org_admin": ["Owner", "المالك"], "branch_manager": ["Branch manager", "مدير الفرع"],
                      "teller": ["Cashier", "كاشير"], "waiter": ["Waiter", "نادل"], "kitchen": ["Kitchen", "المطبخ"]}';
BEGIN
    FOREACH k IN ARRAY ARRAY['org_admin', 'branch_manager', 'teller', 'waiter', 'kitchen'] LOOP
        SELECT id INTO v_role FROM org_roles
         WHERE org_id = p_org AND key = k AND deleted_at IS NULL;
        IF v_role IS NULL THEN
            INSERT INTO org_roles (org_id, key, name_en, name_ar, kind, template_key, template_version, is_system)
            VALUES (p_org, k, names -> k ->> 0, names -> k ->> 1, k::user_role, 'legacy', 0, true)
            RETURNING id INTO v_role;
            INSERT INTO org_role_grants (org_role_id, org_id, capability_id, source, template_version)
            SELECT v_role, p_org, c.id, 'template', 0
              FROM capabilities c
             WHERE position(authz_kind_letter(k) IN c.defaults) > 0;
        END IF;
    END LOOP;
END $$;

-- Data kept, out of the app's schema.
ALTER TABLE permissions SET SCHEMA archive;
ALTER TABLE role_permissions SET SCHEMA archive;

-- Reconcile after applying (must match the counts taken before, per §10):
--   SELECT count(*), count(*) FILTER (WHERE granted) FROM archive.permissions;
--   SELECT role, count(*), count(*) FILTER (WHERE granted) FROM archive.role_permissions GROUP BY 1;
