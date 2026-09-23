-- Dawam Phase B (rules): requests, corrections and rules.
--
-- 1. Requests (RQ-8, RQ-9, B4, B5): a half-day leave says WHICH half is off;
--    a late arrival, early departure or excuse may name the shift it is for
--    (split days); an excuse may run past midnight (night shifts), stored with
--    `end_date` = the next day; a correction's check-out may be earlier on the
--    clock than its check-in (it is the next morning).
-- 2. Rules (RU-2, audit B2): a branch override row holds ONLY what the branch
--    changes — every other field is NULL and inherits the business's value, so
--    later business edits reach the branch and a one-field override never
--    carries an empty ladder.
-- 3. AT-7: a manager's status on an attendance day survives every automatic
--    re-derive (`status_overridden`), and a day a manager deleted is never
--    recreated by the absence sweep (`attendance_tombstones`).
--    A waiver can be undone with a reason (`unwaived_*`).
-- 4. RQ-7, B7: unpaid excused time (an unpaid excuse or early departure) is a
--    deduction of its own source, `excused_unpaid`.

-- ── 1. requests ─────────────────────────────────────────────────────────────
ALTER TABLE staff_requests
    ADD COLUMN leave_half text,
    ADD COLUMN work_shift_id uuid REFERENCES work_shifts(id) ON DELETE SET NULL;
ALTER TABLE staff_requests ADD CONSTRAINT staff_requests_leave_half_chk CHECK (
    leave_half IS NULL OR (leave_half IN ('first', 'second') AND is_half_day)
);
ALTER TABLE staff_requests ADD CONSTRAINT staff_requests_shift_kind_chk CHECK (
    work_shift_id IS NULL OR kind IN ('late_arrival', 'early_departure', 'excuse')
);
COMMENT ON COLUMN staff_requests.leave_half IS
    'Half-day leave: which half is off (first / second). NULL on older rows = the first half.';
COMMENT ON COLUMN staff_requests.work_shift_id IS
    'The shift a late arrival, early departure or excuse is for. NULL = whichever shift of the day the times fall in.';

-- An excuse whose end is at or before its start runs past midnight: its end is
-- on the next day (end_date = on_date + 1). A correction's times are two ends
-- of one punch on a night shift and are not ordered on the clock.
ALTER TABLE staff_requests DROP CONSTRAINT staff_requests_window_ordered;
ALTER TABLE staff_requests ADD CONSTRAINT staff_requests_window_ordered CHECK (
    from_time IS NULL OR to_time IS NULL OR kind = 'correction'
    OR (kind = 'excuse' AND end_date IS NOT NULL AND end_date = on_date + 1)
    OR to_time > from_time
);
ALTER TABLE staff_requests DROP CONSTRAINT staff_requests_shape_chk;
ALTER TABLE staff_requests ADD CONSTRAINT staff_requests_shape_chk CHECK (
    CASE kind
        WHEN 'leave' THEN end_date IS NOT NULL AND from_time IS NULL AND to_time IS NULL
            AND attendance_record_id IS NULL
        WHEN 'late_arrival' THEN to_time IS NOT NULL AND from_time IS NULL AND end_date IS NULL
            AND leave_type_id IS NULL AND attendance_record_id IS NULL
        WHEN 'early_departure' THEN from_time IS NOT NULL AND to_time IS NULL AND end_date IS NULL
            AND leave_type_id IS NULL AND attendance_record_id IS NULL
        WHEN 'excuse' THEN from_time IS NOT NULL AND to_time IS NOT NULL
            AND (end_date IS NULL OR end_date = on_date + 1)
            AND leave_type_id IS NULL AND attendance_record_id IS NULL
        WHEN 'mission' THEN title IS NOT NULL AND end_date IS NOT NULL AND leave_type_id IS NULL
            AND attendance_record_id IS NULL
        WHEN 'correction' THEN attendance_record_id IS NOT NULL
            AND (from_time IS NOT NULL OR to_time IS NOT NULL) AND end_date IS NULL
            AND leave_type_id IS NULL
        ELSE false
    END
);

