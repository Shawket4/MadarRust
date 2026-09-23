-- A cover is a second record on the same date and shift template: Sara worked
-- her own Morning shift and then covers Omar's (CV-1). The one-record-per-day
-- rule stays for a person's own shifts; a cover is unique per shift it covers.
DROP INDEX attendance_records_unique_day;
CREATE UNIQUE INDEX attendance_records_unique_day ON attendance_records (
    user_id, business_date,
    COALESCE(work_shift_id, '00000000-0000-0000-0000-000000000000'::uuid)
) WHERE covered_user_id IS NULL;
CREATE UNIQUE INDEX attendance_records_unique_cover ON attendance_records (
    user_id, business_date, work_shift_id, covered_user_id
) WHERE covered_user_id IS NOT NULL;
