-- Rewards dashboard: what the CRM, the segments and the figures read, indexed
-- (REWARDS_DASHBOARD_DESIGN.md §12, "Indexes and caches").
--
-- * `loyalty_memberships.last_seen_at`: when a member last earned or spent (a
--   stamp, a point, a reward), kept by the ledger trigger in the same UPDATE
--   that moves the balance — so "not seen for 30 days" is an index range, not
--   a max() over the ledger per member. Backfilled from the ledger; a member
--   with no visit counts from joining. NOT NULL, so the index serves it whole.
-- * The ledger trigger writes the base table rather than the compat view (the
--   view's column list predates `last_seen_at`); same row, same effect.
-- * `loyalty_track_balances (org_id, position, balance)`: balance and
--   reward-ready segments of tracks 2–4.
-- The indexes on the busy tables (memberships, ledger) are built CONCURRENTLY,
-- one per file (20261026131000..133000).

SET LOCAL lock_timeout = '15s';

ALTER TABLE loyalty_memberships ADD COLUMN IF NOT EXISTS last_seen_at timestamptz NULL;
UPDATE loyalty_memberships m
   SET last_seen_at = COALESCE(
         (SELECT max(t.created_at) FROM loyalty_transactions t
           WHERE t.customer_id = m.id AND t.kind IN ('earn', 'redeem')),
         m.enrolled_at)
 WHERE m.last_seen_at IS NULL;
ALTER TABLE loyalty_memberships ALTER COLUMN last_seen_at SET DEFAULT now();
ALTER TABLE loyalty_memberships ALTER COLUMN last_seen_at SET NOT NULL;

DROP INDEX IF EXISTS loyalty_track_balances_org;
CREATE INDEX IF NOT EXISTS loyalty_track_balances_org_balance
    ON loyalty_track_balances (org_id, position, balance);

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
        UPDATE loyalty_memberships
           SET updated_at = now(),
               last_seen_at = CASE WHEN NEW.kind IN ('earn', 'redeem')
                                   THEN GREATEST(last_seen_at, NEW.created_at) ELSE last_seen_at END
         WHERE id = NEW.customer_id;
    ELSE
        UPDATE loyalty_memberships
           SET points_balance  = points_balance
                 + CASE WHEN NEW.currency = 'points' THEN NEW.points ELSE 0 END,
               visits_balance  = visits_balance
                 + CASE WHEN NEW.currency = 'visits' THEN NEW.points ELSE 0 END,
               lifetime_points = lifetime_points
                 + CASE WHEN NEW.currency = 'points' THEN lifetime_delta ELSE 0 END,
               lifetime_visits = lifetime_visits
                 + CASE WHEN NEW.currency = 'visits' THEN lifetime_delta ELSE 0 END,
               updated_at      = now(),
               last_seen_at    = CASE WHEN NEW.kind IN ('earn', 'redeem')
                                      THEN GREATEST(last_seen_at, NEW.created_at) ELSE last_seen_at END
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

