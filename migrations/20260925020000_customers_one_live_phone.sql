-- One live customer per phone per tenant, enforced by the database
-- (CUSTOMERS_UNIFICATION_DESIGN.md §2.3, step 2).
--
-- The application used to look for a holder of the phone and then insert: two
-- tills adding the same number at the same moment both saw nobody and both
-- inserted. Now that the key is canonical the rule can be an index.
--
-- Existing duplicates (the same person keyed `010…` on one till and `+2010…`
-- on another, which the old key could not see as equal) are first folded
-- together through the merge chain that already exists, so every id a till or
-- an order holds still resolves. The survivor is the row with the most
-- orders, then the oldest.

DO $$
DECLARE
    folded bigint;
BEGIN
    CREATE TEMP TABLE _customers_dedup ON COMMIT DROP AS
    WITH live AS (
        SELECT c.id, c.org_id, c.phone_key, c.created_at,
               (SELECT count(*) FROM orders o WHERE o.customer_id = c.id) AS n
          FROM customers c
         WHERE c.merged_into IS NULL AND c.erased_at IS NULL AND c.phone_key IS NOT NULL
    ), ranked AS (
        SELECT l.id, l.org_id,
               first_value(l.id) OVER w AS survivor,
               row_number()      OVER w AS rn
          FROM live l
        WINDOW w AS (PARTITION BY l.org_id, l.phone_key ORDER BY l.n DESC, l.created_at, l.id)
    )
    SELECT id AS loser, survivor, org_id FROM ranked WHERE rn > 1;

    -- The survivor takes what it lacks: the notes, and the loyalty link the
    -- next migration turns into the shared key.
    UPDATE customers s
       SET notes = COALESCE(s.notes, x.notes),
           loyalty_customer_id = COALESCE(s.loyalty_customer_id, x.loyalty_customer_id),
           updated_at = now()
      FROM (SELECT d.survivor,
                   (array_agg(c.notes ORDER BY c.created_at)
                        FILTER (WHERE c.notes IS NOT NULL))[1] AS notes,
                   (array_agg(c.loyalty_customer_id ORDER BY c.created_at)
                        FILTER (WHERE c.loyalty_customer_id IS NOT NULL))[1] AS loyalty_customer_id
              FROM _customers_dedup d JOIN customers c ON c.id = d.loser
             GROUP BY d.survivor) x
     WHERE s.id = x.survivor
       AND (s.notes IS NULL OR s.loyalty_customer_id IS NULL);

    UPDATE customers c
       SET merged_into = d.survivor, merged_at = now(), updated_at = now()
      FROM _customers_dedup d
     WHERE c.id = d.loser;
    GET DIAGNOSTICS folded = ROW_COUNT;

    -- Earlier merges into a loser follow it, so the chain stays one hop deep.
    UPDATE customers c
       SET merged_into = d.survivor
      FROM _customers_dedup d
     WHERE c.merged_into = d.loser AND c.id <> d.survivor;

    UPDATE orders o
       SET customer_id = d.survivor
      FROM _customers_dedup d
     WHERE o.customer_id = d.loser;

    RAISE NOTICE 'customers dedup: % duplicate customers folded into their survivors', folded;
END $$;

-- ── Verification, then the rule ─────────────────────────────────────────────
DO $$
DECLARE
    bad bigint;
BEGIN
    SELECT count(*) INTO bad FROM (
        SELECT 1 FROM customers
         WHERE merged_into IS NULL AND erased_at IS NULL AND phone_key IS NOT NULL
         GROUP BY org_id, phone_key HAVING count(*) > 1) x;
    IF bad > 0 THEN
        RAISE EXCEPTION 'customers dedup: % phones still have more than one live customer', bad;
    END IF;
    SELECT count(*) INTO bad FROM customers c
      JOIN customers t ON t.id = c.merged_into
     WHERE t.merged_into IS NOT NULL AND t.erased_at IS NULL AND c.merged_at >= now();
    IF bad > 0 THEN
        RAISE EXCEPTION 'customers dedup: % rows were merged into a row that is itself merged', bad;
    END IF;
END $$;

CREATE UNIQUE INDEX IF NOT EXISTS customers_org_phone_live_key
    ON customers (org_id, phone_key)
    WHERE merged_into IS NULL AND erased_at IS NULL AND phone_key IS NOT NULL;
-- The plain index it replaces covered exactly the same rows.
DROP INDEX IF EXISTS customers_org_phone;
