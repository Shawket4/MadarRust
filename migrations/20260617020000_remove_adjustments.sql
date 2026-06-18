-- Remove manual stock adjustments entirely.
--
-- Stock corrections now flow through stocktakes (count-to-truth) and waste;
-- sales, transfers, purchase receipts and supplier returns remain the other
-- movement sources. The append-only inventory_movements ledger is the sole audit
-- trail (transfers already write transfer_out/transfer_in movements), so the
-- legacy branch_inventory_adjustments table and its enums are dropped.
--
-- No backfill (agreed: customers re-establish branch stock from scratch). The
-- ingredient catalog (org_ingredients) and recipes are left completely untouched.
DROP TABLE IF EXISTS branch_inventory_adjustments;
DROP TYPE  IF EXISTS inventory_adjustment_reason;
DROP TYPE  IF EXISTS inventory_adjustment_type;
