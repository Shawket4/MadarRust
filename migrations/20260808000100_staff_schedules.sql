-- Staff & attendance — part 2 of 5: work shifts and who works them.
--
-- NAMING: `shifts` in this codebase already means a TELLER CASH-DRAWER SESSION
-- (open/close, declared cash, discrepancy). HR schedules are `work_shifts`
-- everywhere — table, module, permission, UI copy. Never overload `shifts`.
--
-- Three layers resolve an employee's expected hours for a date, highest first:
--   1. `staff_schedule_overrides` — a specific date (NULL shift = day off).
--   2. `staff_schedules` with a matching `day_of_week`.
--   3. `staff_schedules` with `day_of_week IS NULL` (every day).
-- A weekday with no matching row is simply a rest day. Rotating patterns are
-- expressed as dated `effective_from`/`effective_to` bands of weekly rows.

CREATE TABLE work_shifts (
    id                          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id                      uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- NULL = an org-wide template usable at any branch.
    branch_id                   uuid REFERENCES branches(id) ON DELETE CASCADE,
    name                        text NOT NULL,
    start_time                  time NOT NULL,
    end_time                    time NOT NULL,
    -- Derived, never hand-set: an end at or before the start means the shift
    -- runs past midnight and checkout lands on the following calendar day.
    crosses_midnight            boolean GENERATED ALWAYS AS (end_time <= start_time) STORED,
    -- Minutes of tolerance after `start_time` before a check-in counts as late.
    grace_minutes               integer NOT NULL DEFAULT 15,
    break_minutes               integer NOT NULL DEFAULT 0,
    -- An unpaid break is subtracted from worked minutes.
    paid_break                  boolean NOT NULL DEFAULT true,
    -- Worked below this → 'half_day'. NULL = half the scheduled span.
    half_day_threshold_minutes  integer,
    -- Minutes past the scheduled end that must elapse before overtime accrues.
    overtime_threshold_minutes  integer NOT NULL DEFAULT 15,
    overtime_multiplier         numeric(4,2) NOT NULL DEFAULT 1.50,
    -- How early a check-in may be attributed to this shift. Also the window the
    -- multi-shift matcher uses to pick between two shifts on one day.
    checkin_window_minutes      integer NOT NULL DEFAULT 120,
    is_active                   boolean NOT NULL DEFAULT true,
    created_at                  timestamptz NOT NULL DEFAULT now(),
    updated_at                  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT work_shifts_name_len        CHECK (char_length(name) BETWEEN 1 AND 120),
    CONSTRAINT work_shifts_grace_nonneg    CHECK (grace_minutes              >= 0),
    CONSTRAINT work_shifts_break_nonneg    CHECK (break_minutes              >= 0),
    CONSTRAINT work_shifts_ot_nonneg       CHECK (overtime_threshold_minutes >= 0),
    CONSTRAINT work_shifts_window_pos      CHECK (checkin_window_minutes     >  0),
    CONSTRAINT work_shifts_multiplier_pos  CHECK (overtime_multiplier        >  0),
    CONSTRAINT work_shifts_halfday_pos
        CHECK (half_day_threshold_minutes IS NULL OR half_day_threshold_minutes > 0)
);

-- One shift name per org per branch. branch_id is nullable, so the NULL is
-- folded to the nil UUID rather than relying on NULLS NOT DISTINCT.
CREATE UNIQUE INDEX work_shifts_org_branch_name_key ON work_shifts (
    org_id,
    COALESCE(branch_id, '00000000-0000-0000-0000-000000000000'::uuid),
    lower(name)
);
CREATE INDEX work_shifts_branch_idx ON work_shifts (branch_id) WHERE branch_id IS NOT NULL;

CREATE TABLE staff_schedules (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id         uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id        uuid NOT NULL REFERENCES users(id)       ON DELETE CASCADE,
    work_shift_id  uuid NOT NULL REFERENCES work_shifts(id) ON DELETE CASCADE,
    -- Postgres EXTRACT(DOW) convention: 0 = Sunday … 6 = Saturday.
    -- NULL = this shift applies on every day of the week.
    day_of_week    smallint,
    effective_from date NOT NULL DEFAULT CURRENT_DATE,
    effective_to   date,
    created_at     timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT staff_schedules_dow_range
        CHECK (day_of_week IS NULL OR day_of_week BETWEEN 0 AND 6),
    CONSTRAINT staff_schedules_dates_ordered
        CHECK (effective_to IS NULL OR effective_to >= effective_from)
);

CREATE INDEX staff_schedules_user_idx  ON staff_schedules (user_id, effective_from DESC);
CREATE INDEX staff_schedules_shift_idx ON staff_schedules (work_shift_id);

CREATE TABLE staff_schedule_overrides (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id        uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id       uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    on_date       date NOT NULL,
    -- NULL = an explicit day off, which outranks any weekly row.
    work_shift_id uuid REFERENCES work_shifts(id) ON DELETE CASCADE,
    reason        text,
    created_by    uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX staff_schedule_overrides_user_date_key
    ON staff_schedule_overrides (user_id, on_date);

-- ── RLS ──────────────────────────────────────────────────────────────────────
ALTER TABLE work_shifts              ENABLE ROW LEVEL SECURITY;
ALTER TABLE staff_schedules          ENABLE ROW LEVEL SECURITY;
ALTER TABLE staff_schedule_overrides ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON work_shifts FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
CREATE POLICY tenant_isolation ON staff_schedules FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
CREATE POLICY tenant_isolation ON staff_schedule_overrides FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));

GRANT ALL ON TABLE work_shifts              TO sufrix;
GRANT ALL ON TABLE staff_schedules          TO sufrix;
GRANT ALL ON TABLE staff_schedule_overrides TO sufrix;
