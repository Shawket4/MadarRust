-- Sign-in emails are stored trimmed and lowercased (owner, 2026-10-05: "we
-- need to normalize emails for login"). Login compared the typed address
-- byte for byte, so "Ahmed@cafe.com" — what a phone keyboard types — never
-- found "ahmed@cafe.com", and the same person could be created twice in two
-- casings.
--
-- 1. Existing rows are normalized. No two ACTIVE users share an address
--    case-insensitively (checked on prod 2026-10-05), so the active-only
--    unique index below cannot be violated; deleted rows may share one.
-- 2. A trigger applies the same rule to every insert and update, so a path
--    that forgets (seeds, the demo, raw SQL) still stores the normal form.
--    The rule is src/auth/email.rs `normalize`: trim whitespace, then lower.
-- 3. Uniqueness stays on the stored column, which is now the normal form.

CREATE OR REPLACE FUNCTION users_normalize_email() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.email IS NOT NULL THEN
        NEW.email := lower(btrim(NEW.email, E' \t\r\n'));
    END IF;
    RETURN NEW;
END $$;

DROP TRIGGER IF EXISTS users_normalize_email ON users;
CREATE TRIGGER users_normalize_email
    BEFORE INSERT OR UPDATE OF email ON users
    FOR EACH ROW EXECUTE FUNCTION users_normalize_email();

UPDATE users SET email = lower(btrim(email, E' \t\r\n'))
 WHERE email IS NOT NULL AND email <> lower(btrim(email, E' \t\r\n'));
