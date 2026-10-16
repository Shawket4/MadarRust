-- DB architecture Phase 2.1 + 2.2 (DB_ARCHITECTURE_AUDIT_2026-10-16 §8 Phase 2).
--
-- 2.1 FLAT RLS. Every tenant policy that reached its org through a parent
--     (`branch_id IN (SELECT id FROM branches)`, `EXISTS (SELECT 1 FROM parent
--     …)`) becomes the flat `org_id = <app org>`: one comparison on the row's
--     own column, no sub-select per row, the same shape as every other tenant
--     table.
--
--     a. The 14 tenant tables that still had no `org_id` get one first, exactly
--        the Phase 1.2 way (nullable column, BEFORE INSERT `fill_org_id`
--        trigger from the parent so no insert code changes, backfill with the
--        table's UPDATE triggers off for that one statement, composite FK
--        (org_id, parent) → parent (org_id, id) copying the existing FK's
--        ON DELETE). Being new, they go straight to NOT NULL.
--     b. Per table, in this one transaction: the set of rows `madar_app` sees
--        for EVERY org is recorded (count + md5 of the row identities),
--        the policy is swapped, and the set is recorded again (row identity =
--        ctid: nothing writes a row between the two reads, so it is stable and
--        far cheaper than hashing whole rows). Any difference
--        raises and the whole migration rolls back: the swap cannot change
--        what any tenant sees. Before that, as the owner, every row's org_id
--        is checked against the org its parent chain gives (the reason the two
--        policies are equivalent); a mismatch raises too.
--
--     The predicate is `(org_id = <app org>) IS TRUE`, not a bare `=`. Same
--     rows (an unset org compares NULL, which is not TRUE; '' still fails the
--     uuid cast, so no org stays an error), but the planner cannot turn it into
--     an index condition. A bare `org_id = $initplan` gets the generic
--     1/n_distinct estimate, which is far too low for a big tenant: on the 30x
--     copy the planner then BitmapAnd-ed the whole org's index into every
--     branch-filtered read (ledger window 0.5 → 4.7 ms, branch sales 8.7 →
--     11.7 ms) and hashed all of the org's tills for a 213-order join. With
--     IS TRUE every hot query is at or below the old policy (add-on revenue
--     2,180 → 374 ms, top items 1,149 → 230 ms, orders summary 21 → 13 ms).
--     The old parent-chain predicates were never indexable either.
--
--     NOT flattened here, on purpose (other branches own them): `permissions`
--     and `user_branch_assignments` (authz, Phase 4) and
--     `sync_feed_watermarks` (sync, Phase 6). The policy name stays
--     `tenant_isolation`.
--
-- 2.2 `org_id SET NOT NULL` on the 22 Phase 1 children. Each has a VALIDATED
--     `<t>_org_id_present CHECK (org_id IS NOT NULL)`, so Postgres proves the
--     column has no NULL from that constraint and skips the table scan; the
--     CHECK is then redundant and dropped. A table whose CHECK is somehow not
--     validated is skipped with a WARNING (scripts/db_phase2_check.sql lists
--     it) rather than failing the boot.
--
-- Nothing here moves or deletes a row. Measured on the prod copy (113 MB,
-- 2026-10-16): see the commit message for the apply time.
SET LOCAL lock_timeout = '15s';

-- ── helpers ─────────────────────────────────────────────────────────────────

-- The org a tenant session sees, as the existing flat policies spell it.
-- (Unset → NULL → no rows; '' → cast error: unchanged fail-closed behaviour.)
--
-- Per-org visible set of `t` as madar_app: '<org>=<count>:<md5 of ctids>'.
CREATE FUNCTION pg_temp.p2_visible(t text) RETURNS text[]
LANGUAGE plpgsql AS $fn$
DECLARE
    o   uuid;
    r   text;
    acc text[] := '{}';
