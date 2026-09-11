-- The loyalty ledger says what happened, and cannot be made to say otherwise.
--
-- A member's balance is already maintained by trigger from
-- `loyalty_transactions`, so the balance can never drift from the rows that
-- explain it. The rows themselves, though, could say anything — or nothing.
-- Three things were wrong with them:
--
--   1. A void left loyalty untouched. Points are written when an order settles
--      and nothing is written when that order is voided, so a customer keeps
--      the points from a sale that, as far as the books are concerned, never
--      happened. Worse, the ledger had no way even to SAY "this movement undoes
--      that one": the only negative kind was `redeem`, and an `adjust` with a
--      note is a story, not a link. Any fix had to be application code
--      remembering, on every path that voids (the till, `/sync/replay`, an
--      admin) — which is exactly the shape of thing that was forgotten once.
--
--   2. Nothing said WHY a row existed. An admin's correction, a birthday gift
--      and a win-back sweetener were all `adjust`, told apart only by reading
--      the note. "How much did the programme give away this month" cannot be
--      built on a free-text column.
--
--   3. It was append-only by convention. UPDATE and DELETE were simply never
--      called; a customer or branch delete CASCADEd straight through the
--      history; and a row could be edited after the fact by anyone holding
--      the table. A ledger whose past can change is a balance with a
--      decoration, not a record.
--
-- So: reversals become kinds of their own with an enforced link to the row they
-- undo; every row names its source; a void writes its own reversals from inside
-- the database; and the table refuses UPDATE, DELETE and TRUNCATE outright.
--
-- Refunds are deliberately left room for and deliberately not automated here.
-- A void undoes a whole order and always means the same thing; a refund returns
-- money, may be partial, and its own feature decides how much loyalty follows
-- it. `source = 'refund'` and `loyalty_reverse()` are the hooks it will use.
--
-- Nothing is live: production holds 0 members and 0 transactions.

-- ── 1. The kinds ─────────────────────────────────────────────────────────────
-- A reversal is not a negative earn (earns are positive, and reports count on
-- it) and not an adjustment (an adjustment is a decision, a reversal is a
-- consequence). Each original kind gets its mirror. `ALTER TYPE ... ADD VALUE`
-- cannot be USED in the transaction that adds it, and the CHECKs below need
-- the new names now, so the type is rebuilt rather than extended.

-- Both mention `kind` in their predicates; dropped so they are not rebuilt
-- against the retiring type, recreated below.
DROP INDEX loyalty_transactions_earn_order_key;
DROP INDEX loyalty_transactions_redeem_line_key;
ALTER TABLE loyalty_transactions
    DROP CONSTRAINT loyalty_txn_earn_positive,
    DROP CONSTRAINT loyalty_txn_redeem_negative;

ALTER TYPE loyalty_txn_kind RENAME TO loyalty_txn_kind_without_reversals;
CREATE TYPE loyalty_txn_kind AS ENUM (
    'earn', 'redeem', 'adjust',
    'reverse_earn', 'reverse_redeem', 'reverse_adjust'
);
ALTER TABLE loyalty_transactions
    ALTER COLUMN kind TYPE loyalty_txn_kind USING kind::text::loyalty_txn_kind;
DROP TYPE loyalty_txn_kind_without_reversals;

COMMENT ON TYPE loyalty_txn_kind IS
    'What a ledger row IS. earn/redeem/adjust move a balance for a reason of
     their own; each reverse_* undoes exactly one earlier row of the matching
     kind, named in reverses_id, and never more than that row moved.';

-- The sign of each kind, so a report may sum a kind without inspecting rows.
-- An adjustment may go either way, and its reversal mirrors it (checked in the
-- trigger, since it depends on the reversed row).
ALTER TABLE loyalty_transactions
    ADD CONSTRAINT loyalty_txn_earn_positive           CHECK (kind <> 'earn'           OR points > 0),
    ADD CONSTRAINT loyalty_txn_redeem_negative         CHECK (kind <> 'redeem'         OR points < 0),
    ADD CONSTRAINT loyalty_txn_reverse_earn_negative   CHECK (kind <> 'reverse_earn'   OR points < 0),
    ADD CONSTRAINT loyalty_txn_reverse_redeem_positive CHECK (kind <> 'reverse_redeem' OR points > 0);

-- Same idempotency as before — an order earns once, a covered line redeems
-- once — now against the rebuilt type. Reversals are other kinds and do not
-- collide with either.
CREATE UNIQUE INDEX loyalty_transactions_earn_order_key
    ON loyalty_transactions (order_id) WHERE kind = 'earn' AND order_id IS NOT NULL;
