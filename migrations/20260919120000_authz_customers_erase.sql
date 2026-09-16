-- Owner decision 2026-09-16: erasing a customer's personal data (PDPL) is its
-- own capability, granted by default to the owner (org_admin) only, never to a
-- branch manager. Before this, erase rode on customers.edit.
INSERT INTO capabilities (id, key, legacy_resource, legacy_action, tier, defaults, core, approval, protected) VALUES
    (219, 'customers.erase', NULL, NULL, 'advanced', 'o', '', false, false)
ON CONFLICT (id) DO NOTHING;

INSERT INTO org_role_grants (org_role_id, org_id, capability_id, source, template_version)
SELECT r.id, r.org_id, 219, 'template', 0
  FROM org_roles r
 WHERE r.is_system AND r.deleted_at IS NULL
   AND r.kind::text = 'org_admin'
ON CONFLICT (org_role_id, capability_id) DO NOTHING;

SELECT authz_bump_epoch(o.id) FROM organizations o;
