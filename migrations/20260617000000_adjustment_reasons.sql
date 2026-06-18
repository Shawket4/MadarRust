-- Categorized manual-adjustment reasons (consistency with waste reasons +
-- stocktake variance reasons). Optional — a free-text note is still allowed and
-- the basic add/remove flow is unchanged; the category just makes manual stock
-- corrections analytically sliceable ("manual shrink by reason").
CREATE TYPE inventory_adjustment_reason AS ENUM (
    'count_correction',   -- fixing a data-entry / counting error
    'damage',             -- broken/unusable on site
    'theft',              -- known loss
    'expiry',             -- past shelf life (not deliberate waste disposal)
    'received_off_book',  -- stock arrived without a purchase order
    'transfer_correction',-- reconciling a mis-recorded transfer
    'other'
);

ALTER TABLE branch_inventory_adjustments
    ADD COLUMN reason inventory_adjustment_reason;