CREATE UNIQUE INDEX loyalty_transactions_redeem_line_key
    ON loyalty_transactions (order_id, order_line_index)
    WHERE kind = 'redeem' AND order_id IS NOT NULL AND order_line_index IS NOT NULL;

-- ── 2. Provenance: where a movement came from, and what it undoes ────────────
ALTER TABLE loyalty_transactions
    -- WHY this row exists. Text with a CHECK rather than an enum, for the
    -- reason section 1 just demonstrated: a new source will arrive (a signup
    -- bonus, a referral, an expiry) and should not need the type rebuilt.
    ADD COLUMN source      text,
    -- The row this one undoes. RESTRICT is belt-and-braces — the table refuses
    -- deletes anyway — but it also documents that a reversal without its
    -- original is meaningless.
    ADD COLUMN reverses_id uuid REFERENCES loyalty_transactions(id) ON DELETE RESTRICT;

-- Every row so far was one of three things, and each is unambiguous: an earn
-- came from a sale, a redemption from a redemption, and an adjustment from a
-- person (birthday and win-back gifts go through `adjust` too; until the code
-- passes the finer word they read as manual, which is the coarser truth rather
-- than a wrong one).
UPDATE loyalty_transactions
   SET source = CASE kind::text
                    WHEN 'earn'   THEN 'sale'
                    WHEN 'redeem' THEN 'redemption'
                    ELSE 'manual'
                END
 WHERE source IS NULL;
ALTER TABLE loyalty_transactions ALTER COLUMN source SET NOT NULL;

ALTER TABLE loyalty_transactions
    ADD CONSTRAINT loyalty_txn_source CHECK (source IN (
        'sale',        -- points/stamps for a settled order
        'redemption',  -- a reward handed over against an order line
        'void',        -- an order was torn up; its movements are undone
        'refund',      -- money went back to the customer; some or all follows
        'birthday',    -- the birthday gift
        'winback',     -- the "we've missed you" sweetener
        'manual'       -- a person typed it, or reversed something by hand
    )),
    -- Which sources may explain which kinds. An `earn` is only ever a sale:
    -- points a person hands out are an `adjust`, so gifts and corrections
    -- never masquerade as revenue-driven earning in a report.
    ADD CONSTRAINT loyalty_txn_kind_has_a_source CHECK (
        CASE kind
            WHEN 'earn'           THEN source = 'sale'
            WHEN 'redeem'         THEN source = 'redemption'
            WHEN 'adjust'         THEN source IN ('manual', 'birthday', 'winback')
            WHEN 'reverse_earn'   THEN source IN ('void', 'refund', 'manual')
            WHEN 'reverse_redeem' THEN source IN ('void', 'refund', 'manual')
            WHEN 'reverse_adjust' THEN source = 'manual'
        END
    ),
    -- A reversal names what it reverses; nothing else may.
    ADD CONSTRAINT loyalty_txn_reversal_is_linked CHECK (
        (kind IN ('reverse_earn', 'reverse_redeem', 'reverse_adjust')) = (reverses_id IS NOT NULL)
    ),
    -- Anything that happened because of an order says which order.
    ADD CONSTRAINT loyalty_txn_order_sources_name_the_order CHECK (
        source NOT IN ('sale', 'redemption', 'void', 'refund') OR order_id IS NOT NULL
    );

-- "How much of this row has been undone" and "everything that touched this
-- order" are the two questions the triggers below ask on every reversal.
CREATE INDEX idx_loyalty_txn_reverses ON loyalty_transactions (reverses_id)
    WHERE reverses_id IS NOT NULL;
CREATE INDEX idx_loyalty_txn_order ON loyalty_transactions (order_id)
    WHERE order_id IS NOT NULL;

COMMENT ON COLUMN loyalty_transactions.source IS
    'Why this row exists. Paired with kind by CHECK: an earn is only ever a
     sale, a gift is an adjust with source birthday/winback, a reversal says
     whether a void, a refund or a person caused it.';
COMMENT ON COLUMN loyalty_transactions.reverses_id IS
    'For reverse_* kinds, the row being undone. The trigger holds the rest of
     the contract: same member, same currency, opposite sign, matching kind,
     never more in total than the original moved, and never a reversal of a
     reversal — a wrong reversal is corrected with an adjustment.';

