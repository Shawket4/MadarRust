-- `inventory_movements.source_type` may be 'import' (2026-10-17).
--
-- Prod holds 83 movements with source_type 'import' (the 2026-10-04 stock
-- import), a value the Phase 1.5 discriminator CHECK
-- (20261016110000_tenant_keys_and_checks) does not list, so on prod that CHECK
-- was left NOT VALID. This adds 'import' to the allowed list and VALIDATEs it,
-- so the constraint holds for every row, old ones included.
--
-- Swapped in one transaction: the CHECK is re-added NOT VALID (no scan under
-- the ACCESS EXCLUSIVE lock), then VALIDATEd (one scan under SHARE UPDATE
-- EXCLUSIVE: reads and writes carry on). Should a copy still hold a value
-- outside the list, the validate is skipped with a WARNING (new rows are still
-- checked) rather than failing the boot; `scripts/db_phase1_check.sql` lists
-- an unvalidated constraint. No row is changed.
SET LOCAL lock_timeout = '15s';

ALTER TABLE inventory_movements DROP CONSTRAINT IF EXISTS inventory_movements_source_type_check;
ALTER TABLE inventory_movements ADD CONSTRAINT inventory_movements_source_type_check
    CHECK (source_type IS NULL OR source_type IN (
        'order', 'refund', 'waste', 'stocktake', 'transfer', 'purchase',
        'goods_receipt', 'delivery_order', 'seed', 'import')) NOT VALID;

DO $v$
BEGIN
    ALTER TABLE inventory_movements VALIDATE CONSTRAINT inventory_movements_source_type_check;
EXCEPTION WHEN check_violation THEN
    RAISE WARNING 'inventory_movements_source_type_check: existing rows carry a source_type outside the list; left NOT VALID (new rows are still checked)';
END
$v$;
