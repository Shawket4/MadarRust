-- Dawam Phase A (owner decision 2026-09-23): an employee is its own entity,
-- optionally linked to a Madar user. Three kinds:
--   linked  — any existing user (teller, waiter, kitchen, branch manager,
--             owner) who is also on payroll: `user_id` set;
--   manual  — payroll and attendance only, no app: no user, no app access;
--   app     — signs in to the staff app with a WhatsApp code: no user row.
-- Creating an employee never creates a users row (so never a POS teller), and
-- creating a user never makes an employee. See dawam-fix/PHASE_A_DESIGN.md.
--
-- Every Dawam / HR table that recorded a PERSON by `users.id` now records the
-- employee. Actor columns (created_by, decided_by, …) stay users: managers act
-- through their linked user.
--
-- Nothing is lost: each migrated employee keeps the id of the user it came
-- from, so every existing subject value is already a valid employee id and
-- the swap below is a rename plus a new foreign key.

-- ── the entity ─────────────────────────────────────────────────────────────
CREATE TABLE employees (
    id                      uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id                  uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- The optional link to a Madar user (a till worker, a manager, the owner).
    user_id                 uuid UNIQUE REFERENCES users(id) ON DELETE SET NULL,
    name                    text NOT NULL CHECK (char_length(btrim(name)) BETWEEN 1 AND 120),
    -- As entered ("+2010…"); `phone_key` is the one canonical form (E.164
    -- digits, see phone_canonical), what sign-in matches on.
    phone                   text,
    phone_key               text GENERATED ALWAYS AS (phone_canonical(phone)) STORED,
    -- May sign in to the staff app with a WhatsApp code (needs a phone).
    app_access              boolean NOT NULL DEFAULT false,
    department_id           uuid REFERENCES departments(id) ON DELETE SET NULL,
    employee_code           text,
    job_title               text,
    hire_date               date,
    termination_date        date,
    employment_status       text NOT NULL DEFAULT 'active',
    base_salary_piastres    bigint NOT NULL DEFAULT 0,
    national_id             text,
    photo_url               text,
    emergency_contact_name  text,
    emergency_contact_phone text,
    notes                   text,
    gender                  text,
    pay_method              text NOT NULL DEFAULT 'cash',
    pay_account             text,
    pref_time               text,
    cant_work_days          smallint[] NOT NULL DEFAULT '{}',
    created_at              timestamptz NOT NULL DEFAULT now(),
    updated_at              timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT employees_status_chk CHECK (employment_status IN ('active', 'suspended', 'terminated')),
    CONSTRAINT employees_termination_chk CHECK (
        (employment_status = 'terminated' AND termination_date IS NOT NULL)
        OR (employment_status <> 'terminated' AND termination_date IS NULL)),
    CONSTRAINT employees_termination_after_hire CHECK (
        termination_date IS NULL OR hire_date IS NULL OR termination_date >= hire_date),
    CONSTRAINT employees_salary_nonneg CHECK (base_salary_piastres >= 0),
    CONSTRAINT employees_gender_chk CHECK (gender IS NULL OR gender IN ('m', 'f')),
    CONSTRAINT employees_pay_method_chk CHECK (pay_method IN ('cash', 'bank', 'wallet')),
    CONSTRAINT employees_pref_time_chk CHECK (pref_time IS NULL OR pref_time IN ('morning', 'evening')),
    -- The app needs a number the code can go to.
    CONSTRAINT employees_app_needs_phone CHECK (NOT app_access OR phone_key IS NOT NULL)
);
CREATE INDEX employees_org_idx ON employees (org_id, employment_status);
CREATE INDEX employees_department_idx ON employees (department_id);
CREATE UNIQUE INDEX employees_org_code_key ON employees (org_id, lower(employee_code))
    WHERE employee_code IS NOT NULL;
CREATE INDEX employees_phone_key_idx ON employees (phone_key) WHERE phone_key IS NOT NULL;

-- A linked user belongs to the employee's own org.
CREATE FUNCTION employees_link_same_org() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.user_id IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM users u WHERE u.id = NEW.user_id AND u.org_id = NEW.org_id
    ) THEN
        RAISE EXCEPTION 'employee % links a user outside its org', NEW.id
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER employees_link_same_org BEFORE INSERT OR UPDATE OF user_id, org_id
    ON employees FOR EACH ROW EXECUTE FUNCTION employees_link_same_org();

