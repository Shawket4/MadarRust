-- Dawam Phase B (rules): ONE deduction source for unpaid excused time.
--
-- Two names meant the same money: the flag path wrote `unpaid_excuse`
-- (a manager excusing a mid-shift absence without pay) and the request path
-- wrote `excused_unpaid` (an approved but unpaid excuse or early departure,
-- RQ-7). The orchestrator's decision #2: the one name is `excused_unpaid`.
--
-- Old `unpaid_excuse` rows become `excused_unpaid`; the CHECK then narrows so
-- nothing writes `unpaid_excuse` again.
--
-- A manager's row from a flag carries `created_by`; `penalties` never
-- rewrites or deletes such a row (AT-7), so the rule's own recompute of the
-- shift can't undo the manager's line.
--
-- NOTE for merging: other Phase B branches may re-create this constraint too;
-- the merged list must hold every value they add, WITHOUT `unpaid_excuse`.

-- 1. Convert. The automatic-row unique index is per (record, source): where
--    that record already has an `excused_unpaid` row, the converted row keeps its money but lets
--    go of the record link, so both lines survive and nothing merges silently.
UPDATE payroll_deductions d
   SET source = 'excused_unpaid',
       attendance_record_id = CASE
           WHEN d.attendance_record_id IS NOT NULL AND EXISTS (
                SELECT 1 FROM payroll_deductions o
                 WHERE o.attendance_record_id = d.attendance_record_id
                   AND o.source = 'excused_unpaid')
           THEN NULL
           ELSE d.attendance_record_id
       END,
       updated_at = now()
 WHERE d.source = 'unpaid_excuse';

-- 2. Narrow the CHECK.
ALTER TABLE payroll_deductions DROP CONSTRAINT payroll_deductions_source_chk;
ALTER TABLE payroll_deductions ADD CONSTRAINT payroll_deductions_source_chk CHECK (source IN
    ('manual', 'late_penalty', 'absence', 'left_mid_shift', 'carry', 'excused_unpaid'));
