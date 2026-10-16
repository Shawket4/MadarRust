-- PARKED (DB audit 2026-10-16 Phase 5.2, CONTRACT). NOT APPLIED.
--
-- Drops kitchen_tickets.source_type / source_id. They are GENERATED columns
-- (from order_id / open_ticket_id, under the exactly-one CHECK
-- kitchen_tickets_has_one_source), so they hold no data of their own.
--
-- Unpark only when ALL hold:
--   1. The CHECK kitchen_tickets_has_one_source has held (it is validated; no
--      violation logged) since it was added.
--   2. Every active KDS/POS device is on a build that reads order_id /
--      open_ticket_id; until then the server computes the pair for the wire
--      (src/kitchen/mod.rs reads them today).
--   3. uq_kitchen_tickets_source_round (source_type, source_id, round_number) is
--      replaced, in the same PR, by two partial uniques on (order_id,
--      round_number) and (open_ticket_id, round_number).
--
-- Owner sign-off: ______________________  date: __________

ALTER TABLE kitchen_tickets DROP COLUMN IF EXISTS source_type;
ALTER TABLE kitchen_tickets DROP COLUMN IF EXISTS source_id;
