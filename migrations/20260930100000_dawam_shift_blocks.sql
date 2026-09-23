-- Dawam Phase B · schedules (dawam-fix/phase-b/01-schedules.md).
--
-- 1. Day-scoped shift blocks: a block (work_shifts row) is valid on a set of
--    weekdays (default all) and may carry its own start/end per weekday on top
--    of its default times ("Evening" 16:00–00:00 Sat–Wed, 16:00–01:00 Thu–Fri).
-- 2. A date change is a SET of assignments: several override rows per
--    (employee, date), one per block, each optionally with its own from/to.
--    A NULL-shift row is the explicit day off and stands alone.
-- 3. The one roster function, `dawam_roster`, in SQL so every consumer (clock-in,
--    the absence sweep, team presence, the roster views, payroll) resolves the
--    same way: date change > weekday row > every-day row, block valid on the day,
--    effective times = assignment override > weekday time > block default, an
--    end at or before the start runs into the next day, in the branch's zone.
-- 4. Changed-after-publish per person and day (SC-4), whatever changed it.
-- 5. Preference changes are logged, by the employee or a manager (SC-12).
-- 6. Learning state and the monthly fairness audit per branch (SC-13 guardrails).
-- 7. Suggestion events say where they came from (a suggestion or a manual edit),
--    and a roster edit makes the kept week stale instead of dropping it
--    (recomputed at most once per 30 s per branch-week).
-- 8. A rejected cover's flag is recorded as rejected (CV-3).

-- ── 1. day-scoped blocks ────────────────────────────────────────────────────
ALTER TABLE work_shifts
    ADD COLUMN valid_days smallint[] NOT NULL DEFAULT '{0,1,2,3,4,5,6}',
    ADD CONSTRAINT work_shifts_valid_days_chk
        CHECK (cardinality(valid_days) BETWEEN 1 AND 7
               AND valid_days <@ '{0,1,2,3,4,5,6}'::smallint[]);
COMMENT ON COLUMN work_shifts.valid_days IS
    'Weekdays the block may be rostered on (0 = Sunday … 6 = Saturday).';

CREATE TABLE work_shift_day_times (
    work_shift_id uuid NOT NULL REFERENCES work_shifts(id) ON DELETE CASCADE,
    org_id        uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    day_of_week   smallint NOT NULL,
    start_time    time NOT NULL,
    end_time      time NOT NULL,
    PRIMARY KEY (work_shift_id, day_of_week),
    CONSTRAINT work_shift_day_times_dow_chk CHECK (day_of_week BETWEEN 0 AND 6),
    -- An end at or before the start runs into the next day; equal is no shift.
    CONSTRAINT work_shift_day_times_len_chk CHECK (start_time <> end_time)
);
ALTER TABLE work_shift_day_times ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON work_shift_day_times FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE work_shift_day_times TO madar_app;

-- ── 2. a date change holds several shifts ──────────────────────────────────
DROP INDEX staff_schedule_overrides_user_date_key;
CREATE UNIQUE INDEX staff_schedule_overrides_day_block_key
    ON staff_schedule_overrides (employee_id, on_date,
        COALESCE(work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid));
CREATE INDEX staff_schedule_overrides_day_idx ON staff_schedule_overrides (employee_id, on_date);

ALTER TABLE staff_schedule_overrides
    ADD COLUMN start_time time,
    ADD COLUMN end_time   time,
    ADD CONSTRAINT staff_schedule_overrides_times_chk CHECK (
        (start_time IS NULL AND end_time IS NULL)
        OR (start_time IS NOT NULL AND end_time IS NOT NULL
            AND start_time <> end_time AND work_shift_id IS NOT NULL));
COMMENT ON COLUMN staff_schedule_overrides.start_time IS
    'This one assignment''s own from/to (both or neither); the block is unchanged.';

-- A day off stands alone: a date holding shifts has no NULL row and vice versa.
CREATE FUNCTION dawam_override_day_shape() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    -- Checked at commit: a row inserted and removed again in one change is gone.
    IF NOT EXISTS (SELECT 1 FROM staff_schedule_overrides WHERE id = NEW.id) THEN
        RETURN NEW;
    END IF;
    IF EXISTS (SELECT 1 FROM staff_schedule_overrides o
                WHERE o.employee_id = NEW.employee_id AND o.on_date = NEW.on_date
                  AND o.id <> NEW.id
                  AND ((NEW.work_shift_id IS NULL) <> (o.work_shift_id IS NULL))) THEN
        RAISE EXCEPTION 'A day off can''t hold shifts'
            USING ERRCODE = '23514', CONSTRAINT = 'staff_schedule_overrides_day_shape';
    END IF;
    RETURN NEW;
