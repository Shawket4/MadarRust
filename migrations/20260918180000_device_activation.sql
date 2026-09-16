-- Device activation codes (POS_SIGNIN_OVERHAUL.md §4). Replaces the manager
-- email login for binding a tablet to an org and branch: an owner issues a
-- short numeric code in the dashboard, the tablet enters it, and nothing about
-- a person is involved.
--
-- The code is kept in plain text: it is short-lived (24 h), single-use,
-- revocable, and the dashboard shows it while it is free (Foodics' green/red
-- list). Activation is unauthenticated, so a LIVE code must be unique across
-- every org — the partial unique index below.
CREATE TABLE IF NOT EXISTS device_activation_codes (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id          uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id       uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    code            text NOT NULL CHECK (code ~ '^[0-9]{8}$'),
    label           text NULL CHECK (label IS NULL OR btrim(label) <> ''),
    kind            text NOT NULL DEFAULT 'pos' CHECK (kind IN ('pos','kds','waiter')),
    created_by      uuid NULL REFERENCES users(id) ON DELETE SET NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    expires_at      timestamptz NOT NULL DEFAULT now() + interval '24 hours',
    used_at         timestamptz NULL,
    -- No foreign key on purpose: a code's history outlives the device row, and
    -- the tills rework's down script must still be able to drop `devices`.
    used_by_device  uuid NULL,
    revoked_at      timestamptz NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS device_activation_codes_live_code
    ON device_activation_codes (code)
    WHERE used_at IS NULL AND revoked_at IS NULL;
CREATE INDEX IF NOT EXISTS device_activation_codes_branch
    ON device_activation_codes (branch_id, created_at DESC);

ALTER TABLE device_activation_codes ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON device_activation_codes;
CREATE POLICY tenant_isolation ON device_activation_codes
    USING (org_id = NULLIF(current_setting('app.org_id', true), '')::uuid);
GRANT SELECT, INSERT, UPDATE ON device_activation_codes TO madar_app;

-- The device's own long-lived credential, issued once at activation. Only a
-- SHA-256 of the random token is stored; retiring the device clears it.
ALTER TABLE devices ADD COLUMN IF NOT EXISTS credential_hash text NULL;
