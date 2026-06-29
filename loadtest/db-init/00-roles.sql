-- Runs once on first init of the load-test Postgres container
-- (/docker-entrypoint-initdb.d). Pre-rebrand migrations GRANT to a legacy
-- 'sufrix' role; the backend applies migrations on boot, so the role must exist
-- or boot aborts with SQLSTATE 42704. NOLOGIN — it only needs to exist.
DO $do$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sufrix') THEN
    CREATE ROLE sufrix NOLOGIN;
  END IF;
END
$do$;
