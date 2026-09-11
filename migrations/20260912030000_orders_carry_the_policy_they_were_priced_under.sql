-- An order carries the policy it was priced under, and its money adds up.
--
-- `orders` is the books. Every report, reprint, export and shift close reads
-- it, and four things about it could only be answered by guessing:
--
-- 1. WHICH TICKET this sale settled. A settled ticket points at its order
--    (`open_tickets.order_id`), but the order does not point back — the link
--    only exists as the convention that the ticket's id was used as the
--    order's `idempotency_key`. That is a dedup key doing a second job it was
--    never named for: nobody reading `orders` can tell a table sale from a
--    counter sale without knowing the trick, and a JOIN on it costs a comment
--    at every use site. 3,227 orders in production were settled this way.
--
-- 2. WHETHER THE SERVICE CHARGE WAS TAXED. The tax migration recorded the two
--    rates and the inclusivity flag on the order, but not the fourth half of
--    the policy — `service_charge_taxable`. A reader reconstructing the tax
--    line of an order with a service charge had to look up TODAY's org
--    setting, which is precisely the read-time lookup the applied columns
--    exist to remove.
--
-- 3. WHAT KIND OF SALE IT WAS. `order_type` allowed `dine_in` and `delivery`
--    and nothing else, so a takeaway rung up at the counter was recorded as
--    dine-in. That was harmless while both were priced identically. The
--    owner has now ruled that the service charge is dine-in only, and the
--    engine cannot decide not to charge one for a kind of sale the row
--    cannot express.
--
-- 4. WHETHER THE FIGURES AGREE. Nothing in the database said that
--    `total_amount` is what the other columns add up to, or that a discount
--    cannot exceed the bill. The server computes the bill and then writes
--    its own answer, so today every row agrees with itself — all 9,931 in
--    the local copy of production do — but a table this central should
--    refuse a row that does not, rather than let the next writer that
--    forgets a term become a reconciliation problem in March.
--
-- And one lie to stop telling: `payment_method` on a split sale. Eight real
-- orders were paid with two tenders. Two say `mixed`, because newer tills
-- send that label; six say `cash` or `card`, whichever leg was larger,
-- because older tills sent the biggest leg and the server recorded it. The
-- money reports already ignore the label and bucket by the legs, so no
-- figure is wrong — but "card" printed on a receipt that was half cash is a
-- statement about the customer that the database can see is false. The six
-- historic rows are left as recorded; from here on the legs decide.
--
-- REFUNDS are coming as their own feature and are NOT built here. What this
-- migration does is refuse to paint them into a corner: a refund is money
-- going OUT, against a sale that stands as rung up. So `total_amount` stays
-- the sale, a payment leg is money IN and can never be negative, and the
-- `refunded` status value already in `order_status` is left exactly as it is.
-- A refund will be its own row against the order, with its own legs.

-- ── The ticket this order settled ───────────────────────────────────────────
ALTER TABLE orders
    ADD COLUMN open_ticket_id uuid REFERENCES open_tickets(id) ON DELETE SET NULL,
    ADD COLUMN service_charge_taxable_applied boolean;

-- Backfill from the convention this column replaces. Every settled ticket
-- carries its id as the order's idempotency key, and in the local copy of
-- production all 3,227 such orders agree with `open_tickets.order_id` — zero
-- disagreements — so the join is safe to trust.
--
-- The `updated_at` triggers are suspended for the one statement. `updated_at`
-- is what a till polls with `?updated_after=` to catch up after being
-- offline; stamping 3,227 historic orders as edited tonight would make every
-- device re-download three thousand sales that did not change. Recording a
-- link that was always true is not an edit.
ALTER TABLE orders DISABLE TRIGGER USER;

UPDATE orders o
   SET open_ticket_id = t.id
  FROM open_tickets t
 WHERE t.id = o.idempotency_key
   AND o.open_ticket_id IS NULL;

ALTER TABLE orders ENABLE TRIGGER USER;

