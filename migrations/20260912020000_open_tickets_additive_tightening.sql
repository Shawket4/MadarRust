-- A ticket's status is what the BILL is, not what the kitchen is doing.
--
-- `open_tickets.status` has been carrying two different facts in one column.
-- Three of its values — open, settled, voided — describe the bill: is it still
-- on the floor, has it been paid, was it torn up. The fourth, `ready`, describes
-- the kitchen: every line has been bumped. Those move on different clocks, and
-- the code shows it — the KDS bump flips a ticket to `ready`, the next fired
-- round flips it straight back to `open`, and everything that wants to know
-- "is this bill still live" has to write `status IN ('open', 'ready')` and hope
-- it remembered. `uq_open_tickets_live_table` is one such spelling. Meanwhile
-- the readiness fact was never the ticket's to keep: it is a projection of
-- `kitchen_tickets`, one per round, and the KDS already recomputes it there on
-- every bump. Keeping a second copy on the bill guaranteed the two would drift.
--
-- So the vocabulary shrinks to what a bill can be. Per-round readiness is read
-- from `kitchen_tickets (source_type, source_id, round_number)`, which this
-- migration makes UNIQUE so that lookup has exactly one answer. `ready_at`
-- stays as the last moment the kitchen had the whole ticket plated — history,
-- not state.
--
-- Postgres cannot remove a label from an enum, so the type is rebuilt under the
-- same name. Every live `ready` ticket becomes `open`, which is what it is: a
-- bill that has not been paid. Nothing is lost — its readiness is still in
-- `kitchen_tickets`, where it was always being recomputed from anyway.
--
-- Three more gaps are closed while the table is open, all additive:
--
--   * A void was a status flip with an optional string. A void is an EVENT — it
--     has an actor, a categorised reason, and a note — and the orders table has
--     modelled it that way since `void_note`. The ticket now uses the same
--     `void_reason` enum and the same `void_note` column, so a report can count
--     wrong-order voids across counter sales and dine-in without a translation
--     layer. Ticket LINES get the same three columns, because "the waiter took
--     the calamari off" is a void too, and today it leaves no trace of who or
--     why. This wave also brings refunds in as a first-class feature; those are
--     a money event on the settled ORDER, never on the ticket, and the ticket
--     schema deliberately leaves them there — see the comment on `order_id`.
--
--   * Rounds were numbered by `MAX(round_number) + 1` with no lock. Two waiters
--     firing on the same table in the same second both computed the same next
--     number; the unique key turned that into a 500 for whichever lost. Now the
--     ticket carries `rounds_fired`, and a trigger on the round INSERT takes the
--     parent row lock, bumps the counter and hands out the number. Concurrent
--     fires serialise on the lock and cannot collide. A caller that passes an
--     explicit number is checked against the counter rather than trusted.
--
--   * The bill and its snapshot could disagree. `line_total` is a column and
--     ALSO a key inside the `line` jsonb; `subtotal` is a running sum that
--     nothing reconciles. Every existing row agrees with itself, so the
--     agreement becomes a CHECK before the first row that does not.

-- ── 1. The bill vocabulary ───────────────────────────────────────────────────

-- Both of these mention `ready` in their definitions and would be rebuilt with
-- the old literal. Dropped first, recreated below against the new type.
DROP INDEX uq_open_tickets_live_table;
DROP INDEX idx_open_tickets_branch_status;

ALTER TYPE open_ticket_status RENAME TO open_ticket_status_with_kitchen_state;
CREATE TYPE open_ticket_status AS ENUM ('open', 'settled', 'voided');

ALTER TABLE open_tickets ALTER COLUMN status DROP DEFAULT;
ALTER TABLE open_tickets
    ALTER COLUMN status TYPE open_ticket_status
    USING (CASE WHEN status::text = 'ready' THEN 'open' ELSE status::text END)::open_ticket_status;
ALTER TABLE open_tickets ALTER COLUMN status SET DEFAULT 'open';

DROP TYPE open_ticket_status_with_kitchen_state;

CREATE INDEX idx_open_tickets_branch_status ON open_tickets (branch_id, status);

-- Same invariant as before — at most one live occupant per table — now spelled
-- the only way it can be.
CREATE UNIQUE INDEX uq_open_tickets_live_table
    ON open_tickets (table_id)
    WHERE table_id IS NOT NULL AND status = 'open';

COMMENT ON TYPE open_ticket_status IS
    'What the BILL is: still on the floor, paid, or torn up. Kitchen readiness
     is not a bill state — read it per round from kitchen_tickets.';

