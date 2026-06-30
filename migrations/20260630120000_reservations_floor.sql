-- Reservations & waitlist — part 1: the graphical floor plan.
--
-- Sections (floors/areas), table geometry, and per-branch reservation settings.
-- The existing thin `branch_tables` (created QR-only in 20260618010000) is
-- EXTENDED into a floor entity here rather than replaced, so table-QR rows keep
-- working. Geometry is dashboard-authored; `status` is operational (POS + the
-- nudge scheduler).

CREATE TABLE floor_sections (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id      uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id   uuid NOT NULL REFERENCES branches(id)      ON DELETE CASCADE,
    name        text    NOT NULL,
    ordering    integer NOT NULL DEFAULT 0,
    canvas_w    integer NOT NULL DEFAULT 1000,
    canvas_h    integer NOT NULL DEFAULT 700,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX idx_floor_sections_branch ON floor_sections (branch_id);

ALTER TABLE branch_tables
    ADD COLUMN IF NOT EXISTS section_id uuid REFERENCES floor_sections(id) ON DELETE SET NULL,
    ADD COLUMN IF NOT EXISTS seats      smallint         NOT NULL DEFAULT 2,
    ADD COLUMN IF NOT EXISTS shape      text             NOT NULL DEFAULT 'rect',
    ADD COLUMN IF NOT EXISTS pos_x      double precision NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS pos_y      double precision NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS width      double precision NOT NULL DEFAULT 80,
    ADD COLUMN IF NOT EXISTS height     double precision NOT NULL DEFAULT 80,
    ADD COLUMN IF NOT EXISTS rotation   double precision NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS status     text             NOT NULL DEFAULT 'free';

ALTER TABLE branch_tables
    ADD CONSTRAINT branch_tables_shape_chk  CHECK (shape  IN ('rect','circle')),
    ADD CONSTRAINT branch_tables_status_chk CHECK (status IN ('free','held','seated','dirty')),
    ADD CONSTRAINT branch_tables_seats_pos  CHECK (seats >= 0);

CREATE INDEX IF NOT EXISTS idx_branch_tables_section ON branch_tables (section_id);

CREATE TABLE branch_reservation_settings (
    branch_id              uuid PRIMARY KEY REFERENCES branches(id) ON DELETE CASCADE,
    accepting_reservations boolean NOT NULL DEFAULT false,
    accepting_waitlist     boolean NOT NULL DEFAULT false,
    -- Flat lead: the departure nudge fires `lead_minutes` before `reserved_for`.
    lead_minutes           integer NOT NULL DEFAULT 30,
    -- A pre-assigned table flips to 'held' this many minutes before the booking.
    hold_lead_minutes      integer NOT NULL DEFAULT 120,
    -- No-show warn fires this many minutes after `reserved_for` with no arrival.
    grace_minutes          integer NOT NULL DEFAULT 15,
    max_party_size         integer,
    slot_minutes           integer NOT NULL DEFAULT 15,
    updated_at             timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT brs_lead_nonneg  CHECK (lead_minutes      >= 0),
    CONSTRAINT brs_hold_nonneg  CHECK (hold_lead_minutes >= 0),
    CONSTRAINT brs_grace_nonneg CHECK (grace_minutes     >= 0),
    CONSTRAINT brs_slot_pos     CHECK (slot_minutes      >  0),
    CONSTRAINT brs_party_pos    CHECK (max_party_size IS NULL OR max_party_size > 0)
);
