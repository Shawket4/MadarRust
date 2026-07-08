-- Row-Level Security, part 2 of 2: enable RLS + tenant policies on every table.
--
-- Policies bind ONLY the `madar_app` role (src/db.rs tenant pools). The table
-- owner — dev/CI superusers and the prod `madar` role — bypasses them (no
-- FORCE), which is the sanctioned path for migrations, seeder, super-admin
-- endpoints, public unauthenticated flows, and cross-tenant jobs.
--
-- The tenant identity is `current_setting('app.org_id', true)`, set once per
-- connection from verified JWT claims. Unset GUC → NULL → every comparison
-- fails → deny-by-default (a scoped connection can never "fall open").
--
-- Rather than 80 hand-written statements (which silently rot when the schema
-- changes — an earlier draft referenced tables a later migration had dropped),
-- this generates policies from the live catalog:
--
--   * table has `org_id`      → USING (org_id = <guc>)
--   * else table has `branch_id` → USING (branch_id IN (SELECT id FROM branches))
--     — branches' own policy scopes that subquery to the org; policies compose.
--   * else (a child/junction table) → a correlated EXISTS against its owning
--     parent, taken from the explicit CHILD_MAP below. The parent's policy
--     supplies the tenant check; scoping recurses up to an org/branch root.
--
-- FOR ALL + USING only: WITH CHECK defaults to USING, so INSERT/UPDATE cannot
-- place a row in — or repoint an FK at — another org.
--
-- A completeness assertion at the end RAISEs if any base table was left without
-- row security, so a NEW table added by a future migration fails loudly here
-- (and in the src/rls_tests.rs coverage test) until it is classified — it can
-- never quietly ship world-readable.

DO $rls$
DECLARE
    t      text;
    m      record;
    missing text;
