-- DB architecture Phase 1.2 (DB_ARCHITECTURE_AUDIT_2026-10-16 §8): `org_id` on
-- the tenant children that reach their org only through a parent today.
--
-- ADDITIVE ONLY. No DROP, no RENAME, no type change, no policy change. The RLS
-- policies are untouched (the flat `org_id = app_org()` swap is Phase 2).
--
-- Per table, in parent-before-child order, all in this ONE transaction:
--   1. ADD COLUMN org_id uuid            nullable, no default: catalog-only, no rewrite
--   2. BEFORE INSERT trigger `fill_org_id` sets NEW.org_id from the parent when
--      the insert does not name it. No insert code changes. It is in place
--      BEFORE the backfill, so no row inserted from here on can slip through.
--   3. Backfill: UPDATE … SET org_id = parent.org_id WHERE org_id IS NULL.
--      Idempotent (touches NULLs only). The table's own UPDATE triggers are
--      switched off for this one statement and switched back on right after:
--      they would otherwise re-stamp `updated_at`, re-emit every row into the
--      POS changefeed (every till would re-pull its whole history) and bump
--      `catalog_revision` (every till would re-download its menu). The
--      ALTER TABLE that disables them holds a lock that blocks every other
--      writer until COMMIT, so no other session ever runs with them off.
--   4. CHECK (org_id IS NOT NULL) NOT VALID — enforces every new row.
--   5. VALIDATE. Should a row somehow still be NULL the constraint stays
--      NOT VALID (still enforcing new rows) and a WARNING is raised instead of
--      failing the boot; scripts/db_phase1_check.sql reports it.
--
-- Prod is ~80 MB (2026-10): measured on a copy of it, this whole file applies
-- in well under a second. Owner decision (coordinator, 2026-10-16): backfill
-- inside the migration rather than a background job.
--
-- Parent paths (every column below is NOT NULL, every parent org_id NOT NULL):
--   level 1  orders, tills, inventory_movements, branch_stock, sync_changes  ← branches.branch_id
--   level 2  order_items, order_payments                   ← orders
--            till_cash_movements                           ← tills
--            open_ticket_rounds, open_ticket_items         ← open_tickets
--            kitchen_ticket_items                          ← kitchen_tickets
--            menu_item_sizes                               ← menu_items
--            modifier_options                              ← modifier_groups
--            recipe_lines                                  ← org_ingredients (ingredient_id)
--            stocktake_items                               ← stocktakes
--            purchase_order_lines                          ← purchase_orders
--            goods_receipt_lines                           ← goods_receipts
--            stock_transfer_lines                          ← stock_transfers (transfer_id)
--            packaging_rule_lines                          ← packaging_rules (rule_id)
--            recipe_base_lines                             ← recipe_bases (base_id)
--   level 3  order_item_addons, order_item_optionals       ← order_items
-- (order_refunds and order_refund_lines already carry org_id.)

-- Fail fast rather than queue behind a long transaction while every writer
-- queues behind us; the boot then fails, the container restarts, and this
-- transaction (all IF NOT EXISTS / NULL-only) simply runs again.
SET LOCAL lock_timeout = '15s';

