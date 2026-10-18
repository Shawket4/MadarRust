-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_goods_receipt_lines_po_line ON goods_receipt_lines (purchase_order_line_id) WHERE purchase_order_line_id IS NOT NULL;
-- The last-price lookup takes a received line's ACTUAL price from its latest
-- goods receipt line; nothing indexed goods_receipt_lines by PO line.
-- One statement per file: CREATE INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
