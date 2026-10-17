-- DB architecture Phase 5.5 (DB_ARCHITECTURE_AUDIT_2026-10-16 §8 Phase 5.5):
-- `loyalty_customers` is renamed `loyalty_memberships`. Since the customers
-- unification it holds a person's MEMBERSHIP of a shop's programme (the person
-- is `customers`); the old name read as a second customer table.
--
-- SOURCE TABLES (additions; machine-read by the migration tests together with
-- the earlier changefeed headers). Format:  table -> type[, type…]  (scope)
--   loyalty_memberships        -> customer                    (org fan-out)
--
-- SOURCE TABLES REMOVED (machine-read by the migration tests; renamed above,
-- the header of 20260918200000_customers.sql still lists the old name):
--   loyalty_customers          -> customer
--
-- A rename is a catalog change: the rows, indexes, constraints, triggers, RLS
-- policy, grants and every FK (by OID) move with the table, and views over it
-- (loyalty_members_v) keep working. Nothing is copied or lost.
--
-- COMPAT: a view under the old name, `SELECT * FROM loyalty_memberships`, is
-- automatically updatable, so the PREVIOUS binary of a rolling deploy (and any
-- function body that still says loyalty_customers) keeps reading and writing
-- through it: base-table defaults, triggers and RLS all apply (security
-- invoker). Dropping it is parked: migrations/parked/p5_loyalty_customers_view_drop.sql.
--
-- The changefeed follows the table: the emitter function takes the new name
-- (the `sync_emit` trigger points at it by OID) and sync_source_tables() lists
-- the new name, rewritten from its current body so nothing else in it moves.
SET LOCAL lock_timeout = '15s';

DO $p55$
BEGIN
    IF to_regclass('public.loyalty_memberships') IS NULL THEN
        ALTER TABLE loyalty_customers RENAME TO loyalty_memberships;
    END IF;
    IF to_regclass('public.loyalty_customers') IS NULL THEN
        CREATE VIEW loyalty_customers WITH (security_invoker = true) AS
            SELECT * FROM loyalty_memberships;
        GRANT SELECT, INSERT, UPDATE, DELETE ON loyalty_customers TO madar_app;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_proc WHERE proname = 'sync_emit_loyalty_customers') THEN
        ALTER FUNCTION sync_emit_loyalty_customers() RENAME TO sync_emit_loyalty_memberships;
    END IF;
    IF pg_get_functiondef('sync_source_tables'::regproc) LIKE '%(''loyalty_customers'',%' THEN
        EXECUTE replace(pg_get_functiondef('sync_source_tables'::regproc),
                        '(''loyalty_customers'',', '(''loyalty_memberships'',');
    END IF;
END
$p55$;
