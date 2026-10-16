-- PARKED (DB audit 2026-10-16 Phase 5.3, CONTRACT). NOT APPLIED.
--
-- The three per-scope counter tables retire into `archive`; ref_counters is
-- the only sequence.
--
-- Unpark only when ALL hold:
--   1. Phase 5 has been live for one release: no binary that bumps
--      order_ref_counters / ticket_ref_counters / delivery_ref_counters on its own
--      is running (they are the serialization point the expand relies on).
--   2. scripts/db_phase5_check.sql run READ-ONLY on live prod prints no
--      `ref_counters_*` row.
--   3. The same PR changes src/refs.rs to bump ref_counters alone
--      (INSERT … ON CONFLICT DO UPDATE SET last_seq = ref_counters.last_seq + 1),
--      and src/bin/backfill_order_ref.rs to seed ref_counters (scope 'order').
--
-- Owner sign-off: ______________________  date: __________

CREATE SCHEMA IF NOT EXISTS archive;
ALTER TABLE order_ref_counters SET SCHEMA archive;
ALTER TABLE ticket_ref_counters SET SCHEMA archive;
ALTER TABLE delivery_ref_counters SET SCHEMA archive;
REVOKE ALL ON archive.order_ref_counters, archive.ticket_ref_counters,
              archive.delivery_ref_counters FROM madar_app;
