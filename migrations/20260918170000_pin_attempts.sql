-- Failed PIN attempts: a growing delay, never a lock
-- (POS_SIGNIN_OVERHAUL.md §3.4, owner decision 2026-09-16).
--
-- Account lockout is meaningless once a PIN is the whole credential: a wrong
-- PIN matches NOBODY, so there is no account to attach the failure to. And the
-- tablet is shared — a hard lock stops the shop, which is a likelier outcome
-- than a real attack. So attempts are counted against the PLACE, not a person,
-- and the answer is a wait that grows.
--
-- Two buckets: the device (a tablet being ground on) and the branch (someone
-- moving between tablets in one shop). Old tablets send no device id and land
-- in the branch bucket only; their existing per-account throttling is
-- unchanged.
--
-- In the database rather than in memory because the delay must survive a
-- restart — otherwise restarting the API is the way around it.
CREATE TABLE IF NOT EXISTS pin_attempts (
    kind          text        NOT NULL CHECK (kind IN ('device', 'branch')),
    key           text        NOT NULL,
    fails         integer     NOT NULL DEFAULT 0,
    last_fail_at  timestamptz NOT NULL DEFAULT now(),
    blocked_until timestamptz,
    PRIMARY KEY (kind, key)
);

-- Housekeeping: a bucket nobody has touched for a day is meaningless.
CREATE INDEX IF NOT EXISTS pin_attempts_last_fail_idx ON pin_attempts (last_fail_at);

COMMENT ON TABLE pin_attempts IS
    'Failed PIN sign-in attempts per device and per branch. A growing delay, never a lock (POS_SIGNIN_OVERHAUL.md §3.4).';

GRANT SELECT, INSERT, UPDATE, DELETE ON pin_attempts TO madar_app;

-- The one failed sign-in WITH an identity: a correct PIN typed at a branch its
-- holder may not sign in at. Counted per person in the owner's existing review
-- queue rather than a new one (§3.4), under its own reason.
ALTER TABLE authz_replay_flags DROP CONSTRAINT IF EXISTS authz_replay_flags_reason_check;
ALTER TABLE authz_replay_flags ADD CONSTRAINT authz_replay_flags_reason_check
    CHECK (reason IN ('stale_snapshot', 'unauthorized_offline', 'pin_wrong_branch'));
