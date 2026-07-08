-- Row-Level Security, part 1 of 2: the enforced role + grants.
--
-- Tenant isolation moves into the database: merchant-facing connections run as
-- `madar_app` (NOLOGIN, NOBYPASSRLS, owns nothing) with `app.org_id` set from
-- the verified JWT — see `src/db.rs`. The role that owns the tables (dev:
-- shawket, prod: madar, CI: postgres) keeps bypassing RLS by ownership: that is
-- the deliberate escape hatch for migrations, the permissions seeder,
-- cross-tenant background jobs, operator backfill binaries, and the
-- super-admin surface. Do NOT add FORCE ROW LEVEL SECURITY — it would subject
-- the owner too and break all of the above.
--
-- Roles are cluster-global (same story as the legacy `sufrix` role): this
-- block is idempotent and race-safe because parallel `#[sqlx::test]` databases
-- all run the full migration set concurrently. If the migration user lacks
-- CREATEROLE (possible in prod), it fails loudly with the exact statements an
-- operator must run once as postgres.

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'madar_app') THEN
        BEGIN
            CREATE ROLE madar_app NOLOGIN NOBYPASSRLS;
        EXCEPTION
            WHEN duplicate_object THEN NULL;  -- lost a race with a sibling test DB
            WHEN unique_violation THEN NULL;  -- same race, surfacing from pg_authid
            WHEN insufficient_privilege THEN
                RAISE EXCEPTION USING message =
                    'madar_app role is missing and this user cannot create it. '
                    'Run once as a superuser: '
                    'CREATE ROLE madar_app NOLOGIN NOBYPASSRLS; '
                    'GRANT madar_app TO ' || quote_ident(session_user) || ';';
        END;
    END IF;

    -- The app enters the role via SET ROLE, which requires membership.
    -- Superusers (dev/CI) already pass pg_has_role, so this is prod-only.
    IF NOT pg_has_role(session_user, 'madar_app', 'MEMBER') THEN
        BEGIN
            EXECUTE format('GRANT madar_app TO %I', session_user);
        EXCEPTION
            WHEN unique_violation THEN NULL; -- concurrent grant from a sibling test DB
            WHEN insufficient_privilege THEN
                RAISE EXCEPTION USING message =
                    'Cannot grant madar_app to ' || quote_ident(session_user) || '. '
                    'Run once as a superuser: '
                    'GRANT madar_app TO ' || quote_ident(session_user) || ';';
        END;
    END IF;
END
$$;

GRANT USAGE ON SCHEMA public TO madar_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO madar_app;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO madar_app;

-- Tables/sequences created by FUTURE migrations (run by this same owner role
-- in every environment) inherit the grants automatically.
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO madar_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT USAGE, SELECT ON SEQUENCES TO madar_app;

-- Migration bookkeeping is never the app role's business.
REVOKE ALL ON _sqlx_migrations FROM madar_app;
