-- Phase 4: accept-and-flag for replayed offline acts (PERMISSIONS_ARCHITECTURE
-- §4.4.5, and the binding owner decision "offline actions that fail the server
-- re-check are accepted and flagged, never rejected").
--
-- Until now a queued money op whose author no longer holds the capability was
-- rejected with a 403 and dead-lettered on the tablet. That is wrong for the
-- only case that matters: the sale ALREADY HAPPENED. The customer paid, took
-- their coffee and left while the shop was offline. Throwing the op away does
-- not un-take the money; it only loses the record of it and leaves the drawer
-- short at close. So a money op is now accepted and a row is written here for
-- the owner to review.
--
-- Two reasons, because they are different failures:
--   `stale_snapshot`       the author DID hold the capability when the op
--                          happened and it was revoked afterwards. The device
--                          was right; it just had not heard yet. Routine.
--   `unauthorized_offline` no revocation explains it — an old optimistic
--                          client, or a tampered one. Worth a look.
CREATE TABLE IF NOT EXISTS authz_replay_flags (
    id           bigserial PRIMARY KEY,
    org_id       uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id    uuid NULL REFERENCES branches(id) ON DELETE SET NULL,
    -- The ReplayOp variant, e.g. `CreateOrder`.
    op           text NOT NULL,
    author_id    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    -- The `resource:action` cell that failed, e.g. `refunds:create`.
    capability   text NOT NULL,
    reason       text NOT NULL
                 CHECK (reason IN ('stale_snapshot', 'unauthorized_offline')),
    -- When the act happened ON THE DEVICE (the envelope's `occurred_at` when a
    -- client sends one), not when it reached us. The gap is the offline window.
    occurred_at  timestamptz NOT NULL DEFAULT now(),
    created_at   timestamptz NOT NULL DEFAULT now(),
    -- Set once phase 5 mints approvals; an op that carried a valid one is not
    -- flagged at all, so this is for an approval attached after the fact.
    approval_id  uuid NULL,
    -- The owner's review, through `approvals.review`.
    reviewed_at  timestamptz NULL,
    reviewed_by  uuid NULL REFERENCES users(id) ON DELETE SET NULL,
    details      jsonb NOT NULL DEFAULT '{}'::jsonb
);

-- The owner's review queue is "this org, still unreviewed, newest first".
CREATE INDEX IF NOT EXISTS authz_replay_flags_org_open
    ON authz_replay_flags (org_id, created_at DESC)
    WHERE reviewed_at IS NULL;
CREATE INDEX IF NOT EXISTS authz_replay_flags_author
    ON authz_replay_flags (author_id, occurred_at DESC);

-- Tenant isolation, like every public table.
ALTER TABLE authz_replay_flags ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS authz_replay_flags_tenant ON authz_replay_flags;
CREATE POLICY authz_replay_flags_tenant ON authz_replay_flags
    USING (org_id = NULLIF(current_setting('app.org_id', true), '')::uuid);

GRANT SELECT, INSERT, UPDATE ON authz_replay_flags TO madar_app;
GRANT USAGE, SELECT ON SEQUENCE authz_replay_flags_id_seq TO madar_app;
