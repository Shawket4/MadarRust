-- One customer, step 6 (CUSTOMERS_UNIFICATION_DESIGN.md §2.6): the places a
-- customer has had things sent to, as rows of their own.
--
-- Until now "past locations" was a `DISTINCT ON` scan of `delivery_orders` by
-- phone string. It could not survive a phone change, could not be erased
-- without touching order history, and re-derived the same list on every open.
--
-- Written ONLY when an order is successfully placed (design §4.3: an abandoned
-- cart saves nothing), and deduplicated on write by ONE function,
-- `customer_address_upsert`, which the application and this backfill both call
-- so the rule cannot drift:
--   * the normalised text is equal, or
--   * the pin is within 30 m AND the unit is the same
--   → `use_count` / `last_used_at` are bumped instead of inserting.
--
-- `branch_id` and `channel` are not in the design's column list: they are the
-- branch and channel the address was LAST used with, which is what the public
-- "past locations" response has always carried and what order-now revalidates
-- against. `norm_key` is the normalised text, stored so equality is an index
-- lookup.

CREATE TABLE IF NOT EXISTS customer_addresses (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id           uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    customer_id      uuid NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    label            text NULL,
    place_name       text NULL,
    floor            text NULL,
    unit_number      text NULL,
    landmark         text NULL,
    address_line     text NULL,
    delivery_notes   text NULL,
    lat              double precision NULL,
    lng              double precision NULL,
    delivery_zone_id uuid NULL REFERENCES delivery_zones(id) ON DELETE SET NULL,
    branch_id        uuid NULL REFERENCES branches(id) ON DELETE SET NULL,
    channel          text NOT NULL DEFAULT 'outside',
    norm_key         text NOT NULL DEFAULT '',
    use_count        integer NOT NULL DEFAULT 1,
    last_used_at     timestamptz NOT NULL DEFAULT now(),
    created_at       timestamptz NOT NULL DEFAULT now(),
    erased_at        timestamptz NULL,
    CONSTRAINT customer_addresses_pin_pair CHECK ((lat IS NULL) = (lng IS NULL))
);
CREATE INDEX IF NOT EXISTS customer_addresses_recent
    ON customer_addresses (customer_id, last_used_at DESC) WHERE erased_at IS NULL;
CREATE INDEX IF NOT EXISTS customer_addresses_norm
    ON customer_addresses (customer_id, channel, norm_key) WHERE erased_at IS NULL;

ALTER TABLE customer_addresses ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS customer_addresses_tenant ON customer_addresses;
CREATE POLICY customer_addresses_tenant ON customer_addresses
    USING (org_id = NULLIF(current_setting('app.org_id', true), '')::uuid);
GRANT SELECT, INSERT, UPDATE, DELETE ON customer_addresses TO madar_app;

-- Lower-cased, whitespace-collapsed, punctuation-free: what makes
-- "12 Tahrir St., Flat 4" and "12 tahrir st flat 4" the same place.
CREATE OR REPLACE FUNCTION customer_address_norm(
    p_address_line text, p_place_name text, p_floor text, p_unit text
) RETURNS text LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT btrim(regexp_replace(
               regexp_replace(
                   lower(concat_ws(' | ', NULLIF(btrim(COALESCE(p_address_line, '')), ''),
                                          NULLIF(btrim(COALESCE(p_place_name, '')), ''),
                                          NULLIF(btrim(COALESCE(p_floor, '')), ''),
                                          NULLIF(btrim(COALESCE(p_unit, '')), ''))),
                   '[.,;:#/\\()\-_''"،]+', ' ', 'g'),
               '\s+', ' ', 'g'))
$$;

-- Great-circle metres. Mirrors `geo::haversine_meters`.
CREATE OR REPLACE FUNCTION geo_distance_m(lat1 double precision, lng1 double precision,
                                          lat2 double precision, lng2 double precision)
RETURNS double precision LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT 6371000.0 * 2.0 * asin(LEAST(1.0, sqrt(
               power(sin(radians(lat2 - lat1) / 2.0), 2)
             + cos(radians(lat1)) * cos(radians(lat2)) * power(sin(radians(lng2 - lng1) / 2.0), 2))))
$$;

-- THE write. Returns the address row the order was sent to (new or bumped), or
-- NULL when there is nothing worth keeping (no text at all).
CREATE OR REPLACE FUNCTION customer_address_upsert(
    p_org uuid, p_customer uuid, p_branch uuid, p_channel text,
    p_place_name text, p_floor text, p_unit text, p_landmark text,
    p_address_line text, p_notes text,
    p_lat double precision, p_lng double precision, p_zone uuid, p_used_at timestamptz
) RETURNS uuid LANGUAGE plpgsql AS $$
DECLARE
    k     text := customer_address_norm(p_address_line, p_place_name, p_floor, p_unit);
    unit  text := lower(btrim(COALESCE(p_unit, '')));
    found uuid;
