-- Foodics-grade inventory polish: stocktake variance guardrail + gap closers.
-- Every change here is ADDITIVE and data-preserving on the existing schema
-- (new enum, new nullable columns, new enum value, one index). Safe to run on
-- production once the 20260613* set ahead of it has been applied.

-- ── Stocktake variance guardrail ──────────────────────────────────────
-- A categorised reason a manager attaches to a "suspicious" difference found
-- during a stock count. Distinct from waste reasons: shrinkage is unexplained
-- loss, not deliberate disposal.
CREATE TYPE stocktake_variance_reason AS ENUM (
    'theft', 'spoilage', 'breakage', 'miscount', 'supplier_short', 'transfer_error', 'other'
);

ALTER TABLE stocktake_items
    ADD COLUMN variance_reason stocktake_variance_reason;

-- Per-org tolerance. A counted row whose |difference| is >= this percent of the
-- expected quantity (or that appears-from / vanishes-to zero) is flagged and
-- needs a reason before the count can be finalized. Default 10%.
ALTER TABLE organizations
    ADD COLUMN stocktake_variance_threshold_pct numeric(6,3) NOT NULL DEFAULT 10;

-- ── G1: partial multi-shipment receiving ──────────────────────────────
-- New status between 'ordered' and 'received' so a PO can be received across
-- several deliveries. (PG 12+; added value is not used in this transaction.)
ALTER TYPE purchase_order_status ADD VALUE IF NOT EXISTS 'partially_received';

-- ── G2: ingredient -> default supplier link ───────────────────────────
-- Lets the reorder view pre-fill / group a "create PO" by supplier.
ALTER TABLE org_ingredients
    ADD COLUMN supplier_id uuid REFERENCES suppliers(id) ON DELETE SET NULL;

CREATE INDEX idx_org_ingredients_supplier ON org_ingredients (supplier_id);
