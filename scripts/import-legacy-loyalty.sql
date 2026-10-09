-- Legacy loyalty import: one transaction. Driven by import-legacy-loyalty.py,
-- which validates and dedupes the export, sets :org and :apply, and puts the
-- COPY of `import_rows` where the marker line below stands.
--
-- It writes exactly what the backend writes when a customer joins at the
-- counter QR (`loyalty::public::join`): the person through the shape of
-- `customers::resolve_or_create` (source 'loyalty'), the card through the shape
-- of `loyalty::model::enrol` (a fresh member token and Apple auth token), and
-- the carried balance as ONE ledger row of the kind an admin's
-- `POST /loyalty/adjust` writes (kind 'adjust', source 'manual', a note that
-- says why). The balance is never written by hand: the ledger trigger moves it.
--
-- Every statement below is free of personal data: names and phones arrive only
-- in the COPY stream, so a slow or failed statement that the server logs names
-- nobody.
--
-- Idempotence: each ledger row's note ends in `[legacy-loyalty:<old id>]`. An
-- old id that already has one is reported and never credited again.

\set ON_ERROR_STOP on
SET client_min_messages = warning;
BEGIN;
-- Never wait on a till: a row a sale is holding makes the import fail, not queue.
SET LOCAL lock_timeout = '5s';
SET LOCAL statement_timeout = '120s';

-- ── The only organisation this import may touch ─────────────────────────────
SELECT set_config('app.org_id', :'org', true) AS rls_org \gset
DO $$
BEGIN
    IF current_setting('app.org_id')::uuid <> '27b8f8db-fec2-4909-b9f6-9fffbd860a1a'::uuid THEN
        RAISE EXCEPTION 'legacy import: org % is not the Drops org; refusing', current_setting('app.org_id');
    END IF;
    IF NOT EXISTS (SELECT 1 FROM organizations
                    WHERE id = current_setting('app.org_id')::uuid AND deleted_at IS NULL) THEN
        RAISE EXCEPTION 'legacy import: organisation % not found', current_setting('app.org_id');
    END IF;
END $$;

-- The tenant pool's role (src/db.rs): row-level security now binds every read
-- and write below to app.org_id, exactly as it binds the API's own queries.
SET LOCAL ROLE madar_app;

CREATE TEMP TABLE import_rows (
    old_id           text PRIMARY KEY,
    grp              integer NOT NULL,      -- one per person (canonical phone)
    primary_in_group boolean NOT NULL,      -- whose name/phone the person takes
    phone_raw        text NOT NULL,
    phone_key        text NOT NULL,
    name             text NOT NULL,
    stamps           integer NOT NULL,
    visits           integer NOT NULL,
    old_created_at   timestamptz NOT NULL,
    last_visit       timestamptz,
    card_status      text,
    program          text NOT NULL
) ON COMMIT DROP;

-- @@COPY_IMPORT_ROWS@@

-- Mirrors `loyalty::mint_member_token` / `madar_ids::member::member_token`:
-- `M` + the 16 bytes of a v4 UUID in unpadded base64url (23 characters).
CREATE FUNCTION pg_temp.mint_member_token() RETURNS text LANGUAGE sql VOLATILE AS $$
    SELECT 'M' || rtrim(translate(encode(uuid_send(gen_random_uuid()), 'base64'), '+/', '-_'), '=')
$$;

-- ── The programme the balances go into ──────────────────────────────────────
CREATE TEMP TABLE import_ctx ON COMMIT DROP AS
SELECT o.name AS org_name, s.enabled, s.mode, s.program_name, s.default_reward_cost,
       s.stamp_per_line_item, s.balance_cap_enabled, s.balance_cap, s.max_rewards_per_order,
       s.reward_any_item,
       (SELECT min(r.cost_amount) FROM loyalty_reward_items r
         WHERE r.org_id = s.org_id AND r.branch_id IS NULL) AS cheapest_reward,
       (SELECT max(r.cost_amount) FROM loyalty_reward_items r
         WHERE r.org_id = s.org_id AND r.branch_id IS NULL) AS dearest_reward,
       (SELECT count(*) FROM loyalty_settings b
         WHERE b.org_id = s.org_id AND b.branch_id IS NOT NULL) AS branch_overrides,
       -- A ledger row names a branch; the same pick as `model::merge_memberships`.
       (SELECT b.id FROM branches b WHERE b.org_id = s.org_id AND b.deleted_at IS NULL
         ORDER BY b.created_at, b.id LIMIT 1) AS ledger_branch,
       -- Where they joined: the shop's only branch when it has one, else unknown
       -- (NULL, as the join path stores an org-wide code).
       (SELECT CASE WHEN count(*) = 1 THEN (array_agg(b.id))[1] END FROM branches b
         WHERE b.org_id = s.org_id AND b.deleted_at IS NULL) AS joined_branch,
       (SELECT count(*) FROM loyalty_customers m
         WHERE m.org_id = s.org_id AND m.deleted_at IS NULL) AS members_before,
       (SELECT COALESCE(sum(m.visits_balance), 0) FROM loyalty_customers m
         WHERE m.org_id = s.org_id AND m.deleted_at IS NULL) AS stamps_before,
       (SELECT count(*) FROM customers c WHERE c.org_id = s.org_id
           AND c.merged_into IS NULL AND c.erased_at IS NULL) AS customers_before
  FROM loyalty_settings s
  JOIN organizations o ON o.id = s.org_id
 WHERE s.org_id = current_setting('app.org_id')::uuid AND s.branch_id IS NULL;

