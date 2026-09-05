-- ═══════════════════════════════════════════════════════════════════════════
-- Inventory v2 — count-first, ledger-derived stock.
--
-- Why: a `branch_inventory` row was a hidden prerequisite for everything
-- (counting, low stock, valuation, waste, transfers, even sale deductions),
-- stock could be rewritten directly with no ledger entry, and two competing
-- reorder fields (reorder_threshold vs par_min) made reports disagree with the
-- UI. Categories were free text.
--
-- After this migration:
--   • ingredient_categories is a real org-scoped table; org_ingredients keeps a
--     NOT NULL category_id (the free-text column is gone).
--   • branch_stock (was branch_inventory) holds one lazily-created row per
--     (branch, ingredient): on_hand, actual cost, par levels, last activity.
--     A row is NEVER a precondition — every read is org_ingredients LEFT JOIN.
--   • inventory_movements is the ONLY way on_hand changes: a BEFORE INSERT
--     trigger upserts branch_stock and fills balance_after/below_zero, and a
--     guard trigger rejects any other write to on_hand.
--   • stocktake_items measure variance against book_qty (live stock at
--     finalize), with opening_qty kept as the open-time reference.
--   • stock_transfers (was branch_inventory_transfers).
--
-- Data: fully preserving. Every existing row is carried across (verified by
-- the assertions at the end). Rehearsed on a restored prod copy.
-- ═══════════════════════════════════════════════════════════════════════════

-- Snapshot invariants we assert at the end.
CREATE TEMP TABLE _inv_v2_before AS
SELECT (SELECT count(*)                          FROM branch_inventory)           AS stock_rows,
       (SELECT COALESCE(sum(current_stock), 0)   FROM branch_inventory)           AS stock_sum,
       (SELECT count(*)                          FROM org_ingredients)            AS ingredient_rows,
       (SELECT count(*)                          FROM inventory_movements)        AS movement_rows,
       (SELECT count(*)                          FROM stocktake_items)            AS stocktake_item_rows,
       (SELECT count(*)                          FROM branch_inventory_transfers) AS transfer_rows;

-- ── 1. Ingredient categories ──────────────────────────────────────────────

CREATE FUNCTION _inv_v2_slugify(src text) RETURNS text
LANGUAGE sql IMMUTABLE AS $$
    SELECT COALESCE(
        NULLIF(trim(both '_' from regexp_replace(lower(trim(src)), '[^a-z0-9]+', '_', 'g')), ''),
        'general')
$$;

CREATE TABLE ingredient_categories (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id     uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- Stable machine key: `milk` and `coffee_bean` carry swap semantics in the
    -- menu (a milk/coffee swap add-on replaces the base recipe line of the
    -- matching category), so the slug is what code keys on, never the name.
    slug       text NOT NULL CHECK (slug ~ '^[a-z0-9_]{1,64}$'),
    name       text NOT NULL CHECK (length(trim(name)) BETWEEN 1 AND 80),
    sort_order integer NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (org_id, slug)
);
CREATE INDEX idx_ingredient_categories_org ON ingredient_categories (org_id, sort_order, name);
CREATE TRIGGER trg_ingredient_categories_updated_at
    BEFORE UPDATE ON ingredient_categories FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Every org gets `general` (sort 0); every other category in use follows.
INSERT INTO ingredient_categories (org_id, slug, name, sort_order)
SELECT id, 'general', 'General', 0 FROM organizations;

INSERT INTO ingredient_categories (org_id, slug, name, sort_order)
SELECT DISTINCT ON (oi.org_id, _inv_v2_slugify(oi.category))
       oi.org_id,
       _inv_v2_slugify(oi.category),
       initcap(replace(_inv_v2_slugify(oi.category), '_', ' ')),
       10
FROM org_ingredients oi
WHERE _inv_v2_slugify(oi.category) <> 'general'
ORDER BY oi.org_id, _inv_v2_slugify(oi.category)
ON CONFLICT (org_id, slug) DO NOTHING;

ALTER TABLE org_ingredients
    ADD COLUMN category_id uuid REFERENCES ingredient_categories(id) ON DELETE RESTRICT;

UPDATE org_ingredients oi
SET category_id = ic.id
FROM ingredient_categories ic
WHERE ic.org_id = oi.org_id AND ic.slug = _inv_v2_slugify(oi.category);

