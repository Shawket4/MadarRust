-- Seeing a customer's saved delivery addresses is its own capability (design
-- §7). A phone number finds a person; an address finds their door, so it does
-- not ride on `customers.view`. Capability 225, `customers.addresses.view`,
-- default owner + manager + teller: whoever dispatches a delivery needs it, a
-- waiter does not.
INSERT INTO capabilities (id, key, legacy_resource, legacy_action, tier, defaults, core, approval, protected) VALUES
    (225, 'customers.addresses.view', NULL, NULL, 'configurable', 'omt', '', false, false)
ON CONFLICT (id) DO NOTHING;

-- System roles get the template default. No custom role inherits it from
-- another grant: nothing exposed addresses before, so nobody "already had" it.
INSERT INTO org_role_grants (org_role_id, org_id, capability_id, source, template_version)
SELECT r.id, r.org_id, c.id, 'template', 0
  FROM org_roles r
  JOIN capabilities c ON c.id = 225
 WHERE r.is_system AND r.deleted_at IS NULL
   AND position(authz_kind_letter(r.kind::text) IN c.defaults) > 0
ON CONFLICT (org_role_id, capability_id) DO NOTHING;

SELECT authz_bump_epoch(o.id) FROM organizations o;