-- A ticket settles into at most one order. The settle path already dedups on
-- the idempotency key; this states the same fact where a path that forgot
-- the key cannot bypass it.
CREATE UNIQUE INDEX uq_orders_open_ticket
    ON orders (open_ticket_id)
    WHERE open_ticket_id IS NOT NULL;

COMMENT ON COLUMN orders.open_ticket_id IS
    'The floor ticket this sale settled, when it came from one. NULL for a '
    'counter sale, a takeaway or a delivery. Historically recoverable only '
    'as `idempotency_key = open_tickets.id`; recorded outright since 2026-09.';

-- ── The whole policy, not three quarters of it ──────────────────────────────
COMMENT ON COLUMN orders.service_charge_taxable_applied IS
    'Whether the service charge entered the tax base when this order was '
    'priced. NULL on rows predating the column — do not backfill with '
    'today''s setting; the tax line is already what it was.';

-- Mirrors the org and branch guards. `numeric(5,4)` would accept `14`.
ALTER TABLE orders
    ADD CONSTRAINT orders_tax_rate_applied_is_a_fraction
        CHECK (tax_rate_applied IS NULL
               OR (tax_rate_applied >= 0 AND tax_rate_applied <= 1)),
    ADD CONSTRAINT orders_service_charge_rate_applied_is_a_fraction
        CHECK (service_charge_rate_applied IS NULL
               OR (service_charge_rate_applied >= 0 AND service_charge_rate_applied <= 1));

-- ── A takeaway is not a dine-in ─────────────────────────────────────────────
ALTER TABLE orders
    DROP CONSTRAINT orders_order_type_chk,
    ADD CONSTRAINT orders_order_type_is_known
        CHECK (order_type IN ('dine_in', 'takeaway', 'delivery'));

COMMENT ON COLUMN orders.order_type IS
    'dine_in = eaten on the premises, the only kind that carries a service '
    'charge; takeaway = rung up at the counter and carried out; delivery = '
    'settled from a delivery_order (a pickup is a delivery channel, not a '
    'takeaway). Rows before 2026-09 say dine_in for every till sale because '
    'takeaway could not be expressed.';

-- ── The money is self-consistent ────────────────────────────────────────────
--
-- These are the identities the tax engine produces, stated so a row that
-- breaks them is refused instead of recorded. Measured against the local
-- copy of production before being written: 9,931 of 9,931 rows pass every
-- one. Deliberately NOT stated: `amount_tendered` and `change_given`. 35 rows
-- disagree with each other there — a till that rounded change, a tip taken
-- from the tendered cash — and that is a recording of what happened at the
-- drawer, not a figure the server derives.
ALTER TABLE orders
    ADD CONSTRAINT orders_money_is_not_negative
        CHECK (subtotal >= 0 AND discount_amount >= 0 AND tax_amount >= 0
               AND total_amount >= 0 AND (tip_amount IS NULL OR tip_amount >= 0)),

    ADD CONSTRAINT orders_discount_does_not_exceed_subtotal
        CHECK (discount_amount <= subtotal),

    -- Exclusive: tax is added on top. Inclusive: the tax is already inside the
    -- base and the receipt breaks it out, so it does not add again. The
    -- delivery fee rides outside the tax base in both. The tip is not part of
    -- the bill at all — it is not in the legs and not in the total.
    ADD CONSTRAINT orders_total_is_the_sum_of_its_parts
        CHECK (total_amount = subtotal - discount_amount + service_charge_amount
                              + delivery_fee
                              + CASE WHEN tax_inclusive THEN 0 ELSE tax_amount END),

    -- An inclusive tax line cannot exceed what it is said to be inside of.
    ADD CONSTRAINT orders_inclusive_tax_fits_inside_the_bill
        CHECK (NOT tax_inclusive
               OR tax_amount <= subtotal - discount_amount + service_charge_amount),

    -- A charge that was applied has a nonzero rate on record.
    ADD CONSTRAINT orders_service_charge_needs_a_rate
        CHECK (service_charge_amount = 0
               OR (service_charge_rate_applied IS NOT NULL AND service_charge_rate_applied > 0)),

    -- The owner's rule, stated outright: the service charge is dine-in only.
    -- A takeaway or a delivery carries neither the charge nor a rate for one.
    -- The rate a delivery was priced under IS zero whatever the branch setting
    -- says, so a row recording 0.10 beside a charge of 0 would be lying about
    -- its own policy, and the rate is pinned with the amount. The worry that
    -- history was priced under a policy of its day and would refuse this was
    -- measured before it was written: no order of any type in production has
    -- ever carried a service charge or a nonzero rate, so there is no history
    -- to restate. `delivery_orders` pins the same two figures the same way on
    -- the quote; this is the books agreeing with it.
    ADD CONSTRAINT orders_service_charge_is_dine_in_only
        CHECK (order_type = 'dine_in'
               OR (service_charge_amount = 0
                   AND COALESCE(service_charge_rate_applied, 0) = 0)),

    -- Same for tax, with the one escape history needs: a taxed order from
    -- before the rate was recorded has `tax_rate_applied IS NULL`, and that
    -- is the truth about it.
    ADD CONSTRAINT orders_tax_needs_a_rate
        CHECK (tax_amount = 0 OR tax_rate_applied IS NULL OR tax_rate_applied > 0);

