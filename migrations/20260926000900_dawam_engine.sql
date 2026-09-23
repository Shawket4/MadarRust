-- Dawam: modules, labour limits, coverage needs and the suggestion cache.
--
-- PS-2 / PS-7: an org's modules. Switching one off hides it; no row is
-- touched, so switching it back on restores everything. Every existing org is
-- a Madar (POS) org that already runs staff, so it starts with both.
ALTER TABLE organizations
    ADD COLUMN modules text[] NOT NULL DEFAULT '{pos,dawam}',
    ADD CONSTRAINT organizations_modules_chk
        CHECK (modules <@ '{pos,dawam}'::text[] AND cardinality(modules) > 0);

-- RU-13: labour limits. They warn, never block, and ship "unconfirmed" until
-- a lawyer signs them off (Law 14 of 2025 sources conflict). Hours.
ALTER TABLE attendance_settings
    ADD COLUMN limit_day_hours          numeric(4,1) NOT NULL DEFAULT 8,
    ADD COLUMN limit_week_hours         numeric(4,1) NOT NULL DEFAULT 48,
    ADD COLUMN limit_presence_hours     numeric(4,1) NOT NULL DEFAULT 10,
    ADD COLUMN limit_rest_hours         numeric(4,1) NOT NULL DEFAULT 12,
    ADD COLUMN limit_overtime_day_hours numeric(4,1) NOT NULL DEFAULT 2,
    -- Coverage derived from POS: one person per this many orders an hour.
    ADD COLUMN orders_per_staff         integer NOT NULL DEFAULT 12,
    ADD CONSTRAINT attendance_settings_limits_chk CHECK (
        limit_day_hours > 0 AND limit_week_hours > 0 AND limit_presence_hours > 0
        AND limit_rest_hours >= 0 AND limit_overtime_day_hours >= 0
        AND orders_per_staff > 0);

-- The weekly coverage grid (Dawam-only orgs type it; Madar orgs may too, and a
-- typed grid wins over the POS-derived one). Hour bands per weekday
-- (0 = Sunday, as in staff_schedules), optionally for one department.
CREATE TABLE staff_coverage_needs (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id        uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id     uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    day_of_week   smallint NOT NULL CHECK (day_of_week BETWEEN 0 AND 6),
    band_start    time NOT NULL,
    band_end      time NOT NULL,
    staff         smallint NOT NULL CHECK (staff BETWEEN 0 AND 200),
    department_id uuid REFERENCES departments(id) ON DELETE CASCADE,
    CHECK (band_end > band_start)
);
CREATE UNIQUE INDEX staff_coverage_needs_band_key ON staff_coverage_needs
    (branch_id, day_of_week, band_start,
     COALESCE(department_id, '00000000-0000-0000-0000-000000000000'::uuid));
ALTER TABLE staff_coverage_needs ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON staff_coverage_needs FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));

-- Learning keeps which side decided: the manager (accept/reject) or the
-- employee (claims, swaps, turning up). History is kept 24 months.
ALTER TABLE staff_suggestion_events
    ADD COLUMN work_shift_id uuid REFERENCES work_shifts(id) ON DELETE SET NULL;
CREATE INDEX staff_suggestion_events_branch_idx
    ON staff_suggestion_events (branch_id, created_at);

-- Precomputed suggestions (Wednesday 22:00 branch time for the week starting
-- Saturday). Any roster input changing drops the org's cache, so a stale
-- week is never served; the next read recomputes.
CREATE TABLE staff_suggestion_cache (
    org_id      uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id   uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    week_start  date NOT NULL,
    payload     jsonb NOT NULL,
    computed_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, week_start)
);
ALTER TABLE staff_suggestion_cache ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON staff_suggestion_cache FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));

CREATE FUNCTION dawam_drop_suggestion_cache() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM staff_suggestion_cache
     WHERE org_id = COALESCE(NEW.org_id, OLD.org_id);
    RETURN NULL;
END $$;

CREATE TRIGGER dawam_suggestion_cache AFTER INSERT OR UPDATE OR DELETE
    ON staff_schedules FOR EACH ROW EXECUTE FUNCTION dawam_drop_suggestion_cache();
CREATE TRIGGER dawam_suggestion_cache AFTER INSERT OR UPDATE OR DELETE
    ON staff_schedule_overrides FOR EACH ROW EXECUTE FUNCTION dawam_drop_suggestion_cache();
CREATE TRIGGER dawam_suggestion_cache AFTER INSERT OR UPDATE OR DELETE
    ON staff_requests FOR EACH ROW EXECUTE FUNCTION dawam_drop_suggestion_cache();
CREATE TRIGGER dawam_suggestion_cache AFTER INSERT OR UPDATE OR DELETE
    ON staff_profiles FOR EACH ROW EXECUTE FUNCTION dawam_drop_suggestion_cache();
CREATE TRIGGER dawam_suggestion_cache AFTER INSERT OR UPDATE OR DELETE
    ON staff_coverage_needs FOR EACH ROW EXECUTE FUNCTION dawam_drop_suggestion_cache();
CREATE TRIGGER dawam_suggestion_cache AFTER INSERT OR UPDATE OR DELETE
    ON attendance_settings FOR EACH ROW EXECUTE FUNCTION dawam_drop_suggestion_cache();
