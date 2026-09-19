-- Recording a staff drink is its own act: it is neither ringing a sale nor
-- voiding one, it spends a branch allowance, and the owner wants it on for
-- managers and off for tellers unless deliberately granted. Capability 223,
-- `orders.staff_drink.record` (selling, configurable, risk money, approval
-- offered so a teller without it can be unlocked by a manager's PIN).
--
-- Existing orgs' system roles get the same default the template gives new ones.
INSERT INTO capabilities (id, key, legacy_resource, legacy_action, tier, defaults, core, approval, protected) VALUES
    (223, 'orders.staff_drink.record', NULL, NULL, 'configurable', 'om', '', true, false)
ON CONFLICT (id) DO NOTHING;

INSERT INTO org_role_grants (org_role_id, org_id, capability_id, source, template_version)
SELECT r.id, r.org_id, c.id, 'template', 0
  FROM org_roles r
  JOIN capabilities c ON c.id = 223
 WHERE r.is_system AND r.deleted_at IS NULL
   AND position(authz_kind_letter(r.kind::text) IN c.defaults) > 0
ON CONFLICT (org_role_id, capability_id) DO NOTHING;

SELECT authz_bump_epoch(o.id) FROM organizations o;