COMMENT ON COLUMN orders.total_amount IS
    'What the customer was charged for this sale: subtotal - discount + '
    'service charge + delivery fee, plus tax when exclusive. Excludes the '
    'tip. It is the SALE and does not move afterwards — a void marks the row, '
    'a refund is its own event against it.';

-- ── A payment leg is money in ───────────────────────────────────────────────
--
-- 376 legs in the local copy are exactly zero — a bill discounted to nothing
-- still records how it was "paid" — and none are negative. Keeping it that
-- way is what leaves the refund feature a clean room: a refund is not a
-- negative leg on the sale, and the shift's drawer maths (which sums cash
-- legs) will never have to learn to subtract from this table.
ALTER TABLE order_payments
    ADD CONSTRAINT order_payments_leg_is_money_in
        CHECK (amount >= 0);

-- ── The legs decide whether a sale is mixed ─────────────────────────────────
--
-- A trigger rather than a generated column, because a generated column
-- cannot read another table and the legs live in one. It runs on INSERT
-- only: the legs are written once, immediately after the order row, so
-- there is no later edit to track — and an insert-only rule cannot touch
-- the six historic rows, which stay as their till recorded them.
--
-- The label is nominal. No money report reads it; they bucket by the legs.
-- It is the badge on the receipt and the order list, and the badge should
-- not say "card" about a sale that was half cash.
CREATE OR REPLACE FUNCTION order_payments_mark_mixed() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    -- A zero-amount leg is not a way the customer paid, so it cannot make a
    -- sale mixed on its own.
    IF NEW.amount > 0 THEN
        UPDATE orders
           SET payment_method = 'mixed'
         WHERE id = NEW.order_id
           AND payment_method <> 'mixed'
           AND EXISTS (SELECT 1 FROM order_payments p
                        WHERE p.order_id = NEW.order_id
                          AND p.amount > 0
                          AND p.method <> NEW.method);
    END IF;
    RETURN NULL;
END;
$$;

CREATE TRIGGER order_payments_mark_mixed
    AFTER INSERT ON order_payments
    FOR EACH ROW EXECUTE FUNCTION order_payments_mark_mixed();

COMMENT ON COLUMN orders.payment_method IS
    'The nominal label: the single tender, or the literal ''mixed'' once the '
    'legs in order_payments name more than one. A display badge, not a money '
    'bucket — reports sum the legs. Kept in step by order_payments_mark_mixed '
    'for orders from 2026-09 on; six older split sales carry their largest leg.';
