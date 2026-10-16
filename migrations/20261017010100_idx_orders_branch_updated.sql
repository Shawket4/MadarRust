-- no-transaction
-- DB audit Phase 6: the incremental sweep's order arm (`sync_live_rows_since`)
-- reads a branch's orders changed since the retention start. Without this it
-- reads the branch's whole order history. CONCURRENTLY, so writes to orders
-- are never blocked while it builds (hence no transaction).
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_orders_branch_updated ON orders (branch_id, updated_at);
