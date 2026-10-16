-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_tills_closed_by ON tills (closed_by) WHERE closed_by IS NOT NULL;
-- DB architecture Phase 1 (1.1 actor FK).
-- One statement per file: CREATE INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
