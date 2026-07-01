-- Attribute a dine-in order to the WAITER who opened its ticket
-- (open_tickets.opened_by), stamped server-side at settle time. NULL for direct
-- teller sales and delivery orders — hence nullable + ON DELETE SET NULL (a
-- removed staff user must not cascade-delete historical sales).
ALTER TABLE public.orders
    ADD COLUMN waiter_id uuid REFERENCES public.users(id) ON DELETE SET NULL;

-- Partial index: the dashboard filters/segments by waiter, but the column is null
-- for the majority (direct + delivery) — index only the rows that carry a waiter.
CREATE INDEX idx_orders_waiter ON public.orders (waiter_id) WHERE waiter_id IS NOT NULL;
