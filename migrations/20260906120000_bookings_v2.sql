-- Bookings v2 — a future claim on floor capacity, rebuilt on the floor/ticket
-- layer. Nothing here writes `branch_tables.status`: a table is "held" when a
-- confirmed booking is about to start (derived at read time), and occupied only
-- when the POS seats the party (an open ticket carrying `booking_id`).
--
-- Every write is guarded in the database, not just in a handler:
--   * `booking_tables` carries the booking's time range and an EXCLUDE
--     constraint, so two active bookings can never claim one table for
--     overlapping windows, whichever code path inserts them.
--   * triggers keep that range/active flag in lockstep with the booking row.
CREATE EXTENSION IF NOT EXISTS btree_gist;

-- The previous enum (requested/notified/arrived/...) survived the 2026-09-05
-- drop; nothing references it any more. Public bookings auto-confirm, so there
-- is no `requested` state: a booking exists because capacity was claimed.
DROP TYPE IF EXISTS booking_status;
CREATE TYPE booking_status AS ENUM ('confirmed', 'seated', 'completed', 'no_show', 'cancelled');

-- ── Per-branch booking settings ──────────────────────────────────────────────
CREATE TABLE branch_booking_settings (
    branch_id                uuid PRIMARY KEY REFERENCES branches(id) ON DELETE CASCADE,
    org_id                   uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- Online (public) booking switch. Host bookings work regardless.
    enabled                  boolean  NOT NULL DEFAULT false,
    -- Weekly opening windows for bookings: [{"dow":0..6,"open":"12:00","close":"23:00"}].
    -- dow 0 = Sunday. A day absent from the list takes no online bookings.
    hours                    jsonb    NOT NULL DEFAULT '[
        {"dow":0,"open":"12:00","close":"23:00"},{"dow":1,"open":"12:00","close":"23:00"},
        {"dow":2,"open":"12:00","close":"23:00"},{"dow":3,"open":"12:00","close":"23:00"},
        {"dow":4,"open":"12:00","close":"23:00"},{"dow":5,"open":"12:00","close":"23:00"},
        {"dow":6,"open":"12:00","close":"23:00"}]'::jsonb,
    slot_minutes             smallint NOT NULL DEFAULT 30  CHECK (slot_minutes IN (15, 30, 60)),
    default_duration_minutes smallint NOT NULL DEFAULT 90  CHECK (default_duration_minutes BETWEEN 15 AND 600),
    min_party                smallint NOT NULL DEFAULT 1   CHECK (min_party >= 1),
    max_party                smallint NOT NULL DEFAULT 12  CHECK (max_party >= min_party),
    -- Earliest online slot is now + lead_time; latest is today + horizon.
    lead_time_minutes        integer  NOT NULL DEFAULT 60  CHECK (lead_time_minutes >= 0),
    horizon_days             smallint NOT NULL DEFAULT 30  CHECK (horizon_days BETWEEN 1 AND 365),
    -- The floor shows the table as held from starts_at - hold_minutes.
    hold_minutes             smallint NOT NULL DEFAULT 15  CHECK (hold_minutes BETWEEN 0 AND 180),
    -- A confirmed party not seated this long after starts_at rolls to no_show
    -- (NULL = never automatically).
    auto_no_show_minutes     smallint                      CHECK (auto_no_show_minutes IS NULL OR auto_no_show_minutes BETWEEN 5 AND 240),
    -- WhatsApp reminder this long before starts_at (NULL = no reminder).
    reminder_lead_minutes    integer                       CHECK (reminder_lead_minutes IS NULL OR reminder_lead_minutes BETWEEN 15 AND 2880),
    -- Online guests must verify their phone by WhatsApp code.
    require_otp              boolean  NOT NULL DEFAULT true,
    -- Optional ceiling on guests whose bookings START in one slot.
    max_covers_per_slot      integer                       CHECK (max_covers_per_slot IS NULL OR max_covers_per_slot > 0),
    -- ISO dates with no online slots: ["2026-12-25"].
    blackout_dates           jsonb    NOT NULL DEFAULT '[]'::jsonb,
    created_at               timestamptz NOT NULL DEFAULT now(),
    updated_at               timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE branch_booking_settings ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON branch_booking_settings FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT ALL ON TABLE branch_booking_settings TO sufrix;

