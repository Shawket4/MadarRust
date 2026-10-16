-- no-transaction
DROP INDEX CONCURRENTLY IF EXISTS uq_open_tickets_idem;
-- DB architecture Phase 2.3 (the GLOBAL idempotency unique, replaced by the per-org uq_open_tickets_org_idem (Phase 1.4). Every lookup by the key is org-scoped (explicit org filter or RLS), so a key two orgs happen to share no longer collides across tenants).
-- One statement per file: DROP INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
