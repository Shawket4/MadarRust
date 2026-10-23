-- Rewards dashboard: stamp tracks — one card, up to four rewards
-- (REWARDS_DASHBOARD_DESIGN.md §2; POS_MULTITRACK_DESIGN.md D1–D9, owner-confirmed 2026-10-23).
--
-- Track 1 is the programme's own card and stays EXACTLY where it is:
-- `loyalty_memberships.visits_balance` / `points_balance`, the programme's card
-- size, earning list and catalogue. Every released till, the teller page and
-- the passes keep reading those. Tracks 2–4 are new, stamps only:
--   * `loyalty_stamp_tracks` — the definition (track 1's row holds only its
--     name, shape and colour; its size is the programme's);
--   * `loyalty_track_items` — the menu items a POS sale earns a track on and a
--     completed track covers; none = a counter-only track (stand-alone shops);
--   * `loyalty_track_balances` — a member's balance per track 2–4, kept by the
--     ledger trigger like track 1's;
--   * `loyalty_transactions.track` (2–4; NULL = track 1, so every existing row
--     and writer is unchanged) and `order_items.reward_track`.
-- One earn per (order, track) replaces one earn per order: the single "Add
-- points" press earns every track the sale qualifies for.
--
-- SOURCE TABLES (additions; machine-read by the migration tests together with
-- the earlier changefeed headers). Format:  table -> type[, type…]  (scope)
--   loyalty_stamp_tracks       -> branch_settings             (org fan-out)

SET LOCAL lock_timeout = '15s';

-- ── 1. Definitions ──────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS loyalty_stamp_tracks (
    org_id     uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    position   smallint NOT NULL CHECK (position BETWEEN 1 AND 4),
    name_en    text NOT NULL DEFAULT '' CHECK (char_length(name_en) <= 24),
    name_ar    text NOT NULL DEFAULT '' CHECK (char_length(name_ar) <= 24),
    -- NULL on track 1 (its size is the programme's card size).
    size       integer NULL,
    shape      text NULL CHECK (shape IN ('circle','square','star','heart','cup','gift','check')),
    colour     text NULL CHECK (colour ~ '^#[0-9A-Fa-f]{6}$'),
    -- A stamp per unit of its items (true) or one per sale holding any.
    per_item   boolean NOT NULL DEFAULT true,
    -- The balance ceiling (a stamp past it is trimmed), at least the size.
    cap        integer NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (org_id, position),
    CONSTRAINT loyalty_stamp_tracks_size CHECK (
        (position = 1 AND size IS NULL AND cap IS NULL)
        OR (position > 1 AND size BETWEEN 1 AND 50 AND (cap IS NULL OR cap >= size)))
);
ALTER TABLE loyalty_stamp_tracks ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON loyalty_stamp_tracks;
CREATE POLICY tenant_isolation ON loyalty_stamp_tracks FOR ALL
    USING ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE)
    WITH CHECK ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE);
GRANT SELECT, INSERT, UPDATE, DELETE ON loyalty_stamp_tracks TO madar_app;

CREATE TABLE IF NOT EXISTS loyalty_track_items (
    org_id       uuid NOT NULL,
    position     smallint NOT NULL CHECK (position BETWEEN 2 AND 4),
    menu_item_id uuid NOT NULL REFERENCES menu_items(id) ON DELETE CASCADE,
    PRIMARY KEY (org_id, position, menu_item_id),
    FOREIGN KEY (org_id, position) REFERENCES loyalty_stamp_tracks(org_id, position) ON DELETE CASCADE
);
ALTER TABLE loyalty_track_items ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON loyalty_track_items;
CREATE POLICY tenant_isolation ON loyalty_track_items FOR ALL
    USING ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE)
    WITH CHECK ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE);
GRANT SELECT, INSERT, UPDATE, DELETE ON loyalty_track_items TO madar_app;

-- ── 2. Balances of tracks 2–4 ───────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS loyalty_track_balances (
    org_id      uuid NOT NULL,
    customer_id uuid NOT NULL REFERENCES loyalty_memberships(id) ON DELETE CASCADE,
    position    smallint NOT NULL CHECK (position BETWEEN 2 AND 4),
    balance     integer NOT NULL DEFAULT 0,
    lifetime    integer NOT NULL DEFAULT 0,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (customer_id, position)
);
CREATE INDEX IF NOT EXISTS loyalty_track_balances_org ON loyalty_track_balances (org_id, position);
ALTER TABLE loyalty_track_balances ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON loyalty_track_balances;
CREATE POLICY tenant_isolation ON loyalty_track_balances FOR ALL
    USING ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE)
    WITH CHECK ((org_id = (SELECT current_setting('app.org_id', true)::uuid)) IS TRUE);
