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
--   addon_items                -> addon_item                  (org fan-out; legacy table)
--   addon_item_ingredients     -> addon_item (parent)         (org fan-out; legacy table)
--   branch_addon_overrides     -> addon_item (that branch; legacy table)
--   branch_delivery_settings   -> branch_settings (that branch)
--   role_permissions           -> teller (every user of the role) (org fan-out)
--
-- Tables of the original header that now ALSO emit `addon_item` (their registry
-- rows gain the type; their emitter functions are replaced whole below):
--   modifier_groups      (a legacy addon group: its options; a deleted group retires them)
--   modifier_options     (legacy_source 'addon')
--   recipe_lines         (owner_type 'modifier_option')
--   menu_price_overrides (target_type 'modifier_option', branch scope)
--   org_ingredients      (the ingredient an addon uses was renamed)
--
-- TWO SCHEMA STATES. Before the menu-unification contract shim
-- (deploy/menu_unification_shim.sql) `addon_items`, `addon_item_ingredients` and
-- `branch_addon_overrides` are TABLES the legacy handlers write. After it they are
-- VIEWS over the unified tables, which cannot carry row triggers. So the legacy
-- triggers are created only where the relation is a table, and the unified
-- tables' emitters cover the view state. In the table state a unified write may
-- touch an addon too: the feed is compacted, so a second emit only moves a seq.

-- /addon-items lists inactive addons too (the till greys them); a branch that
-- turned one off ships it with `is_available:false`. So every addon is live.
-- Keyed by id, never by the `addon_items` row type: the contract shim drops the
-- table (CASCADE would take a function over its row type with it) and recreates it
-- as a view; SQL/plpgsql bodies resolve the name at call time and keep working.
CREATE FUNCTION sync_live_addon_item(p_id uuid) RETURNS boolean LANGUAGE sql STABLE
    AS $$ SELECT p_id IS NOT NULL $$;

CREATE FUNCTION sync_touch_addon_item(p_addon uuid, p_branch uuid DEFAULT NULL) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    o uuid;
BEGIN
    IF p_addon IS NULL THEN RETURN; END IF;
    SELECT org_id INTO o FROM addon_items WHERE id = p_addon;
    IF NOT FOUND THEN RETURN; END IF;
    IF p_branch IS NULL THEN
        PERFORM sync_emit_org(o, 'addon_item', p_addon, sync_op(sync_live_addon_item(p_addon)));
    ELSIF EXISTS (SELECT 1 FROM branches WHERE id = p_branch AND org_id = o AND deleted_at IS NULL) THEN
        PERFORM sync_emit(p_branch, 'addon_item', p_addon, sync_op(sync_live_addon_item(p_addon)));
    END IF;
END;
$$;

-- An addon id that may have stopped being one: re-emit it if it still is,
-- otherwise retire it from the org's branches.
CREATE FUNCTION sync_touch_or_retire_addon_item(p_addon uuid, p_org uuid) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
BEGIN
    IF p_addon IS NULL THEN RETURN; END IF;
    IF EXISTS (SELECT 1 FROM addon_items WHERE id = p_addon) THEN
        PERFORM sync_touch_addon_item(p_addon);
    ELSIF p_org IS NOT NULL THEN
        PERFORM sync_emit_org(p_org, 'addon_item', p_addon, 'delete');
    END IF;
END;
$$;

-- Every addon the org's feed still lists that no longer exists (a group delete
-- cascaded its options away before this trigger could name them).
CREATE FUNCTION sync_touch_retired_addon_items(p_org uuid) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    x uuid;
BEGIN
    FOR x IN SELECT DISTINCT s.entity_id FROM sync_changes s JOIN branches b ON b.id = s.branch_id
              WHERE b.org_id = p_org AND s.type = 'addon_item' AND s.op = 'upsert'
                AND NOT EXISTS (SELECT 1 FROM addon_items a WHERE a.id = s.entity_id)
              ORDER BY 1 LOOP
        PERFORM sync_emit_org(p_org, 'addon_item', x, 'delete');
    END LOOP;
END;
$$;

