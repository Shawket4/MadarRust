-- A refund RETURNS MONEY. A void CORRECTS A MISTAKE. They are different events.
--
-- `order_status` has carried a `refunded` label since the first schema, and in
-- the local copy of production not one of the 9,931 orders has ever worn it:
-- 9,821 are `completed`, 110 are `voided`, and every report spells its revenue
-- filter `NOT IN ('voided', 'refunded')` against a value nothing writes. When a
-- customer actually gets money back today, the till voids the sale — which
-- tells the books the sale never happened. It did happen. The food was made,
-- the tax was charged, the drawer took the cash, the loyalty points were
-- earned; and then, later, some or all of the money went back out. A report
-- that cannot tell those two stories apart cannot answer "how much did we
-- give back last month", cannot explain why a shift's drawer is short by
-- exactly one bill, and files a wrong-order refund under the same heading as
-- a mis-rung duplicate.
--
-- So a refund becomes its own row against the sale. The sale stands as rung
-- up — `total_amount` does not move, the payment legs stay money in, the
-- `orders` row keeps every figure it had — and `order_refunds` records money
-- going the other way: how much, out of which drawer, how it was handed back,
-- why, by whom, when. The wave-1 migrations were written to leave exactly
-- this room (`orders_total_is_the_sum_of_its_parts` and
-- `order_payments_leg_is_money_in` in 20260912030000; the "a refund is not a
-- cash movement" ruling in 20260912070000; `source = 'refund'` and
-- `loyalty_reverse()` in 20260912060000), and this file is the feature they
-- were leaving room for.
--
-- WHAT A REFUND IS HERE
--
--   * It is against ONE settled order and returns at most what that order was
--     charged. Partial refunds are normal — one dish sent back, an overcharge
--     put right — and may accumulate, but the CUMULATIVE amount refunded
--     against an order can never exceed its `total_amount`. That bound is the
--     whole safety of the feature: without it a refund loop on a retrying
--     till hands out the same bill again and again. It is enforced INSIDE the
--     database, by a BEFORE INSERT trigger that takes the order's row lock
--     (`SELECT … FOR UPDATE`), sums the refunds already on record, and refuses
--     the row if the sum plus this one passes the total. The row lock is what
--     makes it correct under concurrency: two refunds racing on one order
--     serialise on the lock, and the second sums after the first has
--     committed. A CHECK constraint cannot see other rows and a UNIQUE index
--     cannot add, so a trigger is the only place this rule can live.
--
--   * It is issued IN A SHIFT. Cash leaving a drawer belongs to the shift
--     whose drawer it left, the way a sale does — and that is the shift the
--     refund was issued in, which need not be the shift the order was sold
--     in. The drawer maths at close must subtract the cash refunds issued in
--     THIS shift, keyed on `order_refunds.shift_id`, not on the order's.
--
--   * It says how the money went back — `method` in the org's payment-method
--     vocabulary (the same words `order_payments.method` uses) and `is_cash`
--     snapshotted at issue, so the drawer maths never has to ask a payment
--     method whose cash flag may since have been flipped. One tender per
--     row; a refund split across two tenders is two rows.
--
--   * It has a categorised reason and a note. The four void reasons keep
--     their spelling here so a "wrong order" reads the same across voids and
--     refunds in a report; three more exist only for refunds, because you do
--     not void a sale for being late. `other` requires a note — an `other`
--     with nothing behind it tells the report nothing.
--
--   * It may name the LINES it is for (`order_refund_lines`), with a quantity
--     and an amount per line and whether the goods came back. Optional: an
--     overcharge or a goodwill gesture is an amount with no line behind it.
--     When lines are named, the quantities are held to what the order sold
--     (cumulatively, across every refund of that order) and the amounts to
--     what the refund returns. Per-line `restock` is what drives inventory —
--     see below.
--
--   * It is APPEND-ONLY. A refund is a receipt for money that has left the
--     building; a wrong one is not edited into a right one. UPDATE, DELETE
--     and TRUNCATE are refused, with the same single exception the loyalty
--     ledger makes: rows belonging to a demo organisation being erased, or to
--     an organisation already gone, may go with it.
--
-- WHAT THE ORDER'S STATUS DOES
--
-- `orders.status` becomes `refunded` ONLY when the cumulative refunded amount
-- equals `total_amount`. A partial refund leaves the status alone and is
-- visible through this table. That is the default taken in wave 1 and it is
-- what lets every existing report keep meaning what it meant: `SOLD`
-- (`NOT IN ('voided', 'refunded')`) still excludes exactly the sales that are
-- no longer sales, and nothing that was `completed` yesterday changes label
-- because a customer was given 20 back today. The flip is done by an AFTER
-- INSERT trigger on this table rather than by application code, for the
-- reason the loyalty ledger gave for its void trigger: every path that can
-- refund (the till, `/sync/replay`, an admin) would otherwise have to
-- remember, and the one that forgets leaves a fully refunded order counted
-- as revenue. A BEFORE UPDATE guard on `orders` closes the other direction:
-- `refunded` cannot be written by hand unless the money is on record, cannot
-- be left once it is, and an order that has returned money cannot be voided —
-- a void says the sale never happened, and a sale that has given money back
-- demonstrably did. Refund the remainder instead.
--
-- WHAT FOLLOWS A REFUND, INSIDE THE DATABASE
--
--   * LOYALTY. The points a sale earned follow the money back out, in
--     proportion: after each refund the earn is reversed down to
--     `floor(earned × refunded ÷ total)`, through `loyalty_reverse()` with
--     `source = 'refund'`, under the programme's clawback policy
--     (`loyalty_settings.allow_negative_balance`, default clamp at zero —
--     wave 1 default (a)). Redemptions are NOT reversed: a reward that was
--     handed over was consumed, and refunding the money the customer paid
--     does not un-drink the free coffee. The owner may rule otherwise; the
--     hook is the same function. As with the void trigger, application code
--     must NOT also write refund reversals — the second writer would hit
--     "never more than the original" and fail the refund.
--
--   * A DRAWER. Not here — the drawer is arithmetic over rows, and the rows
--     are here. `compute_system_cash` must learn to subtract
--     `SUM(amount) WHERE shift_id = $1 AND is_cash`. That is application
--     code and is listed for it, not done for it.
--
-- WHAT DOES NOT FOLLOW A REFUND
--
--   * INVENTORY, automatically. Whether the goods came back is a fact about
--     the world that only the person at the till knows — a sealed bottle
--     returns to the shelf, a sent-back plate does not — so it is recorded
--     per line (`order_refund_lines.restock`) and the handler writes a
--     `refund_restock` movement (added to the type enum below, unused in this
--     file) for the lines that say so, mirroring `void_restock`. No lines, no
--     restock: an amount-only refund moves no stock.
--   * THE TIP. `total_amount` excludes it and so does the refundable ceiling;
--     a tip handed back is not modelled and not pretended to be.
--   * THE TICKET. A dine-in refund is an event on the order the bill settled
--     into, never on the `open_tickets` row — 20260912020000 says so on
--     `open_tickets.order_id`, and nothing here touches that table.
--   * `delivery_orders.total`. It is the quote; the sale is the orders row;
--     the refund is against the sale.
--
-- A `refunds` permission resource is added so a shop can decide who issues
-- them separately from who voids (the enum value is added here and not used
-- in-file; the per-role defaults are seeded at boot by `permissions::seeder`).
-- Per the owner's ruling there is no approval flow: whoever holds the
-- permission refunds, and the row says who it was.

