-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_orders_voided_by ON orders (voided_by) WHERE voided_by IS NOT NULL;
-- DB architecture Phase 1 (1.1 actor FK, for user deletes and the demo sweeper).
-- One statement per file: CREATE INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
