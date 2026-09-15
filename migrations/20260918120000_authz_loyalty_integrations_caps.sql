-- Architecture E, phase 3: capabilities for the last role-name gates.
--   215 loyalty.members.list   (was: org_admin / branch_manager role check)
--   216 loyalty.points.adjust  (was: org_admin role check)
--   217 loyalty.members.delete (was: org_admin role check)
--   218 integrations.read      (was: require_org_admin)
-- The boot catalogue sync would add the rows too; they are inserted here so the
-- existing orgs' system roles get the same grants the role checks gave them.
INSERT INTO capabilities (id, key, legacy_resource, legacy_action, tier, defaults, core, approval, protected) VALUES
    (215, 'loyalty.members.list', NULL, NULL, 'configurable', 'om', '', false, false),
    (216, 'loyalty.points.adjust', NULL, NULL, 'advanced', 'o', '', false, false),
    (217, 'loyalty.members.delete', NULL, NULL, 'advanced', 'o', '', false, false),
    (218, 'integrations.read', NULL, NULL, 'advanced', 'o', '', false, false)
ON CONFLICT (id) DO NOTHING;

INSERT INTO org_role_grants (org_role_id, org_id, capability_id, source, template_version)
SELECT r.id, r.org_id, c.id, 'template', 0
  FROM org_roles r
  JOIN capabilities c ON c.id BETWEEN 215 AND 218
 WHERE r.is_system AND r.deleted_at IS NULL
   AND position(authz_kind_letter(r.kind::text) IN c.defaults) > 0
ON CONFLICT (org_role_id, capability_id) DO NOTHING;

SELECT authz_bump_epoch(o.id) FROM organizations o;
