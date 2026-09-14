-- Client version telemetry (LEGACY_REMOVAL.md, Phase T).
--
-- One row per device (or, for a client that names no device, per org + branch +
-- client string): which client and app version it runs, when it was first and
-- last seen, and the last legacy code path it took. Written by the
-- `client_seen` middleware in a background upsert, throttled per device, so it
-- costs nothing on the request path. Read by `GET /devices/client-versions`
-- (which devices still hit legacy paths) and by the removal gates.
CREATE TABLE client_seen (
    org_id            uuid        NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- `d:<device uuid>` when the request named a device (X-Madar-Device, or the
    -- legacy X-Madar-Device-Id), else `c:<branch uuid or ->:<client>`.
    seen_key          text        NOT NULL CHECK (btrim(seen_key) <> ''),
    branch_id         uuid        NULL REFERENCES branches(id) ON DELETE SET NULL,
    -- Not a FK: old clients are never registered in `devices`.
    device_id         uuid        NULL,
    -- X-Madar-Client when sent (`pos/0.7.2 (ios)`), else the User-Agent. Latest wins.
    client            text        NULL,
    -- The semver parsed from `client` (`<app>/<semver>`), when there is one.
    app_version       text        NULL,
    first_seen_at     timestamptz NOT NULL DEFAULT now(),
    last_seen_at      timestamptz NOT NULL DEFAULT now(),
    -- The most recent legacy path this client took (`legacy_hit` kind + request path).
    last_legacy_kind  text        NULL,
    last_legacy_path  text        NULL,
    last_legacy_at    timestamptz NULL,
    -- Every legacy kind this client has ever hit (distinct).
    legacy_kinds      text[]      NOT NULL DEFAULT '{}',
    PRIMARY KEY (org_id, seen_key)
);
CREATE INDEX idx_client_seen_legacy ON client_seen (org_id, last_legacy_at DESC) WHERE last_legacy_at IS NOT NULL;
CREATE INDEX idx_client_seen_device ON client_seen (device_id) WHERE device_id IS NOT NULL;

ALTER TABLE client_seen ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON client_seen
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE client_seen TO madar_app;
