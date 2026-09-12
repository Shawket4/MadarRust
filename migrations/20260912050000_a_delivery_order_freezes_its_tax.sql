-- A delivery order freezes the tax it was quoted under, the way it already
-- freezes everything else.
--
-- `delivery_orders` is the quote the customer agreed to on the ordering site,
-- and it is a good freeze: the priced cart, the ingredient deductions, the
-- channel discount and the fee are all copied onto the row at intake so that
-- finalize, hours later and on a different shift, replays the same bill. Tax
-- was the one figure left out. Intake wrote `total = subtotal - discount +
-- fee` with no tax in it at all; finalize priced the same cart through the
-- tax engine under the branch's policy of THAT moment and booked
-- `engine.total + fee` — and never wrote the result back. Two totals for one
-- sale, and the row the customer was shown carried the smaller one.
--
-- Nobody has noticed because every organisation currently has a tax rate of
-- zero, so both numbers agree. The schema default for a new organisation is
-- 14% exclusive. That is one signup away from a customer agreeing to 100 on
-- the site and being charged 114 at the door, with the receipt disagreeing
-- with the WhatsApp message — and the books, which read the orders row, would
-- be right, while every screen that reads this table would be wrong.
--
-- So the row now carries what the orders row carries: the tax amount and the
-- rate and inclusivity it was priced under, recorded at intake and replayed
-- verbatim at finalize. Finalize must stop pricing. A rate the shop changes
-- between the quote and the door does not move a bill the customer already
-- agreed, which is the same reason `orders.tax_rate_applied` exists.
--
-- Three rulings from the owner shape what is NOT here:
--
--   * Inclusivity is ONE flag — `organizations.tax_inclusive`, branch
--     override, NULL inherits — and it governs online as much as the till.
--     There is no online-specific flag. The frozen `tax_inclusive` below is
--     a copy of whichever value was in force, not a second setting.
--   * The service charge is dine-in only. A delivery or a pickup never
--     carries one, so its amount and rate on this row are pinned at zero by
--     CHECK rather than by everyone remembering. The columns still exist,
--     because finalize replays a whole policy from this row and the orders
--     row it produces states its service-charge rate; a zero here is the
--     honest value of that rate. `service_charge_taxable` is deliberately
--     not recorded: with the rate pinned at zero it cannot influence a
--     figure, and a column that records a policy that cannot apply is a
--     column that lies the day someone reads it.
--   * The delivery fee stays outside the tax base. It is a carriage charge
--     on a sale, not part of the sale, and the identity below adds it after
--     the tax has been settled.
--
-- Two smaller lies stop here as well. `payment_method_hint` is what the
-- customer SAID they would pay with, validated to cash or card at intake — and
-- finalize overwrote it with what the teller actually took, which is how two
-- rows now say `digital_wallet`, a value the intake validator refuses. What
-- was paid gets its own column and the hint is left to mean what it says.
-- And a branch had no way to give up on an order nobody accepted: a
-- `received` row sat there until someone noticed. The timeout after which
-- the sweeper rejects it is a per-branch setting, NULL meaning never.
--
-- REFUNDS are a coming feature and nothing here presumes their shape, but
-- nothing here obstructs it either: `total` on this row is the quote, the
-- sale it became is the orders row, and a refund is an event against that
-- sale. This table never learns about money going back out.

-- ── The frozen policy ───────────────────────────────────────────────────────
--
-- Added nullable, backfilled, then tightened: the three figures the engine
-- produces have NO default. An intake that forgets to price tax must be
-- refused, not quietly recorded as tax-free — a default of zero here would
-- reproduce the exact defect this migration removes, one column over.
ALTER TABLE delivery_orders
    ADD COLUMN tax_amount                  integer,
    ADD COLUMN tax_rate_applied            numeric(5,4),
    ADD COLUMN tax_inclusive               boolean,
    ADD COLUMN service_charge_amount       integer      NOT NULL DEFAULT 0,
    ADD COLUMN service_charge_rate_applied numeric(5,4) NOT NULL DEFAULT 0,
    ADD COLUMN payment_method              text,
    ADD COLUMN distance_source             text;

ALTER TABLE branch_delivery_settings
    ADD COLUMN auto_reject_minutes integer;

-- ── Backfill ────────────────────────────────────────────────────────────────
--
-- Every rate at every organisation and branch is zero today, and intake never
-- priced tax at all, so the frozen figures of every existing row are zero:
-- tax 0, rate 0, and the inclusivity flag the branch was under. That is the
-- truth about those quotes and it is written out explicitly — a NULL would say
-- "unknown", and it is not unknown.
--
-- The guards below exist for the day this runs somewhere that is NOT the
-- case. A live quote at a branch whose rate is no longer zero is short by the
-- tax and WILL be finalised; writing "priced at 0" onto it is accurate but
-- leaves a bill about to be charged wrong, and a person must decide whether
-- to re-quote it. The migration stops rather than decide.
DO $$
DECLARE
    bad record;
