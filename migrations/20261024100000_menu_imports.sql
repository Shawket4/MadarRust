-- The self-serve menu import (POST /menu/bulk, Madar MENU_IMPORT_PLAN.md):
-- one row per real import (a dry run writes nothing). Who imported what and
-- when, the counts, and a hash of the body; it also caps how many imports an
-- org may run in a day.
CREATE TABLE IF NOT EXISTS menu_imports (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id      uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id     uuid REFERENCES users(id) ON DELETE SET NULL,
    mode        text NOT NULL CHECK (mode IN ('add', 'replace')),
    -- madar_menu::BulkSummary of what was written.
    summary     jsonb NOT NULL,
    -- What a replace archived: {categories, items, groups}; NULL for add.
    archived    jsonb,
    body_sha256 text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_menu_imports_org_created ON menu_imports (org_id, created_at DESC);

ALTER TABLE menu_imports ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON menu_imports;
CREATE POLICY tenant_isolation ON menu_imports FOR ALL
    USING ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE)
    WITH CHECK ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE);
GRANT SELECT, INSERT ON menu_imports TO madar_app;
