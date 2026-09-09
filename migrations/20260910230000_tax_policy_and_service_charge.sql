-- Tax that a shop can actually configure, and a service charge that exists.
--
-- Three things were wrong and this fixes the schema half of all of them.
--
-- 1. `organizations.tax_rate` is a FRACTION (0.14 = 14%) and always was, but
--    the dashboard labelled it "Tax Rate (%)" and rendered it with a percent
--    sign. Typing what the label asked for was rejected by the backend's
--    `0..=1` guard; typing `0.14` displayed as "0.14%". There was no value
--    that both saved and read correctly. The unit stays a fraction — it is
--    what every consumer multiplies by — and a CHECK now states it in the
--    database rather than only in a handler, so the next writer cannot
--    reintroduce a percentage from a path nobody thought about.
--
-- 2. Menu prices could only ever be tax-exclusive. Shops that quote the paid
--    price on the board had no way to say so, and their receipts overstated
--    the bill by the tax.
--
-- 3. A service charge did not exist, though the accounting export has been
--    publishing a hard-coded `service_charge: 0` for every order.
--
-- Branches may override the org, for organisations trading across more than
-- one jurisdiction. NULL means inherit — not "zero" — so an org-wide change
-- still reaches every branch that never asked to differ.

-- ── Org-level policy ────────────────────────────────────────────────────────
ALTER TABLE organizations
    ADD COLUMN tax_inclusive           boolean       NOT NULL DEFAULT false,
    ADD COLUMN service_charge_rate     numeric(5,4)  NOT NULL DEFAULT 0,
    ADD COLUMN service_charge_taxable  boolean       NOT NULL DEFAULT true;

-- The guard the handler already applied, now where it cannot be bypassed.
-- `numeric(5,4)` tops out at 9.9999, so without this a percentage would fit.
ALTER TABLE organizations
    ADD CONSTRAINT organizations_tax_rate_is_a_fraction
        CHECK (tax_rate >= 0 AND tax_rate <= 1),
    ADD CONSTRAINT organizations_service_charge_is_a_fraction
        CHECK (service_charge_rate >= 0 AND service_charge_rate <= 1);

COMMENT ON COLUMN organizations.tax_rate IS
    'Fraction, NOT a percentage: 0.14 means 14%. Guarded by a CHECK.';
COMMENT ON COLUMN organizations.tax_inclusive IS
    'true = menu prices already contain the tax and the receipt breaks it out.';
COMMENT ON COLUMN organizations.service_charge_taxable IS
    'true = the service charge enters the tax base.';

-- ── Per-branch override (NULL = inherit the org) ────────────────────────────
ALTER TABLE branches
    ADD COLUMN tax_rate                numeric(5,4),
    ADD COLUMN tax_inclusive           boolean,
    ADD COLUMN service_charge_rate     numeric(5,4),
    ADD COLUMN service_charge_taxable  boolean;

ALTER TABLE branches
    ADD CONSTRAINT branches_tax_rate_is_a_fraction
        CHECK (tax_rate IS NULL OR (tax_rate >= 0 AND tax_rate <= 1)),
    ADD CONSTRAINT branches_service_charge_is_a_fraction
        CHECK (service_charge_rate IS NULL OR (service_charge_rate >= 0 AND service_charge_rate <= 1));

COMMENT ON COLUMN branches.tax_rate IS
    'Overrides the org rate. NULL = inherit, which is not the same as 0.';

-- ── What was actually applied, kept on the order ────────────────────────────
--
-- The rate is recorded per order, not looked up at read time. A shop that
-- changes its rate in March must not restate February: every report, every
-- reprint and every accounting export reads the figures the customer was
-- actually charged under the policy in force that day.
ALTER TABLE orders
    ADD COLUMN service_charge_amount       integer      NOT NULL DEFAULT 0,
    ADD COLUMN tax_rate_applied            numeric(5,4),
    ADD COLUMN service_charge_rate_applied numeric(5,4),
    ADD COLUMN tax_inclusive               boolean      NOT NULL DEFAULT false;

ALTER TABLE orders
    ADD CONSTRAINT orders_service_charge_is_not_negative
        CHECK (service_charge_amount >= 0);

COMMENT ON COLUMN orders.tax_rate_applied IS
    'The rate in force when this order was taken. NULL on rows predating the '
    'column, where the rate cannot be recovered from tax_amount alone (a '
    'discount changes the base). Do not backfill it with today''s rate.';

-- Deliberately NOT backfilled. `tax_rate_applied` is unknown for historical
-- orders, and writing the current rate into them would assert something untrue
-- about the past — the exact failure mode this column exists to prevent.
-- `service_charge_amount` defaults to 0, which IS true of every historical
-- order, because no service charge could be charged before this migration.
