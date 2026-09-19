-- ════════════════════════════════════════════════════════════════════════════
-- Price lives in SIZES. `menu_items.base_price` stops being a source of truth.
-- ════════════════════════════════════════════════════════════════════════════
--
-- The model:
--   1. Every non-deleted menu item ALWAYS has at least one size row in
--      `menu_item_sizes`. An item created with a single price gets a synthetic
--      `one_size` row carrying it (id = md5(item_id || ':one_size'), the same
--      stable id the unification backfill used, so it stays hidden from the
--      legacy `item_sizes` view).
--   2. The price a single number must show — POS grid, dashboard list,
--      storefront, preview — is the LOWEST active size price. There is no
--      default-size marker: lowest wins.
--   3. `menu_items.base_price` is kept as a MIRROR of that lowest price by a
--      trigger. It stays NOT NULL and keeps its shape purely so clients at or
--      below v0.7.11 — which read an item price from the API and have no
--      concept of a size-less item — charge the right thing instead of a stale
--      field. Nothing may treat it as an independently editable number again.
--
-- ── The one-off reconciliation (owner's ruling) ──
-- Production diverged: the owner edited SIZES in the dashboard while the till
-- kept charging the item's stale `base_price`. The ruling is to take the HIGHER
-- of the two for every diverging item, in every org. One rule produces both
-- observed outcomes:
--   • Drops' one-size items, where the size price was edited upward and the
--     item price went stale → the item rises to the size price.
--   • Rue's one-size items, whose size rows are untouched leftovers from the
--     unification backfill → the charged item price is kept and the stale size
--     row is refreshed to match, so the dashboard stops showing a number
--     nobody chose.
-- The rule is applied ONLY to items with exactly one active size; a multi-size
-- item has nothing to reconcile, because the lowest-price display rule replaces
-- the old base_price display and no size price moves.
--
-- GREATEST() cannot lower a one-size item's charged price; the migration
-- asserts that anyway. A multi-size item whose stale base_price sat ABOVE its
-- cheapest size sees its DISPLAYED number drop to that cheapest size — no
-- charged price moves, because a multi-size item is always ordered with a size.
-- Those are reported by NOTICE rather than blocking the deploy.
--
-- ── Schema drift ──
-- `item_sizes` is a VIEW on production (hand-applied by
-- deploy/menu_unification_shim.sql at the Wave-2 source-of-truth flip) but is
-- still a real TABLE on every test and fresh database, so the test suite could
-- not reproduce production. This migration makes the two agree: where it is
-- still a table, its rows are folded into `menu_item_sizes` and it is replaced
-- by the identical compatibility view. No real `item_sizes` table is recreated.
-- The view is old-client compatibility ONLY; the resolver reads
-- `menu_item_sizes` directly.
--
-- Money stays integer piastres.
-- ════════════════════════════════════════════════════════════════════════════

-- ── 1. Converge item_sizes to the production shape (view over menu_item_sizes) ──
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_class WHERE relname = 'item_sizes' AND relkind = 'r') THEN
        -- Fold any rows that only exist in the legacy table into the new one.
        -- Ids are the stable-id invariant, so a row present in both matches by id.
        INSERT INTO menu_item_sizes (id, menu_item_id, label, price, is_active)
        SELECT z.id, z.menu_item_id, z.label, z.price_override, z.is_active
        FROM item_sizes z
        JOIN menu_items m ON m.id = z.menu_item_id
        ON CONFLICT (id) DO NOTHING;

        -- Same (item, label) reached under a different id: keep the new table's row.
        INSERT INTO menu_item_sizes (menu_item_id, label, price, is_active)
        SELECT z.menu_item_id, z.label, z.price_override, z.is_active
        FROM item_sizes z
        JOIN menu_items m ON m.id = z.menu_item_id
        ON CONFLICT (menu_item_id, label) DO NOTHING;

        DROP TABLE item_sizes CASCADE;
    END IF;

    IF NOT EXISTS (SELECT 1 FROM pg_class WHERE relname = 'item_sizes') THEN
        -- Identical to deploy/menu_unification_shim.sql: the synthetic one_size
        -- sentinel rows stay hidden, so a legacy client sees a size-less item
        -- exactly as it did before.
        CREATE VIEW item_sizes AS
        SELECT z.id, z.menu_item_id, z.label, z.price AS price_override, z.is_active
        FROM menu_item_sizes z
        WHERE NOT (z.label = 'one_size'
                   AND z.id = (md5(z.menu_item_id::text || ':one_size'))::uuid);

        GRANT SELECT ON item_sizes TO sufrix;
    END IF;
