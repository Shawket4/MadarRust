-- One id per person: a loyalty membership IS a customer
-- (CUSTOMERS_UNIFICATION_DESIGN.md §2.1, step 3).
--
-- `customers` becomes the only identity. A membership is a row in
-- `loyalty_customers` whose id EQUALS the customer's id, held by a foreign
-- key. The member's id is the one that survives, because it is baked into
-- wallet passes and seven foreign keys; a manual customer that turns out to be
-- the same person is folded into it through the merge chain, so every id a
-- till or an order still holds keeps resolving.
--
-- The nullable `customers.loyalty_customer_id` link goes away. The API and the
-- sync `customer` payload keep a field of that name for one release, computed
-- as `id` when a live membership exists.
--
-- SOURCE TABLES (additions; machine-read by the migration tests together with
-- the earlier changefeed headers). Format:  table -> type[, type…]  (scope)
--   loyalty_customers          -> customer                    (org fan-out)

-- ── Where a customer came from ──────────────────────────────────────────────
ALTER TABLE customers
    ADD COLUMN IF NOT EXISTS source        text NOT NULL DEFAULT 'pos',
    ADD COLUMN IF NOT EXISTS first_seen_at timestamptz NOT NULL DEFAULT now();
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'customers_source') THEN
        ALTER TABLE customers ADD CONSTRAINT customers_source CHECK (source IN
            ('pos', 'online', 'loyalty', 'booking', 'table_qr', 'aggregator', 'dashboard'));
    END IF;
END $$;
-- Rows that predate the column were first seen when they were created.
UPDATE customers SET first_seen_at = created_at WHERE first_seen_at > created_at;

-- ── The backfill ────────────────────────────────────────────────────────────
DO $$
DECLARE
    n_members  bigint;
    n_new      bigint;
    n_folded   bigint;
    n_keyless  bigint;
    n_conflict bigint;
    n_invalid  bigint := 0;
    r          record;
