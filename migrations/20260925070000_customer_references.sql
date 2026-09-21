-- One customer, step 5 (CUSTOMERS_UNIFICATION_DESIGN.md §2.5, §3.5): every row
-- that involves a person points at the customer, and keeps what was typed.
--
-- `customer_id` is a SOFT reference everywhere, exactly like `orders.customer_id`
-- (no foreign key): an online order, a booking or a bill is never refused over
-- its customer. The name/phone columns beside it stay as the immutable snapshot
-- of what was typed; the id is who it was.
--
-- Backfill: by canonical phone within the org. A phone nobody holds yet makes a
-- customer — the same rule as `customers::resolve_or_create`: one live customer
-- per (org, phone), the unique index decides, the stored name of somebody who
-- already exists is never touched. A phone that fails the shared rule
-- (`phone_canonical` → NULL) makes no customer and the row stays unlinked.
--
-- Idempotent: every statement is guarded, and a second run links nothing new.

ALTER TABLE delivery_orders
    ADD COLUMN IF NOT EXISTS customer_id      uuid NULL,
    -- The saved address this order was sent to (`customer_addresses`, next
    -- migration). Soft, like the customer: an address can be erased.
    ADD COLUMN IF NOT EXISTS address_id       uuid NULL,
    -- "Ordered by X for Y": the snapshot phone is not the customer's own
    -- (design §4.4, one-time order for someone else).
    ADD COLUMN IF NOT EXISTS contact_override boolean NOT NULL DEFAULT false;
ALTER TABLE bookings     ADD COLUMN IF NOT EXISTS customer_id uuid NULL;
ALTER TABLE open_tickets ADD COLUMN IF NOT EXISTS customer_id uuid NULL;

CREATE INDEX IF NOT EXISTS delivery_orders_customer ON delivery_orders (customer_id, created_at DESC)
    WHERE customer_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS bookings_customer ON bookings (customer_id, starts_at DESC)
    WHERE customer_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS open_tickets_customer ON open_tickets (customer_id)
    WHERE customer_id IS NOT NULL;

-- ── Backfill ────────────────────────────────────────────────────────────────
DO $$
DECLARE
    made_online  bigint := 0;
    made_booking bigint := 0;
    linked_d     bigint := 0;
    linked_b     bigint := 0;
    linked_t     bigint := 0;
    linked_o_d   bigint := 0;
    linked_o_l   bigint := 0;
    no_phone_d   bigint := 0;
    no_phone_b   bigint := 0;
