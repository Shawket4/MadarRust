-- Deferred feature 5 (keep the previous teller's queue): a held order started by
-- one person and settled by another after a teller switch records BOTH. The
-- order's `teller_id` stays the person who rang it (the till's drawer and every
-- money report); `started_by` is the person who started the cart, stamped from
-- the POS's additive `started_by` on the order request. NULL for every sale rung
-- by the person who started it, every older client and every dashboard/delivery
-- order. ON DELETE SET NULL: a removed staff member never cascades into history.
--
-- SOURCE TABLES: none added (orders already emits; the projection is unchanged).
ALTER TABLE public.orders
    ADD COLUMN started_by uuid REFERENCES public.users(id) ON DELETE SET NULL;