END $$;
CREATE CONSTRAINT TRIGGER dawam_override_day_shape
    AFTER INSERT OR UPDATE ON staff_schedule_overrides
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION dawam_override_day_shape();

-- ── 3. the one roster function ─────────────────────────────────────────────
-- Every shift each employee is rostered for on each date in [p_from, p_to].
-- `p_tz` pins the zone (a caller that already knows the branch); NULL = the
-- block's branch, else the employee's first branch, else the org's.
-- `p_with_overrides = false` gives the standing pattern alone (what the pattern
-- intends, used as a coverage fallback by the suggestion engine).
CREATE FUNCTION dawam_roster(
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
        SELECT o.employee_id, o.on_date, o.work_shift_id, o.start_time, o.end_time
          FROM staff_schedule_overrides o
          JOIN days d ON d.employee_id = o.employee_id AND d.on_date = o.on_date
         WHERE p_with_overrides
    ),
    pat AS (
        SELECT DISTINCT d.employee_id, d.on_date, s.work_shift_id,
               CASE WHEN s.day_of_week IS NOT NULL THEN 0 ELSE 1 END AS pri
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
    ),
    picked AS (
        SELECT p.employee_id, p.on_date, p.work_shift_id,
               NULL::time AS o_start, NULL::time AS o_end, false AS from_override
          FROM pat p
         WHERE p.pri = (SELECT MIN(p2.pri) FROM pat p2
                         WHERE p2.employee_id = p.employee_id AND p2.on_date = p.on_date)
        UNION ALL
        SELECT ov.employee_id, ov.on_date, ov.work_shift_id, ov.start_time, ov.end_time, true
          FROM ov WHERE ov.work_shift_id IS NOT NULL
    ),
    eff AS (
        SELECT pk.employee_id, pk.on_date, pk.work_shift_id, pk.from_override,
               pk.o_start IS NOT NULL AS times_edited,
               COALESCE(pk.o_start, dt.start_time, w.start_time) AS s,
               COALESCE(pk.o_end, dt.end_time, w.end_time) AS e,
               COALESCE(w.branch_id, (
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
    'The one roster resolver (SC-6, AT-9): who works which shift when, at which effective times.';
GRANT EXECUTE ON FUNCTION dawam_roster(uuid[], date, date, text, boolean) TO madar_app;

-- ── 4. changed after publish, per person and day ───────────────────────────
CREATE TABLE staff_roster_changes (
    org_id      uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    employee_id uuid NOT NULL REFERENCES employees(id) ON DELETE CASCADE,
    on_date     date NOT NULL,
    changed_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (employee_id, on_date)
);
ALTER TABLE staff_roster_changes ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON staff_roster_changes FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE staff_roster_changes TO madar_app;
INSERT INTO staff_roster_changes (org_id, employee_id, on_date)
SELECT DISTINCT org_id, employee_id, on_date FROM staff_schedule_overrides
 WHERE changed_after_publish
ON CONFLICT DO NOTHING;

-- ── 5. preference changes, logged ──────────────────────────────────────────
ALTER TABLE employees ADD COLUMN prefs_set_by text NOT NULL DEFAULT 'employee',
    ADD CONSTRAINT employees_prefs_set_by_chk CHECK (prefs_set_by IN ('employee', 'manager'));
CREATE TABLE staff_preference_log (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id         uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    employee_id    uuid NOT NULL REFERENCES employees(id) ON DELETE CASCADE,
    source         text NOT NULL CHECK (source IN ('employee', 'manager')),
    pref_time      text,
    cant_work_days smallint[] NOT NULL DEFAULT '{}',
    changed_by     uuid REFERENCES users(id) ON DELETE SET NULL,
    note           text CHECK (note IS NULL OR char_length(note) <= 300),
    created_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX staff_preference_log_employee_idx ON staff_preference_log (employee_id, created_at DESC);
ALTER TABLE staff_preference_log ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON staff_preference_log FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE staff_preference_log TO madar_app;

-- ── 6. learning state + fairness audits per branch ─────────────────────────
CREATE TABLE staff_learning_state (
    branch_id  uuid PRIMARY KEY REFERENCES branches(id) ON DELETE CASCADE,
    org_id     uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    frozen     boolean NOT NULL,
    changed_at timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE staff_learning_state ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON staff_learning_state FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE staff_learning_state TO madar_app;

CREATE TABLE staff_fairness_audits (
    org_id      uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id   uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    month       date NOT NULL,
    flagged     boolean NOT NULL,
    payload     jsonb NOT NULL,
    computed_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, month)
);
ALTER TABLE staff_fairness_audits ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON staff_fairness_audits FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE staff_fairness_audits TO madar_app;

-- ── 7. suggestion events and cache ─────────────────────────────────────────
ALTER TABLE staff_suggestion_events
    ADD COLUMN source text NOT NULL DEFAULT 'suggestion',
    ADD CONSTRAINT staff_suggestion_events_source_chk CHECK (source IN ('suggestion', 'manual'));

ALTER TABLE staff_suggestion_cache ADD COLUMN stale boolean NOT NULL DEFAULT false;
-- Roster edits come in bursts (a manager working the grid): they mark the
-- kept week stale, and a stale week is recomputed at most once per 30 s.
-- A change of the engine's inputs (settings, blocks, people, coverage) still
-- drops it outright (dawam_drop_suggestion_cache, unchanged).
CREATE FUNCTION dawam_stale_suggestion_cache() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    UPDATE staff_suggestion_cache SET stale = true
     WHERE org_id = COALESCE(NEW.org_id, OLD.org_id) AND NOT stale;
    RETURN NULL;
END $$;
DROP TRIGGER dawam_suggestion_cache ON staff_schedules;
DROP TRIGGER dawam_suggestion_cache ON staff_schedule_overrides;
DROP TRIGGER dawam_suggestion_cache ON staff_requests;
CREATE TRIGGER dawam_suggestion_cache AFTER INSERT OR UPDATE OR DELETE
    ON staff_schedules FOR EACH ROW EXECUTE FUNCTION dawam_stale_suggestion_cache();
CREATE TRIGGER dawam_suggestion_cache AFTER INSERT OR UPDATE OR DELETE
    ON staff_schedule_overrides FOR EACH ROW EXECUTE FUNCTION dawam_stale_suggestion_cache();
CREATE TRIGGER dawam_suggestion_cache AFTER INSERT OR UPDATE OR DELETE
    ON staff_requests FOR EACH ROW EXECUTE FUNCTION dawam_stale_suggestion_cache();
-- The per-day times, the blocks and the people's preferences feed the engine too.
CREATE TRIGGER dawam_suggestion_cache AFTER INSERT OR UPDATE OR DELETE
    ON work_shift_day_times FOR EACH ROW EXECUTE FUNCTION dawam_drop_suggestion_cache();
DROP TRIGGER IF EXISTS dawam_suggestion_cache ON employees;
CREATE TRIGGER dawam_suggestion_cache
    AFTER UPDATE OF pref_time, cant_work_days, gender, employment_status, department_id
    ON employees FOR EACH ROW EXECUTE FUNCTION dawam_drop_suggestion_cache();
CREATE TRIGGER dawam_suggestion_cache AFTER INSERT OR UPDATE OR DELETE
    ON work_shifts FOR EACH ROW EXECUTE FUNCTION dawam_drop_suggestion_cache();

-- ── 8. a rejected cover's flag says rejected ───────────────────────────────
ALTER TABLE attendance_flags DROP CONSTRAINT attendance_flags_resolution_chk;
ALTER TABLE attendance_flags ADD CONSTRAINT attendance_flags_resolution_chk
    CHECK (resolution IS NULL OR resolution IN
        ('ignored', 'excused_paid', 'excused_unpaid', 'deducted', 'revoked', 'confirmed',
         'rejected'));
UPDATE attendance_flags f SET resolution = 'rejected'
  FROM attendance_records a
 WHERE f.attendance_record_id = a.id AND f.kind = 'cover'
   AND f.resolution = 'confirmed' AND a.cover_status = 'rejected';