-- ── 1. The refund ───────────────────────────────────────────────────────────

CREATE TABLE order_refunds (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    -- A tenant that is hard-deleted takes its refunds with it, as it takes
    -- every other table; a refund that outlives its shop explains nothing.
    org_id      uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- Everything else is RESTRICT: a refunded sale, the drawer it left, the
    -- branch it happened at and the person who issued it are all
    -- soft-deleted in this schema, and the one hard path (the demo sweeper)
    -- must remove refunds first — a sale may not be deleted from under the
    -- money that was returned against it.
    branch_id   uuid NOT NULL REFERENCES branches(id) ON DELETE RESTRICT,
    order_id    uuid NOT NULL REFERENCES orders(id)   ON DELETE RESTRICT,
    shift_id    uuid NOT NULL REFERENCES shifts(id)   ON DELETE RESTRICT,

    -- Minor units, like every money column here. Strictly positive: a
    -- refund of nothing is not an event, and "non-negative" would let a
    -- retrying client write a zero row that reads as a refund in a count.
    amount      integer NOT NULL,
    -- How the money went back: a name from the org's payment-method
    -- vocabulary (`order_payments.method`), and whether that name meant cash
    -- AT THE TIME. `order_payments.is_cash` is nullable for history's sake
    -- and every reader COALESCEs it; this table has no history to excuse, so
    -- the flag is required and the drawer maths reads it plainly.
    method      text    NOT NULL,
    is_cash     boolean NOT NULL,

    reason      text NOT NULL,
    note        text,

    issued_by   uuid NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    -- When the refund was issued, as the till says. An offline till replays
    -- its queue later and the event keeps its real time, the way `voided_at`
    -- does; `created_at` is when the server recorded it.
    issued_at   timestamptz NOT NULL DEFAULT now(),
    -- Idempotency for a retried request or a replayed offline queue, the
    -- same shape as `shift_cash_movements.client_ref`. The cumulative bound
    -- below would in any case stop a replay giving the money away twice on
    -- a fully refunded order; this stops it on a partially refunded one.
    client_ref  uuid,
    created_at  timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT order_refunds_amount_is_money_out
        CHECK (amount > 0),
    CONSTRAINT order_refunds_method_is_not_blank
        CHECK (btrim(method) <> ''),
    CONSTRAINT order_refunds_reason_is_known
        CHECK (reason IN (
            'customer_request',     -- same word as the void reason
            'wrong_order',          -- same word as the void reason
            'quality_issue',        -- same word as the void reason
            'overcharged',          -- the bill was wrong; the difference goes back
            'late_or_undelivered',  -- delivery/pickup that did not arrive, or not in time
            'goodwill',             -- nothing was wrong on paper; the shop chose to
            'other'                 -- requires a note
        )),
    CONSTRAINT order_refunds_other_needs_a_note
        CHECK (reason <> 'other' OR (note IS NOT NULL AND btrim(note) <> '')),
    CONSTRAINT order_refunds_note_is_not_blank
        CHECK (note IS NULL OR btrim(note) <> '')
);