BEGIN
    -- Every member that has no customers row yet. Re-running finds none.
    CREATE TEMP TABLE _members ON COMMIT DROP AS
    SELECT m.id, m.org_id, m.name, m.phone, m.joined_branch_id, m.enrolled_at, m.deleted_at,
           CASE WHEN m.deleted_at IS NULL THEN phone_canonical(m.phone) END AS phone_key
      FROM loyalty_customers m
     WHERE NOT EXISTS (SELECT 1 FROM customers c WHERE c.id = m.id);
    SELECT count(*) INTO n_members FROM _members;

    -- A live member whose stored phone fails the shared rule (a truncated
    -- mobile, free text) KEEPS the membership: the customer is created with the
    -- phone as it was typed and no key. Listed one by one, for a human to fix.
    FOR r IN SELECT m.id, m.org_id, m.phone FROM _members m
              WHERE m.deleted_at IS NULL AND m.phone_key IS NULL
                AND NULLIF(btrim(m.phone), '') IS NOT NULL
              ORDER BY m.org_id, m.id
    LOOP
        n_invalid := n_invalid + 1;
        RAISE NOTICE 'customers shared key: member % (org %) keeps the phone "%" without a phone key: it is not a valid phone',
            r.id, r.org_id, r.phone;
    END LOOP;
    IF n_invalid > 0 THEN
        RAISE NOTICE 'customers shared key: % live members have a phone that fails the canonical rule', n_invalid;
    END IF;

    -- Two live members of one tenant whose phones canonicalise to the same
    -- number cannot both hold it. Loyalty normalised on the way in and had its
    -- own unique index, so this is not expected; if it happens the earlier
    -- member keeps the key and the later one keeps its row and typed phone.
    UPDATE _members m SET phone_key = NULL
     WHERE m.phone_key IS NOT NULL
       AND EXISTS (SELECT 1 FROM _members o
                    WHERE o.org_id = m.org_id AND o.phone_key = m.phone_key
                      AND (o.enrolled_at, o.id) < (m.enrolled_at, m.id));
    GET DIAGNOSTICS n_keyless = ROW_COUNT;
    IF n_keyless > 0 THEN
        RAISE WARNING 'customers shared key: % members share a canonical phone with an earlier member and were left without a phone key', n_keyless;
    END IF;

    -- Which live customer is which member: the phone decides; the old link
    -- decides only when the phone says nothing. Always within ONE tenant.
    CREATE TEMP TABLE _fold ON COMMIT DROP AS
    SELECT c.id AS customer_id, COALESCE(mp.id, ml.id) AS member_id,
           (mp.id IS NOT NULL) AS by_phone
      FROM customers c
      LEFT JOIN _members mp ON mp.org_id = c.org_id AND mp.deleted_at IS NULL
                           AND mp.phone_key IS NOT NULL AND mp.phone_key = c.phone_key
      LEFT JOIN _members ml ON ml.id = c.loyalty_customer_id AND ml.org_id = c.org_id
                           AND ml.deleted_at IS NULL
     WHERE c.merged_into IS NULL AND c.erased_at IS NULL
       AND COALESCE(mp.id, ml.id) IS NOT NULL;
    SELECT count(*) INTO n_folded FROM _fold;

    -- For review: same person, two spellings. The customer's name is kept
    -- (staff typed it on purpose); the member's is reported.
    SELECT count(*) INTO n_conflict
      FROM _fold f JOIN customers c ON c.id = f.customer_id JOIN _members m ON m.id = f.member_id
     WHERE lower(btrim(c.name)) <> lower(btrim(m.name));

    -- (a)+(b) A customers row under THE MEMBER'S id. Inserted without its
    -- phone key first: the customer being folded in still holds that key, and
    -- it cannot be merged into a row that does not exist yet.
    INSERT INTO customers (id, org_id, name, phone, phone_key, notes, source,
                           created_by, created_branch_id, created_at, first_seen_at,
                           updated_at, erased_at)
    SELECT m.id, m.org_id,
           CASE WHEN m.deleted_at IS NOT NULL THEN '' ELSE COALESCE(k.name, m.name) END,
           CASE WHEN m.deleted_at IS NOT NULL THEN NULL
                WHEN m.phone_key IS NOT NULL AND k.phone_key = m.phone_key THEN k.phone
                ELSE m.phone END,
           NULL,
           k.notes,
           COALESCE(k.source, 'loyalty'),
           k.created_by,
           COALESCE(k.created_branch_id, m.joined_branch_id),
           LEAST(m.enrolled_at, COALESCE(k.created_at, m.enrolled_at)),
           LEAST(m.enrolled_at, COALESCE(k.first_seen_at, m.enrolled_at)),
           now(),
           -- A forgotten member is an erased customer: the row exists only so
           -- the ledger's member still has an identity to hang from.
           m.deleted_at
      FROM _members m
      LEFT JOIN LATERAL (
            -- The details that carry over: the phone match first, then oldest.
            SELECT c.name, c.phone, c.phone_key, c.source, c.created_by, c.created_branch_id,
                   c.created_at, c.first_seen_at,
                   (SELECT string_agg(c2.notes, E'\n' ORDER BY f2.by_phone DESC, c2.created_at)
                      FROM _fold f2 JOIN customers c2 ON c2.id = f2.customer_id
                     WHERE f2.member_id = m.id AND c2.notes IS NOT NULL) AS notes
              FROM _fold f JOIN customers c ON c.id = f.customer_id
             WHERE f.member_id = m.id
             ORDER BY f.by_phone DESC, c.created_at, c.id
             LIMIT 1) k ON true;
    GET DIAGNOSTICS n_new = ROW_COUNT;

    -- A folded customer whose own number differs from the member's: the
    -- member's number (the one the pass and the OTP know) wins, the other is
    -- remembered.
    INSERT INTO customer_phone_history (org_id, customer_id, phone, phone_key, reason)
    SELECT c.org_id, f.member_id, c.phone, c.phone_key, 'backfill'
      FROM _fold f JOIN customers c ON c.id = f.customer_id JOIN _members m ON m.id = f.member_id
     WHERE c.phone IS NOT NULL AND c.phone_key IS DISTINCT FROM m.phone_key;

    -- (b) The old rows step aside through the merge chain.
    UPDATE customers c
       SET merged_into = f.member_id, merged_at = now(), updated_at = now()
      FROM _fold f
     WHERE c.id = f.customer_id;
    UPDATE customers c
       SET merged_into = f.member_id
      FROM _fold f
     WHERE c.merged_into = f.customer_id AND c.id <> f.member_id;
    UPDATE orders o
       SET customer_id = f.member_id
      FROM _fold f
     WHERE o.customer_id = f.customer_id;

    -- Now the key is free. A member whose number a NON-folded live customer
    -- somehow still holds cannot happen (that customer would have folded), so
    -- a violation here is a real inconsistency and should stop the migration.
    UPDATE customers c
       SET phone_key = m.phone_key
      FROM _members m
     WHERE c.id = m.id AND m.phone_key IS NOT NULL;

    RAISE NOTICE 'customers shared key: % members; % customers rows created, % existing customers folded into members, % name conflicts kept the customer spelling',
        n_members, n_new, n_folded, n_conflict;
