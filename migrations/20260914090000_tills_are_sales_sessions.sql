-- Tills are sales sessions (TILLS_CONTRACT.md §1.2 / §1.3).
--
-- What used to be called a "shift" — a person's sales session with its own cash
-- drawer — is now a TILL. The old `tills` table (a drawer/device binding: one
-- default "Till 1" per branch, never printed, never counted) is removed. Its rows
-- are archived, not lost, and every reference to it is either archived
-- (shifts.till_id) or remapped to the sales session that covered it
-- (table_occupancies.started_till_id / ended_till_id).
--
-- One transaction (sqlx default). Invariants at the bottom RAISE on any loss and
-- abort the whole migration. Reverse: scripts/tills_rework/down.sql.
--
-- Staff scheduling (work_shifts, work_shift_id, staff_schedules*) is NOT renamed.

-- ── 1. Archive schema + raw copies ─────────────────────────────────────────────
CREATE SCHEMA IF NOT EXISTS archive;

CREATE TABLE archive.till_entities AS SELECT * FROM public.tills;
ALTER TABLE archive.till_entities ADD COLUMN archived_at timestamptz NOT NULL DEFAULT now();
ALTER TABLE archive.till_entities ADD PRIMARY KEY (id);

CREATE TABLE archive.shift_till_bindings (
    shift_id       uuid PRIMARY KEY,
    till_entity_id uuid NOT NULL
);
INSERT INTO archive.shift_till_bindings (shift_id, till_entity_id)
SELECT id, till_id FROM shifts;

CREATE TABLE archive.permission_rows_snapshot (
    source   text NOT NULL,          -- 'permissions' | 'role_permissions'
    id       uuid NULL,              -- permissions.id (role_permissions has no id)
    role     text NULL,
    user_id  uuid NULL,
    resource text NOT NULL,
    action   text NOT NULL,
    granted  boolean NOT NULL
);
INSERT INTO archive.permission_rows_snapshot (source, id, role, user_id, resource, action, granted)
SELECT 'permissions', p.id, NULL, p.user_id, p.resource::text, p.action::text, p.granted
  FROM permissions p WHERE p.resource::text IN ('shifts', 'shift_counts')
UNION ALL
SELECT 'role_permissions', NULL, rp.role::text, NULL, rp.resource::text, rp.action::text, rp.granted
  FROM role_permissions rp WHERE rp.resource::text IN ('shifts', 'shift_counts');

-- ── 2. Per-session money snapshot ─────────────────────────────────────────────
CREATE TABLE archive.tills_rework_snapshot (
    shift_id              uuid PRIMARY KEY,
    branch_id             uuid,
    teller_id             uuid,
    status                text,
    orders_n              bigint,
    orders_total          bigint,
    payments_n            bigint,
    payments_total        bigint,
    cash_moves_n          bigint,
    cash_moves_total      bigint,
    refunds_n             bigint,
    refunds_total         bigint,
    tickets_settled_n     bigint,
    closing_cash_system   integer,
    closing_cash_declared integer
);
INSERT INTO archive.tills_rework_snapshot
SELECT s.id, s.branch_id, s.teller_id, s.status::text,
       (SELECT count(*)                       FROM orders o WHERE o.shift_id = s.id),
       (SELECT coalesce(sum(o.total_amount),0) FROM orders o WHERE o.shift_id = s.id),
       (SELECT count(*)                       FROM order_payments p JOIN orders o ON o.id = p.order_id WHERE o.shift_id = s.id),
       (SELECT coalesce(sum(p.amount),0)       FROM order_payments p JOIN orders o ON o.id = p.order_id WHERE o.shift_id = s.id),
       (SELECT count(*)                       FROM shift_cash_movements m WHERE m.shift_id = s.id),
       (SELECT coalesce(sum(m.amount),0)       FROM shift_cash_movements m WHERE m.shift_id = s.id),
       (SELECT count(*)                       FROM order_refunds r WHERE r.shift_id = s.id),
       (SELECT coalesce(sum(r.amount),0)       FROM order_refunds r WHERE r.shift_id = s.id),
       (SELECT count(*)                       FROM open_tickets t WHERE t.settled_shift_id = s.id),
       s.closing_cash_system, s.closing_cash_declared
  FROM shifts s;

