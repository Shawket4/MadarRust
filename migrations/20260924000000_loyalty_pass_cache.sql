-- Built passes, kept so "Add to Apple Wallet" serves bytes instead of making
-- them.
--
-- A .pkpass was assembled on the button press: three settings reads, two
-- catalogue reads, two branding reads, the branch list, the locations, six
-- brand images, three strip images, a manifest of SHA-1 digests and a PKCS#7
-- signature — measured at ~20 s on the production VPS, where Postgres shares
-- the single vCPU with the server waiting on it. None of it is work that has
-- to happen while a customer holds their phone.
--
-- ── Why a table and not the asset store ─────────────────────────────────────
-- `src/assets` is built for ORG-OWNED, PUBLIC-ish media: it ingests an upload,
-- derives renditions, and hands out signed org-scoped URLs that a browser
-- fetches directly. A pass is none of those things. It is per MEMBER, it is
-- private to that member, it is never fetched by URL (the bytes are the
-- response body of an authenticated handler), and it is disposable — wrong
-- bytes are thrown away and rebuilt, they are never migrated or backfilled.
-- Putting it there would mean inventing a member-scoped lifetime inside a
-- store whose whole model is org-scoped assets that outlive their request.
--
-- A table gives what this actually needs and nothing else: the row is deleted
-- in the same transaction as the thing that invalidated it, a sweep is one
-- DELETE, and there is no second place for a deploy to lose track of.
--
-- ── Why it cannot serve something silently wrong ────────────────────────────
-- The pass carries the member's BALANCE, so it changes with every purchase.
-- Two independent guards, because stale-but-correct is fine here and
-- silently-wrong is not:
--
--   * `fingerprint` pins everything member-specific that is drawn on the card
--     — the balances, the name, the notice line. The reader already holds the
--     member row, so comparing it costs no query at all; a mismatch is a miss
--     and the pass is built exactly as it was before this table existed.
--   * `built_at` expires the row after an hour, which is the SAME hour the
--     per-org strip and brand image caches already live by. A stored pass can
--     therefore never be staler than the images inside it already were, which
--     is what makes one TTL the whole story for everything org-level:
--     branding, palette, card photograph, branch list.
--
-- Loyalty settings are invalidated explicitly on save rather than waiting out
-- the hour, because that is the edit an owner makes and then immediately looks
-- at their own card to check.
--
-- Nothing here is a source of truth. Every row can be deleted at any moment
-- and the only consequence is that the next tap is as slow as it used to be.

CREATE TABLE loyalty_pass_cache (
    customer_id uuid PRIMARY KEY REFERENCES loyalty_customers(id) ON DELETE CASCADE,
    org_id      uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- 'apple' today. Google needs no bytes — an update there is a PATCH to
    -- Google's API, not a file we hand out — but the column keeps the table
    -- from being renamed the day that stops being true.
    kind        text NOT NULL DEFAULT 'apple',
    bytes       bytea NOT NULL,
    -- Hex SHA-256 of the member-specific values the pass draws.
    fingerprint text NOT NULL,
    built_at    timestamptz NOT NULL DEFAULT now()
);

-- The sweep's index: "everything older than an hour", org-agnostic.
CREATE INDEX idx_loyalty_pass_cache_built_at ON loyalty_pass_cache (built_at);
-- Invalidating a whole shop at once (settings saved, branding changed).
CREATE INDEX idx_loyalty_pass_cache_org ON loyalty_pass_cache (org_id);

ALTER TABLE loyalty_pass_cache ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON loyalty_pass_cache FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));

GRANT ALL ON TABLE loyalty_pass_cache TO sufrix;
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE loyalty_pass_cache TO madar_app;

COMMENT ON TABLE loyalty_pass_cache IS
  'Pre-built .pkpass bytes per loyalty member. A pure cache: safe to empty at '
  'any time, and every reader falls back to building the pass on demand.';
