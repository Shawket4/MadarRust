-- Link delivery sales into the existing orders / reporting plumbing.
-- Additive and default-safe: every existing order stays 'dine_in' with a 0 fee,
-- so all current order and report queries keep working unchanged. The orders row
-- for a delivery is created at finalize via the shared create path, then
-- back-links to its delivery_order (which also carries order_id the other way).
ALTER TABLE public.orders
    ADD COLUMN order_type        text    NOT NULL DEFAULT 'dine_in',
    ADD COLUMN delivery_fee      integer NOT NULL DEFAULT 0,
    ADD COLUMN delivery_order_id uuid REFERENCES public.delivery_orders(id) ON DELETE SET NULL;

ALTER TABLE public.orders
    ADD CONSTRAINT orders_order_type_chk     CHECK (order_type IN ('dine_in', 'delivery')),
    ADD CONSTRAINT orders_delivery_fee_nonneg CHECK (delivery_fee >= 0);

CREATE INDEX idx_orders_delivery_order ON public.orders (delivery_order_id) WHERE delivery_order_id IS NOT NULL;
CREATE INDEX idx_orders_order_type     ON public.orders (order_type);
