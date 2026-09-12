-- A sale remembers the table it was eaten at, when the party sat, and how many.
--
-- Until now a settled order reached its table only through its ticket
-- (`orders.open_ticket_id` -> `open_tickets.table_id`), and nothing anywhere
-- knew when the party actually SAT DOWN: a party is seated with a bare `party`
-- hold long before their first round opens a ticket, and the ledger row that
-- hold wrote was ended `seated` and forgotten the moment the ticket took over.
-- Dwell time was measured from the bill, so the longest wait on the floor — the
-- one before anybody ordered — was invisible to every clock and every report.
--
-- 1. `table_occupancies.seated_at`: when the party sat. A hold stamps it (with
--    the till's own clock, clamped server-side, so an offline seat replayed an
--    hour later still says when it happened); a ticket that takes over a hold
--    inherits it, and so does the row a moved ticket opens on its new table.
--    `started_at` is deliberately NOT backdated: it orders the ledger.
-- 2. `open_tickets.seated_at`: the same instant, on the bill.
-- 3. `orders.table_id / seated_at / covers`: copied from the ticket when it
--    settles, so table analytics read one row per sale with no join chain.
-- 4. `v_table_status.seated_at`: every device's floor shows the same clock.

ALTER TABLE table_occupancies ADD COLUMN seated_at timestamptz;
ALTER TABLE open_tickets     ADD COLUMN seated_at timestamptz;

ALTER TABLE orders
    ADD COLUMN table_id  uuid REFERENCES branch_tables(id) ON DELETE SET NULL,
    ADD COLUMN seated_at timestamptz,
    ADD COLUMN covers    integer,
    ADD CONSTRAINT orders_covers_positive CHECK (covers IS NULL OR covers > 0);

CREATE INDEX idx_orders_table_created ON orders (table_id, created_at)
    WHERE table_id IS NOT NULL;

COMMENT ON COLUMN table_occupancies.seated_at IS
    'When the party sat down. Stamped by a hold, inherited by the ticket that
     takes over the hold and by a moved ticket''s new row. NULL = started_at.';
COMMENT ON COLUMN orders.table_id IS
    'The table the settled ticket was on when it settled (NULL for counter sales).';
COMMENT ON COLUMN orders.seated_at IS
    'When the party sat down (the hold, else the ticket''s opened_at).';
COMMENT ON COLUMN orders.covers IS 'Guests on the settled ticket.';

-- ── Backfill ────────────────────────────────────────────────────────────────
-- Existing rows cannot know the hold that preceded them; the ledger's own start
-- and the bill's opening are the best the data holds.
UPDATE open_tickets SET seated_at = opened_at WHERE seated_at IS NULL;

UPDATE orders o
   SET table_id  = ot.table_id,
       seated_at = COALESCE(ot.seated_at, ot.opened_at),
       covers    = CASE WHEN ot.guest_count > 0 THEN ot.guest_count END
  FROM open_tickets ot
 WHERE o.open_ticket_id = ot.id
   AND o.table_id IS NULL;

-- ── The view, with the clock ────────────────────────────────────────────────
-- Same shape as before, one column appended (CREATE OR REPLACE may only add).
CREATE OR REPLACE VIEW v_table_status WITH (security_invoker = true) AS
SELECT
    s.table_id,
    s.org_id,
    s.branch_id,
    s.section_id,
    s.label,
    s.is_active,
    s.status,
    CASE s.status
        WHEN 'held'   THEN s.started_at
        WHEN 'seated' THEN s.started_at
        WHEN 'dirty'  THEN s.ended_at
        ELSE COALESCE(s.cleared_at, s.ended_at)
    END AS since,
    CASE WHEN s.status IN ('held', 'seated') THEN s.occupancy_id    END AS occupancy_id,
    CASE WHEN s.status IN ('held', 'seated') THEN s.held_by         END AS held_by,
    CASE WHEN s.status IN ('held', 'seated') THEN s.open_ticket_id  END AS open_ticket_id,
    CASE WHEN s.status IN ('held', 'seated') THEN s.booking_id      END AS booking_id,
    CASE WHEN s.status IN ('held', 'seated') THEN s.party_size      END AS party_size,
    CASE WHEN s.status IN ('held', 'seated') THEN s.started_by      END AS started_by,
    CASE WHEN s.status IN ('held', 'seated') THEN s.started_till_id END AS started_till_id,
    CASE WHEN s.status IN ('held', 'seated') THEN s.started_device  END AS started_device,
    -- When the party sat: only a seated table has one (a booking's hold is a
    -- claim on the table, not a party at it).
    CASE WHEN s.status = 'seated' THEN COALESCE(s.seated_at, s.started_at) END AS seated_at
FROM (
    SELECT
        t.id AS table_id, t.org_id, t.branch_id, t.section_id, t.label, t.is_active,
        o.id AS occupancy_id, o.held_by, o.open_ticket_id, o.booking_id, o.party_size,
        o.started_at, o.started_by, o.started_till_id, o.started_device,
        o.ended_at, o.cleared_at, o.seated_at,
        CASE
            WHEN o.id IS NULL                                 THEN 'free'
            WHEN o.ended_at IS NULL AND o.held_by = 'booking' THEN 'held'
            WHEN o.ended_at IS NULL                           THEN 'seated'
            WHEN o.needs_bussing AND o.cleared_at IS NULL     THEN 'dirty'
            ELSE 'free'
        END AS status
    FROM branch_tables t
    LEFT JOIN LATERAL (
        SELECT * FROM table_occupancies o
         WHERE o.table_id = t.id
         ORDER BY o.started_at DESC, o.id DESC
         LIMIT 1
    ) o ON true
) s;