BEGIN
    IF k = '' THEN
        RETURN NULL;
    END IF;
    -- Two orders of one customer landing together must not both insert.
    PERFORM pg_advisory_xact_lock(hashtextextended('customer_address:' || p_customer::text, 0));

    SELECT a.id INTO found
      FROM customer_addresses a
     WHERE a.customer_id = p_customer AND a.erased_at IS NULL AND a.channel = p_channel
       AND (a.norm_key = k
            OR (p_lat IS NOT NULL AND a.lat IS NOT NULL
                AND lower(btrim(COALESCE(a.unit_number, ''))) = unit
                AND geo_distance_m(a.lat, a.lng, p_lat, p_lng) <= 30.0))
     ORDER BY (a.norm_key = k) DESC, a.last_used_at DESC
     LIMIT 1;

    IF found IS NOT NULL THEN
        UPDATE customer_addresses a
           SET use_count        = a.use_count + 1,
               last_used_at     = GREATEST(a.last_used_at, p_used_at),
               -- The latest order's details win where it gave any.
               branch_id        = COALESCE(p_branch, a.branch_id),
               lat              = COALESCE(p_lat, a.lat),
               lng              = COALESCE(p_lng, a.lng),
               delivery_zone_id = COALESCE(p_zone, a.delivery_zone_id),
               landmark         = COALESCE(NULLIF(btrim(COALESCE(p_landmark, '')), ''), a.landmark),
               delivery_notes   = COALESCE(NULLIF(btrim(COALESCE(p_notes, '')), ''), a.delivery_notes)
         WHERE a.id = found;
        RETURN found;
    END IF;

    INSERT INTO customer_addresses
        (org_id, customer_id, place_name, floor, unit_number, landmark, address_line,
         delivery_notes, lat, lng, delivery_zone_id, branch_id, channel, norm_key,
         last_used_at, created_at)
    VALUES (p_org, p_customer, NULLIF(btrim(COALESCE(p_place_name, '')), ''),
            NULLIF(btrim(COALESCE(p_floor, '')), ''), NULLIF(btrim(COALESCE(p_unit, '')), ''),
            NULLIF(btrim(COALESCE(p_landmark, '')), ''), NULLIF(btrim(COALESCE(p_address_line, '')), ''),
            NULLIF(btrim(COALESCE(p_notes, '')), ''),
            CASE WHEN p_lng IS NOT NULL THEN p_lat END, CASE WHEN p_lat IS NOT NULL THEN p_lng END,
            p_zone, p_branch, p_channel, k, p_used_at, p_used_at)
    RETURNING id INTO found;
    RETURN found;
END $$;
GRANT EXECUTE ON FUNCTION customer_address_norm(text, text, text, text) TO madar_app;
GRANT EXECUTE ON FUNCTION geo_distance_m(double precision, double precision, double precision, double precision) TO madar_app;
GRANT EXECUTE ON FUNCTION customer_address_upsert(uuid, uuid, uuid, text, text, text, text, text, text, text,
    double precision, double precision, uuid, timestamptz) TO madar_app;

-- ── Backfill: every linked, not-rejected delivery order, oldest first ───────
DO $$
DECLARE
    r       record;
    aid     uuid;
    orders_ bigint := 0;
    made    bigint := 0;
BEGIN
    FOR r IN
        SELECT d.id, d.org_id, customers_resolve(d.org_id, d.customer_id) AS customer,
               d.branch_id, d.channel::text AS channel, d.place_name, d.floor, d.unit_number,
               d.landmark, d.address_line, d.delivery_notes, d.customer_lat, d.customer_lng,
               (SELECT z.id FROM delivery_zones z WHERE z.id = d.delivery_zone_id) AS zone,
               d.created_at
          FROM delivery_orders d
         WHERE d.customer_id IS NOT NULL AND d.address_id IS NULL
           AND d.contact_override = false
           AND d.channel::text <> 'pickup'
           AND d.status::text <> 'rejected'
         ORDER BY d.created_at
    LOOP
        CONTINUE WHEN r.customer IS NULL;
        -- An erased customer keeps no addresses.
        CONTINUE WHEN EXISTS (SELECT 1 FROM customers c WHERE c.id = r.customer AND c.erased_at IS NOT NULL);
        aid := customer_address_upsert(r.org_id, r.customer, r.branch_id, r.channel,
                   r.place_name, r.floor, r.unit_number, r.landmark, r.address_line,
                   r.delivery_notes, r.customer_lat, r.customer_lng, r.zone, r.created_at);
        IF aid IS NOT NULL THEN
            UPDATE delivery_orders SET address_id = aid WHERE id = r.id;
            orders_ := orders_ + 1;
        END IF;
    END LOOP;
    SELECT count(*) INTO made FROM customer_addresses;
    RAISE NOTICE 'customer_addresses: % delivery orders folded into % addresses', orders_, made;
END $$;

-- ── Verification ────────────────────────────────────────────────────────────
DO $$
DECLARE
    bad bigint;
BEGIN
    SELECT count(*) INTO bad FROM customer_addresses a JOIN customers c ON c.id = a.customer_id
     WHERE c.org_id <> a.org_id;
    IF bad > 0 THEN
        RAISE EXCEPTION 'customer_addresses: % addresses belong to another org''s customer', bad;
    END IF;
    SELECT count(*) INTO bad FROM (
        SELECT 1 FROM customer_addresses WHERE erased_at IS NULL
         GROUP BY customer_id, channel, norm_key HAVING count(*) > 1) x;
    IF bad > 0 THEN
        RAISE EXCEPTION 'customer_addresses: % duplicate addresses survived the backfill', bad;
    END IF;
    SELECT count(*) INTO bad FROM delivery_orders d
     WHERE d.address_id IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM customer_addresses a WHERE a.id = d.address_id);
    IF bad > 0 THEN
        RAISE EXCEPTION 'customer_addresses: % delivery orders point at a missing address', bad;
    END IF;
    IF customer_address_norm('12 Tahrir St., Flat 4', NULL, NULL, NULL)
       IS DISTINCT FROM customer_address_norm('  12 tahrir st   flat 4 ', NULL, NULL, NULL) THEN
        RAISE EXCEPTION 'customer_address_norm does not fold punctuation and case';
    END IF;
END $$;