END $$;

-- ── 2. Every non-deleted item gets at least one size ──
-- An item with no size row at all takes a synthetic one_size row at its
-- base_price, which is exactly what it was charging.
INSERT INTO menu_item_sizes (id, menu_item_id, label, price, sort, is_active)
SELECT (md5(m.id::text || ':one_size'))::uuid, m.id, 'one_size', m.base_price, 0, true
FROM menu_items m
WHERE m.deleted_at IS NULL
  AND NOT EXISTS (SELECT 1 FROM menu_item_sizes z WHERE z.menu_item_id = m.id)
ON CONFLICT (menu_item_id, label) DO NOTHING;

-- ── 3. The reconciliation: HIGHER of the two, for single-active-size items ──
DO $$
DECLARE
    r          record;
    n_raised   int := 0;
    n_display  int := 0;
BEGIN
    -- Report every single-size item whose charged price moves.
    FOR r IN
        SELECT m.id, o.name AS org, m.name, m.base_price AS old_price, z.price AS old_size,
               GREATEST(m.base_price, z.price) AS new_price
        FROM menu_items m
        JOIN organizations o ON o.id = m.org_id
        JOIN menu_item_sizes z ON z.menu_item_id = m.id AND z.is_active
        WHERE m.deleted_at IS NULL
          AND m.base_price <> z.price
          AND (SELECT count(*) FROM menu_item_sizes s
                WHERE s.menu_item_id = m.id AND s.is_active) = 1
        ORDER BY o.name, m.name
    LOOP
        RAISE NOTICE 'price-reconcile [%] % : item % / size % -> %',
            r.org, r.name, r.old_price, r.old_size, r.new_price;
        n_raised := n_raised + 1;
    END LOOP;
    RAISE NOTICE 'price-reconcile: % single-size item(s) reconciled', n_raised;

    -- Apply it: the single active size takes the higher of the two.
    UPDATE menu_item_sizes z
       SET price = GREATEST(z.price, m.base_price)
      FROM menu_items m
     WHERE m.id = z.menu_item_id
       AND m.deleted_at IS NULL
       AND z.is_active
       AND z.price < m.base_price
       AND (SELECT count(*) FROM menu_item_sizes s
             WHERE s.menu_item_id = m.id AND s.is_active) = 1;

    -- Multi-size items whose stale base_price sat above their cheapest size:
    -- the DISPLAYED number drops to the cheapest size. No charged price moves,
    -- because a multi-size item is always ordered with an explicit size.
    FOR r IN
        SELECT o.name AS org, m.name, m.base_price AS old_price,
               min(z.price) AS new_price
        FROM menu_items m
        JOIN organizations o ON o.id = m.org_id
        JOIN menu_item_sizes z ON z.menu_item_id = m.id AND z.is_active
        WHERE m.deleted_at IS NULL
        GROUP BY o.name, m.id, m.name, m.base_price
        HAVING count(*) > 1 AND min(z.price) < m.base_price
        ORDER BY o.name, m.name
    LOOP
        RAISE NOTICE 'price-display-drop [%] % : displayed % -> % (multi-size; charged price unchanged)',
            r.org, r.name, r.old_price, r.new_price;
        n_display := n_display + 1;
    END LOOP;
    RAISE NOTICE 'price-reconcile: % multi-size display-only reduction(s)', n_display;
END $$;

-- ── 4. base_price becomes a mirror of the lowest active size price ──
CREATE OR REPLACE FUNCTION menu_item_mirror_lowest_price(p_item uuid) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF p_item IS NULL THEN
        RETURN;
    END IF;
    UPDATE menu_items m
       SET base_price = sub.lowest
      FROM (SELECT min(price) AS lowest FROM menu_item_sizes
             WHERE menu_item_id = p_item AND is_active) sub
     WHERE m.id = p_item
       AND sub.lowest IS NOT NULL
       AND m.base_price IS DISTINCT FROM sub.lowest;
END $$;

CREATE OR REPLACE FUNCTION menu_item_sizes_mirror_price() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM menu_item_mirror_lowest_price(OLD.menu_item_id);
        RETURN OLD;
    END IF;
    PERFORM menu_item_mirror_lowest_price(NEW.menu_item_id);
    IF TG_OP = 'UPDATE' AND NEW.menu_item_id IS DISTINCT FROM OLD.menu_item_id THEN
        PERFORM menu_item_mirror_lowest_price(OLD.menu_item_id);
    END IF;
    RETURN NEW;
