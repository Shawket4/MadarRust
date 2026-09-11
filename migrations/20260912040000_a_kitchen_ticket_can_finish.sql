-- A kitchen ticket can finish.
--
-- Nothing has ever closed one. A kitchen ticket is `firing` until every line
-- is bumped, and that is the ONLY way out that is not a void. At a branch that
-- has no kitchen screen — routing mode `till`, where every line is kept and
-- nobody bumps anything — a ticket fires, the food goes out, the bill is
-- settled, and the kitchen ticket stays `firing` for ever. Settling a bill
-- does not touch it; voiding an order does not touch it either. In production
-- that is 4,334 of 4,731 tickets, 3,485 of them at one branch, whose KDS feed
-- has quietly become a list of every round it has served since July.
--
-- Two things were missing, and this adds both.
--
-- FIRST, the ticket did not really know what it was for. `source_type` +
-- `source_id` pointed at an order or an open ticket by convention only — no
-- foreign key, so nothing could cascade to it and nothing could be asked to
-- close it when its source finished. Now it carries real references:
--
--   * `order_id`      — a counter order fired straight to the kitchen;
--   * `open_ticket_id` + `round_id` — one round of a waiter's open ticket.
--
-- Exactly one source is set. Delivery is NOT a source: a delivery order
-- materialises into an `orders` row before anything could reach the kitchen,
-- and today that path does not fire at all. `source_type` and `source_id`
-- survive — the KDS wire contract and every read site use them — but they are
-- now GENERATED from the references, so the pair and the keys cannot disagree.
--
-- SECOND, a ticket can now close: `closed_at` says when, `close_reason` says
-- why, `closed_by` says who. The kitchen `bumped` it (every line done — the
-- only close a recall can reopen), its bill was `settled`, its bill or round
-- was `voided`, or it was `retired` by hand. A close is the end of the
-- ticket's life on the screen; `status` stays what it always was, the state
-- of the cooking. The feed becomes "not closed, at this branch, oldest first",
-- and there is an index for exactly that.
--
-- THEN a one-time close of the backlog: every `firing` ticket whose bill is
-- already settled or voided, and every ticket for a counter order older than
-- a day (an order is paid the moment it fires, so nothing later will ever
-- close it — a kitchen that has not bumped yesterday's order is not going
-- to). These are closed as `retired`, dated now, by nobody. NOT `bumped` and
-- NOT `settled`: the kitchen never bumped them and the settle never closed
-- them, and a report that says otherwise is a lie. The six that remain
-- belong to bills still open on the floor; they close when those do.
--
-- This is a visible change to that branch's KDS feed: ~3,500 stale tickets
-- disappear from it the moment the feed honours `closed_at`.

CREATE TYPE kitchen_ticket_close_reason AS ENUM ('bumped', 'settled', 'voided', 'retired');

-- ── What the ticket is for ──────────────────────────────────────────────────

ALTER TABLE kitchen_tickets
    ADD COLUMN order_id       uuid REFERENCES orders(id)             ON DELETE CASCADE,
    ADD COLUMN open_ticket_id uuid REFERENCES open_tickets(id)       ON DELETE CASCADE,
    ADD COLUMN round_id       uuid REFERENCES open_ticket_rounds(id) ON DELETE CASCADE,
    ADD COLUMN closed_at      timestamp with time zone,
    ADD COLUMN close_reason   kitchen_ticket_close_reason,
    ADD COLUMN closed_by      uuid REFERENCES users(id);

-- Cascade, deliberately: the demo sweeper and shift deletion remove orders,
-- and a kitchen copy of a sale that no longer exists is not worth keeping.

UPDATE kitchen_tickets SET order_id       = source_id WHERE source_type = 'order';
UPDATE kitchen_tickets SET open_ticket_id = source_id WHERE source_type = 'open_ticket';
UPDATE kitchen_tickets kt
   SET round_id = r.id
  FROM open_ticket_rounds r
 WHERE kt.open_ticket_id = r.open_ticket_id
   AND kt.round_number   = r.round_number;

-- A round belongs to a ticket; the kitchen ticket names both, so the pair is
-- held to agree by the database rather than by every writer remembering to.
ALTER TABLE open_ticket_rounds
    ADD CONSTRAINT uq_open_ticket_rounds_id_ticket UNIQUE (id, open_ticket_id);

ALTER TABLE kitchen_tickets
    ADD CONSTRAINT kitchen_tickets_has_one_source
        CHECK (num_nonnulls(order_id, open_ticket_id) = 1),
    ADD CONSTRAINT kitchen_tickets_round_iff_open_ticket
        CHECK ((round_id IS NULL) = (open_ticket_id IS NULL)),
    ADD CONSTRAINT kitchen_tickets_round_belongs_to_ticket
        FOREIGN KEY (round_id, open_ticket_id)
        REFERENCES open_ticket_rounds (id, open_ticket_id) ON DELETE CASCADE;

-- `source_type` / `source_id` become a projection of the references. Dropping
-- and re-adding is the only way to make an existing column GENERATED; the
-- index that served the old lookups is put back so the read sites that still
-- filter on the pair are no slower than they were.
DROP INDEX idx_kitchen_tickets_source;
ALTER TABLE kitchen_tickets
    DROP CONSTRAINT kitchen_tickets_source_chk,
    DROP COLUMN source_type,
    DROP COLUMN source_id;
