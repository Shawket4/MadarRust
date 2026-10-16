-- DB architecture Phase 5.7 (DB_ARCHITECTURE_AUDIT_2026-10-16 §8 Phase 5.7, owner
-- decision 12.5): ledger rows are never removed as a side effect.
--
-- 1. Every FK from organizations / branches INTO a ledger table that said
--    ON DELETE CASCADE now says RESTRICT: deleting a shop or a branch can no
--    longer silently delete its sales, refunds, tickets, loyalty ledger or
--    stock movements. Constraint swap without blocking writes: the new FK is
--    added NOT VALID (a catalog change), VALIDATEd (SHARE UPDATE EXCLUSIVE:
--    reads and writes continue), then the old one is dropped. Same columns, so
--    nothing about what the database accepts changes, only what a DELETE of
--    the parent does. (Branches and orgs are soft-deleted by the app; the demo
--    sweeper, which hard-deletes expired DEMO orgs, deletes those ledgers
--    explicitly, in src/demo/sweeper.rs.)
--    Not changed: sync_changes (a changefeed, not a ledger; Phase 6's), and
--    orders/tills, whose FKs to branches already restrict.
-- 2. Tills get `deleted_at`. DELETE /tills/{id} no longer hard-deletes the
--    till and its voided orders: it stamps deleted_at (same 204), and every
--    till read that lists or fetches tills leaves stamped ones out.
SET LOCAL lock_timeout = '15s';

CREATE FUNCTION pg_temp.p5_restrict(con text) RETURNS void
LANGUAGE plpgsql AS $fn$
DECLARE
    r       record;
    newname text := con || '_restrict';
BEGIN
    SELECT k.conrelid::regclass AS rel, pg_get_constraintdef(k.oid) AS def
      INTO r FROM pg_constraint k WHERE k.conname = con AND k.contype = 'f';
    IF r IS NULL THEN
        RETURN;  -- already swapped, or a database that never had it
    END IF;
    IF r.def !~ 'ON DELETE CASCADE' THEN
        RETURN;
    END IF;
    EXECUTE format('ALTER TABLE %s ADD CONSTRAINT %I %s NOT VALID', r.rel, newname,
                   replace(r.def, 'ON DELETE CASCADE', 'ON DELETE RESTRICT'));
    EXECUTE format('ALTER TABLE %s VALIDATE CONSTRAINT %I', r.rel, newname);
    EXECUTE format('ALTER TABLE %s DROP CONSTRAINT %I', r.rel, con);
    EXECUTE format('ALTER TABLE %s RENAME CONSTRAINT %I TO %I', r.rel, newname, con);
END
$fn$;

SELECT pg_temp.p5_restrict(c) FROM unnest(ARRAY[
    'delivery_orders_branch_id_fkey', 'delivery_orders_org_id_fkey',
    'kitchen_tickets_branch_id_fkey', 'kitchen_tickets_org_id_fkey', 'kitchen_tickets_org_branch_fk',
    'open_tickets_branch_id_fkey', 'open_tickets_org_id_fkey', 'open_tickets_org_branch_fk',
    'loyalty_transactions_org_id_fkey',
    'order_refunds_org_id_fkey', 'order_refund_lines_org_id_fkey',
    'inventory_movements_branch_id_fkey', 'inventory_movements_org_branch_fk'
]) AS c;

DROP FUNCTION pg_temp.p5_restrict(text);

-- 2. Till soft delete.
ALTER TABLE tills ADD COLUMN IF NOT EXISTS deleted_at timestamptz;
