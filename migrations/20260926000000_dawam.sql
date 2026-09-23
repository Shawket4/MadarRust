-- Dawam (DAWAM_TARGET_SPEC, 2026-09-22): what the staff app needs beyond the
-- August staff module. Every table is org-scoped with the usual tenant policy.
--
-- Sign-in (RO-1..RO-5): a WhatsApp code per phone, then one live device per
-- person. Presence (CL-4..CL-17): 15-minute pings while on shift, and the
-- flags they raise for a manager. Roster (SC-3, SC-8, SC-9, RU-10): published
-- weeks, swaps, open shifts, holidays, preferences. Pay (AD-3, AD-5, AV-7,
-- PAY-1, PAY-7): recurring and pending adjustments, expense advances, the
-- period start day, paid-per-person. And an inbox (APP-6).

-- ── sign-in ────────────────────────────────────────────────────────────────
-- Not org-scoped: a code is asked for BEFORE anyone knows the org. Plain text
-- by the delivery OTP's reasoning; five tries; deleted on use (RO-2).
CREATE TABLE staff_otp (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    phone      text NOT NULL,
    code       text NOT NULL,
    attempts   integer NOT NULL DEFAULT 0,
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX staff_otp_phone_idx ON staff_otp (phone, created_at DESC);
GRANT ALL ON TABLE staff_otp TO sufrix;

CREATE TABLE staff_devices (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id       uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id      uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash   text NOT NULL,
    platform     text NOT NULL DEFAULT '',
    model        text NOT NULL DEFAULT '',
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at  timestamptz NOT NULL DEFAULT now(),
    revoked_at   timestamptz
);
-- One live phone per person per business (RO-4, RO-5).
CREATE UNIQUE INDEX staff_devices_live_key ON staff_devices (org_id, user_id) WHERE revoked_at IS NULL;
CREATE INDEX staff_devices_hash_idx ON staff_devices (token_hash);

-- ── profile ────────────────────────────────────────────────────────────────
ALTER TABLE staff_profiles
    ADD COLUMN gender text,
    ADD COLUMN pay_method text NOT NULL DEFAULT 'cash',
    ADD COLUMN pay_account text,
    ADD COLUMN pref_time text,
    ADD COLUMN cant_work_days smallint[] NOT NULL DEFAULT '{}',
    ADD CONSTRAINT staff_profiles_gender_chk CHECK (gender IS NULL OR gender IN ('m', 'f')),
    ADD CONSTRAINT staff_profiles_pay_method_chk CHECK (pay_method IN ('cash', 'bank', 'wallet')),
    ADD CONSTRAINT staff_profiles_pref_time_chk CHECK (pref_time IS NULL OR pref_time IN ('morning', 'evening'));

-- ── rules ──────────────────────────────────────────────────────────────────
ALTER TABLE attendance_settings
    ADD COLUMN period_start_day smallint NOT NULL DEFAULT 26,
    ADD COLUMN overtime_mode text NOT NULL DEFAULT 'off',
    ADD COLUMN overtime_day_multiplier numeric(4,2) NOT NULL DEFAULT 1.35,
    ADD COLUMN overtime_night_multiplier numeric(4,2) NOT NULL DEFAULT 1.70,
    ADD COLUMN holiday_multiplier numeric(4,2) NOT NULL DEFAULT 2.00,
    ADD COLUMN advance_cap_percent numeric(5,2) NOT NULL DEFAULT 50,
    ADD COLUMN half_day_leave_counts text NOT NULL DEFAULT 'half_shift',
    ADD CONSTRAINT attendance_settings_period_day_chk CHECK (period_start_day BETWEEN 1 AND 28),
    ADD CONSTRAINT attendance_settings_ot_mode_chk CHECK (overtime_mode IN ('off', 'automatic', 'approval')),
    ADD CONSTRAINT attendance_settings_half_day_chk CHECK (half_day_leave_counts IN ('half_shift', 'whole_day'));

-- ── attendance: cover, overtime approval, tracking ────────────────────────
ALTER TABLE attendance_records
    ADD COLUMN covered_user_id uuid REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN cover_status text,
    ADD COLUMN overtime_status text,
    ADD COLUMN tracking_off boolean NOT NULL DEFAULT false,
    ADD COLUMN punch_reason text,
    ADD CONSTRAINT attendance_records_cover_status_chk
        CHECK (cover_status IS NULL OR cover_status IN ('pending', 'confirmed', 'rejected')),
    ADD CONSTRAINT attendance_records_ot_status_chk
        CHECK (overtime_status IS NULL OR overtime_status IN ('pending', 'approved', 'rejected'));
ALTER TABLE attendance_records DROP CONSTRAINT IF EXISTS attendance_records_check_in_method_check;
ALTER TABLE attendance_records DROP CONSTRAINT IF EXISTS attendance_records_check_out_method_check;
DO $$
DECLARE c record;
BEGIN
    -- The method CHECKs were created inline, so their names are generated:
    -- drop whichever constraint mentions the method columns.
    FOR c IN SELECT conname FROM pg_constraint
              WHERE conrelid = 'attendance_records'::regclass AND contype = 'c'
                AND pg_get_constraintdef(oid) ~ 'check_(in|out)_method'
    LOOP
        EXECUTE format('ALTER TABLE attendance_records DROP CONSTRAINT %I', c.conname);
    END LOOP;
END $$;
-- Every punch says how it was made (CL-16).
ALTER TABLE attendance_records
    ADD CONSTRAINT attendance_records_in_method_chk CHECK (check_in_method IS NULL OR check_in_method IN
        ('mobile_gps', 'manual', 'auto', 'offline', 'cover', 'till', 'kiosk', 'correction')),
    ADD CONSTRAINT attendance_records_out_method_chk CHECK (check_out_method IS NULL OR check_out_method IN
        ('mobile_gps', 'manual', 'auto', 'offline', 'cover', 'till', 'kiosk', 'correction'));

CREATE TABLE attendance_pings (
    id                   uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id               uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id              uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    attendance_record_id uuid NOT NULL REFERENCES attendance_records(id) ON DELETE CASCADE,
    at                   timestamptz NOT NULL DEFAULT now(),
    latitude             double precision,
    longitude            double precision,
    accuracy_meters      double precision,
    distance_meters      double precision,
    inside               boolean NOT NULL,
    is_mock              boolean NOT NULL DEFAULT false,
    battery_percent      smallint
);
CREATE INDEX attendance_pings_record_idx ON attendance_pings (attendance_record_id, at);

CREATE TABLE attendance_flags (
    id                   uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id               uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id              uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    branch_id            uuid REFERENCES branches(id) ON DELETE SET NULL,
    attendance_record_id uuid REFERENCES attendance_records(id) ON DELETE CASCADE,
    kind                 text NOT NULL,
    minutes_away         integer NOT NULL DEFAULT 0,
    detected_at          timestamptz NOT NULL DEFAULT now(),
    resolution           text,
    resolved_by          uuid REFERENCES users(id) ON DELETE SET NULL,
    resolved_at          timestamptz,
    deduction_id         uuid,
    CONSTRAINT attendance_flags_kind_chk CHECK (kind IN
        ('left_mid_shift', 'suspicious', 'tracking_off', 'time_unverified', 'new_phone', 'cover')),
    CONSTRAINT attendance_flags_resolution_chk CHECK (resolution IS NULL OR resolution IN
        ('ignored', 'excused_paid', 'excused_unpaid', 'deducted', 'revoked', 'confirmed'))
);
CREATE INDEX attendance_flags_open_idx ON attendance_flags (org_id, detected_at DESC) WHERE resolution IS NULL;
-- One open flag of a kind per shift: pings keep arriving.
CREATE UNIQUE INDEX attendance_flags_one_open_key ON attendance_flags (attendance_record_id, kind)
    WHERE resolution IS NULL AND attendance_record_id IS NOT NULL;

ALTER TABLE payroll_deductions DROP CONSTRAINT IF EXISTS payroll_deductions_source_check;
DO $$
DECLARE c record;
BEGIN
    FOR c IN SELECT conname FROM pg_constraint
              WHERE conrelid = 'payroll_deductions'::regclass AND contype = 'c'
                AND pg_get_constraintdef(oid) ~ 'source'
    LOOP
        EXECUTE format('ALTER TABLE payroll_deductions DROP CONSTRAINT %I', c.conname);
    END LOOP;
END $$;
ALTER TABLE payroll_deductions
    ADD CONSTRAINT payroll_deductions_source_chk CHECK (source IN
        ('manual', 'late_penalty', 'absence', 'left_mid_shift', 'unpaid_excuse', 'carry')),
    ADD COLUMN recurring boolean NOT NULL DEFAULT false,
    ADD COLUMN ends_on date,
    ADD COLUMN decided_by uuid REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN decided_at timestamptz;
ALTER TABLE payroll_bonuses
    ADD COLUMN recurring boolean NOT NULL DEFAULT false,
    ADD COLUMN ends_on date,
    ADD COLUMN decided_by uuid REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN decided_at timestamptz;

-- ── roster ────────────────────────────────────────────────────────────────
CREATE TABLE staff_week_publications (
    org_id       uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id    uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    week_start   date NOT NULL,
    published_at timestamptz NOT NULL DEFAULT now(),
    published_by uuid REFERENCES users(id) ON DELETE SET NULL,
    PRIMARY KEY (branch_id, week_start)
);
ALTER TABLE staff_schedule_overrides ADD COLUMN changed_after_publish boolean NOT NULL DEFAULT false;

CREATE TABLE staff_open_shifts (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id        uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id     uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    work_shift_id uuid NOT NULL REFERENCES work_shifts(id) ON DELETE CASCADE,
    on_date       date NOT NULL,
    status        text NOT NULL DEFAULT 'open',
    claimed_by    uuid REFERENCES users(id) ON DELETE SET NULL,
    claimed_at    timestamptz,
    posted_by     uuid REFERENCES users(id) ON DELETE SET NULL,
    decided_by    uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT staff_open_shifts_status_chk CHECK (status IN ('open', 'claimed', 'filled', 'cancelled'))
);
CREATE INDEX staff_open_shifts_branch_idx ON staff_open_shifts (branch_id, on_date);

CREATE TABLE staff_swaps (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id            uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    requester_id      uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    requester_date    date NOT NULL,
    requester_shift_id uuid NOT NULL REFERENCES work_shifts(id) ON DELETE CASCADE,
    peer_id           uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    peer_date         date NOT NULL,
    peer_shift_id     uuid NOT NULL REFERENCES work_shifts(id) ON DELETE CASCADE,
    status            text NOT NULL DEFAULT 'awaiting_peer',
    decided_by        uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at        timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT staff_swaps_status_chk CHECK (status IN
        ('awaiting_peer', 'pending', 'approved', 'rejected', 'cancelled')),
    CONSTRAINT staff_swaps_two_people CHECK (requester_id <> peer_id)
);

CREATE TABLE staff_holidays (
    org_id   uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    on_date  date NOT NULL,
    name_en  text NOT NULL,
    name_ar  text NOT NULL,
    decision text,
    decided_by uuid REFERENCES users(id) ON DELETE SET NULL,
    PRIMARY KEY (org_id, on_date),
    CONSTRAINT staff_holidays_decision_chk CHECK (decision IS NULL OR decision IN ('holiday', 'dismissed'))
);

-- Suggestion answers, kept for learning (SC-13): 24 months.
CREATE TABLE staff_suggestion_events (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id      uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id   uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    suggestion  text NOT NULL,
    user_id     uuid REFERENCES users(id) ON DELETE SET NULL,
    on_date     date NOT NULL,
    accepted    boolean NOT NULL,
    by_default  boolean NOT NULL DEFAULT false,
    decided_by  uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

-- ── money ──────────────────────────────────────────────────────────────────
-- Cash handed over for shop purchases: logged, never deducted (AV-7).
CREATE TABLE expense_advances (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id          uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id         uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    branch_id       uuid REFERENCES branches(id) ON DELETE SET NULL,
    amount_piastres bigint NOT NULL CHECK (amount_piastres > 0),
    purpose         text NOT NULL CHECK (char_length(purpose) BETWEEN 1 AND 300),
    via             text NOT NULL DEFAULT 'safe',
    handed_by       uuid REFERENCES users(id) ON DELETE SET NULL,
    given_on        date NOT NULL DEFAULT CURRENT_DATE,
    created_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT expense_advances_via_chk CHECK (via IN ('safe', 'bank', 'till'))
);
CREATE INDEX expense_advances_user_idx ON expense_advances (user_id, given_on DESC);

-- Paid per person, with a method (PAY-7); a shortfall carried out (PAY-12).
ALTER TABLE payslips
    ADD COLUMN paid_method text,
    ADD COLUMN paid_at timestamptz,
    ADD COLUMN carry_out_piastres bigint NOT NULL DEFAULT 0,
    ADD CONSTRAINT payslips_paid_method_chk CHECK (paid_method IS NULL OR paid_method IN ('cash', 'bank', 'wallet'));

-- ── inbox (APP-6) ──────────────────────────────────────────────────────────
CREATE TABLE staff_notifications (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id     uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    key        text NOT NULL,
    args       jsonb NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now(),
    read_at    timestamptz
);
CREATE INDEX staff_notifications_user_idx ON staff_notifications (user_id, created_at DESC);

-- ── tenancy ────────────────────────────────────────────────────────────────
DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['staff_devices', 'attendance_pings', 'attendance_flags',
        'staff_week_publications', 'staff_open_shifts', 'staff_swaps', 'staff_holidays',
        'staff_suggestion_events', 'expense_advances', 'staff_notifications']
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I FOR ALL USING (org_id = (SELECT current_setting(''app.org_id'', true)::uuid))', t);
        EXECUTE format('GRANT ALL ON TABLE %I TO sufrix', t);
    END LOOP;
END $$;

-- ── capabilities 226-235 (PM-2: shipped with their preset grants) ──────────
INSERT INTO capabilities (id, key, legacy_resource, legacy_action, tier, defaults, core, approval, protected) VALUES
    (226, 'hr.requests.self_approve', NULL, NULL, 'configurable', 'o',  '', false, true),
    (227, 'hr.adjustments.create',    NULL, NULL, 'configurable', 'om', '', true,  false),
    (228, 'hr.advances.decide',       NULL, NULL, 'configurable', 'om', '', true,  false),
    (229, 'hr.payroll.run',           NULL, NULL, 'configurable', 'o',  '', false, true),
    (230, 'hr.shift_cover.confirm',   NULL, NULL, 'configurable', 'om', '', false, false),
    (231, 'hr.overtime.approve',      NULL, NULL, 'configurable', 'om', '', true,  false),
    (232, 'hr.attendance.punch_others', NULL, NULL, 'configurable', 'om', '', false, false),
    (233, 'hr.schedule.publish',      NULL, NULL, 'configurable', 'om', '', false, false),
    (234, 'hr.expense_advances.log',  NULL, NULL, 'configurable', 'om', '', false, false),
    (235, 'hr.roster.settings',       NULL, NULL, 'configurable', 'o',  '', false, true)
ON CONFLICT (id) DO NOTHING;

-- System roles get the template default. A branch manager's money acts carry
-- a limit (AD-5, AV-5): 1,000 EGP per adjustment or overtime, advances up to
-- half a month's salary owed. Above it the act waits for the owner.
INSERT INTO org_role_grants (org_role_id, org_id, capability_id, limits, source, template_version)
SELECT r.id, r.org_id, c.id,
       CASE WHEN r.kind::text = 'branch_manager' AND c.id IN (227, 231) THEN '{"max_amount": 100000}'::jsonb
            WHEN r.kind::text = 'branch_manager' AND c.id = 228 THEN '{"max_percent": 50}'::jsonb
            ELSE '{}'::jsonb END,
       'template', 0
  FROM org_roles r
  JOIN capabilities c ON c.id BETWEEN 226 AND 235
 WHERE r.is_system AND r.deleted_at IS NULL
   AND position(authz_kind_letter(r.kind::text) IN c.defaults) > 0
ON CONFLICT (org_role_id, capability_id) DO NOTHING;

SELECT authz_bump_epoch(o.id) FROM organizations o;