-- ── 3. Global pre-counts ──────────────────────────────────────────────────────
CREATE TEMP TABLE _pre (k text PRIMARY KEY, v bigint NOT NULL);
INSERT INTO _pre VALUES
    ('shifts',              (SELECT count(*) FROM shifts)),
    ('open_shifts',         (SELECT count(*) FROM shifts WHERE status = 'open')),
    ('orders',              (SELECT count(*) FROM orders)),
    ('order_payments',      (SELECT count(*) FROM order_payments)),
    ('cash_moves',          (SELECT count(*) FROM shift_cash_movements)),
    ('refunds',             (SELECT count(*) FROM order_refunds)),
    ('tickets_settled',     (SELECT count(*) FROM open_tickets WHERE settled_shift_id IS NOT NULL)),
    ('occupancies',         (SELECT count(*) FROM table_occupancies)),
    ('sum_orders',          (SELECT coalesce(sum(total_amount),0) FROM orders)),
    ('sum_payments',        (SELECT coalesce(sum(amount),0) FROM order_payments)),
    ('sum_refunds',         (SELECT coalesce(sum(amount),0) FROM order_refunds)),
    ('sum_cash_moves',      (SELECT coalesce(sum(amount),0) FROM shift_cash_movements)),
    ('occ_started_ref',     (SELECT count(*) FROM table_occupancies WHERE started_till_id IS NOT NULL)),
    ('occ_ended_ref',       (SELECT count(*) FROM table_occupancies WHERE ended_till_id IS NOT NULL)),
    ('occ_any_ref',         (SELECT count(*) FROM table_occupancies WHERE started_till_id IS NOT NULL OR ended_till_id IS NOT NULL)),
    ('till_entities',       (SELECT count(*) FROM tills)),
    ('perm_rows_shifts',    (SELECT count(*) FROM permissions WHERE resource::text = 'shifts')),
    ('role_perm_rows_shifts', (SELECT count(*) FROM role_permissions WHERE resource::text = 'shifts'));

-- ── 4. Drop the entity binding and the one-open-per-person rule ──────────────
DROP TRIGGER trg_shifts_fill_default_till ON shifts;
DROP FUNCTION shifts_fill_default_till();
DROP INDEX idx_shifts_one_open_per_till;
DROP INDEX idx_shifts_till;
ALTER TABLE shifts DROP CONSTRAINT shifts_till_id_fkey;
ALTER TABLE table_occupancies DROP CONSTRAINT table_occupancies_started_till_id_fkey;
ALTER TABLE table_occupancies DROP CONSTRAINT table_occupancies_ended_till_id_fkey;
-- Decision 4: many open tills per person are allowed; the server flags, never refuses.
DROP INDEX idx_shifts_one_open_per_teller;

-- ── 5. Occupancy refs: archive, then remap to the covering sales session ─────
CREATE TABLE archive.occupancy_till_refs (
    occupancy_id             uuid PRIMARY KEY,
    started_till_entity_id   uuid NULL,
    ended_till_entity_id     uuid NULL,
    started_remapped_till_id uuid NULL,
    ended_remapped_till_id   uuid NULL
);
INSERT INTO archive.occupancy_till_refs (occupancy_id, started_till_entity_id, ended_till_entity_id,
                                         started_remapped_till_id, ended_remapped_till_id)
