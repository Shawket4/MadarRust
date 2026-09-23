-- A staff drink is a NORMAL sale whose pooled line is comped (owner rule,
-- 2026-09-21; docs/staff-drink-comp-contract.md).
--
-- Until now putting a line "on the pool" only wrote a `staff_drinks` side
-- record: nothing linked it to the order's price, so the line rang in full
-- unless a teller hand-applied a discount. The server now prices the comp
-- itself (`staff_pool::comp`) inside the order's own transaction.
--
-- HOW THE MONEY IS STORED — the same way a loyalty reward is
-- (`order_items.reward_covered`): the comp is TAKEN OFF the stored figures.
--   * `order_items.line_total`        = unit_price × quantity − the SIZE part of the comp
--   * `order_item_addons.line_total`  = unit_price × quantity × line qty − the part of
--                                       the comp that required-group pick absorbed
--   * `orders.subtotal` and everything computed from it (discount, service,
--     tax, total, payments) are over the CHARGED part only.
-- So every report that sums `line_total`, `subtotal` or `total_amount` — sales,
-- item sales, add-on sales, the Z/till report, POS metrics, `compute_system_cash`
-- — already counts only what was charged, with NO formula change and no
-- regenerated till vectors. `unit_price` stays the NORMAL price so a receipt
-- can print "Latte 60.00, staff drink −60.00". COST columns are untouched: the
-- drink was made, its cost counts in full.
--
-- Additive and idempotent. No new table, so RLS and grants are the tables' own
-- (`staff_drinks` already grants UPDATE to madar_app, which attaching an order
-- to an earlier record-only row needs). `order_items` and `staff_drinks`
-- already re-emit `order` / `staff_drink` on the changefeed; only the
-- projections grow (src/sync/pull/projection.rs).

ALTER TABLE order_items
    -- Total comp on the line (size part + required-group part), piastres.
    ADD COLUMN IF NOT EXISTS staff_comp_minor integer NOT NULL DEFAULT 0,
    -- The `staff_drinks` row this line is. Client-minted there, so this is a
    -- soft link by design (no FK): the two rows are written in one transaction
    -- and neither may ever block the other's retention or erasure.
    ADD COLUMN IF NOT EXISTS staff_drink_id uuid NULL;

ALTER TABLE order_item_addons
    -- The part of the line's comp this pick absorbed (whole line, not per unit).
    ADD COLUMN IF NOT EXISTS staff_comp_minor integer NOT NULL DEFAULT 0;

ALTER TABLE staff_drinks
    -- The comp as the SERVER computes it. NULL on a record-only row (POS
    -- v0.5.0–v0.7.12), which never priced anything.
    ADD COLUMN IF NOT EXISTS comp_minor integer NULL,
    -- What the line was still charged (a bigger size, extras, pricier picks).
    ADD COLUMN IF NOT EXISTS extras_minor integer NULL,
    -- What the TILL said the comp was, on a replayed sale. Kept beside the
    -- server's own figure; a difference is flagged, never refused.
    ADD COLUMN IF NOT EXISTS comp_minor_reported integer NULL;

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'order_items_staff_comp_not_negative') THEN
        ALTER TABLE order_items
            ADD CONSTRAINT order_items_staff_comp_not_negative CHECK (staff_comp_minor >= 0);
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'order_item_addons_staff_comp_not_negative') THEN
        ALTER TABLE order_item_addons
            ADD CONSTRAINT order_item_addons_staff_comp_not_negative CHECK (staff_comp_minor >= 0);
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'staff_drinks_comp_not_negative') THEN
        ALTER TABLE staff_drinks
            ADD CONSTRAINT staff_drinks_comp_not_negative CHECK (
                COALESCE(comp_minor, 0) >= 0 AND COALESCE(extras_minor, 0) >= 0
                AND COALESCE(comp_minor_reported, 0) >= 0);
    END IF;
END $$;

CREATE INDEX IF NOT EXISTS idx_order_items_staff_drink
    ON order_items (staff_drink_id) WHERE staff_drink_id IS NOT NULL;
