-- Dawam Phase B · clocking in, presence, location, privacy (audit 03, AT-4/AT-5).
--
-- 1. CL-16: a manager's punch for someone is recorded as `manager`, not
--    `manual` (which stays the dashboard's hand-entered record). Existing
--    manager punches (a `manual` punch that carries a punch reason) move over.
-- 2. AT-5: the privacy notice is accepted per employee AND phone, and the
--    server keeps when (`staff_devices.privacy_accepted_at`). A new phone
--    starts unaccepted; pings and located punches are refused until it is.
-- 3. CL-11: a ping whose offline time could not be verified says so.
-- 4. AT-4: when a payroll month is approved its exact coordinates are wiped,
--    and the period records when that happened.

ALTER TABLE attendance_records DROP CONSTRAINT attendance_records_in_method_chk;
ALTER TABLE attendance_records DROP CONSTRAINT attendance_records_out_method_chk;
ALTER TABLE attendance_records ADD CONSTRAINT attendance_records_in_method_chk
    CHECK (check_in_method IS NULL OR check_in_method = ANY (ARRAY[
        'mobile_gps', 'manual', 'manager', 'auto', 'offline', 'cover', 'till', 'kiosk', 'correction']));
ALTER TABLE attendance_records ADD CONSTRAINT attendance_records_out_method_chk
    CHECK (check_out_method IS NULL OR check_out_method = ANY (ARRAY[
        'mobile_gps', 'manual', 'manager', 'auto', 'offline', 'cover', 'till', 'kiosk', 'correction']));

UPDATE attendance_records SET check_in_method = 'manager'
 WHERE check_in_method = 'manual' AND punch_reason IS NOT NULL AND punch_reason <> '';
UPDATE attendance_records SET check_out_method = 'manager'
 WHERE check_out_method = 'manual' AND punch_reason IS NOT NULL AND punch_reason <> '';

ALTER TABLE staff_devices ADD COLUMN privacy_accepted_at timestamptz;

ALTER TABLE attendance_pings ADD COLUMN time_unverified boolean NOT NULL DEFAULT false;

ALTER TABLE payroll_periods ADD COLUMN coordinates_wiped_at timestamptz;

-- The wipe and the approval sweep look periods up by org and status.
CREATE INDEX IF NOT EXISTS attendance_pings_org_at_idx ON attendance_pings (org_id, at);

GRANT SELECT, INSERT, UPDATE, DELETE ON attendance_records, attendance_pings, staff_devices,
    payroll_periods TO madar_app;
