-- A PDPL erase purges the OTP rows of every number the person held (design
-- §2.8). `delivery_otp` is keyed by phone alone — it has no org — and sits
-- behind row security with no policy, so the tenant role an erase runs under
-- cannot see its rows at all: a plain DELETE there succeeds and removes
-- nothing. This function is the one sanctioned way through.
--
-- Codes live five minutes and carry no tenant data, so deleting by phone across
-- tenants loses nothing anyone is owed; the worst case is a customer who is
-- mid-verification at another shop at that instant asking for a new code.
CREATE OR REPLACE FUNCTION customers_purge_otp(p_phones text[]) RETURNS integer
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    n integer;
BEGIN
    DELETE FROM delivery_otp WHERE phone = ANY(p_phones);
    GET DIAGNOSTICS n = ROW_COUNT;
    RETURN n;
END $$;
REVOKE ALL ON FUNCTION customers_purge_otp(text[]) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION customers_purge_otp(text[]) TO madar_app;
