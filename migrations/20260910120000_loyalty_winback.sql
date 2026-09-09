-- "We've missed you" — and the first way a customer can ask us to stop.

ALTER TABLE loyalty_settings
    -- Off by default, like every message this programme sends. A shop opts in.
    ADD COLUMN IF NOT EXISTS winback_enabled boolean NOT NULL DEFAULT false,
    -- ONE override, in whichever language the shop writes it, replacing the
    -- built-in English and Arabic both. A shop that has something to say says
    -- it once; a shop that has not gets a sentence that reads properly in the
    -- customer's own language. `{name}` is substituted; nothing else is.
    ADD COLUMN IF NOT EXISTS winback_message text,
    -- NULL = words only. "We've missed you — here's a free coffee" converts far
    -- better than the words alone, and the ledger already knows how to give.
    ADD COLUMN IF NOT EXISTS winback_reward_amount integer,
    ADD CONSTRAINT loyalty_settings_winback_reward_positive
        CHECK (winback_reward_amount IS NULL OR winback_reward_amount > 0);

-- The first opt-out this system has ever had.
--
-- The birthday greeting has been going out with no way to decline it, which was
-- defensible for one welcome message a year and is not for marketing. One flag
-- covers both: a customer who asks us to stop is asking the SHOP to stop, not
-- asking to be excluded from one campaign.
ALTER TABLE loyalty_customers
    ADD COLUMN IF NOT EXISTS marketing_opt_out boolean NOT NULL DEFAULT false;

-- One nudge per member, per dormant spell, per position in the sequence.
--
-- `since` is the member's last visit at the moment they were found dormant. It
-- does not move while they stay away, so it names the spell; a visit changes it,
-- which is what makes every earlier nudge belong to a spell that is over. That
-- is the whole reset mechanism — no counter for anything to forget to clear.
--
-- Written BEFORE the message goes anywhere, like the birthday greeting: a row we
-- wrote and failed to send is one missed message; a message we sent and failed
-- to record is a customer nudged again on the next tick.
CREATE TABLE IF NOT EXISTS loyalty_winbacks (
    customer_id uuid NOT NULL REFERENCES loyalty_customers(id) ON DELETE CASCADE,
    org_id      uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    since       timestamptz NOT NULL,
    seq         smallint NOT NULL,
    reward_amount integer,
    sent_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (customer_id, since, seq)
);
CREATE INDEX IF NOT EXISTS idx_loyalty_winbacks_org
    ON loyalty_winbacks (org_id, sent_at);
ALTER TABLE loyalty_winbacks ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON loyalty_winbacks FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT ALL ON TABLE loyalty_winbacks TO sufrix;

-- "When did this member last do anything" is the sweep's hot question, asked
-- over the ledger for every member of every shop that has this switched on.
CREATE INDEX IF NOT EXISTS idx_loyalty_transactions_customer_recent
    ON loyalty_transactions (customer_id, created_at DESC);
