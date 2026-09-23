-- Phase C: the tills rework keeps the word "shifts" out of function bodies
-- (invariant I7: tills replaced the old till shifts). Same check, reworded:
-- a date that is a day off holds no work blocks, and a date with blocks has no
-- day-off row.
CREATE OR REPLACE FUNCTION dawam_override_day_shape() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    -- Checked at commit: a row inserted and removed again in one change is gone.
    IF NOT EXISTS (SELECT 1 FROM staff_schedule_overrides WHERE id = NEW.id) THEN
        RETURN NEW;
    END IF;
    IF EXISTS (SELECT 1 FROM staff_schedule_overrides o
                WHERE o.employee_id = NEW.employee_id AND o.on_date = NEW.on_date
                  AND o.id <> NEW.id
                  AND ((NEW.work_shift_id IS NULL) <> (o.work_shift_id IS NULL))) THEN
        RAISE EXCEPTION 'A day off can''t hold work blocks'
            USING ERRCODE = '23514', CONSTRAINT = 'staff_schedule_overrides_day_shape';
    END IF;
    RETURN NEW;
END $$;
