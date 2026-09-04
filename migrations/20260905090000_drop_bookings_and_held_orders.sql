-- Retire the booking flow and server-side held orders.
--
-- ── Bookings ────────────────────────────────────────────────────────────────
-- Reservations, the waitlist and the public self-booking flow shipped but were
-- never used: zero rows in `bookings` on production, ever. The replacement is
-- being built on the floor/ticket layer rather than bolted beside it, so the
-- tables go with the code instead of lingering as a schema nobody reads.
--
-- ── Held orders ─────────────────────────────────────────────────────────────
-- A parked order is a CLIENT-LOCAL draft again: one terminal's way to juggle
-- several orders at once. It needs no server identity, no claim lease across
-- tills, and no offline replay op. An order reaches the floor only by becoming
-- an open ticket, which is what now owns a table.
--
-- `held_orders` never reached production at all -- the migration that created
-- it is in the tree but was never applied there -- so this drops it where it
-- exists (dev and test databases) and is a no-op everywhere else.
--
-- `table_transfer_requests` STAYS: a transfer wish is genuinely shared (a host
-- works the queue from elsewhere in the room). Its `occupant_kind` column is
-- now always 'open_ticket'; the column is kept and constrained rather than
-- dropped, so the rebuilt booking flow can reuse the queue without a migration.

-- Bookings: drop the dependent FK column first.
ALTER TABLE open_tickets DROP COLUMN IF EXISTS booking_id;

DROP TABLE IF EXISTS booking_nudges;
DROP TABLE IF EXISTS booking_tables;
DROP TABLE IF EXISTS bookings;
DROP TABLE IF EXISTS branch_reservation_settings;

-- Held orders: any transfer wish belonging to one has no owner left.
DELETE FROM table_transfer_requests WHERE occupant_kind <> 'open_ticket';
DROP TABLE IF EXISTS held_orders;

ALTER TABLE table_transfer_requests
    DROP CONSTRAINT IF EXISTS table_transfer_requests_occupant_kind_check;
ALTER TABLE table_transfer_requests
    ALTER COLUMN occupant_kind SET DEFAULT 'open_ticket',
    ADD CONSTRAINT table_transfer_requests_occupant_kind_check
        CHECK (occupant_kind = 'open_ticket');