-- ── Bookings ─────────────────────────────────────────────────────────────────
CREATE TABLE bookings (
    id                    uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id                uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id             uuid NOT NULL REFERENCES branches(id)      ON DELETE CASCADE,
    status                booking_status NOT NULL DEFAULT 'confirmed',
    party_size            smallint NOT NULL CHECK (party_size > 0),
    starts_at             timestamptz NOT NULL,
    ends_at               timestamptz NOT NULL,
    guest_name            text NOT NULL,
    guest_phone           text NOT NULL,
    phone_verified        boolean NOT NULL DEFAULT false,
    notes                 text,
    -- `public` (self-service site) or `host` (dashboard / POS).
    source                text NOT NULL DEFAULT 'host' CHECK (source IN ('public', 'host')),
    -- Guest's language for WhatsApp messages.
    locale                text NOT NULL DEFAULT 'en' CHECK (locale IN ('en', 'ar')),
    -- Seating preference; assignment favours tables in this section.
    section_id            uuid REFERENCES floor_sections(id) ON DELETE SET NULL,
    -- The ticket that seated this party (set when the POS fires it).
    open_ticket_id        uuid,
    -- Public manage link secret.
    manage_token          text NOT NULL UNIQUE DEFAULT encode(gen_random_bytes(16), 'hex'),
    created_by            uuid REFERENCES users(id) ON DELETE SET NULL,
    cancel_reason         text,
    cancelled_by          text CHECK (cancelled_by IS NULL OR cancelled_by IN ('guest', 'host', 'system')),
    seated_at             timestamptz,
    completed_at          timestamptz,
    cancelled_at          timestamptz,
    no_show_at            timestamptz,
    reminder_sent_at      timestamptz,
    arriving_notified_at  timestamptz,
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT bookings_window_chk CHECK (ends_at > starts_at)
);
CREATE INDEX idx_bookings_branch_starts ON bookings (branch_id, starts_at);
CREATE INDEX idx_bookings_branch_status ON bookings (branch_id, status);
CREATE INDEX idx_bookings_phone         ON bookings (guest_phone);
ALTER TABLE bookings ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON bookings FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT ALL ON TABLE bookings TO sufrix;

-- The seating link. SET NULL so deleting a booking never orphans a ticket.
ALTER TABLE open_tickets ADD COLUMN booking_id uuid REFERENCES bookings(id) ON DELETE SET NULL;
CREATE INDEX idx_open_tickets_booking ON open_tickets (booking_id) WHERE booking_id IS NOT NULL;
ALTER TABLE bookings ADD CONSTRAINT bookings_open_ticket_fk
    FOREIGN KEY (open_ticket_id) REFERENCES open_tickets(id) ON DELETE SET NULL;

-- ── Table claims (multi-table parties) ───────────────────────────────────────
CREATE TABLE booking_tables (
    booking_id uuid NOT NULL REFERENCES bookings(id)      ON DELETE CASCADE,
    table_id   uuid NOT NULL REFERENCES branch_tables(id) ON DELETE CASCADE,
    org_id     uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- Mirrors bookings.[starts_at, ends_at) — maintained by trigger.
    during     tstzrange NOT NULL,
    -- Mirrors status IN (confirmed, seated) — maintained by trigger. Only
    -- active claims take part in the overlap exclusion.
    active     boolean NOT NULL DEFAULT true,
    PRIMARY KEY (booking_id, table_id),
    CONSTRAINT booking_tables_no_overlap
        EXCLUDE USING gist (table_id WITH =, during WITH &&) WHERE (active)
);
CREATE INDEX idx_booking_tables_table ON booking_tables (table_id);
ALTER TABLE booking_tables ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON booking_tables FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT ALL ON TABLE booking_tables TO sufrix;

-- Fill the claim's range/active/org from its booking on insert, so no writer
-- can create a claim that disagrees with the booking it belongs to.
CREATE OR REPLACE FUNCTION booking_tables_fill() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    b RECORD;
BEGIN
    SELECT org_id, starts_at, ends_at, status INTO b FROM bookings WHERE id = NEW.booking_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'booking % not found', NEW.booking_id;
    END IF;
    NEW.org_id := b.org_id;
    NEW.during := tstzrange(b.starts_at, b.ends_at, '[)');
    NEW.active := b.status IN ('confirmed', 'seated');
    RETURN NEW;
END $$;
CREATE TRIGGER booking_tables_fill BEFORE INSERT ON booking_tables
    FOR EACH ROW EXECUTE FUNCTION booking_tables_fill();

-- Re-sync claims when the booking moves or leaves the active states. A move
-- into an overlapping claim raises through the exclusion constraint — the
-- handler surfaces that as a 409 and the booking keeps its old window.
CREATE OR REPLACE FUNCTION bookings_sync_tables() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.starts_at IS DISTINCT FROM OLD.starts_at
       OR NEW.ends_at IS DISTINCT FROM OLD.ends_at
       OR NEW.status IS DISTINCT FROM OLD.status THEN
        UPDATE booking_tables
           SET during = tstzrange(NEW.starts_at, NEW.ends_at, '[)'),
               active = NEW.status IN ('confirmed', 'seated')
         WHERE booking_id = NEW.id;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER bookings_sync_tables AFTER UPDATE ON bookings
    FOR EACH ROW EXECUTE FUNCTION bookings_sync_tables();

-- Permission resource for the host surfaces. Seeded per role on boot.
ALTER TYPE permission_resource ADD VALUE IF NOT EXISTS 'bookings';