-- Where an employee works. Managers see and decide for the employees assigned
-- to one of their branches (RO-6).
CREATE TABLE employee_branches (
    employee_id uuid NOT NULL REFERENCES employees(id) ON DELETE CASCADE,
    branch_id   uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    org_id      uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    assigned_at timestamptz NOT NULL DEFAULT now(),
    assigned_by uuid REFERENCES users(id) ON DELETE SET NULL,
    PRIMARY KEY (employee_id, branch_id)
);
CREATE INDEX employee_branches_branch_idx ON employee_branches (branch_id);

-- ── 1. everyone with a staff profile ──────────────────────────────────────
INSERT INTO employees (
    id, org_id, user_id, name, phone, app_access, department_id, employee_code,
    job_title, hire_date, termination_date, employment_status, base_salary_piastres,
    national_id, photo_url, emergency_contact_name, emergency_contact_phone, notes,
    gender, pay_method, pay_account, pref_time, cant_work_days, created_at, updated_at)
SELECT p.user_id, p.org_id, p.user_id,
       COALESCE(NULLIF(btrim(u.name), ''), 'Employee'),
       CASE WHEN phone_canonical(u.phone) IS NOT NULL THEN '+' || phone_canonical(u.phone)
            ELSE NULLIF(btrim(u.phone), '') END,
       false,
       p.department_id, p.employee_code, p.job_title, p.hire_date,
       -- A deleted login ends the employment it carried.
       CASE WHEN u.deleted_at IS NOT NULL AND p.employment_status <> 'terminated'
            THEN GREATEST(u.deleted_at::date, COALESCE(p.hire_date, u.deleted_at::date))
            ELSE p.termination_date END,
       CASE WHEN u.deleted_at IS NOT NULL THEN 'terminated' ELSE p.employment_status END,
       p.base_salary_piastres, p.national_id, p.photo_url, p.emergency_contact_name,
       p.emergency_contact_phone, p.notes, p.gender, p.pay_method, p.pay_account,
       p.pref_time, p.cant_work_days, p.created_at, p.updated_at
  FROM staff_profiles p
  JOIN users u ON u.id = p.user_id;

-- The app stays open to exactly who could sign in before: an active login
-- with a valid number. One live number per org: the earliest keeps it.
UPDATE employees e SET app_access = true
  FROM users u
 WHERE u.id = e.user_id AND u.is_active AND u.deleted_at IS NULL
   AND e.phone_key IS NOT NULL AND e.employment_status = 'active';
UPDATE employees e SET app_access = false
  FROM (SELECT id, row_number() OVER (PARTITION BY org_id, phone_key ORDER BY created_at, id) AS n
          FROM employees WHERE app_access) d
 WHERE d.id = e.id AND d.n > 1;

-- ── 2. older HR records whose person never had (or lost) a profile ─────────
-- Kept, attached to a terminated employee of the same id, so history and
-- payslips keep their person while nobody new is paid or rostered.
WITH subjects AS (
    SELECT user_id AS person, org_id FROM attendance_records
    UNION SELECT covered_user_id, org_id FROM attendance_records WHERE covered_user_id IS NOT NULL
    UNION SELECT user_id, org_id FROM attendance_pings
    UNION SELECT user_id, org_id FROM attendance_flags
    UNION SELECT user_id, org_id FROM staff_requests
    UNION SELECT user_id, org_id FROM leave_balances
    UNION SELECT user_id, org_id FROM payroll_deductions
    UNION SELECT user_id, org_id FROM payroll_bonuses
    UNION SELECT user_id, org_id FROM payslips
    UNION SELECT user_id, org_id FROM salary_advances
    UNION SELECT user_id, org_id FROM staff_schedules
    UNION SELECT user_id, org_id FROM staff_schedule_overrides
    UNION SELECT user_id, org_id FROM staff_documents
    UNION SELECT user_id, org_id FROM expense_advances
    UNION SELECT requester_id, org_id FROM staff_swaps
    UNION SELECT peer_id, org_id FROM staff_swaps
    UNION SELECT claimed_by, org_id FROM staff_open_shifts WHERE claimed_by IS NOT NULL
), orphans AS (
    SELECT DISTINCT ON (s.person) s.person, s.org_id
      FROM subjects s
     WHERE s.person IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM employees e WHERE e.id = s.person)
     ORDER BY s.person, s.org_id
)
INSERT INTO employees (id, org_id, user_id, name, phone, app_access,
                       employment_status, termination_date)
