-- DB architecture Phase 5.6 (DB_ARCHITECTURE_AUDIT_2026-10-16 §8 Phase 5.6, L6;
-- owner decision 12.9): money and lifetime totals that can outgrow int4.
--
-- int4 piastres stop at about 21.4 M EGP. That is plenty for one line or one
-- order, and not for a till's or a reconciliation's totals, or a member's
-- lifetime points. The columns widened (the audit's list):
--   tills                  opening_cash, opening_cash_original, closing_cash_declared,
--                          closing_cash_system, held_orders_left_open_total
--                          (cash_discrepancy is GENERATED from declared - system: the
--                          contract re-creates it as a bigint generated column over
--                          the widened pair; a BEFORE trigger cannot copy it.)
--   till_reconciliations   system_total, current_system_total, declared_amount
--   loyalty_memberships    lifetime_points, lifetime_visits
--
-- `ALTER COLUMN … TYPE bigint` rewrites the table under ACCESS EXCLUSIVE, so
-- this is EXPAND / CONTRACT instead:
--   EXPAND (this file): a `<col>_wide bigint` beside each column, kept equal to
--     it by a BEFORE INSERT OR UPDATE trigger (the int4 column stays the one
--     every binary writes, so old and new binaries behave the same), and
--     backfilled here. The table's own UPDATE triggers are off for the backfill
--     statement (Phase 1.2's rule: the copy changes nothing a till or the
--     changefeed should see).
--     The prod copy holds 853 tills, so this is milliseconds. At 30x it is about
--     25,000 rows, still one short statement.
--   RECONCILE: scripts/db_phase5_check.sql, exact per-org sum(col) =
--     sum(col_wide) and no row where they differ.
--   CONTRACT (parked: migrations/parked/p5_money_width_contract.sql): copy the
--     int4 values to archive, drop the int4 columns, rename `_wide` to the old
--     names, in the same release as the Rust row types move to i64.
SET LOCAL lock_timeout = '15s';

CREATE FUNCTION pg_temp.p56_expand(t text, cols text[]) RETURNS void
LANGUAGE plpgsql AS $fn$
DECLARE
    c    text;
    body text := '';
    sets text := '';
    trg  record;
    off  text[] := '{}';
    mode text[] := '{}';
    i    int;
    fn   text := 'money_wide_' || t;
BEGIN
    FOREACH c IN ARRAY cols LOOP
        EXECUTE format('ALTER TABLE %I ADD COLUMN IF NOT EXISTS %I bigint', t, c || '_wide');
        body := body || format('NEW.%I := NEW.%I; ', c || '_wide', c);
        sets := sets || format('%I = %I, ', c || '_wide', c);
    END LOOP;
    EXECUTE format($f$CREATE OR REPLACE FUNCTION %I() RETURNS trigger LANGUAGE plpgsql AS $b$
        BEGIN %s RETURN NEW; END $b$$f$, fn, body);
    EXECUTE format('DROP TRIGGER IF EXISTS money_wide ON %I', t);
    EXECUTE format('CREATE TRIGGER money_wide BEFORE INSERT OR UPDATE ON %I '
                   'FOR EACH ROW EXECUTE FUNCTION %I()', t, fn);

    FOR trg IN
        SELECT tgname, tgenabled FROM pg_trigger
         WHERE tgrelid = format('%I', t)::regclass AND NOT tgisinternal
           AND tgenabled <> 'D' AND (tgtype & 16) <> 0 AND tgname <> 'money_wide'
         ORDER BY tgname
    LOOP
        EXECUTE format('ALTER TABLE %I DISABLE TRIGGER %I', t, trg.tgname);
        off  := off  || trg.tgname::text;
        mode := mode || trg.tgenabled::text;
    END LOOP;
    -- money_wide itself fires on this UPDATE and sets every _wide from its column.
    EXECUTE format('UPDATE %I SET %s WHERE %s', t, rtrim(sets, ', '),
        (SELECT string_agg(format('%I IS DISTINCT FROM %I', c2 || '_wide', c2), ' OR ')
           FROM unnest(cols) c2));
    FOR i IN 1 .. coalesce(array_length(off, 1), 0) LOOP
        EXECUTE format('ALTER TABLE %I ENABLE %s TRIGGER %I', t,
            CASE mode[i] WHEN 'A' THEN 'ALWAYS' WHEN 'R' THEN 'REPLICA' ELSE '' END, off[i]);
    END LOOP;
END
$fn$;

SELECT pg_temp.p56_expand('tills', ARRAY['opening_cash', 'opening_cash_original',
    'closing_cash_declared', 'closing_cash_system', 'held_orders_left_open_total']);
SELECT pg_temp.p56_expand('till_reconciliations', ARRAY['system_total', 'current_system_total',
    'declared_amount']);
SELECT pg_temp.p56_expand('loyalty_memberships', ARRAY['lifetime_points', 'lifetime_visits']);

DROP FUNCTION pg_temp.p56_expand(text, text[]);
