-- E2E B-SETUP-4 (AV-5, PM-1): the advance limit (hr.advances.decide, 228) is
-- a max_percent in BASIS POINTS, like every percent limit the dashboard writes
-- (30% is stored as 3000). The branch manager's default of "half a month's
-- salary" was seeded as 50 — whole percent — which the dashboard shows as
-- 0.5% and which the server now reads as 0.5%.
--
-- 1. Grants still as the template/legacy seeding wrote them (source
--    'template': never edited by a person) move from whole percent to bp. A
--    grant an owner edited ('custom') was typed through the dashboard in bp
--    already and is left alone, as are per-person overrides.
UPDATE org_role_grants
   SET limits = jsonb_set(limits, '{max_percent}',
                          to_jsonb((limits->>'max_percent')::bigint * 100))
 WHERE capability_id = 228
   AND source = 'template'
   AND limits ? 'max_percent'
   AND (limits->>'max_percent')::bigint <= 100;

-- 2. The legacy role path seeds 5000 from now on. Same function as
--    20260930500200_dawam_deduction_limit_roles.sql otherwise.
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
                        WHEN k = 'branch_manager' AND c.id = 228 THEN '{"max_percent": 5000}'::jsonb
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

-- 3. Every org re-reads its grants.
SELECT authz_bump_epoch(o.id) FROM organizations o;