-- "Everything returned against this order" — the cumulative check, the view
-- below and every report join walk this.
CREATE INDEX idx_order_refunds_order ON order_refunds (order_id);
-- The drawer maths and the Z-report: refunds issued in this shift.
CREATE INDEX idx_order_refunds_shift ON order_refunds (shift_id);
-- The refunds report: a branch over a period, newest first.
CREATE INDEX idx_order_refunds_branch_issued ON order_refunds (branch_id, issued_at DESC);
CREATE UNIQUE INDEX uq_order_refunds_client_ref
    ON order_refunds (client_ref) WHERE client_ref IS NOT NULL;

COMMENT ON TABLE order_refunds IS
    'Money returned to a customer against a settled order. The sale stands as
     rung up; each row here is money going the other way. Append-only. The
     sum of amounts per order never exceeds orders.total_amount (trigger,
     under the order''s row lock), and the order''s status flips to refunded
     only when the sum reaches it.';
COMMENT ON COLUMN order_refunds.shift_id IS
    'The shift this refund was ISSUED in — the drawer the money left. Not
     necessarily the shift the order was sold in. The drawer maths subtracts
     cash refunds by this column.';
COMMENT ON COLUMN order_refunds.amount IS
    'Minor units, > 0. Cumulatively bounded by the order''s total_amount,
     which excludes the tip — a tip handed back is not modelled.';