BEGIN
    -- 1. Online guests nobody knows yet. The EARLIEST order names them (that is
    --    who they said they were on first contact) and dates `first_seen_at`.
    WITH guests AS (
        SELECT DISTINCT ON (d.org_id, phone_canonical(d.customer_phone))
               d.org_id, phone_canonical(d.customer_phone) AS key,
               d.customer_phone AS phone, d.customer_name AS name,
               d.branch_id, d.created_at
          FROM delivery_orders d
         WHERE d.customer_id IS NULL
           AND phone_canonical(d.customer_phone) IS NOT NULL
           AND btrim(COALESCE(d.customer_name, '')) <> ''
         ORDER BY d.org_id, phone_canonical(d.customer_phone), d.created_at
    ), ins AS (
        INSERT INTO customers (org_id, name, phone, phone_key, source, created_branch_id,
                               created_at, first_seen_at)
        SELECT g.org_id, left(btrim(g.name), 120), g.phone, g.key, 'online', g.branch_id,
               g.created_at, g.created_at
          FROM guests g
         WHERE NOT EXISTS (SELECT 1 FROM customers c
                            WHERE c.org_id = g.org_id AND c.phone_key = g.key
                              AND c.merged_into IS NULL AND c.erased_at IS NULL)
        ON CONFLICT DO NOTHING
        RETURNING 1
    )
    SELECT count(*) INTO made_online FROM ins;

    -- 2. Booking guests, the same way.
    WITH guests AS (
        SELECT DISTINCT ON (b.org_id, phone_canonical(b.guest_phone))
               b.org_id, phone_canonical(b.guest_phone) AS key,
               b.guest_phone AS phone, b.guest_name AS name,
               b.branch_id, b.created_at
          FROM bookings b
         WHERE b.customer_id IS NULL
           AND phone_canonical(b.guest_phone) IS NOT NULL
           AND btrim(COALESCE(b.guest_name, '')) <> ''
         ORDER BY b.org_id, phone_canonical(b.guest_phone), b.created_at
    ), ins AS (
        INSERT INTO customers (org_id, name, phone, phone_key, source, created_branch_id,
                               created_at, first_seen_at)
        SELECT g.org_id, left(btrim(g.name), 120), g.phone, g.key, 'booking', g.branch_id,
               g.created_at, g.created_at
          FROM guests g
         WHERE NOT EXISTS (SELECT 1 FROM customers c
                            WHERE c.org_id = g.org_id AND c.phone_key = g.key
                              AND c.merged_into IS NULL AND c.erased_at IS NULL)
        ON CONFLICT DO NOTHING
        RETURNING 1
    )
    SELECT count(*) INTO made_booking FROM ins;

    -- A customer that existed already may have been seen earlier than it knew.
    UPDATE customers c SET first_seen_at = s.first_at
      FROM (SELECT org_id, key, min(at) AS first_at FROM (
                SELECT org_id, phone_canonical(customer_phone) AS key, created_at AS at FROM delivery_orders
                UNION ALL
                SELECT org_id, phone_canonical(guest_phone), created_at FROM bookings) x
             WHERE key IS NOT NULL GROUP BY org_id, key) s
     WHERE c.org_id = s.org_id AND c.phone_key = s.key
       AND c.merged_into IS NULL AND c.erased_at IS NULL
       AND c.first_seen_at > s.first_at;

    -- 3. Link.
    UPDATE delivery_orders d SET customer_id = c.id
      FROM customers c
     WHERE d.customer_id IS NULL
       AND c.org_id = d.org_id AND c.phone_key = phone_canonical(d.customer_phone)
       AND c.merged_into IS NULL AND c.erased_at IS NULL;
    GET DIAGNOSTICS linked_d = ROW_COUNT;

    UPDATE bookings b SET customer_id = c.id
      FROM customers c
     WHERE b.customer_id IS NULL
       AND c.org_id = b.org_id AND c.phone_key = phone_canonical(b.guest_phone)
       AND c.merged_into IS NULL AND c.erased_at IS NULL;
    GET DIAGNOSTICS linked_b = ROW_COUNT;

    -- 4. The sale a delivery became belongs to whoever ordered it …
    UPDATE orders o SET customer_id = d.customer_id
      FROM delivery_orders d
     WHERE o.customer_id IS NULL AND d.customer_id IS NOT NULL
       AND (o.delivery_order_id = d.id OR d.order_id = o.id);
    GET DIAGNOSTICS linked_o_d = ROW_COUNT;

    -- … and a sale rung for a loyalty member belongs to that member: the
    -- membership shares the customer's id (design §2.1).
    UPDATE orders o SET customer_id = customers_resolve(c.org_id, c.id)
      FROM customers c
     WHERE o.customer_id IS NULL AND o.loyalty_customer_id IS NOT NULL
       AND c.id = o.loyalty_customer_id
       AND customers_resolve(c.org_id, c.id) IS NOT NULL;
    GET DIAGNOSTICS linked_o_l = ROW_COUNT;

    -- 5. A bill has no phone of its own; it is whoever its sale or its booking is.
    UPDATE open_tickets t SET customer_id = COALESCE(
               (SELECT o.customer_id FROM orders o WHERE o.id = t.order_id),
               (SELECT b.customer_id FROM bookings b WHERE b.id = t.booking_id))
     WHERE t.customer_id IS NULL
       AND (t.order_id IS NOT NULL OR t.booking_id IS NOT NULL)
       AND COALESCE(
               (SELECT o.customer_id FROM orders o WHERE o.id = t.order_id),
               (SELECT b.customer_id FROM bookings b WHERE b.id = t.booking_id)) IS NOT NULL;
    GET DIAGNOSTICS linked_t = ROW_COUNT;

    SELECT count(*) INTO no_phone_d FROM delivery_orders
     WHERE customer_id IS NULL AND phone_canonical(customer_phone) IS NULL;
    SELECT count(*) INTO no_phone_b FROM bookings
     WHERE customer_id IS NULL AND phone_canonical(guest_phone) IS NULL;

    RAISE NOTICE 'customer_references: customers created: % online, % booking', made_online, made_booking;
    RAISE NOTICE 'customer_references: linked % delivery orders, % bookings, % open tickets', linked_d, linked_b, linked_t;
    RAISE NOTICE 'customer_references: orders linked: % via delivery, % via loyalty member', linked_o_d, linked_o_l;
    RAISE NOTICE 'customer_references: left unlinked (phone fails the rule): % delivery orders, % bookings', no_phone_d, no_phone_b;
END $$;

-- ── Verification ────────────────────────────────────────────────────────────
DO $$
DECLARE
    bad bigint;
BEGIN
    -- Every row with a valid phone whose person can still be named is linked.
    -- (An erased person's phone is gone from `customers`, so a row that only
    -- ever matched them is legitimately unlinked: excluded by the name test
    -- being applied to rows, and by erasure blanking the snapshots too.)
    SELECT count(*) INTO bad FROM delivery_orders d
     WHERE d.customer_id IS NULL AND phone_canonical(d.customer_phone) IS NOT NULL
       AND btrim(COALESCE(d.customer_name, '')) <> '';
    IF bad > 0 THEN
        RAISE EXCEPTION 'customer_references: % delivery orders with a valid phone are unlinked', bad;
    END IF;
    SELECT count(*) INTO bad FROM bookings b
     WHERE b.customer_id IS NULL AND phone_canonical(b.guest_phone) IS NOT NULL
       AND btrim(COALESCE(b.guest_name, '')) <> '';
    IF bad > 0 THEN
        RAISE EXCEPTION 'customer_references: % bookings with a valid phone are unlinked', bad;
    END IF;
    -- A reference never crosses tenants.
    SELECT count(*) INTO bad FROM delivery_orders d JOIN customers c ON c.id = d.customer_id
     WHERE c.org_id <> d.org_id;
    IF bad > 0 THEN
        RAISE EXCEPTION 'customer_references: % delivery orders point at another org''s customer', bad;
    END IF;
    SELECT count(*) INTO bad FROM bookings b JOIN customers c ON c.id = b.customer_id
     WHERE c.org_id <> b.org_id;
    IF bad > 0 THEN
        RAISE EXCEPTION 'customer_references: % bookings point at another org''s customer', bad;
    END IF;
    -- A finalized delivery and its sale name the same person.
    SELECT count(*) INTO bad FROM orders o JOIN delivery_orders d ON d.id = o.delivery_order_id
     WHERE d.customer_id IS NOT NULL AND o.customer_id IS NULL;
    IF bad > 0 THEN
        RAISE EXCEPTION 'customer_references: % delivery sales lack their customer', bad;
    END IF;
END $$;