DO $$
DECLARE
    c   record;
    bad bigint;
BEGIN
    SELECT * INTO c FROM import_ctx;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'legacy import: this org has no loyalty programme (no loyalty_settings row); set it up before importing';
    END IF;
    IF NOT c.enabled THEN
        RAISE EXCEPTION 'legacy import: the loyalty programme is switched off';
    END IF;
    IF c.mode <> 'visits' THEN
        RAISE EXCEPTION 'legacy import: the programme collects %, not stamps; a stamp balance has no meaning in it', c.mode;
    END IF;
    IF c.ledger_branch IS NULL THEN
        RAISE EXCEPTION 'legacy import: the org has no live branch to record the ledger rows at';
    END IF;
    -- The rule the database itself applies must agree with the script's, row by row.
    SELECT count(*) INTO bad FROM import_rows
     WHERE phone_canonical(phone_raw) IS DISTINCT FROM phone_key
        OR phone_key !~ '^20(10|11|12|15)[0-9]{8}$';
    IF bad > 0 THEN
        RAISE EXCEPTION 'legacy import: % rows carry a phone key the database does not agree with', bad;
    END IF;
    SELECT count(*) INTO bad FROM import_rows WHERE stamps < 0 OR visits < 0;
    IF bad > 0 THEN
        RAISE EXCEPTION 'legacy import: % rows have a negative balance', bad;
    END IF;
    SELECT count(*) INTO bad FROM (SELECT 1 FROM import_rows GROUP BY grp HAVING count(DISTINCT phone_key) > 1
                                   UNION ALL
                                   SELECT 1 FROM import_rows GROUP BY phone_key HAVING count(DISTINCT grp) > 1) x;
    IF bad > 0 THEN
        RAISE EXCEPTION 'legacy import: a person group does not match one phone';
    END IF;
END $$;

-- ── Already imported? The ledger marker, whatever has happened since ────────
ALTER TABLE import_rows
    ADD COLUMN done_txn      uuid,
    ADD COLUMN done_customer uuid,
    ADD COLUMN done_points   integer,
    ADD COLUMN done_at       timestamptz;
UPDATE import_rows r
   SET done_txn = t.id,
       done_customer = COALESCE(customers_resolve(t.org_id, t.customer_id), t.customer_id),
       done_points = t.points,
       done_at = t.created_at
  FROM loyalty_transactions t
 WHERE t.org_id = current_setting('app.org_id')::uuid
   AND t.kind = 'adjust'
   AND t.note IS NOT NULL
   AND position('[legacy-loyalty:' || r.old_id || ']' IN t.note) > 0;

-- ── One person per phone ────────────────────────────────────────────────────
CREATE TEMP TABLE import_people ON COMMIT DROP AS
SELECT DISTINCT ON (r.grp)
       r.grp, r.phone_key, r.phone_raw, r.name,
       (SELECT min(x.old_created_at) FROM import_rows x WHERE x.grp = r.grp) AS first_seen,
       NULL::uuid        AS customer_id,
       false             AS existed,
       NULL::text        AS existing_name,
       false             AS name_filled,
       false             AS first_seen_moved,
       'none'::text      AS member_state,   -- none | live | ended
       false             AS enrolled_now,
       0                 AS balance_before,
       0                 AS credited,
       0                 AS balance_after,
       false             AS pass_marked_stale
  FROM import_rows r
 ORDER BY r.grp, r.primary_in_group DESC, r.old_id;

