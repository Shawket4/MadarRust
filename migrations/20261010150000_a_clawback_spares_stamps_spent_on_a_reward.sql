-- A void or refund no longer claws an earn back once a reward has been claimed
-- since that sale (owner, 2026-10-10).
--
-- The stamps that sale earned went into the reward. Taking them back out of
-- what the customer has collected SINCE punishes them for stamps they earned
-- afterwards, so a later void or refund deducts nothing. "Claimed since" means
-- a redeem by the same member, in the same currency, written after the earn,
-- on another order (a reward on the voided sale itself is given back by the
-- same void), and not itself undone in full.
--
-- A programme that opted into `allow_negative_balance` asked for spent points
-- to be clawed back below zero; it keeps that. Every other clawback is still
-- clamped to the balance, as before.

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

    -- A clawback (undoing something that ADDED to the balance) is clamped to
    -- what the member still holds unless the programme lets balances go
    -- negative. Undoing a redeem or a deduction only ever gives back.
    IF orig.points > 0 AND NOT loyalty_allows_negative_balance(orig.org_id, orig.branch_id) THEN
        -- An earn whose stamps a reward has since spent is not clawed back.
        IF orig.kind = 'earn' AND EXISTS (
            SELECT 1 FROM loyalty_transactions r
             WHERE r.customer_id = orig.customer_id
               AND r.currency = orig.currency
               AND r.kind = 'redeem'
               AND r.created_at > orig.created_at
               AND r.order_id IS DISTINCT FROM orig.order_id
               AND abs(r.points) > (SELECT COALESCE(SUM(abs(x.points)), 0)
                                      FROM loyalty_transactions x WHERE x.reverses_id = r.id))
        THEN
            RETURN NULL;
        END IF;
        SELECT CASE WHEN orig.currency = 'points' THEN points_balance ELSE visits_balance END
          INTO balance
          FROM loyalty_customers WHERE id = orig.customer_id FOR UPDATE;
        amount := LEAST(amount, GREATEST(COALESCE(balance, 0), 0));
        IF amount <= 0 THEN
            RETURN NULL;
        END IF;
    END IF;

    INSERT INTO loyalty_transactions
        (org_id, customer_id, branch_id, kind, currency, points,
         order_id, reverses_id, source, created_by, note)
    VALUES
        (orig.org_id, orig.customer_id, orig.branch_id,
         ('reverse_' || orig.kind::text)::loyalty_txn_kind,
         orig.currency, -sign(orig.points) * amount,
         orig.order_id, orig.id, p_source, p_by, p_note)
    RETURNING id INTO new_id;
    RETURN new_id;
END $$;
