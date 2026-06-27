-- Kitchen Display System: stations (Grill, Bar, Dessert…) per branch, the
-- category→station + per-item routing maps, and the per-branch routing mode that
-- decides whether fired tickets show on KDS screens, the till, or both.
--
-- A station carries its own printer config (the client device prints; the backend
-- just stores it), mirroring the branch printer shape. `is_default` marks the
-- catch-all station for lines that resolve to no explicit station.

CREATE TABLE kitchen_stations (
    id                uuid    PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id            uuid    NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id         uuid    NOT NULL REFERENCES branches(id)      ON DELETE CASCADE,
    name              text    NOT NULL,
    name_translations jsonb   NOT NULL DEFAULT '{}'::jsonb,
    sort_order        integer NOT NULL DEFAULT 0,
    printer_brand     printer_brand,
    printer_ip        text,
    printer_port      integer,
    is_default        boolean NOT NULL DEFAULT false,
    is_active         boolean NOT NULL DEFAULT true,
    created_at        timestamp with time zone NOT NULL DEFAULT now(),
    updated_at        timestamp with time zone NOT NULL DEFAULT now(),
    deleted_at        timestamp with time zone,
    CONSTRAINT kitchen_stations_port_chk
        CHECK (printer_port IS NULL OR printer_port BETWEEN 1 AND 65535)
);
CREATE INDEX        idx_kitchen_stations_branch  ON kitchen_stations (branch_id) WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX uq_kitchen_stations_default  ON kitchen_stations (branch_id) WHERE is_default AND deleted_at IS NULL;
CREATE UNIQUE INDEX uq_kitchen_stations_name     ON kitchen_stations (branch_id, lower(name)) WHERE deleted_at IS NULL;

-- Category → station (the default routing rule for every item in the category).
CREATE TABLE category_station_routes (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    branch_id   uuid NOT NULL REFERENCES branches(id)         ON DELETE CASCADE,
    category_id uuid NOT NULL REFERENCES categories(id)       ON DELETE CASCADE,
    station_id  uuid NOT NULL REFERENCES kitchen_stations(id) ON DELETE CASCADE,
    created_at  timestamp with time zone NOT NULL DEFAULT now(),
    UNIQUE (branch_id, category_id)
);
CREATE INDEX idx_csr_station ON category_station_routes (station_id);

-- Per-item override (wins over the category rule).
CREATE TABLE menu_item_station_routes (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    branch_id    uuid NOT NULL REFERENCES branches(id)         ON DELETE CASCADE,
    menu_item_id uuid NOT NULL REFERENCES menu_items(id)       ON DELETE CASCADE,
    station_id   uuid NOT NULL REFERENCES kitchen_stations(id) ON DELETE CASCADE,
    created_at   timestamp with time zone NOT NULL DEFAULT now(),
    UNIQUE (branch_id, menu_item_id)
);
CREATE INDEX idx_misr_station ON menu_item_station_routes (menu_item_id);

-- Where fired tickets surface. NULL = auto (kds if the branch has any station,
-- else till). An explicit value is the dashboard override.
CREATE TYPE kitchen_routing_mode AS ENUM ('kds', 'till', 'both');
ALTER TABLE branches ADD COLUMN kitchen_routing_mode kitchen_routing_mode;

GRANT ALL ON kitchen_stations          TO sufrix;
GRANT ALL ON category_station_routes   TO sufrix;
GRANT ALL ON menu_item_station_routes  TO sufrix;
