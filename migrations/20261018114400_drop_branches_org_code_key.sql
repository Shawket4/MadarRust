-- no-transaction
DROP INDEX CONCURRENTLY IF EXISTS branches_org_code_key;
-- DB architecture Phase 2.3 (audit L2: a plain UNIQUE (org_id, code) beside the partial uq_branches_org_code ... WHERE deleted_at IS NULL kept a deleted branch's code reserved for ever. The partial index stays and is the rule).
-- One statement per file: DROP INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
