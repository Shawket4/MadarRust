-- Remove the combos / bundles module.
--
-- The owner (2026-09-25): "rip it out completely"; a new module will be
-- designed from scratch. The code is gone; this drops its tables, its column
-- on order_items, its sync-feed plumbing and its enum.
--
-- PROD DATA. Production held exactly one bundle (org Rue, "Spanish Latte &
-- Perry", ARCHIVED, offered only at Rue's "Test" branch for 2026-05-20..21)
-- and two completed test sales of it at that branch on 2026-05-21:
--   59311db1-bd4f-4d96-ad87-de2c4bc4b5fc  TEST-260521-0001  350.00 card
--   07aed383-4a6e-4f84-9045-52c6ce131e9d  TEST-260521-0002  510.00 card
--     (the bundle plus one "Spanish Latte Blended")
-- The owner decided to delete those two orders with the bundle. Nothing else
-- referenced them: no refund, loyalty, kitchen, delivery, open-ticket, staff
-- drink, stock movement or text reference (checked on prod, read-only, before
-- writing this). Their till (300293a8-276e-4ac7-8a5d-18123ee6644e, closed
-- 2026-05-23) stores cash figures only and both sales were card, so no
-- stored total changes; its report, computed from orders, shows 860.00 less
-- card sales. The ref counter keeps its gap (0001 and 0002 are gone, 0003
-- stays).
--
-- GUARD. Before deleting anything, the migration refuses (RAISE, whole
-- migration rolled back) if ANY order other than those two references a
-- bundle, or if either of the two is not at Rue's Test branch. It can never
-- silently delete a real sale. On a database without them it deletes nothing.
--
-- SOURCE TABLES REMOVED (machine-read by the migration tests; the header of
-- 20260914090300_sync_changefeed.sql still lists them):
--   bundles                    -> bundle
--   bundle_components          -> bundle
--   bundle_branch_availability -> bundle
--
-- The WIRE type `bundle` stays: madar-sync's ALL_TYPES carries it and old
-- tills ask for it, so /sync/pull answers it with an empty set (see
-- `sync::pull::projection`). sync_types() keeps listing it for that reason.

-- ── 1. The two test orders ──────────────────────────────────────────────────
DO $$
DECLARE
    test_orders CONSTANT uuid[] := ARRAY[
        '59311db1-bd4f-4d96-ad87-de2c4bc4b5fc',
        '07aed383-4a6e-4f84-9045-52c6ce131e9d'
    ]::uuid[];
    rue         CONSTANT uuid := '685f6bfa-0d44-4a9f-bb3e-50eec96d50c9';
    test_branch CONSTANT uuid := '7e3136dd-2bb7-4105-9fb3-3b50df8bb601';
    others  uuid[];
    doomed  uuid[];
    lines   uuid[];
    pays    uuid[];
    n       bigint;
BEGIN
    -- Every order a bundle touches, by any of the four ways a line can say so.
    SELECT coalesce(array_agg(DISTINCT x.order_id), '{}') INTO others FROM (
        SELECT oi.order_id FROM order_items oi WHERE oi.bundle_id IS NOT NULL
        UNION ALL
        SELECT oi.order_id FROM order_line_bundle_components c JOIN order_items oi ON oi.id = c.order_line_id
        UNION ALL
        SELECT oi.order_id FROM order_line_bundle_component_addons c JOIN order_items oi ON oi.id = c.order_line_id
        UNION ALL
        SELECT oi.order_id FROM order_line_bundle_component_optionals c JOIN order_items oi ON oi.id = c.order_line_id
    ) x
    WHERE x.order_id <> ALL (test_orders);
    IF cardinality(others) > 0 THEN
        RAISE EXCEPTION 'remove_combos: % order(s) besides the two Rue test orders reference a bundle (%); refusing to delete sales. Ask the owner.',
            cardinality(others), others;
    END IF;

    -- The two orders, only where they are what the owner approved deleting.
    SELECT count(*) INTO n FROM orders o
     WHERE o.id = ANY (test_orders)
       AND NOT EXISTS (SELECT 1 FROM branches b WHERE b.id = o.branch_id AND b.id = test_branch AND b.org_id = rue);
    IF n > 0 THEN
        RAISE EXCEPTION 'remove_combos: % of the test orders is not at Rue''s Test branch; refusing', n;
    END IF;

    SELECT coalesce(array_agg(o.id), '{}') INTO doomed FROM orders o WHERE o.id = ANY (test_orders);
    IF cardinality(doomed) = 0 THEN
        RETURN;
    END IF;
    SELECT coalesce(array_agg(id), '{}') INTO lines FROM order_items WHERE order_id = ANY (doomed);
    SELECT coalesce(array_agg(id), '{}') INTO pays FROM order_payments WHERE order_id = ANY (doomed);

    -- Children first, in dependency order. Refunds and loyalty rows reference
    -- these with ON DELETE RESTRICT: none exist, and if one appeared the
    -- DELETE below fails and the whole migration rolls back.
    DELETE FROM order_line_bundle_component_addons    WHERE order_line_id = ANY (lines);
    DELETE FROM order_line_bundle_component_optionals WHERE order_line_id = ANY (lines);
    DELETE FROM order_line_bundle_components          WHERE order_line_id = ANY (lines);
    DELETE FROM order_item_addons                     WHERE order_item_id = ANY (lines);
    DELETE FROM order_item_optionals                  WHERE order_item_id = ANY (lines);
    DELETE FROM order_items                           WHERE id = ANY (lines);
    DELETE FROM order_payments                        WHERE id = ANY (pays);
    DELETE FROM kitchen_tickets                       WHERE order_id = ANY (doomed);
    UPDATE open_tickets    SET order_id = NULL        WHERE order_id = ANY (doomed);
    UPDATE delivery_orders SET order_id = NULL        WHERE order_id = ANY (doomed);
    UPDATE staff_drinks    SET order_id = NULL        WHERE order_id = ANY (doomed);
    DELETE FROM orders                                WHERE id = ANY (doomed);
    -- Their feed rows, including the tombstones the deletes just emitted. The
    -- till closed in May, far outside the ledger window: no device holds them.
    DELETE FROM sync_changes WHERE entity_id = ANY (doomed || lines || pays);

    RAISE NOTICE 'remove_combos: deleted % test order(s), % line(s), % payment(s)',
        cardinality(doomed), cardinality(lines), cardinality(pays);
END $$;

-- ── 2. The feed: bundles leave the live sets and the source registry ───────
DELETE FROM sync_changes WHERE type = 'bundle';

CREATE OR REPLACE FUNCTION sync_live_rows() RETURNS TABLE (branch_id uuid, type text, entity_id uuid)
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public
    AS $$
    WITH ob AS (SELECT id AS branch_id, org_id FROM branches WHERE deleted_at IS NULL)
    SELECT ob.branch_id, 'category', x.id FROM categories x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_category(x)
    UNION ALL
    SELECT ob.branch_id, 'menu_item', x.id FROM menu_items x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_menu_item(x)
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
    UNION ALL
    SELECT ob.branch_id, 'customer', x.id FROM customers x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_customer(x)
    $$;

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
        ('role_permissions',           ARRAY['teller']),
        ('loyalty_settings',           ARRAY['branch_settings']),
        ('organizations',              ARRAY['branch_settings']),
        ('role_assignments',           ARRAY['teller']),
        ('role_assignment_branches',   ARRAY['teller']),
        ('user_overrides',             ARRAY['teller']),
        ('org_role_grants',            ARRAY['teller']),
        ('org_capability_policy',      ARRAY['teller']),
        ('customers',                  ARRAY['customer']),
        ('till_spot_views',           ARRAY['till']),
        ('staff_pool_settings',        ARRAY['branch_settings']),
        ('staff_drinks',               ARRAY['staff_drink']),
        ('loyalty_customers',          ARRAY['customer'])
    $$;

-- ── 3. The schema ───────────────────────────────────────────────────────────
DELETE FROM asset_jobs WHERE target_table = 'bundles';
DELETE FROM asset_backfill_items WHERE source_table = 'bundles';

-- The only function whose SIGNATURE names the bundles row type: it must go
-- before the table. Its callers (below, and the old sync_live_rows) are
-- plpgsql/sql bodies Postgres does not track.
DROP FUNCTION sync_live_bundle(bundles);

-- Takes the FK order_items_bundle_id_fkey with it.
ALTER TABLE order_items DROP COLUMN bundle_id, DROP COLUMN bundle_unit_price;

-- Triggers (sync_emit, trg_bundles_updated_at, asset_ref_changed), RLS
-- policies and grants go with their tables.
DROP TABLE order_line_bundle_component_addons;
DROP TABLE order_line_bundle_component_optionals;
DROP TABLE order_line_bundle_components;
DROP TABLE bundle_price_epochs;
DROP TABLE bundle_branch_availability;
DROP TABLE bundle_components;
DROP TABLE bundles;

DROP FUNCTION sync_touch_bundle(uuid, uuid);
DROP FUNCTION sync_emit_bundles();
DROP FUNCTION sync_emit_bundle_components();
DROP FUNCTION sync_emit_bundle_branch_availability();

-- Dropped by 20260704120000_drop_menu_advisor on every live database; kept
-- idempotent for a database restored from before it.
DROP TABLE IF EXISTS menu_advisor_bundle_suggestions;

DROP TYPE bundle_status;