ALTER TABLE org_ingredients ALTER COLUMN category_id SET NOT NULL;
ALTER TABLE org_ingredients DROP COLUMN category;
CREATE INDEX idx_org_ingredients_category ON org_ingredients (category_id);

DROP FUNCTION _inv_v2_slugify(text);

-- Every organization has a `general` category from birth (the seed above
-- covers existing orgs; this covers every org created afterwards), and any
-- code path that needs a category by slug can get-or-create it atomically.
CREATE FUNCTION ingredient_category_id(p_org_id uuid, p_slug text) RETURNS uuid
LANGUAGE plpgsql AS $$
DECLARE v_id uuid;
BEGIN
    INSERT INTO ingredient_categories (org_id, slug, name, sort_order)
    VALUES (p_org_id, p_slug,
            CASE WHEN p_slug = 'general' THEN 'General'
                 ELSE initcap(replace(p_slug, '_', ' ')) END,
            CASE WHEN p_slug = 'general' THEN 0 ELSE 10 END)
    ON CONFLICT (org_id, slug) DO UPDATE SET slug = EXCLUDED.slug
    RETURNING id INTO v_id;
    RETURN v_id;
END $$;

CREATE FUNCTION organizations_seed_general_category() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM ingredient_category_id(NEW.id, 'general');
    RETURN NEW;
END $$;

CREATE TRIGGER trg_organizations_seed_general_category
    AFTER INSERT ON organizations
    FOR EACH ROW EXECUTE FUNCTION organizations_seed_general_category();

-- Every ingredient has a category: an insert that names none lands in the
-- org's `general` (keeps operator scripts and seeds simple; the API always
-- sends one explicitly).
CREATE FUNCTION org_ingredients_default_category() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.category_id IS NULL THEN
        NEW.category_id := ingredient_category_id(NEW.org_id, 'general');
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER trg_org_ingredients_default_category
    BEFORE INSERT ON org_ingredients
    FOR EACH ROW EXECUTE FUNCTION org_ingredients_default_category();

-- RLS (org-rooted) — the generator in 20260708000100 has already run, so a
-- table added afterwards classifies itself here.
ALTER TABLE ingredient_categories ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON ingredient_categories FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT ALL ON TABLE ingredient_categories TO sufrix;

-- ── 2. branch_stock (was branch_inventory) ────────────────────────────────

ALTER TABLE branch_inventory RENAME TO branch_stock;
ALTER TABLE branch_stock RENAME COLUMN current_stock TO on_hand;

ALTER INDEX branch_inventory_pkey RENAME TO branch_stock_pkey;
ALTER TABLE branch_stock RENAME CONSTRAINT branch_inventory_branch_id_org_ingredient_id_key
    TO branch_stock_branch_id_org_ingredient_id_key;
ALTER TABLE branch_stock RENAME CONSTRAINT branch_inventory_branch_id_fkey TO branch_stock_branch_id_fkey;
ALTER TABLE branch_stock RENAME CONSTRAINT branch_inventory_org_ingredient_id_fkey TO branch_stock_org_ingredient_id_fkey;
ALTER INDEX idx_branch_inventory_branch     RENAME TO idx_branch_stock_branch;
ALTER INDEX idx_branch_inventory_ingredient RENAME TO idx_branch_stock_ingredient;
ALTER TRIGGER trg_branch_inventory_updated_at ON branch_stock RENAME TO trg_branch_stock_updated_at;

-- One reorder point: the legacy threshold folds into par_min where no par was
-- ever set (0 meant "unset" in the old model).
UPDATE branch_stock SET par_min = reorder_threshold
WHERE par_min IS NULL AND reorder_threshold > 0;
ALTER TABLE branch_stock DROP COLUMN reorder_threshold;

ALTER TABLE branch_stock
    ADD COLUMN last_counted_at  timestamptz,
    ADD COLUMN last_movement_at timestamptz,
    ADD CONSTRAINT chk_branch_stock_par_nonneg CHECK (
        (par_min IS NULL OR par_min >= 0) AND (par_max IS NULL OR par_max >= 0)),
    ADD CONSTRAINT chk_branch_stock_par_order CHECK (
        par_min IS NULL OR par_max IS NULL OR par_max >= par_min);