GRANT SELECT, INSERT, UPDATE, DELETE ON loyalty_track_balances TO madar_app;

-- ── 3. The ledger and the order line name their track ───────────────────────
ALTER TABLE loyalty_transactions
    ADD COLUMN IF NOT EXISTS track smallint NULL;
ALTER TABLE loyalty_transactions
    DROP CONSTRAINT IF EXISTS loyalty_txn_track,
    ADD CONSTRAINT loyalty_txn_track
        CHECK (track IS NULL OR (track BETWEEN 2 AND 4 AND currency = 'visits')) NOT VALID;
ALTER TABLE loyalty_transactions VALIDATE CONSTRAINT loyalty_txn_track;

ALTER TABLE order_items
    ADD COLUMN IF NOT EXISTS reward_track smallint NULL;
ALTER TABLE order_items
    DROP CONSTRAINT IF EXISTS order_items_reward_track,
    ADD CONSTRAINT order_items_reward_track
        CHECK (reward_track IS NULL OR reward_track BETWEEN 2 AND 4) NOT VALID;

-- One earn per (order, track). Built before the old one-per-order key goes, so
-- there is never a moment with neither.
CREATE UNIQUE INDEX IF NOT EXISTS loyalty_transactions_earn_order_track_key
    ON loyalty_transactions (order_id, COALESCE(track, 1))
    WHERE kind = 'earn' AND order_id IS NOT NULL;
DROP INDEX IF EXISTS loyalty_transactions_earn_order_key;

-- ── 4. The balance trigger: a row with a track moves that track ─────────────
-- Without a track the body is 20260912060000's, byte for byte below the IF.
CREATE OR REPLACE FUNCTION loyalty_apply_txn() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    lifetime_delta integer;
    new_balance    integer;
BEGIN
    lifetime_delta := CASE NEW.kind
        WHEN 'earn'           THEN NEW.points
        WHEN 'reverse_earn'   THEN NEW.points               -- negative
        WHEN 'adjust'         THEN GREATEST(NEW.points, 0)  -- a gift counts, a deduction does not
        WHEN 'reverse_adjust' THEN LEAST(NEW.points, 0)     -- undoing a gift; undoing a deduction is not earning
        ELSE 0                                              -- redeem / reverse_redeem: spending, not earning
    END;

    IF NEW.track IS NOT NULL THEN
        IF NOT EXISTS (SELECT 1 FROM loyalty_memberships WHERE id = NEW.customer_id) THEN
            RAISE EXCEPTION 'loyalty customer % not found', NEW.customer_id;
        END IF;
        INSERT INTO loyalty_track_balances (org_id, customer_id, position, balance, lifetime)
        VALUES (NEW.org_id, NEW.customer_id, NEW.track, NEW.points, lifetime_delta)
        ON CONFLICT (customer_id, position) DO UPDATE
           SET balance    = loyalty_track_balances.balance + EXCLUDED.balance,
               lifetime   = loyalty_track_balances.lifetime + EXCLUDED.lifetime,
               updated_at = now()
        RETURNING balance INTO new_balance;
        UPDATE loyalty_memberships SET updated_at = now() WHERE id = NEW.customer_id;
    ELSE
        UPDATE loyalty_customers
           SET points_balance  = points_balance
                 + CASE WHEN NEW.currency = 'points' THEN NEW.points ELSE 0 END,
               visits_balance  = visits_balance
                 + CASE WHEN NEW.currency = 'visits' THEN NEW.points ELSE 0 END,
               lifetime_points = lifetime_points
                 + CASE WHEN NEW.currency = 'points' THEN lifetime_delta ELSE 0 END,
               lifetime_visits = lifetime_visits
                 + CASE WHEN NEW.currency = 'visits' THEN lifetime_delta ELSE 0 END,
               updated_at      = now()
         WHERE id = NEW.customer_id
         RETURNING CASE WHEN NEW.currency = 'points' THEN points_balance ELSE visits_balance END
              INTO new_balance;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'loyalty customer % not found', NEW.customer_id;
        END IF;
    END IF;

    -- The floor applies to the movement that crosses it, never to one that
    -- climbs back towards zero: a member already in debt must still be able
    -- to receive a gift or a reversed redemption.
    IF new_balance < 0 AND NEW.points < 0 THEN
        IF NEW.kind NOT IN ('reverse_earn', 'reverse_adjust') THEN
            RAISE EXCEPTION 'loyalty: % of % % would take member % to %; a balance cannot be spent below zero',
                NEW.kind, NEW.points, NEW.currency, NEW.customer_id, new_balance
                USING ERRCODE = 'check_violation';
        ELSIF NOT loyalty_allows_negative_balance(NEW.org_id, NEW.branch_id) THEN
            RAISE EXCEPTION 'loyalty: clawing back % % would take member % to %, and this programme clamps at zero (loyalty_settings.allow_negative_balance)',
                NEW.points, NEW.currency, NEW.customer_id, new_balance
                USING ERRCODE = 'check_violation';
        END IF;
    END IF;
    RETURN NEW;
