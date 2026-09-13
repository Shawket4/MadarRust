-- ROLLBACK of the tills rework (NOT a sqlx migration). Reverses, in order:
--   20260914090600_asset_files_are_shared_groups_have_a_profile.sql (the asset tables are dropped whole)
--   20260914090400_assets.sql
--   20260914090500_asset_internal_tables_rls.sql
--   20260914090300_sync_changefeed.sql
--   20260914090200_till_reconciliation.sql
--   20260914090100_payment_method_availability.sql
--   20260914090000_tills_are_sales_sessions.sql
--
-- PRECONDITIONS
--   1. Stop the server (no writes during the rollback).
--   2. Run in ONE transaction:  psql -v ON_ERROR_STOP=1 -1 -f scripts/tills_rework/down.sql
--   3. The rolled-back binary must not see the five migration rows: this script
--      deletes them from _sqlx_migrations as its last step (so
--      Migrator.ignore_missing is not needed).
--
-- LOSSY BY NECESSITY (only for data created AFTER the migration):
--   * devices, device columns, reconciliation lines, availability lists,
--     changefeed and asset tables/refs are dropped (asset FILES are untouched).
--   * A person with >1 open till, or >1 open till at a branch, cannot exist under
--     the old unique indexes: all but the OLDEST open one are force-closed with
--     reason 'tills-rework rollback'.
--   * Tills opened after the migration are bound to their branch's default
--     drawer entity ("Till 1", created if missing — the old trigger's rule).
-- The archive schema is KEPT, renamed to archive_rollback_<timestamp>, so the
-- migration can be applied again.

\set ON_ERROR_STOP 1
SET LOCAL lock_timeout = '30s';

-- ── Prechecks ─────────────────────────────────────────────────────────────────
DO $$
DECLARE
    dup bigint;
BEGIN
    IF to_regclass('archive.till_entities') IS NULL THEN
        RAISE EXCEPTION 'tills-rework down: archive.till_entities missing — was the migration applied?';
    END IF;
    SELECT count(*) INTO dup FROM (
        SELECT till_id, order_number FROM orders GROUP BY 1, 2 HAVING count(*) > 1) d;
    IF dup > 0 THEN
        RAISE EXCEPTION 'tills-rework down: % (till, order_number) pair(s) are duplicated by device-numbered orders; '
                        'UNIQUE(shift_id, order_number) cannot be restored. Renumber them manually first.', dup;
    END IF;
END $$;

CREATE TEMP TABLE _down_pre ON COMMIT DROP AS
SELECT (SELECT count(*) FROM tills) AS tills, (SELECT count(*) FROM orders) AS orders,
       (SELECT count(*) FROM order_payments) AS payments, (SELECT count(*) FROM till_cash_movements) AS cash_moves,
       (SELECT count(*) FROM order_refunds) AS refunds,
       (SELECT coalesce(sum(total_amount),0) FROM orders) AS sum_orders,
       (SELECT coalesce(sum(amount),0) FROM order_payments) AS sum_payments,
       (SELECT coalesce(sum(amount),0) FROM till_cash_movements) AS sum_cash_moves,
       (SELECT coalesce(sum(amount),0) FROM order_refunds) AS sum_refunds;

-- ═══ 090400 assets (+ 090600) ═══════════════════════════════════════════════════════════
DROP TRIGGER IF EXISTS asset_ref_changed ON menu_items;
DROP TRIGGER IF EXISTS asset_ref_changed ON categories;
DROP TRIGGER IF EXISTS asset_ref_changed ON bundles;
DROP TRIGGER IF EXISTS asset_ref_changed ON organizations;
DROP TRIGGER IF EXISTS asset_ref_changed ON recipe_step_presets;
DROP FUNCTION IF EXISTS asset_ref_changed_org_row();
DROP FUNCTION IF EXISTS asset_ref_changed_organizations();
DROP FUNCTION IF EXISTS asset_ref_changed_recipe_step_presets();
DROP FUNCTION IF EXISTS asset_mark_org_dirty(uuid);
ALTER TABLE menu_items          DROP COLUMN IF EXISTS image_group_id;
ALTER TABLE categories          DROP COLUMN IF EXISTS image_group_id;
ALTER TABLE bundles             DROP COLUMN IF EXISTS image_group_id;
ALTER TABLE organizations       DROP COLUMN IF EXISTS logo_group_id, DROP COLUMN IF EXISTS brand_card_image_group_id;
ALTER TABLE recipe_step_presets DROP COLUMN IF EXISTS animation_group_id;
DROP TABLE IF EXISTS asset_backfill_items, asset_legacy_paths, asset_bundle_dirty, asset_bundles, asset_jobs, assets, asset_groups;

-- ═══ 090300 changefeed (derived; no data loss) ═══════════════════════════════
DO $$
DECLARE
    t text;
BEGIN
    IF to_regproc('sync_source_tables') IS NOT NULL THEN
        FOR t IN SELECT source_table FROM sync_source_tables() LOOP
            EXECUTE format('DROP TRIGGER IF EXISTS sync_emit ON %I', t);
        END LOOP;
    END IF;
    FOR t IN SELECT p.oid::regprocedure::text FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
              WHERE n.nspname = 'public' AND p.proname ~ '^sync_(emit|touch|live|op$|source_tables$|types$|branch_lock_key$|safe_horizon$)'
              ORDER BY 1 LOOP
        EXECUTE 'DROP FUNCTION IF EXISTS ' || t || ' CASCADE';
    END LOOP;
END $$;
DROP TABLE IF EXISTS sync_feed_watermarks, sync_changes;
DROP SEQUENCE IF EXISTS sync_changes_seq;

-- ═══ 090200 reconciliation + branch settings ═════════════════════════════════
DROP TABLE IF EXISTS till_reconciliations;
ALTER TABLE branches DISABLE TRIGGER trg_branches_updated_at;
ALTER TABLE branches DROP COLUMN IF EXISTS old_bill_hours, DROP COLUMN IF EXISTS standard_float;
ALTER TABLE branches ENABLE TRIGGER trg_branches_updated_at;

-- ═══ 090100 availability ═════════════════════════════════════════════════════
DROP TABLE IF EXISTS device_payment_methods, user_payment_methods, branch_payment_methods;
DROP FUNCTION IF EXISTS payment_availability_same_org();

-- ═══ 090000 tills are sales sessions ═════════════════════════════════════════
ALTER TYPE permission_resource RENAME VALUE 'tills' TO 'shifts';

DROP TRIGGER order_payments_fill_till ON order_payments;
DROP FUNCTION order_payments_fill_till();
DROP INDEX idx_order_payments_till_method;
ALTER TABLE order_payments DROP COLUMN till_id;

ALTER TABLE order_refunds DROP COLUMN device_id;
ALTER TABLE open_tickets  DROP COLUMN settled_device_id;
ALTER TABLE till_cash_movements DROP COLUMN device_id;
DROP INDEX uq_orders_till_legacy_number;
DROP INDEX idx_orders_device_created;
ALTER TABLE orders DROP CONSTRAINT orders_device_numbered_has_code;
ALTER TABLE orders DROP COLUMN device_id, DROP COLUMN device_code, DROP COLUMN verification;

-- Old unique rules: keep the OLDEST open till per person, and per branch (every
-- rolled-back till binds to the branch's single default drawer).
ALTER TABLE tills DISABLE TRIGGER trg_tills_updated_at;
UPDATE tills t
   SET status = 'force_closed', force_closed_at = now(), closed_at = now(),
       force_close_reason = 'tills-rework rollback'
 WHERE t.status = 'open'
   AND EXISTS (SELECT 1 FROM tills o WHERE o.status = 'open' AND o.id <> t.id
                AND (o.teller_id = t.teller_id OR o.branch_id = t.branch_id)
                AND (o.opened_at, o.id) < (t.opened_at, t.id));
ALTER TABLE tills ENABLE TRIGGER trg_tills_updated_at;

DROP INDEX idx_tills_flagged;
DROP INDEX idx_tills_device;
DROP INDEX idx_tills_open_by_teller;
DROP INDEX idx_tills_open_by_branch;
ALTER TABLE tills
    DROP CONSTRAINT tills_flag_links_other,
    DROP CONSTRAINT tills_other_is_not_self,
    DROP CONSTRAINT tills_reconciled_when_closed,
    DROP COLUMN device_id, DROP COLUMN device_code, DROP COLUMN device_label, DROP COLUMN closed_device_id,
    DROP COLUMN verification, DROP COLUMN opened_while_another_open, DROP COLUMN other_till_id,
    DROP COLUMN flagged_at, DROP COLUMN reconciliation_status, DROP COLUMN open_bills_at_close,
    DROP COLUMN old_bills_at_close;
DROP TABLE devices;

-- Renames back
ALTER TABLE open_tickets RENAME COLUMN settled_till_id TO settled_shift_id;
ALTER TABLE open_tickets RENAME CONSTRAINT open_tickets_settled_till_id_fkey TO open_tickets_settled_shift_id_fkey;

ALTER TABLE order_refunds RENAME COLUMN till_id TO shift_id;
ALTER TABLE order_refunds RENAME CONSTRAINT order_refunds_till_id_fkey TO order_refunds_shift_id_fkey;
DROP INDEX idx_order_refunds_till;
CREATE INDEX idx_order_refunds_shift ON order_refunds (shift_id);
COMMENT ON COLUMN order_refunds.shift_id IS 'The shift this refund was ISSUED in — the drawer the money left. Not
     necessarily the shift the order was sold in. The drawer maths subtracts
     cash refunds by this column.';

ALTER TABLE orders RENAME COLUMN till_id TO shift_id;
ALTER TABLE orders RENAME CONSTRAINT orders_till_id_fkey TO orders_shift_id_fkey;
DROP INDEX idx_orders_till_id;
DROP INDEX idx_orders_till_created;
CREATE INDEX idx_orders_shift_id ON orders (shift_id);
CREATE INDEX idx_orders_shift_created ON orders (shift_id, created_at DESC) WHERE shift_id IS NOT NULL;
ALTER TABLE orders ADD CONSTRAINT orders_shift_id_order_number_key UNIQUE (shift_id, order_number);

DROP POLICY tenant_isolation ON till_cash_movements;
ALTER TRIGGER trg_till_cash_movements_fill_kind ON till_cash_movements RENAME TO trg_shift_cash_movements_fill_kind;
ALTER FUNCTION till_cash_movements_fill_kind() RENAME TO shift_cash_movements_fill_kind;
ALTER TABLE till_cash_movements RENAME CONSTRAINT till_cash_movements_only_a_correction_corrects TO shift_cash_movements_only_a_correction_corrects;
ALTER TABLE till_cash_movements RENAME CONSTRAINT till_cash_movements_kind_matches_sign TO shift_cash_movements_kind_matches_sign;
ALTER TABLE till_cash_movements RENAME CONSTRAINT till_cash_movements_kind_is_known    TO shift_cash_movements_kind_is_known;
ALTER TABLE till_cash_movements RENAME CONSTRAINT till_cash_movements_corrects_id_fkey TO shift_cash_movements_corrects_id_fkey;
ALTER TABLE till_cash_movements RENAME CONSTRAINT till_cash_movements_moved_by_fkey    TO shift_cash_movements_moved_by_fkey;
ALTER TABLE till_cash_movements RENAME CONSTRAINT till_cash_movements_till_id_fkey     TO shift_cash_movements_shift_id_fkey;
ALTER INDEX idx_till_cash_movements_corrects  RENAME TO idx_shift_cash_movements_corrects;
ALTER INDEX uq_till_cash_movements_client_ref RENAME TO uq_shift_cash_movements_client_ref;
DROP INDEX idx_till_cash_movements_till;
ALTER INDEX till_cash_movements_pkey          RENAME TO shift_cash_movements_pkey;
ALTER TABLE till_cash_movements RENAME COLUMN till_id TO shift_id;
ALTER TABLE till_cash_movements RENAME TO shift_cash_movements;
CREATE INDEX idx_shift_cash_movements_shift ON shift_cash_movements (shift_id);

ALTER TRIGGER trg_tills_updated_at ON tills RENAME TO trg_shifts_updated_at;
ALTER TABLE tills RENAME CONSTRAINT tills_force_closed_by_fkey TO shifts_force_closed_by_fkey;
ALTER TABLE tills RENAME CONSTRAINT tills_closed_by_fkey       TO shifts_closed_by_fkey;
ALTER TABLE tills RENAME CONSTRAINT tills_teller_id_fkey       TO shifts_teller_id_fkey;
ALTER TABLE tills RENAME CONSTRAINT tills_branch_id_fkey       TO shifts_branch_id_fkey;
ALTER INDEX idx_tills_opened_at RENAME TO idx_shifts_opened_at;
ALTER INDEX idx_tills_teller    RENAME TO idx_shifts_teller;
ALTER INDEX idx_tills_branch    RENAME TO idx_shifts_branch;
ALTER INDEX tills_pkey          RENAME TO shifts_pkey;
ALTER TABLE tills RENAME TO shifts;
ALTER TYPE till_status RENAME TO shift_status;

CREATE POLICY tenant_isolation ON shift_cash_movements
    USING (EXISTS (SELECT 1 FROM shifts p WHERE p.id = shift_cash_movements.shift_id));
COMMENT ON COLUMN shift_cash_movements.kind IS 'What the movement IS, which fixes its sign: pay_in (+, non-sale cash placed
     in the drawer), pay_out (−, spent on something), safe_drop (−, moved to the
     safe, NOT a cost), correction (either sign, reverses `corrects_id`). The
     opening float is `shifts.opening_cash`, not a movement; a refund belongs
     to its order, not here.';

-- Refund trigger: the original body, verbatim.
CREATE OR REPLACE FUNCTION order_refunds_before_insert() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    o            orders%ROWTYPE;
    order_org    uuid;
    shift_branch uuid;
    already      bigint;
BEGIN
    -- Locked. Two refunds racing on one order serialise here, and the second
    -- sums after the first has committed — that is what makes the bound
    -- below hold under concurrency and not just in a single-user test.
    SELECT * INTO o FROM orders WHERE id = NEW.order_id FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'refund: order % does not exist', NEW.order_id
            USING ERRCODE = 'foreign_key_violation';
    END IF;

    -- A voided sale was corrected, not sold; there is no money on the books
    -- to return. If money did change hands before the void, the void was the
    -- wrong tool and the books already say so.
    IF o.status = 'voided' THEN
        RAISE EXCEPTION 'refund: order % is voided — a voided sale has no money to return', NEW.order_id
            USING ERRCODE = 'check_violation';
    END IF;

    SELECT org_id INTO order_org FROM branches WHERE id = o.branch_id;

    IF NEW.branch_id IS NULL THEN
        NEW.branch_id := o.branch_id;
    ELSIF NEW.branch_id <> o.branch_id THEN
        RAISE EXCEPTION 'refund: order % was sold at branch %, not branch %', NEW.order_id, o.branch_id, NEW.branch_id
            USING ERRCODE = 'check_violation';
    END IF;

    IF NEW.org_id IS NULL THEN
        NEW.org_id := order_org;
    ELSIF NEW.org_id <> order_org THEN
        RAISE EXCEPTION 'refund: order % belongs to organisation %, not %', NEW.order_id, order_org, NEW.org_id
            USING ERRCODE = 'check_violation';
    END IF;

    -- The drawer the money leaves is at the branch that took it. A refund
    -- carried to another branch of the same organisation is not modelled:
    -- that branch's drawer would be short by a sale it never made.
    SELECT branch_id INTO shift_branch FROM shifts WHERE id = NEW.shift_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'refund: shift % does not exist', NEW.shift_id
            USING ERRCODE = 'foreign_key_violation';
    END IF;
    IF shift_branch <> o.branch_id THEN
        RAISE EXCEPTION 'refund: shift % is at branch %, but order % was sold at branch %',
            NEW.shift_id, shift_branch, NEW.order_id, o.branch_id
            USING ERRCODE = 'check_violation';
    END IF;

    -- The bound. What has already gone back plus this row may not pass what
    -- the customer was charged. A zero-total bill (discounted to nothing —
    -- 376 such legs exist) can therefore never be refunded, which is right.
    SELECT COALESCE(SUM(amount), 0) INTO already
      FROM order_refunds WHERE order_id = NEW.order_id;
    IF already + NEW.amount > o.total_amount THEN
        RAISE EXCEPTION 'refund: order % was charged %; % already refunded, % more requested',
            NEW.order_id, o.total_amount, already, NEW.amount
            USING ERRCODE = 'check_violation';
    END IF;

    RETURN NEW;
END $$;

-- The drawer entity, from its archive (every row, every column).
CREATE TABLE tills (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    branch_id uuid NOT NULL,
    name text NOT NULL,
    is_default boolean DEFAULT false NOT NULL,
    is_active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    deleted_at timestamp with time zone,
    standard_float integer,
    CONSTRAINT tills_standard_float_is_not_negative CHECK (((standard_float IS NULL) OR (standard_float >= 0)))
);
INSERT INTO tills (id, org_id, branch_id, name, is_default, is_active, created_at, updated_at, deleted_at, standard_float)
SELECT id, org_id, branch_id, name, is_default, is_active, created_at, updated_at, deleted_at, standard_float
  FROM archive.till_entities;
ALTER TABLE ONLY tills ADD CONSTRAINT tills_pkey PRIMARY KEY (id);
ALTER TABLE ONLY tills ADD CONSTRAINT tills_branch_id_fkey FOREIGN KEY (branch_id) REFERENCES branches(id) ON DELETE CASCADE;
ALTER TABLE ONLY tills ADD CONSTRAINT tills_org_id_fkey FOREIGN KEY (org_id) REFERENCES organizations(id) ON DELETE CASCADE;
CREATE INDEX idx_tills_branch ON tills USING btree (branch_id) WHERE (deleted_at IS NULL);
CREATE UNIQUE INDEX uq_tills_default ON tills USING btree (branch_id) WHERE (is_default AND (deleted_at IS NULL));
CREATE UNIQUE INDEX uq_tills_name ON tills USING btree (branch_id, lower(name)) WHERE (deleted_at IS NULL);
ALTER TABLE tills ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON tills USING ((org_id = ( SELECT (current_setting('app.org_id'::text, true))::uuid AS current_setting)));
GRANT SELECT, INSERT, DELETE, UPDATE ON TABLE tills TO madar_app;
COMMENT ON COLUMN tills.standard_float IS 'The cash that should be in this drawer at the start of a shift, in minor
     units. The close defaults `closing_cash_declared` to it (drop the rest to
     the safe); the next open on this till inherits it as the carryover. NULL
     when the shop has not set one — no default is proposed.';

-- Default drawer for any branch with a till opened after the migration and no default.
INSERT INTO tills (org_id, branch_id, name, is_default, is_active)
SELECT DISTINCT b.org_id, b.id, 'Till 1', true, true
  FROM shifts s JOIN branches b ON b.id = s.branch_id
 WHERE NOT EXISTS (SELECT 1 FROM archive.shift_till_bindings x WHERE x.shift_id = s.id)
   AND NOT EXISTS (SELECT 1 FROM tills t WHERE t.branch_id = b.id AND t.is_default AND t.deleted_at IS NULL)
ON CONFLICT DO NOTHING;

ALTER TABLE shifts DISABLE TRIGGER trg_shifts_updated_at;
ALTER TABLE shifts ADD COLUMN till_id uuid;
UPDATE shifts s SET till_id = x.till_entity_id FROM archive.shift_till_bindings x WHERE x.shift_id = s.id;
UPDATE shifts s SET till_id = t.id
  FROM tills t WHERE s.till_id IS NULL AND t.branch_id = s.branch_id AND t.is_default AND t.deleted_at IS NULL;
ALTER TABLE shifts ENABLE TRIGGER trg_shifts_updated_at;
ALTER TABLE shifts ALTER COLUMN till_id SET NOT NULL;
ALTER TABLE ONLY shifts ADD CONSTRAINT shifts_till_id_fkey FOREIGN KEY (till_id) REFERENCES tills(id);
CREATE INDEX idx_shifts_till ON shifts USING btree (till_id);
CREATE UNIQUE INDEX idx_shifts_one_open_per_till ON shifts USING btree (till_id) WHERE (status = 'open'::shift_status);
CREATE UNIQUE INDEX idx_shifts_one_open_per_teller ON shifts USING btree (teller_id) WHERE (status = 'open'::shift_status);

CREATE FUNCTION shifts_fill_default_till() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    v_till uuid;
    v_org  uuid;
BEGIN
    IF NEW.till_id IS NOT NULL THEN
        RETURN NEW;
    END IF;
    SELECT id INTO v_till FROM tills
        WHERE branch_id = NEW.branch_id AND is_default AND deleted_at IS NULL
        LIMIT 1;
    IF v_till IS NULL THEN
        SELECT org_id INTO v_org FROM branches WHERE id = NEW.branch_id;
        INSERT INTO tills (org_id, branch_id, name, is_default, is_active)
            VALUES (v_org, NEW.branch_id, 'Till 1', true, true)
            ON CONFLICT DO NOTHING;
        SELECT id INTO v_till FROM tills
            WHERE branch_id = NEW.branch_id AND is_default AND deleted_at IS NULL
            LIMIT 1;
    END IF;
    NEW.till_id := v_till;
    RETURN NEW;
END;
$$;
CREATE TRIGGER trg_shifts_fill_default_till BEFORE INSERT ON shifts FOR EACH ROW EXECUTE FUNCTION shifts_fill_default_till();

-- Occupancy refs back to drawer entities (see 090000 step 5 for the trigger choice).
ALTER TABLE table_occupancies DROP CONSTRAINT table_occupancies_started_till_id_fkey;
ALTER TABLE table_occupancies DROP CONSTRAINT table_occupancies_ended_till_id_fkey;
ALTER TABLE table_occupancies DISABLE TRIGGER table_occupancies_project_status;
UPDATE table_occupancies o
   SET started_till_id = a.started_till_entity_id, ended_till_id = a.ended_till_entity_id
  FROM archive.occupancy_till_refs a WHERE a.occupancy_id = o.id;
UPDATE table_occupancies o
   SET started_till_id = NULL, ended_till_id = NULL
 WHERE NOT EXISTS (SELECT 1 FROM archive.occupancy_till_refs a WHERE a.occupancy_id = o.id)
   AND (o.started_till_id IS NOT NULL OR o.ended_till_id IS NOT NULL);
ALTER TABLE table_occupancies ENABLE TRIGGER table_occupancies_project_status;
ALTER TABLE ONLY table_occupancies
    ADD CONSTRAINT table_occupancies_ended_till_id_fkey FOREIGN KEY (ended_till_id) REFERENCES tills(id) ON DELETE SET NULL;
ALTER TABLE ONLY table_occupancies
    ADD CONSTRAINT table_occupancies_started_till_id_fkey FOREIGN KEY (started_till_id) REFERENCES tills(id) ON DELETE SET NULL;

-- ── Reverse invariants I1–I3 ──────────────────────────────────────────────────
DO $$
DECLARE
    p   _down_pre;
    bad bigint;
BEGIN
    SELECT * INTO p FROM _down_pre;
    IF (SELECT count(*) FROM shifts) <> p.tills OR (SELECT count(*) FROM orders) <> p.orders
       OR (SELECT count(*) FROM order_payments) <> p.payments OR (SELECT count(*) FROM shift_cash_movements) <> p.cash_moves
       OR (SELECT count(*) FROM order_refunds) <> p.refunds THEN
        RAISE EXCEPTION 'tills-rework down invariant I1: row counts changed';
    END IF;
    IF (SELECT coalesce(sum(total_amount),0) FROM orders) <> p.sum_orders
       OR (SELECT coalesce(sum(amount),0) FROM order_payments) <> p.sum_payments
       OR (SELECT coalesce(sum(amount),0) FROM shift_cash_movements) <> p.sum_cash_moves
       OR (SELECT coalesce(sum(amount),0) FROM order_refunds) <> p.sum_refunds THEN
        RAISE EXCEPTION 'tills-rework down invariant I2: money sums changed';
    END IF;
    -- I3: nothing a pre-migration session held was lost (>=: sales may have been
    -- added to it after the migration; equality holds on a quiet rehearsal).
    SELECT count(*) INTO bad
      FROM archive.tills_rework_snapshot x
      LEFT JOIN shifts s ON s.id = x.shift_id
     WHERE s.id IS NULL OR s.branch_id <> x.branch_id OR s.teller_id <> x.teller_id
        OR (SELECT count(*) FROM orders o WHERE o.shift_id = x.shift_id) < x.orders_n
        OR (SELECT count(*) FROM order_payments pm JOIN orders o ON o.id = pm.order_id WHERE o.shift_id = x.shift_id) < x.payments_n
        OR (SELECT count(*) FROM shift_cash_movements m WHERE m.shift_id = x.shift_id) < x.cash_moves_n
        OR (SELECT count(*) FROM order_refunds r WHERE r.shift_id = x.shift_id) < x.refunds_n
        OR (SELECT count(*) FROM open_tickets k WHERE k.settled_shift_id = x.shift_id) < x.tickets_settled_n;
    IF bad <> 0 THEN
        RAISE EXCEPTION 'tills-rework down invariant I3: % session(s) lost rows', bad;
    END IF;
    IF (SELECT count(*) FROM tills) < (SELECT count(*) FROM archive.till_entities) THEN
        RAISE EXCEPTION 'tills-rework down invariant: drawer entities not restored';
    END IF;
END $$;

-- Keep the archive, out of the way of a re-apply.
DO $$
BEGIN
    EXECUTE format('ALTER SCHEMA archive RENAME TO %I', 'archive_rollback_' || to_char(clock_timestamp(), 'YYYYMMDDHH24MISS'));
END $$;

DELETE FROM _sqlx_migrations WHERE version IN
    (20260914090000, 20260914090100, 20260914090200, 20260914090300, 20260914090400, 20260914090500, 20260914090600);
