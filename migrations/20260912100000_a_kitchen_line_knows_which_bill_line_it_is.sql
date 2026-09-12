-- A kitchen line knows which bill line it is.
--
-- Voiding ONE line of an open bill — "take the calamari off, they changed
-- their mind" — has been half-built since the tightening migration: the
-- columns are there (`open_ticket_items.voided_at/voided_by/void_reason/
-- void_note`), the constraints are there, the permission seeder already says
-- a waiter may take a line off a bill. What is missing is the act, and the
-- reason the act could not be written is here, in this table.
--
-- A kitchen line has no way back to the bill line it came from. The two are
-- created in the same loop from the same resolved list, and for a client fire
-- their ids are both derived — but from DIFFERENT things, and not by the same
-- index: in `kds` routing mode `emit_kitchen_ticket` DROPS lines that route to
-- no station, so the nth kitchen line is not the nth bill line. Matching them
-- back by menu item and quantity works until a table orders two of the same
-- thing and one of them is sent back, which is the case the feature exists for.
--
-- So the link is recorded. Nullable, because every row written before now has
-- no answer and inventing one by position would be exactly the guess this
-- column exists to stop: a NULL here means "fired before the link existed",
-- and a line void on such a round takes the money off the bill without
-- reaching the board. That is the honest degradation — the alternative is
-- pulling the wrong plate off a screen a cook is working from.
--
-- ON DELETE CASCADE mirrors the bill line's own cascade: a round deleted takes
-- its kitchen copy with it, and a dangling kitchen line pointing at a bill line
-- that no longer exists would be worse than no link at all.

ALTER TABLE kitchen_ticket_items
    ADD COLUMN open_ticket_item_id uuid
        REFERENCES open_ticket_items(id) ON DELETE CASCADE;

COMMENT ON COLUMN kitchen_ticket_items.open_ticket_item_id IS
    'The bill line this kitchen line was fired from, so voiding that line can
     take this one off the board. NULL on counter-order fires (an order has no
     open_ticket_items) and on any round fired before 2026-09-12.';

-- The lookup the void performs: given a bill line, its live kitchen copies.
-- Partial, because a bumped or already-voided line is not something a void has
-- anything left to do to.
CREATE INDEX idx_kti_bill_line ON kitchen_ticket_items (open_ticket_item_id)
    WHERE open_ticket_item_id IS NOT NULL AND bumped_at IS NULL AND voided_at IS NULL;