END $$;

-- ── 5. A reversal keeps its original's track ────────────────────────────────
-- The contract trigger: the 20260912060000 body, plus "same track" beside
-- "same member, tenant and currency".
CREATE OR REPLACE FUNCTION loyalty_txn_before_insert() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    orig          loyalty_transactions%ROWTYPE;
    already       integer;
    order_status  text;
BEGIN
    IF NEW.source IS NULL THEN
        NEW.source := CASE NEW.kind
            WHEN 'earn'   THEN 'sale'
            WHEN 'redeem' THEN 'redemption'
            WHEN 'adjust' THEN 'manual'
        END;
    END IF;

    IF NEW.reverses_id IS NOT NULL THEN
        SELECT * INTO orig FROM loyalty_transactions WHERE id = NEW.reverses_id FOR UPDATE;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'loyalty: nothing to reverse — transaction % does not exist', NEW.reverses_id
                USING ERRCODE = 'foreign_key_violation';
        END IF;
        IF orig.reverses_id IS NOT NULL THEN
            RAISE EXCEPTION 'loyalty: % is itself a reversal and cannot be reversed; correct it with an adjustment', orig.id
                USING ERRCODE = 'check_violation';
        END IF;
        IF NEW.kind::text <> 'reverse_' || orig.kind::text THEN
            RAISE EXCEPTION 'loyalty: % cannot reverse % (transaction %)', NEW.kind, orig.kind, orig.id
                USING ERRCODE = 'check_violation';
        END IF;
        -- A reversal written without a track takes its original's.
        IF NEW.track IS NULL THEN
            NEW.track := orig.track;
        END IF;
        IF orig.customer_id <> NEW.customer_id OR orig.org_id <> NEW.org_id OR orig.currency <> NEW.currency
           OR orig.track IS DISTINCT FROM NEW.track THEN
            RAISE EXCEPTION 'loyalty: a reversal must move the same member, tenant, currency and track as transaction %', orig.id
                USING ERRCODE = 'check_violation';
        END IF;
        IF sign(NEW.points) <> -sign(orig.points) THEN
            RAISE EXCEPTION 'loyalty: reversing % (% points) needs the opposite sign, got %', orig.id, orig.points, NEW.points
                USING ERRCODE = 'check_violation';
        END IF;
        IF NEW.order_id IS NULL THEN
            NEW.order_id := orig.order_id;
        ELSIF NEW.order_id IS DISTINCT FROM orig.order_id THEN
            RAISE EXCEPTION 'loyalty: reversal of % names order % but the original was for order %',
                orig.id, NEW.order_id, orig.order_id
                USING ERRCODE = 'check_violation';
        END IF;
        SELECT COALESCE(SUM(abs(points)), 0) INTO already
          FROM loyalty_transactions WHERE reverses_id = NEW.reverses_id;
        IF already + abs(NEW.points) > abs(orig.points) THEN
            RAISE EXCEPTION 'loyalty: transaction % moved %; % already reversed, % more requested',
                orig.id, abs(orig.points), already, abs(NEW.points)
                USING ERRCODE = 'check_violation';
        END IF;
    END IF;

    IF NEW.source = 'void' THEN
        SELECT status::text INTO order_status FROM orders WHERE id = NEW.order_id;
        IF order_status IS DISTINCT FROM 'voided' THEN
            RAISE EXCEPTION 'loyalty: source is void but order % is %', NEW.order_id, COALESCE(order_status, 'missing')
                USING ERRCODE = 'check_violation';
        END IF;
    END IF;

    RETURN NEW;
