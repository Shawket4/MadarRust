-- DB architecture Phase 1.7 (DB_ARCHITECTURE_AUDIT_2026-10-16 §8): bound how
-- long a tenant (RLS) session may run one statement or sit idle inside a
-- transaction. Measured first: the slowest report at 30x prod history takes
-- ~2 s, far under 30 s.
--
-- Only `madar_app` gets them. Migrations, the seeder and every background job
-- run on the OWNER connection (DATABASE_URL), never as madar_app, so none of
-- them is bounded by this. A role setting applies to sessions that LOG IN as
-- madar_app (MADAR_APP_DB_PASSWORD set); the `SET ROLE` fallback path sets the
-- same values on connect (src/db.rs), so both paths behave alike.
--
-- Changing a role needs CREATEROLE/ADMIN on it or superuser. A migrating user
-- without that gets a WARNING and the boot continues: the on-connect SET in
-- src/db.rs still applies the limits.
DO $$
BEGIN
    ALTER ROLE madar_app SET statement_timeout = '30s';
    ALTER ROLE madar_app SET idle_in_transaction_session_timeout = '60s';
EXCEPTION WHEN insufficient_privilege THEN
    RAISE WARNING 'cannot ALTER ROLE madar_app (%); the app sets the timeouts per connection instead', SQLERRM;
END $$;
