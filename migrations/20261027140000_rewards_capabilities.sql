-- The Rewards dashboard's two capabilities (madar-shared v0.12.0). The boot
-- sync (`authz::sync_catalogue`) writes them too; the rows are here so every
-- database migrated without a boot (the test template, a restore) has them
-- before an org's role grants name them.
--   254 loyalty.card.design  owner, manager
--   255 loyalty.broadcast    owner, manager
INSERT INTO capabilities (id, key, legacy_resource, legacy_action, tier, defaults, core, approval, protected) VALUES
    (254, 'loyalty.card.design', NULL, NULL, 'configurable', 'om', '', false, false),
    (255, 'loyalty.broadcast',   NULL, NULL, 'configurable', 'om', '', false, false)
ON CONFLICT (id) DO NOTHING;

-- System roles get the template default; nothing held these before.
INSERT INTO org_role_grants (org_role_id, org_id, capability_id, source, template_version)
SELECT r.id, r.org_id, c.id, 'template', 0
  FROM org_roles r
  JOIN capabilities c ON c.id IN (254, 255)
 WHERE r.is_system AND r.deleted_at IS NULL
   AND position(authz_kind_letter(r.kind::text) IN c.defaults) > 0
ON CONFLICT (org_role_id, capability_id) DO NOTHING;

SELECT authz_bump_epoch(o.id) FROM organizations o;
