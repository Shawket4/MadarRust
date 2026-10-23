-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_loyalty_txn_org_created ON loyalty_transactions (org_id, created_at);
-- Rewards dashboard: The Rewards figures: one org's ledger over a date range (stats, per track).
-- One statement per file (CONCURRENTLY cannot run in a transaction); a failed
-- build's INVALID index is dropped by the boot guard before the retry.