END $$;

DROP TRIGGER IF EXISTS menu_item_sizes_mirror_price ON menu_item_sizes;
CREATE TRIGGER menu_item_sizes_mirror_price
AFTER INSERT OR UPDATE OR DELETE ON menu_item_sizes
FOR EACH ROW EXECUTE FUNCTION menu_item_sizes_mirror_price();

-- Bring every existing item's mirror in line (post-reconciliation).
UPDATE menu_items m
   SET base_price = sub.lowest
  FROM (SELECT menu_item_id, min(price) AS lowest
          FROM menu_item_sizes WHERE is_active GROUP BY menu_item_id) sub
 WHERE m.id = sub.menu_item_id
   AND m.base_price IS DISTINCT FROM sub.lowest;

-- ── 5. "An item with no sizes at all" is made impossible ──
-- (a) A newly inserted item materialises its one_size row from the price it was
--     created with, so there is no window in which it has none.
CREATE OR REPLACE FUNCTION menu_items_ensure_one_size() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF NEW.deleted_at IS NULL
       AND NOT EXISTS (SELECT 1 FROM menu_item_sizes z WHERE z.menu_item_id = NEW.id)
    THEN
        INSERT INTO menu_item_sizes (id, menu_item_id, label, price, sort, is_active)
        VALUES ((md5(NEW.id::text || ':one_size'))::uuid, NEW.id, 'one_size',
                NEW.base_price, 0, true)
        ON CONFLICT (menu_item_id, label) DO NOTHING;
    END IF;
    RETURN NULL;
END $$;

DROP TRIGGER IF EXISTS menu_items_ensure_one_size ON menu_items;
CREATE TRIGGER menu_items_ensure_one_size
AFTER INSERT ON menu_items
FOR EACH ROW EXECUTE FUNCTION menu_items_ensure_one_size();

-- (b) A live item cannot END a transaction with no active size.
--     This is a DEFERRED constraint trigger, checked at COMMIT rather than per
--     statement, so the ordinary editor pattern of replacing an item's whole
--     size set (delete them all, insert the new ones, one transaction) still
--     works — what it forbids is COMMITTING an item with none left.
--     Deleting the item itself still works: menu_item_sizes cascades from
--     menu_items, and a soft delete sets deleted_at, which this skips.
CREATE OR REPLACE FUNCTION menu_item_sizes_keep_one() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    item uuid := COALESCE(NEW.menu_item_id, OLD.menu_item_id);
BEGIN
    IF NOT EXISTS (SELECT 1 FROM menu_items m
                    WHERE m.id = item AND m.deleted_at IS NULL) THEN
        RETURN NULL;   -- item gone or soft-deleted: nothing to protect
    END IF;

    IF NOT EXISTS (SELECT 1 FROM menu_item_sizes z
                    WHERE z.menu_item_id = item AND z.is_active) THEN
        RAISE EXCEPTION
            'a menu item must always have at least one active size (item %)', item
            USING ERRCODE = '23514';
    END IF;

    RETURN NULL;
END $$;

DROP TRIGGER IF EXISTS menu_item_sizes_keep_one ON menu_item_sizes;
CREATE CONSTRAINT TRIGGER menu_item_sizes_keep_one
AFTER DELETE OR UPDATE OF is_active ON menu_item_sizes
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION menu_item_sizes_keep_one();

-- ── 6. Assertions ──
DO $$
DECLARE
    n int;
BEGIN
    SELECT count(*) INTO n
      FROM menu_items m
     WHERE m.deleted_at IS NULL
       AND NOT EXISTS (SELECT 1 FROM menu_item_sizes z
                        WHERE z.menu_item_id = m.id AND z.is_active);
    IF n > 0 THEN
        RAISE EXCEPTION 'price-reconcile: % live menu item(s) still have no active size', n;
    END IF;

    -- No single-size item's charged price fell.
    SELECT count(*) INTO n
      FROM menu_items m
      JOIN menu_item_sizes z ON z.menu_item_id = m.id AND z.is_active
     WHERE m.deleted_at IS NULL
       AND (SELECT count(*) FROM menu_item_sizes s
             WHERE s.menu_item_id = m.id AND s.is_active) = 1
       AND z.price <> m.base_price;
    IF n > 0 THEN
        RAISE EXCEPTION 'price-reconcile: % single-size item(s) out of step with their mirror', n;
    END IF;
END $$;
