-- Waiter open tickets: fire-now-pay-later. A waiter (no shift) opens a dine-in
-- ticket and fires items in ROUNDS over time; a cashier/teller SETTLES it later,
-- materializing a normal paid `orders` row (via delivery::snapshot::apply_snapshot)
-- into the settling teller's shift. The bill lives here (priced lines); the
-- kitchen copy lives in kitchen_tickets/_items.
--
-- Mirrors the delivery_orders "separate entity → materialize on settle" pattern.

CREATE TYPE open_ticket_status AS ENUM ('open', 'ready', 'settled', 'voided');

CREATE TABLE open_tickets (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id           uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id        uuid NOT NULL REFERENCES branches(id)      ON DELETE CASCADE,
    table_id         uuid REFERENCES branch_tables(id) ON DELETE SET NULL,
    ticket_ref       text,
    status           open_ticket_status NOT NULL DEFAULT 'open',
    opened_by        uuid NOT NULL REFERENCES users(id),      -- the waiter
    customer_name    text,
    notes            text,
    guest_count      integer,
    subtotal         integer NOT NULL DEFAULT 0,              -- piastres
    -- Settlement linkage.
    order_id         uuid REFERENCES orders(id) ON DELETE SET NULL,
    settled_by       uuid REFERENCES users(id),               -- the cashier
    settled_shift_id uuid REFERENCES shifts(id),
    idempotency_key  uuid,
    opened_at        timestamp with time zone NOT NULL DEFAULT now(),
    ready_at         timestamp with time zone,
    settled_at       timestamp with time zone,
    voided_at        timestamp with time zone,
    void_reason      text,
    updated_at       timestamp with time zone NOT NULL DEFAULT now(),
    CONSTRAINT open_tickets_money_nonneg CHECK (subtotal >= 0)
);
CREATE INDEX        idx_open_tickets_branch_status ON open_tickets (branch_id, status);
CREATE INDEX        idx_open_tickets_table         ON open_tickets (table_id) WHERE table_id IS NOT NULL;
CREATE UNIQUE INDEX uq_open_tickets_idem           ON open_tickets (idempotency_key) WHERE idempotency_key IS NOT NULL;
CREATE UNIQUE INDEX uq_open_tickets_ref            ON open_tickets (ticket_ref) WHERE ticket_ref IS NOT NULL;

-- A round = one fire event (the waiter taps "fire" with a set of new lines).
CREATE TABLE open_ticket_rounds (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    open_ticket_id  uuid NOT NULL REFERENCES open_tickets(id) ON DELETE CASCADE,
    round_number    integer NOT NULL,
    fired_by        uuid NOT NULL REFERENCES users(id),
    idempotency_key uuid,
    fired_at        timestamp with time zone NOT NULL DEFAULT now(),
    UNIQUE (open_ticket_id, round_number)
);
CREATE UNIQUE INDEX uq_open_ticket_rounds_idem ON open_ticket_rounds (idempotency_key) WHERE idempotency_key IS NOT NULL;

-- The BILL lines (priced snapshot + frozen inventory plan) used at settlement.
CREATE TABLE open_ticket_items (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    open_ticket_id      uuid NOT NULL REFERENCES open_tickets(id)       ON DELETE CASCADE,
    round_id            uuid NOT NULL REFERENCES open_ticket_rounds(id) ON DELETE CASCADE,
    menu_item_id        uuid REFERENCES menu_items(id),
    line                jsonb   NOT NULL,                 -- one SnapshotLine
    deductions_snapshot jsonb   NOT NULL DEFAULT '[]'::jsonb,
    line_total          integer NOT NULL DEFAULT 0,
    voided_at           timestamp with time zone,
    created_at          timestamp with time zone NOT NULL DEFAULT now(),
    CONSTRAINT oti_line_total_nonneg CHECK (line_total >= 0)
);
CREATE INDEX idx_oti_ticket ON open_ticket_items (open_ticket_id);
CREATE INDEX idx_oti_round  ON open_ticket_items (round_id);

-- Human-readable per-(branch, business_date) ticket ref counter (mirrors
-- order_ref_counters; the settled order still mints its own order_ref).
CREATE TABLE ticket_ref_counters (
    branch_id     uuid NOT NULL REFERENCES branches(id),
    business_date date NOT NULL,
    last_seq      integer NOT NULL DEFAULT 0,
    PRIMARY KEY (branch_id, business_date)
);

GRANT ALL ON open_tickets        TO sufrix;
GRANT ALL ON open_ticket_rounds  TO sufrix;
GRANT ALL ON open_ticket_items   TO sufrix;
GRANT ALL ON ticket_ref_counters TO sufrix;
