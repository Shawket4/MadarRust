-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_asset_jobs_org_updated ON asset_jobs (org_id, updated_at);
-- DB architecture Phase 1.8: the menu cache fingerprint reads
-- max(asset_jobs.updated_at) per org on every cached GET /menu-items
-- (src/menu/cache.rs). One statement per file: CREATE INDEX CONCURRENTLY
-- cannot run inside a transaction. A failed attempt leaves an INVALID index;
-- the boot guard (src/migrate_guard.rs) drops it before the next attempt.
