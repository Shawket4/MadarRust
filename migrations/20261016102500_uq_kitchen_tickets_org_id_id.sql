-- no-transaction
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS uq_kitchen_tickets_org_id_id ON kitchen_tickets (org_id, id);
-- DB architecture Phase 1 (1.3 composite tenant key).
-- One statement per file: CREATE INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
