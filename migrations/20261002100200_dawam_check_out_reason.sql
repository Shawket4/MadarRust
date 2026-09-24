-- Mac E2E BC-1 (AT-10): a manager's (or the till's) punch-OUT wrote its
-- reason over the punch-IN's (punch_reason), so why the person was punched
-- in survived only in a notification. The out-reason gets its own column;
-- punch_reason keeps the in-reason. Same table: its grants and RLS policy
-- already cover the new column.
ALTER TABLE attendance_records ADD COLUMN check_out_reason text;