BEGIN
    -- A quote still on its way to the door, at a branch that now taxes.
    SELECT d.id, d.delivery_ref, d.branch_id INTO bad
      FROM delivery_orders d
      JOIN branches b       ON b.id = d.branch_id
      JOIN organizations o  ON o.id = b.org_id
     WHERE d.status NOT IN ('delivered', 'cancelled', 'rejected')
       AND (COALESCE(b.tax_rate, o.tax_rate) <> 0
            OR COALESCE(b.service_charge_rate, o.service_charge_rate) <> 0)
     LIMIT 1;
    IF FOUND THEN
        RAISE EXCEPTION
            'delivery order % (%) is still live at branch % whose tax policy is no longer zero; '
            'its quote was taken without tax and must be re-priced by the engine before this '
            'migration can freeze it',
            bad.id, bad.delivery_ref, bad.branch_id;
    END IF;

    -- A delivered row inherits its figures from the sale it became. If that
    -- sale carried tax without recording the rate (an orders row from before
    -- `tax_rate_applied` existed, at a shop that has since zeroed its rate),
    -- the rate cannot be recovered and a person must supply it.
    SELECT d.id, d.delivery_ref INTO bad
      FROM delivery_orders d
      JOIN orders o ON o.id = d.order_id
     WHERE o.tax_amount > 0 AND o.tax_rate_applied IS NULL
     LIMIT 1;
    IF FOUND THEN
        RAISE EXCEPTION
            'delivery order % (%) settled into a sale that carries tax but no recorded rate; '
            'supply the rate by hand before freezing it',
            bad.id, bad.delivery_ref;
    END IF;

    -- A sale that was somehow charged a service charge on a delivery. None
    -- exists; the CHECK below would refuse it, and this says why first.
    SELECT d.id, d.delivery_ref INTO bad
      FROM delivery_orders d
      JOIN orders o ON o.id = d.order_id
     WHERE o.service_charge_amount <> 0
     LIMIT 1;
    IF FOUND THEN
        RAISE EXCEPTION
            'delivery order % (%) settled into a sale carrying a service charge, which a '
            'delivery cannot; reconcile the sale before freezing it',
            bad.id, bad.delivery_ref;
    END IF;
END $$;

-- Delivered: the sale is what was actually charged and is already in the
-- books, so the quote takes the sale's figures, total included. In the local
-- copy of production all 8 delivered rows already agree with their sale to
-- the piastre, so no total moves; the assignment is written anyway because
-- the rule is "the sale wins", not "they happen to agree".
--
-- No `updated_at` bump: recording what was always true of a row is not an
-- edit, and the till does not need to re-download it.
UPDATE delivery_orders d
   SET tax_amount        = o.tax_amount,
       tax_rate_applied  = COALESCE(o.tax_rate_applied, 0),
       tax_inclusive     = o.tax_inclusive,
       total             = o.total_amount,
       payment_method    = o.payment_method
  FROM orders o
 WHERE o.id = d.order_id;

-- A delivered row whose sale has since been deleted (a shift purge, the demo
-- sweeper — `order_id` is SET NULL, and the rule below deliberately allows the
-- severed state) has no orders row to copy from. It is not unpaid: finalize
-- wrote what the teller took into the hint, so on such a row the hint IS the
-- payment. None exists in the local copy of production; without this the
-- `paid_when_delivered` rule would refuse the whole migration on the first one.
UPDATE delivery_orders
   SET payment_method = payment_method_hint
 WHERE status = 'delivered'
   AND payment_method IS NULL
   AND payment_method_hint IS NOT NULL;

-- Everything else — live, cancelled, rejected, or delivered but severed from
-- its sale — was quoted with no tax in it under a zero rate. The inclusivity
-- flag is the one in force at the branch; with a zero rate it changes no
-- figure, but the row should say what policy it sat under rather than a
-- placeholder.
UPDATE delivery_orders d
   SET tax_amount       = 0,
       tax_rate_applied = 0,
       tax_inclusive    = COALESCE(b.tax_inclusive, o.tax_inclusive)
  FROM branches b
  JOIN organizations o ON o.id = b.org_id
 WHERE b.id = d.branch_id
   AND d.tax_amount IS NULL;

ALTER TABLE delivery_orders
    ALTER COLUMN tax_amount       SET NOT NULL,
    ALTER COLUMN tax_rate_applied SET NOT NULL,
    ALTER COLUMN tax_inclusive    SET NOT NULL;

