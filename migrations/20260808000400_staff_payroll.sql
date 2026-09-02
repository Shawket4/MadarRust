-- Staff & attendance — part 5 of 5: payroll.
--
--   Net = base + overtime + bonuses − deductions − advance installment
--
-- A PAYSLIP IS AN IMMUTABLE SNAPSHOT. Once a period is generated, editing an
-- attendance record or adding a deduction inside that window does NOT retro-
-- change the payslip — the operator must regenerate the period explicitly, which
-- is only allowed while it is still `draft`/`generated`. That matches how a
-- payroll actually works: you cannot un-pay a month.
--
-- All money is PIASTRES (bigint). Percent-based adjustments carry a rate
-- instead, resolved against base salary at generation time and frozen into the
-- payslip breakdown.

CREATE TABLE payroll_deductions (
    id                   uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id               uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id              uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- Exactly one of the two is set (see the CHECK below).
    amount_piastres      bigint,
    percent_of_base      numeric(5,2),
    reason               text NOT NULL,
    effective_date       date NOT NULL,
    -- 'late_penalty' / 'absence' rows are machine-generated from attendance;
    -- 'manual' is an operator entry.
    source               text NOT NULL DEFAULT 'manual',
    attendance_record_id uuid REFERENCES attendance_records(id) ON DELETE SET NULL,
    status               text NOT NULL DEFAULT 'approved',
    created_by           uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT payroll_deductions_source_chk
        CHECK (source IN ('manual', 'late_penalty', 'absence')),
    CONSTRAINT payroll_deductions_status_chk
        CHECK (status IN ('pending', 'approved', 'rejected')),
    CONSTRAINT payroll_deductions_one_basis CHECK (
        (amount_piastres IS NOT NULL AND percent_of_base IS NULL)
     OR (amount_piastres IS NULL     AND percent_of_base IS NOT NULL)
    ),
    CONSTRAINT payroll_deductions_amount_pos
        CHECK (amount_piastres IS NULL OR amount_piastres > 0),
    CONSTRAINT payroll_deductions_percent_range
        CHECK (percent_of_base IS NULL OR (percent_of_base > 0 AND percent_of_base <= 100))
);

CREATE INDEX payroll_deductions_user_idx ON payroll_deductions (user_id, effective_date DESC);
CREATE INDEX payroll_deductions_org_idx  ON payroll_deductions (org_id, effective_date DESC);
-- One machine-generated penalty per attendance record — re-running the nightly
-- job must not stack duplicate deductions onto the same late arrival.
CREATE UNIQUE INDEX payroll_deductions_auto_unique
    ON payroll_deductions (attendance_record_id, source)
    WHERE attendance_record_id IS NOT NULL AND source <> 'manual';

CREATE TABLE payroll_bonuses (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id          uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id         uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    amount_piastres bigint,
    percent_of_base numeric(5,2),
    reason          text NOT NULL,
    effective_date  date NOT NULL,
    source          text NOT NULL DEFAULT 'manual',
    status          text NOT NULL DEFAULT 'approved',
    created_by      uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT payroll_bonuses_source_chk
        CHECK (source IN ('manual', 'performance', 'commission')),
    CONSTRAINT payroll_bonuses_status_chk
        CHECK (status IN ('pending', 'approved', 'rejected')),
    CONSTRAINT payroll_bonuses_one_basis CHECK (
        (amount_piastres IS NOT NULL AND percent_of_base IS NULL)
     OR (amount_piastres IS NULL     AND percent_of_base IS NOT NULL)
    ),
    CONSTRAINT payroll_bonuses_amount_pos
        CHECK (amount_piastres IS NULL OR amount_piastres > 0),
    CONSTRAINT payroll_bonuses_percent_pos
        CHECK (percent_of_base IS NULL OR percent_of_base > 0)
);

CREATE INDEX payroll_bonuses_user_idx ON payroll_bonuses (user_id, effective_date DESC);
CREATE INDEX payroll_bonuses_org_idx  ON payroll_bonuses (org_id, effective_date DESC);

CREATE TABLE salary_advances (
    id                           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id                       uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id                      uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    amount_piastres              bigint  NOT NULL,
    installments                 integer NOT NULL DEFAULT 1,
    monthly_installment_piastres bigint  NOT NULL,
    -- Decremented by each generated payslip. Hits zero → 'settled'.
    remaining_piastres           bigint  NOT NULL,
    reason                       text,
    status                       text NOT NULL DEFAULT 'pending',
    decided_by                   uuid REFERENCES users(id) ON DELETE SET NULL,
    decided_at                   timestamptz,
    decision_note                text,
    created_at                   timestamptz NOT NULL DEFAULT now(),
    updated_at                   timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT salary_advances_status_chk
        CHECK (status IN ('pending', 'approved', 'rejected', 'settled', 'cancelled')),
    CONSTRAINT salary_advances_amount_pos       CHECK (amount_piastres              >  0),
    CONSTRAINT salary_advances_installments_pos CHECK (installments                 >  0),
    CONSTRAINT salary_advances_monthly_pos      CHECK (monthly_installment_piastres >  0),
    CONSTRAINT salary_advances_remaining_range
        CHECK (remaining_piastres >= 0 AND remaining_piastres <= amount_piastres)
);

