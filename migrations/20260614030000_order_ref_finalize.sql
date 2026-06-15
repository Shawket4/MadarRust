-- V31 — order_ref stage 2 of 2: finalize orders.order_ref (UNIQUE + NOT NULL).
--
-- MUST run only AFTER `cargo run --bin backfill-order-ref` has populated every
-- existing order. On a fresh database (e.g. the #[sqlx::test] harness) orders is
-- empty, so this is trivially satisfied. The guard below fails loudly — rolling
-- the migration transaction back — instead of producing a cryptic NOT NULL
-- violation if this is ever applied before the backfill on a populated DB.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM public.orders WHERE order_ref IS NULL) THEN
        RAISE EXCEPTION
            'orders.order_ref has NULL rows — run `cargo run --bin backfill-order-ref` before applying this migration';
    END IF;
END $$;

-- A single global UNIQUE is sufficient: the branch code is embedded in the string,
-- so two orders in different branches can never collide.
--
-- NOTE: sqlx 0.7 wraps each migration in a transaction, so CREATE INDEX
-- CONCURRENTLY cannot be used here. On a large production orders table, a DBA may
-- build this index by hand with CONCURRENTLY *before* applying this migration:
--   CREATE UNIQUE INDEX CONCURRENTLY orders_order_ref_key ON public.orders (order_ref);
-- The IF NOT EXISTS below then makes this step a no-op.
CREATE UNIQUE INDEX IF NOT EXISTS orders_order_ref_key ON public.orders (order_ref);
ALTER TABLE public.orders ALTER COLUMN order_ref SET NOT NULL;
