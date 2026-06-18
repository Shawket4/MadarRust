-- Goods receipts (GRN): first-class per-delivery records, auto-created when a
-- purchase order is received. Each captures the ACTUAL received quantity + cost
-- per line (so multi-shipment partials, price variance and who/when are all
-- auditable per delivery rather than only as a running total on the PO line).
--
-- Supplier returns are modeled as a receipt with is_return = true: a negative
-- stock effect posted via a 'purchase_return' movement.

CREATE TABLE goods_receipts (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id            uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id         uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    purchase_order_id uuid REFERENCES purchase_orders(id) ON DELETE SET NULL,
    supplier_id       uuid REFERENCES suppliers(id) ON DELETE SET NULL,
    is_return         boolean NOT NULL DEFAULT false,
    reference         text,
    note              text,
    received_by       uuid NOT NULL REFERENCES users(id),
    received_at       timestamp with time zone NOT NULL DEFAULT now(),
    created_at        timestamp with time zone NOT NULL DEFAULT now()
);
CREATE INDEX idx_goods_receipts_branch ON goods_receipts (branch_id, received_at DESC);
CREATE INDEX idx_goods_receipts_po     ON goods_receipts (purchase_order_id);

CREATE TABLE goods_receipt_lines (
    id                     uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    goods_receipt_id       uuid NOT NULL REFERENCES goods_receipts(id) ON DELETE CASCADE,
    purchase_order_line_id uuid REFERENCES purchase_order_lines(id) ON DELETE SET NULL,
    org_ingredient_id      uuid NOT NULL REFERENCES org_ingredients(id) ON DELETE RESTRICT,
    -- Base STOCK units received (+) or returned (−).
    quantity               numeric(12,3) NOT NULL,
    -- Piastres per base STOCK unit (actual).
    unit_cost              bigint,
    created_at             timestamp with time zone NOT NULL DEFAULT now()
);
CREATE INDEX idx_goods_receipt_lines_receipt ON goods_receipt_lines (goods_receipt_id);

-- Stock-out movement for a return to supplier (distinct from waste/adjustment so
-- it reports separately). Not used in this transaction (added value only).
ALTER TYPE inventory_movement_type ADD VALUE IF NOT EXISTS 'purchase_return';