ALTER TABLE kitchen_tickets
    ADD COLUMN source_type text NOT NULL
        GENERATED ALWAYS AS (CASE WHEN order_id IS NOT NULL THEN 'order' ELSE 'open_ticket' END) STORED,
    ADD COLUMN source_id uuid NOT NULL
        GENERATED ALWAYS AS (COALESCE(order_id, open_ticket_id)) STORED;
CREATE INDEX idx_kitchen_tickets_source ON kitchen_tickets (source_type, source_id);

-- One kitchen ticket per round. `20260912020000` states this on the pair, and
-- Postgres drops an index with its column, so the rebuild above took it; it
-- is put back under its own name and definition, because the per-round
-- readiness lookup that migration describes is keyed on exactly these three.
CREATE UNIQUE INDEX uq_kitchen_tickets_source_round
    ON kitchen_tickets (source_type, source_id, round_number);

-- ── How it finishes ─────────────────────────────────────────────────────────

-- What was already over, recorded as such. A voided ticket closed when it was
-- voided; a ready ticket closed when its last line was bumped, by whoever
-- bumped it. Both are true statements about the past.
UPDATE kitchen_tickets
   SET closed_at = voided_at, close_reason = 'voided'
 WHERE status = 'voided' AND closed_at IS NULL;

UPDATE kitchen_tickets kt
   SET closed_at    = kt.ready_at,
       close_reason = 'bumped',
       closed_by    = (SELECT i.bumped_by FROM kitchen_ticket_items i
                        WHERE i.kitchen_ticket_id = kt.id AND i.bumped_at IS NOT NULL
                        ORDER BY i.bumped_at DESC LIMIT 1)
 WHERE kt.status = 'ready' AND kt.closed_at IS NULL;

-- The backlog. `retired`, dated now, by nobody — see the header.
UPDATE kitchen_tickets kt
   SET closed_at = now(), close_reason = 'retired'
 WHERE kt.closed_at IS NULL
   AND kt.status = 'firing'
   AND (
        (kt.order_id IS NOT NULL AND kt.created_at < now() - interval '1 day')
     OR (kt.open_ticket_id IS NOT NULL AND EXISTS (
            SELECT 1 FROM open_tickets t
             WHERE t.id = kt.open_ticket_id AND t.status IN ('settled', 'voided')))
   );

-- The invariants. A close has a reason; a voided or ready ticket is a closed
-- one. The bump and void paths must now write the close alongside the status.
ALTER TABLE kitchen_tickets
    ADD CONSTRAINT kitchen_tickets_close_has_reason
        CHECK ((closed_at IS NULL) = (close_reason IS NULL)),
    ADD CONSTRAINT kitchen_tickets_voided_is_closed
        CHECK (status <> 'voided' OR closed_at IS NOT NULL),
    ADD CONSTRAINT kitchen_tickets_ready_is_closed
        CHECK (status <> 'ready' OR closed_at IS NOT NULL);

-- ── What the KDS asks ───────────────────────────────────────────────────────

-- The feed: this branch's live tickets, oldest first. Partial, because once
-- the backlog is closed the live set is a few dozen rows out of thousands.
CREATE INDEX idx_kitchen_tickets_live
    ON kitchen_tickets (branch_id, created_at)
    WHERE closed_at IS NULL;

-- Settle and void find the tickets by their source; the cascades need these
-- too, or deleting an order walks the whole table. The round one is UNIQUE:
-- it is `uq_kitchen_tickets_source_round` said in keys — one fire, one round,
-- one kitchen ticket.
CREATE INDEX        idx_kitchen_tickets_order       ON kitchen_tickets (order_id)       WHERE order_id       IS NOT NULL;
CREATE INDEX        idx_kitchen_tickets_open_ticket ON kitchen_tickets (open_ticket_id) WHERE open_ticket_id IS NOT NULL;
CREATE UNIQUE INDEX uq_kitchen_tickets_round        ON kitchen_tickets (round_id)       WHERE round_id       IS NOT NULL;

COMMENT ON COLUMN kitchen_tickets.order_id IS
    'The counter order this ticket was fired for. Exactly one of order_id /
     open_ticket_id is set; source_type and source_id are generated from them.';
COMMENT ON COLUMN kitchen_tickets.open_ticket_id IS
    'The open ticket whose round this ticket was fired for; round_id names the
     round. Set together or not at all.';
COMMENT ON COLUMN kitchen_tickets.round_id IS
    'The round of open_ticket_id this ticket was fired for. One fire event, one
     round, one kitchen ticket.';
COMMENT ON COLUMN kitchen_tickets.closed_at IS
    'When the ticket left the kitchen''s attention for good. NULL while live;
     the KDS feed is the live set. Always paired with close_reason.';
COMMENT ON COLUMN kitchen_tickets.close_reason IS
    'Why it closed: bumped (every line done — the only close a recall may
     reopen), settled (its bill was paid), voided (its bill or round was
     voided), retired (closed by hand or by migration, never by the kitchen).
     The FIRST close wins; a later settle or void does not rewrite it.';
COMMENT ON COLUMN kitchen_tickets.closed_by IS
    'Who closed it — the last bumper, the settling cashier, the voider. NULL
     for a retirement.';
COMMENT ON COLUMN kitchen_tickets.status IS
    'The state of the cooking: firing, ready (every line bumped), voided.
     Orthogonal to closed_at — a ticket may close firing (its bill settled
     before the kitchen finished, or nobody bumps at this branch).';
