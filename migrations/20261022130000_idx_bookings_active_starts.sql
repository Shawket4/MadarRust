-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_bookings_active_starts ON bookings (starts_at) WHERE status IN ('confirmed', 'seated');
-- Query survey 2026-10-17 #11: the bookings sweep (every 60 s) reads only
-- the live bookings, confirmed or seated, by time. Without this every tick
-- scanned the whole bookings history (only `(branch_id, ...)` indexes exist).
-- One statement per file: CREATE INDEX CONCURRENTLY cannot run inside a
-- transaction, and sqlx runs a `-- no-transaction` file as one simple query.
-- A failed attempt leaves an INVALID index; the boot guard
-- (src/migrate_guard.rs) drops it before the next attempt.
