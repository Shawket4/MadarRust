-- Merging two customers is its own capability (design §7). It rode on
-- customers.edit; but a merge now moves loyalty balances and retires a wallet
-- card, which is more than correcting a name. Capability 224,
-- `customers.merge`, default owner + manager.
INSERT INTO capabilities (id, key, legacy_resource, legacy_action, tier, defaults, core, approval, protected) VALUES
    (224, 'customers.merge', NULL, NULL, 'configurable', 'om', '', false, false)
ON CONFLICT (id) DO NOTHING;

-- System roles get the template default. Any other role that was deliberately
-- given customers.edit (211) could merge until now, and keeps that.
INSERT INTO org_role_grants (org_role_id, org_id, capability_id, source, template_version)
SELECT r.id, r.org_id, c.id, 'template', 0
  FROM org_roles r
  JOIN capabilities c ON c.id = 224
 WHERE r.is_system AND r.deleted_at IS NULL
   AND position(authz_kind_letter(r.kind::text) IN c.defaults) > 0
ON CONFLICT (org_role_id, capability_id) DO NOTHING;

INSERT INTO org_role_grants (org_role_id, org_id, capability_id, source, template_version)
SELECT g.org_role_id, g.org_id, 224, g.source, g.template_version
  FROM org_role_grants g
  JOIN org_roles r ON r.id = g.org_role_id
 WHERE g.capability_id = 211 AND NOT r.is_system AND r.deleted_at IS NULL
ON CONFLICT (org_role_id, capability_id) DO NOTHING;

SELECT authz_bump_epoch(o.id) FROM organizations o;
