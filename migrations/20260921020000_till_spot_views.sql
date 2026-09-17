-- Cash spot report views (owner design 2026-09-16 evening, item 5, as
-- corrected by the owner 2026-09-17; DEFERRED_FEATURES stream 2). A person
-- holding `till.cash_spot_check` (203) opens the FULL live till report of an
-- OPEN till (expected cash, tenders, every figure: the old X report) and may
-- print it. A person without it may open it once when someone holding it
-- types their PIN on the till. No amount is entered: counting happens only at
-- close.
--
-- This table is the AUDIT TRAIL: who viewed the spot report, when, whether it
-- was printed, and whose PIN unlocked it. Rows are client-minted
-- (offline-capable, idempotent on id; a print marks the same row). They reach
-- devices inside the `till` projection (`spot_views`), so the table re-emits
-- the till; no new feed type, old tablets are unaffected.
--
-- SOURCE TABLES (additions; machine-read by the migration tests together with
-- the earlier changefeed headers). Format:  table -> type[, type…]  (scope)
--   till_spot_views            -> till                        (branch)

-- ── Capability 203: on by default for owners and managers ───────────────────
UPDATE capabilities SET defaults = 'om' WHERE id = 203;

INSERT INTO org_role_grants (org_role_id, org_id, capability_id, source, template_version)
SELECT r.id, r.org_id, 203, 'template', 0
  FROM org_roles r
 WHERE r.is_system AND r.deleted_at IS NULL
   AND r.kind::text IN ('org_admin', 'branch_manager')
ON CONFLICT (org_role_id, capability_id) DO NOTHING;

SELECT authz_bump_epoch(o.id) FROM organizations o;

-- ── Table ───────────────────────────────────────────────────────────────────
CREATE TABLE till_spot_views (
    id                uuid PRIMARY KEY,
    till_id           uuid NOT NULL REFERENCES tills(id) ON DELETE CASCADE,
    branch_id         uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    viewed_by         uuid NOT NULL REFERENCES users(id),
    printed           boolean NOT NULL DEFAULT false,
    printed_at        timestamptz NULL,
    -- The person whose PIN unlocked this one view (NULL = the viewer held it).
    approved_by       uuid NULL REFERENCES users(id),
    approval_id       uuid NULL,
    device_id         uuid NULL,
    viewed_at         timestamptz NOT NULL,
    created_at        timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX idx_till_spot_views_till ON till_spot_views (till_id, viewed_at);
ALTER TABLE till_spot_views ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON till_spot_views
    USING (EXISTS (SELECT 1 FROM tills p WHERE p.id = till_spot_views.till_id));
GRANT SELECT, INSERT, UPDATE ON TABLE till_spot_views TO madar_app;

-- ── Feed: a spot check re-emits its till ───────────────────────────────────
CREATE FUNCTION sync_emit_till_spot_views() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_till(OLD.till_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.till_id IS DISTINCT FROM OLD.till_id) THEN
        PERFORM sync_touch_till(NEW.till_id);
    END IF;
    RETURN NULL;
END $$;
CREATE TRIGGER sync_emit AFTER INSERT OR UPDATE OR DELETE ON till_spot_views
    FOR EACH ROW EXECUTE FUNCTION sync_emit_till_spot_views();

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
        ('customers',                  ARRAY['customer']),
        ('till_spot_views',           ARRAY['till'])
    $$;