COMMENT ON COLUMN order_refunds.method IS
    'How the money went back, in the org''s payment-method vocabulary — the
     same words order_payments.method uses. One tender per row.';
COMMENT ON COLUMN order_refunds.is_cash IS
    'Whether `method` meant cash when this refund was issued. Snapshotted, not
     looked up, so a method whose flag is later flipped does not move a
     closed shift''s drawer.';
COMMENT ON COLUMN order_refunds.reason IS
    'Categorised. The first three share their spelling with void_reason so a
     report can read "wrong order" across voids and refunds; the rest exist
     only for refunds. `other` needs a note.';
COMMENT ON COLUMN order_refunds.issued_at IS
    'When the refund was issued, as the till says — kept on replay of an
     offline queue. created_at is when the server recorded it.';
COMMENT ON COLUMN order_refunds.client_ref IS
    'Client-minted idempotency key for a retry or an offline replay. Unique
     when present.';

-- ── 2. The lines a refund is for ────────────────────────────────────────────

CREATE TABLE order_refund_lines (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    -- Copied from the refund by trigger, for the row policy.
    org_id        uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- A line goes with its refund; nothing deletes a refund except the org
    -- purge, and then the lines should go too.
    refund_id     uuid NOT NULL REFERENCES order_refunds(id) ON DELETE CASCADE,
    -- Deleting the order is already refused by order_refunds.order_id, so
    -- this RESTRICT is never the one that fires; it states the dependency.
    order_item_id uuid NOT NULL REFERENCES order_items(id) ON DELETE RESTRICT,
    -- How many of the line's units this refund is for. Held, cumulatively
    -- across every refund of the order, to what the line sold.
    quantity      integer NOT NULL,
    -- The share of the refund's amount attributed to this line. Zero is
    -- allowed: a reward line (`order_items.is_reward`) is sent back for
    -- nothing. NOT bounded by `order_items.line_total`: on an exclusive-tax
    -- order the line's share of what the customer paid is more than its
    -- pre-tax line total, and on a discounted one less; the honest bound is
    -- the refund's own amount, checked below.
    amount        integer NOT NULL,
    -- The goods came back and the handler puts their deductions back in
    -- stock (`refund_restock`). False for a plate that was sent back and
    -- thrown away, and for anything that was consumed.
    restock       boolean NOT NULL DEFAULT false,

    CONSTRAINT order_refund_lines_quantity_is_positive
        CHECK (quantity > 0),
    CONSTRAINT order_refund_lines_amount_is_not_negative
        CHECK (amount >= 0)
);

CREATE INDEX idx_order_refund_lines_refund ON order_refund_lines (refund_id);
-- The cumulative quantity check, and "how often does this dish come back".
CREATE INDEX idx_order_refund_lines_item   ON order_refund_lines (order_item_id);

COMMENT ON TABLE order_refund_lines IS
    'Which lines of the order a refund was for, when it was for lines at all.
     Optional detail on order_refunds: an overcharge or a goodwill refund has
     none. Quantities are bounded by the line (cumulatively across refunds),
     amounts by the refund. Append-only with its parent.';
COMMENT ON COLUMN order_refund_lines.restock IS
    'The goods came back. The handler writes a refund_restock inventory
     movement for this line''s deductions when true. Per line, because only
     the person at the till knows which plate returned and which was eaten.';

-- ── 3. The rules a refund must meet to be written ───────────────────────────

-- The cumulative bound, the order's state, and the tenant/branch/shift
-- agreement, checked where no writer can skip them. `org_id` and `branch_id`
-- are filled from the order when the writer leaves them NULL and refused when
-- they disagree — the loyalty ledger's pattern: fill what is unambiguous,
-- reject what is wrong.
CREATE FUNCTION order_refunds_before_insert() RETURNS trigger
LANGUAGE plpgsql AS $$
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

CREATE TRIGGER order_refunds_before_insert
    BEFORE INSERT ON order_refunds
    FOR EACH ROW EXECUTE FUNCTION order_refunds_before_insert();

