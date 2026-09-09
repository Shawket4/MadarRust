-- At most one live ticket per table, stated in the database.
--
-- This invariant has always been the point of the floor — "at most one live
-- occupant per table" — and it has only ever been enforced in application code,
-- by a `SELECT … FOR UPDATE` on the table row before inserting. That held while
-- tickets were created rarely, at the moment a waiter fired a first round.
--
-- Seating changes the arithmetic. A ticket is now opened the moment a party
-- sits down, which makes ticket creation the most common act on the floor and
-- turns a rare race into a busy one: two tellers tapping the same table during
-- a rush is no longer hypothetical, it is Friday.
--
-- The lock still comes first — a constraint violation is a 409 nobody enjoys —
-- but the index means a path that forgets to lock cannot seat two parties at
-- one table. `settled` and `voided` are excluded: a table is free the moment
-- its ticket leaves the floor, and the row stays for the books.
CREATE UNIQUE INDEX uq_open_tickets_live_table
    ON open_tickets (table_id)
    WHERE table_id IS NOT NULL AND status IN ('open', 'ready');