UPDATE branch_stock bs
SET last_counted_at = x.last_at
FROM (
    SELECT s.branch_id, si.org_ingredient_id, max(s.finalized_at) AS last_at
    FROM stocktakes s
    JOIN stocktake_items si ON si.stocktake_id = s.id
    WHERE s.status = 'finalized' AND si.counted_qty IS NOT NULL
    GROUP BY s.branch_id, si.org_ingredient_id
) x
WHERE x.branch_id = bs.branch_id AND x.org_ingredient_id = bs.org_ingredient_id;

UPDATE branch_stock bs
SET last_movement_at = x.last_at
FROM (
    SELECT branch_id, org_ingredient_id, max(created_at) AS last_at
    FROM inventory_movements
    GROUP BY branch_id, org_ingredient_id
) x
WHERE x.branch_id = bs.branch_id AND x.org_ingredient_id = bs.org_ingredient_id;

-- ── 3. inventory_movements: the single writer of on_hand ──────────────────

ALTER TABLE inventory_movements RENAME COLUMN branch_inventory_id TO branch_stock_id;
ALTER TABLE inventory_movements RENAME CONSTRAINT inventory_movements_branch_inventory_id_fkey
    TO inventory_movements_branch_stock_id_fkey;

-- Historical rows: re-link to their stock row and fill any missing balance by a
-- reverse running sum from today's on_hand (later movements subtracted).
UPDATE inventory_movements m
SET branch_stock_id = bs.id
FROM branch_stock bs
WHERE m.branch_stock_id IS NULL
  AND bs.branch_id = m.branch_id AND bs.org_ingredient_id = m.org_ingredient_id;

WITH ordered AS (
    SELECT m.id, m.branch_id, m.org_ingredient_id,
           COALESCE(SUM(m.quantity) OVER (
               PARTITION BY m.branch_id, m.org_ingredient_id
               ORDER BY m.created_at DESC, m.id DESC
               ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING), 0) AS later_sum
    FROM inventory_movements m
)
UPDATE inventory_movements m
SET balance_after = COALESCE(bs.on_hand, 0) - o.later_sum
FROM ordered o
LEFT JOIN branch_stock bs
       ON bs.branch_id = o.branch_id AND bs.org_ingredient_id = o.org_ingredient_id
WHERE m.id = o.id AND m.balance_after IS NULL;

ALTER TABLE inventory_movements ALTER COLUMN balance_after SET NOT NULL;
CREATE INDEX idx_inventory_movements_stock_time
    ON inventory_movements (branch_id, org_ingredient_id, created_at DESC);

-- Posting a movement upserts the balance row (creating it on first activity),
-- and stamps the resulting balance on the movement. The upsert takes the row
-- lock, so concurrent movements on one ingredient serialise correctly.
CREATE FUNCTION inventory_movements_apply() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    v_stock_id uuid;
    v_balance  numeric(12,3);
BEGIN
    INSERT INTO branch_stock (branch_id, org_ingredient_id, on_hand, last_movement_at)
    VALUES (NEW.branch_id, NEW.org_ingredient_id, NEW.quantity, NEW.created_at)
    ON CONFLICT (branch_id, org_ingredient_id) DO UPDATE
        SET on_hand          = branch_stock.on_hand + EXCLUDED.on_hand,
            last_movement_at = GREATEST(branch_stock.last_movement_at, EXCLUDED.last_movement_at)
    RETURNING id, on_hand INTO v_stock_id, v_balance;

    NEW.branch_stock_id := v_stock_id;
    NEW.balance_after   := v_balance;
    NEW.below_zero      := v_balance < 0;
    RETURN NEW;
END $$;

CREATE TRIGGER trg_inventory_movements_apply
    BEFORE INSERT ON inventory_movements
    FOR EACH ROW EXECUTE FUNCTION inventory_movements_apply();

-- Ledger is truth: on_hand only moves from inside the movement trigger (depth 2
-- when this guard fires from that path, 1 for any direct UPDATE). The one
-- sanctioned exception is a unit re-denomination (g→kg), which rescales the
-- ledger and the balance together under `SET LOCAL madar.stock_rebase = 'on'`.
CREATE FUNCTION branch_stock_on_hand_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.on_hand IS DISTINCT FROM OLD.on_hand
       AND pg_trigger_depth() < 2
       AND COALESCE(current_setting('madar.stock_rebase', true), '') <> 'on'
    THEN
        RAISE EXCEPTION 'branch_stock.on_hand is derived from inventory_movements — post a movement instead'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER trg_branch_stock_on_hand_guard
    BEFORE UPDATE OF on_hand ON branch_stock
    FOR EACH ROW EXECUTE FUNCTION branch_stock_on_hand_guard();

