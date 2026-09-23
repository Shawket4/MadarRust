-- Dawam Phase B (platform, APP-6, audit 06 B7): a notification may carry a
-- dedupe key, and one key reaches a person once. A flag re-seen on every
-- ping while someone stays away (`attendance_flags … ON CONFLICT DO UPDATE`)
-- no longer writes an inbox row and pushes to every manager each time: the
-- sender keys the notice by the flag, and a repeat is dropped before the push.
ALTER TABLE staff_notifications ADD COLUMN IF NOT EXISTS dedupe_key text;
CREATE UNIQUE INDEX IF NOT EXISTS staff_notifications_dedupe_idx
    ON staff_notifications (employee_id, dedupe_key)
    WHERE dedupe_key IS NOT NULL;

-- The table already has RLS (tenant_isolation) and the tenant role's grants
-- (20260927000100); restated so this migration stands on its own.
GRANT SELECT, INSERT, UPDATE, DELETE ON staff_notifications TO madar_app;
