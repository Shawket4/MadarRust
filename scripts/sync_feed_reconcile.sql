-- DB audit Phase 6: changefeed reconciliation (read-only). Run before and
-- after the first ledger-retention sweep on a prod copy, and read-only on prod:
--
--   psql "$DATABASE_URL" -v retention_hours=384 -At -F$'\t' -f scripts/sync_feed_reconcile.sql
--
-- retention_hours = 48 (ledger window) + SYNC_LEDGER_RETENTION_SLACK_HOURS
-- (default 336). One TSV row per (branch, type, metric), ids only.
--
-- Must hold after the sweep:
--   * `state_missing` = 0 for every state type (no live row without a feed row);
--   * `ledger_missing_in_retention` = 0 (no ledger row a snapshot can still ask
--     for lost its feed row);
--   * ledger source counts (`source_rows`) unchanged from before: the sweep
--     only ever deletes changefeed rows;
--   * `watermark` >= the max seq the sweep purged (resync for older cursors).
\set ON_ERROR_STOP on
WITH cutoff AS (SELECT now() - make_interval(hours => :retention_hours) AS at),
live_since AS (
    SELECT l.* FROM branches b, cutoff, LATERAL sync_live_rows_since(b.id, cutoff.at) l
     WHERE b.deleted_at IS NULL),
feed AS (
    SELECT branch_id, type, count(*) AS n FROM sync_changes GROUP BY 1, 2)
SELECT branch_id, type, 'feed_rows', n FROM feed
UNION ALL
SELECT l.branch_id, l.type,
       CASE WHEN l.type IN ('till', 'cash_movement', 'order', 'refund', 'staff_drink')
            THEN 'ledger_missing_in_retention' ELSE 'state_missing' END,
       count(*)
  FROM live_since l
 WHERE NOT EXISTS (SELECT 1 FROM sync_changes c
                    WHERE c.branch_id = l.branch_id AND c.type = l.type AND c.entity_id = l.entity_id)
 GROUP BY 1, 2, 3
UNION ALL
SELECT t.branch_id, 'till', 'source_rows', count(*) FROM tills t GROUP BY 1
UNION ALL
SELECT t.branch_id, 'cash_movement', 'source_rows', count(*)
  FROM till_cash_movements m JOIN tills t ON t.id = m.till_id GROUP BY 1
UNION ALL
SELECT o.branch_id, 'order', 'source_rows', count(*) FROM orders o GROUP BY 1
UNION ALL
SELECT r.branch_id, 'refund', 'source_rows', count(*) FROM order_refunds r GROUP BY 1
UNION ALL
SELECT branch_id, '-', 'watermark', purged_through_seq FROM sync_feed_watermarks
ORDER BY 1, 2, 3;
