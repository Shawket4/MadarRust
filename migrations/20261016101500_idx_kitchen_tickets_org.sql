-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_kitchen_tickets_org ON kitchen_tickets (org_id);
-- DB architecture Phase 1 (1.1 org cascade).
-- One statement per file: CREATE INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