-- ── The figures agree with each other ───────────────────────────────────────
--
-- The same identities `orders` holds, stated here so a quote that breaks them
-- is refused at intake instead of discovered at the door. Measured against
-- the local copy of production before being written: 34 of 34 rows pass.
ALTER TABLE delivery_orders
    -- Mirrors the org, branch and orders guards. `numeric(5,4)` would take 14.
    ADD CONSTRAINT delivery_orders_tax_rate_applied_is_a_fraction
        CHECK (tax_rate_applied >= 0 AND tax_rate_applied <= 1),

    ADD CONSTRAINT delivery_orders_tax_is_not_negative
        CHECK (tax_amount >= 0),

    -- The database's half of "service charge is dine-in only". Not a
    -- convention the engine follows; a row that carries one is refused.
    ADD CONSTRAINT delivery_orders_carry_no_service_charge
        CHECK (service_charge_amount = 0 AND service_charge_rate_applied = 0),

    -- Exclusive: tax is added on top. Inclusive: it is already inside the
    -- base and the receipt breaks it out. The fee rides outside in both.
    -- `service_charge_amount` is in the sum even though it is pinned at
    -- zero, so the identity reads the same as the one on `orders` and does
    -- not need rewriting if the pin is ever lifted.
    ADD CONSTRAINT delivery_orders_total_is_the_sum_of_its_parts
        CHECK (total = subtotal - discount_amount + service_charge_amount + delivery_fee
                       + CASE WHEN tax_inclusive THEN 0 ELSE tax_amount END),

    -- An inclusive tax line cannot exceed what it is said to be inside of.
    ADD CONSTRAINT delivery_orders_inclusive_tax_fits_inside_the_bill
        CHECK (NOT tax_inclusive
               OR tax_amount <= subtotal - discount_amount + service_charge_amount),

    -- A tax that was charged has a nonzero rate on record. No NULL escape
    -- here, unlike `orders`: this table has no history from before the rate
    -- was recorded, because the rate was zero for all of it.
    ADD CONSTRAINT delivery_orders_tax_needs_a_rate
        CHECK (tax_amount = 0 OR tax_rate_applied > 0),

    -- The discount trio is frozen at intake like everything else, and it
    -- follows the schema-wide rule that a percentage is a fraction. The
    -- existing `delivery_orders_discount_nonneg` holds the lower bound and
    -- `discount_amount <= subtotal`; this adds the upper bound, and says that
    -- a row with no discount has no discount figures either.
    ADD CONSTRAINT delivery_orders_percentage_discount_is_a_fraction
        CHECK (discount_type IS DISTINCT FROM 'percentage' OR discount_value <= 1),
    ADD CONSTRAINT delivery_orders_no_discount_means_no_discount
        CHECK (discount_type IS NOT NULL OR (discount_value = 0 AND discount_amount = 0));

COMMENT ON COLUMN delivery_orders.tax_amount IS
    'Tax on the quote as priced at intake, under tax_rate_applied and '
    'tax_inclusive. Inside `total` when inclusive, added to it when '
    'exclusive. Replayed verbatim at finalize — finalize does not re-price.';
COMMENT ON COLUMN delivery_orders.tax_rate_applied IS
    'Fraction, not a percentage: 0.14 means 14%. The rate in force at the '
    'branch when the quote was taken. Rows before 2026-09 say 0, which is '
    'what every branch charged and what intake priced.';
COMMENT ON COLUMN delivery_orders.tax_inclusive IS
    'Copy of the ONE inclusivity flag (organizations.tax_inclusive, branch '
    'override) as it stood at intake. Not a setting of its own.';
COMMENT ON COLUMN delivery_orders.service_charge_amount IS
    'Always 0: the service charge is dine-in only. Present so the identity '
    'on `total` reads the same as the one on `orders`.';
COMMENT ON COLUMN delivery_orders.service_charge_rate_applied IS
    'Always 0, for the same reason as service_charge_amount. Finalize '
    'replays the whole policy from this row and the sale states its rate.';

-- ── What was paid, and what the customer said they would pay ────────────────
ALTER TABLE delivery_orders
    ADD CONSTRAINT delivery_orders_payment_method_is_not_blank
        CHECK (payment_method IS NULL OR btrim(payment_method) <> ''),

    -- Money changes hands exactly once, at the door, and that is when the
    -- row becomes `delivered`. Before that nothing was paid; after that
    -- something was. Both directions hold, and neither is disturbed by a
    -- later deletion of the sale.
    ADD CONSTRAINT delivery_orders_paid_when_delivered
        CHECK ((payment_method IS NOT NULL) = (status = 'delivered'));

COMMENT ON COLUMN delivery_orders.payment_method IS
    'What was actually taken at the door, in the org''s payment-method '
    'vocabulary (the same values orders.payment_method uses). Set at '
    'finalize and only then. Not the customer''s hint.';
