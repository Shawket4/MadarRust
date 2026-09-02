-- Staff & attendance — part 3 of 5: the attendance ledger.
--
-- One row per (employee, business day, work shift). The BUSINESS DATE is always
-- derived server-side from the check-in instant `AT TIME ZONE` the branch's
-- effective timezone (branch → org → Africa/Cairo), exactly like orders and
-- shift reports — never from the device's clock or zone.
--
-- Geofencing reuses the columns already on `branches` (latitude, longitude,
-- geo_radius_meters, added in 20260612000000_teller_pin_branch_scope.sql). The
-- SERVER decides whether a check-in is inside the fence; the client only
-- supplies coordinates. The measured distance is stored so a disputed record can
-- be audited after the fact.

CREATE TABLE attendance_settings (
    id                           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id                       uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- NULL = the org-wide default; a branch row overrides it for that branch.
    branch_id                    uuid REFERENCES branches(id) ON DELETE CASCADE,
    -- Ordered tiers, each: {"from_minutes":n, "to_minutes":n|null,
    --                       "kind":"minutes"|"piastres"|"day_fraction", "value":n}
    -- Validated in Rust (src/staff/rules.rs), stored as jsonb so operators can
    -- add tiers without a migration.
    late_deduction_tiers         jsonb   NOT NULL DEFAULT '[]'::jsonb,
    -- Days of pay docked per unexcused absent day.
    absence_deduction_days       numeric(5,2) NOT NULL DEFAULT 1.00,
    -- Fallback when a work shift does not set its own.
    default_overtime_multiplier  numeric(4,2) NOT NULL DEFAULT 1.50,
    -- A record left open is auto-closed at scheduled end + this buffer. Per
    -- AttendUX semantics an auto-close accrues NO overtime.
    auto_checkout_buffer_minutes integer NOT NULL DEFAULT 120,
    -- EXTRACT(DOW) values that are rest days by default. Egypt: Fri + Sat.
    weekend_days                 smallint[] NOT NULL DEFAULT ARRAY[5,6]::smallint[],
    -- Divisor turning a monthly salary into a daily rate.
    working_days_per_month       numeric(5,2) NOT NULL DEFAULT 30.00,
    -- When false, a mobile check-in is accepted without a coordinate match
    -- (small orgs whose branch coordinates are not set up yet).
    require_geofence             boolean NOT NULL DEFAULT true,
    created_at                   timestamptz NOT NULL DEFAULT now(),
    updated_at                   timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT attendance_settings_tiers_is_array CHECK (jsonb_typeof(late_deduction_tiers) = 'array'),
    CONSTRAINT attendance_settings_absence_nonneg CHECK (absence_deduction_days      >= 0),
    CONSTRAINT attendance_settings_mult_pos       CHECK (default_overtime_multiplier >  0),
    CONSTRAINT attendance_settings_buffer_nonneg  CHECK (auto_checkout_buffer_minutes >= 0),
    CONSTRAINT attendance_settings_days_pos       CHECK (working_days_per_month      >  0)
);

CREATE UNIQUE INDEX attendance_settings_scope_key ON attendance_settings (
    org_id,
    COALESCE(branch_id, '00000000-0000-0000-0000-000000000000'::uuid)
);

CREATE TABLE attendance_records (
    id                        uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id                    uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id                   uuid NOT NULL REFERENCES users(id)    ON DELETE CASCADE,
    branch_id                 uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    -- NULL when the employee had no scheduled shift (an unscheduled walk-in day,
    -- or a manual record an admin created outside any roster).
    work_shift_id             uuid REFERENCES work_shifts(id) ON DELETE SET NULL,
    business_date             date NOT NULL,
    status                    text NOT NULL DEFAULT 'present',

    -- Snapshot of the expected window at the time the record was opened. Kept on
    -- the row so editing a work shift later never silently rewrites history.
    scheduled_start_at        timestamptz,
    scheduled_end_at          timestamptz,

    check_in_at               timestamptz,
    check_in_latitude         double precision,
    check_in_longitude        double precision,
    check_in_distance_meters  double precision,
    check_in_method           text,

    check_out_at              timestamptz,
    check_out_latitude        double precision,
    check_out_longitude       double precision,
    check_out_distance_meters double precision,
    check_out_method          text,

    late_minutes              integer NOT NULL DEFAULT 0,
    early_leave_minutes       integer NOT NULL DEFAULT 0,
    overtime_minutes          integer NOT NULL DEFAULT 0,
    worked_minutes            integer NOT NULL DEFAULT 0,

    -- True for anything an admin typed rather than an employee clocking in.
    is_manual                 boolean NOT NULL DEFAULT false,
    notes                     text,
    edit_reason               text,
    created_by                uuid REFERENCES users(id) ON DELETE SET NULL,
    edited_by                 uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at                timestamptz NOT NULL DEFAULT now(),
    updated_at                timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT attendance_records_status_chk
        CHECK (status IN ('present', 'late', 'absent', 'half_day', 'on_leave')),
    -- 'auto' is the nightly job closing a forgotten checkout.
    CONSTRAINT attendance_records_in_method_chk
        CHECK (check_in_method  IS NULL OR check_in_method  IN ('mobile_gps', 'manual', 'auto')),
    CONSTRAINT attendance_records_out_method_chk
        CHECK (check_out_method IS NULL OR check_out_method IN ('mobile_gps', 'manual', 'auto')),
    CONSTRAINT attendance_records_order_chk
        CHECK (check_out_at IS NULL OR check_in_at IS NULL OR check_out_at >= check_in_at),
    CONSTRAINT attendance_records_minutes_nonneg CHECK (
        late_minutes >= 0 AND early_leave_minutes >= 0
        AND overtime_minutes >= 0 AND worked_minutes >= 0
    ),
    -- A checkout without a check-in is never a legitimate state.
    CONSTRAINT attendance_records_out_needs_in
        CHECK (check_out_at IS NULL OR check_in_at IS NOT NULL)
);

-- One record per employee per business day per shift. A double check-in is a
-- 409, not a duplicate row that would double-count a day of pay.
CREATE UNIQUE INDEX attendance_records_unique_day ON attendance_records (
    user_id,
    business_date,
    COALESCE(work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid)
);
CREATE INDEX attendance_records_org_date_idx    ON attendance_records (org_id, business_date DESC);
CREATE INDEX attendance_records_branch_date_idx ON attendance_records (branch_id, business_date DESC);
CREATE INDEX attendance_records_user_date_idx   ON attendance_records (user_id, business_date DESC);
-- Drives the nightly auto-checkout sweep.
CREATE INDEX attendance_records_open_idx ON attendance_records (scheduled_end_at)
    WHERE check_out_at IS NULL AND check_in_at IS NOT NULL;

-- ── RLS ──────────────────────────────────────────────────────────────────────
ALTER TABLE attendance_settings ENABLE ROW LEVEL SECURITY;
ALTER TABLE attendance_records  ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON attendance_settings FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
CREATE POLICY tenant_isolation ON attendance_records FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));

GRANT ALL ON TABLE attendance_settings TO sufrix;
GRANT ALL ON TABLE attendance_records  TO sufrix;