-- What follows the money: the status flip at the ceiling, and the loyalty
-- clawback in proportion. AFTER INSERT, so the sum includes this row and the
-- bound above has already held.
CREATE FUNCTION order_refunds_after_insert() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    total     integer;
    refunded  bigint;
    earn      record;
    target    integer;
    reversed  bigint;
BEGIN
    SELECT total_amount INTO total FROM orders WHERE id = NEW.order_id;
    SELECT SUM(amount) INTO refunded FROM order_refunds WHERE order_id = NEW.order_id;

    -- Default (b): the label follows the money only when all of it is back.
    -- The guard on orders (below) lets this UPDATE through because the sum
    -- is on record.
    IF refunded = total THEN
        UPDATE orders SET status = 'refunded'
         WHERE id = NEW.order_id AND status <> 'refunded';
    END IF;

    -- The earn follows the money back out, in proportion, down to
    -- floor(earned × refunded ÷ total). `total` is > 0 here: refunded is at
    -- least this row's amount, which is > 0, and at most total. Whatever was
    -- already reversed against the earn — by an earlier refund, or by hand —
    -- counts towards the target, and loyalty_reverse() applies the clamp and
    -- the "never more than the original" rule. Redemptions are left alone:
    -- the reward was consumed. See the header.
    FOR earn IN
        SELECT id, points
          FROM loyalty_transactions
         WHERE order_id = NEW.order_id AND kind = 'earn' AND reverses_id IS NULL
    LOOP
        target := floor(earn.points::numeric * refunded / total)::integer;
        SELECT COALESCE(SUM(abs(points)), 0) INTO reversed
          FROM loyalty_transactions WHERE reverses_id = earn.id;
        IF target > reversed THEN
            PERFORM loyalty_reverse(earn.id, (target - reversed)::integer, 'refund', NEW.issued_by, NEW.note);
        END IF;
    END LOOP;

    RETURN NULL;
END $$;

CREATE TRIGGER order_refunds_after_insert
    AFTER INSERT ON order_refunds
    FOR EACH ROW EXECUTE FUNCTION order_refunds_after_insert();

COMMENT ON TRIGGER order_refunds_after_insert ON order_refunds IS
    'Flips orders.status to refunded when the cumulative amount reaches
     total_amount (and only then), and claws back the sale''s loyalty earn in
     proportion via loyalty_reverse(source = refund). Application code must
     NOT also flip the status or write refund reversals.';

-- The other direction, on the order itself. `refunded` is a statement about
-- money on record in this table; no writer may make it without the money,
-- take it back once made, or void over the top of it.
CREATE FUNCTION orders_status_respects_refunds() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    refunded bigint;
BEGIN
    IF NEW.status IS NOT DISTINCT FROM OLD.status THEN
        RETURN NEW;
    END IF;

    SELECT COALESCE(SUM(amount), 0) INTO refunded
      FROM order_refunds WHERE order_id = NEW.id;

    IF OLD.status = 'refunded' THEN
        RAISE EXCEPTION 'order % is refunded: % went back to the customer and that cannot be unsaid; the status stays',
            NEW.id, refunded
            USING ERRCODE = 'check_violation';
    END IF;

    IF NEW.status = 'voided' AND refunded > 0 THEN
        RAISE EXCEPTION 'order % cannot be voided: % has already been refunded against it. A void says the sale never happened; refund the remainder instead',
            NEW.id, refunded
            USING ERRCODE = 'check_violation';
    END IF;

    IF NEW.status = 'refunded' AND (refunded = 0 OR refunded <> NEW.total_amount) THEN
        RAISE EXCEPTION 'order % cannot be marked refunded: % of % is on record in order_refunds. The status follows the money — insert the refund',
            NEW.id, refunded, NEW.total_amount
            USING ERRCODE = 'check_violation';
    END IF;

    RETURN NEW;
END $$;