-- ── 3. Nothing referenced by history may be deleted ──────────────────────────
-- Customers, branches, users, menu items and orders are all soft-deleted in
-- this schema; the only hard deletes are the demo-org sweeper and a shift
-- purge of voided orders. Cascading or nulling through the ledger would erase
-- or blur the explanation of a balance, so every reference is RESTRICT.
--
-- The one exception is the organisation. A tenant that is hard-deleted takes
-- every other loyalty table with it, and a ledger that outlives its shop
-- explains nothing to anybody; the delete trigger in section 6 lets that
-- cascade through.
ALTER TABLE loyalty_transactions
    DROP CONSTRAINT loyalty_transactions_customer_id_fkey,
    ADD  CONSTRAINT loyalty_transactions_customer_id_fkey
         FOREIGN KEY (customer_id) REFERENCES loyalty_customers(id) ON DELETE RESTRICT,
    DROP CONSTRAINT loyalty_transactions_branch_id_fkey,
    ADD  CONSTRAINT loyalty_transactions_branch_id_fkey
         FOREIGN KEY (branch_id) REFERENCES branches(id) ON DELETE RESTRICT,
    DROP CONSTRAINT loyalty_transactions_order_id_fkey,
    ADD  CONSTRAINT loyalty_transactions_order_id_fkey
         FOREIGN KEY (order_id) REFERENCES orders(id) ON DELETE RESTRICT,
    DROP CONSTRAINT loyalty_transactions_reward_menu_item_id_fkey,
    ADD  CONSTRAINT loyalty_transactions_reward_menu_item_id_fkey
         FOREIGN KEY (reward_menu_item_id) REFERENCES menu_items(id) ON DELETE RESTRICT,
    DROP CONSTRAINT loyalty_transactions_created_by_fkey,
    ADD  CONSTRAINT loyalty_transactions_created_by_fkey
         FOREIGN KEY (created_by) REFERENCES users(id) ON DELETE RESTRICT;

-- ── 4. The policy the owner still has to rule on ─────────────────────────────
-- Earning clawback when the points are already spent: a member earns 100 on a
-- sale, spends them on a reward, and the sale is then voided. Either the
-- balance is clamped at zero and the shop eats the reward, or it goes to -100
-- and the next visits earn into the hole.
--
-- The default is to CLAMP, because:
--   * a void is nearly always the shop's correction (a wrong ring-up, a
--     duplicate), and showing a customer a negative card for the shop's mistake
--     is the worst outcome a loyalty programme can produce;
--   * the loss is bounded — at most one reward that has already been handed
--     over — and it stays VISIBLE: the earn sits on the ledger only partly
--     reversed, against an order that says `voided`, so a report can list
--     exactly who benefited and by how much;
--   * a debt on a card the customer may never present again is uncollectable.
--
-- Shops that would rather the books balance can turn this on per scope, like
-- every other setting here (a branch row overrides the org row). It governs
-- CLAWBACKS ONLY: a redeem or a negative adjust can never take a balance below
-- zero, whatever this says — you cannot spend what you do not have; you can
-- only owe because a sale you were paid for was undone.
ALTER TABLE loyalty_settings
    ADD COLUMN allow_negative_balance boolean NOT NULL DEFAULT false;

COMMENT ON COLUMN loyalty_settings.allow_negative_balance IS
    'When a void or refund claws back points the member has already spent:
     false clamps the balance at zero (the shop eats the reward, and the earn
     stays visibly part-reversed); true lets the balance go negative so the
     next visits earn into the hole. Clawbacks only — spending never overdraws.';

-- Resolved the way `load_effective` resolves every other setting: the branch
-- row if there is one, else the org row, else the default.
CREATE OR REPLACE FUNCTION loyalty_allows_negative_balance(p_org uuid, p_branch uuid)
RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT COALESCE(
        (SELECT allow_negative_balance
           FROM loyalty_settings
          WHERE org_id = p_org AND (branch_id = p_branch OR branch_id IS NULL)
          ORDER BY branch_id NULLS LAST
          LIMIT 1),
        false)
$$;

-- The floor moves from a CHECK on the customer row into the balance trigger,
-- where it can tell a clawback from a spend. A CHECK cannot ask who is moving
-- the balance or what the programme's policy is.
ALTER TABLE loyalty_customers
    DROP CONSTRAINT loyalty_customers_balance_nonneg,
    DROP CONSTRAINT loyalty_customers_visits_nonneg;

