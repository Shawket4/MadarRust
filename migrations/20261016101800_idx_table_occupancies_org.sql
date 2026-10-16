-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_table_occupancies_org ON table_occupancies (org_id);
-- DB architecture Phase 1 (1.1 org cascade).
-- One statement per file: CREATE INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
