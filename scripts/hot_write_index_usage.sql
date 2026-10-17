-- Index usage on the hot WRITE tables (OPT-B item 4, 2026-10-17). Read-only.
--
-- Every index on these tables costs every INSERT (and every non-HOT UPDATE)
-- one more index write. One is a drop candidate only when ALL hold:
--   * it enforces nothing (`backs_constraint` = f: not a PK / UNIQUE /
--     EXCLUDE constraint and not a unique index; those stay whatever their
--     scan count),
--   * it is not the only index on a foreign-key column (`fk_column` = t means
--     a DELETE on the referenced table scans this table without it),
--   * `idx_scan` = 0 on LIVE prod over a stats window of at least 30 days
--     (`stats_window`). A restored copy carries no usage stats, and a bench
--     only exercises the till's paths, never the dashboard's reports: neither
--     is evidence.
-- A candidate is dropped with its own `-- no-transaction` migration holding one
-- `DROP INDEX CONCURRENTLY IF EXISTS …` (see migrations/parked/idx_orders_*__drop.sql).
--
--   psql "$PROD_RO" -X -A -F $'\t' -f scripts/hot_write_index_usage.sql
SELECT c.relname AS table_name,
       x.indexrelname AS index_name,
       x.idx_scan,
       now() - d.stats_reset AS stats_window,
       pg_size_pretty(pg_relation_size(i.indexrelid)) AS size,
       i.indisunique OR EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conindid = i.indexrelid)
           AS backs_constraint,
       EXISTS (SELECT 1 FROM pg_constraint k
                WHERE k.conrelid = i.indrelid AND k.contype = 'f'
                  AND k.conkey::int2[] = (i.indkey::int2[])[0:array_length(k.conkey, 1) - 1]) AS fk_column,
       pg_get_indexdef(i.indexrelid) AS definition
  FROM pg_index i
  JOIN pg_class c ON c.oid = i.indrelid
  JOIN pg_stat_user_indexes x ON x.indexrelid = i.indexrelid
  JOIN pg_stat_database d ON d.datname = current_database()
 WHERE c.relname IN ('orders', 'order_items', 'order_item_addons', 'order_item_optionals',
                     'order_payments', 'inventory_movements', 'kitchen_tickets',
                     'kitchen_ticket_items', 'sync_changes', 'open_tickets',
                     'open_ticket_items', 'open_ticket_rounds', 'branch_stock', 'tills',
                     'table_occupancies', 'till_cash_movements', 'order_refunds',
                     'order_refund_lines')
 ORDER BY c.relname, x.idx_scan, x.indexrelname;