-- ── 5. The balance follows the ledger — including backwards ──────────────────
-- Same trigger as before, with two corrections a reversal forces:
--   * `lifetime_*` counted every positive movement, which would have let a
--     reverse_redeem (points handed back) inflate "lifetime earned". Lifetime
--     now means what was genuinely earned or gifted, net of what was undone.
--   * the floor: a spend may never overdraw; a clawback may only when the
--     programme allows it; and a movement UP towards zero is always allowed,
--     so a member in debt can still be gifted or handed points back.
CREATE OR REPLACE FUNCTION loyalty_apply_txn() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    lifetime_delta integer;
    new_balance    integer;
BEGIN
    lifetime_delta := CASE NEW.kind
        WHEN 'earn'           THEN NEW.points
        WHEN 'reverse_earn'   THEN NEW.points               -- negative
        WHEN 'adjust'         THEN GREATEST(NEW.points, 0)  -- a gift counts, a deduction does not
        WHEN 'reverse_adjust' THEN LEAST(NEW.points, 0)     -- undoing a gift; undoing a deduction is not earning
        ELSE 0                                              -- redeem / reverse_redeem: spending, not earning
    END;

    UPDATE loyalty_customers
       SET points_balance  = points_balance
             + CASE WHEN NEW.currency = 'points' THEN NEW.points ELSE 0 END,
           visits_balance  = visits_balance
             + CASE WHEN NEW.currency = 'visits' THEN NEW.points ELSE 0 END,
           lifetime_points = lifetime_points
             + CASE WHEN NEW.currency = 'points' THEN lifetime_delta ELSE 0 END,
           lifetime_visits = lifetime_visits
             + CASE WHEN NEW.currency = 'visits' THEN lifetime_delta ELSE 0 END,
           updated_at      = now()
     WHERE id = NEW.customer_id
     RETURNING CASE WHEN NEW.currency = 'points' THEN points_balance ELSE visits_balance END
          INTO new_balance;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'loyalty customer % not found', NEW.customer_id;
    END IF;

    -- The floor applies to the movement that crosses it, never to one that
    -- climbs back towards zero: a member already in debt must still be able
    -- to receive a gift or a reversed redemption.
    IF new_balance < 0 AND NEW.points < 0 THEN
        IF NEW.kind NOT IN ('reverse_earn', 'reverse_adjust') THEN
            RAISE EXCEPTION 'loyalty: % of % % would take member % to %; a balance cannot be spent below zero',
                NEW.kind, NEW.points, NEW.currency, NEW.customer_id, new_balance
                USING ERRCODE = 'check_violation';
        ELSIF NOT loyalty_allows_negative_balance(NEW.org_id, NEW.branch_id) THEN
            RAISE EXCEPTION 'loyalty: clawing back % % would take member % to %, and this programme clamps at zero (loyalty_settings.allow_negative_balance)',
                NEW.points, NEW.currency, NEW.customer_id, new_balance
                USING ERRCODE = 'check_violation';
        END IF;
    END IF;
    RETURN NEW;
END $$;
-- The trigger itself is unchanged and still bound to this function.

-- ── 6. The contract a row must meet to be written, and the refusal to change ─
CREATE OR REPLACE FUNCTION loyalty_txn_before_insert() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    orig          loyalty_transactions%ROWTYPE;
    already       integer;
    order_status  text;