-- The live customer holding the phone, as `customers::live_with_phone` finds it.
UPDATE import_people p
   SET customer_id = c.id, existed = true, existing_name = c.name
  FROM customers c
 WHERE c.org_id = current_setting('app.org_id')::uuid
   AND c.phone_key = p.phone_key
   AND c.merged_into IS NULL AND c.erased_at IS NULL;

UPDATE import_people p
   SET member_state = CASE WHEN m.deleted_at IS NULL THEN 'live' ELSE 'ended' END,
       balance_before = CASE WHEN m.deleted_at IS NULL THEN m.visits_balance ELSE 0 END
  FROM loyalty_customers m
 WHERE m.id = p.customer_id;

-- ── New people: the row `resolve_or_create` writes (source 'loyalty') ───────
-- Dated from the old programme: the shop first saw them when they joined it.
UPDATE import_people SET customer_id = gen_random_uuid() WHERE NOT existed;
INSERT INTO customers (id, org_id, name, phone, phone_key, notes, source,
                       created_by, created_branch_id, created_at, first_seen_at)
SELECT p.customer_id, current_setting('app.org_id')::uuid, p.name, p.phone_raw, p.phone_key,
       NULL, 'loyalty', NULL, x.joined_branch, p.first_seen, p.first_seen
  FROM import_people p CROSS JOIN import_ctx x
 WHERE NOT p.existed;

-- A matched person keeps the name on file; only an empty one is filled.
WITH filled AS (
    UPDATE customers c SET name = p.name, updated_at = now()
      FROM import_people p
     WHERE p.existed AND c.id = p.customer_id AND btrim(c.name) = ''
    RETURNING c.id)
UPDATE import_people p SET name_filled = true FROM filled f WHERE f.id = p.customer_id;

-- …and was first seen when the old programme first saw them, if that is earlier.
WITH moved AS (
    UPDATE customers c SET first_seen_at = p.first_seen
      FROM import_people p
     WHERE p.existed AND c.id = p.customer_id AND c.first_seen_at > p.first_seen
    RETURNING c.id)
UPDATE import_people p SET first_seen_moved = true FROM moved m WHERE m.id = p.customer_id;

-- ── The card: `loyalty::model::enrol` ───────────────────────────────────────
-- A person whose membership was ENDED (they left the programme) is not signed
-- back up by an import; they are reported for the owner to decide.
WITH enrolled AS (
    INSERT INTO loyalty_customers (id, org_id, member_token, joined_branch_id,
                                   apple_auth_token, enrolled_at)
    SELECT p.customer_id, current_setting('app.org_id')::uuid, pg_temp.mint_member_token(),
           x.joined_branch, pg_temp.mint_member_token(), p.first_seen
      FROM import_people p CROSS JOIN import_ctx x
     WHERE p.member_state = 'none'
    RETURNING id)
UPDATE import_people p SET enrolled_now = true, member_state = 'live'
  FROM enrolled e WHERE e.id = p.customer_id;

-- ── The carried balance: one ledger row per old card ────────────────────────
-- The shape of `POST /loyalty/adjust` (kind 'adjust', source 'manual', a note),
-- in the programme's currency. No actor: the system wrote it.
INSERT INTO loyalty_transactions (org_id, customer_id, branch_id, kind, currency, points,
                                  note, created_by, source)
SELECT current_setting('app.org_id')::uuid, p.customer_id,
       COALESCE(m.joined_branch_id, x.ledger_branch), 'adjust', 'visits', r.stamps,
       format('Carried over from the old loyalty programme (%s): %s stamp%s. [legacy-loyalty:%s]',
              r.program, r.stamps, CASE WHEN r.stamps = 1 THEN '' ELSE 's' END, r.old_id),
       NULL, 'manual'
  FROM import_rows r
  JOIN import_people p ON p.grp = r.grp
  JOIN loyalty_customers m ON m.id = p.customer_id AND m.deleted_at IS NULL
 CROSS JOIN import_ctx x
 WHERE r.done_txn IS NULL AND r.stamps > 0
 ORDER BY p.grp, r.old_id;

UPDATE import_people p
   SET credited = COALESCE((SELECT sum(r.stamps) FROM import_rows r
                             WHERE r.grp = p.grp AND r.done_txn IS NULL AND r.stamps > 0
                               AND p.member_state = 'live'), 0),
       balance_after = COALESCE((SELECT m.visits_balance FROM loyalty_customers m
                                  WHERE m.id = p.customer_id AND m.deleted_at IS NULL), 0);

