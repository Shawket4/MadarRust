-- Warehouses (WAREHOUSE_DESIGN.md).
--
-- A warehouse is a `branches` row with kind = 'warehouse': every inventory
-- table already keys on branch_id, so stock, the ledger, counts, waste, POs
-- and par levels work there unchanged. It never sells: the selling tables
-- refuse a warehouse at INSERT (assert_selling_branch), so a filter missed
-- somewhere in the app fails loudly instead of selling from a warehouse.
--
-- Transfers become documents with lines and a lifecycle
-- (requested → draft → dispatched → received | cancelled; the rule is
-- madar_inventory::transfer::step). Every existing transfer was instant, so
-- it becomes a `received` transfer with one line.

-- ── Kind ────────────────────────────────────────────────────────────────
CREATE TYPE branch_kind AS ENUM ('branch', 'warehouse');
ALTER TABLE branches ADD COLUMN kind branch_kind NOT NULL DEFAULT 'branch';
CREATE INDEX idx_branches_org_kind ON branches (org_id, kind) WHERE deleted_at IS NULL;

-- How many live warehouses an org may have. NULL = unlimited. Set by a super
-- admin; there is no plan system yet.
ALTER TABLE organizations
    ADD COLUMN max_warehouses integer CHECK (max_warehouses >= 0);

-- ── A warehouse never sells ────────────────────────────────────────────
CREATE FUNCTION assert_selling_branch() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF NEW.branch_id IS NOT NULL AND EXISTS (
        SELECT 1 FROM branches WHERE id = NEW.branch_id AND kind = 'warehouse'
    ) THEN
        RAISE EXCEPTION 'WAREHOUSE_CANNOT_SELL: % rows cannot belong to a warehouse', TG_TABLE_NAME
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END $$;

DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY[
        'orders', 'open_tickets', 'tills', 'devices', 'device_activation_codes',
        'delivery_orders', 'bookings', 'branch_tables', 'kitchen_tickets',
        'qr_short_links', 'staff_drinks',
        -- The branch builder's pieces: a warehouse has no devices, printers or kitchen.
        'branch_device_slots', 'branch_printers', 'kitchen_stations'
    ] LOOP
        EXECUTE format(
            'CREATE TRIGGER trg_%1$s_selling_branch BEFORE INSERT OR UPDATE OF branch_id ON %1$I
             FOR EACH ROW EXECUTE FUNCTION assert_selling_branch()', t);
    END LOOP;
END $$;

-- ── Transfers: a document with lines ───────────────────────────────────
CREATE TYPE stock_transfer_status AS ENUM ('requested', 'draft', 'dispatched', 'received', 'cancelled');

ALTER TABLE stock_transfers
    ADD COLUMN status        stock_transfer_status NOT NULL DEFAULT 'draft',
    -- Per-org running number; shown as TR-<number>.
    ADD COLUMN number        integer,
    ADD COLUMN requested_at  timestamptz,
    ADD COLUMN requested_by  uuid REFERENCES users(id),
    ADD COLUMN dispatched_at timestamptz,
    ADD COLUMN dispatched_by uuid REFERENCES users(id),
    ADD COLUMN received_at   timestamptz,
    ADD COLUMN received_by   uuid REFERENCES users(id),
    ADD COLUMN cancelled_at  timestamptz,
    ADD COLUMN cancelled_by  uuid REFERENCES users(id);

CREATE TABLE stock_transfer_lines (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    transfer_id       uuid NOT NULL REFERENCES stock_transfers(id) ON DELETE CASCADE,
    org_ingredient_id uuid NOT NULL REFERENCES org_ingredients(id),
    -- Asked for while requested, planned while draft, sent from dispatch on.
    qty_sent          numeric(12,3) NOT NULL CHECK (qty_sent > 0),
    -- NULL until received.
    qty_received      numeric(12,3) CHECK (qty_received >= 0),
    -- Piastres per base unit, frozen at dispatch; fractional (a gram of coffee
    -- costs under a piastre) because the destination's WAC blends with it.
    -- NULL = unknown (never 0).
    unit_cost         numeric(18,6),
    note              text,
    UNIQUE (transfer_id, org_ingredient_id)
);
CREATE INDEX idx_stock_transfer_lines_ingredient ON stock_transfer_lines (org_ingredient_id);

-- Tenant isolation through the parent, as purchase_order_lines.
ALTER TABLE stock_transfer_lines ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON stock_transfer_lines FOR ALL
    USING (EXISTS (SELECT 1 FROM stock_transfers p WHERE p.id = stock_transfer_lines.transfer_id));
GRANT ALL ON TABLE stock_transfer_lines TO sufrix;
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE stock_transfer_lines TO madar_app;

-- Backfill: every existing transfer was instant, so it was dispatched and
-- received at initiated_at by whoever initiated it. Its cost is what its
-- transfer_out movement carried.
DO $$
DECLARE
    n_before  bigint;
    q_before  numeric;
    n_after   bigint;
    q_after   numeric;
BEGIN
    SELECT count(*), COALESCE(sum(quantity), 0) INTO n_before, q_before FROM stock_transfers;

    INSERT INTO stock_transfer_lines (transfer_id, org_ingredient_id, qty_sent, qty_received, unit_cost)
    SELECT t.id, t.org_ingredient_id, t.quantity, t.quantity,
           (SELECT m.unit_cost FROM inventory_movements m
             WHERE m.source_type = 'transfer' AND m.source_id = t.id AND m.type = 'transfer_out'
             ORDER BY m.created_at LIMIT 1)
    FROM stock_transfers t;

    UPDATE stock_transfers SET
        status        = 'received',
        dispatched_at = initiated_at, dispatched_by = initiated_by,
        received_at   = initiated_at, received_by   = initiated_by;

    UPDATE stock_transfers t SET number = n.rn
    FROM (SELECT id, row_number() OVER (PARTITION BY org_id ORDER BY initiated_at, id) AS rn
          FROM stock_transfers) n
    WHERE n.id = t.id;

    SELECT count(DISTINCT transfer_id), COALESCE(sum(qty_received), 0)
      INTO n_after, q_after FROM stock_transfer_lines;
    IF n_after <> n_before OR q_after <> q_before THEN
        RAISE EXCEPTION 'transfer backfill lost data: % / % rows, % / % qty', n_after, n_before, q_after, q_before;
    END IF;
END $$;

ALTER TABLE stock_transfers
    ALTER COLUMN number SET NOT NULL,
    ADD CONSTRAINT stock_transfers_org_number_key UNIQUE (org_id, number),
    DROP COLUMN org_ingredient_id,
    DROP COLUMN quantity;

CREATE INDEX idx_stock_transfers_source_status ON stock_transfers (source_branch_id, status);
CREATE INDEX idx_stock_transfers_dest_status   ON stock_transfers (destination_branch_id, status);
