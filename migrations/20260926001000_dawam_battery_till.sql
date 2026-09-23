-- CL-12: silence after a low battery reads "phone likely died", never
-- "left the branch". Also the one-time "charge your phone" note per shift.
ALTER TABLE attendance_flags DROP CONSTRAINT attendance_flags_kind_chk;
ALTER TABLE attendance_flags ADD CONSTRAINT attendance_flags_kind_chk CHECK (kind IN
    ('left_mid_shift', 'suspicious', 'tracking_off', 'time_unverified', 'new_phone', 'cover',
     'phone_died'));
ALTER TABLE attendance_records ADD COLUMN low_battery_warned_at timestamptz;

-- AV-8: a till pay-out tagged "expense advance to (employee)" is logged in
-- Dawam; correcting the pay-out removes it.
ALTER TABLE expense_advances
    ADD COLUMN till_movement_id uuid UNIQUE REFERENCES till_cash_movements(id) ON DELETE CASCADE;
