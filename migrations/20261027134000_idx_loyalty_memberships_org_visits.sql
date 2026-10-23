-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_loyalty_memberships_org_visits ON loyalty_memberships (org_id, visits_balance) WHERE deleted_at IS NULL;
-- Rewards dashboard: Balance and reward-ready segments of track 1 in a stamps programme, and the balance sort.
-- One statement per file (CONCURRENTLY cannot run in a transaction); a failed
-- build's INVALID index is dropped by the boot guard before the retry.
