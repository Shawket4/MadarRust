-- Keep the day, drop the year.
--
-- A greeting needs to know WHEN, not HOW OLD. A full date of birth is a
-- different category of data from a month and a day: it is an identity
-- credential, it is what a bank asks to verify someone, and holding one to send
-- an annual "happy birthday" is collecting far more than the purpose needs.
--
-- Two smallints rather than a date with a fictional year: a `date` cannot
-- express "the 17th of March" without inventing a year, and an invented year is
-- a lie the schema tells every reader after us. It is also what made the sweep
-- read `EXTRACT(...)` on both sides of a comparison it could not index.
ALTER TABLE loyalty_customers
    ADD COLUMN IF NOT EXISTS birth_month smallint,
    ADD COLUMN IF NOT EXISTS birth_day   smallint;

-- Carry across whatever was already collected, then forget the years.
UPDATE loyalty_customers
   SET birth_month = EXTRACT(MONTH FROM birthday)::smallint,
       birth_day   = EXTRACT(DAY   FROM birthday)::smallint
 WHERE birthday IS NOT NULL AND birth_month IS NULL;

ALTER TABLE loyalty_customers
    DROP COLUMN IF EXISTS birthday;

-- Both or neither, and both real. A month with no day greets nobody, and a day
-- with no month greets everybody twelve times.
ALTER TABLE loyalty_customers
    ADD CONSTRAINT loyalty_customers_birthday_pair CHECK (
        (birth_month IS NULL) = (birth_day IS NULL)
        AND (birth_month IS NULL OR birth_month BETWEEN 1 AND 12)
        AND (birth_day   IS NULL OR birth_day   BETWEEN 1 AND 31)
    );

DROP INDEX IF EXISTS idx_loyalty_customers_birthday;
CREATE INDEX IF NOT EXISTS idx_loyalty_customers_birthday
    ON loyalty_customers (org_id, birth_month, birth_day)
    WHERE birth_month IS NOT NULL;