-- ── 2. branch overrides inherit ─────────────────────────────────────────────
-- The business row keeps its NOT NULL values through the handler (every
-- business-level write COALESCEs to the stored value or a default); a branch
-- row stores NULL for "same as the business".
ALTER TABLE attendance_settings
    ALTER COLUMN late_deduction_tiers DROP NOT NULL,
    ALTER COLUMN absence_deduction_days DROP NOT NULL,
    ALTER COLUMN default_overtime_multiplier DROP NOT NULL,
    ALTER COLUMN auto_checkout_buffer_minutes DROP NOT NULL,
    ALTER COLUMN working_days_per_month DROP NOT NULL,
    ALTER COLUMN require_geofence DROP NOT NULL,
    ALTER COLUMN excused_time_paid_default DROP NOT NULL,
    ALTER COLUMN period_start_day DROP NOT NULL,
    ALTER COLUMN overtime_mode DROP NOT NULL,
    ALTER COLUMN overtime_day_multiplier DROP NOT NULL,
    ALTER COLUMN overtime_night_multiplier DROP NOT NULL,
    ALTER COLUMN holiday_multiplier DROP NOT NULL,
    ALTER COLUMN advance_cap_percent DROP NOT NULL,
    ALTER COLUMN half_day_leave_counts DROP NOT NULL,
    ALTER COLUMN night_start DROP NOT NULL,
    ALTER COLUMN night_end DROP NOT NULL,
    ALTER COLUMN gender_mode DROP NOT NULL,
    ALTER COLUMN limit_day_hours DROP NOT NULL,
    ALTER COLUMN limit_week_hours DROP NOT NULL,
    ALTER COLUMN limit_presence_hours DROP NOT NULL,
    ALTER COLUMN limit_rest_hours DROP NOT NULL,
    ALTER COLUMN limit_overtime_day_hours DROP NOT NULL,
    ALTER COLUMN orders_per_staff DROP NOT NULL;

-- The business row is always complete.
ALTER TABLE attendance_settings ADD CONSTRAINT attendance_settings_business_complete CHECK (
    branch_id IS NOT NULL OR (
        late_deduction_tiers IS NOT NULL AND absence_deduction_days IS NOT NULL
        AND default_overtime_multiplier IS NOT NULL AND auto_checkout_buffer_minutes IS NOT NULL
        AND working_days_per_month IS NOT NULL AND require_geofence IS NOT NULL
        AND excused_time_paid_default IS NOT NULL AND period_start_day IS NOT NULL
        AND overtime_mode IS NOT NULL AND overtime_day_multiplier IS NOT NULL
        AND overtime_night_multiplier IS NOT NULL AND holiday_multiplier IS NOT NULL
        AND advance_cap_percent IS NOT NULL AND half_day_leave_counts IS NOT NULL
        AND night_start IS NOT NULL AND night_end IS NOT NULL AND gender_mode IS NOT NULL
        AND limit_day_hours IS NOT NULL AND limit_week_hours IS NOT NULL
        AND limit_presence_hours IS NOT NULL AND limit_rest_hours IS NOT NULL
        AND limit_overtime_day_hours IS NOT NULL AND orders_per_staff IS NOT NULL
    )
);

-- Existing branch rows were full copies made from built-in defaults (B2).
-- Keep only what differs from the business; the business-only settings (pay
-- period, advance cap, gender mode) never belong to a branch. An empty ladder
-- under a business that has one is the B2 accident itself, not a choice.
UPDATE attendance_settings b SET
    late_deduction_tiers = CASE
        WHEN b.late_deduction_tiers = o.late_deduction_tiers THEN NULL
        WHEN b.late_deduction_tiers = '[]'::jsonb AND o.late_deduction_tiers <> '[]'::jsonb THEN NULL
        ELSE b.late_deduction_tiers END,
    absence_deduction_days = NULLIF(b.absence_deduction_days, o.absence_deduction_days),
    default_overtime_multiplier = NULLIF(b.default_overtime_multiplier, o.default_overtime_multiplier),
    auto_checkout_buffer_minutes = NULLIF(b.auto_checkout_buffer_minutes, o.auto_checkout_buffer_minutes),
    working_days_per_month = NULLIF(b.working_days_per_month, o.working_days_per_month),
    require_geofence = NULLIF(b.require_geofence, o.require_geofence),
    excused_time_paid_default = NULLIF(b.excused_time_paid_default, o.excused_time_paid_default),
    period_start_day = NULL,
    overtime_mode = NULLIF(b.overtime_mode, o.overtime_mode),
    overtime_day_multiplier = NULLIF(b.overtime_day_multiplier, o.overtime_day_multiplier),
    overtime_night_multiplier = NULLIF(b.overtime_night_multiplier, o.overtime_night_multiplier),
    holiday_multiplier = NULLIF(b.holiday_multiplier, o.holiday_multiplier),
    advance_cap_percent = NULL,
    half_day_leave_counts = NULLIF(b.half_day_leave_counts, o.half_day_leave_counts),
    night_start = NULLIF(b.night_start, o.night_start),
    night_end = NULLIF(b.night_end, o.night_end),
    gender_mode = NULL,
    limit_day_hours = NULLIF(b.limit_day_hours, o.limit_day_hours),
    limit_week_hours = NULLIF(b.limit_week_hours, o.limit_week_hours),
    limit_presence_hours = NULLIF(b.limit_presence_hours, o.limit_presence_hours),
    limit_rest_hours = NULLIF(b.limit_rest_hours, o.limit_rest_hours),
    limit_overtime_day_hours = NULLIF(b.limit_overtime_day_hours, o.limit_overtime_day_hours),
    orders_per_staff = NULLIF(b.orders_per_staff, o.orders_per_staff),
    rules_saved_at = NULL,
    updated_at = now()
  FROM attendance_settings o
 WHERE b.branch_id IS NOT NULL AND o.org_id = b.org_id AND o.branch_id IS NULL;

