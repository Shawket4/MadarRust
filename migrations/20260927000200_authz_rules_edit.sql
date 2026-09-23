-- Dawam Phase A (RO-6, RO-9, audit B2): the business-wide attendance and pay
-- rules (lateness ladder, absence cost, working days, overtime, the pay period,
-- the advance cap) were editable by anyone holding `hr.attendance.edit`, which
-- every branch manager does. They are the owner's: a new capability, held at
-- every branch, shipped with its preset grant (PM-2). Id 236 of the reserve
-- (PM-3: 237-255 stay free).
INSERT INTO capabilities (id, key, legacy_resource, legacy_action, tier, defaults, core, approval, protected)
VALUES (236, 'hr.rules.edit', NULL, NULL, 'configurable', 'o', '', false, true)
ON CONFLICT (id) DO NOTHING;

-- Every org's system Owner role holds it, like the template default.
INSERT INTO org_role_grants (org_role_id, org_id, capability_id, limits, source, template_version)
SELECT r.id, r.org_id, 236, '{}'::jsonb, 'template', 0
  FROM org_roles r
 WHERE r.is_system AND r.deleted_at IS NULL
   AND position(authz_kind_letter(r.kind::text) IN 'o') > 0
ON CONFLICT (org_role_id, capability_id) DO NOTHING;

-- RO-1, RO-4: a branch manager adds people and signs a phone out, at their own
-- branches (branch scope limits both; salary stays behind hr.payroll.edit).
-- hr.staff.create (137) and hr.staff.edit (139) join the manager preset.
UPDATE capabilities SET defaults = 'om' WHERE id IN (137, 139);
INSERT INTO role_permissions (role, resource, action, granted)
VALUES ('branch_manager', 'staff', 'create', true), ('branch_manager', 'staff', 'update', true)
ON CONFLICT (role, resource, action) DO UPDATE SET granted = true;
INSERT INTO org_role_grants (org_role_id, org_id, capability_id, limits, source, template_version)
SELECT r.id, r.org_id, c, '{}'::jsonb, 'template', 0
  FROM org_roles r CROSS JOIN (VALUES (137), (139)) AS caps(c)
 WHERE r.is_system AND r.deleted_at IS NULL AND r.kind::text = 'branch_manager'
ON CONFLICT (org_role_id, capability_id) DO NOTHING;

SELECT authz_bump_epoch(o.id) FROM organizations o;