SELECT o.id, o.started_till_id, o.ended_till_id,
       CASE WHEN o.started_till_id IS NOT NULL THEN
         (SELECT s.id FROM shifts s
           WHERE s.teller_id = o.started_by AND s.branch_id = o.branch_id
             AND s.opened_at <= o.started_at AND (s.closed_at IS NULL OR s.closed_at >= o.started_at)
           ORDER BY s.opened_at DESC LIMIT 1) END,
       CASE WHEN o.ended_till_id IS NOT NULL AND o.ended_at IS NOT NULL THEN
         (SELECT s.id FROM shifts s
           WHERE s.teller_id = o.ended_by AND s.branch_id = o.branch_id
             AND s.opened_at <= o.ended_at AND (s.closed_at IS NULL OR s.closed_at >= o.ended_at)
           ORDER BY s.opened_at DESC LIMIT 1) END
  FROM table_occupancies o
 WHERE o.started_till_id IS NOT NULL OR o.ended_till_id IS NOT NULL;

-- Only refs that pointed at a drawer are remapped (a NULL ref stays NULL: the
-- occupancy was never attributed to a till, and the migration does not invent
-- an attribution).
--
-- Trigger choice (§1.3 step 5): table_occupancies has AFTER trigger
-- table_occupancies_project_status, which rewrites branch_tables.status from
-- v_table_status when they differ. The remap changes no column that
-- v_table_status derives status from, so on a consistent database the trigger
-- is a no-op. On a database whose deprecated branch_tables.status projection has
-- drifted it would silently "repair" rows — a mutation this rename migration
-- must not make. So the trigger is disabled for exactly this UPDATE (owner
-- privilege; not session_replication_role, which needs superuser and would also
-- skip FK checks).
ALTER TABLE table_occupancies DISABLE TRIGGER table_occupancies_project_status;
UPDATE table_occupancies o
   SET started_till_id = a.started_remapped_till_id,
       ended_till_id   = a.ended_remapped_till_id
  FROM archive.occupancy_till_refs a
 WHERE a.occupancy_id = o.id;
ALTER TABLE table_occupancies ENABLE TRIGGER table_occupancies_project_status;

-- ── 6. Remove the entity ──────────────────────────────────────────────────────
ALTER TABLE shifts DROP COLUMN till_id;
DROP TABLE tills;

-- ── 7. Renames ────────────────────────────────────────────────────────────────
ALTER TYPE shift_status RENAME TO till_status;

ALTER TABLE shifts RENAME TO tills;
ALTER INDEX shifts_pkey          RENAME TO tills_pkey;
ALTER INDEX idx_shifts_branch    RENAME TO idx_tills_branch;
ALTER INDEX idx_shifts_teller    RENAME TO idx_tills_teller;
ALTER INDEX idx_shifts_opened_at RENAME TO idx_tills_opened_at;
ALTER TABLE tills RENAME CONSTRAINT shifts_branch_id_fkey       TO tills_branch_id_fkey;
ALTER TABLE tills RENAME CONSTRAINT shifts_teller_id_fkey       TO tills_teller_id_fkey;
ALTER TABLE tills RENAME CONSTRAINT shifts_closed_by_fkey       TO tills_closed_by_fkey;
ALTER TABLE tills RENAME CONSTRAINT shifts_force_closed_by_fkey TO tills_force_closed_by_fkey;
ALTER TRIGGER trg_shifts_updated_at ON tills RENAME TO trg_tills_updated_at;
CREATE INDEX idx_tills_open_by_teller ON tills (teller_id) WHERE status = 'open';
CREATE INDEX idx_tills_open_by_branch ON tills (branch_id) WHERE status = 'open';