BEGIN
    -- A session with row_security = off (pg_dump style, some test harnesses)
    -- would get an error instead of the tenant's rows.
    PERFORM set_config('row_security', 'on', true);
    FOR o IN SELECT id FROM organizations ORDER BY id LOOP
        PERFORM set_config('app.org_id', o::text, true);
        EXECUTE 'SET LOCAL ROLE madar_app';
        EXECUTE format(
            'SELECT count(*) || '':'' || md5(coalesce(string_agg(k, '','' ORDER BY k), '''')) '
            '  FROM (SELECT x.ctid::text AS k FROM %I x) s', t)
          INTO r;
        EXECUTE 'RESET ROLE';
        acc := acc || (o::text || '=' || r);
    END LOOP;
    RETURN acc;
END
$fn$;

-- Run the madar_app visible-set check here? It reads each table once per org
-- while this transaction holds its locks, so only while that is cheap: a
-- role that may SET ROLE madar_app, and at most 25 orgs (prod has 4: 0.7 s
-- for the whole file). Beyond that it is skipped with a NOTICE: the
-- owner-side parent-org proof (one pass per table) still runs, and
-- scripts/db_rls_visibility.sql makes the full per-org comparison on a copy.
CREATE FUNCTION pg_temp.p2_can_set_role() RETURNS boolean
LANGUAGE sql AS $$
    SELECT pg_has_role(current_user, 'madar_app', 'MEMBER')
       AND (SELECT count(*) FROM organizations) <= 25
$$;

-- Owner-side proof of equivalence: rows whose org_id is not the org of the
-- parent the OLD policy reached through. `parent_org_sql` yields that org for
-- alias `c`.
CREATE FUNCTION pg_temp.p2_assert_parent_org(t text, parent_org_sql text)
RETURNS void LANGUAGE plpgsql AS $fn$
DECLARE n bigint;
BEGIN
    EXECUTE format('SELECT count(*) FROM %I c WHERE c.org_id IS DISTINCT FROM (%s)',
                   t, parent_org_sql) INTO n;
    IF n > 0 THEN
        RAISE EXCEPTION 'p2 flat RLS: % rows of % have an org_id other than their parent''s', n, t;
    END IF;
END
$fn$;

-- The swap itself, with the visible-set identity check around it.
CREATE FUNCTION pg_temp.p2_flatten(t text, parent_org_sql text)
RETURNS void LANGUAGE plpgsql AS $fn$
DECLARE
    before text[];
    after  text[];
    check_role boolean := pg_temp.p2_can_set_role();
BEGIN
    PERFORM pg_temp.p2_assert_parent_org(t, parent_org_sql);
    IF check_role THEN
        before := pg_temp.p2_visible(t);
    END IF;
    EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON %I', t);
    EXECUTE format($p$CREATE POLICY tenant_isolation ON %I FOR ALL
        USING ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE)
        WITH CHECK ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE)$p$, t);
    IF check_role THEN
        after := pg_temp.p2_visible(t);
        IF before IS DISTINCT FROM after THEN
            RAISE EXCEPTION 'p2 flat RLS: visible rows of % changed: before % after %',
                t, before, after;
        END IF;
    ELSE
        RAISE NOTICE 'p2 flat RLS: % swapped; parent-org proof passed, per-org madar_app check '
                     'skipped (role or org count; run scripts/db_rls_visibility.sql on a copy)', t;
    END IF;
END
$fn$;

-- Phase 1.2's expand, for one more table, ending at NOT NULL.
--   fill_sql: the plpgsql expression that yields the org for NEW.
--   back_sql: the same org for alias `c` (used by the backfill).
CREATE FUNCTION pg_temp.p2_org_id_expand(t text, fill_sql text, back_sql text)
RETURNS void LANGUAGE plpgsql AS $fn$
DECLARE
    fn   text := 'fill_org_id_' || t;
    trg  record;
    off  text[] := '{}';
    mode text[] := '{}';
    i    int;
BEGIN
    EXECUTE format('ALTER TABLE %I ADD COLUMN IF NOT EXISTS org_id uuid', t);

    EXECUTE format($f$
        CREATE OR REPLACE FUNCTION %I() RETURNS trigger LANGUAGE plpgsql AS $body$
        BEGIN
            IF NEW.org_id IS NULL THEN
                NEW.org_id := (%s);
                IF NEW.org_id IS NULL THEN
                    RAISE EXCEPTION '%s: no organization for the new row'
                        USING ERRCODE = 'foreign_key_violation';
                END IF;
            END IF;
            RETURN NEW;
        END
        $body$
    $f$, fn, fill_sql, t);
    EXECUTE format('DROP TRIGGER IF EXISTS fill_org_id ON %I', t);
    EXECUTE format(
        'CREATE TRIGGER fill_org_id BEFORE INSERT ON %I FOR EACH ROW EXECUTE FUNCTION %I()',
        t, fn);

    FOR trg IN
        SELECT tgname, tgenabled FROM pg_trigger
         WHERE tgrelid = format('%I', t)::regclass AND NOT tgisinternal
           AND tgenabled <> 'D' AND (tgtype & 16) <> 0      -- TRIGGER_TYPE_UPDATE
         ORDER BY tgname
    LOOP
        EXECUTE format('ALTER TABLE %I DISABLE TRIGGER %I', t, trg.tgname);
        off  := off  || trg.tgname::text;
        mode := mode || trg.tgenabled::text;
    END LOOP;

    EXECUTE format('UPDATE %I c SET org_id = (%s) WHERE c.org_id IS NULL', t, back_sql);

    FOR i IN 1 .. coalesce(array_length(off, 1), 0) LOOP
        EXECUTE format('ALTER TABLE %I ENABLE %s TRIGGER %I', t,
            CASE mode[i] WHEN 'A' THEN 'ALWAYS' WHEN 'R' THEN 'REPLICA' ELSE '' END, off[i]);
    END LOOP;

    -- A row whose parent is gone has no org: that is not a tenant row anyone
    -- can see today either (the old policy needs a visible parent). It is
    -- KEPT, never deleted, but would block NOT NULL: stop and say so.
    EXECUTE format('ALTER TABLE %I ALTER COLUMN org_id SET NOT NULL', t);
END
$fn$;

-- Composite tenant FK (org_id, col) → parent (org_id, id), with the ON DELETE
-- of the existing single-column FK on `col`.
CREATE FUNCTION pg_temp.p2_tenant_fk(t text, col text, parent text)
RETURNS void LANGUAGE plpgsql AS $fn$
DECLARE
    con text := t || '_org_' || col || '_fk';
    del "char";
BEGIN
    IF EXISTS (SELECT 1 FROM pg_constraint
                WHERE conrelid = format('%I', t)::regclass AND conname = con) THEN
        RETURN;
    END IF;
    SELECT k.confdeltype INTO del
      FROM pg_constraint k
     WHERE k.conrelid = format('%I', t)::regclass AND k.contype = 'f'
       AND k.confrelid = format('%I', parent)::regclass
       AND k.conkey = ARRAY[(SELECT attnum FROM pg_attribute
                              WHERE attrelid = format('%I', t)::regclass AND attname = col)]
     LIMIT 1;
    EXECUTE format('ALTER TABLE %I ADD CONSTRAINT %I FOREIGN KEY (org_id, %I) '
                   'REFERENCES %I (org_id, id) ON DELETE %s NOT VALID',
        t, con, col, parent,
        CASE del WHEN 'c' THEN 'CASCADE' WHEN 'n' THEN 'SET NULL (' || quote_ident(col) || ')'
                 WHEN 'r' THEN 'RESTRICT' WHEN 'd' THEN 'SET DEFAULT' ELSE 'NO ACTION' END);
    EXECUTE format('ALTER TABLE %I VALIDATE CONSTRAINT %I', t, con);
END
$fn$;

-- ── 2.1a org_id on the remaining tenant tables ──────────────────────────────
SELECT pg_temp.p2_org_id_expand(x.t,
           format('SELECT p.org_id FROM %I p WHERE p.id = NEW.%I', x.parent, x.col),
           format('SELECT p.org_id FROM %I p WHERE p.id = c.%I', x.parent, x.col)),
       pg_temp.p2_tenant_fk(x.t, x.col, x.parent)
  FROM (VALUES
    ('asset_bundle_dirty',        'branch_id',         'branches'),
    ('branch_delivery_settings',  'branch_id',         'branches'),
    ('category_station_routes',   'branch_id',         'branches'),
    ('delivery_ref_counters',     'branch_id',         'branches'),
    ('delivery_zones',            'branch_id',         'branches'),
    ('menu_item_station_routes',  'branch_id',         'branches'),
    ('order_ref_counters',        'branch_id',         'branches'),
    ('ticket_ref_counters',       'branch_id',         'branches'),
    ('ingredient_cost_history',   'org_ingredient_id', 'org_ingredients'),
    ('menu_item_modifier_groups', 'menu_item_id',      'menu_items'),
    ('menu_item_price_epochs',    'menu_item_id',      'menu_items'),
    ('till_reconciliations',      'till_id',           'tills'),
    ('till_spot_views',           'till_id',           'tills')
  ) AS x(t, col, parent);

-- menu_price_overrides: the branch's org when scoped to a branch, else the
-- org of the size / option it prices (scope 'channel' has no branch).
SELECT pg_temp.p2_org_id_expand('menu_price_overrides',
    $$CASE WHEN NEW.branch_id IS NOT NULL
             THEN (SELECT b.org_id FROM branches b WHERE b.id = NEW.branch_id)
           WHEN NEW.target_type = 'menu_item_size'
             THEN (SELECT s.org_id FROM menu_item_sizes s WHERE s.id = NEW.target_id)
           ELSE (SELECT o.org_id FROM modifier_options o WHERE o.id = NEW.target_id) END$$,
    $$CASE WHEN c.branch_id IS NOT NULL
             THEN (SELECT b.org_id FROM branches b WHERE b.id = c.branch_id)
           WHEN c.target_type = 'menu_item_size'
             THEN (SELECT s.org_id FROM menu_item_sizes s WHERE s.id = c.target_id)
           ELSE (SELECT o.org_id FROM modifier_options o WHERE o.id = c.target_id) END$$);
SELECT pg_temp.p2_tenant_fk('menu_price_overrides', 'branch_id', 'branches');

-- ── 2.1b the policy swap, with its identity checks ──────────────────────────
-- (table, the org its OLD policy reached it through, for alias c)
SELECT pg_temp.p2_flatten(x.t, x.via)
  FROM (VALUES
    -- reached through branches
    ('orders',                    'SELECT b.org_id FROM branches b WHERE b.id = c.branch_id'),
    ('tills',                     'SELECT b.org_id FROM branches b WHERE b.id = c.branch_id'),
    ('inventory_movements',       'SELECT b.org_id FROM branches b WHERE b.id = c.branch_id'),
    ('branch_stock',              'SELECT b.org_id FROM branches b WHERE b.id = c.branch_id'),
    ('sync_changes',              'SELECT b.org_id FROM branches b WHERE b.id = c.branch_id'),
    ('asset_bundle_dirty',        'SELECT b.org_id FROM branches b WHERE b.id = c.branch_id'),
    ('branch_delivery_settings',  'SELECT b.org_id FROM branches b WHERE b.id = c.branch_id'),
    ('category_station_routes',   'SELECT b.org_id FROM branches b WHERE b.id = c.branch_id'),
    ('delivery_ref_counters',     'SELECT b.org_id FROM branches b WHERE b.id = c.branch_id'),
    ('delivery_zones',            'SELECT b.org_id FROM branches b WHERE b.id = c.branch_id'),
    ('menu_item_station_routes',  'SELECT b.org_id FROM branches b WHERE b.id = c.branch_id'),
    ('order_ref_counters',        'SELECT b.org_id FROM branches b WHERE b.id = c.branch_id'),
    ('ticket_ref_counters',       'SELECT b.org_id FROM branches b WHERE b.id = c.branch_id'),
    -- reached through a parent row
    ('order_items',               'SELECT p.org_id FROM orders p WHERE p.id = c.order_id'),
    ('order_payments',            'SELECT p.org_id FROM orders p WHERE p.id = c.order_id'),
    ('order_item_addons',         'SELECT p.org_id FROM order_items p WHERE p.id = c.order_item_id'),
    ('order_item_optionals',      'SELECT p.org_id FROM order_items p WHERE p.id = c.order_item_id'),
    ('till_cash_movements',       'SELECT p.org_id FROM tills p WHERE p.id = c.till_id'),
    ('till_reconciliations',      'SELECT p.org_id FROM tills p WHERE p.id = c.till_id'),
    ('till_spot_views',           'SELECT p.org_id FROM tills p WHERE p.id = c.till_id'),
    ('open_ticket_rounds',        'SELECT p.org_id FROM open_tickets p WHERE p.id = c.open_ticket_id'),
    ('open_ticket_items',         'SELECT p.org_id FROM open_tickets p WHERE p.id = c.open_ticket_id'),
    ('kitchen_ticket_items',      'SELECT p.org_id FROM kitchen_tickets p WHERE p.id = c.kitchen_ticket_id'),
    ('menu_item_sizes',           'SELECT p.org_id FROM menu_items p WHERE p.id = c.menu_item_id'),
    ('menu_item_modifier_groups', 'SELECT p.org_id FROM menu_items p WHERE p.id = c.menu_item_id'),
    ('menu_item_price_epochs',    'SELECT p.org_id FROM menu_items p WHERE p.id = c.menu_item_id'),
    ('modifier_options',          'SELECT p.org_id FROM modifier_groups p WHERE p.id = c.group_id'),
    ('recipe_lines',              'SELECT p.org_id FROM org_ingredients p WHERE p.id = c.ingredient_id'),
    ('ingredient_cost_history',   'SELECT p.org_id FROM org_ingredients p WHERE p.id = c.org_ingredient_id'),
    ('stocktake_items',           'SELECT p.org_id FROM stocktakes p WHERE p.id = c.stocktake_id'),
    ('purchase_order_lines',      'SELECT p.org_id FROM purchase_orders p WHERE p.id = c.purchase_order_id'),
    ('goods_receipt_lines',       'SELECT p.org_id FROM goods_receipts p WHERE p.id = c.goods_receipt_id'),
    ('stock_transfer_lines',      'SELECT p.org_id FROM stock_transfers p WHERE p.id = c.transfer_id'),
    ('packaging_rule_lines',      'SELECT p.org_id FROM packaging_rules p WHERE p.id = c.rule_id'),
    ('recipe_base_lines',         'SELECT p.org_id FROM recipe_bases p WHERE p.id = c.base_id'),
    ('menu_price_overrides', $$CASE WHEN c.branch_id IS NOT NULL
             THEN (SELECT b.org_id FROM branches b WHERE b.id = c.branch_id)
           WHEN c.target_type = 'menu_item_size'
             THEN (SELECT s.org_id FROM menu_item_sizes s WHERE s.id = c.target_id)
           ELSE (SELECT o.org_id FROM modifier_options o WHERE o.id = c.target_id) END$$)
  ) AS x(t, via);

-- ── 2.2 NOT NULL on the Phase 1 children; their CHECK is then redundant ────
DO $p22$
DECLARE
    r record;
BEGIN
    FOR r IN
        SELECT k.conrelid::regclass::text AS t, k.conname, k.convalidated
          FROM pg_constraint k
         WHERE k.contype = 'c' AND k.conname = k.conrelid::regclass::text || '_org_id_present'
         ORDER BY 1
    LOOP
        IF NOT r.convalidated THEN
            BEGIN
                EXECUTE format('ALTER TABLE %I VALIDATE CONSTRAINT %I', r.t, r.conname);
            EXCEPTION WHEN check_violation THEN
                RAISE WARNING 'p2.2: % still has rows without org_id; NOT NULL skipped', r.t;
                CONTINUE;
            END;
        END IF;
        -- The validated CHECK proves no NULL: this is a catalog change, no scan.
        EXECUTE format('ALTER TABLE %I ALTER COLUMN org_id SET NOT NULL', r.t);
        EXECUTE format('ALTER TABLE %I DROP CONSTRAINT %I', r.t, r.conname);
    END LOOP;
END
$p22$;

DROP FUNCTION pg_temp.p2_flatten(text, text);
DROP FUNCTION pg_temp.p2_assert_parent_org(text, text);
DROP FUNCTION pg_temp.p2_visible(text);
DROP FUNCTION pg_temp.p2_can_set_role();
DROP FUNCTION pg_temp.p2_org_id_expand(text, text, text);
DROP FUNCTION pg_temp.p2_tenant_fk(text, text, text);
