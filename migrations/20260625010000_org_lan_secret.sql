-- Per-org LAN relay secret (Phase E — LAN offline relay).
--
-- A stable 32-byte random key, shipped to devices inside the offline-auth bundle
-- they already fetch on login. Each device derives a per-BRANCH HMAC-SHA256 subkey
-- from it and signs every LAN relay message, so only branch-provisioned devices
-- (which pulled the bundle via an authenticated login) are trusted — without this
-- the LAN relay would be an injection vector on the shared Wi-Fi.
--
-- Why a dedicated column and not "derive from the bundle's PIN hashes": the key must
-- be STABLE across bundle refreshes (a refresh re-stamps generated_at and a single
-- PIN change would rotate any hash-derived key, silently splitting the mesh). A
-- column-level random default backfills every existing org with its own distinct
-- secret and auto-mints one for every new org (the volatile default is evaluated
-- per row), so no application code or provisioning flow changes.
CREATE EXTENSION IF NOT EXISTS pgcrypto;

ALTER TABLE organizations
    ADD COLUMN IF NOT EXISTS lan_secret bytea NOT NULL DEFAULT gen_random_bytes(32);
