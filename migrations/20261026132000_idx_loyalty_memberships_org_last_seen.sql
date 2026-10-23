-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_loyalty_memberships_org_last_seen ON loyalty_memberships (org_id, last_seen_at DESC) WHERE deleted_at IS NULL;
-- Rewards dashboard: "Visited within / not visited for N days" segments and the last-visit sort.
-- One statement per file (CONCURRENTLY cannot run in a transaction); a failed
-- build's INVALID index is dropped by the boot guard before the retry.
