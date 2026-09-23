-- Dawam RQ-2, RQ-3: leave has no types and no balances — the employee asks
-- for full or half days with a note, and the manager approves each as paid
-- or unpaid. A type is still accepted (older clients send one) but never
-- required.
ALTER TABLE staff_requests DROP CONSTRAINT staff_requests_shape_chk;
ALTER TABLE staff_requests ADD CONSTRAINT staff_requests_shape_chk CHECK (
    CASE kind
        WHEN 'leave' THEN end_date IS NOT NULL AND from_time IS NULL AND to_time IS NULL
            AND attendance_record_id IS NULL
        WHEN 'late_arrival' THEN to_time IS NOT NULL AND from_time IS NULL AND end_date IS NULL
            AND leave_type_id IS NULL AND attendance_record_id IS NULL
        WHEN 'early_departure' THEN from_time IS NOT NULL AND to_time IS NULL AND end_date IS NULL
            AND leave_type_id IS NULL AND attendance_record_id IS NULL
        WHEN 'excuse' THEN from_time IS NOT NULL AND to_time IS NOT NULL AND end_date IS NULL
            AND leave_type_id IS NULL AND attendance_record_id IS NULL
        WHEN 'mission' THEN title IS NOT NULL AND end_date IS NOT NULL AND leave_type_id IS NULL
            AND attendance_record_id IS NULL
        WHEN 'correction' THEN attendance_record_id IS NOT NULL
            AND (from_time IS NOT NULL OR to_time IS NOT NULL) AND end_date IS NULL
            AND leave_type_id IS NULL
        ELSE false
    END
);
