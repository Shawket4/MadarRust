-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_inventory_movements_branch_stock ON inventory_movements (branch_stock_id) WHERE branch_stock_id IS NOT NULL;
-- DB architecture Phase 1 (1.1 FK inventory_movements.branch_stock_id (ON DELETE SET NULL) was unindexed).
-- One statement per file: CREATE INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
