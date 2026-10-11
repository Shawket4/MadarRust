-- Provisioning an org cost time proportional to EVERY org on the box, which a
-- free self-signup tier turns into a slope (load test 2026-10-11, ~2000 orgs):
--
-- 1. Each of a new org's ~330 `org_role_grants` rows fires
--    `sync_emit_org_role_grants`, which looks up `role_assignments` by
--    `org_role_id`. The only index holding that column leads with `user_id`,
--    so each lookup read the whole table: ~666k rows per signup.
--
-- 2. The new branch's backfill (`sync_backfill_branch`) asked
--    `sync_live_rows()` for EVERY branch's live rows — every org's menu, every
--    order ever taken — and then kept the new branch's. A SECURITY DEFINER
--    function with a SET clause is never inlined, so the filter could not be
--    pushed into it: 118 ms with 2000 one-branch orgs, growing with all data.
--
-- `sync_live_rows_for_branch` is the same set restricted to one branch at
-- every arm. `sync_live_rows()` stays the source of truth for the whole-feed
-- reconcilers; the test `sync_live_rows_for_branch_matches_the_whole_feed`
-- (tests/signup_scale.rs) fails if the two ever list different types.

CREATE INDEX IF NOT EXISTS role_assignments_role
    ON role_assignments (org_role_id) WHERE revoked_at IS NULL;

CREATE OR REPLACE FUNCTION sync_live_rows_for_branch(p_branch uuid)
    RETURNS TABLE (branch_id uuid, type text, entity_id uuid)
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public
    AS $$
    WITH ob AS (SELECT id AS branch_id, org_id FROM branches WHERE deleted_at IS NULL AND id = p_branch)
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
    SELECT x.id, 'branch_settings', x.id FROM branches x WHERE x.id = p_branch AND sync_live_branch_settings(x)
    UNION ALL
    SELECT x.branch_id, 'device', x.id FROM devices x WHERE x.branch_id = p_branch AND sync_live_device(x)
    UNION ALL
    SELECT ob.branch_id, 'teller', x.id FROM users x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_teller(x)
    UNION ALL
    SELECT x.branch_id, 'floor_section', x.id FROM floor_sections x WHERE x.branch_id = p_branch
    UNION ALL
    SELECT x.branch_id, 'floor_table', x.id FROM branch_tables x WHERE x.branch_id = p_branch AND sync_live_floor_table(x)
    UNION ALL
    SELECT x.branch_id, 'table_occupancy', x.id FROM table_occupancies x WHERE x.branch_id = p_branch AND sync_live_table_occupancy(x)
    UNION ALL
    SELECT x.branch_id, 'table_transfer', x.id FROM table_transfer_requests x WHERE x.branch_id = p_branch AND sync_live_table_transfer(x)
    UNION ALL
    SELECT x.branch_id, 'open_ticket', x.id FROM open_tickets x WHERE x.branch_id = p_branch AND sync_live_open_ticket(x)
    UNION ALL
    SELECT x.branch_id, 'kitchen_ticket', x.id FROM kitchen_tickets x WHERE x.branch_id = p_branch AND sync_live_kitchen_ticket(x)
    UNION ALL
    SELECT x.branch_id, 'delivery', x.id FROM delivery_orders x WHERE x.branch_id = p_branch AND sync_live_delivery(x)
    UNION ALL
    SELECT x.branch_id, 'booking', x.id FROM bookings x WHERE x.branch_id = p_branch AND sync_live_booking(x)
    UNION ALL
    SELECT x.branch_id, 'till', x.id FROM tills x WHERE x.branch_id = p_branch
    UNION ALL
    SELECT t.branch_id, 'cash_movement', x.id FROM till_cash_movements x JOIN tills t ON t.id = x.till_id WHERE t.branch_id = p_branch
    UNION ALL
    SELECT x.branch_id, 'order', x.id FROM orders x WHERE x.branch_id = p_branch
    UNION ALL
    SELECT x.branch_id, 'refund', x.id FROM order_refunds x WHERE x.branch_id = p_branch
    UNION ALL
    SELECT ob.branch_id, 'addon_item', x.id FROM addon_items x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_addon_item(x.id)
    UNION ALL
    SELECT ob.branch_id, 'customer', x.id FROM customers x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_customer(x)
    UNION ALL
    SELECT ob.branch_id, 'deal_rule', x.id FROM deal_rules x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_deal_rule(x)
    $$;

CREATE OR REPLACE FUNCTION sync_backfill_branch(p_branch uuid) RETURNS integer
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    n integer;
BEGIN
    IF p_branch IS NULL THEN
        RETURN 0;
    END IF;
    PERFORM pg_advisory_xact_lock_shared(sync_branch_lock_key(p_branch));
    INSERT INTO sync_changes (branch_id, type, entity_id, op)
    SELECT l.branch_id, l.type, l.entity_id, 'upsert'
      FROM sync_live_rows_for_branch(p_branch) l
     WHERE NOT EXISTS (SELECT 1 FROM sync_changes c
                        WHERE c.branch_id = l.branch_id
                          AND c.type = l.type
                          AND c.entity_id = l.entity_id)
    ON CONFLICT (branch_id, type, entity_id) DO NOTHING;
    GET DIAGNOSTICS n = ROW_COUNT;
    IF n > 0 THEN
        PERFORM pg_notify('sync_changes', p_branch::text);
    END IF;
    RETURN n;
END;
$$;
