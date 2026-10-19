-- Rewards by Madar: the card's new look (wallet::look, LOOK_VERSION 2) and no
-- reward text on passes. A pass already in a customer's wallet only changes
-- when its device is told to fetch it again, so every card that is installed
-- somewhere is marked stale ONCE: the refresh sweep (wallet::refresh, 200 per
-- 5-minute tick, Apple push + Google PUT) then brings each one up to date.
-- Bookkeeping only: `pass_updated_at` draws nothing, and the sync trigger on
-- this table fires on `deleted_at` alone.
UPDATE loyalty_memberships c
   SET pass_updated_at = NULL
 WHERE c.pass_updated_at IS NOT NULL
   AND c.deleted_at IS NULL
   AND (c.google_object_id IS NOT NULL
        OR EXISTS (SELECT 1 FROM loyalty_pass_devices d WHERE d.customer_id = c.id));