BEGIN
    -- ── Org-rooted tables (direct org_id equality) ───────────────────────────
    FOR t IN
        SELECT table_name FROM information_schema.columns
        WHERE table_schema = 'public' AND column_name = 'org_id'
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format(
            'CREATE POLICY tenant_isolation ON %I FOR ALL '
            'USING (org_id = (SELECT current_setting(''app.org_id'', true)::uuid))', t);
    END LOOP;

    -- ── Branch-rooted tables (org reached via the branches policy) ───────────
    -- Only NOT NULL branch_id qualifies: a nullable branch_id row (e.g. an
    -- org-level `ingredient_cost_history` cost) would be `NULL IN (…)` → never
    -- true → invisible AND un-insertable. Such tables are routed through the
    -- child map below on a NOT NULL FK instead; any future nullable-branch_id
    -- table that is *not* mapped trips the completeness gate.
    FOR t IN
        SELECT c.table_name FROM information_schema.columns c
        WHERE c.table_schema = 'public' AND c.column_name = 'branch_id'
          AND c.is_nullable = 'NO'
          AND NOT EXISTS (
              SELECT 1 FROM information_schema.columns o
              WHERE o.table_schema = 'public' AND o.table_name = c.table_name
                AND o.column_name = 'org_id')
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format(
            'CREATE POLICY tenant_isolation ON %I FOR ALL '
            'USING (branch_id IN (SELECT b.id FROM branches b))', t);
    END LOOP;

    -- ── Child / junction tables (scoped through their owning parent) ──────────
    -- Each entry names the ONE foreign key that defines the row's tenancy. It
    -- is deliberately explicit: which FK owns a junction row is a semantic call
    -- (e.g. order_items belongs to its `order`, not to the menu_item it names),
    -- and picking wrong would either leak rows or hide legitimate ones.
    FOR m IN
        SELECT * FROM (VALUES
            ('addon_item_ingredients',                'addon_items',    'addon_item_id'),
            ('booking_nudges',                        'bookings',       'booking_id'),
            ('booking_tables',                        'bookings',       'booking_id'),
            ('bundle_components',                     'bundles',        'bundle_id'),
            ('bundle_price_epochs',                   'bundles',        'bundle_id'),
            ('goods_receipt_lines',                   'goods_receipts', 'goods_receipt_id'),
            -- branch_id is nullable here (NULL = org-level standard cost), so it
            -- is scoped through its NOT NULL org_ingredient parent, not branch_id.
            ('ingredient_cost_history',               'org_ingredients','org_ingredient_id'),
            ('item_sizes',                            'menu_items',     'menu_item_id'),
            ('kitchen_ticket_items',                  'kitchen_tickets','kitchen_ticket_id'),
            ('menu_item_addon_overrides',             'menu_items',     'menu_item_id'),
            ('menu_item_addon_slots',                 'menu_items',     'menu_item_id'),
            ('menu_item_allowed_addons',              'menu_items',     'menu_item_id'),
            ('menu_item_modifier_groups',             'menu_items',     'menu_item_id'),
            ('menu_item_optional_fields',             'menu_items',     'menu_item_id'),
            ('menu_item_price_epochs',                'menu_items',     'menu_item_id'),
            ('menu_item_recipes',                     'menu_items',     'menu_item_id'),
            ('menu_item_sizes',                       'menu_items',     'menu_item_id'),
            ('modifier_options',                      'modifier_groups','group_id'),
            ('open_ticket_items',                     'open_tickets',   'open_ticket_id'),
            ('open_ticket_rounds',                    'open_tickets',   'open_ticket_id'),
            ('order_item_addons',                     'order_items',    'order_item_id'),
            ('order_item_optionals',                  'order_items',    'order_item_id'),
            ('order_items',                           'orders',         'order_id'),
            ('order_line_bundle_component_addons',    'order_items',    'order_line_id'),
            ('order_line_bundle_component_optionals', 'order_items',    'order_line_id'),
            ('order_line_bundle_components',          'order_items',    'order_line_id'),
            ('order_payments',                        'orders',         'order_id'),
            ('permissions',                           'users',          'user_id'),
            ('purchase_order_lines',                  'purchase_orders','purchase_order_id'),
            ('recipe_lines',                          'org_ingredients','ingredient_id'),
            ('shift_cash_movements',                  'shifts',         'shift_id'),
            ('stocktake_items',                       'stocktakes',     'stocktake_id')
        ) AS cm(child, parent, fk)
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', m.child);
        EXECUTE format(
            'CREATE POLICY tenant_isolation ON %I FOR ALL '
            'USING (EXISTS (SELECT 1 FROM %I p WHERE p.id = %I.%I))',
            m.child, m.parent, m.child, m.fk);
    END LOOP;

    -- ── Special cases ─────────────────────────────────────────────────────────

    -- A tenant may read + update its own org row (settings, timezone, logo),
    -- but never insert/delete orgs and never see another's.
    ALTER TABLE organizations ENABLE ROW LEVEL SECURITY;
    CREATE POLICY org_read   ON organizations FOR SELECT
        USING (id = (SELECT current_setting('app.org_id', true)::uuid));
    CREATE POLICY org_update ON organizations FOR UPDATE
        USING (id = (SELECT current_setting('app.org_id', true)::uuid));

    -- Global role→permission defaults: world-readable to tenants, writable only
    -- through the super-admin bypass pool (no write policy).
    ALTER TABLE role_permissions ENABLE ROW LEVEL SECURITY;
    CREATE POLICY global_read ON role_permissions FOR SELECT USING (true);

    -- Singleton WhatsApp pause switch: readable by the send path, written only
    -- via the super-admin bypass pool.
    ALTER TABLE whatsapp_gateway_settings ENABLE ROW LEVEL SECURITY;
    CREATE POLICY global_read ON whatsapp_gateway_settings FOR SELECT USING (true);

    -- Menu price overrides are polymorphic: `branch`/`branch_channel` scopes
    -- carry a branch_id, while a `channel` scope has branch_id NULL and anchors
    -- on target_id — which points (no FK; target_type discriminates) at either a
    -- menu_item_size or a modifier_option, both org-scoped by their own policy.
    -- One predicate covers all three scopes.
    ALTER TABLE menu_price_overrides ENABLE ROW LEVEL SECURITY;
    CREATE POLICY tenant_isolation ON menu_price_overrides FOR ALL
        USING (
            branch_id IN (SELECT b.id FROM branches b)
            OR (branch_id IS NULL AND EXISTS (
                SELECT 1 FROM menu_item_sizes s   WHERE s.id  = target_id
                UNION ALL
                SELECT 1 FROM modifier_options mo WHERE mo.id = target_id))
        );

    -- Customer OTPs are keyed by phone, not tenant, and touched exclusively by
    -- the PUBLIC delivery flow (bypass pool). RLS on with no policy = deny-all
    -- for madar_app.
    ALTER TABLE delivery_otp ENABLE ROW LEVEL SECURITY;

    -- ── Completeness gate ─────────────────────────────────────────────────────
    -- Every base table must now enforce row security. `_sqlx_migrations` is
    -- owner-only (its madar_app grants were revoked in part 1) and is the sole
    -- exemption. Anything else here is an unclassified table — fail the
    -- migration rather than ship it readable.
    SELECT string_agg(c.relname, ', ' ORDER BY c.relname) INTO missing
    FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE n.nspname = 'public' AND c.relkind = 'r'
      AND c.relname <> '_sqlx_migrations'
      AND NOT c.relrowsecurity;

    IF missing IS NOT NULL THEN
        RAISE EXCEPTION
            'RLS coverage gap — these tables have no row security: %', missing;
    END IF;
END
$rls$;
