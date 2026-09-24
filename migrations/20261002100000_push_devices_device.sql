-- POS pushes (`app = 'pos'`) need to know WHICH install a token belongs to, so
-- the server can skip a till whose realtime stream is up (SSE is the primary
-- path; FCM is only the fallback for a till that is closed, backgrounded or
-- offline). The install id is the `X-Madar-Device` header both the push
-- registration and `/realtime/stream` carry. NULL for apps that send none
-- (Dawam) and for registrations from before this column.
ALTER TABLE push_devices ADD COLUMN device_id uuid;

-- The POS fan-out reads every live `pos` device of one org.
CREATE INDEX push_devices_org_app_idx ON push_devices (org_id, app) WHERE revoked_at IS NULL;

COMMENT ON COLUMN push_devices.device_id IS
    'The install (X-Madar-Device) that registered this token; matches a live /realtime/stream connection. NULL when the app sent none.';
