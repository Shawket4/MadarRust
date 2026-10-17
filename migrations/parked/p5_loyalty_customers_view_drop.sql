-- PARKED (DB audit 2026-10-16 Phase 5.5, CONTRACT). NOT APPLIED.
--
-- Drops the compat view `loyalty_customers` (over loyalty_memberships, since
-- 20261020120000). A view holds no rows: nothing is lost.
--
-- Unpark (rename to a timestamp after the newest applied migration, move into
-- migrations/) only when ALL hold:
--   1. Phase 5 has been live for one release (no older binary in a rolling
--      deploy still names loyalty_customers).
--   2. No SQL in src/ or in a function body names loyalty_customers:
--        SELECT proname FROM pg_proc WHERE prosrc ~ '\mloyalty_customers\M';  -- empty
--      and `cargo sqlx prepare --check` passes against a database with this
--      file applied.
--
-- Owner sign-off: ______________________  date: __________

DROP VIEW IF EXISTS public.loyalty_customers;
