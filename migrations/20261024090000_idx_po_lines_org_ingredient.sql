-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_po_lines_org_ingredient ON purchase_order_lines (org_id, org_ingredient_id);
-- Purchase orders remember the last price (GET /purchasing/orgs/{org}/last-prices):
-- the lookup reads an org's PO lines by ingredient. Only `(purchase_order_id)`
-- was indexed, so every lookup walked all of the org's orders.
-- One statement per file: CREATE INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
