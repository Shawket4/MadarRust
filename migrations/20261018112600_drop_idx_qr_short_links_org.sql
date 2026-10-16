-- no-transaction
DROP INDEX CONCURRENTLY IF EXISTS idx_qr_short_links_org;
-- DB architecture Phase 2.3 (redundant index: a strict prefix of qr_short_links_org_id_kind_target_ref_key (org_id, kind, target_ref), same predicate; that index serves every query this one did).
-- One statement per file: DROP INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
