-- Occupancy is a LEDGER, not a status word.
--
-- `branch_tables.status` folds three unrelated facts into one column — "a
-- party is sitting here", "a till parked a draft here", "this needs bussing" —
-- and records nothing about any of them: no owner, no timestamp, no device.
-- Nothing can answer "who seated this table, when, from which till", a parked
-- draft holds a table anonymously, and the column has reached states no screen
-- can get it out of. `held` has been in the CHECK since the floor was authored
-- and no code path has ever written it; the bookings rebuild derives its holds
-- at read time precisely because the column could not be trusted to carry one.
--
-- The deeper problem is that a status is a CACHE of a fact nobody stored. Every
-- writer must remember to walk it (seat, free, bus) and the settle path once
-- grew its own inline copy of the `dirty` UPDATE — two writers of one
-- invariant, drifting apart by construction. A cache with no source of truth
-- behind it cannot be rebuilt when it is wrong, and it has been wrong.
--
-- So the fact is stored instead. `table_occupancies` is an append-only ledger:
-- one row per time a table was taken — under a ticket, a booking, or a bare
-- party hold — with who took it, when, from which till and device, and when it
-- ended and why. A party that has paid and left leaves the table `dirty`, and
-- clearing it is a recorded human act on the same row. The table's CURRENT
-- status is DERIVED from the ledger by `v_table_status`, never cached by hand.
--
-- `branch_tables.status` is NOT dropped here. Code still reads it, so for the
-- transition a trigger projects the derived status back into the column on
-- every ledger write. That makes the ledger the single writer of the column
-- from the moment the application moves to it; the column and the trigger go
-- together, in a later migration, once nothing reads it.

