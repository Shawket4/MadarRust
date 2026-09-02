-- Staff & attendance — part 1 of 5: the employee directory.
--
-- An EMPLOYEE IS A USER. There is no parallel person entity and no new
-- `user_role`: whether someone is treated as staff is decided by the presence of
-- a `staff_profiles` row, and what they may do about it is decided by the
-- `staff` / `attendance` / `leave` / `payroll` permission resources added in
-- part 5. A cleaner who never touches the POS is a `teller`-role user with every
-- POS permission revoked; a cashier who also draws a salary is the same user row
-- with a profile attached.
--
-- HR fields live in a 1:1 SIDE TABLE rather than as columns on `users` so that
-- base salary never rides along in the existing `/users` list responses (which
-- branch managers can read), and so salary reads get their own permission and
-- RLS surface. `user_id` is the primary key, which enforces the 1:1.
--
-- Money is PIASTRES (integer) system-wide — never a float, never major units.

CREATE TABLE departments (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id          uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name            text NOT NULL,
    -- Informational: who signs off leave for this department. Approval is still
    -- gated by the `leave` permission, not by this pointer.
    manager_user_id uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT departments_name_len CHECK (char_length(name) BETWEEN 1 AND 120)
);

CREATE UNIQUE INDEX departments_org_name_key ON departments (org_id, lower(name));
CREATE INDEX departments_manager_idx ON departments (manager_user_id);

CREATE TABLE staff_profiles (
    -- 1:1 with users. PK on the FK is the enforcement.
    user_id                 uuid PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    org_id                  uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    department_id           uuid REFERENCES departments(id) ON DELETE SET NULL,
    -- Operator-facing staff number (payslips, door lists). Optional; unique per
    -- org when present.
    employee_code           text,
    job_title               text,
    hire_date               date,
    termination_date        date,
    employment_status       text NOT NULL DEFAULT 'active',
    -- Monthly base pay in piastres, before overtime/bonuses/deductions.
    base_salary_piastres    bigint NOT NULL DEFAULT 0,
    national_id             text,
    photo_url               text,
    emergency_contact_name  text,
    emergency_contact_phone text,
    notes                   text,
    created_at              timestamptz NOT NULL DEFAULT now(),
    updated_at              timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT staff_profiles_status_chk
        CHECK (employment_status IN ('active', 'suspended', 'terminated')),
    CONSTRAINT staff_profiles_salary_nonneg CHECK (base_salary_piastres >= 0),
    -- A terminated profile must say when; a live one must not.
    CONSTRAINT staff_profiles_termination_chk CHECK (
        (employment_status = 'terminated' AND termination_date IS NOT NULL)
     OR (employment_status <> 'terminated' AND termination_date IS NULL)
    ),
    CONSTRAINT staff_profiles_termination_after_hire
        CHECK (termination_date IS NULL OR hire_date IS NULL OR termination_date >= hire_date)
);

CREATE UNIQUE INDEX staff_profiles_org_code_key
    ON staff_profiles (org_id, lower(employee_code))
    WHERE employee_code IS NOT NULL;
CREATE INDEX staff_profiles_org_idx        ON staff_profiles (org_id);
CREATE INDEX staff_profiles_department_idx ON staff_profiles (department_id);

CREATE TABLE staff_documents (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id      uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id     uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Free-form bucket ('contract', 'id', 'certificate', …) — deliberately not
    -- an enum, operators invent their own categories.
    kind        text NOT NULL DEFAULT 'other',
    title       text NOT NULL,
    -- Path returned by the existing /uploads endpoints.
    file_url    text NOT NULL,
    -- Drives the "expiring documents" dashboard nag (residency permits, etc).
    expires_on  date,
    uploaded_by uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT staff_documents_title_len CHECK (char_length(title) BETWEEN 1 AND 200)
);

CREATE INDEX staff_documents_user_idx    ON staff_documents (user_id, created_at DESC);
CREATE INDEX staff_documents_expiry_idx  ON staff_documents (org_id, expires_on)
    WHERE expires_on IS NOT NULL;

-- ── RLS ──────────────────────────────────────────────────────────────────────
-- The generator in 20260708000100_rls_policies.sql has already run, so every
-- table added afterwards classifies itself here or the completeness assertion in
-- src/rls_tests.rs fails. All three are org-rooted.
ALTER TABLE departments     ENABLE ROW LEVEL SECURITY;
ALTER TABLE staff_profiles  ENABLE ROW LEVEL SECURITY;
ALTER TABLE staff_documents ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON departments FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
CREATE POLICY tenant_isolation ON staff_profiles FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
CREATE POLICY tenant_isolation ON staff_documents FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));

GRANT ALL ON TABLE departments     TO sufrix;
GRANT ALL ON TABLE staff_profiles  TO sufrix;
GRANT ALL ON TABLE staff_documents TO sufrix;
