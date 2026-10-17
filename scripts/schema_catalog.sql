-- Normalized schema catalog (DB_ARCHITECTURE_AUDIT_2026-10-16 §9.4): one line
-- per object, sorted, so two databases can be compared with `diff`. Used by the
-- CI schema-drift job (migrations from scratch vs the prod schema dump):
--
--   psql "$DB" -X -At -f scripts/schema_catalog.sql > catalog.txt
--
-- Names that Postgres generates (index names, constraint names) are kept where
-- the code depends on them; index lines also carry the definition without the
-- name. Function bodies are hashed. Only user schemas are listed.
\set ON_ERROR_STOP 1
\set QUIET on
WITH ns AS (
    SELECT oid, nspname FROM pg_namespace
     WHERE nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast')
       AND nspname NOT LIKE 'pg\_temp\_%' AND nspname NOT LIKE 'pg\_toast\_temp\_%'
       -- Retired tables kept for their data (never dropped). What a database
       -- holds there depends on its history, not on the migrations: a fresh
       -- build archives empty legacy tables that prod never had (Phase 2.4).
       AND nspname <> 'archive'
), rel AS (
    SELECT c.oid, n.nspname, c.relname, c.relkind, c.relrowsecurity, c.relforcerowsecurity
      FROM pg_class c JOIN ns n ON n.oid = c.relnamespace
     WHERE c.relkind IN ('r', 'v', 'm', 'p', 'f')
), lines AS (
    SELECT 'schema ' || nspname AS l FROM ns
    UNION ALL
    SELECT 'extension ' || extname FROM pg_extension
    UNION ALL
    SELECT format('relation %s.%s kind=%s rls=%s force=%s', nspname, relname, relkind,
                  relrowsecurity, relforcerowsecurity) FROM rel
    UNION ALL
    SELECT format('column %s.%s.%s %s%s%s', r.nspname, r.relname, a.attname,
                  format_type(a.atttypid, a.atttypmod),
                  CASE WHEN a.attnotnull THEN ' not null' ELSE '' END,
                  COALESCE(' default ' || pg_get_expr(d.adbin, d.adrelid), ''))
      FROM rel r JOIN pg_attribute a ON a.attrelid = r.oid AND a.attnum > 0 AND NOT a.attisdropped
      LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
    UNION ALL
    SELECT format('constraint %s.%s %s: %s', r.nspname, r.relname, c.conname, pg_get_constraintdef(c.oid))
      FROM pg_constraint c JOIN rel r ON r.oid = c.conrelid
    UNION ALL
    SELECT format('index %s.%s: %s', r.nspname, r.relname,
                  regexp_replace(pg_get_indexdef(i.indexrelid), '^CREATE (UNIQUE )?INDEX \S+ ON ', 'CREATE \1INDEX ON '))
      FROM pg_index i JOIN rel r ON r.oid = i.indrelid
    UNION ALL
    SELECT format('trigger %s.%s: %s', r.nspname, r.relname, pg_get_triggerdef(t.oid))
      FROM pg_trigger t JOIN rel r ON r.oid = t.tgrelid WHERE NOT t.tgisinternal
    UNION ALL
    SELECT format('function %s.%s(%s) -> %s md5=%s', n.nspname, p.proname,
                  pg_get_function_identity_arguments(p.oid), pg_get_function_result(p.oid),
                  md5(COALESCE(p.prosrc, '')))
      FROM pg_proc p JOIN ns n ON n.oid = p.pronamespace
     WHERE NOT EXISTS (SELECT 1 FROM pg_depend dp
                        WHERE dp.objid = p.oid AND dp.deptype = 'e')   -- not an extension's
    UNION ALL
    SELECT format('enum %s.%s: %s', n.nspname, t.typname,
                  (SELECT string_agg(e.enumlabel, ',' ORDER BY e.enumsortorder)
                     FROM pg_enum e WHERE e.enumtypid = t.oid))
      FROM pg_type t JOIN ns n ON n.oid = t.typnamespace WHERE t.typtype = 'e'
    UNION ALL
    SELECT format('policy %s.%s %s: cmd=%s permissive=%s roles=%s using=%s check=%s',
                  r.nspname, r.relname, p.polname, p.polcmd, p.polpermissive,
                  (SELECT string_agg(CASE WHEN x = 0 THEN 'public' ELSE x::regrole::text END, ',' ORDER BY 1)
                     FROM unnest(p.polroles) x),
                  pg_get_expr(p.polqual, p.polrelid), pg_get_expr(p.polwithcheck, p.polrelid))
      FROM pg_policy p JOIN rel r ON r.oid = p.polrelid
    UNION ALL
    SELECT format('view %s.%s md5=%s', nspname, relname, md5(pg_get_viewdef(oid)))
      FROM rel WHERE relkind IN ('v', 'm')
    UNION ALL
    SELECT format('sequence %s.%s', n.nspname, c.relname)
      FROM pg_class c JOIN ns n ON n.oid = c.relnamespace WHERE c.relkind = 'S'
)
SELECT x FROM (SELECT regexp_replace(l, '\s+', ' ', 'g') AS x FROM lines) s ORDER BY x COLLATE "C";
