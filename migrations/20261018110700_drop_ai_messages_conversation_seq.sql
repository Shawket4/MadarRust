-- no-transaction
DROP INDEX CONCURRENTLY IF EXISTS ai_messages_conversation_seq;
-- DB architecture Phase 2.3 (redundant index: a strict prefix of ai_messages_seq_unique (conversation_id, seq), same predicate; that index serves every query this one did).
-- One statement per file: DROP INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
