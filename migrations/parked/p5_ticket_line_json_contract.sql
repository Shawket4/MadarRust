-- PARKED (DB audit 2026-10-16 Phase 5.1, CONTRACT). NOT APPLIED.
--
-- Retires open_ticket_items.line (jsonb). Since 20261020140000 what a line IS
-- lives in columns + open_ticket_item_addons / _optionals (dual-written by
-- trigger). `line.input`, the frozen request settle replays, moves to its own
-- column; the rest of `line` is archived, never dropped with data in it.
--
-- Unpark only when ALL hold:
--   1. Every active device (client_seen.app_version) is on a POS/KDS build that
--      reads the relational line fields of OpenTicketItemView rather than
--      `line` (the server keeps serving `line` until then: it is the wire
--      contract of every older build).
--   2. scripts/db_phase5_check.sql run READ-ONLY on live prod prints no
--      `ticket_lines_*` row, over at least one full release.
--   3. The same PR: writers write the columns + children + `input` directly,
--      OpenTicketItemView builds `line` (if still served) from them, settle
--      reads `input`, and the dual-write triggers go.
--
-- Owner sign-off: ______________________  date: __________

SET LOCAL lock_timeout = '15s';
ALTER TABLE open_ticket_items ADD COLUMN IF NOT EXISTS input jsonb;
UPDATE open_ticket_items SET input = line -> 'input' WHERE input IS NULL;
CREATE SCHEMA IF NOT EXISTS archive;
CREATE TABLE IF NOT EXISTS archive.open_ticket_items_line AS
    SELECT id, org_id, line, now() AS archived_at FROM open_ticket_items;
DROP TRIGGER IF EXISTS open_ticket_item_columns ON open_ticket_items;
DROP TRIGGER IF EXISTS open_ticket_item_children ON open_ticket_items;
ALTER TABLE open_ticket_items ALTER COLUMN input SET NOT NULL;
ALTER TABLE open_ticket_items DROP COLUMN line;
