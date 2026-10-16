-- no-transaction
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS uq_order_refunds_org_client_ref ON order_refunds (org_id, client_ref) WHERE client_ref IS NOT NULL;
-- DB architecture Phase 1 (1.4 per-org idempotency (global uq_order_refunds_client_ref stays)).
-- One statement per file: CREATE INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
