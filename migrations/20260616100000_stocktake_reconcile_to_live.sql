-- Stocktake finalize must reconcile to LIVE stock, not the open-time snapshot.
--
-- `expected_qty` is captured when the count opens. Sales/purchases keep moving
-- `branch_inventory.current_stock` while the count is in progress, so finalizing
-- by overwriting stock with `counted_qty` erased those movements and reported
-- legitimate sales as shrinkage. Finalize now re-reads the live book stock under
-- a row lock and reconciles by delta (counted - live).
--
-- `system_qty` records that live book stock captured AT FINALIZE — the true
-- reconciliation baseline. True unexplained variance = counted_qty - system_qty
-- (legitimate activity during the count nets out, since it already moved live
-- stock). `expected_qty` is kept as the open-time snapshot for reference
-- ("activity during the count" = system_qty - expected_qty).
ALTER TABLE stocktake_items
    ADD COLUMN system_qty numeric(12,3);
