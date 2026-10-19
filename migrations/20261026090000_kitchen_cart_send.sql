-- A counter cart sent to the kitchen before it is paid (owner, 2026-10-18).
--
-- The till's "Send to kitchen" put a counter order on the paper printer only;
-- a kitchen screen heard of it over the LAN, never from the server, so a
-- closed screen got no push. Now the send reaches the server too, as a
-- kitchen ticket whose source is the CART: `cart_key`, the key the till will
-- sell the order under. When the sale lands it takes that ticket over
-- (`order_id` set, `cart_key` cleared), so the board keeps one ticket and
-- what the cook already finished stays finished.
--
-- A ticket has exactly one source, now one of three. `source_type` /
-- `source_id` are generated from the references (20260912040000), so they
-- are rebuilt to know the third one, with the unique index on them.

ALTER TABLE kitchen_tickets ADD COLUMN cart_key uuid;

ALTER TABLE kitchen_tickets DROP CONSTRAINT kitchen_tickets_has_one_source;
ALTER TABLE kitchen_tickets
    ADD CONSTRAINT kitchen_tickets_has_one_source
    CHECK (num_nonnulls(order_id, open_ticket_id, cart_key) = 1);

DROP INDEX uq_kitchen_tickets_source_round;
ALTER TABLE kitchen_tickets
    DROP COLUMN source_type,
    DROP COLUMN source_id;
ALTER TABLE kitchen_tickets
    ADD COLUMN source_type text NOT NULL
        GENERATED ALWAYS AS (CASE WHEN order_id IS NOT NULL THEN 'order'
                                  WHEN open_ticket_id IS NOT NULL THEN 'open_ticket'
                                  ELSE 'cart' END) STORED,
    ADD COLUMN source_id uuid NOT NULL
        GENERATED ALWAYS AS (COALESCE(order_id, open_ticket_id, cart_key)) STORED;
CREATE UNIQUE INDEX uq_kitchen_tickets_source_round
    ON kitchen_tickets (source_type, source_id, round_number);

-- One ticket per sent cart, per org (keys are the till's, and repeat across orgs).
CREATE UNIQUE INDEX uq_kitchen_tickets_cart_key
    ON kitchen_tickets (org_id, cart_key) WHERE cart_key IS NOT NULL;
