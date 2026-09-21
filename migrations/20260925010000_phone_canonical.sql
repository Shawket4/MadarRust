-- One canonical phone form (CUSTOMERS_UNIFICATION_DESIGN.md §2.3, step 1).
--
-- Until now three forms were in use: `customers.phone_key` folded Egypt's
-- country code to a local `0`, loyalty/delivery/bookings stored E.164 digits
-- without the `+` (`201001234567`), and free text elsewhere. Nothing could be
-- joined on a phone. From here on there is one form, the E.164 one, and one
-- function per language that produces it; `tests/phone_vectors.json` is run
-- against all of them (Rust `crate::phone`, this function, the POS core, the
-- dashboard) so they cannot drift.
--
-- `customers.phone_key` keeps its NAME, because the sync projection and every
-- deployed till read that key, but its CONTENT becomes canonical. `phone`
-- stays what the person typed.

-- Returns NULL for anything that is not a phone number. Mirrors
-- `crate::phone::canonical` rule for rule; change both or neither.
CREATE OR REPLACE FUNCTION phone_canonical(p text) RETURNS text
LANGUAGE plpgsql IMMUTABLE PARALLEL SAFE AS $$
DECLARE
    d text;
BEGIN
    IF p IS NULL OR char_length(p) > 32 THEN
        RETURN NULL;
    END IF;
    -- Arabic-Indic and Extended Arabic-Indic digits to ASCII, then digits only.
    d := translate(p, '٠١٢٣٤٥٦٧٨٩۰۱۲۳۴۵۶۷۸۹', '01234567890123456789');
    d := regexp_replace(d, '[^0123456789]', '', 'g');
    IF d LIKE '00%' THEN
        d := substr(d, 3);
    ELSIF d LIKE '20%' THEN
        NULL;
    ELSIF d LIKE '0%' THEN
        d := '20' || substr(d, 2);
    ELSIF length(d) = 10 AND d LIKE '1%' THEN
        d := '20' || d;
    END IF;
    IF length(d) < 10 OR length(d) > 15 THEN
        RETURN NULL;
    END IF;
    -- Egyptian mobile guard (rule 6): 2010/2011/2012/2015 must be exactly 12
    -- digits. Landlines such as 2013xxxxxxx are untouched.
    IF d ~ '^201[0125]' AND length(d) <> 12 THEN
        RETURN NULL;
    END IF;
    RETURN d;
END $$;
GRANT EXECUTE ON FUNCTION phone_canonical(text) TO madar_app;

-- The old key function now answers with the canonical form, so anything that
-- still calls it agrees with everything that calls the new one.
CREATE OR REPLACE FUNCTION customers_phone_key(p text) RETURNS text
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$ SELECT phone_canonical(p) $$;

-- Re-key every customer. A stored phone that is not a phone number keeps its
-- row and its typed text and simply has no key (it could never be looked up by
-- a canonical phone anyway). Merged rows are re-keyed too, so a history query
-- sees one form.
UPDATE customers
   SET phone_key = phone_canonical(phone)
 WHERE phone_key IS DISTINCT FROM phone_canonical(phone);

-- One phone per customer; the numbers they used to have live here, for audit
-- and so an old reference (a booking, a delivery row) can still be resolved.
CREATE TABLE IF NOT EXISTS customer_phone_history (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id       uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    customer_id  uuid NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    -- As it was typed, and its canonical key (NULL when it never was a phone).
    phone        text NOT NULL,
    phone_key    text NULL,
    -- 'edit' (staff changed it), 'merge' (the duplicate's number),
    -- 'backfill' (the unification migration), 'self' (the customer replaced it).
    reason       text NOT NULL DEFAULT 'edit',
    replaced_by  uuid NULL REFERENCES users(id) ON DELETE SET NULL,
    replaced_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT customer_phone_history_reason
        CHECK (reason IN ('edit', 'merge', 'backfill', 'self'))
);
CREATE INDEX IF NOT EXISTS customer_phone_history_customer
    ON customer_phone_history (customer_id, replaced_at DESC);
CREATE INDEX IF NOT EXISTS customer_phone_history_org_key
    ON customer_phone_history (org_id, phone_key) WHERE phone_key IS NOT NULL;

ALTER TABLE customer_phone_history ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS customer_phone_history_tenant ON customer_phone_history;
CREATE POLICY customer_phone_history_tenant ON customer_phone_history
    USING (org_id = NULLIF(current_setting('app.org_id', true), '')::uuid);
-- DELETE as well: a PDPL erase purges the history.
GRANT SELECT, INSERT, UPDATE, DELETE ON customer_phone_history TO madar_app;

-- ── Verification ────────────────────────────────────────────────────────────
DO $$
DECLARE
    bad bigint;
BEGIN
    SELECT count(*) INTO bad FROM customers
     WHERE phone_key IS DISTINCT FROM phone_canonical(phone);
    IF bad > 0 THEN
        RAISE EXCEPTION 'phone_canonical: % customers still carry a non-canonical phone_key', bad;
    END IF;
    IF phone_canonical('0100 123 4567') IS DISTINCT FROM '201001234567'
       OR phone_canonical('٠١٠٠١٢٣٤٥٦٧') IS DISTINCT FROM '201001234567'
       OR phone_canonical('12345') IS NOT NULL THEN
        RAISE EXCEPTION 'phone_canonical does not implement the shared rule';
    END IF;
END $$;
