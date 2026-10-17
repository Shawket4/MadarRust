-- OPT-B (capacity, owner-approved 2026-10-17): the changefeed triggers of the
-- tables a sale writes MANY rows into fire once per statement, not per row.
--
-- order_items, order_payments, order_deals (-> the order), kitchen_ticket_items
-- (-> its kitchen ticket) and open_ticket_items (-> its open ticket) each had
-- an `AFTER INSERT OR UPDATE OR DELETE … FOR EACH ROW` trigger that re-emitted
-- the PARENT: per row, one parent lookup, a shared advisory lock, an upsert of
-- the parent's one `sync_changes` row (bumping its seq) and a NOTIFY. A sale
-- with five lines bumped the same order row five times in one statement.
-- The order path now writes those rows in one multi-row INSERT, and these
-- triggers touch each parent ONCE per statement, from the transition tables.
--
-- The changefeed does not change: `sync_changes` holds ONE row per entity
-- (PK branch_id, type, entity_id), so what a till pulls is that row and its
-- position in seq order. Per statement:
--   * the set of parents touched is the same: INSERT -> the new rows'
--     parents; DELETE -> the old rows'; UPDATE -> the old rows' parents plus
--     the new rows' (the row trigger touched OLD's, and NEW's when it moved);
--   * they are touched at the same moment, the end of the statement (where
--     the per-row AFTER triggers ran too), so the parent row reads the same
--     state, and its last bump lands after the same earlier writes.
-- Only the seq NUMBERS differ (fewer nextval() calls); a till only ever
-- compares them. A statement touching several parents (none on the hot paths)
-- bumps them in an unspecified order instead of last-row order; they commit
-- together, so a pull sees all or none of them either way.
--
-- The trigger keeps its name `sync_emit` (the INSERT one; the UPDATE and
-- DELETE ones are `sync_emit_upd` / `sync_emit_del`, since a trigger with
-- transition tables may name only one event) and its function
-- `sync_emit_<table>`, so `every_projection_source_table_has_emitter` holds.
--
-- open_ticket_items' `open_ticket_item_children` (Phase 5.1: mirrors a line's
-- add-ons / optional fields into their tables) splits the same way: INSERT is
-- now one set-based statement over the new rows (a new line has no children to
-- delete first); UPDATE OF line keeps the row trigger (a column-list trigger
-- cannot take transition tables) and is unchanged.
--
-- Also: orders had TWO identical `BEFORE UPDATE … set_updated_at()` triggers
-- (`orders_set_updated_at`, `trg_orders_updated_at`), both setting
-- updated_at = now(). The second is dropped: the same value, one call less on
-- every order UPDATE.

-- ── parent touches, once per statement ───────────────────────────────────────
DO $optb$
DECLARE
    r record;
