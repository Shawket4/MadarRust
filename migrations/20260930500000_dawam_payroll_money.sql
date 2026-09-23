-- Dawam Phase B (payroll): the money model the audit (05-money.md) found wanting.
--
-- 1. The "not on payroll" flag (owner decision 2026-09-23): an app-using owner
--    who is not paid through Dawam is skipped by the run, the estimate and the
--    payslips, but still clocks in and is rostered.
-- 2. Advance collections are a LEDGER (AV-6). `salary_advances.remaining_piastres`
--    is derived from it by trigger, so a regeneration, a reopen or a deleted
--    period can never collect an installment twice or lose a refund, and the
--    balance is rebuildable from the rows.
-- 3. A salary history (PAY-13): payroll prices each calendar day at the salary in
--    force that day; a change mid-period pays pro rata at each rate.
-- 4. Who did what to money (AD-9, AT-10): `payslips.paid_by`, who stopped a
--    recurring line and why, and one audit log for the acts that left no trace
--    (deletes, reopen, close, generate, waive/unwaive/override, advances).
-- 5. Constraints the handlers relied on but the schema contradicted: an
--    override to zero (B6), overlapping periods (B8), and a 'none' pay method
--    for a 0-net payslip auto-marked paid so it never blocks "month Paid".
-- 6. Per-shift overtime rates (RU-8): nullable overrides on work_shifts, read by
--    the one shift-pricing function.
-- 7. Separate bonus / deduction limits (AD-5): `hr.deductions.create` (id 245)
--    carries the deduction limit; `hr.adjustments.create` keeps the bonus one.

-- ── 1. not on payroll ──────────────────────────────────────────────────────
ALTER TABLE employees ADD COLUMN on_payroll boolean NOT NULL DEFAULT true;

-- ── 2. advance collections ledger ──────────────────────────────────────────
CREATE TABLE salary_advance_collections (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id           uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    advance_id       uuid NOT NULL REFERENCES salary_advances(id) ON DELETE CASCADE,
    -- Deleting the payslip (regenerate, reopen, delete period) deletes the
    -- collection, and the trigger below gives the money back.
    payslip_id       uuid NOT NULL REFERENCES payslips(id) ON DELETE CASCADE,
    period_id        uuid NOT NULL REFERENCES payroll_periods(id) ON DELETE CASCADE,
    employee_id      uuid NOT NULL REFERENCES employees(id),
    amount_piastres  bigint NOT NULL CHECK (amount_piastres > 0),
    created_at       timestamptz NOT NULL DEFAULT now(),
    UNIQUE (advance_id, payslip_id)
);
CREATE INDEX salary_advance_collections_advance_idx ON salary_advance_collections (advance_id);
CREATE INDEX salary_advance_collections_period_idx ON salary_advance_collections (period_id);
ALTER TABLE salary_advance_collections ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON salary_advance_collections FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT SELECT, INSERT, UPDATE, DELETE ON salary_advance_collections TO madar_app;
GRANT ALL ON TABLE salary_advance_collections TO sufrix;

-- remaining = amount − collected; settled when nothing is left, back to
-- approved when a refund reopens it.
CREATE OR REPLACE FUNCTION salary_advance_sync_remaining(p_advance uuid) RETURNS void
LANGUAGE plpgsql AS $$
BEGIN
    UPDATE salary_advances a
       SET remaining_piastres = GREATEST(0, a.amount_piastres - c.collected),
           status = CASE
                        WHEN a.status NOT IN ('approved', 'settled') THEN a.status
                        WHEN a.amount_piastres - c.collected <= 0 THEN 'settled'
                        ELSE 'approved'
                    END,
           updated_at = now()
      FROM (SELECT COALESCE(SUM(amount_piastres), 0) AS collected
              FROM salary_advance_collections WHERE advance_id = p_advance) c
     WHERE a.id = p_advance;
END $$;

CREATE OR REPLACE FUNCTION salary_advance_collections_sync() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM salary_advance_sync_remaining(OLD.advance_id);
        RETURN OLD;
    END IF;
    PERFORM salary_advance_sync_remaining(NEW.advance_id);
    RETURN NEW;
