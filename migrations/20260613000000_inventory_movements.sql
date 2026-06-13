-- Unified stock-movement ledger.
--
-- Until now, branch_inventory.current_stock was mutated directly on every
-- sale with no audit row — only manual adjustments and transfers were
-- logged (in branch_inventory_adjustments). That makes per-sale consumption
-- invisible and blocks real variance/valuation/usage reporting.
--
-- inventory_movements is an append-only ledger of EVERY stock change (sale,
-- void restock, manual adjustment, waste, transfer, purchase receipt, stock
-- count). `quantity` is the SIGNED delta applied to current_stock
-- (consumption < 0, replenishment > 0). `balance_after` snapshots the stock
-- level after the movement, `unit_cost` the piastres/unit cost at that moment
-- (NULL = unknown, never zero — same convention as org_ingredients), and
-- `below_zero` flags a movement that drove stock negative (sales are allowed
-- to, but flagged). `source_type`/`source_id` link back to the originating
-- order / transfer / adjustment / waste / purchase order / stocktake.

CREATE TYPE inventory_movement_type AS ENUM (
    'sale',
    'void_restock',
    'adjustment_add',
    'adjustment_remove',
    'waste',
    'transfer_out',
    'transfer_in',
    'purchase_in',
    'stock_count'
);

CREATE TABLE inventory_movements (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    branch_id           uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    org_ingredient_id   uuid NOT NULL REFERENCES org_ingredients(id) ON DELETE RESTRICT,
    branch_inventory_id uuid REFERENCES branch_inventory(id) ON DELETE SET NULL,
    type                inventory_movement_type NOT NULL,
    quantity            numeric(12,3) NOT NULL,
    balance_after       numeric(12,3),
    unit_cost           bigint,
    reason              text,
    below_zero          boolean NOT NULL DEFAULT false,
    source_type         text,
    source_id           uuid,
    note                text,
    created_by          uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at          timestamp with time zone NOT NULL DEFAULT now()
);

CREATE INDEX idx_inventory_movements_branch_time ON inventory_movements (branch_id, created_at DESC);
CREATE INDEX idx_inventory_movements_ingredient  ON inventory_movements (org_ingredient_id);
CREATE INDEX idx_inventory_movements_source      ON inventory_movements (source_type, source_id);