CREATE INDEX salary_advances_user_idx ON salary_advances (user_id, created_at DESC);
CREATE INDEX salary_advances_org_idx  ON salary_advances (org_id, status);
-- The generator's hot lookup: live advances still owing money.
CREATE INDEX salary_advances_active_idx ON salary_advances (user_id)
    WHERE status = 'approved' AND remaining_piastres > 0;

CREATE TABLE payroll_periods (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id             uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name               text NOT NULL,
    start_date         date NOT NULL,
    end_date           date NOT NULL,
    status             text NOT NULL DEFAULT 'draft',
    employee_count     integer NOT NULL DEFAULT 0,
    total_net_piastres bigint  NOT NULL DEFAULT 0,
    generated_at       timestamptz,
    generated_by       uuid REFERENCES users(id) ON DELETE SET NULL,
    paid_at            timestamptz,
    closed_at          timestamptz,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT payroll_periods_status_chk
        CHECK (status IN ('draft', 'generated', 'paid', 'closed')),
    CONSTRAINT payroll_periods_dates_ordered CHECK (end_date >= start_date),
    CONSTRAINT payroll_periods_name_len      CHECK (char_length(name) BETWEEN 1 AND 120)
);

CREATE UNIQUE INDEX payroll_periods_org_span_key
    ON payroll_periods (org_id, start_date, end_date);

CREATE TABLE payslips (
    id                            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id                        uuid NOT NULL REFERENCES organizations(id)   ON DELETE CASCADE,
    payroll_period_id             uuid NOT NULL REFERENCES payroll_periods(id) ON DELETE CASCADE,
    user_id                       uuid NOT NULL REFERENCES users(id)           ON DELETE CASCADE,

    base_salary_piastres          bigint NOT NULL DEFAULT 0,
    worked_days                   numeric(6,2) NOT NULL DEFAULT 0,
    absent_days                   numeric(6,2) NOT NULL DEFAULT 0,
    leave_days                    numeric(6,2) NOT NULL DEFAULT 0,
    late_minutes                  integer NOT NULL DEFAULT 0,
    overtime_minutes              integer NOT NULL DEFAULT 0,

    overtime_piastres             bigint NOT NULL DEFAULT 0,
    bonuses_piastres              bigint NOT NULL DEFAULT 0,
    deductions_piastres           bigint NOT NULL DEFAULT 0,
    advance_installment_piastres  bigint NOT NULL DEFAULT 0,
    net_piastres                  bigint NOT NULL DEFAULT 0,

    -- Frozen line-by-line detail (every bonus/deduction/advance row that fed the
    -- totals, with its own id and label) so a payslip is explainable years later
    -- even after the source rows are edited or deleted.
    breakdown                     jsonb NOT NULL DEFAULT '{}'::jsonb,
    generated_at                  timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT payslips_components_nonneg CHECK (
        base_salary_piastres >= 0 AND overtime_piastres >= 0
        AND bonuses_piastres >= 0 AND deductions_piastres >= 0
        AND advance_installment_piastres >= 0
    ),
    CONSTRAINT payslips_days_nonneg
        CHECK (worked_days >= 0 AND absent_days >= 0 AND leave_days >= 0),
    CONSTRAINT payslips_minutes_nonneg
        CHECK (late_minutes >= 0 AND overtime_minutes >= 0)
);

CREATE UNIQUE INDEX payslips_period_user_key ON payslips (payroll_period_id, user_id);
CREATE INDEX payslips_user_idx ON payslips (user_id, generated_at DESC);

-- ── RLS ──────────────────────────────────────────────────────────────────────
ALTER TABLE payroll_deductions ENABLE ROW LEVEL SECURITY;
ALTER TABLE payroll_bonuses    ENABLE ROW LEVEL SECURITY;
ALTER TABLE salary_advances    ENABLE ROW LEVEL SECURITY;
ALTER TABLE payroll_periods    ENABLE ROW LEVEL SECURITY;
ALTER TABLE payslips           ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON payroll_deductions FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
CREATE POLICY tenant_isolation ON payroll_bonuses FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
CREATE POLICY tenant_isolation ON salary_advances FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
CREATE POLICY tenant_isolation ON payroll_periods FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
CREATE POLICY tenant_isolation ON payslips FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));

GRANT ALL ON TABLE payroll_deductions TO sufrix;
GRANT ALL ON TABLE payroll_bonuses    TO sufrix;
GRANT ALL ON TABLE salary_advances    TO sufrix;
GRANT ALL ON TABLE payroll_periods    TO sufrix;
GRANT ALL ON TABLE payslips           TO sufrix;