END $$;

CREATE TRIGGER salary_advance_collections_sync
    AFTER INSERT OR UPDATE OR DELETE ON salary_advance_collections
    FOR EACH ROW EXECUTE FUNCTION salary_advance_collections_sync();

-- Backfill from the frozen payslips: every advance line a payslip recorded
-- becomes a ledger row, then every advance's balance is rebuilt from it.
INSERT INTO salary_advance_collections (org_id, advance_id, payslip_id, period_id, employee_id, amount_piastres)
SELECT s.org_id, a.id, s.id, s.payroll_period_id, s.employee_id, (l->>'applied_piastres')::bigint
  FROM payslips s
  CROSS JOIN LATERAL jsonb_array_elements(COALESCE(s.breakdown->'advances', '[]'::jsonb)) AS l
  JOIN salary_advances a ON a.id = (l->>'id')::uuid
 WHERE (l->>'applied_piastres')::bigint > 0
ON CONFLICT (advance_id, payslip_id) DO NOTHING;
SELECT salary_advance_sync_remaining(id) FROM salary_advances WHERE status IN ('approved', 'settled');

-- ── 3. salary history ──────────────────────────────────────────────────────
CREATE TABLE employee_salary_history (
    id                    uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id                uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    employee_id           uuid NOT NULL REFERENCES employees(id) ON DELETE CASCADE,
    base_salary_piastres  bigint NOT NULL CHECK (base_salary_piastres >= 0),
    effective_from        date NOT NULL,
    created_by            uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at            timestamptz NOT NULL DEFAULT now(),
    UNIQUE (employee_id, effective_from)
);
CREATE INDEX employee_salary_history_emp_idx ON employee_salary_history (employee_id, effective_from DESC);
ALTER TABLE employee_salary_history ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON employee_salary_history FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT SELECT, INSERT, UPDATE, DELETE ON employee_salary_history TO madar_app;
GRANT ALL ON TABLE employee_salary_history TO sufrix;

-- Today's salary, dated from the hire date (or the row's creation), so every
-- employee has a segment from before any period.
INSERT INTO employee_salary_history (org_id, employee_id, base_salary_piastres, effective_from)
SELECT org_id, id, base_salary_piastres, COALESCE(hire_date, created_at::date, CURRENT_DATE)
  FROM employees
ON CONFLICT DO NOTHING;

-- A salary change records the new rate from the day it was made. The handler
-- may insert an explicit dated row itself; this trigger is the safety net so
-- a raw UPDATE never leaves the history behind the column.
CREATE OR REPLACE FUNCTION employees_salary_history_trg() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO employee_salary_history (org_id, employee_id, base_salary_piastres, effective_from)
        VALUES (NEW.org_id, NEW.id, NEW.base_salary_piastres, COALESCE(NEW.hire_date, CURRENT_DATE))
        ON CONFLICT (employee_id, effective_from) DO UPDATE SET base_salary_piastres = EXCLUDED.base_salary_piastres;
    ELSIF NEW.base_salary_piastres IS DISTINCT FROM OLD.base_salary_piastres THEN
        INSERT INTO employee_salary_history (org_id, employee_id, base_salary_piastres, effective_from)
        VALUES (NEW.org_id, NEW.id, NEW.base_salary_piastres, CURRENT_DATE)
        ON CONFLICT (employee_id, effective_from) DO UPDATE SET base_salary_piastres = EXCLUDED.base_salary_piastres;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER employees_salary_history
    AFTER INSERT OR UPDATE OF base_salary_piastres ON employees
    FOR EACH ROW EXECUTE FUNCTION employees_salary_history_trg();

-- ── 4. who did what ────────────────────────────────────────────────────────
ALTER TABLE payslips
    ADD COLUMN paid_by uuid REFERENCES users(id) ON DELETE SET NULL,
    DROP CONSTRAINT payslips_paid_method_chk,
    -- 'none': a payslip with nothing to pay (net 0, no lines), marked paid by
    -- the run itself so the month can still reach Paid (PAY-7).
    ADD CONSTRAINT payslips_paid_method_chk
        CHECK (paid_method IS NULL OR paid_method IN ('cash', 'bank', 'wallet', 'none'));

