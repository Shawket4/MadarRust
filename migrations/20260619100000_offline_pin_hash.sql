-- Offline-auth (POS rebuild, Layer 3). A teller's OFFLINE PIN verifier —
-- argon2id, derived at online login, DISTINCT from the bcrypt login `pin_hash`
-- so a leaked offline-auth bundle is never the login credential. NULL until the
-- teller has logged in online at least once. Surfaced only via the org-scoped
-- GET /orgs/{id}/offline-auth-bundle, never in normal user reads.
ALTER TABLE public.users
    ADD COLUMN IF NOT EXISTS offline_pin_hash text;
