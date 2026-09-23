-- Dawam Phase B (payroll): the advance cap as ONE server-side figure (AV-5,
-- AT-3). `salary × cap% ÷ 100`, rounded half away from zero (numeric round()),
-- read by every advance row so no client recomputes it in floating point.
CREATE OR REPLACE FUNCTION dawam_advance_cap(p_org uuid, p_salary bigint) RETURNS bigint
LANGUAGE sql STABLE AS $$
    SELECT COALESCE(
        (SELECT round(GREATEST(p_salary, 0)::numeric * s.advance_cap_percent / 100)::bigint
           FROM attendance_settings s
          WHERE s.org_id = p_org AND s.branch_id IS NULL
          LIMIT 1),
        round(GREATEST(p_salary, 0)::numeric * 50 / 100)::bigint)
$$;
GRANT EXECUTE ON FUNCTION dawam_advance_cap(uuid, bigint) TO madar_app;