ALTER TABLE shift_cash_movements RENAME TO till_cash_movements;
ALTER TABLE till_cash_movements RENAME COLUMN shift_id TO till_id;
ALTER INDEX shift_cash_movements_pkey          RENAME TO till_cash_movements_pkey;
-- Recreated, not renamed: an index keeps its column's OLD name in pg_attribute.
DROP INDEX idx_shift_cash_movements_shift;
CREATE INDEX idx_till_cash_movements_till ON till_cash_movements (till_id);
ALTER INDEX uq_shift_cash_movements_client_ref RENAME TO uq_till_cash_movements_client_ref;
ALTER INDEX idx_shift_cash_movements_corrects  RENAME TO idx_till_cash_movements_corrects;
ALTER TABLE till_cash_movements RENAME CONSTRAINT shift_cash_movements_shift_id_fkey    TO till_cash_movements_till_id_fkey;
ALTER TABLE till_cash_movements RENAME CONSTRAINT shift_cash_movements_moved_by_fkey    TO till_cash_movements_moved_by_fkey;
ALTER TABLE till_cash_movements RENAME CONSTRAINT shift_cash_movements_corrects_id_fkey TO till_cash_movements_corrects_id_fkey;
ALTER TABLE till_cash_movements RENAME CONSTRAINT shift_cash_movements_kind_is_known    TO till_cash_movements_kind_is_known;
ALTER TABLE till_cash_movements RENAME CONSTRAINT shift_cash_movements_kind_matches_sign TO till_cash_movements_kind_matches_sign;
ALTER TABLE till_cash_movements RENAME CONSTRAINT shift_cash_movements_only_a_correction_corrects TO till_cash_movements_only_a_correction_corrects;
ALTER FUNCTION shift_cash_movements_fill_kind() RENAME TO till_cash_movements_fill_kind;
ALTER TRIGGER trg_shift_cash_movements_fill_kind ON till_cash_movements RENAME TO trg_till_cash_movements_fill_kind;
DROP POLICY tenant_isolation ON till_cash_movements;
CREATE POLICY tenant_isolation ON till_cash_movements
    USING (EXISTS (SELECT 1 FROM tills p WHERE p.id = till_cash_movements.till_id));
COMMENT ON COLUMN till_cash_movements.kind IS
    'What the movement IS, which fixes its sign: pay_in (+, non-sale cash placed
     in the drawer), pay_out (−, spent on something), safe_drop (−, moved to the
     safe, NOT a cost), correction (either sign, reverses `corrects_id`). The
     opening float is `tills.opening_cash`, not a movement; a refund belongs
     to its order, not here.';

ALTER TABLE orders RENAME COLUMN shift_id TO till_id;
ALTER TABLE orders RENAME CONSTRAINT orders_shift_id_fkey TO orders_till_id_fkey;
DROP INDEX idx_orders_shift_id;
DROP INDEX idx_orders_shift_created;
CREATE INDEX idx_orders_till_id      ON orders (till_id);
CREATE INDEX idx_orders_till_created ON orders (till_id, created_at DESC);

ALTER TABLE order_refunds RENAME COLUMN shift_id TO till_id;
ALTER TABLE order_refunds RENAME CONSTRAINT order_refunds_shift_id_fkey TO order_refunds_till_id_fkey;
DROP INDEX idx_order_refunds_shift;
CREATE INDEX idx_order_refunds_till ON order_refunds (till_id);
COMMENT ON COLUMN order_refunds.till_id IS
    'The till this refund was ISSUED in — the drawer the money left. Not
     necessarily the till the order was sold in.';

ALTER TABLE open_tickets RENAME COLUMN settled_shift_id TO settled_till_id;
ALTER TABLE open_tickets RENAME CONSTRAINT open_tickets_settled_shift_id_fkey TO open_tickets_settled_till_id_fkey;

-- ── 8. Refund trigger: plpgsql bodies are NOT rewritten by renames ───────────
CREATE OR REPLACE FUNCTION order_refunds_before_insert() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
DECLARE
    o            orders%ROWTYPE;
    order_org    uuid;
    till_branch  uuid;
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
    SELECT branch_id INTO till_branch FROM tills WHERE id = NEW.till_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'refund: till % does not exist', NEW.till_id
            USING ERRCODE = 'foreign_key_violation';
    END IF;
    IF till_branch <> o.branch_id THEN
        RAISE EXCEPTION 'refund: till % is at branch %, but order % was sold at branch %',
            NEW.till_id, till_branch, NEW.order_id, o.branch_id
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
END;
$$;