CREATE OR REPLACE FUNCTION pg_temp.p1_org_id_expand(t text, col text, parent text)
RETURNS void LANGUAGE plpgsql AS $fn$
DECLARE
    fn   text := 'fill_org_id_' || t;
    con  text := t || '_org_id_present';
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
                SELECT p.org_id INTO NEW.org_id FROM %I p WHERE p.id = NEW.%I;
                IF NEW.org_id IS NULL THEN
                    RAISE EXCEPTION '%s: no organization for %s %%', NEW.%I
                        USING ERRCODE = 'foreign_key_violation';
                END IF;
            END IF;
            RETURN NEW;
        END
        $body$
    $f$, fn, parent, col, t, col, col);
    EXECUTE format('DROP TRIGGER IF EXISTS fill_org_id ON %I', t);
    EXECUTE format(
        'CREATE TRIGGER fill_org_id BEFORE INSERT ON %I FOR EACH ROW EXECUTE FUNCTION %I()',
        t, fn);

    -- The backfill. Only the user triggers that fire on UPDATE and are on now.
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

    EXECUTE format(
        'UPDATE %I c SET org_id = p.org_id FROM %I p WHERE p.id = c.%I AND c.org_id IS NULL',
        t, parent, col);

    FOR i IN 1 .. coalesce(array_length(off, 1), 0) LOOP
        EXECUTE format('ALTER TABLE %I ENABLE %s TRIGGER %I', t,
            CASE mode[i] WHEN 'A' THEN 'ALWAYS' WHEN 'R' THEN 'REPLICA' ELSE '' END, off[i]);
    END LOOP;

    IF NOT EXISTS (SELECT 1 FROM pg_constraint
                    WHERE conrelid = format('%I', t)::regclass AND conname = con) THEN
        EXECUTE format('ALTER TABLE %I ADD CONSTRAINT %I CHECK (org_id IS NOT NULL) NOT VALID',
                       t, con);
    END IF;
    BEGIN
        EXECUTE format('ALTER TABLE %I VALIDATE CONSTRAINT %I', t, con);
    EXCEPTION WHEN check_violation THEN
        RAISE WARNING '%: rows without org_id remain; % left NOT VALID (new rows are still checked)',
            t, con;
    END;
END
$fn$;

-- level 1: from the branch
SELECT pg_temp.p1_org_id_expand('orders',               'branch_id',         'branches');
SELECT pg_temp.p1_org_id_expand('tills',                'branch_id',         'branches');
SELECT pg_temp.p1_org_id_expand('inventory_movements',  'branch_id',         'branches');
SELECT pg_temp.p1_org_id_expand('branch_stock',         'branch_id',         'branches');
SELECT pg_temp.p1_org_id_expand('sync_changes',         'branch_id',         'branches');
-- level 2
SELECT pg_temp.p1_org_id_expand('order_items',          'order_id',          'orders');
SELECT pg_temp.p1_org_id_expand('order_payments',       'order_id',          'orders');
SELECT pg_temp.p1_org_id_expand('till_cash_movements',  'till_id',           'tills');
SELECT pg_temp.p1_org_id_expand('open_ticket_rounds',   'open_ticket_id',    'open_tickets');
SELECT pg_temp.p1_org_id_expand('open_ticket_items',    'open_ticket_id',    'open_tickets');
SELECT pg_temp.p1_org_id_expand('kitchen_ticket_items', 'kitchen_ticket_id', 'kitchen_tickets');
SELECT pg_temp.p1_org_id_expand('menu_item_sizes',      'menu_item_id',      'menu_items');
SELECT pg_temp.p1_org_id_expand('modifier_options',     'group_id',          'modifier_groups');
SELECT pg_temp.p1_org_id_expand('recipe_lines',         'ingredient_id',     'org_ingredients');
SELECT pg_temp.p1_org_id_expand('stocktake_items',      'stocktake_id',      'stocktakes');
SELECT pg_temp.p1_org_id_expand('purchase_order_lines', 'purchase_order_id', 'purchase_orders');
SELECT pg_temp.p1_org_id_expand('goods_receipt_lines',  'goods_receipt_id',  'goods_receipts');
SELECT pg_temp.p1_org_id_expand('stock_transfer_lines', 'transfer_id',       'stock_transfers');
SELECT pg_temp.p1_org_id_expand('packaging_rule_lines', 'rule_id',           'packaging_rules');
SELECT pg_temp.p1_org_id_expand('recipe_base_lines',    'base_id',           'recipe_bases');
-- level 3
SELECT pg_temp.p1_org_id_expand('order_item_addons',    'order_item_id',     'order_items');
SELECT pg_temp.p1_org_id_expand('order_item_optionals', 'order_item_id',     'order_items');

DROP FUNCTION pg_temp.p1_org_id_expand(text, text, text);
