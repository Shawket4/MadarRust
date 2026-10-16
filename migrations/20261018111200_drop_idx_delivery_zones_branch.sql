-- no-transaction
DROP INDEX CONCURRENTLY IF EXISTS idx_delivery_zones_branch;
-- DB architecture Phase 2.3 (redundant index: a strict prefix of delivery_zones_dist_unique (branch_id, max_road_distance_meters), same predicate; that index serves every query this one did).
-- One statement per file: DROP INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
