-- The birthday gift: one free item (owner, 2026-10-23; Madar BIRTHDAY_GIFT_DESIGN.md).
--
-- Additive only. Existing programmes keep `birthday_gift = 'amount'` (today's
-- behaviour: `birthday_reward_amount` on the balance), so nothing changes until
-- an owner picks 'item'.

-- ── Settings ─────────────────────────────────────────────────────────────
ALTER TABLE loyalty_settings
    ADD COLUMN IF NOT EXISTS birthday_gift text NOT NULL DEFAULT 'amount'
        CONSTRAINT loyalty_settings_birthday_gift CHECK (birthday_gift IN ('none', 'amount', 'item')),
    ADD COLUMN IF NOT EXISTS birthday_gift_days integer NOT NULL DEFAULT 30
        CONSTRAINT loyalty_settings_birthday_gift_days CHECK (birthday_gift_days BETWEEN 7 AND 60);

-- ── The gift ─────────────────────────────────────────────────────────────
-- One row per member, kind and year: a rerun of the job never grants twice.
-- Claimed once, at a till (an order line covered at zero) or at a Rewards by
-- Madar counter. A void or refund of the claiming order re-opens it while it
-- is still valid. Expired is derived (`valid_until <= now()`) for reads; the
-- status column turns 'expired' only when a membership is retired.
CREATE TABLE IF NOT EXISTS loyalty_gifts (
    id                    uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id                uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    customer_id           uuid NOT NULL REFERENCES loyalty_memberships(id) ON DELETE CASCADE,
    kind                  text NOT NULL DEFAULT 'birthday' CHECK (kind IN ('birthday')),
    year                  integer NOT NULL,
    valid_from            timestamptz NOT NULL,
    valid_until           timestamptz NOT NULL,
    status                text NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'claimed', 'expired')),
    created_at            timestamptz NOT NULL DEFAULT now(),
    claimed_at            timestamptz NULL,
    claimed_source        text NULL CHECK (claimed_source IN ('order', 'counter')),
    claimed_order_id      uuid NULL REFERENCES orders(id) ON DELETE SET NULL,
    claimed_order_item_id uuid NULL,
    claimed_line_index    integer NULL,
    claimed_menu_item_id  uuid NULL,
    claimed_branch_id     uuid NULL REFERENCES branches(id) ON DELETE SET NULL,
    claimed_by            uuid NULL,
    claim_key             text NULL,
    CONSTRAINT loyalty_gifts_once UNIQUE (customer_id, kind, year),
    CONSTRAINT loyalty_gifts_window CHECK (valid_until > valid_from),
    CONSTRAINT loyalty_gifts_claimed_has_when CHECK ((status = 'claimed') = (claimed_at IS NOT NULL))
);

CREATE INDEX IF NOT EXISTS loyalty_gifts_open_member
    ON loyalty_gifts (customer_id) WHERE status = 'open';
CREATE INDEX IF NOT EXISTS loyalty_gifts_open_org
    ON loyalty_gifts (org_id, valid_until) WHERE status = 'open';
CREATE INDEX IF NOT EXISTS loyalty_gifts_claimed_order
    ON loyalty_gifts (claimed_order_id) WHERE claimed_order_id IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS loyalty_gifts_org_claim_key
    ON loyalty_gifts (org_id, claim_key) WHERE claim_key IS NOT NULL;

ALTER TABLE loyalty_gifts ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON loyalty_gifts;
CREATE POLICY tenant_isolation ON loyalty_gifts FOR ALL
    USING ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE)
    WITH CHECK ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE);
GRANT SELECT, INSERT, UPDATE, DELETE ON loyalty_gifts TO madar_app;

-- ── The ledger: a claimed gift is a redemption that spends nothing ────────
-- `gift`: kind 'redeem', 0 points, so it shows in the member's history and in
-- redemption counts but moves no balance. `loyalty_reverse` returns NULL for a
-- row with nothing to reverse, so a void or refund never touches it; the gift
-- table re-opens on its own (below).
ALTER TABLE loyalty_transactions
    DROP CONSTRAINT IF EXISTS loyalty_txn_points_nonzero,
    DROP CONSTRAINT IF EXISTS loyalty_txn_redeem_negative,
    DROP CONSTRAINT IF EXISTS loyalty_txn_source,
    DROP CONSTRAINT IF EXISTS loyalty_txn_kind_has_a_source;
ALTER TABLE loyalty_transactions
    ADD CONSTRAINT loyalty_txn_points_nonzero CHECK (points <> 0 OR source = 'gift'),
    ADD CONSTRAINT loyalty_txn_redeem_negative CHECK (
        kind <> 'redeem' OR points < 0 OR (source = 'gift' AND points = 0)),
    ADD CONSTRAINT loyalty_txn_source CHECK (source IN (
        'sale', 'redemption', 'void', 'refund', 'birthday', 'winback', 'manual',
        'merge', 'counter',
        'gift'         -- a birthday gift item handed over; spends nothing
    )),
    ADD CONSTRAINT loyalty_txn_kind_has_a_source CHECK (
        CASE kind
            WHEN 'earn'           THEN source IN ('sale', 'counter')
            WHEN 'redeem'         THEN source IN ('redemption', 'counter', 'gift')
            WHEN 'adjust'         THEN source IN ('manual', 'birthday', 'winback', 'merge')
            WHEN 'reverse_earn'   THEN source IN ('void', 'refund', 'manual')
            WHEN 'reverse_redeem' THEN source IN ('void', 'refund', 'manual')
            WHEN 'reverse_adjust' THEN source = 'manual'
        END
    ),
    -- Only a gift may be worth nothing, and it is worth exactly nothing.
    ADD CONSTRAINT loyalty_txn_gift_is_free CHECK (source <> 'gift' OR points = 0);

-- ── A void re-opens the gift it claimed, while it is still valid ─────────
-- Beside `orders_reverse_loyalty_on_void`, on the same event, so every path
-- that voids (till, replay, admin) is covered.
CREATE OR REPLACE FUNCTION loyalty_gift_reopen_on_void() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    UPDATE loyalty_gifts
       SET status = 'open', claimed_at = NULL, claimed_source = NULL,
           claimed_order_id = NULL, claimed_order_item_id = NULL,
           claimed_line_index = NULL, claimed_menu_item_id = NULL,
           claimed_branch_id = NULL, claimed_by = NULL, claim_key = NULL
     WHERE claimed_order_id = NEW.id AND status = 'claimed' AND valid_until > now();
    RETURN NULL;
END $$;
DROP TRIGGER IF EXISTS orders_reopen_loyalty_gift_on_void ON orders;
CREATE TRIGGER orders_reopen_loyalty_gift_on_void
    AFTER UPDATE OF status ON orders
    FOR EACH ROW
    WHEN (NEW.status = 'voided' AND OLD.status IS DISTINCT FROM 'voided')
    EXECUTE FUNCTION loyalty_gift_reopen_on_void();

-- ── A retired membership (left, erased, merged away) forfeits an open gift ─
CREATE OR REPLACE FUNCTION loyalty_gift_expire_on_retire() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    UPDATE loyalty_gifts SET status = 'expired'
     WHERE customer_id = NEW.id AND status = 'open';
    RETURN NULL;
END $$;
DROP TRIGGER IF EXISTS loyalty_memberships_expire_gifts ON loyalty_memberships;
CREATE TRIGGER loyalty_memberships_expire_gifts
    AFTER UPDATE OF deleted_at ON loyalty_memberships
    FOR EACH ROW
    WHEN (OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL)
    EXECUTE FUNCTION loyalty_gift_expire_on_retire();
