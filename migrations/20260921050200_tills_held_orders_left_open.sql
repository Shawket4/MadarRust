-- Deferred feature 5 (keep the previous teller's queue): a till closed while
-- held (parked) orders or open counter carts were still on the device records
-- how many were left for the next till, and what they came to. The teller was
-- warned and chose to close anyway; the orders stay parked. NULL for every
-- close from an older client and every forced close.
--
-- SOURCE TABLES: none added (tills already emits; the till projection reads
-- TILL_COLUMNS, which now carries these two).
ALTER TABLE public.tills
    ADD COLUMN held_orders_left_open integer NULL
        CONSTRAINT tills_held_orders_left_open_nonneg CHECK (held_orders_left_open IS NULL OR held_orders_left_open >= 0),
    ADD COLUMN held_orders_left_open_total integer NULL
        CONSTRAINT tills_held_orders_left_open_total_nonneg CHECK (held_orders_left_open_total IS NULL OR held_orders_left_open_total >= 0);
