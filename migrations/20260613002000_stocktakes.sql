-- Standalone stocktake (physical count) sessions, independent of cashier
-- shifts. A stocktake snapshots current branch stock as `expected_qty`, the
-- operator records `counted_qty` per ingredient, and finalize reconciles
-- branch_inventory.current_stock to the count, posting a 'stock_count'
-- inventory_movements row for each non-zero variance.

CREATE TYPE stocktake_status AS ENUM ('draft', 'in_progress', 'finalized', 'cancelled');

CREATE TABLE stocktakes (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id       uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id    uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    status       stocktake_status NOT NULL DEFAULT 'in_progress',
    note         text,
    started_by   uuid NOT NULL REFERENCES users(id),
    started_at   timestamp with time zone NOT NULL DEFAULT now(),
    finalized_by uuid REFERENCES users(id),
    finalized_at timestamp with time zone,
    created_at   timestamp with time zone NOT NULL DEFAULT now()
);

CREATE INDEX idx_stocktakes_branch ON stocktakes (branch_id, started_at DESC);
CREATE INDEX idx_stocktakes_org    ON stocktakes (org_id);

CREATE TABLE stocktake_items (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    stocktake_id        uuid NOT NULL REFERENCES stocktakes(id) ON DELETE CASCADE,
    org_ingredient_id   uuid NOT NULL REFERENCES org_ingredients(id) ON DELETE RESTRICT,
    branch_inventory_id uuid REFERENCES branch_inventory(id) ON DELETE SET NULL,
    expected_qty        numeric(12,3) NOT NULL,
    counted_qty         numeric(12,3),
    variance            numeric(12,3) GENERATED ALWAYS AS (counted_qty - expected_qty) STORED,
    unit_cost           bigint,
    note                text,
    counted_by          uuid REFERENCES users(id),
    created_at          timestamp with time zone NOT NULL DEFAULT now(),
    UNIQUE (stocktake_id, org_ingredient_id)
);

CREATE INDEX idx_stocktake_items_stocktake ON stocktake_items (stocktake_id);
