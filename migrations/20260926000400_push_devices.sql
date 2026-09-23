-- Generic push-notification registry, replacing the Dawam-only
-- staff_devices.push_token/push_locale columns. One row per (app, token):
-- any authenticated user of any future app (dawam, dashboard, ...) can
-- register a device here, and a token always belongs to whoever last
-- registered it (a phone that changes owner rebinds).
CREATE TABLE push_devices (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id       uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id      uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    app          text NOT NULL,
    platform     text NOT NULL DEFAULT '',
    token        text NOT NULL,
    locale       text NOT NULL DEFAULT 'ar',
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    revoked_at   timestamptz
);
-- A live token names exactly one device; ON CONFLICT rebinds it instead of
-- erroring when it moves to a new user.
CREATE UNIQUE INDEX push_devices_live_token ON push_devices (token) WHERE revoked_at IS NULL;
CREATE INDEX push_devices_user_app_idx ON push_devices (user_id, app) WHERE revoked_at IS NULL;

ALTER TABLE push_devices ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON push_devices FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));

-- Backfill the live Dawam tokens already registered under staff_devices.
INSERT INTO push_devices (org_id, user_id, app, token, locale, created_at, last_seen_at)
SELECT org_id, user_id, 'dawam', push_token, push_locale, first_seen_at, last_seen_at
FROM staff_devices
WHERE push_token IS NOT NULL AND revoked_at IS NULL;

COMMENT ON COLUMN staff_devices.push_token IS
    'Superseded by push_devices (app=''dawam''); no longer written. Kept for old app builds reading /staff/me/push-token until it is retired.';
COMMENT ON COLUMN staff_devices.push_locale IS
    'Superseded by push_devices (app=''dawam''); no longer written.';
