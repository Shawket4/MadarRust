-- V30: snapshot whether a payment was cash AT SALE TIME, so renaming a payment
-- method (or flipping its is_cash) can no longer retroactively change historical
-- shift cash reconciliation. close_shift / shift-report now read these columns
-- (with a legacy method='cash' fallback) instead of joining org_payment_methods
-- by name.
ALTER TABLE order_payments ADD COLUMN IF NOT EXISTS is_cash boolean;
ALTER TABLE orders         ADD COLUMN IF NOT EXISTS tip_is_cash boolean;

-- Backfill from the CURRENT method config where the name still matches
-- (historical config is unrecoverable; this is the best available value), then
-- fall back to the legacy 'cash'-by-name heuristic for any remaining rows.
UPDATE order_payments op
SET is_cash = opm.is_cash
FROM orders o
JOIN branches b ON b.id = o.branch_id
JOIN org_payment_methods opm ON opm.org_id = b.org_id
WHERE op.order_id = o.id
  AND opm.name = op.method
  AND op.is_cash IS NULL;

UPDATE order_payments SET is_cash = (method = 'cash') WHERE is_cash IS NULL;

UPDATE orders o
SET tip_is_cash = opm.is_cash
FROM branches b, org_payment_methods opm
WHERE b.id = o.branch_id
  AND opm.org_id = b.org_id
  AND opm.name = COALESCE(o.tip_payment_method, o.payment_method)
  AND o.tip_amount IS NOT NULL AND o.tip_amount > 0
  AND o.tip_is_cash IS NULL;

UPDATE orders
SET tip_is_cash = (COALESCE(tip_payment_method, payment_method) = 'cash')
WHERE tip_is_cash IS NULL AND tip_amount IS NOT NULL AND tip_amount > 0;
