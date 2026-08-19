-- Held orders (teller parked carts) + the table-transfer waitlist.
--
-- A held order is the TELLER's parked cart, promoted from a device-local kv
-- blob to a first-class, branch-shared entity so it can OWN a floor table and
-- survive the till it was parked on. The cart payload stays CLIENT-authored
-- and opaque (the same StoredLine JSON the POS core persists locally): the
-- server brokers identity, table occupancy, cross-device claims, and sync —
-- it never prices or interprets the lines. Money enters the books only when
-- the cart is checked out through the normal create-order path.
--
-- Sync model (mirrors the POS outbox): ids are CLIENT-minted (offline-first
-- identity), rows are soft-terminated ('completed'/'discarded' are tombstones
-- so a `since` pull can retire local copies), and `revision` provides a cheap
-- conflict fence for cross-device edits.
--
-- Table occupancy: at most ONE live occupant per table across BOTH dine-in
-- entities (held_orders here, open_tickets from 20260625004000). The partial
-- unique index below enforces the held-order half; the cross-entity half is
-- enforced transactionally (lock the branch_tables row, check both tables) in
-- src/held_orders — the same choreography open-ticket moves use.

CREATE TABLE held_orders (
    id                uuid PRIMARY KEY,   -- client-minted (offline-first identity)
    org_id            uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id         uuid NOT NULL REFERENCES branches(id)      ON DELETE CASCADE,
    table_id          uuid REFERENCES branch_tables(id) ON DELETE SET NULL,
    name              text  NOT NULL DEFAULT '',
    -- Opaque client cart: {"lines": [...], "discount_id": ...} — recorded verbatim.
    cart              jsonb NOT NULL DEFAULT '{}'::jsonb,
    -- 'held' (parked) | 'resumed' (live on a till, still owns its table) |
    -- 'completed' / 'discarded' (tombstones for sync).
    status            text  NOT NULL DEFAULT 'held'
                      CHECK (status IN ('held','resumed','completed','discarded')),
    created_by        uuid REFERENCES users(id) ON DELETE SET NULL,
    -- The device that parked it last / holds the resume claim (free-text device
    -- installation id from the POS core; not a FK — devices aren't entities here).
    device_id         text,
    claimed_by_device text,
    claimed_at        timestamptz,
    -- The paid order this cart became, once completed.
    order_id          uuid REFERENCES orders(id) ON DELETE SET NULL,
    revision          bigint NOT NULL DEFAULT 1,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT held_orders_name_len CHECK (char_length(name) <= 120)
);

-- The `since` sync pull (branch + updated_at cursor) and the active-board list.
CREATE INDEX idx_held_orders_branch_updated ON held_orders (branch_id, updated_at);
-- One LIVE held order per table (its half of the occupancy invariant).
CREATE UNIQUE INDEX uq_held_orders_live_table ON held_orders (table_id)
    WHERE table_id IS NOT NULL AND status IN ('held','resumed');

-- ── Transfer waitlist ────────────────────────────────────────────────────────
-- A party ALREADY seated (or an outside/no-table order) queued to move to a
-- zone (any table in a section) or to one specific table. Distinct from the
-- deprecated bookings waitlist, which queues people not yet inside. The
-- occupant is polymorphic: a teller-parked held order or a waiter open ticket.

CREATE TABLE table_transfer_requests (
    id                 uuid PRIMARY KEY,  -- client-minted (offline-first identity)
    org_id             uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id          uuid NOT NULL REFERENCES branches(id)      ON DELETE CASCADE,
    occupant_kind      text NOT NULL CHECK (occupant_kind IN ('held_order','open_ticket')),
    occupant_id        uuid NOT NULL,
    -- Where the party sits now (NULL = outside / no table yet).
    from_table_id      uuid REFERENCES branch_tables(id) ON DELETE SET NULL,
    -- The wish: a whole section ("anywhere inside") or one exact table.
    target_section_id  uuid REFERENCES floor_sections(id) ON DELETE CASCADE,
    target_table_id    uuid REFERENCES branch_tables(id)  ON DELETE CASCADE,
    note               text,
    status             text NOT NULL DEFAULT 'waiting'
                       CHECK (status IN ('waiting','fulfilled','cancelled')),
    requested_by       uuid REFERENCES users(id) ON DELETE SET NULL,
    -- The table the move actually landed on (section targets pick one at fulfill).
    fulfilled_table_id uuid REFERENCES branch_tables(id) ON DELETE SET NULL,
    created_at         timestamptz NOT NULL DEFAULT now(),
    resolved_at        timestamptz,
    updated_at         timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT ttr_target_present CHECK (target_section_id IS NOT NULL OR target_table_id IS NOT NULL),
    CONSTRAINT ttr_note_len CHECK (note IS NULL OR char_length(note) <= 500)
);

CREATE INDEX idx_ttr_branch_updated ON table_transfer_requests (branch_id, updated_at);
-- FIFO scans of the live queue.
CREATE INDEX idx_ttr_branch_waiting ON table_transfer_requests (branch_id, created_at)
    WHERE status = 'waiting';
-- One live wish per party.
CREATE UNIQUE INDEX uq_ttr_waiting_occupant ON table_transfer_requests (occupant_kind, occupant_id)
    WHERE status = 'waiting';

-- RLS: org-rooted, mirroring the generator in 20260708000100_rls_policies.sql.
ALTER TABLE held_orders ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON held_orders FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
ALTER TABLE table_transfer_requests ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON table_transfer_requests FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));

GRANT ALL ON held_orders             TO sufrix;
GRANT ALL ON table_transfer_requests TO sufrix;
