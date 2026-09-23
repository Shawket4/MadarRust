-- E2E D-B2 (AT-13, PAY-10): a line the SERVER writes carries a stable code and
-- its figures, so each client words it in its own language; `reason` stays the
-- English text for old clients. A person-typed reason has no code.
ALTER TABLE payroll_deductions
    ADD COLUMN IF NOT EXISTS reason_code text,
    ADD COLUMN IF NOT EXISTS reason_vars jsonb;

COMMENT ON COLUMN payroll_deductions.reason_code IS
    'late · absent_no_punch · unpaid_leave · absent_half_unpaid_leave · unpaid_excused_minutes · unpaid_excuse · left_mid_shift; NULL = a person''s own words';

-- Backfill the rows the server already wrote, from their exact English.
UPDATE payroll_deductions
   SET reason_code = 'late',
       reason_vars = jsonb_build_object('minutes', substring(reason FROM '^Late by (\d+) minutes$')::int)
 WHERE source = 'late_penalty' AND reason ~ '^Late by \d+ minutes$' AND reason_code IS NULL;
UPDATE payroll_deductions SET reason_code = 'absent_no_punch'
 WHERE source = 'absence' AND reason = 'Absent — no check-in recorded' AND reason_code IS NULL;
UPDATE payroll_deductions SET reason_code = 'unpaid_leave'
 WHERE source = 'absence' AND reason = 'Unpaid leave' AND reason_code IS NULL;
UPDATE payroll_deductions SET reason_code = 'absent_half_unpaid_leave'
 WHERE source = 'absence' AND reason = 'Absent from the worked half · unpaid half-day leave' AND reason_code IS NULL;
UPDATE payroll_deductions
   SET reason_code = 'unpaid_excused_minutes',
       reason_vars = jsonb_build_object('minutes', substring(reason FROM '^Unpaid excused time: (\d+) minutes$')::int)
 WHERE source = 'excused_unpaid' AND reason ~ '^Unpaid excused time: \d+ minutes$' AND reason_code IS NULL;
UPDATE payroll_deductions SET reason_code = 'unpaid_excuse'
 WHERE source = 'excused_unpaid' AND reason = 'Unpaid excuse' AND reason_code IS NULL;
UPDATE payroll_deductions SET reason_code = 'left_mid_shift'
 WHERE source = 'left_mid_shift' AND reason = 'Left mid-shift' AND reason_code IS NULL;