END $$;

-- ── (c) The key, and the end of the link column ─────────────────────────────
DO $$
DECLARE
    bad bigint;
BEGIN
    SELECT count(*) INTO bad FROM loyalty_customers m
     WHERE NOT EXISTS (SELECT 1 FROM customers c WHERE c.id = m.id AND c.org_id = m.org_id);
    IF bad > 0 THEN
        RAISE EXCEPTION 'customers shared key: % members have no customer of their own tenant', bad;
    END IF;
    SELECT count(*) INTO bad FROM loyalty_customers m JOIN customers c ON c.id = m.id
     WHERE m.deleted_at IS NULL AND (c.merged_into IS NOT NULL OR c.erased_at IS NOT NULL);
    IF bad > 0 THEN
        RAISE EXCEPTION 'customers shared key: % live members sit on a merged or erased customer', bad;
    END IF;
    SELECT count(*) INTO bad FROM (
        SELECT 1 FROM customers
         WHERE merged_into IS NULL AND erased_at IS NULL AND phone_key IS NOT NULL
         GROUP BY org_id, phone_key HAVING count(*) > 1) x;
    IF bad > 0 THEN
        RAISE EXCEPTION 'customers shared key: % phones have more than one live customer', bad;
    END IF;
    SELECT count(*) INTO bad FROM orders o JOIN customers c ON c.id = o.customer_id
     WHERE c.merged_into IS NOT NULL;
    IF bad > 0 THEN
        RAISE EXCEPTION 'customers shared key: % orders still point at a merged customer', bad;
    END IF;
END $$;

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'loyalty_customers_is_a_customer') THEN
        ALTER TABLE loyalty_customers
            ADD CONSTRAINT loyalty_customers_is_a_customer
            FOREIGN KEY (id) REFERENCES customers(id) ON DELETE RESTRICT;
    END IF;
END $$;

ALTER TABLE customers DROP COLUMN IF EXISTS loyalty_customer_id;

-- ── Feed: joining or leaving the programme re-sends the customer ────────────
-- The `customer` payload says whether the person is a member. Only the two
-- events that change that answer emit; a balance movement (every sale) must
-- not fan a customer out to every till.
CREATE OR REPLACE FUNCTION sync_emit_loyalty_customers() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    r    record;
    live boolean;
BEGIN
    IF TG_OP = 'DELETE' THEN r := OLD; ELSE r := NEW; END IF;
    SELECT sync_live_customer(c) INTO live FROM customers c WHERE c.id = r.id;
    IF live IS NOT NULL THEN
        PERFORM sync_emit_org(r.org_id, 'customer', r.id, sync_op(live));
    END IF;
    RETURN NULL;
END $$;
DROP TRIGGER IF EXISTS sync_emit ON loyalty_customers;
CREATE TRIGGER sync_emit AFTER INSERT OR DELETE OR UPDATE OF deleted_at ON loyalty_customers
    FOR EACH ROW EXECUTE FUNCTION sync_emit_loyalty_customers();

