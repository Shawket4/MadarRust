-- Make every automatic deduction a row a human can veto.
--
-- Two problems this fixes:
--
-- 1. LATE PENALTIES were rows, but nothing could waive one. A manager who agreed
--    someone had a good reason had no way to say so — the only options were to
--    delete the row (which the nightly sweep would recreate) or let it stand.
--
-- 2. ABSENCE DEDUCTIONS were not rows at all. They were computed inline inside
--    `compute_net_salary`, which meant a day's pay could vanish from a payslip
--    with no line explaining it and nothing to point at. From here on, absences
--    are ordinary `source='absence'` rows like everything else, and the net-salary
--    function stops knowing about absence entirely (see src/staff/rules.rs).
--
-- The audit trail is the point: `original_amount_piastres` always holds what the
-- RULE computed, so "why was Nour not docked in March?" has an answer forever.
-- Waiving zeroes a deduction WITHOUT deleting it, for the same reason.

ALTER TABLE payroll_deductions
    -- What the rule computed. Set on every machine-generated row; NULL for a
    -- hand-entered one (nothing was overridden, so there is no "original").
    ADD COLUMN IF NOT EXISTS original_amount_piastres bigint,

    ADD COLUMN IF NOT EXISTS overridden_at    timestamptz,
    ADD COLUMN IF NOT EXISTS overridden_by    uuid REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN IF NOT EXISTS override_reason  text,

    -- A waived row stays visible and keeps its amount; payroll simply skips it.
    ADD COLUMN IF NOT EXISTS waived_at        timestamptz,
    ADD COLUMN IF NOT EXISTS waived_by        uuid REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN IF NOT EXISTS waive_reason     text;

ALTER TABLE payroll_deductions
    -- Both interventions are audited or they did not happen.
    ADD CONSTRAINT payroll_deductions_override_reason_chk
        CHECK (overridden_at IS NULL OR override_reason IS NOT NULL),
    ADD CONSTRAINT payroll_deductions_waive_reason_chk
        CHECK (waived_at IS NULL OR waive_reason IS NOT NULL);

-- Payroll generation reads only live rows; this keeps that scan cheap.
CREATE INDEX IF NOT EXISTS payroll_deductions_live_idx
    ON payroll_deductions (org_id, effective_date)
    WHERE waived_at IS NULL AND status = 'approved';

-- Backfill: every existing machine-generated row records what it computed, so
-- the "original" column is meaningful from day one rather than only for rows
-- created after this migration.
UPDATE payroll_deductions
   SET original_amount_piastres = amount_piastres
 WHERE source <> 'manual' AND original_amount_piastres IS NULL;
