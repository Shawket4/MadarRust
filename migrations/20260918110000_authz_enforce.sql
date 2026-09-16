-- Permissions Phase 3 (PERMISSIONS_ARCHITECTURE.md §6 Phase 3): the new model
-- decides, so it must reach tills and survive owner edits.
--
-- 1. Assignments made in the new access editor are not "managed" by users.role:
--    the mirror only maintains a person's system-role assignment while they
--    have no assignment an owner set by hand.
-- 2. The POS feed carries each person's effective capabilities on their teller
--    row, so a grant change in the new tables must re-emit those rows.
--
-- SOURCE TABLES (additions; machine-read by the migration tests together with
-- the earlier changefeed headers). Format:  table -> type[, type…]  (scope)
--   role_assignments           -> teller (the person)
--   role_assignment_branches   -> teller (the assignment's person)
--   user_overrides             -> teller (the person)
--   org_role_grants            -> teller (everyone holding the role)
--   org_capability_policy      -> teller (org fan-out)

ALTER TABLE role_assignments ADD COLUMN IF NOT EXISTS managed boolean NOT NULL DEFAULT true;

CREATE OR REPLACE FUNCTION authz_sync_user(p_user uuid) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    u        users%ROWTYPE;
    v_role   uuid;
    v_assign uuid;
BEGIN
    SELECT * INTO u FROM users WHERE id = p_user;
    IF NOT FOUND OR u.org_id IS NULL OR u.role = 'super_admin' OR u.is_guest_principal
       OR u.deleted_at IS NOT NULL THEN
        UPDATE role_assignments SET revoked_at = now()
         WHERE user_id = p_user AND revoked_at IS NULL;
        RETURN;
    END IF;
    -- Access set by hand in the access editor wins; users.role is then only the
    -- label older tablets read.
    IF EXISTS (SELECT 1 FROM role_assignments
                WHERE user_id = p_user AND revoked_at IS NULL AND NOT managed) THEN
        UPDATE role_assignments SET revoked_at = now()
         WHERE user_id = p_user AND revoked_at IS NULL AND managed;
        RETURN;
    END IF;
    PERFORM authz_ensure_org_roles(u.org_id);
    SELECT id INTO v_role FROM org_roles
     WHERE org_id = u.org_id AND key = u.role::text AND is_system AND deleted_at IS NULL;

    UPDATE role_assignments SET revoked_at = now()
     WHERE user_id = p_user AND revoked_at IS NULL AND managed AND org_role_id <> v_role;

    SELECT id INTO v_assign FROM role_assignments
     WHERE user_id = p_user AND org_role_id = v_role AND revoked_at IS NULL;
    IF v_assign IS NULL THEN
        INSERT INTO role_assignments (org_id, user_id, org_role_id, all_branches, reason, managed)
        VALUES (u.org_id, p_user, v_role, u.role <> 'branch_manager', 'follows the account role', true)
        RETURNING id INTO v_assign;
    ELSE
        UPDATE role_assignments SET all_branches = (u.role <> 'branch_manager')
         WHERE id = v_assign AND all_branches IS DISTINCT FROM (u.role <> 'branch_manager');
    END IF;

    DELETE FROM role_assignment_branches rab
     WHERE rab.assignment_id = v_assign
       AND NOT EXISTS (SELECT 1 FROM user_branch_assignments a
                        WHERE a.user_id = p_user AND a.branch_id = rab.branch_id);
    INSERT INTO role_assignment_branches (assignment_id, branch_id, org_id)
    SELECT v_assign, a.branch_id, u.org_id
      FROM user_branch_assignments a JOIN branches b ON b.id = a.branch_id AND b.org_id = u.org_id
     WHERE a.user_id = p_user
    ON CONFLICT DO NOTHING;
END $$;

-- ── Feed emitters ───────────────────────────────────────────────────────────
CREATE FUNCTION sync_emit_role_assignments() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE', 'DELETE') THEN PERFORM sync_touch_teller(OLD.user_id); END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') THEN PERFORM sync_touch_teller(NEW.user_id); END IF;
    RETURN NULL;
END $$;
CREATE TRIGGER sync_emit AFTER INSERT OR UPDATE OR DELETE ON role_assignments
    FOR EACH ROW EXECUTE FUNCTION sync_emit_role_assignments();

CREATE FUNCTION sync_emit_role_assignment_branches() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE v_assign uuid := CASE WHEN TG_OP = 'DELETE' THEN OLD.assignment_id ELSE NEW.assignment_id END;
BEGIN
    PERFORM sync_touch_teller((SELECT user_id FROM role_assignments WHERE id = v_assign));
    RETURN NULL;
END $$;
CREATE TRIGGER sync_emit AFTER INSERT OR UPDATE OR DELETE ON role_assignment_branches
    FOR EACH ROW EXECUTE FUNCTION sync_emit_role_assignment_branches();

CREATE FUNCTION sync_emit_user_overrides() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE', 'DELETE') THEN PERFORM sync_touch_teller(OLD.user_id); END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') THEN PERFORM sync_touch_teller(NEW.user_id); END IF;
    RETURN NULL;
END $$;
CREATE TRIGGER sync_emit AFTER INSERT OR UPDATE OR DELETE ON user_overrides
    FOR EACH ROW EXECUTE FUNCTION sync_emit_user_overrides();

CREATE FUNCTION sync_emit_org_role_grants() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    v_role uuid := CASE WHEN TG_OP = 'DELETE' THEN OLD.org_role_id ELSE NEW.org_role_id END;
    u uuid;
BEGIN
    FOR u IN SELECT DISTINCT user_id FROM role_assignments
              WHERE org_role_id = v_role AND revoked_at IS NULL LOOP
        PERFORM sync_touch_teller(u);
    END LOOP;
    RETURN NULL;
END $$;
CREATE TRIGGER sync_emit AFTER INSERT OR UPDATE OR DELETE ON org_role_grants
    FOR EACH ROW EXECUTE FUNCTION sync_emit_org_role_grants();

CREATE FUNCTION sync_emit_org_capability_policy() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    v_org uuid := CASE WHEN TG_OP = 'DELETE' THEN OLD.org_id ELSE NEW.org_id END;
    u uuid;
BEGIN
    FOR u IN SELECT id FROM users WHERE org_id = v_org AND deleted_at IS NULL LOOP
        PERFORM sync_touch_teller(u);
    END LOOP;
    RETURN NULL;
END $$;
CREATE TRIGGER sync_emit AFTER INSERT OR UPDATE OR DELETE ON org_capability_policy
    FOR EACH ROW EXECUTE FUNCTION sync_emit_org_capability_policy();

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
        ('org_capability_policy',      ARRAY['teller'])
    $$;
