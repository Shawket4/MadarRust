-- no-transaction
DROP INDEX CONCURRENTLY IF EXISTS idx_branch_tables_branch;
-- DB architecture Phase 2.3 (redundant index: a strict prefix of branch_tables_branch_id_label_key (branch_id, label), same predicate; that index serves every query this one did).
-- One statement per file: DROP INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
