-- Merging two customers who are both members
-- (CUSTOMERS_UNIFICATION_DESIGN.md §2.7).
--
-- The loser's balance moves to the survivor as a PAIR of `adjust` rows, one
-- taking it off the loser and one putting it on the survivor. Nothing is
-- edited and nothing is reversed, so the append-only ledger stays exactly
-- that; both rows say `source = 'merge'`, so "what did the programme give
-- away" never counts a merge as a gift.
ALTER TABLE loyalty_transactions
    DROP CONSTRAINT IF EXISTS loyalty_txn_source,
    DROP CONSTRAINT IF EXISTS loyalty_txn_kind_has_a_source;
ALTER TABLE loyalty_transactions
    ADD CONSTRAINT loyalty_txn_source CHECK (source IN (
        'sale', 'redemption', 'void', 'refund', 'birthday', 'winback', 'manual',
        'merge'        -- two memberships of one person became one
    )),
    ADD CONSTRAINT loyalty_txn_kind_has_a_source CHECK (
        CASE kind
            WHEN 'earn'           THEN source = 'sale'
            WHEN 'redeem'         THEN source = 'redemption'
            WHEN 'adjust'         THEN source IN ('manual', 'birthday', 'winback', 'merge')
            WHEN 'reverse_earn'   THEN source IN ('void', 'refund', 'manual')
            WHEN 'reverse_redeem' THEN source IN ('void', 'refund', 'manual')
            WHEN 'reverse_adjust' THEN source = 'manual'
        END
    );

-- The loser's card may already be in someone's wallet or printed on a key fob.
-- Its token keeps finding the SURVIVOR for ninety days, so that card still
-- scans at the counter; after that it is simply an unknown card.
CREATE TABLE IF NOT EXISTS loyalty_token_aliases (
    member_token text PRIMARY KEY,
    org_id       uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- The membership the token now stands for.
    customer_id  uuid NOT NULL REFERENCES loyalty_customers(id) ON DELETE CASCADE,
    -- The membership it used to be, for the audit trail.
    was_customer_id uuid NULL REFERENCES loyalty_customers(id) ON DELETE SET NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL DEFAULT now() + interval '90 days'
);
CREATE INDEX IF NOT EXISTS loyalty_token_aliases_customer ON loyalty_token_aliases (customer_id);
ALTER TABLE loyalty_token_aliases ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON loyalty_token_aliases;
CREATE POLICY tenant_isolation ON loyalty_token_aliases FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT SELECT, INSERT, UPDATE, DELETE ON loyalty_token_aliases TO madar_app;
GRANT ALL ON TABLE loyalty_token_aliases TO sufrix;
