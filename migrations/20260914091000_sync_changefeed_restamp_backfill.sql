-- The changefeed backfill (20260914090300) inserted every live row with
-- changed_at = now(), i.e. the moment that migration ran. A full /sync/pull
-- windows LEDGER rows (till, cash_movement, order, refund) by changed_at
-- (last 48 h + open tills), so for 48 h after deploy every device's full pull
-- shipped the branch's entire history.
--
-- Why a new migration and not a different window query: changed_at is the
-- contract's window key (TILLS_CONTRACT §10.2) and it is RIGHT for every row
-- written after the feed exists — a void or refund on a week-old order moves
-- that order's changed_at to now, so the correction reaches a fresh snapshot.
-- A window on the business timestamp would drop exactly those rows. Only the
-- backfilled stamps are wrong, so only they are corrected. 20260914090300 is
-- already applied on rehearsal databases, so it is not edited.
--
-- A backfilled row is one still carrying that migration's transaction time
-- (sqlx records installed_on inside the same transaction, so the two are
-- equal); any row changed since has a later stamp and is left alone. Each is
-- restamped to its entity's own latest business timestamp (opened/closed,
-- created/voided, issued), never later than the original stamp. `updated_at`
-- is NOT used: bulk migrations (the rework's own column renames among them)
-- touch it on every row, which would put all history back in the window.
-- State types are not windowed, and their changed_at is read by nothing but
-- the tombstone purge (op = 'delete' rows, which the backfill never wrote), so
-- they are left as they are.
DO $$
DECLARE
    stamp timestamptz;
BEGIN
    SELECT installed_on INTO stamp FROM _sqlx_migrations WHERE version = 20260914090300;
    IF stamp IS NULL THEN
        RETURN;
    END IF;

    UPDATE sync_changes c
       SET changed_at = LEAST(stamp, GREATEST(t.opened_at, t.closed_at, t.force_closed_at))
      FROM tills t
     WHERE c.type = 'till' AND c.entity_id = t.id AND c.changed_at = stamp;

    UPDATE sync_changes c
       SET changed_at = LEAST(stamp, m.created_at)
      FROM till_cash_movements m
     WHERE c.type = 'cash_movement' AND c.entity_id = m.id AND c.changed_at = stamp;

    UPDATE sync_changes c
       SET changed_at = LEAST(stamp, GREATEST(o.created_at, o.voided_at))
      FROM orders o
     WHERE c.type = 'order' AND c.entity_id = o.id AND c.changed_at = stamp;

    UPDATE sync_changes c
       SET changed_at = LEAST(stamp, GREATEST(r.issued_at, r.created_at))
      FROM order_refunds r
     WHERE c.type = 'refund' AND c.entity_id = r.id AND c.changed_at = stamp;
END $$;
