-- no-transaction
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS uq_delivery_orders_org_ref ON delivery_orders (org_id, delivery_ref) WHERE delivery_ref IS NOT NULL;
-- DB architecture Phase 2.3 (per-org ref uniqueness, expand: refs are minted from a branch or device code plus a date and a per-branch or per-device sequence, and codes repeat across orgs (two prod orgs share a branch code): a GLOBAL unique made the second org's ref collide with the first's, failing a ticket fire or a delivery checkout, and for a device-minted order_ref answering 409 to a sale the till then dead-letters. Per org is the real invariant; no code looks a ref up outside its org).
-- One statement per file: CREATE INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