-- A card already in someone's wallet shows the old balance until it is pushed.
-- The backend's refresh sweep (`wallet::refresh::stale_passes`) pushes any card
-- whose `pass_updated_at` is NULL, through the same `refresh_pass` a sale uses.
WITH stale AS (
    UPDATE loyalty_customers m SET pass_updated_at = NULL
      FROM import_people p
     WHERE m.id = p.customer_id AND p.credited > 0 AND NOT p.enrolled_now
       AND (m.google_object_id IS NOT NULL
            OR EXISTS (SELECT 1 FROM loyalty_pass_devices d WHERE d.customer_id = m.id))
    RETURNING m.id)
UPDATE import_people p SET pass_marked_stale = true FROM stale s WHERE s.id = p.customer_id;
DELETE FROM loyalty_pass_cache c USING import_people p
 WHERE c.customer_id = p.customer_id AND p.credited > 0;

-- ── Report (CSV sections, read by the script) ───────────────────────────────
\pset format csv
\pset footer off
\echo @@PROGRAM
SELECT x.org_name, x.program_name, x.mode, x.enabled, x.default_reward_cost, x.cheapest_reward,
       x.dearest_reward, x.reward_any_item, x.stamp_per_line_item, x.balance_cap_enabled,
       x.balance_cap, x.max_rewards_per_order, x.branch_overrides,
       (SELECT b.name FROM branches b WHERE b.id = x.ledger_branch) AS ledger_branch_name,
       x.members_before, x.stamps_before, x.customers_before
  FROM import_ctx x;

\echo @@ROWS
SELECT r.old_id, r.grp, p.customer_id, p.existed, p.existing_name, p.name AS person_name,
       p.name_filled, p.first_seen_moved, p.member_state, p.enrolled_now,
       p.balance_before, p.credited, p.balance_after, p.pass_marked_stale,
       CASE WHEN r.done_txn IS NOT NULL THEN 'already_imported'
            WHEN p.member_state <> 'live' THEN 'membership_ended'
            WHEN r.stamps = 0 THEN 'no_balance'
            ELSE 'credited' END AS credit_outcome,
       CASE WHEN r.done_txn IS NULL AND r.stamps > 0 AND p.member_state = 'live'
            THEN r.stamps ELSE 0 END AS stamps_credited,
       r.done_points, r.done_at, (r.done_customer IS DISTINCT FROM p.customer_id
                                  AND r.done_customer IS NOT NULL) AS done_elsewhere
  FROM import_rows r JOIN import_people p ON p.grp = r.grp
 ORDER BY r.grp, r.old_id;

\echo @@CHECKS
SELECT
    (SELECT count(*) FROM loyalty_customers m
      WHERE m.org_id = current_setting('app.org_id')::uuid AND m.deleted_at IS NULL) AS members_after,
    (SELECT COALESCE(sum(m.visits_balance), 0) FROM loyalty_customers m
      WHERE m.org_id = current_setting('app.org_id')::uuid AND m.deleted_at IS NULL) AS stamps_after,
    (SELECT count(*) FROM customers c WHERE c.org_id = current_setting('app.org_id')::uuid
        AND c.merged_into IS NULL AND c.erased_at IS NULL) AS customers_after,
    -- Balances reconcile with the ledger, for every member of the org.
    (SELECT count(*) FROM loyalty_customers m
      WHERE m.org_id = current_setting('app.org_id')::uuid
        AND (m.visits_balance <> COALESCE((SELECT sum(t.points) FROM loyalty_transactions t
                                             WHERE t.customer_id = m.id AND t.currency = 'visits'), 0)
          OR m.points_balance <> COALESCE((SELECT sum(t.points) FROM loyalty_transactions t
                                             WHERE t.customer_id = m.id AND t.currency = 'points'), 0))
    ) AS ledger_mismatches,
    (SELECT count(*) FROM loyalty_transactions t
      WHERE t.org_id = current_setting('app.org_id')::uuid AND t.note LIKE '%[legacy-loyalty:%]%') AS marker_rows,
    -- Every person of this import is one live customer with one live card.
    (SELECT count(*) FROM import_people p
      WHERE p.member_state = 'live' AND NOT EXISTS (
            SELECT 1 FROM loyalty_members_v v WHERE v.id = p.customer_id AND v.deleted_at IS NULL
               AND v.phone = p.phone_key)) AS people_not_visible;

\pset format aligned
\if :apply
COMMIT;
\echo @@COMMITTED
\else
ROLLBACK;
\echo @@ROLLED_BACK
\endif
