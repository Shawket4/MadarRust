-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_loyalty_memberships_org_enrolled ON loyalty_memberships (org_id, enrolled_at DESC, id) WHERE deleted_at IS NULL;
-- Rewards dashboard: The CRM table sorted by joining date (its default), and "joined within N days".
-- One statement per file (CONCURRENTLY cannot run in a transaction); a failed
-- build's INVALID index is dropped by the boot guard before the retry.
