-- Rewards dashboard: the card designer (REWARDS_DASHBOARD_DESIGN.md §3).
--
-- One row per organisation: the design being edited (`draft`) and the one the
-- wallet passes wear (`published`), both madar-card `CardDesign` JSON (v1),
-- validated by madar-card before they are written. No row, or nothing
-- published, means the passes are built exactly as before (the classic look).
-- `revision` bumps on every publish/unpublish: it is in the strip cache key,
-- the stored-pass fingerprint and the Google hero's URL.

CREATE TABLE IF NOT EXISTS loyalty_card_designs (
    org_id       uuid PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    draft        jsonb NULL,
    published    jsonb NULL,
    revision     integer NOT NULL DEFAULT 0,
    published_at timestamptz NULL,
    published_by uuid NULL,
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   uuid NULL
);
ALTER TABLE loyalty_card_designs ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON loyalty_card_designs;
CREATE POLICY tenant_isolation ON loyalty_card_designs FOR ALL
    USING ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE)
    WITH CHECK ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE);
GRANT SELECT, INSERT, UPDATE, DELETE ON loyalty_card_designs TO madar_app;
