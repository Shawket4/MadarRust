-- Dawam RU-1: nobody clocks in until the business has saved its rules.
-- `rules_saved_at` is set by the owner's save (the dashboard's set-up step);
-- businesses already clocking in on the defaults get a saved row now, so a
-- deploy never locks a working team out.
ALTER TABLE attendance_settings ADD COLUMN rules_saved_at timestamptz;
UPDATE attendance_settings SET rules_saved_at = updated_at WHERE branch_id IS NULL;
INSERT INTO attendance_settings (org_id, rules_saved_at)
SELECT DISTINCT a.org_id, now()
  FROM attendance_records a
 WHERE NOT EXISTS (SELECT 1 FROM attendance_settings s WHERE s.org_id = a.org_id AND s.branch_id IS NULL);

-- RU-9: night is 22:00–06:00 unless the business says otherwise; it prices
-- night overtime (RU-8) and marks the late and night shifts suggestions weigh.
ALTER TABLE attendance_settings
    ADD COLUMN night_start time NOT NULL DEFAULT '22:00',
    ADD COLUMN night_end   time NOT NULL DEFAULT '06:00',
    -- SC-12: how much the gender default weighs in suggestions. New businesses
    -- start on `soft`; the owner can switch it `off` or to `hard`.
    ADD COLUMN gender_mode text NOT NULL DEFAULT 'soft'
        CONSTRAINT attendance_settings_gender_mode_chk CHECK (gender_mode IN ('off', 'soft', 'hard'));

-- RU-11: weekly rest days come from the roster (an empty day is a day off);
-- the unused weekend setting goes.
ALTER TABLE attendance_settings DROP COLUMN weekend_days;
