-- The branch plan: what hardware a branch has and how it talks (kitchen target
-- spec, madar/docs/specs/kitchen-target-spec.md, BB-* and CH-*).
--
-- The dashboard's branch builder places every piece on a canvas — POS and
-- waiter devices, kitchen screens, receipt and kitchen printers, and
-- kitchen sections (the existing `kitchen_stations`) — and draws how they
-- connect. It saves the whole plan in one request (`PUT /branch-plan`), so
-- these tables are only ever written together, in one transaction, by that
-- handler.
--
-- A device placed on the canvas is a SLOT until a real device claims it with an
-- activation code bound to the slot (`device_activation_codes.slot_id`). The
-- slot keeps its role; the device that fills it can be swapped without redrawing
-- anything.
--
-- Compatibility with what reads kitchen routing today: sections stay
-- `kitchen_stations` rows and categories stay `category_station_routes`; the
-- save also writes each section's first network kitchen printer back onto the
-- station's own `printer_*` columns, which is where the POS still looks for it.
-- None of the new tables is a sync source yet; the POS and kitchen app start
-- reading the plan in phase 2 of the spec.

CREATE TABLE branch_device_slots (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id             uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id          uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    kind               text NOT NULL CHECK (kind IN ('pos', 'waiter', 'kitchen')),
    name               text NOT NULL CHECK (btrim(name) <> ''),
    -- The real install filling this slot, once a slot code was used. No FK, as
    -- with `device_activation_codes.used_by_device` and `push_devices.device_id`:
    -- the tills rework's down script drops `devices`, and devices are retired,
    -- never deleted. `PUT /branch-plan` checks it is a live device of the branch.
    device_id          uuid NULL,
    -- For a POS or waiter device: where its receipts print. FK added below.
    receipt_printer_id uuid NULL,
    -- The order the builder lists pieces in; kept so a plan reads back as saved.
    sort_order         integer NOT NULL DEFAULT 0,
    canvas_x           double precision NOT NULL DEFAULT 0,
    canvas_y           double precision NOT NULL DEFAULT 0,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX branch_device_slots_branch ON branch_device_slots (branch_id);
-- One install fills at most one slot.
CREATE UNIQUE INDEX branch_device_slots_device ON branch_device_slots (device_id) WHERE device_id IS NOT NULL;

CREATE TABLE branch_printers (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id         uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id      uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    role           text NOT NULL CHECK (role IN ('receipt', 'kitchen')),
    name           text NOT NULL CHECK (btrim(name) <> ''),
    connection     text NOT NULL CHECK (connection IN ('network', 'usb', 'bluetooth')),
    brand          printer_brand NULL,
    ip             text NULL,
    port           integer NULL CHECK (port IS NULL OR port BETWEEN 1 AND 65535),
    paper_mm       integer NOT NULL DEFAULT 80 CHECK (paper_mm IN (58, 80)),
    -- A USB or Bluetooth printer is driven by the device it is plugged into.
    host_device_id uuid NULL REFERENCES branch_device_slots(id) ON DELETE SET NULL,
    sort_order     integer NOT NULL DEFAULT 0,
    canvas_x       double precision NOT NULL DEFAULT 0,
    canvas_y       double precision NOT NULL DEFAULT 0,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT branch_printers_network_has_ip CHECK (connection <> 'network' OR ip IS NOT NULL)
);
CREATE INDEX branch_printers_branch ON branch_printers (branch_id);

ALTER TABLE branch_device_slots
    ADD CONSTRAINT branch_device_slots_receipt_printer_fk
    FOREIGN KEY (receipt_printer_id) REFERENCES branch_printers(id) ON DELETE SET NULL;

-- A section's outputs: the kitchen screens its items show on and the kitchen
-- printers they print on (KS-7: any number of each).
CREATE TABLE kitchen_station_screens (
    org_id     uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    station_id uuid NOT NULL REFERENCES kitchen_stations(id) ON DELETE CASCADE,
    slot_id    uuid NOT NULL REFERENCES branch_device_slots(id) ON DELETE CASCADE,
    PRIMARY KEY (station_id, slot_id)
);
CREATE INDEX kitchen_station_screens_slot ON kitchen_station_screens (slot_id);

CREATE TABLE kitchen_station_printers (
    org_id     uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    station_id uuid NOT NULL REFERENCES kitchen_stations(id) ON DELETE CASCADE,
    printer_id uuid NOT NULL REFERENCES branch_printers(id) ON DELETE CASCADE,
    PRIMARY KEY (station_id, printer_id)
);
CREATE INDEX kitchen_station_printers_printer ON kitchen_station_printers (printer_id);

-- Where a section sits on the builder's canvas. NULL = never placed (a station
-- made before the builder); the builder lays those out itself.
ALTER TABLE kitchen_stations
    ADD COLUMN canvas_x double precision NULL,
    ADD COLUMN canvas_y double precision NULL;

-- The plan's version, for optimistic saving (two admins editing one branch),
-- and the case-1 switch: the till's receipt printer also prints kitchen chits.
ALTER TABLE branches
    ADD COLUMN hardware_plan_version integer NOT NULL DEFAULT 0,
    ADD COLUMN till_prints_kitchen boolean NOT NULL DEFAULT false;

-- Every saved plan, whole, with who saved it (BB-12, CH-8).
CREATE TABLE branch_plan_versions (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id     uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id  uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    version    integer NOT NULL,
    plan       jsonb NOT NULL,
    saved_by   uuid NULL REFERENCES users(id) ON DELETE SET NULL,
    saved_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (branch_id, version)
);

-- An activation code made for a slot: the device that uses it fills the slot.
ALTER TABLE device_activation_codes
    ADD COLUMN slot_id uuid NULL REFERENCES branch_device_slots(id) ON DELETE CASCADE;

-- Tenant isolation, the same policy every org table carries.
ALTER TABLE branch_device_slots ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON branch_device_slots
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
ALTER TABLE branch_printers ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON branch_printers
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
ALTER TABLE kitchen_station_screens ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON kitchen_station_screens
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
ALTER TABLE kitchen_station_printers ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON kitchen_station_printers
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
ALTER TABLE branch_plan_versions ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON branch_plan_versions
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));

GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE
    branch_device_slots, branch_printers, kitchen_station_screens, kitchen_station_printers
    TO madar_app;
GRANT SELECT, INSERT ON TABLE branch_plan_versions TO madar_app;