-- A branch row under a business with no row of its own: only its business-only
-- fields go (it still overrides the built-in defaults field by field).
UPDATE attendance_settings b SET period_start_day = NULL, advance_cap_percent = NULL,
       gender_mode = NULL, rules_saved_at = NULL
 WHERE b.branch_id IS NOT NULL
   AND NOT EXISTS (SELECT 1 FROM attendance_settings o
                    WHERE o.org_id = b.org_id AND o.branch_id IS NULL);

-- ── 3. AT-7 ────────────────────────────────────────────────────────────────
ALTER TABLE attendance_records
    ADD COLUMN status_overridden boolean NOT NULL DEFAULT false;
COMMENT ON COLUMN attendance_records.status_overridden IS
    'A manager set this day''s status by hand: no automatic re-derive (a request decision, the sweep) replaces it.';

-- A day a manager deleted, so the absence sweep never writes it back.
CREATE TABLE attendance_tombstones (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id        uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    employee_id   uuid NOT NULL REFERENCES employees(id),
    business_date date NOT NULL,
    work_shift_id uuid,
    deleted_by    uuid REFERENCES users(id) ON DELETE SET NULL,
    reason        text,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX attendance_tombstones_day_key ON attendance_tombstones
    (employee_id, business_date, COALESCE(work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid));
ALTER TABLE attendance_tombstones ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON attendance_tombstones FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT SELECT, INSERT, UPDATE, DELETE ON attendance_tombstones TO madar_app;
GRANT ALL ON TABLE attendance_tombstones TO sufrix;

-- The sweep's own rows (no punch, not manual, nobody's) are skipped where a
-- manager deleted the day. A trigger, not a clause in the sweep's query, so
-- every writer of automatic absences honours it.
CREATE FUNCTION attendance_respect_tombstone() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.check_in_at IS NULL AND NOT NEW.is_manual AND NEW.created_by IS NULL
       AND NEW.covered_employee_id IS NULL
       AND NEW.status IN ('absent', 'on_leave')
       AND EXISTS (
           SELECT 1 FROM attendance_tombstones t
            WHERE t.employee_id = NEW.employee_id
              AND t.business_date = NEW.business_date
              AND COALESCE(t.work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid)
                  = COALESCE(NEW.work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid))
    THEN
        RETURN NULL;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER attendance_respect_tombstone BEFORE INSERT ON attendance_records
    FOR EACH ROW EXECUTE FUNCTION attendance_respect_tombstone();

-- AT-7: a waiver can be undone, with who, when and why (AT-10).
ALTER TABLE payroll_deductions
    ADD COLUMN unwaived_at timestamptz,
    ADD COLUMN unwaived_by uuid REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN unwaive_reason text;
ALTER TABLE payroll_deductions ADD CONSTRAINT payroll_deductions_unwaive_reason_chk
    CHECK (unwaived_at IS NULL OR unwaive_reason IS NOT NULL);

-- ── 4. unpaid excused time ──────────────────────────────────────────────────
-- NOTE for merging: other Phase B branches may re-create this constraint too;
-- the merged list must hold every value.
ALTER TABLE payroll_deductions DROP CONSTRAINT payroll_deductions_source_chk;
ALTER TABLE payroll_deductions ADD CONSTRAINT payroll_deductions_source_chk CHECK (source IN
    ('manual', 'late_penalty', 'absence', 'left_mid_shift', 'unpaid_excuse', 'carry',
     'excused_unpaid'));
