-- Purchase costs keep their exact value.
--
-- A purchase line stored its cost as whole PIASTRES per purchase unit, so a
-- litre of milk bought by the gram (12 000 g at 0.04568 EGP/g = 548.16 EGP)
-- was saved as 5 piastres/g and became a 600.00 EGP order. The same rounding
-- repeated down the chain: the receipt line and the stock movement took
-- whole piastres per stock unit, and the weighted-average cost kept 2 dp.
--
-- From here the INVOICE TOTAL is the truth and the unit cost is derived from
-- it, exactly:
--   * purchase_order_lines.line_cost   — piastres for the whole line, as on
--     the supplier's invoice; unit_cost_exact = line_cost / quantity_ordered.
--   * goods_receipt_lines.line_cost    — piastres for what this delivery
--     brought (negative for a return); unit_cost_exact per base stock unit.
--   * inventory_movements.unit_cost_exact — the movement's cost per unit at
--     full precision; reports value movements with it.
--   * cost_per_unit (org, branch, history) widens from numeric(15,2) to
--     numeric(20,6) piastres, so a weighted average stops losing a piastre
--     fraction on every blend.
-- The old bigint `unit_cost` columns stay, holding the rounded value, so
-- older dashboards and readers keep working unchanged.
--
-- Additive and data-preserving; safe to run on production.

-- ── 1. Cost per unit: 6 dp of a piastre ────────────────────────────
ALTER TABLE org_ingredients         ALTER COLUMN cost_per_unit TYPE numeric(20,6);
ALTER TABLE branch_stock            ALTER COLUMN cost_per_unit TYPE numeric(20,6);
ALTER TABLE ingredient_cost_history ALTER COLUMN cost_per_unit TYPE numeric(20,6);

-- ── 2. Purchase order lines: the invoice total is the truth ───────
ALTER TABLE purchase_order_lines
    ADD COLUMN line_cost       bigint,
    ADD COLUMN unit_cost_exact numeric(24,8);

UPDATE purchase_order_lines
   SET line_cost       = round(quantity_ordered * unit_cost)::bigint,
       unit_cost_exact = unit_cost;

ALTER TABLE purchase_order_lines
    ALTER COLUMN line_cost       SET NOT NULL,
    ALTER COLUMN unit_cost_exact SET NOT NULL,
    ADD CONSTRAINT purchase_order_lines_line_cost_nonneg CHECK (line_cost >= 0);

COMMENT ON COLUMN purchase_order_lines.line_cost IS
    'Piastres for the whole line as invoiced; the source of the unit cost.';
COMMENT ON COLUMN purchase_order_lines.unit_cost_exact IS
    'Piastres per PURCHASE unit = line_cost / quantity_ordered (unit_cost is this, rounded).';

-- ── 3. Goods receipt lines: what each delivery actually cost ──────
ALTER TABLE goods_receipt_lines
    ADD COLUMN line_cost       bigint,
    ADD COLUMN unit_cost_exact numeric(24,8);

UPDATE goods_receipt_lines
   SET line_cost       = round(quantity * unit_cost)::bigint,
       unit_cost_exact = unit_cost
 WHERE unit_cost IS NOT NULL;

COMMENT ON COLUMN goods_receipt_lines.line_cost IS
    'Piastres this delivery cost (negative for a return); NULL when the cost is unknown.';
COMMENT ON COLUMN goods_receipt_lines.unit_cost_exact IS
    'Piastres per base STOCK unit at full precision (unit_cost is this, rounded).';

-- ── 3b. Any writer, any one of the three figures ──────────────────
-- The API sends the line total; a seed script or an older writer may send
-- only the rounded unit_cost. Fill the rest from what was given, so no row
-- is ever stored without its exact figures (a line total wins over a unit
-- cost, the exact unit cost over the rounded one).
CREATE FUNCTION purchase_costs_fill() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    v_qty numeric;
BEGIN
    -- Separate statements: plpgsql prepares each on first run, so the column
    -- the other table lacks is never looked up.
    IF TG_TABLE_NAME = 'purchase_order_lines' THEN
        v_qty := NEW.quantity_ordered;
    ELSE
        v_qty := NEW.quantity;
    END IF;
    IF NEW.unit_cost_exact IS NULL THEN
        NEW.unit_cost_exact := CASE
            WHEN NEW.line_cost IS NOT NULL AND v_qty <> 0 THEN round(NEW.line_cost / v_qty, 8)
            ELSE NEW.unit_cost
        END;
    END IF;
    IF NEW.line_cost IS NULL AND NEW.unit_cost_exact IS NOT NULL THEN
        NEW.line_cost := round(v_qty * NEW.unit_cost_exact)::bigint;
    END IF;
    IF NEW.unit_cost IS NULL AND NEW.unit_cost_exact IS NOT NULL THEN
        NEW.unit_cost := round(NEW.unit_cost_exact)::bigint;
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER purchase_costs_fill BEFORE INSERT ON purchase_order_lines
    FOR EACH ROW EXECUTE FUNCTION purchase_costs_fill();
CREATE TRIGGER purchase_costs_fill BEFORE INSERT ON goods_receipt_lines
    FOR EACH ROW EXECUTE FUNCTION purchase_costs_fill();

-- ── 4. Stock movements: the exact cost beside the rounded one ─────
-- Not backfilled: the ledger is append-only and large, and a reader falls
-- back to the rounded `unit_cost` where this is NULL.
ALTER TABLE inventory_movements ADD COLUMN unit_cost_exact numeric(24,8);

COMMENT ON COLUMN inventory_movements.unit_cost_exact IS
    'Piastres per unit at movement time, full precision; NULL on rows written before it existed.';
