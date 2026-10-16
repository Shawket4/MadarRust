-- no-transaction
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS uq_open_tickets_org_idem ON open_tickets (org_id, idempotency_key) WHERE idempotency_key IS NOT NULL;
-- DB architecture Phase 1 (1.4 per-org idempotency (global uq_open_tickets_idem stays)).
-- One statement per file: CREATE INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