-- ── 4. Stocktakes: variance against book stock, scope recorded ────────────

ALTER TABLE stocktakes
    ADD COLUMN scope jsonb NOT NULL DEFAULT '{"kind":"full"}'::jsonb;

ALTER TABLE stocktake_items RENAME COLUMN expected_qty        TO opening_qty;
ALTER TABLE stocktake_items RENAME COLUMN system_qty          TO book_qty;
ALTER TABLE stocktake_items RENAME COLUMN branch_inventory_id TO branch_stock_id;
ALTER TABLE stocktake_items RENAME CONSTRAINT stocktake_items_branch_inventory_id_fkey
    TO stocktake_items_branch_stock_id_fkey;
ALTER TABLE stocktake_items DROP COLUMN variance;
ALTER TABLE stocktake_items
    ADD COLUMN variance numeric(12,3)
        GENERATED ALWAYS AS (counted_qty - COALESCE(book_qty, opening_qty)) STORED;

-- ── 5. stock_transfers (was branch_inventory_transfers) ───────────────────

ALTER TABLE branch_inventory_transfers RENAME TO stock_transfers;
ALTER INDEX branch_inventory_transfers_pkey RENAME TO stock_transfers_pkey;
ALTER INDEX idx_bit_dest       RENAME TO idx_stock_transfers_destination;
ALTER INDEX idx_bit_ingredient RENAME TO idx_stock_transfers_ingredient;
ALTER INDEX idx_bit_source     RENAME TO idx_stock_transfers_source;
ALTER TABLE stock_transfers RENAME CONSTRAINT branch_inventory_transfers_destination_branch_id_fkey TO stock_transfers_destination_branch_id_fkey;
ALTER TABLE stock_transfers RENAME CONSTRAINT branch_inventory_transfers_initiated_by_fkey          TO stock_transfers_initiated_by_fkey;
ALTER TABLE stock_transfers RENAME CONSTRAINT branch_inventory_transfers_org_id_fkey                TO stock_transfers_org_id_fkey;
ALTER TABLE stock_transfers RENAME CONSTRAINT branch_inventory_transfers_org_ingredient_id_fkey     TO stock_transfers_org_ingredient_id_fkey;
ALTER TABLE stock_transfers RENAME CONSTRAINT branch_inventory_transfers_source_branch_id_fkey      TO stock_transfers_source_branch_id_fkey;

-- ── 6. Assertions: nothing lost, invariants hold ──────────────────────────

DO $$
DECLARE b RECORD;
BEGIN
    SELECT * INTO b FROM _inv_v2_before;

    IF (SELECT count(*) FROM branch_stock) <> b.stock_rows THEN
        RAISE EXCEPTION 'inventory_v2: branch_stock row count changed';
    END IF;
    IF (SELECT COALESCE(sum(on_hand), 0) FROM branch_stock) <> b.stock_sum THEN
        RAISE EXCEPTION 'inventory_v2: on_hand total changed';
    END IF;
    IF (SELECT count(*) FROM org_ingredients) <> b.ingredient_rows THEN
        RAISE EXCEPTION 'inventory_v2: org_ingredients row count changed';
    END IF;
    IF (SELECT count(*) FROM inventory_movements) <> b.movement_rows THEN
        RAISE EXCEPTION 'inventory_v2: inventory_movements row count changed';
    END IF;
    IF (SELECT count(*) FROM stocktake_items) <> b.stocktake_item_rows THEN
        RAISE EXCEPTION 'inventory_v2: stocktake_items row count changed';
    END IF;
    IF (SELECT count(*) FROM stock_transfers) <> b.transfer_rows THEN
        RAISE EXCEPTION 'inventory_v2: stock_transfers row count changed';
    END IF;
    IF EXISTS (SELECT 1 FROM organizations o
               WHERE NOT EXISTS (SELECT 1 FROM ingredient_categories c
                                 WHERE c.org_id = o.id AND c.slug = 'general')) THEN
        RAISE EXCEPTION 'inventory_v2: an organization has no general category';
    END IF;
END $$;

DROP TABLE _inv_v2_before;
