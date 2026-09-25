-- Hunt H2-B8b (SC-5, RO-6): as 20261003100200 did for a date, a standing
-- pattern row records where a business-wide block is worked (the board it
-- was set from), so a two-branch person's pattern set from branch B is
-- worked at B every week, not at their first branch. NULL = as before.
-- Written only for a business-wide block and one of the person's branches.
ALTER TABLE staff_schedules
    ADD COLUMN branch_id uuid REFERENCES branches(id) ON DELETE SET NULL;
COMMENT ON COLUMN staff_schedules.branch_id IS
    'Where a business-wide block is worked (one of the person''s branches); NULL = the block''s branch, else the person''s first.';

-- The one roster function (as 20261003100200), a pattern row's branch
-- resolved like a date's own.
CREATE OR REPLACE FUNCTION dawam_roster(
    p_employees      uuid[],
    p_from           date,
    p_to             date,
    p_tz             text DEFAULT NULL,
    p_with_overrides boolean DEFAULT true
) RETURNS TABLE (
    employee_id      uuid,
    on_date          date,
    work_shift_id    uuid,
    branch_id        uuid,
    tz               text,
    start_local      time,
    end_local        time,
    crosses_midnight boolean,
    times_edited     boolean,
    from_override    boolean,
    start_at         timestamptz,
    end_at           timestamptz
)
LANGUAGE sql STABLE AS $$
    WITH days AS (
        SELECT e.id AS employee_id, e.org_id, d::date AS on_date,
               EXTRACT(DOW FROM d)::smallint AS dow
          FROM employees e
         CROSS JOIN generate_series(p_from::timestamp, p_to::timestamp, INTERVAL '1 day') d
         WHERE e.id = ANY(p_employees)
    ),
    ov AS (
        SELECT o.employee_id, o.on_date, o.work_shift_id, o.start_time, o.end_time,
               o.branch_id
          FROM staff_schedule_overrides o
          JOIN days d ON d.employee_id = o.employee_id AND d.on_date = o.on_date
         WHERE p_with_overrides
    ),
    pat AS (
        -- One row per block and priority; the newest row's branch if two
        -- rows of the same kind name the same block.
        SELECT DISTINCT ON (d.employee_id, d.on_date, s.work_shift_id,
                            (s.day_of_week IS NULL))
               d.employee_id, d.on_date, s.work_shift_id,
               CASE WHEN s.day_of_week IS NOT NULL THEN 0 ELSE 1 END AS pri,
               s.branch_id AS p_branch
          FROM days d
          JOIN staff_schedules s
            ON s.employee_id = d.employee_id
           AND s.effective_from <= d.on_date
           AND (s.effective_to IS NULL OR s.effective_to >= d.on_date)
           AND (s.day_of_week IS NULL OR s.day_of_week = d.dow)
          JOIN work_shifts w ON w.id = s.work_shift_id AND w.is_active
                            AND d.dow = ANY(w.valid_days)
         WHERE NOT EXISTS (SELECT 1 FROM ov
                            WHERE ov.employee_id = d.employee_id AND ov.on_date = d.on_date)
         ORDER BY d.employee_id, d.on_date, s.work_shift_id, (s.day_of_week IS NULL),
                  s.effective_from DESC
    ),
    picked AS (
        SELECT p.employee_id, p.on_date, p.work_shift_id,
               NULL::time AS o_start, NULL::time AS o_end, false AS from_override,
               p.p_branch AS o_branch
          FROM pat p
         WHERE p.pri = (SELECT MIN(p2.pri) FROM pat p2
                         WHERE p2.employee_id = p.employee_id AND p2.on_date = p.on_date)
        UNION ALL
        SELECT ov.employee_id, ov.on_date, ov.work_shift_id, ov.start_time, ov.end_time, true,
               ov.branch_id
          FROM ov WHERE ov.work_shift_id IS NOT NULL
    ),
    eff AS (
        SELECT pk.employee_id, pk.on_date, pk.work_shift_id, pk.from_override,
               pk.o_start IS NOT NULL AS times_edited,
               COALESCE(pk.o_start, dt.start_time, w.start_time) AS s,
               COALESCE(pk.o_end, dt.end_time, w.end_time) AS e,
               COALESCE(w.branch_id, (
                   -- the date's own branch, while the person still works there
                   SELECT eb.branch_id FROM employee_branches eb
                     JOIN branches b ON b.id = eb.branch_id AND b.deleted_at IS NULL
                    WHERE eb.employee_id = pk.employee_id AND eb.branch_id = pk.o_branch), (
                   SELECT eb.branch_id FROM employee_branches eb
                     JOIN branches b ON b.id = eb.branch_id AND b.deleted_at IS NULL
                    WHERE eb.employee_id = pk.employee_id
                    ORDER BY eb.assigned_at, eb.branch_id LIMIT 1)) AS branch_id,
               w.org_id
          FROM picked pk
          JOIN work_shifts w ON w.id = pk.work_shift_id AND w.is_active
          LEFT JOIN work_shift_day_times dt
                 ON dt.work_shift_id = w.id
                AND dt.day_of_week = EXTRACT(DOW FROM pk.on_date)::smallint
    ),
    zoned AS (
        SELECT eff.*,
               COALESCE(p_tz, b.timezone::text, o.timezone::text, 'Africa/Cairo') AS zone
          FROM eff
          LEFT JOIN branches b ON b.id = eff.branch_id
          LEFT JOIN organizations o ON o.id = eff.org_id
    )
    SELECT z.employee_id, z.on_date, z.work_shift_id, z.branch_id, z.zone, z.s, z.e,
           z.e <= z.s, z.times_edited, z.from_override,
           (z.on_date + z.s) AT TIME ZONE z.zone,
           (z.on_date + z.e + CASE WHEN z.e <= z.s THEN INTERVAL '1 day'
                                   ELSE INTERVAL '0 day' END) AT TIME ZONE z.zone
      FROM zoned z
     ORDER BY z.employee_id, z.on_date, (z.on_date + z.s)
$$;
COMMENT ON FUNCTION dawam_roster(uuid[], date, date, text, boolean) IS
    'The one roster resolver (SC-6, AT-9): who works which shift when, where, at which effective times.';
