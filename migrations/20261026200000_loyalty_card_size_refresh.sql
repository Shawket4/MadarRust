-- The card size (`model::card_size`, owner 2026-10-23): a pass now counts
-- towards the cheapest reward on offer, as the till always did, instead of
-- the programme's `default_reward_cost`. Installed cards only change when
-- their device is told to fetch them, so every card installed somewhere is
-- marked stale ONCE and the refresh sweep (wallet::refresh, 200 per 5-minute
-- tick) brings each up to date. Idempotent: rerunning marks the same rows,
-- which the sweep then refreshes again, harmlessly. Bookkeeping only:
-- `pass_updated_at` draws nothing, and this table's sync trigger fires on
-- `deleted_at` alone.
UPDATE loyalty_memberships c
   SET pass_updated_at = NULL
 WHERE c.pass_updated_at IS NOT NULL
   AND c.deleted_at IS NULL
   AND (c.google_object_id IS NOT NULL
        OR EXISTS (SELECT 1 FROM loyalty_pass_devices d WHERE d.customer_id = c.id));