SELECT o.person, o.org_id,
       CASE WHEN u.org_id = o.org_id
             AND NOT EXISTS (SELECT 1 FROM employees x WHERE x.user_id = u.id) THEN u.id END,
       COALESCE(NULLIF(btrim(u.name), ''), 'Former employee'),
       CASE WHEN phone_canonical(u.phone) IS NOT NULL THEN '+' || phone_canonical(u.phone) END,
       false, 'terminated', CURRENT_DATE
  FROM orphans o
  LEFT JOIN users u ON u.id = o.person;

-- ── 3. branches ────────────────────────────────────────────────────────────
INSERT INTO employee_branches (employee_id, branch_id, org_id, assigned_at, assigned_by)
SELECT e.id, a.branch_id, e.org_id, a.assigned_at, a.assigned_by
  FROM employees e
  JOIN user_branch_assignments a ON a.user_id = e.user_id
  JOIN branches b ON b.id = a.branch_id AND b.org_id = e.org_id
ON CONFLICT DO NOTHING;
-- Where they actually punched.
INSERT INTO employee_branches (employee_id, branch_id, org_id)
SELECT DISTINCT a.user_id, a.branch_id, a.org_id
  FROM attendance_records a
  JOIN branches b ON b.id = a.branch_id AND b.org_id = a.org_id AND b.deleted_at IS NULL
ON CONFLICT DO NOTHING;
-- A one-branch business: that branch.
INSERT INTO employee_branches (employee_id, branch_id, org_id)
SELECT e.id, b.id, e.org_id
  FROM employees e
  JOIN LATERAL (SELECT id FROM branches WHERE org_id = e.org_id AND deleted_at IS NULL) b ON true
 WHERE NOT EXISTS (SELECT 1 FROM employee_branches x WHERE x.employee_id = e.id)
   AND (SELECT count(*) FROM branches WHERE org_id = e.org_id AND deleted_at IS NULL) = 1
ON CONFLICT DO NOTHING;

-- ── 4. re-key every subject column ────────────────────────────────────────
-- Rows that only ever addressed a login (a phone binding, an inbox line, a
-- push registration) and have no employee go: they are not records.
DELETE FROM staff_devices d WHERE NOT EXISTS (SELECT 1 FROM employees e WHERE e.id = d.user_id);
DELETE FROM staff_notifications n WHERE NOT EXISTS (SELECT 1 FROM employees e WHERE e.id = n.user_id);
UPDATE staff_suggestion_events s SET user_id = NULL
 WHERE user_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM employees e WHERE e.id = s.user_id);

-- Drop the FK a column has to users, rename it, and point it at employees.
CREATE FUNCTION pg_temp.rekey(tbl text, old_col text, new_col text, on_delete text)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE c record;
BEGIN
    FOR c IN SELECT con.conname
               FROM pg_constraint con
               JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = ANY (con.conkey)
              WHERE con.contype = 'f' AND con.conrelid = tbl::regclass
                AND con.confrelid = 'users'::regclass AND a.attname = old_col
    LOOP
        EXECUTE format('ALTER TABLE %I DROP CONSTRAINT %I', tbl, c.conname);
    END LOOP;
    IF old_col <> new_col THEN
        EXECUTE format('ALTER TABLE %I RENAME COLUMN %I TO %I', tbl, old_col, new_col);
    END IF;
    EXECUTE format('ALTER TABLE %I ADD CONSTRAINT %I FOREIGN KEY (%I) REFERENCES employees(id) %s',
                   tbl, tbl || '_' || new_col || '_fkey', new_col, on_delete);
END $$;

