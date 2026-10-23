-- Server-side sessions for every dashboard, till and kitchen sign-in
-- (src/auth/sessions.rs). A bearer token now names its row (`sid`); a revoked
-- or expired row refuses the token at once (401 SESSION_REVOKED), whatever its
-- own `exp` says. Only a super admin lists or revokes them (/admin/sessions,
-- driven from madar-admin).
--
-- Read and written through the owner pool (the JWT check, sign-in, the
-- super-admin routes), which bypasses row security by ownership. Like every
-- public table it still enforces the flat tenant policy for `madar_app`, so a
-- tenant connection could only ever see its own org's sessions.
--
-- Tokens issued before this migration carry no `sid`. They are adopted on first
-- use as a row keyed by `legacy_token_hash` (SHA-256 of the token, hex), so they
-- can be revoked too, and simply run out at their own `exp`: nobody is signed
-- out by the deploy.
--
-- A revocation notifies `authz_user` (payload = the user), the channel the
-- fast-path listener already clears user-keyed entries on, so every instance
-- drops its cached answer at commit.
--
-- Additive only.

CREATE TABLE auth_sessions (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id           uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    org_id            uuid NULL REFERENCES organizations (id) ON DELETE CASCADE,
    branch_id         uuid NULL REFERENCES branches (id) ON DELETE SET NULL,
    role              user_role NOT NULL,
    kind              text NOT NULL
        CHECK (kind IN ('password', 'pin', 'device', 'signup', 'demo', 'legacy')),
    device_id         text NULL,
    client            text NULL,
    ip                inet NULL,
    user_agent        text NULL,
    legacy_token_hash text NULL,
    created_at        timestamptz NOT NULL DEFAULT now(),
    last_seen_at      timestamptz NOT NULL DEFAULT now(),
    expires_at        timestamptz NOT NULL,
    revoked_at        timestamptz NULL,
    revoked_by        uuid NULL REFERENCES users (id) ON DELETE SET NULL,
    revoke_reason     text NULL,
    CHECK ((revoked_at IS NULL) = (revoke_reason IS NULL))
);

CREATE INDEX auth_sessions_user_live ON auth_sessions (user_id) WHERE revoked_at IS NULL;
CREATE INDEX auth_sessions_org_created ON auth_sessions (org_id, created_at DESC);
CREATE INDEX auth_sessions_created ON auth_sessions (created_at DESC);
CREATE INDEX auth_sessions_expires ON auth_sessions (expires_at);
CREATE UNIQUE INDEX auth_sessions_legacy_token ON auth_sessions (legacy_token_hash)
    WHERE legacy_token_hash IS NOT NULL;

ALTER TABLE auth_sessions ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON auth_sessions FOR ALL
    USING ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE)
    WITH CHECK ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE);
GRANT SELECT, INSERT, UPDATE, DELETE ON auth_sessions TO madar_app;

CREATE FUNCTION auth_sessions_notify_revoked() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('authz_user', NEW.user_id::text);
    RETURN NULL;
END $$;

CREATE TRIGGER auth_sessions_notify_revoked AFTER UPDATE OF revoked_at ON auth_sessions
    FOR EACH ROW WHEN (OLD.revoked_at IS DISTINCT FROM NEW.revoked_at)
    EXECUTE FUNCTION auth_sessions_notify_revoked();
