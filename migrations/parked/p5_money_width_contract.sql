-- PARKED (DB audit 2026-10-16 Phase 5.6, CONTRACT). NOT APPLIED.
--
-- Swaps the int4 money / lifetime columns for their bigint `_wide` twins
-- (expanded and kept equal since 20261020130000). The int4 values are copied to
-- archive first: no data is lost.
--
-- Unpark only when ALL hold:
--   1. scripts/db_phase5_check.sql run READ-ONLY on live prod prints no
--      `money_wide_*` row, after at least one full release with the expand live.
--   2. The same PR moves the Rust row types of these columns to i64 (tills::Till,
--      the till rows/report loaders, till_reconciliations, loyalty member rows)
--      and `cargo sqlx prepare --check` passes against a database with this file
--      applied.
--   3. The deploy is NOT rolling for this release: stop the old container before
--      the new one migrates (an older binary would decode bigint into i32 and fail).
--
-- Owner sign-off: ______________________  date: __________

SET LOCAL lock_timeout = '15s';
CREATE SCHEMA IF NOT EXISTS archive;
CREATE TABLE IF NOT EXISTS archive.tills_money_int4 AS
    SELECT id, opening_cash, opening_cash_original, closing_cash_declared, closing_cash_system,
           cash_discrepancy, held_orders_left_open_total, now() AS archived_at FROM tills;
CREATE TABLE IF NOT EXISTS archive.till_reconciliations_money_int4 AS
    SELECT id, system_total, current_system_total, declared_amount, now() AS archived_at
      FROM till_reconciliations;
CREATE TABLE IF NOT EXISTS archive.loyalty_memberships_lifetime_int4 AS
    SELECT id, lifetime_points, lifetime_visits, now() AS archived_at FROM loyalty_memberships;

DROP TRIGGER IF EXISTS money_wide ON tills;
DROP TRIGGER IF EXISTS money_wide ON till_reconciliations;
DROP TRIGGER IF EXISTS money_wide ON loyalty_memberships;
DROP FUNCTION IF EXISTS money_wide_tills();
DROP FUNCTION IF EXISTS money_wide_till_reconciliations();
DROP FUNCTION IF EXISTS money_wide_loyalty_memberships();

-- loyalty_members_v selects lifetime_points / lifetime_visits: it is dropped
-- and recreated around the swap (its definition re-read from the catalog, so
-- it is whatever it is at unpark time). Each column: the bigint twin takes the
-- name, with the int4's NOT NULL and default.
CREATE TEMP TABLE p56_views AS
    SELECT 'loyalty_members_v'::text AS v, pg_get_viewdef('public.loyalty_members_v'::regclass) AS def,
           (SELECT reloptions FROM pg_class WHERE oid = 'public.loyalty_members_v'::regclass) AS opts;
DROP VIEW IF EXISTS compat.loyalty_members_v;
DROP VIEW public.loyalty_members_v;
DROP VIEW IF EXISTS public.loyalty_customers;  -- SELECT * view from 5.5, recreated below

ALTER TABLE tills DROP COLUMN opening_cash;
ALTER TABLE tills RENAME COLUMN opening_cash_wide TO opening_cash;
ALTER TABLE tills ALTER COLUMN opening_cash SET DEFAULT 0;
ALTER TABLE tills ALTER COLUMN opening_cash SET NOT NULL;
ALTER TABLE tills DROP COLUMN opening_cash_original;
ALTER TABLE tills RENAME COLUMN opening_cash_original_wide TO opening_cash_original;
-- cash_discrepancy is GENERATED from the pair below: it goes first and comes
-- back as a bigint generated column over the widened pair (a rewrite of tills,
-- which is why this release is not a rolling deploy).
ALTER TABLE tills DROP COLUMN cash_discrepancy;
ALTER TABLE tills DROP COLUMN closing_cash_declared;
ALTER TABLE tills RENAME COLUMN closing_cash_declared_wide TO closing_cash_declared;
ALTER TABLE tills DROP COLUMN closing_cash_system;
ALTER TABLE tills RENAME COLUMN closing_cash_system_wide TO closing_cash_system;
ALTER TABLE tills DROP COLUMN held_orders_left_open_total;
ALTER TABLE tills RENAME COLUMN held_orders_left_open_total_wide TO held_orders_left_open_total;
ALTER TABLE tills ADD COLUMN cash_discrepancy bigint
    GENERATED ALWAYS AS (closing_cash_declared - closing_cash_system) STORED;
ALTER TABLE till_reconciliations DROP COLUMN system_total;
ALTER TABLE till_reconciliations RENAME COLUMN system_total_wide TO system_total;
ALTER TABLE till_reconciliations ALTER COLUMN system_total SET NOT NULL;
ALTER TABLE till_reconciliations DROP COLUMN current_system_total;
ALTER TABLE till_reconciliations RENAME COLUMN current_system_total_wide TO current_system_total;
ALTER TABLE till_reconciliations ALTER COLUMN current_system_total SET NOT NULL;
ALTER TABLE till_reconciliations DROP COLUMN declared_amount;
ALTER TABLE till_reconciliations RENAME COLUMN declared_amount_wide TO declared_amount;
ALTER TABLE loyalty_memberships DROP COLUMN lifetime_points;
ALTER TABLE loyalty_memberships RENAME COLUMN lifetime_points_wide TO lifetime_points;
ALTER TABLE loyalty_memberships ALTER COLUMN lifetime_points SET DEFAULT 0;
ALTER TABLE loyalty_memberships ALTER COLUMN lifetime_points SET NOT NULL;
ALTER TABLE loyalty_memberships DROP COLUMN lifetime_visits;
ALTER TABLE loyalty_memberships RENAME COLUMN lifetime_visits_wide TO lifetime_visits;
ALTER TABLE loyalty_memberships ALTER COLUMN lifetime_visits SET DEFAULT 0;
ALTER TABLE loyalty_memberships ALTER COLUMN lifetime_visits SET NOT NULL;

DO $v$
DECLARE r record;
BEGIN
    FOR r IN SELECT * FROM p56_views LOOP
        EXECUTE format('CREATE VIEW public.%I WITH (security_invoker = true) AS %s', r.v, r.def);
        EXECUTE format('GRANT SELECT ON public.%I TO madar_app', r.v);
        EXECUTE format('CREATE VIEW compat.%I WITH (security_invoker = true) AS %s', r.v, r.def);
        EXECUTE format('GRANT SELECT ON compat.%I TO madar_app', r.v);
    END LOOP;
END $v$;
-- Unless p5_loyalty_customers_view_drop.sql went first:
CREATE VIEW public.loyalty_customers WITH (security_invoker = true) AS SELECT * FROM loyalty_memberships;
GRANT SELECT, INSERT, UPDATE, DELETE ON public.loyalty_customers TO madar_app;
