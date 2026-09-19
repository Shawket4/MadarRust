-- Stamps counted per LINE ITEM, and only for items the shop chooses.
--
-- A stamp card has always been one stamp per SALE, whatever was on the bill.
-- That is the wrong unit for the card almost every shop actually prints: "buy
-- ten coffees, get one free" counts coffees, and a customer who buys three at
-- once has bought three. The old rule made them queue three times for it.
--
-- Two additions, and both are inert until someone asks for them:
--
-- 1. `stamp_per_line_item` counts the items on the bill instead of the bill.
--    An order of three lattes earns three stamps rather than one, and quantity
--    multiplies (one line of three is three).
--
-- 2. `loyalty_earning_items` names the items that collect. An EMPTY list means
--    everything collects, which is where every programme starts and where a
--    shop that never opens the picker stays — so this table existing changes
--    nothing on its own.
--
-- Stamps only. In points mode neither has any effect: points already scale
-- with the bill, and a second thing scaling with it would be the same money
-- counted twice. The column and the table are still per-scope like everything
-- else here — a branch row overrides the org row wholesale.
--
-- There is deliberately NO new per-order ceiling. The only ceiling on stamps
-- remains `balance_cap`, which trims the accrual without ever failing the sale.

-- ── The counting mode ────────────────────────────────────────────────────────
-- DEFAULT true, then every existing row set back to false. The two halves say
-- different things and both are meant:
--   * a programme created from today counts items, because that is what a stamp
--     card means and nobody should have to find a switch to get it;
--   * a programme that already exists keeps counting sales, because its members
--     are holding cards that were filled under that rule and quietly tripling
--     the rate is the shop's decision to make, not this migration's.
ALTER TABLE loyalty_settings
    ADD COLUMN stamp_per_line_item boolean NOT NULL DEFAULT true;

UPDATE loyalty_settings SET stamp_per_line_item = false;

COMMENT ON COLUMN loyalty_settings.stamp_per_line_item IS
    'Stamps mode only. true = one stamp per eligible item sold (quantity '
    'multiplies); false = one stamp per sale, the original rule. Ignored when '
    'mode = ''points''. New programmes default to true; every programme that '
    'existed when this shipped was set to false and switches only when its '
    'owner flips it.';

-- ── The items that collect ───────────────────────────────────────────────────
-- Scoped exactly like `loyalty_reward_items`, and sitting beside it in the
-- dashboard: one list is what a balance BUYS, this one is what FILLS it. They
-- are independent on purpose — a shop may well let you collect on coffee and
-- spend on cake — so this is a second table rather than a flag on that one.
--
-- An empty list for a scope is not "nothing earns". It means the programme has
-- not narrowed itself, so the whole menu collects. A shop that wants nothing to
-- earn switches the programme off.
CREATE TABLE loyalty_earning_items (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id       uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- NULL = the org-wide list. A branch id = that branch's own.
    branch_id    uuid REFERENCES branches(id) ON DELETE CASCADE,
    menu_item_id uuid NOT NULL REFERENCES menu_items(id) ON DELETE CASCADE,
    sort_order   integer NOT NULL DEFAULT 0,
    created_at   timestamptz NOT NULL DEFAULT now()
);
-- One row per item per scope: saving the same picker twice cannot double a list
-- whose emptiness is load-bearing.
CREATE UNIQUE INDEX loyalty_earning_items_scope_key ON loyalty_earning_items (
    org_id,
    COALESCE(branch_id, '00000000-0000-0000-0000-000000000000'::uuid),
    menu_item_id
);
CREATE INDEX idx_loyalty_earning_items_org ON loyalty_earning_items (org_id);

ALTER TABLE loyalty_earning_items ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON loyalty_earning_items FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT ALL ON TABLE loyalty_earning_items TO sufrix;
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE loyalty_earning_items TO madar_app;

COMMENT ON TABLE loyalty_earning_items IS
    'Menu items that collect a stamp, per scope. An empty list for a scope '
    'means every item collects. Stamps only — points ignore it.';