COMMENT ON COLUMN delivery_orders.payment_method_hint IS
    'What the customer said at checkout they would pay with: cash or card. '
    'Display-only. Finalize used to overwrite it with the real method, which '
    'is why two rows from before 2026-09 say digital_wallet; they are left '
    'as recorded, since the hint they replaced is gone.';

-- ── The sale a quote became ─────────────────────────────────────────────────
--
-- A quote settles into at most one sale. The finalize path already refuses a
-- second finalize under `FOR UPDATE`; this states the same fact where a path
-- that forgot the lock cannot bypass it. The plain index it replaces served
-- the same lookups and is redundant beside a unique one.
DROP INDEX IF EXISTS idx_delivery_orders_order;
CREATE UNIQUE INDEX uq_delivery_orders_order
    ON delivery_orders (order_id)
    WHERE order_id IS NOT NULL;

-- A sale link only ever exists on a delivered row: finalize is the one path
-- that writes `order_id` and it sets `delivered` in the same statement.
-- Deliberately NOT the converse. `DELETE FROM orders WHERE shift_id = $1`
-- (deleting a shift that holds only voided sales) SETs this NULL, and a
-- delivered order whose voided sale was later removed is a true state, not a
-- corrupt one. Requiring a sale on every delivered row would make that
-- shift deletion fail with a message about delivery orders.
ALTER TABLE delivery_orders
    ADD CONSTRAINT delivery_orders_sale_means_delivered
        CHECK (order_id IS NULL OR status = 'delivered');

-- ── Who cancelled it ────────────────────────────────────────────────────────
--
-- `cancelled_by` was a bare uuid. Every one of the 25 recorded values in the
-- local copy of production is a real user, so the reference is only stating
-- what is already true. SET NULL rather than RESTRICT: users are soft-deleted
-- and the hard delete, when it happens, must not be blocked by a cancellation
-- someone recorded a year ago.
ALTER TABLE delivery_orders
    ADD CONSTRAINT delivery_orders_cancelled_by_fkey
        FOREIGN KEY (cancelled_by) REFERENCES users(id) ON DELETE SET NULL;

-- ── Where the distance came from ────────────────────────────────────────────
--
-- An outside order's fee is the zone ring its road distance falls in. When
-- the routing service is down, intake falls back to the straight line, which
-- can be a good deal shorter than the road, and a ring matched on it is a
-- fee the shop did not set for that address. In-mall records a walking
-- distance as a spam signal, and that is a straight line by design. Until
-- now the row could not say which it had; a distance whose provenance is
-- unknown is one nobody can act on.
--
-- ROWS THAT ALREADY CARRY A DISTANCE GET `legacy`, and this migration failed
-- in production for want of it. An earlier draft asserted "no existing row
-- records a distance at all" — true of the development copy it was checked
-- against, and false of production, which had drifted. The deploy got as far
-- as ATRewriteTable and stopped, so nothing shipped.
--
-- `legacy` rather than guessing 'haversine': we do not know how those
-- distances were measured, and writing a provenance we invented is precisely
-- what this pair of constraints exists to prevent. A fee dispute on an old
-- order should read "recorded before we tracked how", not a confident answer
-- nobody checked. Intake never writes it, so the value can only shrink.
UPDATE delivery_orders
   SET distance_source = 'legacy'
 WHERE road_distance_meters IS NOT NULL
   AND distance_source IS NULL;

ALTER TABLE delivery_orders
    ADD CONSTRAINT delivery_orders_distance_source_is_known
        CHECK (distance_source IS NULL OR distance_source IN ('osrm', 'haversine', 'legacy')),
    ADD CONSTRAINT delivery_orders_distance_has_a_source
        CHECK ((distance_source IS NULL) = (road_distance_meters IS NULL));

COMMENT ON COLUMN delivery_orders.distance_source IS
    'How road_distance_meters was measured: osrm = routed road distance, '
    'haversine = straight line (the routing fallback, and always the in-mall '
    'walking distance), legacy = recorded before provenance was tracked. '
    'NULL exactly when no distance was recorded.';

-- ── Giving up on an order nobody accepted ───────────────────────────────────
ALTER TABLE branch_delivery_settings
    ADD CONSTRAINT bds_auto_reject_is_positive
        CHECK (auto_reject_minutes IS NULL OR auto_reject_minutes > 0);

COMMENT ON COLUMN branch_delivery_settings.auto_reject_minutes IS
    'Minutes a `received` order may wait for a teller before the sweeper '
    'rejects it and tells the customer. NULL = never; the order waits until '
    'someone acts on it. Read at sweep time, not frozen on the order: a '
    'branch that shortens it wants the change to apply to what is waiting.';