BEGIN
    -- Provenance that is unambiguous from the kind is filled in, so the
    -- existing writers (earn, redeem, adjust) keep working and keep telling the
    -- truth. A reversal's source is never obvious and must be stated.
    IF NEW.source IS NULL THEN
        NEW.source := CASE NEW.kind
            WHEN 'earn'   THEN 'sale'
            WHEN 'redeem' THEN 'redemption'
            WHEN 'adjust' THEN 'manual'
        END;
    END IF;

    IF NEW.reverses_id IS NOT NULL THEN
        -- Locked, so two reversals of the same row racing each other
        -- serialise and the second sees the first when it sums.
        SELECT * INTO orig FROM loyalty_transactions WHERE id = NEW.reverses_id FOR UPDATE;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'loyalty: nothing to reverse — transaction % does not exist', NEW.reverses_id
                USING ERRCODE = 'foreign_key_violation';
        END IF;
        IF orig.reverses_id IS NOT NULL THEN
            RAISE EXCEPTION 'loyalty: % is itself a reversal and cannot be reversed; correct it with an adjustment', orig.id
                USING ERRCODE = 'check_violation';
        END IF;
        IF NEW.kind::text <> 'reverse_' || orig.kind::text THEN
            RAISE EXCEPTION 'loyalty: % cannot reverse % (transaction %)', NEW.kind, orig.kind, orig.id
                USING ERRCODE = 'check_violation';
        END IF;
        IF orig.customer_id <> NEW.customer_id OR orig.org_id <> NEW.org_id OR orig.currency <> NEW.currency THEN
            RAISE EXCEPTION 'loyalty: a reversal must move the same member, tenant and currency as transaction %', orig.id
                USING ERRCODE = 'check_violation';
        END IF;
        IF sign(NEW.points) <> -sign(orig.points) THEN
            RAISE EXCEPTION 'loyalty: reversing % (% points) needs the opposite sign, got %', orig.id, orig.points, NEW.points
                USING ERRCODE = 'check_violation';
        END IF;
        -- The order is the original's order; a reversal cannot invent one.
        IF NEW.order_id IS NULL THEN
            NEW.order_id := orig.order_id;
        ELSIF NEW.order_id IS DISTINCT FROM orig.order_id THEN
            RAISE EXCEPTION 'loyalty: reversal of % names order % but the original was for order %',
                orig.id, NEW.order_id, orig.order_id
                USING ERRCODE = 'check_violation';
        END IF;
        -- Partial reversals may accumulate (a refund in two parts), but never
        -- past what was moved in the first place.
        SELECT COALESCE(SUM(abs(points)), 0) INTO already
          FROM loyalty_transactions WHERE reverses_id = NEW.reverses_id;
        IF already + abs(NEW.points) > abs(orig.points) THEN
            RAISE EXCEPTION 'loyalty: transaction % moved %; % already reversed, % more requested',
                orig.id, abs(orig.points), already, abs(NEW.points)
                USING ERRCODE = 'check_violation';
        END IF;
    END IF;

    -- A row that blames a void must point at an order that is, in fact, voided.
    IF NEW.source = 'void' THEN
        SELECT status::text INTO order_status FROM orders WHERE id = NEW.order_id;
        IF order_status IS DISTINCT FROM 'voided' THEN
            RAISE EXCEPTION 'loyalty: source is void but order % is %', NEW.order_id, COALESCE(order_status, 'missing')
                USING ERRCODE = 'check_violation';
        END IF;
    END IF;

    RETURN NEW;
END $$;
CREATE TRIGGER loyalty_txn_before_insert BEFORE INSERT ON loyalty_transactions
    FOR EACH ROW EXECUTE FUNCTION loyalty_txn_before_insert();

-- Append-only, enforced. There is no legitimate edit of a ledger row: a wrong
-- row is answered with a reversal or an adjustment that says so. The one
-- delete allowed is a tenant being erased — a demo organisation reaching its
-- TTL, or an org row already gone and cascading — because a ledger with no
-- shop is not a record of anything.
CREATE OR REPLACE FUNCTION loyalty_txn_is_append_only() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'TRUNCATE' THEN
        RAISE EXCEPTION 'loyalty_transactions is append-only and cannot be truncated'
            USING ERRCODE = 'insufficient_privilege';
    ELSIF TG_OP = 'UPDATE' THEN
        RAISE EXCEPTION 'loyalty_transactions is append-only: row % cannot be changed. Write a reversal (reverses_id) or an adjustment', OLD.id
            USING ERRCODE = 'insufficient_privilege';
    ELSIF EXISTS (SELECT 1 FROM organizations WHERE id = OLD.org_id AND NOT is_demo) THEN
        RAISE EXCEPTION 'loyalty_transactions is append-only: row % is history and cannot be deleted while its organisation exists', OLD.id
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    RETURN OLD;
END $$;
CREATE TRIGGER loyalty_txn_no_update_or_delete BEFORE UPDATE OR DELETE ON loyalty_transactions
    FOR EACH ROW EXECUTE FUNCTION loyalty_txn_is_append_only();
CREATE TRIGGER loyalty_txn_no_truncate BEFORE TRUNCATE ON loyalty_transactions
    FOR EACH STATEMENT EXECUTE FUNCTION loyalty_txn_is_append_only();

-- ── 7. Undoing a movement, and a void undoing all of them ────────────────────
-- ONE place knows how to reverse a row, so the void trigger below and the
-- refund handler to come apply the same policy. Returns the new row's id, or
-- NULL when there was nothing left to reverse — either the row is already
-- fully undone, or the programme clamps at zero and the member has nothing
-- left to take. A NULL is not an error: the void still happened, and the
-- unreversed remainder stays on the ledger for a report to find.
--
-- `p_amount` is a magnitude (always positive); NULL means "whatever remains".
CREATE OR REPLACE FUNCTION loyalty_reverse(
    p_txn    uuid,
    p_amount integer,
    p_source text,
    p_by     uuid,
    p_note   text
) RETURNS uuid LANGUAGE plpgsql AS $$
DECLARE
    orig      loyalty_transactions%ROWTYPE;
    already   integer;
    remaining integer;
    amount    integer;
    balance   integer;
    new_id    uuid;
