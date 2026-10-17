-- no-transaction
DROP INDEX CONCURRENTLY IF EXISTS uq_delivery_orders_ref;
-- DB architecture Phase 2.3 (per-org ref uniqueness, contract: the global unique goes, uq_delivery_orders_org_ref (the file before) enforces it per org).
-- One statement per file: DROP INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
