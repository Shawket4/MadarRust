-- Purchasing: suppliers, purchase orders, and order lines. Receiving a PO
-- increments branch stock (converting purchase units to stock units via
-- units_per_purchase_unit), posts a 'purchase_in' inventory_movements row, and
-- updates the ingredient's weighted moving-average cost.

CREATE TABLE suppliers (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id       uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name         text NOT NULL,
    contact_name text,
    phone        text,
    email        text,
    is_active    boolean NOT NULL DEFAULT true,
    deleted_at   timestamp with time zone,
    created_at   timestamp with time zone NOT NULL DEFAULT now(),
    updated_at   timestamp with time zone NOT NULL DEFAULT now()
);
CREATE INDEX idx_suppliers_org ON suppliers (org_id) WHERE deleted_at IS NULL;

CREATE TYPE purchase_order_status AS ENUM ('draft', 'ordered', 'received', 'cancelled');

CREATE TABLE purchase_orders (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id      uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id   uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    supplier_id uuid REFERENCES suppliers(id) ON DELETE SET NULL,
    status      purchase_order_status NOT NULL DEFAULT 'draft',
    reference   text,
    note        text,
    expected_at timestamp with time zone,
    received_at timestamp with time zone,
    received_by uuid REFERENCES users(id),
    created_by  uuid NOT NULL REFERENCES users(id),
    created_at  timestamp with time zone NOT NULL DEFAULT now(),
    updated_at  timestamp with time zone NOT NULL DEFAULT now()
);
CREATE INDEX idx_purchase_orders_branch ON purchase_orders (branch_id, created_at DESC);
CREATE INDEX idx_purchase_orders_org    ON purchase_orders (org_id);

CREATE TABLE purchase_order_lines (
    id                      uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    purchase_order_id       uuid NOT NULL REFERENCES purchase_orders(id) ON DELETE CASCADE,
    org_ingredient_id       uuid NOT NULL REFERENCES org_ingredients(id) ON DELETE RESTRICT,
    purchase_unit           text NOT NULL,
    -- How many base STOCK units one purchase unit yields (e.g. a 24-pack → 24,
    -- a kg bag for a 'g' ingredient → 1000).
    units_per_purchase_unit numeric(12,4) NOT NULL DEFAULT 1,
    quantity_ordered        numeric(12,3) NOT NULL,
    quantity_received       numeric(12,3) NOT NULL DEFAULT 0,
    -- Piastres per PURCHASE unit.
    unit_cost               bigint NOT NULL,
    created_at              timestamp with time zone NOT NULL DEFAULT now()
);
CREATE INDEX idx_po_lines_order ON purchase_order_lines (purchase_order_id);
