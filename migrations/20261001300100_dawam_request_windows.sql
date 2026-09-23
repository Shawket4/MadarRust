-- Dawam Phase B (rules): requests measured on the shift they are for.
--
-- 1. RQ-11 (night shifts, split days): two requests of one kind may not
--    overlap. The old rule compared `on_date + time`, so a late arrival of a
--    night shift after midnight sat at the START of its business date, and
--    two late arrivals of a split day (morning and evening) both began at
--    00:00 and collided. The server now stores each timed request's window
--    in the branch's wall clock, resolved against the shift
--    (`window_from` / `window_to`: a late arrival runs from the shift's start
--    to the arrival, an early departure from the departure to the shift's
--    end, an excuse's times land on the shift's side of midnight). Rows
--    without a resolved window (older rows, an unrostered day, leave and
--    missions) keep the old expression, so nothing existing changes.
-- 2. RQ-9: a correction can fix a rostered shift that has no attendance
--    record yet (no punch, before the sweep): it names the shift instead of
--    a record, and approving it writes the record. One pending record-less
--    correction per shift and day; once approved it points at the record it
--    wrote, and the per-record rule takes over.

ALTER TABLE staff_requests
    ADD COLUMN window_from timestamp,
    ADD COLUMN window_to   timestamp;
COMMENT ON COLUMN staff_requests.window_from IS
    'Branch wall clock: where the excused window starts, resolved on its shift (NULL = on_date + from_time).';
COMMENT ON COLUMN staff_requests.window_to IS
    'Branch wall clock: where the excused window ends, resolved on its shift (NULL = end/on_date + to_time).';

ALTER TABLE staff_requests DROP CONSTRAINT staff_requests_no_overlap;
ALTER TABLE staff_requests ADD CONSTRAINT staff_requests_no_overlap EXCLUDE USING gist (
    employee_id WITH =,
    kind WITH =,
    tsrange(
        COALESCE(window_from, on_date + COALESCE(from_time, '00:00'::time)),
        COALESCE(window_to, COALESCE(end_date, on_date) + COALESCE(to_time, '24:00'::time)),
        '[)'
    ) WITH &&
) WHERE (kind <> 'correction' AND status IN ('pending', 'approved'));

-- A correction names its record, or (no record yet) its shift.
ALTER TABLE staff_requests DROP CONSTRAINT staff_requests_shape_chk;
ALTER TABLE staff_requests ADD CONSTRAINT staff_requests_shape_chk CHECK (
CASE kind
    WHEN 'leave' THEN end_date IS NOT NULL AND from_time IS NULL AND to_time IS NULL AND attendance_record_id IS NULL
    WHEN 'late_arrival' THEN to_time IS NOT NULL AND from_time IS NULL AND end_date IS NULL AND leave_type_id IS NULL AND attendance_record_id IS NULL
    WHEN 'early_departure' THEN from_time IS NOT NULL AND to_time IS NULL AND end_date IS NULL AND leave_type_id IS NULL AND attendance_record_id IS NULL
    WHEN 'excuse' THEN from_time IS NOT NULL AND to_time IS NOT NULL AND (end_date IS NULL OR end_date = on_date + 1) AND leave_type_id IS NULL AND attendance_record_id IS NULL
    WHEN 'mission' THEN title IS NOT NULL AND end_date IS NOT NULL AND leave_type_id IS NULL AND attendance_record_id IS NULL
    WHEN 'correction' THEN (attendance_record_id IS NOT NULL OR work_shift_id IS NOT NULL)
        AND (from_time IS NOT NULL OR to_time IS NOT NULL) AND end_date IS NULL AND leave_type_id IS NULL
    ELSE false
END);

ALTER TABLE staff_requests DROP CONSTRAINT staff_requests_shift_kind_chk;
ALTER TABLE staff_requests ADD CONSTRAINT staff_requests_shift_kind_chk CHECK (
    work_shift_id IS NULL OR kind IN ('late_arrival', 'early_departure', 'excuse', 'correction'));

CREATE UNIQUE INDEX staff_requests_live_shift_correction_unique
    ON staff_requests (employee_id, on_date, work_shift_id)
    WHERE kind = 'correction' AND attendance_record_id IS NULL AND status = 'pending';
