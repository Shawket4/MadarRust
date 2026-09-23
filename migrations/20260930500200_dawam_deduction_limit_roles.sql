-- Dawam Phase B (payroll, AD-5): an org set up by the legacy role path gives
-- its branch managers the same 1,000 EGP ceiling on DEDUCTIONS (245) as on
-- bonuses (227) and overtime approvals (231). Same function as before
-- otherwise (20260926000100_dawam_manager_limits.sql).
CREATE OR REPLACE FUNCTION public.authz_ensure_org_roles(p_org uuid)
 RETURNS void
 LANGUAGE plpgsql
 SECURITY DEFINER
 SET search_path TO 'public'
AS $function$
DECLARE
    k      text;
    v_role uuid;
    names  jsonb := '{"org_admin": ["Owner", "المالك"], "branch_manager": ["Branch manager", "مدير الفرع"],
                      "teller": ["Cashier", "كاشير"], "waiter": ["Waiter", "نادل"], "kitchen": ["Kitchen", "المطبخ"]}';
BEGIN
    FOREACH k IN ARRAY ARRAY['org_admin', 'branch_manager', 'teller', 'waiter', 'kitchen'] LOOP
        SELECT id INTO v_role FROM org_roles
         WHERE org_id = p_org AND key = k AND deleted_at IS NULL;
        IF v_role IS NULL THEN
            INSERT INTO org_roles (org_id, key, name_en, name_ar, kind, template_key, template_version, is_system)
            VALUES (p_org, k, names -> k ->> 0, names -> k ->> 1, k::user_role, 'legacy', 0, true)
            RETURNING id INTO v_role;
            INSERT INTO org_role_grants (org_role_id, org_id, capability_id, limits, source, template_version)
            SELECT v_role, p_org, c.id,
                   CASE WHEN k = 'branch_manager' AND c.id IN (227, 231, 245) THEN '{"max_amount": 100000}'::jsonb
                        WHEN k = 'branch_manager' AND c.id = 228 THEN '{"max_percent": 50}'::jsonb
                        ELSE '{}'::jsonb END,
                   'template', 0
              FROM capabilities c
             WHERE (c.legacy_resource IS NOT NULL AND EXISTS (
                        SELECT 1 FROM role_permissions rp
                         WHERE rp.role = k::user_role AND rp.granted
                           AND rp.resource::text = c.legacy_resource
                           AND rp.action::text = c.legacy_action))
                OR (c.legacy_resource IS NULL AND position(authz_kind_letter(k) IN c.defaults) > 0);
        END IF;
    END LOOP;
END $function$;
