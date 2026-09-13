-- Tenant isolation for the two asset bookkeeping tables 090400 left without RLS.
ALTER TABLE asset_bundle_dirty ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON asset_bundle_dirty FOR ALL
    USING (EXISTS (SELECT 1 FROM branches b WHERE b.id = asset_bundle_dirty.branch_id
                     AND b.org_id = (SELECT current_setting('app.org_id', true)::uuid)));

ALTER TABLE asset_backfill_items ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON asset_backfill_items FOR ALL
    USING (org_id IS NULL OR org_id = (SELECT current_setting('app.org_id', true)::uuid));
