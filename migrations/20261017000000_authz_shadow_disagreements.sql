-- DB audit 2026-10-16, Phase 4 (authz unification), step 1: persist the
-- shadow comparison's disagreements.
--
-- `authz::shadow` computes the legacy decision next to the architecture E one
-- and, until now, only logged a divergence and bumped an in-process counter
-- that died with the process. This table is the durable counter the cut-over
-- condition is read from: one row per day x person x cell x (legacy, new)
-- outcome pair, with a hit count. Written in batches by a background task on
-- the owner pool (never on the request's connection, so a rolled-back handler
-- cannot lose a hit and a failed write cannot fail a request).
--
-- Operator telemetry, not tenant data: no RLS, no grant to madar_app. It holds
-- ids only (no names), so a dump of it is not personal data beyond user ids.
--
--   "is the old model still disagreeing?"
--     SELECT day, sum(hits) FROM authz_shadow_disagreements
--      WHERE day > current_date - 30 GROUP BY day ORDER BY day;

CREATE TABLE IF NOT EXISTS authz_shadow_disagreements (
    day        date        NOT NULL,
    org_id     uuid,
    user_id    uuid        NOT NULL,
    resource   text        NOT NULL,
    action     text        NOT NULL,
    legacy     boolean     NOT NULL,
    new        boolean     NOT NULL,
    hits       bigint      NOT NULL CHECK (hits > 0),
    first_seen timestamptz NOT NULL,
    last_seen  timestamptz NOT NULL,
    PRIMARY KEY (day, user_id, resource, action, legacy, new)
);

CREATE INDEX IF NOT EXISTS authz_shadow_disagreements_org_day
    ON authz_shadow_disagreements (org_id, day);
