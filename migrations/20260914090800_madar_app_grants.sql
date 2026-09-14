-- The tenant role's grants, re-asserted.
--
-- Every table the tills rework, payment-method availability, reconciliation,
-- the changefeed, assets and client_seen created carries an explicit GRANT to
-- madar_app, and 20260708000000 set DEFAULT PRIVILEGES for the rest. But
-- default privileges only apply to objects created by the role that set them:
-- a database restored from a dump without ACLs (madar_prodcopy_assets2), or
-- migrated by a different owner, leaves madar_app with nothing on the older
-- tables and every tenant request fails with "permission denied". Granting
-- again is idempotent and cheap, so do it for everything.
GRANT USAGE ON SCHEMA public TO madar_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO madar_app;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO madar_app;

-- The legacy `/tills` entity adapter (POS < 0.7) reads the archived default
-- drawer id through the tenant pool (`tills::legacy::legacy_till_entity_id`,
-- `legacy_routes::legacy_list_till_entities`). Without USAGE on `archive` the
-- read failed and the adapter silently fell back to the synthesized id, so an
-- old tablet saw a different drawer id than the one it had stored. Read-only,
-- and tenant-isolated like every other table.
DO $$
BEGIN
    IF to_regclass('archive.till_entities') IS NOT NULL THEN
        EXECUTE 'GRANT USAGE ON SCHEMA archive TO madar_app';
        EXECUTE 'GRANT SELECT ON TABLE archive.till_entities TO madar_app';
        EXECUTE 'ALTER TABLE archive.till_entities ENABLE ROW LEVEL SECURITY';
        IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE schemaname = 'archive'
                         AND tablename = 'till_entities' AND policyname = 'tenant_isolation') THEN
            EXECUTE 'CREATE POLICY tenant_isolation ON archive.till_entities FOR SELECT '
                    'USING (org_id = (SELECT current_setting(''app.org_id'', true)::uuid))';
        END IF;
    END IF;
END
$$;