ALTER TABLE payroll_bonuses
    ADD COLUMN stopped_by uuid REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN stopped_at timestamptz,
    ADD COLUMN stop_reason text;
ALTER TABLE payroll_deductions
    ADD COLUMN stopped_by uuid REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN stopped_at timestamptz,
    ADD COLUMN stop_reason text;

CREATE TABLE payroll_audit_log (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id       uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    actor_id     uuid REFERENCES users(id) ON DELETE SET NULL,
    -- adjustment.delete · adjustment.stop · deduction.waive · deduction.unwaive ·
    -- deduction.override · period.generate · period.reopen · period.close ·
    -- period.delete · payslip.paid · advance.decide · advance.record
    action       text NOT NULL,
    entity       text NOT NULL,
    entity_id    uuid,
    employee_id  uuid REFERENCES employees(id) ON DELETE SET NULL,
    period_id    uuid REFERENCES payroll_periods(id) ON DELETE SET NULL,
    reason       text,
    details      jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX payroll_audit_log_org_idx ON payroll_audit_log (org_id, created_at DESC);
CREATE INDEX payroll_audit_log_entity_idx ON payroll_audit_log (entity, entity_id);
ALTER TABLE payroll_audit_log ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON payroll_audit_log FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT SELECT, INSERT, UPDATE, DELETE ON payroll_audit_log TO madar_app;
GRANT ALL ON TABLE payroll_audit_log TO sufrix;

-- ── 5. constraints ─────────────────────────────────────────────────────────
-- An override to zero keeps the row and its history (B6).
ALTER TABLE payroll_deductions DROP CONSTRAINT payroll_deductions_amount_pos;
ALTER TABLE payroll_deductions ADD CONSTRAINT payroll_deductions_amount_pos
    CHECK (amount_piastres IS NULL OR amount_piastres > 0 OR overridden_at IS NOT NULL);

-- Two periods of one business never overlap: each would pay the base salary
-- and collect an installment (B8).
CREATE EXTENSION IF NOT EXISTS btree_gist;
ALTER TABLE payroll_periods ADD CONSTRAINT payroll_periods_no_overlap
    EXCLUDE USING gist (org_id WITH =, daterange(start_date, end_date, '[]') WITH &&);

-- ── 6. per-shift overtime rates ────────────────────────────────────────────
ALTER TABLE work_shifts
    ADD COLUMN ot_day_multiplier   numeric(4,2) CHECK (ot_day_multiplier   IS NULL OR ot_day_multiplier   > 0),
    ADD COLUMN ot_night_multiplier numeric(4,2) CHECK (ot_night_multiplier IS NULL OR ot_night_multiplier > 0);

-- ── 7. separate deduction limit (AD-5) ─────────────────────────────────────
-- Id 245: chosen away from the next free id so the parallel Phase B agents
-- adding capabilities in the same band do not collide.
INSERT INTO capabilities (id, key, legacy_resource, legacy_action, tier, defaults, core, approval, protected)
VALUES (245, 'hr.deductions.create', NULL, NULL, 'configurable', 'om', '', true, false)
ON CONFLICT (id) DO NOTHING;
-- Every system Owner and Branch manager role holds it like the template says;
-- the manager's grant carries the same limit the bonus grant does today.
INSERT INTO org_role_grants (org_role_id, org_id, capability_id, limits, source, template_version)
SELECT r.id, r.org_id, 245,
       CASE WHEN r.kind::text = 'branch_manager'
            THEN COALESCE((SELECT g.limits FROM org_role_grants g
                            WHERE g.org_role_id = r.id AND g.capability_id = 227), '{"max_amount": 100000}'::jsonb)
            ELSE '{}'::jsonb END,
       'template', 0
  FROM org_roles r
 WHERE r.is_system AND r.deleted_at IS NULL
   AND position(authz_kind_letter(r.kind::text) IN 'om') > 0
ON CONFLICT (org_role_id, capability_id) DO NOTHING;
SELECT authz_bump_epoch(o.id) FROM organizations o;
