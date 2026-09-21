-- The person's details live on the customer; the membership keeps only what
-- is about the programme (CUSTOMERS_UNIFICATION_DESIGN.md §2.2, step 4).
--
-- `name`, `phone`, `locale`, the birthday and the marketing opt-out described
-- a PERSON and were stored on their loyalty card, so a customer who was not a
-- member had no language and no way to say "stop writing to me", and a member
-- who was also a customer had two names. They move to `customers`. Balances,
-- the member token and the wallet secrets stay where they are: hot,
-- trigger-maintained writes stay off the row that fans out to every till, and
-- secrets stay off a row the POS reads.
--
-- `loyalty_members_v` is the membership joined to its customer, with the old
-- column names, so every loyalty read keeps its shape.

ALTER TABLE customers
    -- NULL = never asked (a customer added at the till). The view answers 'en'.
    ADD COLUMN IF NOT EXISTS locale            text NULL,
    ADD COLUMN IF NOT EXISTS birth_month       smallint NULL,
    ADD COLUMN IF NOT EXISTS birth_day         smallint NULL,
    ADD COLUMN IF NOT EXISTS marketing_opt_out boolean NOT NULL DEFAULT false;
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'customers_locale') THEN
        ALTER TABLE customers ADD CONSTRAINT customers_locale
            CHECK (locale IS NULL OR locale IN ('en', 'ar'));
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'customers_birthday_pair') THEN
        ALTER TABLE customers ADD CONSTRAINT customers_birthday_pair CHECK (
            (birth_month IS NULL) = (birth_day IS NULL)
            AND (birth_month IS NULL OR birth_month BETWEEN 1 AND 12)
            AND (birth_day   IS NULL OR birth_day   BETWEEN 1 AND 31));
    END IF;
END $$;

-- Backfill from the card, while the card still has the columns. Name and
-- phone were settled by the shared-key migration (the member's where the
-- customer was made from the member, the customer's own otherwise); these four
-- only ever existed on the card, so the card is the only source.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM information_schema.columns
                WHERE table_schema = 'public' AND table_name = 'loyalty_customers'
                  AND column_name = 'marketing_opt_out') THEN
        UPDATE customers c
           SET locale            = m.locale,
               birth_month       = CASE WHEN c.erased_at IS NULL THEN m.birth_month END,
               birth_day         = CASE WHEN c.erased_at IS NULL THEN m.birth_day END,
               -- The stricter answer wins, and a forgotten person is never written to.
               marketing_opt_out = c.marketing_opt_out OR m.marketing_opt_out
                                   OR c.erased_at IS NOT NULL
          FROM loyalty_customers m
         WHERE m.id = c.id;
    END IF;
END $$;

-- Today's birthdays without scanning every customer every tick.
CREATE INDEX IF NOT EXISTS customers_org_birthday
    ON customers (org_id, birth_month, birth_day) WHERE birth_month IS NOT NULL;

-- ── Verification, before anything is dropped ────────────────────────────────
DO $$
DECLARE
    bad bigint;
BEGIN
    IF EXISTS (SELECT 1 FROM information_schema.columns
                WHERE table_schema = 'public' AND table_name = 'loyalty_customers'
                  AND column_name = 'marketing_opt_out') THEN
        EXECUTE $q$
            SELECT count(*) FROM loyalty_customers m JOIN customers c ON c.id = m.id
             WHERE m.deleted_at IS NULL
               AND (c.locale IS DISTINCT FROM m.locale
                    OR c.birth_month IS DISTINCT FROM m.birth_month
                    OR c.birth_day IS DISTINCT FROM m.birth_day
                    OR (m.marketing_opt_out AND NOT c.marketing_opt_out)
                    OR (phone_canonical(m.phone) IS NOT NULL AND c.phone_key IS NULL
                        AND NOT EXISTS (SELECT 1 FROM loyalty_customers o
                                         WHERE o.org_id = m.org_id AND o.id <> m.id
                                           AND o.deleted_at IS NULL
                                           AND phone_canonical(o.phone) = phone_canonical(m.phone))))
        $q$ INTO bad;
        IF bad > 0 THEN
            RAISE EXCEPTION 'customers own the person: % live members would lose details in the move', bad;
        END IF;
    END IF;
    SELECT count(*) INTO bad FROM loyalty_customers m
     WHERE NOT EXISTS (SELECT 1 FROM customers c WHERE c.id = m.id);
    IF bad > 0 THEN
        RAISE EXCEPTION 'customers own the person: % members have no customer', bad;
    END IF;
END $$;

-- ── The card gives up the person ────────────────────────────────────────────
-- Dropping a column drops what hangs from it: the (org_id, phone) unique index
-- (one live customer per phone is `customers_org_phone_live_key` now, and one
-- membership per customer is this table's primary key), the birthday index,
-- and the locale and birthday CHECKs.
ALTER TABLE loyalty_customers
    DROP COLUMN IF EXISTS name,
    DROP COLUMN IF EXISTS phone,
    DROP COLUMN IF EXISTS locale,
    DROP COLUMN IF EXISTS birthday,
    DROP COLUMN IF EXISTS birth_month,
    DROP COLUMN IF EXISTS birth_day,
    DROP COLUMN IF EXISTS marketing_opt_out;

-- The membership and its person, under the names every loyalty read already
-- uses. `phone` is the canonical key (what loyalty always stored);
-- `phone_display` is what was typed. `security_invoker` so the tables' RLS
-- binds whoever reads the view.
CREATE OR REPLACE VIEW loyalty_members_v WITH (security_invoker = true) AS
SELECT m.id, m.org_id,
       c.name,
       COALESCE(c.phone_key, c.phone, '') AS phone,
       c.phone                            AS phone_display,
       m.member_token,
       m.points_balance, m.visits_balance, m.lifetime_points, m.lifetime_visits,
       m.joined_branch_id,
       COALESCE(c.locale, 'en')           AS locale,
       m.apple_serial, m.apple_auth_token, m.google_object_id, m.pass_updated_at,
       m.enrolled_at, m.updated_at, m.deleted_at,
       c.birth_month, c.birth_day, c.marketing_opt_out,
       m.pass_notice, m.pass_notice_at, m.pass_notice_seen_at,
       m.pass_notice_wallets, m.pass_notice_fallback,
       c.source, c.erased_at
  FROM loyalty_customers m
  JOIN customers c ON c.id = m.id;
GRANT SELECT, INSERT, UPDATE, DELETE ON loyalty_members_v TO madar_app;
GRANT ALL ON loyalty_members_v TO sufrix;
