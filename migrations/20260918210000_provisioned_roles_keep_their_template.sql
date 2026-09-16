-- Phase 7 provisioning: an org created from a template (orgs::provision) gets
-- system roles whose grants and limits come from that template, not from the
-- global legacy `role_permissions` table. A later edit of that global table
-- (the old role matrix) must not rewrite them, so the mirror now reaches only
-- system roles that still follow the legacy defaults.
CREATE OR REPLACE FUNCTION authz_role_permissions_after() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    r       role_permissions%ROWTYPE := CASE WHEN TG_OP = 'DELETE' THEN OLD ELSE NEW END;
    v_cap   smallint;
    v_grant boolean;
BEGIN
    SELECT id INTO v_cap FROM capabilities
     WHERE legacy_resource = r.resource::text AND legacy_action = r.action::text;
    IF v_cap IS NULL OR r.role = 'super_admin' THEN RETURN NULL; END IF;
    SELECT granted INTO v_grant FROM role_permissions
     WHERE role = r.role AND resource = r.resource AND action = r.action;
    IF COALESCE(v_grant, false) THEN
        INSERT INTO org_role_grants (org_role_id, org_id, capability_id, source, template_version)
        SELECT o.id, o.org_id, v_cap, 'template', 0 FROM org_roles o
         WHERE o.is_system AND o.key = r.role::text AND o.deleted_at IS NULL
           AND COALESCE(o.template_key, 'legacy') = 'legacy'
        ON CONFLICT DO NOTHING;
    ELSE
        DELETE FROM org_role_grants g USING org_roles o
         WHERE g.org_role_id = o.id AND o.is_system AND o.key = r.role::text
           AND COALESCE(o.template_key, 'legacy') = 'legacy'
           AND g.capability_id = v_cap AND g.source = 'template';
    END IF;
    UPDATE authz_epoch SET epoch = epoch + 1;
    RETURN NULL;
END $$;