END $$;

-- The clawback: 20261010150000's body; the reversal carries the track (the
-- contract trigger fills it), the "a reward was claimed since" rule looks at
-- the same track, and the clamp reads that track's balance.
CREATE OR REPLACE FUNCTION loyalty_reverse(
    p_txn    uuid,
    p_amount integer,
    p_source text,
    p_by     uuid,
    p_note   text
) RETURNS uuid LANGUAGE plpgsql AS $$
DECLARE
    orig      loyalty_transactions%ROWTYPE;
    already   integer;
    remaining integer;
    amount    integer;
    balance   integer;
    new_id    uuid;
BEGIN
    SELECT * INTO orig FROM loyalty_transactions WHERE id = p_txn FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'loyalty: transaction % not found', p_txn;
    END IF;
    IF p_amount IS NOT NULL AND p_amount <= 0 THEN
        RAISE EXCEPTION 'loyalty: a reversal amount is a positive magnitude, got %', p_amount;
    END IF;

    SELECT COALESCE(SUM(abs(points)), 0) INTO already
      FROM loyalty_transactions WHERE reverses_id = p_txn;
    remaining := abs(orig.points) - already;
    amount    := LEAST(COALESCE(p_amount, remaining), remaining);
    IF amount <= 0 THEN
        RETURN NULL;
    END IF;

    IF orig.points > 0 AND NOT loyalty_allows_negative_balance(orig.org_id, orig.branch_id) THEN
        IF orig.kind = 'earn' AND EXISTS (
            SELECT 1 FROM loyalty_transactions r
             WHERE r.customer_id = orig.customer_id
               AND r.currency = orig.currency
               AND r.track IS NOT DISTINCT FROM orig.track
               AND r.kind = 'redeem'
               AND r.created_at > orig.created_at
               AND r.order_id IS DISTINCT FROM orig.order_id
               AND abs(r.points) > (SELECT COALESCE(SUM(abs(x.points)), 0)
                                      FROM loyalty_transactions x WHERE x.reverses_id = r.id))
        THEN
            RETURN NULL;
        END IF;
        IF orig.track IS NOT NULL THEN
            SELECT b.balance INTO balance
              FROM loyalty_track_balances b
             WHERE b.customer_id = orig.customer_id AND b.position = orig.track FOR UPDATE;
        ELSE
            SELECT CASE WHEN orig.currency = 'points' THEN points_balance ELSE visits_balance END
              INTO balance
              FROM loyalty_customers WHERE id = orig.customer_id FOR UPDATE;
        END IF;
        amount := LEAST(amount, GREATEST(COALESCE(balance, 0), 0));
        IF amount <= 0 THEN
            RETURN NULL;
        END IF;
    END IF;

    INSERT INTO loyalty_transactions
        (org_id, customer_id, branch_id, kind, currency, points,
         order_id, reverses_id, source, created_by, note, track)
    VALUES
        (orig.org_id, orig.customer_id, orig.branch_id,
         ('reverse_' || orig.kind::text)::loyalty_txn_kind,
         orig.currency, -sign(orig.points) * amount,
         orig.order_id, orig.id, p_source, p_by, p_note, orig.track)
    RETURNING id INTO new_id;
    RETURN new_id;
END $$;

-- ── 6. The POS feed: the branch_settings row's loyalty object names tracks ──
CREATE OR REPLACE FUNCTION sync_emit_loyalty_stamp_tracks() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    PERFORM sync_touch_org_branch_settings(COALESCE(NEW.org_id, OLD.org_id), NULL);
    RETURN NULL;
END $$;
DROP TRIGGER IF EXISTS sync_emit ON loyalty_stamp_tracks;
CREATE TRIGGER sync_emit AFTER INSERT OR UPDATE OR DELETE ON loyalty_stamp_tracks
    FOR EACH ROW EXECUTE FUNCTION sync_emit_loyalty_stamp_tracks();

DO $tracks$
BEGIN
    IF pg_get_functiondef('sync_source_tables'::regproc) NOT LIKE '%(''loyalty_stamp_tracks'',%' THEN
        EXECUTE replace(pg_get_functiondef('sync_source_tables'::regproc),
                        '(''loyalty_settings'',           ARRAY[''branch_settings'']),',
                        '(''loyalty_settings'',           ARRAY[''branch_settings'']),
        (''loyalty_stamp_tracks'',       ARRAY[''branch_settings'']),');
    END IF;
END
$tracks$;
