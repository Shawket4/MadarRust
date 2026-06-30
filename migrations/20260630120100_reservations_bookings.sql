-- Reservations & waitlist — part 2: the unified booking entity.
--
-- One row models both a reservation (has `reserved_for`) and a waitlist entry
-- (`reserved_for` NULL). `booking_tables` is the assignment/merge join;
-- `booking_nudges` is an idempotent WhatsApp send log so the scheduler can't
-- double-send across restarts. Phone verification reuses `delivery_otp`.

CREATE TYPE booking_status AS ENUM
    ('requested', 'confirmed', 'notified', 'arrived', 'seated', 'completed', 'no_show', 'cancelled');

CREATE TABLE bookings (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id          uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id       uuid NOT NULL REFERENCES branches(id)      ON DELETE CASCADE,
    kind            text     NOT NULL DEFAULT 'reservation',
    customer_name   text     NOT NULL,
    customer_phone  text     NOT NULL,
    party_size      smallint NOT NULL DEFAULT 1,
    reserved_for    timestamptz,                 -- NULL ⇒ waitlist / now
    quoted_ready_at timestamptz,                 -- waitlist: host estimate of table-ready
    customer_lat    double precision,
    customer_lng    double precision,
    otp_verified    boolean  NOT NULL DEFAULT false,
    source          text     NOT NULL DEFAULT 'staff',
    status          booking_status NOT NULL DEFAULT 'requested',
    notes           text,
    created_by      uuid REFERENCES users(id),
    notified_at     timestamptz,
    arrived_at      timestamptz,
    seated_at       timestamptz,
    completed_at    timestamptz,
    cancelled_at    timestamptz,
    no_show_at      timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT bookings_kind_chk   CHECK (kind   IN ('reservation', 'walk_in')),
    CONSTRAINT bookings_source_chk CHECK (source IN ('public', 'staff')),
    CONSTRAINT bookings_party_pos  CHECK (party_size > 0)
);
CREATE INDEX idx_bookings_branch_time   ON bookings (branch_id, reserved_for);
CREATE INDEX idx_bookings_branch_status ON bookings (branch_id, status);
CREATE INDEX idx_bookings_phone         ON bookings (customer_phone);

CREATE TABLE booking_tables (
    booking_id uuid NOT NULL REFERENCES bookings(id)      ON DELETE CASCADE,
    table_id   uuid NOT NULL REFERENCES branch_tables(id) ON DELETE CASCADE,
    PRIMARY KEY (booking_id, table_id)
);
CREATE INDEX idx_booking_tables_table ON booking_tables (table_id);

CREATE TABLE booking_nudges (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    booking_id  uuid NOT NULL REFERENCES bookings(id) ON DELETE CASCADE,
    kind        text NOT NULL,
    sent_at     timestamptz NOT NULL DEFAULT now(),
    eta_seconds integer,
    depart_by   timestamptz,
    CONSTRAINT booking_nudges_kind_chk
        CHECK (kind IN ('departure', 'table_ready', 'waitlist_headout', 'no_show_warn')),
    UNIQUE (booking_id, kind)
);

-- Link an open ticket back to the booking it was seated from, so moving the
-- ticket's table keeps the booking's assignment in sync (and vice-versa).
ALTER TABLE open_tickets
    ADD COLUMN IF NOT EXISTS booking_id uuid REFERENCES bookings(id) ON DELETE SET NULL;