CREATE TRIGGER orders_status_respects_refunds
    BEFORE UPDATE OF status ON orders
    FOR EACH ROW EXECUTE FUNCTION orders_status_respects_refunds();

-- Lines: the item is the refund's order's; the quantities do not exceed the
-- line (across every refund of the order); the amounts do not exceed the
-- refund. Both sums are taken under a row lock — the line's for quantity,
-- the refund's for amount — for the same reason as the order's above.
CREATE FUNCTION order_refund_lines_before_insert() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    rf     order_refunds%ROWTYPE;
    li     order_items%ROWTYPE;
    taken  bigint;
    spent  bigint;
BEGIN
    SELECT * INTO rf FROM order_refunds WHERE id = NEW.refund_id FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'refund line: refund % does not exist', NEW.refund_id
            USING ERRCODE = 'foreign_key_violation';
    END IF;

    IF NEW.org_id IS NULL THEN
        NEW.org_id := rf.org_id;
    ELSIF NEW.org_id <> rf.org_id THEN
        RAISE EXCEPTION 'refund line: refund % belongs to organisation %, not %', NEW.refund_id, rf.org_id, NEW.org_id
            USING ERRCODE = 'check_violation';
    END IF;

    SELECT * INTO li FROM order_items WHERE id = NEW.order_item_id FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'refund line: order item % does not exist', NEW.order_item_id
            USING ERRCODE = 'foreign_key_violation';
    END IF;
    IF li.order_id <> rf.order_id THEN
        RAISE EXCEPTION 'refund line: item % is on order %, but refund % is against order %',
            NEW.order_item_id, li.order_id, NEW.refund_id, rf.order_id
            USING ERRCODE = 'check_violation';
    END IF;

    SELECT COALESCE(SUM(quantity), 0) INTO taken
      FROM order_refund_lines WHERE order_item_id = NEW.order_item_id;
    IF taken + NEW.quantity > li.quantity THEN
        RAISE EXCEPTION 'refund line: item % sold % × "%"; % already refunded, % more requested',
            NEW.order_item_id, li.quantity, li.item_name, taken, NEW.quantity
            USING ERRCODE = 'check_violation';
    END IF;

    SELECT COALESCE(SUM(amount), 0) INTO spent
      FROM order_refund_lines WHERE refund_id = NEW.refund_id;
    IF spent + NEW.amount > rf.amount THEN
        RAISE EXCEPTION 'refund line: refund % returns %; its lines already account for %, this line asks %',
            NEW.refund_id, rf.amount, spent, NEW.amount
            USING ERRCODE = 'check_violation';
    END IF;

    RETURN NEW;
END $$;

CREATE TRIGGER order_refund_lines_before_insert
    BEFORE INSERT ON order_refund_lines
    FOR EACH ROW EXECUTE FUNCTION order_refund_lines_before_insert();

-- ── 4. Append-only ──────────────────────────────────────────────────────────

-- One function for both tables. The exception is the loyalty ledger's: a
-- demo organisation reaching its TTL (the sweeper deletes refunds before
-- orders, because orders is RESTRICT), or an organisation row already gone
-- and cascading through — during a cascade the org row is already deleted
-- from this statement's point of view, so the EXISTS is false and the rows
-- follow it.
CREATE FUNCTION order_refunds_are_append_only() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'TRUNCATE' THEN
        RAISE EXCEPTION '% is append-only and cannot be truncated', TG_TABLE_NAME
            USING ERRCODE = 'insufficient_privilege';
    ELSIF TG_OP = 'UPDATE' THEN
        RAISE EXCEPTION '% is append-only: row % is a receipt for money that has left and cannot be changed', TG_TABLE_NAME, OLD.id
            USING ERRCODE = 'insufficient_privilege';
    ELSIF EXISTS (SELECT 1 FROM organizations WHERE id = OLD.org_id AND NOT is_demo) THEN
        RAISE EXCEPTION '% is append-only: row % is history and cannot be deleted while its organisation exists', TG_TABLE_NAME, OLD.id
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    RETURN OLD;
END $$;

