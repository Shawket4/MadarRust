-- /sync/pull gaps (TILLS_VERIFICATION "Before/with B"): what a device still
-- fetched outside the changefeed. Offline plan B reads these from the feed.
--
--   * addon_item      — the branch-effective `/addon-items` list, as its own type.
--   * teller          — now carries the person's effective GRANTED permissions, so
--                       a grant or revocation reaches a till through the feed
--                       (role defaults re-emit every user of the role).
--   * branch_settings — now carries the branch's delivery prep minutes.
--   (order lines for reprint and per-channel menu prices are projection-only:
--    order_items already re-emits `order`, channel overrides already re-emit
--    `menu_item`.)
--
-- Nothing here edits an applied migration: the registry functions are replaced
-- whole, the originals' header still describes their tables, and this header
-- describes the additions (read together by the migration tests).
--
-- SOURCE TABLES (additions; machine-read by the migration tests together with
-- 20260914090300's header). Format:  table -> type[, type…]  (scope)
--   addon_items                -> addon_item                  (org fan-out)
--   addon_item_ingredients     -> addon_item (parent)         (org fan-out)
--   branch_addon_overrides     -> addon_item (that branch)
--   branch_delivery_settings   -> branch_settings (that branch)
--   role_permissions           -> teller (every user of the role) (org fan-out)

-- /addon-items lists inactive addons too (the till greys them); a branch that
-- turned one off ships it with `is_available:false`. So every addon is live.
CREATE FUNCTION sync_live_addon_item(r addon_items) RETURNS boolean LANGUAGE sql STABLE
    AS $$ SELECT r.id IS NOT NULL $$;

CREATE FUNCTION sync_touch_addon_item(p_addon uuid, p_branch uuid DEFAULT NULL) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    r addon_items;
BEGIN
    SELECT * INTO r FROM addon_items WHERE id = p_addon;
    IF NOT FOUND THEN RETURN; END IF;
    IF p_branch IS NULL THEN
        PERFORM sync_emit_org(r.org_id, 'addon_item', r.id, sync_op(sync_live_addon_item(r)));
    ELSIF EXISTS (SELECT 1 FROM branches WHERE id = p_branch AND org_id = r.org_id AND deleted_at IS NULL) THEN
        PERFORM sync_emit(p_branch, 'addon_item', r.id, sync_op(sync_live_addon_item(r)));
    END IF;
END;
$$;

CREATE FUNCTION sync_emit_addon_items() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit_org(OLD.org_id, 'addon_item', OLD.id, 'delete');
    ELSE
        PERFORM sync_emit_org(NEW.org_id, 'addon_item', NEW.id, sync_op(sync_live_addon_item(NEW)));
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_addon_item_ingredients() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_addon_item(OLD.addon_item_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.addon_item_id IS DISTINCT FROM OLD.addon_item_id) THEN
        PERFORM sync_touch_addon_item(NEW.addon_item_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_branch_addon_overrides() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_addon_item(OLD.addon_item_id, OLD.branch_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') THEN PERFORM sync_touch_addon_item(NEW.addon_item_id, NEW.branch_id); END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_branch_delivery_settings() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    r branches;
    b uuid := CASE WHEN TG_OP = 'DELETE' THEN OLD.branch_id ELSE NEW.branch_id END;
BEGIN
    SELECT * INTO r FROM branches WHERE id = b;
    IF FOUND THEN
        PERFORM sync_emit(r.id, 'branch_settings', r.id, sync_op(sync_live_branch_settings(r)));
    END IF;
    RETURN NULL;
END $$;

-- A role default changed (super admin only, rare): every non-deleted user of
-- that role re-projects, in every org.
CREATE FUNCTION sync_emit_role_permissions() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    u uuid;
    rl user_role := CASE WHEN TG_OP = 'DELETE' THEN OLD.role ELSE NEW.role END;
BEGIN
    FOR u IN SELECT id FROM users WHERE role = rl AND deleted_at IS NULL ORDER BY id LOOP
        PERFORM sync_touch_teller(u);
    END LOOP;
    IF TG_OP = 'UPDATE' AND OLD.role IS DISTINCT FROM NEW.role THEN
        FOR u IN SELECT id FROM users WHERE role = OLD.role AND deleted_at IS NULL ORDER BY id LOOP
            PERFORM sync_touch_teller(u);
        END LOOP;
    END IF;
    RETURN NULL;
END $$;

CREATE OR REPLACE FUNCTION sync_source_tables() RETURNS TABLE (source_table text, types text[])
    LANGUAGE sql IMMUTABLE
    AS $$
    VALUES
        ('categories',                 ARRAY['category']),
        ('menu_items',                 ARRAY['menu_item']),
        ('menu_item_sizes',            ARRAY['menu_item']),
        ('menu_item_modifier_groups',  ARRAY['menu_item']),
        ('modifier_groups',            ARRAY['menu_item']),
        ('modifier_options',           ARRAY['menu_item']),
        ('recipe_lines',               ARRAY['menu_item']),
        ('menu_price_overrides',       ARRAY['menu_item']),
        ('menu_item_recipe_steps',     ARRAY['menu_item']),
        ('recipe_step_presets',        ARRAY['menu_item']),
        ('menu_item_station_routes',   ARRAY['menu_item']),
        ('category_station_routes',    ARRAY['menu_item']),
        ('bundles',                    ARRAY['bundle']),
        ('bundle_components',          ARRAY['bundle']),
        ('bundle_branch_availability', ARRAY['bundle']),
        ('org_ingredients',            ARRAY['ingredient']),
        ('org_payment_methods',        ARRAY['payment_method']),
        ('branch_payment_methods',     ARRAY['payment_availability']),
        ('user_payment_methods',       ARRAY['payment_availability']),
        ('device_payment_methods',     ARRAY['payment_availability']),
        ('discounts',                  ARRAY['discount']),
        ('branches',                   ARRAY['branch_settings']),
        ('kitchen_stations',           ARRAY['branch_settings']),
        ('devices',                    ARRAY['device']),
        ('users',                      ARRAY['teller']),
        ('user_branch_assignments',    ARRAY['teller']),
        ('permissions',                ARRAY['teller']),
        ('floor_sections',             ARRAY['floor_section']),
        ('branch_tables',              ARRAY['floor_table']),
        ('table_occupancies',          ARRAY['table_occupancy','floor_table']),
        ('booking_tables',             ARRAY['booking','floor_table']),
        ('bookings',                   ARRAY['booking','floor_table']),
        ('table_transfer_requests',    ARRAY['table_transfer']),
        ('open_tickets',               ARRAY['open_ticket']),
        ('open_ticket_items',          ARRAY['open_ticket']),
        ('open_ticket_rounds',         ARRAY['open_ticket']),
        ('kitchen_tickets',            ARRAY['kitchen_ticket']),
        ('kitchen_ticket_items',       ARRAY['kitchen_ticket']),
        ('delivery_orders',            ARRAY['delivery']),
        ('tills',                      ARRAY['till']),
        ('till_reconciliations',       ARRAY['till']),
        ('till_cash_movements',        ARRAY['cash_movement','till']),
        ('orders',                     ARRAY['order']),
        ('order_items',                ARRAY['order']),
        ('order_payments',             ARRAY['order']),
        ('order_refunds',              ARRAY['refund']),
        ('order_refund_lines',         ARRAY['refund']),
        ('addon_items',                ARRAY['addon_item']),
        ('addon_item_ingredients',     ARRAY['addon_item']),
        ('branch_addon_overrides',     ARRAY['addon_item']),
        ('branch_delivery_settings',   ARRAY['branch_settings']),
        ('role_permissions',           ARRAY['teller'])
    $$;

CREATE OR REPLACE FUNCTION sync_types() RETURNS TABLE (type text, is_ledger boolean)
    LANGUAGE sql IMMUTABLE
    AS $$
    VALUES ('category', false), ('menu_item', false), ('bundle', false), ('ingredient', false),
           ('payment_method', false), ('payment_availability', false), ('discount', false),
           ('branch_settings', false), ('device', false), ('teller', false),
           ('floor_section', false), ('floor_table', false), ('table_occupancy', false),
           ('table_transfer', false), ('open_ticket', false), ('kitchen_ticket', false),
           ('delivery', false), ('booking', false),
           ('till', true), ('cash_movement', true), ('order', true), ('refund', true),
           ('addon_item', false)
    $$;

CREATE OR REPLACE FUNCTION sync_live_rows() RETURNS TABLE (branch_id uuid, type text, entity_id uuid)
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public
    AS $$
    WITH ob AS (SELECT id AS branch_id, org_id FROM branches WHERE deleted_at IS NULL)
    SELECT ob.branch_id, 'category', x.id FROM categories x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_category(x)
    UNION ALL
    SELECT ob.branch_id, 'menu_item', x.id FROM menu_items x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_menu_item(x)
    UNION ALL
    SELECT ob.branch_id, 'bundle', x.id FROM bundles x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_bundle(x)
    UNION ALL
    SELECT ob.branch_id, 'ingredient', x.id FROM org_ingredients x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_ingredient(x)
    UNION ALL
    SELECT ob.branch_id, 'payment_method', x.id FROM org_payment_methods x JOIN ob ON ob.org_id = x.org_id
    UNION ALL
    SELECT DISTINCT x.branch_id, 'payment_availability', x.branch_id FROM branch_payment_methods x JOIN ob ON ob.branch_id = x.branch_id
    UNION ALL
    SELECT DISTINCT ob.branch_id, 'payment_availability', x.user_id FROM user_payment_methods x JOIN users u ON u.id = x.user_id JOIN ob ON ob.org_id = u.org_id
    UNION ALL
    SELECT DISTINCT ob.branch_id, 'payment_availability', x.device_id FROM device_payment_methods x JOIN devices d ON d.id = x.device_id JOIN ob ON ob.org_id = d.org_id
    UNION ALL
    SELECT ob.branch_id, 'discount', x.id FROM discounts x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_discount(x)
    UNION ALL
    SELECT x.id, 'branch_settings', x.id FROM branches x WHERE sync_live_branch_settings(x)
    UNION ALL
    SELECT x.branch_id, 'device', x.id FROM devices x WHERE x.branch_id IS NOT NULL AND sync_live_device(x)
    UNION ALL
    SELECT ob.branch_id, 'teller', x.id FROM users x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_teller(x)
    UNION ALL
    SELECT x.branch_id, 'floor_section', x.id FROM floor_sections x
    UNION ALL
    SELECT x.branch_id, 'floor_table', x.id FROM branch_tables x WHERE sync_live_floor_table(x)
    UNION ALL
    SELECT x.branch_id, 'table_occupancy', x.id FROM table_occupancies x WHERE sync_live_table_occupancy(x)
    UNION ALL
    SELECT x.branch_id, 'table_transfer', x.id FROM table_transfer_requests x WHERE sync_live_table_transfer(x)
    UNION ALL
    SELECT x.branch_id, 'open_ticket', x.id FROM open_tickets x WHERE sync_live_open_ticket(x)
    UNION ALL
    SELECT x.branch_id, 'kitchen_ticket', x.id FROM kitchen_tickets x WHERE sync_live_kitchen_ticket(x)
    UNION ALL
    SELECT x.branch_id, 'delivery', x.id FROM delivery_orders x WHERE sync_live_delivery(x)
    UNION ALL
    SELECT x.branch_id, 'booking', x.id FROM bookings x WHERE sync_live_booking(x)
    UNION ALL
    SELECT x.branch_id, 'till', x.id FROM tills x
    UNION ALL
    SELECT t.branch_id, 'cash_movement', x.id FROM till_cash_movements x JOIN tills t ON t.id = x.till_id
    UNION ALL
    SELECT x.branch_id, 'order', x.id FROM orders x
    UNION ALL
    SELECT x.branch_id, 'refund', x.id FROM order_refunds x
    UNION ALL
    SELECT ob.branch_id, 'addon_item', x.id FROM addon_items x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_addon_item(x)
    $$;

CREATE TRIGGER sync_emit AFTER INSERT OR UPDATE OR DELETE ON addon_items FOR EACH ROW EXECUTE FUNCTION sync_emit_addon_items();
CREATE TRIGGER sync_emit AFTER INSERT OR UPDATE OR DELETE ON addon_item_ingredients FOR EACH ROW EXECUTE FUNCTION sync_emit_addon_item_ingredients();
CREATE TRIGGER sync_emit AFTER INSERT OR UPDATE OR DELETE ON branch_addon_overrides FOR EACH ROW EXECUTE FUNCTION sync_emit_branch_addon_overrides();
CREATE TRIGGER sync_emit AFTER INSERT OR UPDATE OR DELETE ON branch_delivery_settings FOR EACH ROW EXECUTE FUNCTION sync_emit_branch_delivery_settings();
CREATE TRIGGER sync_emit AFTER INSERT OR UPDATE OR DELETE ON role_permissions FOR EACH ROW EXECUTE FUNCTION sync_emit_role_permissions();

-- Backfill the new type so the feed is complete from the moment this runs.
-- (A device's next full pull lists it; an incremental pull sees the rows as
-- ordinary changes.) Existing tellers and branch settings need no backfill:
-- their rows exist and their projections simply gain fields.
INSERT INTO sync_changes (branch_id, type, entity_id, op)
SELECT l.branch_id, l.type, l.entity_id, 'upsert'
  FROM sync_live_rows() l
 WHERE l.type = 'addon_item'
 ORDER BY l.branch_id, l.entity_id
ON CONFLICT DO NOTHING;

DO $$
DECLARE
    bad bigint;
BEGIN
    SELECT count(*) INTO bad FROM (
        (SELECT branch_id, type, entity_id FROM sync_changes WHERE op = 'upsert' AND type = 'addon_item'
         EXCEPT SELECT DISTINCT branch_id, type, entity_id FROM sync_live_rows() WHERE type = 'addon_item')
        UNION ALL
        (SELECT DISTINCT branch_id, type, entity_id FROM sync_live_rows() WHERE type = 'addon_item'
         EXCEPT SELECT branch_id, type, entity_id FROM sync_changes WHERE op = 'upsert' AND type = 'addon_item')
    ) d;
    IF bad <> 0 THEN
        RAISE EXCEPTION 'sync feed gaps invariant: addon_item backfill differs from the live set by % row(s)', bad;
    END IF;
END $$;
