-- Deferred feature 5 (keep the previous teller's queue): resuming a held order
-- someone else started is its own capability, granted by default to the owner
-- (org_admin) and branch managers. A teller or waiter without it asks a manager
-- to approve with their PIN on the till.
INSERT INTO capabilities (id, key, legacy_resource, legacy_action, tier, defaults, core, approval, protected) VALUES
    (222, 'orders.held.resume_others', NULL, NULL, 'configurable', 'om', '', true, false)
ON CONFLICT (id) DO NOTHING;

INSERT INTO org_role_grants (org_role_id, org_id, capability_id, source, template_version)
SELECT r.id, r.org_id, 222, 'template', 0
  FROM org_roles r
 WHERE r.is_system AND r.deleted_at IS NULL
   AND r.kind::text IN ('org_admin', 'branch_manager')
ON CONFLICT (org_role_id, capability_id) DO NOTHING;

SELECT authz_bump_epoch(o.id) FROM organizations o;