COMMENT ON COLUMN open_tickets.ready_at IS
    'The last moment the kitchen had every line of this ticket plated. History
     for the timing reports, not state: whether the ticket is ready NOW is
     derived from its kitchen_tickets.';

-- One kitchen ticket per round, so "is round N ready" has exactly one answer.
-- Counter orders always fire round 1 and get exactly one kitchen ticket, so the
-- rule holds for both sources; a ticket fired in kitchen mode `off` simply has
-- no row here.
CREATE UNIQUE INDEX uq_kitchen_tickets_source_round
    ON kitchen_tickets (source_type, source_id, round_number);

-- ── 2. A void is an event ────────────────────────────────────────────────────

ALTER TABLE open_tickets
    ADD COLUMN voided_by uuid REFERENCES users(id),
    ADD COLUMN void_note text;

-- The old `void_reason` was free text assembled by the client as
-- `<label>` or `<label> — <note>` (em dash, spaced). The label was chosen from
-- a fixed picker; the note was typed. Mapping it onto the shared enum:
--
--   label                       → reason            note
--   'Order mistake'             → wrong_order       the part after the dash
--   'Wrong order'               → wrong_order       "
--   'Customer request'          → customer_request  "
--   'Quality issue'             → quality_issue     "
--   'Other'                     → other             "
--   anything else               → other             the WHOLE original string
--   NULL                        → NULL              NULL
--
-- Recognised labels round-trip exactly (label ↔ enum is one-to-one and the
-- note is carried verbatim). An unrecognised label is not guessed at: it goes
-- into the note whole, so no character of what the waiter wrote is lost, and
-- the reason is honestly `other`. Matching is case-insensitive and trims
-- whitespace, because a picker label typed by hand once is still that label.
--
-- The NULL reasons are the rows from before the picker existed. They stay NULL:
-- the orders table treats reason as optional for the same reason, and a CHECK
-- that pretends history had one would fail the migration on the first such row.
UPDATE open_tickets
SET void_note = CASE
    WHEN void_reason IS NULL OR btrim(void_reason) = '' THEN NULL
    WHEN lower(btrim(split_part(void_reason, ' — ', 1)))
         IN ('order mistake', 'wrong order', 'customer request', 'quality issue', 'other')
        THEN nullif(btrim(substr(void_reason,
                                 length(split_part(void_reason, ' — ', 1)) + length(' — ') + 1)), '')
    ELSE btrim(void_reason)
END
WHERE void_reason IS NOT NULL;

-- The NULL case is handled by the outer CASE on purpose: a simple
-- `CASE x WHEN …` never matches a NULL x and would fall through to `other`,
-- which would stamp a reason on every settled and open ticket in the table.
ALTER TABLE open_tickets
    ALTER COLUMN void_reason TYPE void_reason
    USING (CASE
               WHEN void_reason IS NULL OR btrim(void_reason) = '' THEN NULL
               ELSE CASE lower(btrim(split_part(void_reason, ' — ', 1)))
                        WHEN 'order mistake'    THEN 'wrong_order'
                        WHEN 'wrong order'      THEN 'wrong_order'
                        WHEN 'customer request' THEN 'customer_request'
                        WHEN 'quality issue'    THEN 'quality_issue'
                        WHEN 'other'            THEN 'other'
                        ELSE 'other'
                    END
           END)::void_reason;

COMMENT ON COLUMN open_tickets.void_reason IS
    'Categorised, the same enum as orders.void_reason, so void-rate reports
     read dine-in and counter alike. NULL only on tickets voided before the
     reason picker existed.';
COMMENT ON COLUMN open_tickets.void_note IS
    'What actually happened, in the waiter''s words. Required by the handler
     when the reason is `other`; optional otherwise.';
COMMENT ON COLUMN open_tickets.voided_by IS
    'Who tore the bill up. NULL only on tickets voided before voids were
     attributed; every new void names its actor.';

ALTER TABLE open_ticket_items
    ADD COLUMN voided_by   uuid REFERENCES users(id),
    ADD COLUMN void_reason void_reason,
    ADD COLUMN void_note   text;

COMMENT ON COLUMN open_ticket_items.voided_at IS
    'The line was taken off the bill before settlement. Its money never
     reached an order, so this is a correction, not a refund.';

