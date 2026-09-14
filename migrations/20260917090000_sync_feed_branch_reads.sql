-- /sync/pull gaps, part 2 (madar "offline-first by default"): the branch reads a
-- POS still fetched on its own, on a timer, outside the changefeed. They now
-- ride the `branch_settings` row the device already holds, so every screen
-- reads them locally and a change reaches the till with the feed.
--
--   * branch_settings.kitchen_stations          — the branch's live stations
--                                                 (the `/kitchen/stations` shape).
--   * branch_settings.kitchen_routing_effective — `/kitchen/routing-mode`'s
--                                                 `effective` (auto resolved).
--   * branch_settings.delivery                  — `/delivery/settings` (null when
--                                                 the branch has no row: defaults).
--   * branch_settings.tax_policy                — the branch's EFFECTIVE tax policy
--                                                 (branch override, else the org's:
--                                                 `/auth/me`'s `tax_policy`) and
--                                                 `org_require_table_for_orders`.
--   * branch_settings.loyalty                   — the branch's EFFECTIVE programme
--                                                 (branch row, else org row, else
--                                                 null = disabled): enabled, mode,
--                                                 program_name, program_name_ar.
-- The projection lives in src/sync/pull/projection.rs; fields are only added,
-- so an older POS reads the row exactly as before.
--
-- kitchen_stations and branch_delivery_settings already emit branch_settings.
-- New here: loyalty_settings, and organizations (its tax policy and
-- require-table switch are every branch's effective policy).
--
-- SOURCE TABLES (additions; machine-read by the migration tests together with
-- the earlier changefeed headers). Format:  table -> type[, type…]  (scope)
--   loyalty_settings           -> branch_settings (branch row: that branch; org row: org fan-out)
--   organizations              -> branch_settings (org fan-out; tax policy / require-table changes only)

CREATE FUNCTION sync_touch_org_branch_settings(p_org uuid, p_branch uuid) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    r branches;
BEGIN
    FOR r IN SELECT * FROM branches
              WHERE org_id = p_org AND (p_branch IS NULL OR id = p_branch)
              ORDER BY id
    LOOP
        PERFORM sync_emit(r.id, 'branch_settings', r.id, sync_op(sync_live_branch_settings(r)));
    END LOOP;
END $$;

CREATE FUNCTION sync_emit_loyalty_settings() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN
        PERFORM sync_touch_org_branch_settings(OLD.org_id, OLD.branch_id);
    END IF;
    IF TG_OP IN ('INSERT','UPDATE') THEN
        PERFORM sync_touch_org_branch_settings(NEW.org_id, NEW.branch_id);
    END IF;
    RETURN NULL;
END $$;

CREATE TRIGGER sync_emit AFTER INSERT OR UPDATE OR DELETE ON loyalty_settings
    FOR EACH ROW EXECUTE FUNCTION sync_emit_loyalty_settings();

CREATE FUNCTION sync_emit_organizations() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF (OLD.tax_rate, OLD.tax_inclusive, OLD.service_charge_rate, OLD.service_charge_taxable, OLD.require_table_for_orders)
       IS DISTINCT FROM
       (NEW.tax_rate, NEW.tax_inclusive, NEW.service_charge_rate, NEW.service_charge_taxable, NEW.require_table_for_orders) THEN
        PERFORM sync_touch_org_branch_settings(NEW.id, NULL);
    END IF;
    RETURN NULL;
END $$;

CREATE TRIGGER sync_emit AFTER UPDATE ON organizations
    FOR EACH ROW EXECUTE FUNCTION sync_emit_organizations();

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
        ('organizations',              ARRAY['branch_settings'])
    $$;

-- Every branch's settings row gains fields: move its seq so a device already
-- holding the row takes the new projection on its next incremental pull.
DO $$
DECLARE
    r branches;
BEGIN
    FOR r IN SELECT * FROM branches WHERE deleted_at IS NULL ORDER BY id LOOP
        PERFORM sync_emit(r.id, 'branch_settings', r.id, 'upsert');
    END LOOP;
END $$;
