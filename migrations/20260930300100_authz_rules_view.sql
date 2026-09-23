-- Dawam Phase B (rules), owner decision 2026-09-23: branch managers SEE the
-- attendance and pay rules, read-only — the business's rules and their own
-- branches' overrides. `hr.rules.view` (id 241, owner + branch manager);
-- editing stays on `hr.rules.edit` (236, owner). Shipped with its preset
-- grants (PM-2).
INSERT INTO capabilities (id, key, legacy_resource, legacy_action, tier, defaults, core, approval, protected)
VALUES (241, 'hr.rules.view', NULL, NULL, 'configurable', 'om', '', false, false)
ON CONFLICT (id) DO NOTHING;

INSERT INTO org_role_grants (org_role_id, org_id, capability_id, limits, source, template_version)
SELECT r.id, r.org_id, 241, '{}'::jsonb, 'template', 0
  FROM org_roles r
 WHERE r.is_system AND r.deleted_at IS NULL
   AND position(authz_kind_letter(r.kind::text) IN 'om') > 0
ON CONFLICT (org_role_id, capability_id) DO NOTHING;

SELECT authz_bump_epoch(o.id) FROM organizations o;