-- A status is a claim; the timestamps are the evidence. They may not disagree.
-- Every one of the existing rows already satisfies this — it is stated so the
-- next code path that flips a status without recording the event is refused.
ALTER TABLE open_tickets
    ADD CONSTRAINT open_tickets_settled_is_an_event
        CHECK ((status = 'settled') = (settled_at IS NOT NULL AND settled_by IS NOT NULL)),
    ADD CONSTRAINT open_tickets_voided_is_an_event
        CHECK ((status = 'voided') = (voided_at IS NOT NULL)),
    ADD CONSTRAINT open_tickets_void_details_need_a_void
        CHECK (voided_at IS NOT NULL
               OR (voided_by IS NULL AND void_reason IS NULL AND void_note IS NULL));

-- Lines have no history of voids at all (none has ever been voided), so the
-- rule can be the full one from day one: a voided line names who voided it.
ALTER TABLE open_ticket_items
    ADD CONSTRAINT oti_void_is_attributed
        CHECK ((voided_at IS NULL) = (voided_by IS NULL)),
    ADD CONSTRAINT oti_void_details_need_a_void
        CHECK (voided_at IS NOT NULL OR (void_reason IS NULL AND void_note IS NULL));

-- Refunds and post-settlement voids belong to the ORDER. This index is the
-- walk back from that order to the bill it settled — the 21 tickets whose
-- orders were later voided are found this way, not by a mirrored status.
CREATE INDEX idx_open_tickets_order ON open_tickets (order_id) WHERE order_id IS NOT NULL;

COMMENT ON COLUMN open_tickets.order_id IS
    'The order this bill settled into. `settled` is terminal for the ticket:
     whatever happens to the money afterwards — a void, a refund — is an event
     on the order and is never mirrored back here. Join through this column to
     ask "was this bill''s sale later reversed".';

-- ── 3. Round numbers are handed out, not guessed ─────────────────────────────

ALTER TABLE open_tickets
    ADD COLUMN rounds_fired integer NOT NULL DEFAULT 0
        CONSTRAINT open_tickets_rounds_fired_nonneg CHECK (rounds_fired >= 0);

UPDATE open_tickets t
SET rounds_fired = r.n
FROM (SELECT open_ticket_id, max(round_number) AS n
      FROM open_ticket_rounds GROUP BY open_ticket_id) r
WHERE r.open_ticket_id = t.id;

ALTER TABLE open_ticket_rounds
    ADD CONSTRAINT open_ticket_rounds_number_starts_at_one CHECK (round_number >= 1);

-- The allocator. Inserting a round takes the parent ticket's row lock (the
-- UPDATE), so two fires on one ticket serialise and each sees its own number.
-- Insert with `round_number` NULL and the trigger fills it in; insert with an
-- explicit number and it must be the one the counter would have given — a
-- stale `MAX + 1` computed outside the lock is refused with a message that
-- says so, rather than surfacing as a duplicate-key error.
--
-- BEFORE ROW triggers run ahead of the NOT NULL check, which is what lets a
-- NOT NULL column be filled here.
CREATE FUNCTION open_ticket_rounds_take_number() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    allocated integer;
BEGIN
    UPDATE open_tickets
       SET rounds_fired = rounds_fired + 1
     WHERE id = NEW.open_ticket_id
    RETURNING rounds_fired INTO allocated;

    IF allocated IS NULL THEN
        RAISE EXCEPTION 'open ticket % not found for round', NEW.open_ticket_id
            USING ERRCODE = 'foreign_key_violation';
    END IF;

    IF NEW.round_number IS NULL THEN
        NEW.round_number := allocated;
    ELSIF NEW.round_number <> allocated THEN
        RAISE EXCEPTION 'round % on ticket % is stale: the next round is %',
            NEW.round_number, NEW.open_ticket_id, allocated
            USING ERRCODE = 'serialization_failure';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER trg_open_ticket_rounds_take_number
    BEFORE INSERT ON open_ticket_rounds
    FOR EACH ROW EXECUTE FUNCTION open_ticket_rounds_take_number();

COMMENT ON COLUMN open_tickets.rounds_fired IS
    'How many rounds this ticket has fired; the next round is rounds_fired + 1.
     Bumped under the row lock by the trigger on open_ticket_rounds, so it is
     the allocator, not a cache — never computed from MAX(round_number).';

-- ── 4. The bill agrees with itself ───────────────────────────────────────────

-- `line` is the priced snapshot; `line_total` is the column the settle path
-- sums. Repricing one without the other is exactly how a bill ends up charging
-- something other than what it printed.
ALTER TABLE open_ticket_items
    ADD CONSTRAINT oti_line_total_matches_snapshot
        CHECK (line_total = (line ->> 'line_total')::integer);

GRANT ALL ON FUNCTION open_ticket_rounds_take_number() TO sufrix;
