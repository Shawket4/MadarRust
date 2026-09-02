-- Punch corrections — the sixth request kind.
--
-- The other five kinds are all EXCUSED WINDOWS: "forgive this part of my day".
-- A correction is not. It says "the clock is wrong, here is what actually
-- happened" — a proposed edit to an attendance record, which on approval is
-- WRITTEN to that record and repriced. So it carries the record it fixes, and
-- `day_adjustments` deliberately ignores it: forgiving a day and correcting a
-- day are different acts, and conflating them would let a correction silently
-- waive a penalty it never claimed to.
--
--   kind         on_date      from_time           to_time             extra
--   ─────────────────────────────────────────────────────────────────────────
--   correction   single day   proposed check-IN   proposed check-OUT  record id
--
-- At least one of the two times must be present — a correction that proposes
-- nothing is not a correction.

ALTER TABLE staff_requests
    ADD COLUMN attendance_record_id uuid REFERENCES attendance_records(id) ON DELETE CASCADE;

COMMENT ON COLUMN staff_requests.attendance_record_id IS
    'The record a `correction` proposes to fix. NULL for every other kind.';

-- Widen the two CHECKs that enumerate kinds.
ALTER TABLE staff_requests DROP CONSTRAINT staff_requests_kind_chk;
ALTER TABLE staff_requests ADD CONSTRAINT staff_requests_kind_chk CHECK (
    kind IN ('leave', 'late_arrival', 'early_departure', 'excuse', 'mission', 'correction')
);

ALTER TABLE staff_requests DROP CONSTRAINT staff_requests_shape_chk;
ALTER TABLE staff_requests ADD CONSTRAINT staff_requests_shape_chk CHECK (
    CASE kind
        WHEN 'leave' THEN
            leave_type_id IS NOT NULL AND end_date IS NOT NULL
            AND from_time IS NULL AND to_time IS NULL
            AND attendance_record_id IS NULL
        WHEN 'late_arrival' THEN
            to_time IS NOT NULL AND from_time IS NULL AND end_date IS NULL
            AND leave_type_id IS NULL AND attendance_record_id IS NULL
        WHEN 'early_departure' THEN
            from_time IS NOT NULL AND to_time IS NULL AND end_date IS NULL
            AND leave_type_id IS NULL AND attendance_record_id IS NULL
        WHEN 'excuse' THEN
            from_time IS NOT NULL AND to_time IS NOT NULL AND end_date IS NULL
            AND leave_type_id IS NULL AND attendance_record_id IS NULL
        WHEN 'mission' THEN
            title IS NOT NULL AND end_date IS NOT NULL AND leave_type_id IS NULL
            AND attendance_record_id IS NULL
        WHEN 'correction' THEN
            attendance_record_id IS NOT NULL
            AND (from_time IS NOT NULL OR to_time IS NOT NULL)
            AND end_date IS NULL AND leave_type_id IS NULL
        ELSE false
    END
);

-- A correction proposes BOTH ends of a punch, so `to_time > from_time` (already
-- enforced org-wide) is exactly right for it too — no extra constraint needed.

-- One live correction per record, not merely per day: a day with two shifts can
-- legitimately have a missing punch on each.
CREATE UNIQUE INDEX staff_requests_live_correction_unique
    ON staff_requests (attendance_record_id)
    WHERE status IN ('pending', 'approved') AND kind = 'correction';