CREATE OR REPLACE FUNCTION sync_source_tables() RETURNS TABLE (source_table text, types text[])
    LANGUAGE sql IMMUTABLE
    AS $$
    VALUES
        ('categories',                 ARRAY['category']),
        ('menu_items',                 ARRAY['menu_item']),
        ('menu_item_sizes',            ARRAY['menu_item']),
        ('menu_item_modifier_groups',  ARRAY['menu_item']),
        ('modifier_groups',            ARRAY['menu_item','addon_item']),
        ('modifier_options',           ARRAY['menu_item','addon_item']),
        ('recipe_lines',               ARRAY['menu_item','addon_item']),
        ('menu_price_overrides',       ARRAY['menu_item','addon_item']),
        ('menu_item_recipe_steps',     ARRAY['menu_item']),
        ('recipe_step_presets',        ARRAY['menu_item']),
        ('menu_item_station_routes',   ARRAY['menu_item']),
        ('category_station_routes',    ARRAY['menu_item']),
        ('bundles',                    ARRAY['bundle']),
        ('bundle_components',          ARRAY['bundle']),
        ('bundle_branch_availability', ARRAY['bundle']),
        ('org_ingredients',            ARRAY['ingredient','addon_item']),
        ('org_payment_methods',        ARRAY['payment_method']),
        ('branch_payment_methods',     ARRAY['payment_availability']),
        ('user_payment_methods',       ARRAY['payment_availability']),
        ('device_payment_methods',     ARRAY['payment_availability']),
        ('discounts',                  ARRAY['discount']),
        ('branches',                   ARRAY['branch_settings']),
        ('kitchen_stations',           ARRAY['branch_settings']),
        ('devices',                    ARRAY['device']),
        ('users',                      ARRAY['teller']),
        ('user_branch_assignments',    ARRAY['teller']),
        ('permissions',                ARRAY['teller']),
        ('floor_sections',             ARRAY['floor_section']),
        ('branch_tables',              ARRAY['floor_table']),
        ('table_occupancies',          ARRAY['table_occupancy','floor_table']),
        ('booking_tables',             ARRAY['booking','floor_table']),
        ('bookings',                   ARRAY['booking','floor_table']),
        ('table_transfer_requests',    ARRAY['table_transfer']),
        ('open_tickets',               ARRAY['open_ticket']),
        ('open_ticket_items',          ARRAY['open_ticket']),
        ('open_ticket_rounds',         ARRAY['open_ticket']),
        ('kitchen_tickets',            ARRAY['kitchen_ticket']),
        ('kitchen_ticket_items',       ARRAY['kitchen_ticket']),
        ('delivery_orders',            ARRAY['delivery']),
        ('tills',                      ARRAY['till']),
        ('till_reconciliations',       ARRAY['till']),
        ('till_cash_movements',        ARRAY['cash_movement','till']),
        ('orders',                     ARRAY['order']),
        ('order_items',                ARRAY['order']),
        ('order_payments',             ARRAY['order']),
        ('order_refunds',              ARRAY['refund']),
        ('order_refund_lines',         ARRAY['refund']),
        ('addon_items',                ARRAY['addon_item']),
        ('addon_item_ingredients',     ARRAY['addon_item']),
        ('branch_addon_overrides',     ARRAY['addon_item']),
        ('branch_delivery_settings',   ARRAY['branch_settings']),
        ('role_permissions',           ARRAY['teller']),
        ('loyalty_settings',           ARRAY['branch_settings']),
        ('organizations',              ARRAY['branch_settings']),
        ('role_assignments',           ARRAY['teller']),
        ('role_assignment_branches',   ARRAY['teller']),
        ('user_overrides',             ARRAY['teller']),
        ('org_role_grants',            ARRAY['teller']),
        ('org_capability_policy',      ARRAY['teller']),
        ('customers',                  ARRAY['customer']),
        ('till_spot_views',           ARRAY['till']),
        ('staff_pool_settings',        ARRAY['branch_settings']),
        ('staff_drinks',               ARRAY['staff_drink']),
        ('loyalty_customers',          ARRAY['customer'])
    $$;
