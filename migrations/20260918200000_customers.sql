-- Manual customers (PERMISSIONS_RESUME phase 6; owner decision: separate from
-- loyalty but linkable; attached in the POS, managed and merged in the
-- dashboard; offline through /sync/pull; PDPL applies).
--
-- A till may create a customer while offline, so ids are client-minted and a
-- replayed create is idempotent on id. Two tablets that both add the same
-- phone offline would make a duplicate; instead the second create is stored
-- already MERGED into the live customer holding that phone, so an order that
-- names either id lands on one person. Merging is one-way (`merged_into`), and
-- `customers_resolve` follows the chain.
--
-- PDPL: `erased_at` wipes name, phone and notes but keeps the row, so past
-- orders keep a valid (now anonymous) reference.
--
-- SOURCE TABLES (additions; machine-read by the migration tests together with
-- the earlier changefeed headers). Format:  table -> type[, type…]  (scope)
--   customers                  -> customer                    (org fan-out)

CREATE TABLE IF NOT EXISTS customers (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id              uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name                text NOT NULL,
    phone               text NULL,
    -- Digits only, a leading Egyptian +20 folded to 0: the lookup key.
    phone_key           text NULL,
    notes               text NULL,
    loyalty_customer_id uuid NULL REFERENCES loyalty_customers(id) ON DELETE SET NULL,
    merged_into         uuid NULL REFERENCES customers(id),
    merged_at           timestamptz NULL,
    erased_at           timestamptz NULL,
    created_by          uuid NULL REFERENCES users(id) ON DELETE SET NULL,
    created_branch_id   uuid NULL REFERENCES branches(id) ON DELETE SET NULL,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT customers_not_merged_into_self CHECK (merged_into IS DISTINCT FROM id)
);
CREATE INDEX IF NOT EXISTS customers_org_live ON customers (org_id, lower(name))
    WHERE merged_into IS NULL AND erased_at IS NULL;
CREATE INDEX IF NOT EXISTS customers_org_phone ON customers (org_id, phone_key)
    WHERE merged_into IS NULL AND erased_at IS NULL AND phone_key IS NOT NULL;

ALTER TABLE customers ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS customers_tenant ON customers;
CREATE POLICY customers_tenant ON customers
    USING (org_id = NULLIF(current_setting('app.org_id', true), '')::uuid);
GRANT SELECT, INSERT, UPDATE ON customers TO madar_app;

-- No foreign key: a sale is never refused over its customer reference (the
-- customer's own queued create may have been refused or not arrived yet).
ALTER TABLE orders ADD COLUMN IF NOT EXISTS customer_id uuid NULL;
CREATE INDEX IF NOT EXISTS orders_customer ON orders (customer_id, created_at DESC)
    WHERE customer_id IS NOT NULL;

CREATE OR REPLACE FUNCTION customers_phone_key(p text) RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT CASE
        WHEN d = '' THEN NULL
        WHEN d LIKE '0020%' THEN '0' || substr(d, 5)
        WHEN d LIKE '20%' AND length(d) = 12 THEN '0' || substr(d, 3)
        ELSE d END
      FROM (SELECT regexp_replace(coalesce(p, ''), '[^0-9]', '', 'g') AS d) x
$$;

-- The live customer an id stands for now (follows merges), within the org.
CREATE OR REPLACE FUNCTION customers_resolve(p_org uuid, p_id uuid) RETURNS uuid
LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path = public AS $$
DECLARE
    cur  uuid := p_id;
    nxt  uuid;
    hops int := 0;
BEGIN
    LOOP
        SELECT merged_into INTO nxt FROM customers WHERE id = cur AND org_id = p_org;
        IF NOT FOUND THEN RETURN NULL; END IF;
        IF nxt IS NULL THEN RETURN cur; END IF;
        cur := nxt;
        hops := hops + 1;
        IF hops > 32 THEN RETURN NULL; END IF;
    END LOOP;
END $$;
GRANT EXECUTE ON FUNCTION customers_resolve(uuid, uuid) TO madar_app;

-- ── Feed ────────────────────────────────────────────────────────────────────
CREATE FUNCTION sync_live_customer(r customers) RETURNS boolean LANGUAGE sql STABLE
    AS $$ SELECT r.merged_into IS NULL AND r.erased_at IS NULL $$;

CREATE FUNCTION sync_emit_customers() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit_org(OLD.org_id, 'customer', OLD.id, 'delete');
    ELSE
        PERFORM sync_emit_org(NEW.org_id, 'customer', NEW.id, sync_op(sync_live_customer(NEW)));
    END IF;
    RETURN NULL;
END $$;
CREATE TRIGGER sync_emit AFTER INSERT OR UPDATE OR DELETE ON customers
    FOR EACH ROW EXECUTE FUNCTION sync_emit_customers();

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
        ('role_permissions',           ARRAY['teller']),
        ('loyalty_settings',           ARRAY['branch_settings']),
        ('organizations',              ARRAY['branch_settings']),
        ('role_assignments',           ARRAY['teller']),
        ('role_assignment_branches',   ARRAY['teller']),
        ('user_overrides',             ARRAY['teller']),
        ('org_role_grants',            ARRAY['teller']),
        ('org_capability_policy',      ARRAY['teller']),
        ('customers',                  ARRAY['customer'])
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
           ('addon_item', false), ('customer', false)
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
    UNION ALL
    SELECT ob.branch_id, 'customer', x.id FROM customers x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_customer(x)
    $$;