BEGIN
    SELECT * INTO orig FROM loyalty_transactions WHERE id = p_txn FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'loyalty: transaction % not found', p_txn;
    END IF;
    IF p_amount IS NOT NULL AND p_amount <= 0 THEN
        RAISE EXCEPTION 'loyalty: a reversal amount is a positive magnitude, got %', p_amount;
    END IF;

    SELECT COALESCE(SUM(abs(points)), 0) INTO already
      FROM loyalty_transactions WHERE reverses_id = p_txn;
    remaining := abs(orig.points) - already;
    amount    := LEAST(COALESCE(p_amount, remaining), remaining);
    IF amount <= 0 THEN
        RETURN NULL;
    END IF;

    -- A clawback (undoing something that ADDED to the balance) is clamped to
    -- what the member still holds unless the programme lets balances go
    -- negative. Undoing a redeem or a deduction only ever gives back.
    IF orig.points > 0 AND NOT loyalty_allows_negative_balance(orig.org_id, orig.branch_id) THEN
        SELECT CASE WHEN orig.currency = 'points' THEN points_balance ELSE visits_balance END
          INTO balance
          FROM loyalty_customers WHERE id = orig.customer_id FOR UPDATE;
        amount := LEAST(amount, GREATEST(COALESCE(balance, 0), 0));
        IF amount <= 0 THEN
            RETURN NULL;
        END IF;
    END IF;

    INSERT INTO loyalty_transactions
        (org_id, customer_id, branch_id, kind, currency, points,
         order_id, reverses_id, source, created_by, note)
    VALUES
        (orig.org_id, orig.customer_id, orig.branch_id,
         ('reverse_' || orig.kind::text)::loyalty_txn_kind,
         orig.currency, -sign(orig.points) * amount,
         orig.order_id, orig.id, p_source, p_by, p_note)
    RETURNING id INTO new_id;
    RETURN new_id;
END $$;

COMMENT ON FUNCTION loyalty_reverse(uuid, integer, text, uuid, text) IS
    'Undo (part of) one ledger row under the programme''s clawback policy.
     Used by the void trigger; the refund feature should call it too rather
     than inserting reverse_* rows by hand. NULL return = nothing left to
     reverse, which is a fact for a report, not a failure.';

-- The defect this file exists for, closed at the only point every void passes
-- through. Whoever voids an order — the till, a replayed offline queue, an
-- admin — the ledger is put right in the same transaction, or the void fails.
--
-- Redemptions are undone first: handing back the points a reward cost raises
-- the balance, so the earn clawback that follows has more to take before the
-- clamp bites. Whatever cannot be clawed is left on the ledger, plainly
-- unreversed against an order that says `voided`.
--
-- Refunds are NOT handled here. `status = 'refunded'` says money went back,
-- not how much, and a partial refund claws back a share — that is the refund
-- feature's call, through `loyalty_reverse()` with `source = 'refund'`.
CREATE OR REPLACE FUNCTION loyalty_reverse_on_void() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    r record;
BEGIN
    FOR r IN
        SELECT id
          FROM loyalty_transactions
         WHERE order_id = NEW.id AND reverses_id IS NULL
         ORDER BY (points < 0) DESC, created_at, id
    LOOP
        PERFORM loyalty_reverse(r.id, NULL, 'void', NEW.voided_by, NEW.void_reason::text);
    END LOOP;
    RETURN NULL;
END $$;
CREATE TRIGGER orders_reverse_loyalty_on_void
    AFTER UPDATE OF status ON orders
    FOR EACH ROW
    WHEN (NEW.status = 'voided' AND OLD.status IS DISTINCT FROM 'voided')
    EXECUTE FUNCTION loyalty_reverse_on_void();

COMMENT ON TRIGGER orders_reverse_loyalty_on_void ON orders IS
    'A void undoes the loyalty it earned and gives back what it redeemed, in
     the voiding transaction, via loyalty_reverse(). Application code must NOT
     also write void reversals: the second writer would hit the "never more
     than the original" rule and fail the void.';