CREATE TRIGGER order_refunds_no_update_or_delete
    BEFORE UPDATE OR DELETE ON order_refunds
    FOR EACH ROW EXECUTE FUNCTION order_refunds_are_append_only();
CREATE TRIGGER order_refunds_no_truncate
    BEFORE TRUNCATE ON order_refunds
    FOR EACH STATEMENT EXECUTE FUNCTION order_refunds_are_append_only();
CREATE TRIGGER order_refund_lines_no_update_or_delete
    BEFORE UPDATE OR DELETE ON order_refund_lines
    FOR EACH ROW EXECUTE FUNCTION order_refunds_are_append_only();
CREATE TRIGGER order_refund_lines_no_truncate
    BEFORE TRUNCATE ON order_refund_lines
    FOR EACH STATEMENT EXECUTE FUNCTION order_refunds_are_append_only();

-- ── 5. Who may see it ───────────────────────────────────────────────────────

ALTER TABLE order_refunds      ENABLE ROW LEVEL SECURITY;
ALTER TABLE order_refund_lines ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON order_refunds FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
CREATE POLICY tenant_isolation ON order_refund_lines FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));

GRANT ALL ON TABLE order_refunds      TO sufrix;
GRANT ALL ON TABLE order_refund_lines TO sufrix;
-- Stated rather than left to ALTER DEFAULT PRIVILEGES, for the reason
-- 20260912010000 gives: a database migrated by a different role would
-- otherwise hand the tenant pool a table it cannot read. UPDATE is granted
-- and then refused by the trigger; the trigger is the rule, the grant is not.
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE order_refunds      TO madar_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE order_refund_lines TO madar_app;

-- ── 6. The per-order figure every report will want ──────────────────────────

-- `LEFT JOIN v_order_refund_totals r ON r.order_id = o.id` is the one line a
-- revenue query needs to subtract partial refunds (a fully refunded order is
-- already out of SOLD by status). Kept as a view rather than a cached column
-- on `orders`: the rows behind it are here, the join is on an indexed key,
-- and a cache is one more thing a writer has to remember.
-- `security_invoker` so the tables' RLS applies to whoever queries it.
CREATE VIEW v_order_refund_totals WITH (security_invoker = true) AS
SELECT order_id,
       SUM(amount)::bigint                         AS refunded_amount,
       SUM(amount) FILTER (WHERE is_cash)::bigint  AS refunded_cash,
       COUNT(*)::bigint                            AS refund_count,
       MIN(issued_at)                              AS first_refund_at,
       MAX(issued_at)                              AS last_refund_at
  FROM order_refunds
 GROUP BY order_id;

GRANT SELECT ON v_order_refund_totals TO sufrix;
GRANT SELECT ON v_order_refund_totals TO madar_app;

COMMENT ON VIEW v_order_refund_totals IS
    'Per order: how much has been returned, how much of it in cash, how many
     times, and when. Join it to subtract partial refunds from a revenue
     figure; fully refunded orders are already excluded by status.';

-- ── 7. The vocabulary the handlers will need ────────────────────────────────

-- Neither value is used in this file: `ALTER TYPE … ADD VALUE` is allowed
-- inside a migration transaction on PG 12+, but the new label cannot be
-- USED until that transaction commits (same pattern as 20260815100100).
--
-- `refund_restock`: the inventory movement the handler writes for a line
-- whose goods came back — a positive quantity that nets out the sale's
-- negative one, exactly as `void_restock` does. It is its own type so the
-- consumption reports (which net `void_restock` today) can tell a restocked
-- refund from a restocked void, and must add it to the set they net.
ALTER TYPE inventory_movement_type ADD VALUE IF NOT EXISTS 'refund_restock';

-- `refunds`: the permission resource. Seeded per role at boot by
-- `permissions::seeder`; `permissions::RESOURCES` must list it or the matrix
-- test fails and the app hides the feature.
ALTER TYPE permission_resource ADD VALUE IF NOT EXISTS 'refunds';
