-- Owner decision D8 (24 Sep 2026, AD-9, AT-10): rejecting a bonus or a
-- deduction needs a reason, kept on the line beside who decided and when
-- (salary advances already keep theirs in salary_advances.decision_note).
-- Same tables: their grants and RLS policies already cover the new columns.
ALTER TABLE payroll_bonuses ADD COLUMN decision_note text;
ALTER TABLE payroll_deductions ADD COLUMN decision_note text;