-- ── 9. Devices + new till / money-row columns ─────────────────────────────────
CREATE TABLE devices (
    id             uuid PRIMARY KEY,                 -- POS install UUID (core kv lan_device_id). Client-minted. NO default.
    org_id         uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id      uuid NULL REFERENCES branches(id) ON DELETE SET NULL,
    code           text NOT NULL CHECK (code ~ '^[A-Z0-9]{1,6}$'),
    label          text NULL CHECK (label IS NULL OR btrim(label) <> ''),
    kind           text NOT NULL DEFAULT 'pos' CHECK (kind IN ('pos','kds','waiter')),
    platform       text NULL,
    app_version    text NULL,
    first_seen_at  timestamptz NOT NULL DEFAULT now(),
    last_seen_at   timestamptz NOT NULL DEFAULT now(),
    retired_at     timestamptz NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX idx_devices_org_branch  ON devices (org_id, branch_id) WHERE retired_at IS NULL;
-- NOT unique: offline auto-register must never fail; the API reports conflicts.
CREATE INDEX idx_devices_branch_code ON devices (branch_id, code)   WHERE retired_at IS NULL;
CREATE TRIGGER trg_devices_updated_at BEFORE UPDATE ON devices FOR EACH ROW EXECUTE FUNCTION set_updated_at();
ALTER TABLE devices ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON devices
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE devices TO madar_app;

ALTER TABLE tills
    ADD COLUMN device_id                 uuid NULL CONSTRAINT tills_device_id_fkey REFERENCES devices(id) ON DELETE SET NULL,
    ADD COLUMN device_code               text NULL,
    ADD COLUMN device_label              text NULL,
    ADD COLUMN closed_device_id          uuid NULL CONSTRAINT tills_closed_device_id_fkey REFERENCES devices(id) ON DELETE SET NULL,
    ADD COLUMN verification              text NOT NULL DEFAULT 'server'
        CONSTRAINT tills_verification_is_known CHECK (verification IN ('server','lan','unverified','legacy')),
    ADD COLUMN opened_while_another_open boolean NOT NULL DEFAULT false,
    ADD COLUMN other_till_id             uuid NULL CONSTRAINT tills_other_till_id_fkey REFERENCES tills(id) ON DELETE SET NULL,
    ADD COLUMN flagged_at                timestamptz NULL,
    ADD COLUMN reconciliation_status     text NULL
        CONSTRAINT tills_reconciliation_status_is_known CHECK (reconciliation_status IN ('clean','disagreed','unreviewed')),
    ADD COLUMN open_bills_at_close       integer NULL
        CONSTRAINT tills_open_bills_at_close_nonneg CHECK (open_bills_at_close IS NULL OR open_bills_at_close >= 0),
    ADD COLUMN old_bills_at_close        integer NULL
        CONSTRAINT tills_old_bills_at_close_nonneg CHECK (old_bills_at_close IS NULL OR old_bills_at_close >= 0),
    ADD CONSTRAINT tills_flag_links_other       CHECK (NOT opened_while_another_open OR other_till_id IS NOT NULL),
    ADD CONSTRAINT tills_other_is_not_self      CHECK (other_till_id IS NULL OR other_till_id <> id),
    ADD CONSTRAINT tills_reconciled_when_closed CHECK (reconciliation_status IS NULL OR status <> 'open');
-- Every till that exists before this migration was opened under the old rules.
-- (trg_tills_updated_at would bump updated_at; keep history exact.)
ALTER TABLE tills DISABLE TRIGGER trg_tills_updated_at;
UPDATE tills SET verification = 'legacy';
ALTER TABLE tills ENABLE TRIGGER trg_tills_updated_at;
CREATE INDEX idx_tills_device  ON tills (device_id) WHERE device_id IS NOT NULL;
CREATE INDEX idx_tills_flagged ON tills (branch_id, opened_at DESC)
    WHERE opened_while_another_open OR reconciliation_status = 'disagreed';

ALTER TABLE till_cash_movements
    ADD COLUMN device_id uuid NULL CONSTRAINT till_cash_movements_device_id_fkey REFERENCES devices(id) ON DELETE SET NULL;

ALTER TABLE orders
    ADD COLUMN device_id    uuid NULL CONSTRAINT orders_device_id_fkey REFERENCES devices(id) ON DELETE SET NULL,
    ADD COLUMN device_code  text NULL
        CONSTRAINT orders_device_code_shape CHECK (device_code IS NULL OR device_code ~ '^[A-Z0-9]{1,6}$'),
    ADD COLUMN verification text NULL
        CONSTRAINT orders_verification_is_known CHECK (verification IN ('server','lan','unverified')),
    ADD CONSTRAINT orders_device_numbered_has_code CHECK (device_id IS NULL OR device_code IS NOT NULL);
-- Decision 9: device-numbered orders (36B-12) are unique by order_ref; the
-- per-till number rule survives only for server-numbered rows.
ALTER TABLE orders DROP CONSTRAINT orders_shift_id_order_number_key;
CREATE UNIQUE INDEX uq_orders_till_legacy_number ON orders (till_id, order_number) WHERE device_id IS NULL;
CREATE INDEX idx_orders_device_created ON orders (device_id, created_at DESC) WHERE device_id IS NOT NULL;

ALTER TABLE open_tickets
    ADD COLUMN settled_device_id uuid NULL CONSTRAINT open_tickets_settled_device_id_fkey REFERENCES devices(id) ON DELETE SET NULL;

ALTER TABLE order_refunds
    ADD COLUMN device_id uuid NULL CONSTRAINT order_refunds_device_id_fkey REFERENCES devices(id) ON DELETE SET NULL;

-- ── 10. order_payments.till_id ────────────────────────────────────────────────
ALTER TABLE order_payments ADD COLUMN till_id uuid NULL;
UPDATE order_payments p SET till_id = o.till_id FROM orders o WHERE o.id = p.order_id;
ALTER TABLE order_payments ALTER COLUMN till_id SET NOT NULL;
ALTER TABLE order_payments
    ADD CONSTRAINT order_payments_till_id_fkey FOREIGN KEY (till_id) REFERENCES tills(id);
CREATE INDEX idx_order_payments_till_method ON order_payments (till_id, method);

CREATE FUNCTION order_payments_fill_till() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    -- Safety net: application code passes till_id explicitly. A payment leg
    -- always lands in the till its order was rung up in.
    IF NEW.till_id IS NULL THEN
        SELECT till_id INTO NEW.till_id FROM orders WHERE id = NEW.order_id;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER order_payments_fill_till BEFORE INSERT ON order_payments
    FOR EACH ROW EXECUTE FUNCTION order_payments_fill_till();

-- ── 11. Occupancy refs now point at sales sessions ───────────────────────────
ALTER TABLE table_occupancies
    ADD CONSTRAINT table_occupancies_started_till_id_fkey FOREIGN KEY (started_till_id) REFERENCES tills(id) ON DELETE SET NULL,
    ADD CONSTRAINT table_occupancies_ended_till_id_fkey   FOREIGN KEY (ended_till_id)   REFERENCES tills(id) ON DELETE SET NULL;

-- ── 12. Permission resource ───────────────────────────────────────────────────
-- Rows follow automatically. 'shift_counts' stays as a dead enum value (enum
-- values cannot be dropped); the seeder never grants it.
ALTER TYPE permission_resource RENAME VALUE 'shifts' TO 'tills';

-- ── 13/14. Invariants ─────────────────────────────────────────────────────────
DO $$
DECLARE
    pre   jsonb;
    bad   bigint;
    txt   text;
BEGIN
    SELECT jsonb_object_agg(k, v) INTO pre FROM _pre;

    -- I1 row counts
    IF (SELECT count(*) FROM tills) <> (pre->>'shifts')::bigint THEN
        RAISE EXCEPTION 'tills-rework invariant I1: tills % <> shifts %', (SELECT count(*) FROM tills), pre->>'shifts'; END IF;
    IF (SELECT count(*) FROM orders) <> (pre->>'orders')::bigint THEN
        RAISE EXCEPTION 'tills-rework invariant I1: orders count changed'; END IF;
    IF (SELECT count(*) FROM order_payments) <> (pre->>'order_payments')::bigint THEN
        RAISE EXCEPTION 'tills-rework invariant I1: order_payments count changed'; END IF;
    IF (SELECT count(*) FROM till_cash_movements) <> (pre->>'cash_moves')::bigint THEN
        RAISE EXCEPTION 'tills-rework invariant I1: cash movements count changed'; END IF;
    IF (SELECT count(*) FROM order_refunds) <> (pre->>'refunds')::bigint THEN
        RAISE EXCEPTION 'tills-rework invariant I1: refunds count changed'; END IF;
    IF (SELECT count(*) FROM open_tickets WHERE settled_till_id IS NOT NULL) <> (pre->>'tickets_settled')::bigint THEN
        RAISE EXCEPTION 'tills-rework invariant I1: settled open tickets count changed'; END IF;
    IF (SELECT count(*) FROM table_occupancies) <> (pre->>'occupancies')::bigint THEN
        RAISE EXCEPTION 'tills-rework invariant I1: table_occupancies count changed'; END IF;

    -- I2 money sums
    IF (SELECT coalesce(sum(total_amount),0) FROM orders) <> (pre->>'sum_orders')::bigint THEN
        RAISE EXCEPTION 'tills-rework invariant I2: sum(orders.total_amount) changed'; END IF;
    IF (SELECT coalesce(sum(amount),0) FROM order_payments) <> (pre->>'sum_payments')::bigint THEN
        RAISE EXCEPTION 'tills-rework invariant I2: sum(order_payments.amount) changed'; END IF;
    IF (SELECT coalesce(sum(amount),0) FROM order_refunds) <> (pre->>'sum_refunds')::bigint THEN
        RAISE EXCEPTION 'tills-rework invariant I2: sum(order_refunds.amount) changed'; END IF;
    IF (SELECT coalesce(sum(amount),0) FROM till_cash_movements) <> (pre->>'sum_cash_moves')::bigint THEN
        RAISE EXCEPTION 'tills-rework invariant I2: sum(till_cash_movements.amount) changed'; END IF;

    -- I3 per till
    SELECT count(*), min(x.shift_id::text) INTO bad, txt
      FROM archive.tills_rework_snapshot x
      LEFT JOIN tills t ON t.id = x.shift_id
     WHERE t.id IS NULL
        OR t.branch_id IS DISTINCT FROM x.branch_id
        OR t.teller_id IS DISTINCT FROM x.teller_id
        OR t.status::text IS DISTINCT FROM x.status
        OR t.closing_cash_system   IS DISTINCT FROM x.closing_cash_system
        OR t.closing_cash_declared IS DISTINCT FROM x.closing_cash_declared
        OR (SELECT count(*)                        FROM orders o WHERE o.till_id = x.shift_id) <> x.orders_n
        OR (SELECT coalesce(sum(o.total_amount),0) FROM orders o WHERE o.till_id = x.shift_id) <> x.orders_total
        OR (SELECT count(*)                        FROM order_payments p WHERE p.till_id = x.shift_id) <> x.payments_n
        OR (SELECT coalesce(sum(p.amount),0)       FROM order_payments p WHERE p.till_id = x.shift_id) <> x.payments_total
        OR (SELECT count(*)                        FROM till_cash_movements m WHERE m.till_id = x.shift_id) <> x.cash_moves_n
        OR (SELECT coalesce(sum(m.amount),0)       FROM till_cash_movements m WHERE m.till_id = x.shift_id) <> x.cash_moves_total
        OR (SELECT count(*)                        FROM order_refunds r WHERE r.till_id = x.shift_id) <> x.refunds_n
        OR (SELECT coalesce(sum(r.amount),0)       FROM order_refunds r WHERE r.till_id = x.shift_id) <> x.refunds_total
        OR (SELECT count(*)                        FROM open_tickets k WHERE k.settled_till_id = x.shift_id) <> x.tickets_settled_n;
    IF bad <> 0 THEN
        RAISE EXCEPTION 'tills-rework invariant I3: % till(s) differ from snapshot (first %)', bad, txt; END IF;

    -- I4 payments follow their order's till
    SELECT count(*) INTO bad FROM order_payments p JOIN orders o ON o.id = p.order_id
     WHERE p.till_id IS DISTINCT FROM o.till_id;
    IF bad <> 0 THEN
        RAISE EXCEPTION 'tills-rework invariant I4: % payment leg(s) not in their order''s till', bad; END IF;

    -- I5 archives complete
    IF (SELECT count(*) FROM archive.till_entities) <> (pre->>'till_entities')::bigint THEN
        RAISE EXCEPTION 'tills-rework invariant I5: archive.till_entities incomplete'; END IF;
    IF (SELECT count(*) FROM archive.shift_till_bindings) <> (pre->>'shifts')::bigint THEN
        RAISE EXCEPTION 'tills-rework invariant I5: archive.shift_till_bindings incomplete'; END IF;

    -- I6 occupancy refs archived; remapped ids are same-branch sessions
    IF (SELECT count(*) FROM archive.occupancy_till_refs) <> (pre->>'occ_any_ref')::bigint THEN
        RAISE EXCEPTION 'tills-rework invariant I6: archive.occupancy_till_refs incomplete'; END IF;
    SELECT count(*) INTO bad FROM table_occupancies o
     WHERE (o.started_till_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM tills t WHERE t.id = o.started_till_id AND t.branch_id = o.branch_id))
        OR (o.ended_till_id   IS NOT NULL AND NOT EXISTS (SELECT 1 FROM tills t WHERE t.id = o.ended_till_id   AND t.branch_id = o.branch_id));
    IF bad <> 0 THEN
        RAISE EXCEPTION 'tills-rework invariant I6: % occupancy ref(s) point outside their branch', bad; END IF;

    -- I7 no plpgsql body or view still speaks of shifts
    SELECT count(*), string_agg(p.proname, ',') INTO bad, txt
      FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
     WHERE n.nspname = 'public'
       AND regexp_replace(p.prosrc, 'work_shift', '', 'g') ~ '(\mshifts\M|shift_id)';
    IF bad <> 0 THEN
        RAISE EXCEPTION 'tills-rework invariant I7: function bodies still reference shifts: %', txt; END IF;
    SELECT count(*), string_agg(viewname, ',') INTO bad, txt
      FROM pg_views WHERE schemaname = 'public'
       AND regexp_replace(definition, 'work_shift', '', 'g') ~ '(\mshifts\M|shift_id)';
    IF bad <> 0 THEN
        RAISE EXCEPTION 'tills-rework invariant I7: views still reference shifts: %', txt; END IF;

    -- I8 permission enum
    IF NOT EXISTS (SELECT 1 FROM pg_enum e JOIN pg_type t ON t.oid = e.enumtypid
                    WHERE t.typname = 'permission_resource' AND e.enumlabel = 'tills')
       OR EXISTS (SELECT 1 FROM pg_enum e JOIN pg_type t ON t.oid = e.enumtypid
                    WHERE t.typname = 'permission_resource' AND e.enumlabel = 'shifts') THEN
        RAISE EXCEPTION 'tills-rework invariant I8: permission_resource not renamed'; END IF;
    IF (SELECT count(*) FROM permissions WHERE resource = 'tills') <> (pre->>'perm_rows_shifts')::bigint
       OR (SELECT count(*) FROM role_permissions WHERE resource = 'tills') <> (pre->>'role_perm_rows_shifts')::bigint THEN
        RAISE EXCEPTION 'tills-rework invariant I8: permission rows did not follow the rename'; END IF;

    -- I9 open tills
    IF (SELECT count(*) FROM tills WHERE status = 'open') <> (pre->>'open_shifts')::bigint THEN
        RAISE EXCEPTION 'tills-rework invariant I9: open tills count changed'; END IF;
END $$;
DROP TABLE _pre;
