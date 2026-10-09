-- The combos module's four capabilities (COMBOS_CONTRACT.md §2.1; ids 250-253
-- because the capability set is 256 bits wide). The boot sync
-- (`authz::sync_catalogue`) writes them too; the rows are here so every
-- database migrated without a boot (the test template, a restore) has them
-- before an org's role grants name them.
--   250 menu.combos.edit    owner
--   251 menu.deals.edit     owner
--   252 orders.deals.apply  owner, manager, teller (approval-capable)
--   253 reports.bundles     owner, manager
INSERT INTO capabilities (id, key, legacy_resource, legacy_action, tier, defaults, core, approval, protected) VALUES
    (250, 'menu.combos.edit',   NULL, NULL, 'configurable', 'o',   '', false, false),
    (251, 'menu.deals.edit',    NULL, NULL, 'configurable', 'o',   '', false, false),
    (252, 'orders.deals.apply', NULL, NULL, 'configurable', 'omt', '', true,  false),
    (253, 'reports.bundles',    NULL, NULL, 'configurable', 'om',  '', false, false)
ON CONFLICT (id) DO NOTHING;

-- System roles get the template default; nothing held these before.
INSERT INTO org_role_grants (org_role_id, org_id, capability_id, source, template_version)
SELECT r.id, r.org_id, c.id, 'template', 0
  FROM org_roles r
  JOIN capabilities c ON c.id IN (250, 251, 252, 253)
 WHERE r.is_system AND r.deleted_at IS NULL
   AND position(authz_kind_letter(r.kind::text) IN c.defaults) > 0
ON CONFLICT (org_role_id, capability_id) DO NOTHING;

SELECT authz_bump_epoch(o.id) FROM organizations o;
