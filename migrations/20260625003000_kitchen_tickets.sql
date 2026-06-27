-- The source-agnostic KDS substrate. A "kitchen ticket" is one fire event — a
-- waiter open-ticket round OR a whole teller (counter) order — projected to what
-- the kitchen must cook. Each line carries its FROZEN station (resolved at fire
-- time) and its own bump state. The KDS reads `kitchen_ticket_items` and never
-- cares whether the source was a waiter ticket or a paid counter order.
--
-- This is a display projection: the line jsonb is slim (name/qty/size/modifiers/
-- notes — NO prices). The bill lives elsewhere (order_items / open_ticket_items).

CREATE TYPE kitchen_ticket_status AS ENUM ('firing', 'ready', 'voided');

CREATE TABLE kitchen_tickets (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id       uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id    uuid NOT NULL REFERENCES branches(id)      ON DELETE CASCADE,
    -- Polymorphic source (no FK — points at either an order or an open_ticket).
    source_type  text NOT NULL,
    source_id    uuid NOT NULL,
    table_label  text,
    kitchen_ref  text,
    round_number integer NOT NULL DEFAULT 1,
    status       kitchen_ticket_status NOT NULL DEFAULT 'firing',
    created_at   timestamp with time zone NOT NULL DEFAULT now(),
    ready_at     timestamp with time zone,
    voided_at    timestamp with time zone,
    CONSTRAINT kitchen_tickets_source_chk CHECK (source_type IN ('order', 'open_ticket'))
);
CREATE INDEX idx_kitchen_tickets_branch ON kitchen_tickets (branch_id, status);
CREATE INDEX idx_kitchen_tickets_source ON kitchen_tickets (source_type, source_id);

CREATE TABLE kitchen_ticket_items (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kitchen_ticket_id uuid NOT NULL REFERENCES kitchen_tickets(id)  ON DELETE CASCADE,
    -- Frozen station routing (resolved at fire time; survives config edits).
    station_id        uuid REFERENCES kitchen_stations(id) ON DELETE SET NULL,
    menu_item_id      uuid REFERENCES menu_items(id),
    line              jsonb   NOT NULL,
    qty               integer NOT NULL DEFAULT 1,
    bumped_at         timestamp with time zone,
    bumped_by         uuid REFERENCES users(id),
    voided_at         timestamp with time zone,
    created_at        timestamp with time zone NOT NULL DEFAULT now()
);
CREATE INDEX idx_kti_ticket ON kitchen_ticket_items (kitchen_ticket_id);
-- The KDS feed query: a station's outstanding (un-bumped, un-voided) work.
CREATE INDEX idx_kti_station_pending ON kitchen_ticket_items (station_id)
    WHERE bumped_at IS NULL AND voided_at IS NULL;

GRANT ALL ON kitchen_tickets      TO sufrix;
GRANT ALL ON kitchen_ticket_items TO sufrix;
