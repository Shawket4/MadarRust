-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_order_item_addons_org_item ON order_item_addons (org_id, order_item_id);
-- DB architecture Phase 2.3 (the (org_id, time) index on a child ledger that Phase 1 deferred until org_id existed; it serves the flat RLS policy and org-wide date windows).
-- One statement per file: CREATE INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
