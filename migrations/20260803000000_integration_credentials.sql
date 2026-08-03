-- Partner analytics credentials (HTTP Basic).
--
-- Third-party analytics consumers (an aggregator pulling a branch's sales for
-- its own reporting) authenticate with HTTP Basic rather than a JWT: Basic is
-- what their integration layer speaks, and a narrow, revocable credential is a
-- far better thing to hand a partner than a long-lived org token.
--
-- Each row grants READ-ONLY access to exactly ONE branch's order analytics.
-- There is no role and no escalation path: the only route behind this
-- credential is GET /integrations/analytics/orders, and the request runs on the
-- org's RLS-scoped pool like any other tenant traffic (see src/db.rs).

CREATE TABLE integration_credentials (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id       uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id    uuid NOT NULL REFERENCES branches(id)      ON DELETE CASCADE,
    -- Operator-facing label, e.g. "Rue — One Ninety".
    name         text NOT NULL,
    username     text NOT NULL,
    -- bcrypt of the secret. The plaintext is returned ONCE at creation and
    -- never stored, so a database read cannot recover a partner's password.
    secret_hash  text NOT NULL,
    created_by   uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    -- Stamped on each successful authentication (best-effort, fire-and-forget)
    -- so an operator can see whether a partner is actually pulling.
    last_used_at timestamptz,
    -- Revocation is a soft stamp, not a DELETE: the row stays as an audit trail
    -- of who was granted access to what, and when it was withdrawn.
    revoked_at   timestamptz,
    CONSTRAINT integration_credentials_username_len
        CHECK (char_length(username) BETWEEN 3 AND 64),
    CONSTRAINT integration_credentials_name_len
        CHECK (char_length(name) BETWEEN 1 AND 120)
);

-- Basic auth presents a username with no tenant hint, so the lookup necessarily
-- happens BEFORE any org is known: usernames must be unique cluster-wide, and
-- case-insensitively (partners will not reproduce our casing reliably).
CREATE UNIQUE INDEX integration_credentials_username_key
    ON integration_credentials (lower(username));

CREATE INDEX integration_credentials_org_idx    ON integration_credentials (org_id);
CREATE INDEX integration_credentials_branch_idx ON integration_credentials (branch_id);

-- RLS: org-rooted, mirroring the generator in 20260708000100_rls_policies.sql.
-- That migration's DO block has already run, so every table added afterwards
-- must classify itself here or the completeness assertion in src/rls_tests.rs
-- fails. The credential *lookup* deliberately runs on the owner pool (no org is
-- known yet); everything after authentication is org-scoped.
ALTER TABLE integration_credentials ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON integration_credentials FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));

GRANT ALL ON TABLE integration_credentials TO sufrix;
