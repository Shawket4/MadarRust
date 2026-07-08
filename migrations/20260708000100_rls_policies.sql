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
-- changes), this generates policies from the live catalog:
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
-- VIEWS: this only ever touches BASE TABLES — RLS cannot be enabled on a view
-- (SQLSTATE 42809). After the menu-unification flip, several legacy catalog
-- relations (e.g. `addon_items`, `item_sizes`) are read-only shim VIEWS over the
-- unified base tables in some environments. `information_schema.columns` lists a
-- view's columns too, so every loop below is filtered to `BASE TABLE`. A view
-- would otherwise be an RLS *bypass* (it runs as its owner, ignoring policies),
-- so the last step sets `security_invoker = true` on every tenant-exposing view
-- (PG15+) — making the underlying tables' RLS apply through the view as well.
--
-- A completeness assertion at the end RAISEs if any base table was left without
-- row security, so a NEW table added by a future migration fails loudly here
-- (and in the src/rls_tests.rs coverage test) until it is classified.

DO $rls$
DECLARE
    t       text;
    m       record;
    missing text;
BEGIN
    -- ── Org-rooted BASE TABLES (direct org_id equality) ──────────────────────
    FOR t IN
        SELECT c.table_name FROM information_schema.columns c
        JOIN information_schema.tables it
          ON it.table_schema = c.table_schema AND it.table_name = c.table_name
         AND it.table_type = 'BASE TABLE'
        WHERE c.table_schema = 'public' AND c.column_name = 'org_id'
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format(
            'CREATE POLICY tenant_isolation ON %I FOR ALL '
            'USING (org_id = (SELECT current_setting(''app.org_id'', true)::uuid))', t);
    END LOOP;

    -- ── Branch-rooted BASE TABLES (org reached via the branches policy) ──────
    -- Only NOT NULL branch_id qualifies: a nullable branch_id row would be
    -- `NULL IN (…)` → never true → invisible AND un-insertable. Those are routed
    -- through the child map below on a NOT NULL FK instead.
    FOR t IN
        SELECT c.table_name FROM information_schema.columns c
        JOIN information_schema.tables it
          ON it.table_schema = c.table_schema AND it.table_name = c.table_name
         AND it.table_type = 'BASE TABLE'
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

    -- ── Child / junction BASE TABLES (scoped through their owning parent) ─────
    -- Each entry names the ONE foreign key that defines the row's tenancy. The
    -- `BASE TABLE` filter means an entry that is a view (or absent) in this
    -- environment is skipped — its data is protected by the underlying unified
    -- table's own policy instead.
    FOR m IN
        SELECT cm.* FROM (VALUES
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
        WHERE EXISTS (
            SELECT 1 FROM information_schema.tables it
            WHERE it.table_schema = 'public' AND it.table_name = cm.child
              AND it.table_type = 'BASE TABLE')
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', m.child);
        EXECUTE format(
            'CREATE POLICY tenant_isolation ON %I FOR ALL '
            'USING (EXISTS (SELECT 1 FROM %I p WHERE p.id = %I.%I))',
            m.child, m.parent, m.child, m.fk);
    END LOOP;

    -- ── Special cases (each guarded so a view/absent relation is skipped) ─────

    -- A tenant may read + update its own org row, but never insert/delete orgs
    -- and never see another's.
    IF EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema='public' AND table_name='organizations' AND table_type='BASE TABLE') THEN
        ALTER TABLE organizations ENABLE ROW LEVEL SECURITY;
        CREATE POLICY org_read   ON organizations FOR SELECT
            USING (id = (SELECT current_setting('app.org_id', true)::uuid));
        CREATE POLICY org_update ON organizations FOR UPDATE
            USING (id = (SELECT current_setting('app.org_id', true)::uuid));
    END IF;

    -- Global role→permission defaults: world-readable to tenants, writable only
    -- through the super-admin bypass pool (no write policy).
    IF EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema='public' AND table_name='role_permissions' AND table_type='BASE TABLE') THEN
        ALTER TABLE role_permissions ENABLE ROW LEVEL SECURITY;
        CREATE POLICY global_read ON role_permissions FOR SELECT USING (true);
    END IF;

    -- Singleton WhatsApp pause switch: readable by the send path, written only
    -- via the super-admin bypass pool.
    IF EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema='public' AND table_name='whatsapp_gateway_settings' AND table_type='BASE TABLE') THEN
        ALTER TABLE whatsapp_gateway_settings ENABLE ROW LEVEL SECURITY;
        CREATE POLICY global_read ON whatsapp_gateway_settings FOR SELECT USING (true);
    END IF;

    -- Menu price overrides are polymorphic: `branch`/`branch_channel` scopes
    -- carry a branch_id, while a `channel` scope has branch_id NULL and anchors
    -- on target_id — which points (no FK; target_type discriminates) at either a
    -- menu_item_size or a modifier_option, both org-scoped by their own policy.
    IF EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema='public' AND table_name='menu_price_overrides' AND table_type='BASE TABLE') THEN
        ALTER TABLE menu_price_overrides ENABLE ROW LEVEL SECURITY;
        CREATE POLICY tenant_isolation ON menu_price_overrides FOR ALL
            USING (
                branch_id IN (SELECT b.id FROM branches b)
                OR (branch_id IS NULL AND EXISTS (
                    SELECT 1 FROM menu_item_sizes s   WHERE s.id  = target_id
                    UNION ALL
                    SELECT 1 FROM modifier_options mo WHERE mo.id = target_id))
            );
    END IF;

    -- Customer OTPs are keyed by phone, not tenant, and touched exclusively by
    -- the PUBLIC delivery flow (bypass pool). RLS on with no policy = deny-all.
    IF EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema='public' AND table_name='delivery_otp' AND table_type='BASE TABLE') THEN
        ALTER TABLE delivery_otp ENABLE ROW LEVEL SECURITY;
    END IF;

    -- ── Harden ALL views (PG15+) ──────────────────────────────────────────────
    -- A view runs with its OWNER's rights by default, so a view over a
    -- tenant table would BYPASS the policies above — a cross-tenant read vector.
    -- `security_invoker = true` makes the view run with the querying role's
    -- rights, so the underlying tables' RLS applies through it. Every view here
    -- is a shim over tenant tables (post menu-unification: addon_items,
    -- item_sizes, menu_item_recipes, the branch override views, …), and none
    -- exists to expose owner-only data — so all are hardened, not just those
    -- that happen to surface an org_id/branch_id column (child-level shims like
    -- item_sizes do not, yet still read tenant rows). Best-effort per view so an
    -- unusual/unowned view can't abort the migration; `madar_app` holds SELECT
    -- on every table (part 1), so invoker views resolve fine for it.
    IF current_setting('server_version_num')::int >= 150000 THEN
        FOR t IN
            SELECT it.table_name FROM information_schema.tables it
            WHERE it.table_schema = 'public' AND it.table_type = 'VIEW'
        LOOP
            BEGIN
                EXECUTE format('ALTER VIEW %I SET (security_invoker = true)', t);
            EXCEPTION WHEN OTHERS THEN
                RAISE NOTICE 'RLS: could not set security_invoker on view % (%)', t, SQLERRM;
            END;
        END LOOP;
    END IF;

    -- ── Completeness gate ─────────────────────────────────────────────────────
    -- Every base table must now enforce row security. `_sqlx_migrations` is
    -- owner-only (its madar_app grants were revoked in part 1) and is the sole
    -- exemption. Anything else is an unclassified table — fail the migration.
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