BEGIN
    FOR r IN SELECT * FROM (VALUES
        ('order_items',          'order_id',          'sync_touch_order'),
        ('order_payments',       'order_id',          'sync_touch_order'),
        ('order_deals',          'order_id',          'sync_touch_order'),
        ('kitchen_ticket_items', 'kitchen_ticket_id', 'sync_touch_kitchen_ticket'),
        ('open_ticket_items',    'open_ticket_id',    'sync_touch_open_ticket')
    ) AS x(t, col, touch)
    LOOP
        EXECUTE format($f$
            CREATE OR REPLACE FUNCTION %1$I() RETURNS trigger
            LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $body$
            DECLARE
                p uuid;
            BEGIN
                IF TG_OP = 'INSERT' THEN
                    FOR p IN SELECT DISTINCT n.%2$I FROM new_rows n WHERE n.%2$I IS NOT NULL LOOP
                        PERFORM %3$I(p);
                    END LOOP;
                ELSIF TG_OP = 'DELETE' THEN
                    FOR p IN SELECT DISTINCT o.%2$I FROM old_rows o WHERE o.%2$I IS NOT NULL LOOP
                        PERFORM %3$I(p);
                    END LOOP;
                ELSE
                    FOR p IN SELECT o.%2$I FROM old_rows o WHERE o.%2$I IS NOT NULL
                             UNION
                             SELECT n.%2$I FROM new_rows n WHERE n.%2$I IS NOT NULL LOOP
                        PERFORM %3$I(p);
                    END LOOP;
                END IF;
                RETURN NULL;
            END
            $body$
        $f$, 'sync_emit_' || r.t, r.col, r.touch);

        EXECUTE format('DROP TRIGGER IF EXISTS sync_emit ON %I', r.t);
        EXECUTE format('DROP TRIGGER IF EXISTS sync_emit_upd ON %I', r.t);
        EXECUTE format('DROP TRIGGER IF EXISTS sync_emit_del ON %I', r.t);
        EXECUTE format('CREATE TRIGGER sync_emit AFTER INSERT ON %I '
                       'REFERENCING NEW TABLE AS new_rows '
                       'FOR EACH STATEMENT EXECUTE FUNCTION %I()', r.t, 'sync_emit_' || r.t);
        EXECUTE format('CREATE TRIGGER sync_emit_upd AFTER UPDATE ON %I '
                       'REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows '
                       'FOR EACH STATEMENT EXECUTE FUNCTION %I()', r.t, 'sync_emit_' || r.t);
        EXECUTE format('CREATE TRIGGER sync_emit_del AFTER DELETE ON %I '
                       'REFERENCING OLD TABLE AS old_rows '
                       'FOR EACH STATEMENT EXECUTE FUNCTION %I()', r.t, 'sync_emit_' || r.t);
    END LOOP;
END
$optb$;

-- ── open ticket line children, INSERT as one statement ─────────────────────
CREATE OR REPLACE FUNCTION open_ticket_item_children_ins() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    -- What open_ticket_item_children() writes for a new line, for every new
    -- line at once (each new line has no children yet: nothing to delete).
    INSERT INTO open_ticket_item_addons (org_id, item_id, position, addon_item_id, quantity, unit_price)
    SELECT n.org_id, n.id, (a.ord - 1)::int, (a.v ->> 'addon_item_id')::uuid,
           COALESCE((a.v ->> 'quantity')::int, 1), COALESCE((a.v ->> 'unit_price')::int, 0)
      FROM new_rows n
      CROSS JOIN LATERAL jsonb_array_elements(COALESCE(n.line -> 'input' -> 'addons', '[]'::jsonb))
           WITH ORDINALITY AS a(v, ord)
     WHERE a.v ? 'addon_item_id';
    INSERT INTO open_ticket_item_optionals (org_id, item_id, position, option_id)
    SELECT n.org_id, n.id, (o.ord - 1)::int, (o.v #>> '{}')::uuid
      FROM new_rows n
      CROSS JOIN LATERAL jsonb_array_elements(COALESCE(n.line -> 'input' -> 'optional_field_ids', '[]'::jsonb))
           WITH ORDINALITY AS o(v, ord);
    RETURN NULL;
END $$;

DROP TRIGGER IF EXISTS open_ticket_item_children ON open_ticket_items;
CREATE TRIGGER open_ticket_item_children AFTER UPDATE OF line ON open_ticket_items
    FOR EACH ROW EXECUTE FUNCTION open_ticket_item_children();
CREATE TRIGGER open_ticket_item_children_ins AFTER INSERT ON open_ticket_items
    REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION open_ticket_item_children_ins();

-- ── the duplicate updated_at trigger on orders ──────────────────────────────
-- Only while its twin is there: an order UPDATE always stamps updated_at.
DO $dup$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_trigger t JOIN pg_proc p ON p.oid = t.tgfoid
                WHERE t.tgrelid = 'orders'::regclass AND t.tgname = 'orders_set_updated_at'
                  AND p.proname = 'set_updated_at' AND t.tgenabled <> 'D') THEN
        DROP TRIGGER IF EXISTS trg_orders_updated_at ON orders;
    END IF;
END
$dup$;