-- ── The ledger ──────────────────────────────────────────────────────────────
CREATE TABLE table_occupancies (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id           uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id        uuid NOT NULL REFERENCES branches(id)      ON DELETE CASCADE,
    -- A table deleted from the floor editor takes its history with it: a
    -- ledger row with no table cannot say which table it was about.
    table_id         uuid NOT NULL REFERENCES branch_tables(id) ON DELETE CASCADE,

    -- What holds the table. `ticket` — a party with a bill; `booking` — a
    -- confirmed booking's claim, before the party has a ticket; `party` — a
    -- bare hold with no bill yet (a walk-in waiting on the rest of their
    -- party, or a till parking a draft on the table).
    held_by          text NOT NULL,
    -- Tickets and bookings are never deleted by the application (settled and
    -- voided rows stay for the books), so the default NO ACTION is right: the
    -- ledger will not let its subject be deleted from under it. Deleting the
    -- whole org still works — the org cascade reaches both sides in the same
    -- statement.
    open_ticket_id   uuid REFERENCES open_tickets(id),
    booking_id       uuid REFERENCES bookings(id),
    party_size       smallint,

    started_at       timestamptz NOT NULL DEFAULT now(),
    -- NULL when nobody did it: a hold placed by the bookings scheduler.
    started_by       uuid REFERENCES users(id) ON DELETE SET NULL,
    -- The till entity (NULL from a waiter's handheld or the dashboard) and the
    -- POS installation id, which is free text because devices are not
    -- entities here.
    started_till_id  uuid REFERENCES tills(id) ON DELETE SET NULL,
    started_device   text,

    ended_at         timestamptz,
    ended_by         uuid REFERENCES users(id) ON DELETE SET NULL,
    ended_till_id    uuid REFERENCES tills(id) ON DELETE SET NULL,
    ended_device     text,
    -- `settled`  — the bill was paid; the party is leaving.
    -- `voided`   — the ticket was voided.
    -- `moved`    — the party moved to another table (a new row opens there).
    -- `seated`   — a booking or bare hold became a ticket (a new row opens).
    -- `released` — the hold was let go with no sale: draft discarded, walk-in
    --              left, booking cancelled.
    -- `no_show`  — a booking's hold ran out with nobody seated.
    end_reason       text,

    -- Set when the occupancy ended with plates on the table. Clearing it is a
    -- human act recorded here; the table reads `dirty` until it is.
    needs_bussing    boolean NOT NULL DEFAULT false,
    cleared_at       timestamptz,
    cleared_by       uuid REFERENCES users(id) ON DELETE SET NULL,

    updated_at       timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT table_occupancies_held_by_chk
        CHECK (held_by IN ('ticket', 'booking', 'party')),
    CONSTRAINT table_occupancies_end_reason_chk
        CHECK (end_reason IS NULL OR end_reason IN
               ('settled', 'voided', 'moved', 'seated', 'released', 'no_show')),
    CONSTRAINT table_occupancies_party_size_pos
        CHECK (party_size IS NULL OR party_size > 0),

    -- The link agrees with the kind. A ticket row names its ticket (and may
    -- name the booking it seated); a booking row names its booking and has no
    -- ticket yet; a bare party hold names neither.
    CONSTRAINT table_occupancies_link_matches_kind CHECK (
        (held_by = 'ticket'  AND open_ticket_id IS NOT NULL)
     OR (held_by = 'booking' AND booking_id IS NOT NULL AND open_ticket_id IS NULL)
     OR (held_by = 'party'   AND booking_id IS NULL     AND open_ticket_id IS NULL)
    ),

    -- An ending has a reason, a reason has an ending, and it comes after the start.
    CONSTRAINT table_occupancies_ended_has_reason
        CHECK ((ended_at IS NULL) = (end_reason IS NULL)),
    CONSTRAINT table_occupancies_ends_after_start
        CHECK (ended_at IS NULL OR ended_at >= started_at),

    -- The reason fits the kind: only a ticket settles or is voided, only a
    -- hold is seated, only a booking no-shows.
    CONSTRAINT table_occupancies_reason_fits_kind CHECK (
        end_reason IS NULL
     OR (end_reason IN ('settled', 'voided') AND held_by = 'ticket')
     OR (end_reason = 'seated'               AND held_by <> 'ticket')
     OR (end_reason = 'no_show'              AND held_by = 'booking')
     OR  end_reason IN ('moved', 'released')
    ),

    -- Only something that ended can need bussing, and only something that
    -- needs bussing can be cleared, after it ended.
    CONSTRAINT table_occupancies_bussing_follows_end
        CHECK (NOT needs_bussing OR ended_at IS NOT NULL),
    CONSTRAINT table_occupancies_cleared_needs_bussing
        CHECK (cleared_at IS NULL OR (needs_bussing AND cleared_at >= ended_at))
);

-- The invariant the floor has always meant: at most one live occupant per
-- table, whichever code path inserts. `uq_open_tickets_live_table` states the
-- ticket half; this states it for every kind at once.
CREATE UNIQUE INDEX uq_table_occupancies_live_table
    ON table_occupancies (table_id) WHERE ended_at IS NULL;
-- And a ticket sits on at most one table at a time.
CREATE UNIQUE INDEX uq_table_occupancies_live_ticket
    ON table_occupancies (open_ticket_id) WHERE ended_at IS NULL AND open_ticket_id IS NOT NULL;
-- No live-per-booking index on purpose: a large party claims several tables.

-- The view's "latest row for this table" walk.
CREATE INDEX idx_table_occupancies_table_latest
    ON table_occupancies (table_id, started_at DESC, id DESC);
-- The floor board (live rows in a branch) and a `since` sync pull.
CREATE INDEX idx_table_occupancies_branch_live
    ON table_occupancies (branch_id) WHERE ended_at IS NULL;
CREATE INDEX idx_table_occupancies_branch_updated
    ON table_occupancies (branch_id, updated_at);
-- Where has this ticket sat? (moves leave one row per table)
CREATE INDEX idx_table_occupancies_ticket
    ON table_occupancies (open_ticket_id) WHERE open_ticket_id IS NOT NULL;
CREATE INDEX idx_table_occupancies_booking
    ON table_occupancies (booking_id) WHERE booking_id IS NOT NULL;

ALTER TABLE table_occupancies ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON table_occupancies FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT ALL ON TABLE table_occupancies TO sufrix;
-- Stated rather than left to ALTER DEFAULT PRIVILEGES, which only covers
-- tables created by the role that ran the RLS migration: a database restored
-- from production and migrated by a different role would otherwise hand the
-- tenant pool a table it cannot read.
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE table_occupancies TO madar_app;

COMMENT ON TABLE table_occupancies IS
    'Append-only ledger of every time a table was taken: under what, by whom,
     from where, and how it ended. The table''s current status is derived from
     it by v_table_status; rows are ended and cleared, never deleted.';
COMMENT ON COLUMN table_occupancies.needs_bussing IS
    'The occupancy ended with the party''s plates still on the table. The table
     reads `dirty` until `cleared_at` is set — a human act, never automatic.';

-- ── The derived status ──────────────────────────────────────────────────────
-- The latest ledger row for a table says everything the old column said, and
-- says it honestly:
--   live row under a booking          → held
--   live row under a ticket or party  → seated
--   ended, needs bussing, not cleared → dirty
--   anything else, or no row at all   → free
-- Occupant details are exposed only while the table is actually taken; a free
-- table does not advertise its last party.
--
-- `security_invoker` so the underlying tables' RLS applies to whoever queries
-- the view — a view runs as its owner otherwise, which is an RLS bypass.
CREATE VIEW v_table_status WITH (security_invoker = true) AS
SELECT
    s.table_id,
    s.org_id,
    s.branch_id,
    s.section_id,
    s.label,
    s.is_active,
    s.status,
    -- When the table entered its current status.
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
    CASE WHEN s.status IN ('held', 'seated') THEN s.started_device  END AS started_device
FROM (
    SELECT
        t.id AS table_id, t.org_id, t.branch_id, t.section_id, t.label, t.is_active,
        o.id AS occupancy_id, o.held_by, o.open_ticket_id, o.booking_id, o.party_size,
        o.started_at, o.started_by, o.started_till_id, o.started_device,
        o.ended_at, o.cleared_at,
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

GRANT SELECT ON v_table_status TO sufrix;
GRANT SELECT ON v_table_status TO madar_app;

COMMENT ON VIEW v_table_status IS
    'A table''s current status (free | held | seated | dirty), derived from the
     latest table_occupancies row. Read this, never branch_tables.status.';

-- ── Transitional: project the derived status into the old column ────────────
-- Until the column is dropped, readers that still consult it must see the
-- truth. Every ledger write recomputes the one table it touched. This is the
-- ONLY sanctioned writer of `branch_tables.status` from here on; the walks in
-- src/floor_ops that write it directly are what the application replaces with
-- ledger writes. Dropped together with the column.
CREATE OR REPLACE FUNCTION table_occupancies_project_status() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    UPDATE branch_tables bt
       SET status = v.status, updated_at = now()
      FROM v_table_status v
     WHERE bt.id = NEW.table_id
       AND v.table_id = bt.id
       AND bt.status IS DISTINCT FROM v.status;
    RETURN NULL;
END $$;

CREATE TRIGGER table_occupancies_project_status
    AFTER INSERT OR UPDATE ON table_occupancies
    FOR EACH ROW EXECUTE FUNCTION table_occupancies_project_status();

-- ── Backfill ────────────────────────────────────────────────────────────────
-- The column cannot be trusted, so the ledger is rebuilt from what can: the
-- tickets. Production today has one table, `free`, and every ticket ever
-- opened has NULL table_id, so nothing below inserts there — but a dev or
-- test database may carry seated tables, and the rules must hold everywhere.
--
-- 1. A live ticket on a table is a live occupancy, whatever the column says.
INSERT INTO table_occupancies
    (org_id, branch_id, table_id, held_by, open_ticket_id, booking_id, party_size,
     started_at, started_by, updated_at)
SELECT ot.org_id, ot.branch_id, ot.table_id, 'ticket', ot.id, ot.booking_id,
       CASE WHEN ot.guest_count BETWEEN 1 AND 32767 THEN ot.guest_count::smallint END,
       ot.opened_at, ot.opened_by, ot.opened_at
  FROM open_tickets ot
 WHERE ot.table_id IS NOT NULL
   AND ot.status IN ('open', 'ready');

-- 2. A `dirty` table with nobody on it is a party that paid and left. Where
--    the last settled ticket on that table is known, the ledger names it;
--    otherwise an anonymous party row carries the bussing debt so the table
--    still reads `dirty` until someone clears it.
INSERT INTO table_occupancies
    (org_id, branch_id, table_id, held_by, open_ticket_id, booking_id, party_size,
     started_at, started_by, ended_at, ended_by, end_reason, needs_bussing, updated_at)
SELECT bt.org_id, bt.branch_id, bt.id,
       CASE WHEN ot.id IS NULL THEN 'party' ELSE 'ticket' END,
       ot.id, ot.booking_id,
       CASE WHEN ot.guest_count BETWEEN 1 AND 32767 THEN ot.guest_count::smallint END,
       COALESCE(ot.opened_at, bt.updated_at),
       ot.opened_by,
       GREATEST(COALESCE(ot.settled_at, bt.updated_at), COALESCE(ot.opened_at, bt.updated_at)),
       ot.settled_by,
       CASE WHEN ot.id IS NULL THEN 'released' ELSE 'settled' END,
       true,
       bt.updated_at
  FROM branch_tables bt
  LEFT JOIN LATERAL (
      SELECT t.id, t.org_id, t.booking_id, t.guest_count, t.opened_at, t.opened_by,
             t.settled_at, t.settled_by
        FROM open_tickets t
       WHERE t.table_id = bt.id AND t.status = 'settled'
       ORDER BY t.settled_at DESC NULLS LAST, t.opened_at DESC
       LIMIT 1
  ) ot ON true
 WHERE bt.status = 'dirty'
   AND NOT EXISTS (SELECT 1 FROM table_occupancies o
                    WHERE o.table_id = bt.id AND o.ended_at IS NULL);

-- 3. The column now agrees with the ledger. This is where a stale `seated`
--    with no ticket behind it, or the never-written `held`, becomes `free`.
UPDATE branch_tables bt
   SET status = v.status, updated_at = now()
  FROM v_table_status v
 WHERE v.table_id = bt.id
   AND bt.status IS DISTINCT FROM v.status;

COMMENT ON COLUMN branch_tables.status IS
    'DEPRECATED — a projection of v_table_status kept only until nothing reads
     it. Written by the table_occupancies_project_status trigger; do not write
     it from code.';