-- ── Legacy tables (table state only) ─────────────────────────────────────────
CREATE FUNCTION sync_emit_addon_items() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit_org(OLD.org_id, 'addon_item', OLD.id, 'delete');
    ELSE
        PERFORM sync_emit_org(NEW.org_id, 'addon_item', NEW.id, sync_op(sync_live_addon_item(NEW.id)));
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

-- ── Unified tables (both states): the original bodies, plus addon_item ──────
CREATE OR REPLACE FUNCTION sync_emit_modifier_groups() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    o uuid;
BEGIN
    -- On DELETE the link rows cascade (their own trigger re-emits the items).
    IF TG_OP <> 'DELETE' THEN PERFORM sync_touch_menu_items_of_group(NEW.id); END IF;
    IF TG_OP = 'DELETE' THEN
        IF OLD.legacy_addon_type IS NOT NULL THEN PERFORM sync_touch_retired_addon_items(OLD.org_id); END IF;
    ELSIF NEW.legacy_addon_type IS NOT NULL OR (TG_OP = 'UPDATE' AND OLD.legacy_addon_type IS NOT NULL) THEN
        FOR o IN SELECT id FROM modifier_options WHERE group_id = NEW.id ORDER BY id LOOP
            PERFORM sync_touch_or_retire_addon_item(o, NEW.org_id);
        END LOOP;
        IF TG_OP = 'UPDATE' AND OLD.org_id IS DISTINCT FROM NEW.org_id THEN
            PERFORM sync_touch_retired_addon_items(OLD.org_id);
        END IF;
    END IF;
    RETURN NULL;
END $$;

CREATE OR REPLACE FUNCTION sync_emit_modifier_options() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_menu_items_of_group(OLD.group_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.group_id IS DISTINCT FROM OLD.group_id) THEN
        PERFORM sync_touch_menu_items_of_group(NEW.group_id);
    END IF;
    IF TG_OP IN ('UPDATE','DELETE') AND OLD.legacy_source = 'addon' THEN
        PERFORM sync_touch_or_retire_addon_item(OLD.id, (SELECT org_id FROM modifier_groups WHERE id = OLD.group_id));
    END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND NEW.legacy_source = 'addon' THEN
        PERFORM sync_touch_addon_item(NEW.id);
    END IF;
    RETURN NULL;
END $$;

