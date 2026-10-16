-- no-transaction
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS uq_orders_org_idem ON orders (org_id, idempotency_key) WHERE idempotency_key IS NOT NULL;
-- DB architecture Phase 1 (1.4 per-org idempotency (expand; the global orders_idempotency_key_idx stays until Phase 2)).
-- One statement per file: CREATE INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
