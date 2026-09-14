-- Pricing fixes (PRICING_TAX_AUDIT.md, owner decisions 1, 3 and 5).
--
-- 1. A table's bill FREEZES the policy it is priced under when it is opened:
--    the tax rate, whether prices include tax, the service charge rate and
--    whether the service charge is taxed. A branch that changes a setting
--    mid-service must not re-price the bills already on its tables. NULL on a
--    ticket opened before this migration means "the branch's policy", which
--    is what those tickets have always been priced under.
-- 2. A waived service charge is recorded on the order: who, when, and what it
--    would have been. Only a dine-in bill can have one waived, and a waived
--    bill carries no charge.
-- 3. A refund records how much of its amount was tax and service charge, so a
--    partially refunded order stops reporting all of its tax. Filled by the
--    insert trigger from the order's own figures, pro rata and cumulatively
--    (the same arithmetic as `tax::refund_split` and the till's copy), so no
--    writer can skip it and every refund of an order adds up exactly.

-- ── 1. Frozen policy on the ticket ────────────────────────────────────────────
ALTER TABLE open_tickets
    ADD COLUMN tax_rate_applied               numeric(5,4),
    ADD COLUMN tax_inclusive_applied          boolean,
    ADD COLUMN service_charge_rate_applied    numeric(5,4),
    ADD COLUMN service_charge_taxable_applied boolean,
    ADD CONSTRAINT open_tickets_tax_rate_applied_is_a_fraction
        CHECK (tax_rate_applied IS NULL OR (tax_rate_applied >= 0 AND tax_rate_applied <= 1)),
    ADD CONSTRAINT open_tickets_service_charge_rate_applied_is_a_fraction
        CHECK (service_charge_rate_applied IS NULL
               OR (service_charge_rate_applied >= 0 AND service_charge_rate_applied <= 1)),
    -- All four or none: a half-frozen policy would mix today's settings into
    -- yesterday's bill.
    ADD CONSTRAINT open_tickets_policy_is_frozen_whole
        CHECK ((tax_rate_applied IS NULL) = (tax_inclusive_applied IS NULL)
               AND (tax_rate_applied IS NULL) = (service_charge_rate_applied IS NULL)
               AND (tax_rate_applied IS NULL) = (service_charge_taxable_applied IS NULL));

COMMENT ON COLUMN open_tickets.tax_rate_applied IS
    'The policy this bill is priced under, frozen when the ticket opened (with the three columns beside it). NULL = opened before 2026-09-16: the branch policy.';

-- ── 2. The waiver on the order ────────────────────────────────────────────────
ALTER TABLE orders
    ADD COLUMN service_charge_waived_by     uuid REFERENCES users(id) ON DELETE RESTRICT,
    ADD COLUMN service_charge_waived_at     timestamptz,
    ADD COLUMN service_charge_waived_amount integer NOT NULL DEFAULT 0,
    ADD CONSTRAINT orders_service_charge_waiver_is_whole
        CHECK ((service_charge_waived_by IS NULL) = (service_charge_waived_at IS NULL)),
    ADD CONSTRAINT orders_service_charge_waived_amount_nonneg
        CHECK (service_charge_waived_amount >= 0),
    ADD CONSTRAINT orders_only_a_waiver_has_a_waived_amount
        CHECK (service_charge_waived_by IS NOT NULL OR service_charge_waived_amount = 0),
    -- Only a table's bill carries a service charge, so only a table's bill can
    -- have one waived, and a waived bill carries none.
    ADD CONSTRAINT orders_service_charge_waiver_is_dine_in
        CHECK (service_charge_waived_by IS NULL
               OR (order_type = 'dine_in' AND service_charge_amount = 0));

CREATE INDEX idx_orders_service_charge_waived
    ON orders (branch_id, created_at) WHERE service_charge_waived_by IS NOT NULL;

-- ── 3. The tax and service a refund takes back ────────────────────────────────
ALTER TABLE order_refunds
    ADD COLUMN tax_amount            integer NOT NULL DEFAULT 0,
    ADD COLUMN service_charge_amount integer NOT NULL DEFAULT 0,
    ADD CONSTRAINT order_refunds_tax_amount_nonneg CHECK (tax_amount >= 0),
    ADD CONSTRAINT order_refunds_service_charge_amount_nonneg CHECK (service_charge_amount >= 0);

-- `tax::refund_split`: what the refunds up to and including this one should
-- take back, minus what the earlier ones did. Half away from zero, like every
-- other rounding on the bill (numeric `round` is half away from zero).
CREATE FUNCTION refund_share(figure integer, upto bigint, order_total integer) RETURNS integer
    LANGUAGE sql IMMUTABLE AS $$
    SELECT CASE WHEN order_total <= 0 THEN 0
                ELSE round(GREATEST(figure, 0)::numeric
                           * LEAST(GREATEST(upto, 0), order_total)::numeric
                           / order_total::numeric)::integer
           END
$$;

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

    -- The tax and service charge this refund takes back. Always computed here,
    -- never taken from the writer.
    NEW.tax_amount := refund_share(o.tax_amount, already + NEW.amount, o.total_amount)
                    - refund_share(o.tax_amount, already, o.total_amount);
    NEW.service_charge_amount := refund_share(o.service_charge_amount, already + NEW.amount, o.total_amount)
                               - refund_share(o.service_charge_amount, already, o.total_amount);

    RETURN NEW;
END;
$$;

-- Existing refunds, in the order they were issued. `order_refunds` is
-- append-only by trigger; this one backfill of two derived columns is the
-- sanctioned exception, and the guard is back on before the migration ends.
ALTER TABLE order_refunds DISABLE TRIGGER order_refunds_no_update_or_delete;
WITH ordered AS (
    SELECT r.id, o.tax_amount AS o_tax, o.service_charge_amount AS o_sc, o.total_amount AS o_total,
           COALESCE(SUM(r.amount) OVER (PARTITION BY r.order_id ORDER BY r.created_at, r.id
                                        ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING), 0) AS before,
           SUM(r.amount) OVER (PARTITION BY r.order_id ORDER BY r.created_at, r.id) AS upto
      FROM order_refunds r JOIN orders o ON o.id = r.order_id
)
UPDATE order_refunds r
   SET tax_amount = refund_share(x.o_tax, x.upto, x.o_total) - refund_share(x.o_tax, x.before, x.o_total),
       service_charge_amount = refund_share(x.o_sc, x.upto, x.o_total) - refund_share(x.o_sc, x.before, x.o_total)
  FROM ordered x
 WHERE x.id = r.id AND (x.o_tax <> 0 OR x.o_sc <> 0);
ALTER TABLE order_refunds ENABLE TRIGGER order_refunds_no_update_or_delete;

-- The per-order refund view gains the two splits (appended columns, so every
-- existing reader is untouched), which is what the reports subtract.
CREATE OR REPLACE VIEW v_order_refund_totals WITH (security_invoker = true) AS
SELECT order_id,
       SUM(amount)::bigint                         AS refunded_amount,
       SUM(amount) FILTER (WHERE is_cash)::bigint  AS refunded_cash,
       COUNT(*)::bigint                            AS refund_count,
       MIN(issued_at)                              AS first_refund_at,
       MAX(issued_at)                              AS last_refund_at,
       SUM(tax_amount)::bigint                     AS refunded_tax,
       SUM(service_charge_amount)::bigint          AS refunded_service_charge
  FROM order_refunds
 GROUP BY order_id;

-- ── 4. Default grants for `orders:waive_service` ──────────────────────────────
-- Managers yes, everyone else no — explicit rows either way, so the permission
-- editor shows the default instead of "no rule". The seeder inserts the same
-- rows at boot; ON CONFLICT keeps whatever an admin has already changed.
INSERT INTO role_permissions (role, resource, action, granted) VALUES
    ('org_admin',      'orders', 'waive_service', true),
    ('branch_manager', 'orders', 'waive_service', true),
    ('teller',         'orders', 'waive_service', false),
    ('waiter',         'orders', 'waive_service', false),
    ('kitchen',        'orders', 'waive_service', false)
ON CONFLICT (role, resource, action) DO NOTHING;