CREATE OR REPLACE FUNCTION sync_emit_recipe_lines() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_recipe_owner(OLD.owner_type, OLD.owner_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR (NEW.owner_type, NEW.owner_id) IS DISTINCT FROM (OLD.owner_type, OLD.owner_id)) THEN
        PERFORM sync_touch_recipe_owner(NEW.owner_type, NEW.owner_id);
    END IF;
    IF TG_OP IN ('UPDATE','DELETE') AND OLD.owner_type = 'modifier_option' THEN PERFORM sync_touch_addon_item(OLD.owner_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND NEW.owner_type = 'modifier_option' THEN PERFORM sync_touch_addon_item(NEW.owner_id); END IF;
    RETURN NULL;
END $$;

CREATE OR REPLACE FUNCTION sync_emit_menu_price_overrides() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    -- branch / branch_channel scope: that branch only; channel scope (branch_id NULL): org fan-out.
    IF TG_OP IN ('UPDATE','DELETE') THEN
        PERFORM sync_touch_price_override(OLD.target_type, OLD.target_id, OLD.branch_id);
    END IF;
    IF TG_OP IN ('INSERT','UPDATE') THEN
        PERFORM sync_touch_price_override(NEW.target_type, NEW.target_id, NEW.branch_id);
    END IF;
    -- The addon list reads branch-scope overrides only.
    IF TG_OP IN ('UPDATE','DELETE') AND OLD.target_type = 'modifier_option' AND OLD.scope = 'branch' THEN
        PERFORM sync_touch_addon_item(OLD.target_id, OLD.branch_id);
    END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND NEW.target_type = 'modifier_option' AND NEW.scope = 'branch' THEN
        PERFORM sync_touch_addon_item(NEW.target_id, NEW.branch_id);
    END IF;
    RETURN NULL;
END $$;

CREATE OR REPLACE FUNCTION sync_emit_org_ingredients() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    a uuid;
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit_org(OLD.org_id, 'ingredient', OLD.id, 'delete');
    ELSE
        PERFORM sync_emit_org(NEW.org_id, 'ingredient', NEW.id, sync_op(sync_live_ingredient(NEW)));
        -- An addon's ingredient lines show the ingredient's name and unit.
        IF TG_OP = 'UPDATE' AND (OLD.name, OLD.unit) IS DISTINCT FROM (NEW.name, NEW.unit) THEN
            FOR a IN SELECT DISTINCT addon_item_id FROM addon_item_ingredients WHERE org_ingredient_id = NEW.id ORDER BY 1 LOOP
                PERFORM sync_touch_addon_item(a);
            END LOOP;
        END IF;
    END IF;
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

-- A role default changed (super admin only, rare). Role defaults are global —
-- `role_permissions` has no org — so the change is real in every org that has
-- users of the role; it cannot be scoped to one org without changing what a
-- permission means. What IS scoped: one statement-level pass per change (not a
-- pass per row), only the (role, resource, action) triples that changed, and
-- only the users whose EFFECTIVE permission can move — a per-user override for
-- the same resource/action pins theirs, so they are not re-projected. Each org's
-- users are emitted to that org's branches only.
CREATE FUNCTION sync_emit_role_permissions() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    u uuid;
BEGIN
    CREATE TEMP TABLE IF NOT EXISTS _sync_role_perm_changes (role user_role, resource permission_resource, action permission_action) ON COMMIT DROP;
    TRUNCATE _sync_role_perm_changes;
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        INSERT INTO _sync_role_perm_changes SELECT n.role, n.resource, n.action FROM new_rows n;
    END IF;
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        INSERT INTO _sync_role_perm_changes SELECT o.role, o.resource, o.action FROM old_rows o;
    END IF;
    FOR u IN
        SELECT DISTINCT usr.id
          FROM _sync_role_perm_changes c
          JOIN users usr ON usr.role = c.role AND usr.deleted_at IS NULL AND usr.org_id IS NOT NULL
         WHERE NOT EXISTS (SELECT 1 FROM permissions p
                            WHERE p.user_id = usr.id AND p.resource = c.resource AND p.action = c.action)
         ORDER BY usr.id
    LOOP
        PERFORM sync_touch_teller(u);
    END LOOP;
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
        ('modifier_groups',            ARRAY['menu_item','addon_item']),
        ('modifier_options',           ARRAY['menu_item','addon_item']),
        ('recipe_lines',               ARRAY['menu_item','addon_item']),
        ('menu_price_overrides',       ARRAY['menu_item','addon_item']),
        ('menu_item_recipe_steps',     ARRAY['menu_item']),
        ('recipe_step_presets',        ARRAY['menu_item']),
        ('menu_item_station_routes',   ARRAY['menu_item']),
        ('category_station_routes',    ARRAY['menu_item']),
        ('bundles',                    ARRAY['bundle']),
        ('bundle_components',          ARRAY['bundle']),
        ('bundle_branch_availability', ARRAY['bundle']),
        ('org_ingredients',            ARRAY['ingredient','addon_item']),
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
    SELECT ob.branch_id, 'addon_item', x.id FROM addon_items x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_addon_item(x.id)
    $$;

-- Legacy addon relations carry a trigger only while they are tables (see the header).
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['addon_items','addon_item_ingredients','branch_addon_overrides'] LOOP
        IF EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                    WHERE n.nspname = 'public' AND c.relname = t AND c.relkind IN ('r','p')) THEN
            EXECUTE format('CREATE TRIGGER sync_emit AFTER INSERT OR UPDATE OR DELETE ON %I FOR EACH ROW EXECUTE FUNCTION %I()', t, 'sync_emit_' || t);
        END IF;
    END LOOP;
END $$;
CREATE TRIGGER sync_emit AFTER INSERT OR UPDATE OR DELETE ON branch_delivery_settings FOR EACH ROW EXECUTE FUNCTION sync_emit_branch_delivery_settings();
CREATE TRIGGER sync_emit AFTER UPDATE ON role_permissions REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION sync_emit_role_permissions();
CREATE TRIGGER sync_emit_insert AFTER INSERT ON role_permissions REFERENCING NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION sync_emit_role_permissions();
CREATE TRIGGER sync_emit_delete AFTER DELETE ON role_permissions REFERENCING OLD TABLE AS old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION sync_emit_role_permissions();

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
