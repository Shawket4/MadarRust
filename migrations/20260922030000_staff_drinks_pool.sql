-- The staff drinks pool (owner design, 2026-09-19).
--
-- A branch may give its own people N drinks per business day. The allowance
-- belongs to the BRANCH, not to a person: a staff drink is never attributed to
-- a staff member, and there is no "who is this for" picker. What is required
-- instead is a NOTE, in the teller's own words — which is why `note` is NOT
-- NULL with a non-blank CHECK here and not only in the handler. The database is
-- the last place the rule can be lost.
--
-- This replaces the duplicate zero-priced "… staff" menu items (DROPS §4c).
-- Those are retired separately and by hand:
-- `scripts/prod-cleanup/2026-09-retire-staff-drink-items.sql`.
--
-- An OVERSPEND is recorded, never refused: the drink was already made and the
-- sale already happened. `overspent` is the flag the review queue, the reports
-- and the Z report read.
--
-- SOURCE TABLES (additions; machine-read by the migration tests together with
-- the earlier changefeed headers). Format:  table -> type[, type…]  (scope)
--   staff_pool_settings        -> branch_settings             (branch)
--   staff_drinks               -> staff_drink                 (branch)

-- ── Settings: org-wide, overridden per branch (the loyalty_settings shape) ───
-- `branch_id IS NULL` is the org-wide default; a branch row overrides it
-- WHOLESALE, exactly as loyalty does — not a field-by-field merge.
CREATE TABLE staff_pool_settings (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id             uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id          uuid NULL REFERENCES branches(id) ON DELETE CASCADE,
    enabled            boolean NOT NULL DEFAULT false,
    -- How many staff drinks this branch may give in one business day.
    daily_allowance    integer NOT NULL DEFAULT 0 CHECK (daily_allowance >= 0),
    -- The menu items that count. EMPTY = nothing counts = the pool is off.
    -- Not a FK array by design: an item retired later must not break the
    -- settings row, and the read path resolves names live.
    eligible_item_ids  uuid[] NOT NULL DEFAULT '{}',
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now()
);

-- One row per scope. The COALESCE key is how loyalty_settings does it.
CREATE UNIQUE INDEX staff_pool_settings_scope
    ON staff_pool_settings (org_id, COALESCE(branch_id, '00000000-0000-0000-0000-000000000000'::uuid));

ALTER TABLE staff_pool_settings ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON staff_pool_settings
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE staff_pool_settings TO madar_app;

-- ── The drinks themselves ───────────────────────────────────────────────────
CREATE TABLE staff_drinks (
    -- Client-minted, so an offline record replays idempotently.
    id                  uuid PRIMARY KEY,
    org_id              uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id           uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    till_id             uuid NULL REFERENCES tills(id) ON DELETE SET NULL,
    -- The zero-priced sale it rang as. The sale stays visible in reports; it
    -- does not vanish, and the stock came off it.
    order_id            uuid NULL REFERENCES orders(id) ON DELETE SET NULL,
    menu_item_id        uuid NULL REFERENCES menu_items(id) ON DELETE SET NULL,
    -- Frozen at the moment it was rung, so a later rename or retirement of the
    -- item never rewrites history.
    item_name           text NOT NULL,
    size_label          text NULL,
    quantity            integer NOT NULL DEFAULT 1 CHECK (quantity > 0),
    -- THE REQUIRED NOTE. Who it is for and why, in the teller's words.
    -- Whitespace is not a note, on this side of the wire too.
    note                text NOT NULL CHECK (btrim(note) <> ''),
    -- The branch-local business day this falls on: the reset boundary, the
    -- same one the Z report and `service_day_bounds` draw. A date, not an
    -- instant, precisely so the pool never turns over at midnight UTC.
    business_date       date NOT NULL,
    -- The pool as it stood when the device decided. Kept so an owner can see
    -- what the till believed, beside what the server recomputed.
    allowance_at_record integer NOT NULL DEFAULT 0,
    used_before         integer NOT NULL DEFAULT 0,
    -- Past the allowance. Recorded, never blocking.
    overspent           boolean NOT NULL DEFAULT false,
    -- True when the SERVER's recount disagreed with the till's and made this
    -- an overspend the device did not think it was.
    overspent_on_replay boolean NOT NULL DEFAULT false,
    -- What the drink cost the shop, from the recipe. NULL when unknown.
    cost_minor          integer NULL,
    -- Who RANG it. Never who drank it — that is what the note is for.
    recorded_by         uuid NULL REFERENCES users(id),
    device_id           uuid NULL,
    recorded_at         timestamptz NOT NULL,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);

-- The pool count for a branch's day, and the report's natural order.
CREATE INDEX idx_staff_drinks_branch_day ON staff_drinks (branch_id, business_date, recorded_at);
CREATE INDEX idx_staff_drinks_order ON staff_drinks (order_id) WHERE order_id IS NOT NULL;
-- The owner's "what went over" query.
CREATE INDEX idx_staff_drinks_overspent ON staff_drinks (branch_id, business_date) WHERE overspent;

ALTER TABLE staff_drinks ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON staff_drinks
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT SELECT, INSERT, UPDATE ON TABLE staff_drinks TO madar_app;

-- ── Feed ────────────────────────────────────────────────────────────────────
-- The settings ride the existing `branch_settings` projection, exactly as
-- loyalty_settings does: no new type, and a tablet that has never heard of the
-- staff pool simply reads a projection it already reads.
CREATE FUNCTION sync_emit_staff_pool_settings() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN
        PERFORM sync_touch_org_branch_settings(OLD.org_id, OLD.branch_id);
    END IF;
    IF TG_OP IN ('INSERT','UPDATE') THEN
        PERFORM sync_touch_org_branch_settings(NEW.org_id, NEW.branch_id);
    END IF;
    RETURN NULL;
END $$;
CREATE TRIGGER sync_emit AFTER INSERT OR UPDATE OR DELETE ON staff_pool_settings
    FOR EACH ROW EXECUTE FUNCTION sync_emit_staff_pool_settings();

-- The drinks get their own branch-scoped type, as `customer` did: that is how
-- every till of a branch converges on the true count over the cloud. A client
-- that does not ask for the type never receives it, and an older client that
-- receives one in an incremental page ignores a type it cannot apply — the
-- same contract `customer` shipped under.
CREATE FUNCTION sync_emit_staff_drinks() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit(OLD.branch_id, 'staff_drink', OLD.id, 'delete');
    ELSE
        IF TG_OP = 'UPDATE' AND NEW.branch_id IS DISTINCT FROM OLD.branch_id THEN
            PERFORM sync_emit(OLD.branch_id, 'staff_drink', OLD.id, 'delete');
        END IF;
        PERFORM sync_emit(NEW.branch_id, 'staff_drink', NEW.id, 'upsert');
    END IF;
    RETURN NULL;
END $$;
CREATE TRIGGER sync_emit AFTER INSERT OR UPDATE OR DELETE ON staff_drinks
    FOR EACH ROW EXECUTE FUNCTION sync_emit_staff_drinks();

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
        ('till_spot_views',           ARRAY['till']),
        ('staff_pool_settings',        ARRAY['branch_settings']),
        ('staff_drinks',               ARRAY['staff_drink'])
    $$;
