-- RLS visible-row identity (DB_ARCHITECTURE_AUDIT_2026-10-16 §8 Phase 2.1).
--
-- For every org and every public table with row-level security, the rows a
-- tenant session (`madar_app` with app.org_id = the org) can see: a count and
-- an md5 over each row's primary key (the whole row minus org_id when the
-- table has no primary key). One TSV row per (org, table). No customer data.
--
--   psql "$DB" -X -At -F $'\t' -f scripts/db_rls_visibility.sql > before.tsv
--   … apply a policy change …
--   psql "$DB" -X -At -F $'\t' -f scripts/db_rls_visibility.sql > after.tsv
--   diff before.tsv after.tsv          # must be empty
--
-- Read-only (SELECTs only; the transaction is rolled back). Must run as a
-- role that may `SET ROLE madar_app` (the owner / a superuser). It reads every
-- tenant table once per org, so on a large database run it on a copy.
\set ON_ERROR_STOP 1
\set QUIET on
SET client_min_messages = warning;
BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ;  -- not READ ONLY: it creates a pg_temp function; ROLLBACK at the end

CREATE FUNCTION pg_temp.rls_visible()
RETURNS TABLE (org uuid, tbl text, n bigint, h text)
LANGUAGE plpgsql AS $fn$
DECLARE
    t      record;
    o      uuid;
    keyx   text;
    q      text;
    rn     bigint;
    rh     text;
    orgs   uuid[] := ARRAY(SELECT id FROM organizations ORDER BY id);
    res_o  uuid[] := '{}';
    res_t  text[] := '{}';
    res_n  bigint[] := '{}';
    res_h  text[] := '{}';
BEGIN
    FOR t IN
        SELECT c.oid, c.relname
          FROM pg_class c
         WHERE c.relnamespace = 'public'::regnamespace AND c.relkind = 'r' AND c.relrowsecurity
           AND has_table_privilege('madar_app', c.oid, 'SELECT')
         ORDER BY c.relname
    LOOP
        SELECT '(' || string_agg(format('x.%I', a.attname), ', ' ORDER BY k.ord) || ')::text'
          INTO keyx
          FROM pg_index i
          CROSS JOIN LATERAL unnest(i.indkey) WITH ORDINALITY AS k(attnum, ord)
          JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum
         WHERE i.indrelid = t.oid AND i.indisprimary;
        keyx := coalesce(keyx, '(to_jsonb(x) - ''org_id'')::text');
        q := format('SELECT count(*), md5(coalesce(string_agg(k, '','' ORDER BY k), '''')) '
                    '  FROM (SELECT %s AS k FROM public.%I x) s', keyx, t.relname);
        PERFORM set_config('row_security', 'on', true);
        FOREACH o IN ARRAY orgs LOOP
            PERFORM set_config('app.org_id', o::text, true);
            EXECUTE 'SET LOCAL ROLE madar_app';
            EXECUTE q INTO rn, rh;
            EXECUTE 'RESET ROLE';
            res_o := res_o || o;  res_t := res_t || t.relname::text;
            res_n := res_n || rn; res_h := res_h || rh;
        END LOOP;
    END LOOP;
    RETURN QUERY SELECT * FROM unnest(res_o, res_t, res_n, res_h);
END
$fn$;

SELECT org, tbl, n, h FROM pg_temp.rls_visible() ORDER BY org, tbl;

ROLLBACK;
