-- Birthdays, and who is allowed to look like themselves.

-- ── Birthdays ────────────────────────────────────────────────────────────────
-- Nullable, and stays null unless the org asks for it: a date of birth is the
-- most sensitive thing this feature has ever collected, and a shop that does not
-- run birthday rewards has no business holding one.
ALTER TABLE loyalty_customers
    ADD COLUMN IF NOT EXISTS birthday date;

ALTER TABLE loyalty_settings
    -- Off means the signup form does not ASK. Not "asks and ignores".
    ADD COLUMN IF NOT EXISTS birthday_enabled boolean NOT NULL DEFAULT false,
    -- NULL = a greeting and nothing else. A gift is optional on purpose: plenty
    -- of shops want to say happy birthday without giving away a drink.
    ADD COLUMN IF NOT EXISTS birthday_reward_amount integer,
    -- Overrides the built-in greeting. `{name}` is substituted; nothing else is.
    ADD COLUMN IF NOT EXISTS birthday_message text,
    ADD COLUMN IF NOT EXISTS birthday_message_ar text,
    ADD CONSTRAINT loyalty_settings_birthday_reward_positive
        CHECK (birthday_reward_amount IS NULL OR birthday_reward_amount > 0);

-- One greeting per member per year, enforced by the database rather than by the
-- job remembering. The sweep runs every few hours and on every instance; without
-- this, a restart in the wrong hour sends a customer their birthday message
-- twice, and there is no unsending a WhatsApp.
CREATE TABLE IF NOT EXISTS loyalty_birthday_greetings (
    customer_id uuid NOT NULL REFERENCES loyalty_customers(id) ON DELETE CASCADE,
    org_id      uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    year        integer NOT NULL,
    -- What was actually given, so "why is my balance up" has an answer.
    reward_amount integer,
    sent_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (customer_id, year)
);
CREATE INDEX IF NOT EXISTS idx_loyalty_birthday_greetings_org
    ON loyalty_birthday_greetings (org_id, year);
ALTER TABLE loyalty_birthday_greetings ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON loyalty_birthday_greetings FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT ALL ON TABLE loyalty_birthday_greetings TO sufrix;

-- Finding today's birthdays without scanning every member every tick.
CREATE INDEX IF NOT EXISTS idx_loyalty_customers_birthday
    ON loyalty_customers (org_id, (EXTRACT(MONTH FROM birthday)), (EXTRACT(DAY FROM birthday)))
    WHERE birthday IS NOT NULL;

-- ── Who may look like themselves ─────────────────────────────────────────────
-- A paid tier, so only a super admin may set it. Off means every customer-facing
-- surface falls back to Madar's own palette and mark — the shop's NAME is always
-- its own, because a card that does not say whose it is helps nobody.
ALTER TABLE organizations
    ADD COLUMN IF NOT EXISTS custom_branding boolean NOT NULL DEFAULT false;

-- Grandfather the shops already using it.
--
-- A tier that starts charging for something people already have is a tier that
-- takes it away, and the first anyone would know is a customer opening a card
-- that had been theirs yesterday and is now Madar's. Anyone who has uploaded a
-- logo keeps what they have; the gate applies to organisations created from
-- here on, which is what a new tier actually means.
UPDATE organizations SET custom_branding = true
 WHERE logo_url IS NOT NULL AND deleted_at IS NULL;
