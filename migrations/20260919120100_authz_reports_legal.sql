-- The legal reports (org tax, refunds / voids / discounts / waivers audits,
-- price overrides) name staff and expose money given back, so they are their
-- own capability, reports.legal (id 220). Default: owner and branch manager;
-- a manager sees only the branches they work at (authz::scope::org_read_branches).
-- Existing orgs' system roles get the same default the template gives new orgs.
INSERT INTO capabilities (id, key, legacy_resource, legacy_action, tier, defaults, core, approval, protected) VALUES
    (220, 'reports.legal', NULL, NULL, 'configurable', 'om', '', false, false)
ON CONFLICT (id) DO NOTHING;

INSERT INTO org_role_grants (org_role_id, org_id, capability_id, source, template_version)
SELECT r.id, r.org_id, c.id, 'template', 0
  FROM org_roles r
  JOIN capabilities c ON c.id = 220
 WHERE r.is_system AND r.deleted_at IS NULL
   AND position(authz_kind_letter(r.kind::text) IN c.defaults) > 0
ON CONFLICT (org_role_id, capability_id) DO NOTHING;

SELECT authz_bump_epoch(o.id) FROM organizations o;
