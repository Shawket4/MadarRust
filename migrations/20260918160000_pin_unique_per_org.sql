-- Org-wide PIN uniqueness (POS_SIGNIN_OVERHAUL.md §3.1). A PIN must identify
-- ONE person wherever the device stands: branch-scoped uniqueness would make
-- the same PIN two different people at two branches, and would collide someone
-- allowed at both with themselves.
--
-- Uniqueness is a database property, not something the application hopes it got
-- right. It is PARTIAL over rows that have a fingerprint, so:
--   * PIN-less back-office accounts cost nothing;
--   * today's PINs, which have no fingerprint until their holder next signs in
--     (§6), cannot collide with anything yet — so this constraint can be added
--     now, before the backfill finishes, without locking anybody out.
-- Deleted rows are excluded, or a deleted person's PIN would stay reserved for
-- ever (the partial-index rule in CLAUDE.md).
DROP INDEX IF EXISTS users_pin_fingerprint_idx;
CREATE UNIQUE INDEX IF NOT EXISTS users_pin_fingerprint_key
    ON users (org_id, pin_fingerprint)
    WHERE pin_fingerprint IS NOT NULL AND deleted_at IS NULL;
