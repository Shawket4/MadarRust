-- Saying something to a customer through the card they already have.

ALTER TABLE loyalty_customers
    -- The line the pass carries while a message is outstanding. Apple has no
    -- way to push text: a notification is a FIELD whose value changed and whose
    -- definition carries a `changeMessage`. So the message has to be on the
    -- card, in a field that appears with it and is dropped afterwards.
    ADD COLUMN IF NOT EXISTS pass_notice text,
    ADD COLUMN IF NOT EXISTS pass_notice_at timestamptz,
    -- When the device came back for the pass after we pushed. Apple's pass web
    -- service is the only delivery signal either wallet gives us, and it is
    -- what makes the WhatsApp fallback evidence rather than a guess.
    ADD COLUMN IF NOT EXISTS pass_notice_seen_at timestamptz,
    -- Which wallets were reached: 'apple', 'google', or both. Google gives no
    -- delivery signal at all, so a card saved there means no fallback — the
    -- alternative is messaging everyone twice.
    ADD COLUMN IF NOT EXISTS pass_notice_wallets text,
    -- What we owe them on WhatsApp if the card never gets it. Kept separately
    -- because the two are not the same text: a pass field is a line, and a
    -- WhatsApp carries the link and the way to stop.
    ADD COLUMN IF NOT EXISTS pass_notice_fallback text;

-- The fallback sweep's question: whose message has been sitting undelivered?
CREATE INDEX IF NOT EXISTS idx_loyalty_customers_pass_notice
    ON loyalty_customers (pass_notice_at)
    WHERE pass_notice_at IS NOT NULL AND pass_notice_seen_at IS NULL;
