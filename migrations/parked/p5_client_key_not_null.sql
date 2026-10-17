-- PARKED (DB audit 2026-10-16 Phase 5.4). NOT APPLIED.
--
-- Every client-originated ledger row carries its idempotency key (the audit's
-- `client_key`): orders.idempotency_key, open_tickets.idempotency_key,
-- open_ticket_rounds.idempotency_key, delivery_orders.idempotency_key,
-- till_cash_movements.client_ref, order_refunds.client_ref.
--
-- Unpark only when ALL hold:
--   1. The version gate (client_seen) shows every active device on a build that
--      always sends a key (the owner names the build).
--   2. Reconcile: per org, count(*) unchanged before/after, and 0 NULLs after.
--
-- Historical NULLs get a deterministic key, md5(id)::uuid: rerunning changes
-- nothing (idempotent), and it can never collide with a real key of the same
-- org in practice (both are 122-bit random/hashed uuids). The tables' user
-- triggers are off around each backfill: a key backfill must not re-emit
-- history to every till (sync_emit) or bump updated_at. At prod size each
-- UPDATE is milliseconds; at a much larger size run it in batches first.
--
-- Owner sign-off: ______________________  date: __________

SET LOCAL lock_timeout = '15s';
ALTER TABLE orders DISABLE TRIGGER USER;
UPDATE orders SET idempotency_key = md5(id::text)::uuid WHERE idempotency_key IS NULL;
ALTER TABLE orders ENABLE TRIGGER USER;
ALTER TABLE open_tickets DISABLE TRIGGER USER;
UPDATE open_tickets SET idempotency_key = md5(id::text)::uuid WHERE idempotency_key IS NULL;
ALTER TABLE open_tickets ENABLE TRIGGER USER;
ALTER TABLE open_ticket_rounds DISABLE TRIGGER USER;
UPDATE open_ticket_rounds SET idempotency_key = md5(id::text)::uuid WHERE idempotency_key IS NULL;
ALTER TABLE open_ticket_rounds ENABLE TRIGGER USER;
ALTER TABLE delivery_orders DISABLE TRIGGER USER;
UPDATE delivery_orders SET idempotency_key = md5(id::text)::uuid WHERE idempotency_key IS NULL;
ALTER TABLE delivery_orders ENABLE TRIGGER USER;
ALTER TABLE till_cash_movements DISABLE TRIGGER USER;
UPDATE till_cash_movements SET client_ref = md5(id::text)::uuid WHERE client_ref IS NULL;
ALTER TABLE till_cash_movements ENABLE TRIGGER USER;
ALTER TABLE order_refunds DISABLE TRIGGER USER;
UPDATE order_refunds SET client_ref = md5(id::text)::uuid WHERE client_ref IS NULL;
ALTER TABLE order_refunds ENABLE TRIGGER USER;
ALTER TABLE orders ALTER COLUMN idempotency_key SET NOT NULL;
ALTER TABLE open_tickets ALTER COLUMN idempotency_key SET NOT NULL;
ALTER TABLE open_ticket_rounds ALTER COLUMN idempotency_key SET NOT NULL;
ALTER TABLE delivery_orders ALTER COLUMN idempotency_key SET NOT NULL;
ALTER TABLE till_cash_movements ALTER COLUMN client_ref SET NOT NULL;
ALTER TABLE order_refunds ALTER COLUMN client_ref SET NOT NULL;
