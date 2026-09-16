-- A keyed fingerprint of each PIN, so a PIN can be FOUND instead of scanned
-- (POS_SIGNIN_OVERHAUL.md §2). HMAC-SHA256(server_key, org_id || 0x00 || pin),
-- computed in the application: the key lives in the server's environment and
-- must never reach this database, or the column would be a lookup table for
-- every PIN in the company.
--
-- Nullable, and NOT unique yet. Existing PINs are salted hashes that cannot be
-- compared to each other, so nothing can be backfilled here without the
-- plaintext; rows fill in as people sign in (§6) and as PINs are set or
-- changed. A row without one falls back to today's scan, so nothing breaks in
-- the meantime.
ALTER TABLE users ADD COLUMN IF NOT EXISTS pin_fingerprint bytea;

-- The lookup this exists for: one org, one fingerprint, one row.
CREATE INDEX IF NOT EXISTS users_pin_fingerprint_idx
    ON users (org_id, pin_fingerprint)
    WHERE pin_fingerprint IS NOT NULL AND deleted_at IS NULL;

COMMENT ON COLUMN users.pin_fingerprint IS
    'HMAC-SHA256(server key, org_id || 0x00 || pin). Lookup only — the salted pin_hash still does the verifying. Never returned over the API.';