-- Records of what happened: an employee with history is terminated, never
-- deleted (AT-6), so these do not cascade.
SELECT pg_temp.rekey('attendance_records', 'user_id', 'employee_id', '');
SELECT pg_temp.rekey('attendance_records', 'covered_user_id', 'covered_employee_id', 'ON DELETE SET NULL');
SELECT pg_temp.rekey('attendance_pings', 'user_id', 'employee_id', '');
SELECT pg_temp.rekey('attendance_flags', 'user_id', 'employee_id', '');
SELECT pg_temp.rekey('staff_requests', 'user_id', 'employee_id', '');
SELECT pg_temp.rekey('leave_balances', 'user_id', 'employee_id', 'ON DELETE CASCADE');
SELECT pg_temp.rekey('payroll_deductions', 'user_id', 'employee_id', '');
SELECT pg_temp.rekey('payroll_bonuses', 'user_id', 'employee_id', '');
SELECT pg_temp.rekey('payslips', 'user_id', 'employee_id', '');
SELECT pg_temp.rekey('salary_advances', 'user_id', 'employee_id', '');
SELECT pg_temp.rekey('staff_schedules', 'user_id', 'employee_id', 'ON DELETE CASCADE');
SELECT pg_temp.rekey('staff_schedule_overrides', 'user_id', 'employee_id', 'ON DELETE CASCADE');
SELECT pg_temp.rekey('staff_documents', 'user_id', 'employee_id', '');
SELECT pg_temp.rekey('expense_advances', 'user_id', 'employee_id', '');
SELECT pg_temp.rekey('staff_swaps', 'requester_id', 'requester_id', 'ON DELETE CASCADE');
SELECT pg_temp.rekey('staff_swaps', 'peer_id', 'peer_id', 'ON DELETE CASCADE');
SELECT pg_temp.rekey('staff_open_shifts', 'claimed_by', 'claimed_by', 'ON DELETE SET NULL');
SELECT pg_temp.rekey('staff_suggestion_events', 'user_id', 'employee_id', 'ON DELETE SET NULL');
-- Bindings and inbox lines follow the person.
SELECT pg_temp.rekey('staff_devices', 'user_id', 'employee_id', 'ON DELETE CASCADE');
SELECT pg_temp.rekey('staff_notifications', 'user_id', 'employee_id', 'ON DELETE CASCADE');
COMMENT ON COLUMN staff_open_shifts.claimed_by IS 'The employee who claimed it (employees.id).';

-- The staff app registers its pushes for the employee; other apps keep users.
ALTER TABLE push_devices
    ADD COLUMN employee_id uuid REFERENCES employees(id) ON DELETE CASCADE,
    ALTER COLUMN user_id DROP NOT NULL;
UPDATE push_devices p SET employee_id = p.user_id, user_id = NULL
 WHERE p.app = 'dawam' AND EXISTS (SELECT 1 FROM employees e WHERE e.id = p.user_id);
DELETE FROM push_devices WHERE app = 'dawam' AND employee_id IS NULL;
ALTER TABLE push_devices ADD CONSTRAINT push_devices_one_owner
    CHECK (num_nonnulls(user_id, employee_id) = 1);
CREATE INDEX push_devices_employee_app_idx ON push_devices (employee_id, app)
    WHERE revoked_at IS NULL AND employee_id IS NOT NULL;

-- The old profile is folded in (its suggestion-cache trigger goes with it).
DROP TABLE staff_profiles;

-- Suggestions were cached with user ids; they recompute on the next read.
DELETE FROM staff_suggestion_cache;
CREATE TRIGGER dawam_suggestion_cache AFTER INSERT OR UPDATE OR DELETE
    ON employees FOR EACH ROW EXECUTE FUNCTION dawam_drop_suggestion_cache();
CREATE TRIGGER dawam_suggestion_cache AFTER INSERT OR UPDATE OR DELETE
    ON employee_branches FOR EACH ROW EXECUTE FUNCTION dawam_drop_suggestion_cache();

-- An app-enabled number belongs to one live employee per business (RO-4, RO-5).
CREATE UNIQUE INDEX employees_live_phone_key ON employees (org_id, phone_key)
    WHERE app_access AND employment_status <> 'terminated';

-- ── tenancy ────────────────────────────────────────────────────────────────
ALTER TABLE employees ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON employees FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
ALTER TABLE employee_branches ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON employee_branches FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT SELECT, INSERT, UPDATE, DELETE ON employees, employee_branches TO madar_app;
GRANT ALL ON TABLE employees, employee_branches TO sufrix;
