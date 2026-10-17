-- OPT-B (capacity, owner-approved 2026-10-17): storage settings for the
-- tables the till traffic rewrites all day. Settings only: no row is read,
-- moved or rewritten, and `ALTER TABLE … SET (…)` takes SHARE UPDATE
-- EXCLUSIVE (reads and writes carry on) for a catalog update.
--
-- 1. FILLFACTOR, so an UPDATE finds room for the new row version on the same
--    page and can be a HOT update (no new index entries, no index bloat). It
--    pays only where the columns the hot path updates are in no index; it
--    applies to pages written from now on (existing full pages fill up as
--    they are vacuumed and reused; nothing is rewritten here):
--      branch_stock   on_hand / last_movement_at, every sale line   (70)
--      ref counters   the counter value, every sale / ticket / delivery
--                     (ref_counters + the three per-scope tables it mirrors
--                     into, Phase 5.3)                                (50)
--      open_tickets   subtotal / updated_at, every round              (80)
--      tills          the till's running figures                      (80)
--      branch_tables  live status                                     (80)
--    NOT given one: orders, table_occupancies (an `(branch_id, updated_at)`
--    index makes every update non-HOT), sync_changes (`seq` is indexed and is
--    what every upsert changes), kitchen_tickets / kitchen_ticket_items
--    (status and bump columns are indexed). A fillfactor there only spreads
--    the table over more pages.
--
-- 2. AUTOVACUUM per table. The global default (vacuum at 20 % dead rows,
--    analyze at 10 %) lets a small, constantly-rewritten table carry many
--    times its live size in dead versions between vacuums, and every read of
--    it pays for them:
--      sync_changes   one row per entity, upserted by every changefeed emit
--                     (the most-updated table there is); its indexes bloat
--                     with every seq bump                        (1 % / 2 %)
--      branch_stock, tills, open_tickets, ref counters, branch_tables,
--      table_occupancies, kitchen_tickets                        (5 % / 5 %)
--    and for the append-mostly sale ledgers (orders, order_items,
--    order_payments, order_item_addons, inventory_movements,
--    kitchen_ticket_items) an insert-driven vacuum at 5 % (default 20 %) so
--    the visibility map, which index-only scans and the next vacuum lean on,
--    keeps up with the day's inserts.
--    The server-wide autovacuum settings are the server agent's (OPT-C).
DO $optb$
DECLARE
    r record;
BEGIN
    FOR r IN SELECT * FROM (VALUES
        ('branch_stock',          'fillfactor = 70, autovacuum_vacuum_scale_factor = 0.05, autovacuum_analyze_scale_factor = 0.05'),
        ('ref_counters',          'fillfactor = 50, autovacuum_vacuum_scale_factor = 0.05, autovacuum_analyze_scale_factor = 0.05'),
        ('order_ref_counters',    'fillfactor = 50, autovacuum_vacuum_scale_factor = 0.05, autovacuum_analyze_scale_factor = 0.05'),
        ('ticket_ref_counters',   'fillfactor = 50, autovacuum_vacuum_scale_factor = 0.05, autovacuum_analyze_scale_factor = 0.05'),
        ('delivery_ref_counters', 'fillfactor = 50, autovacuum_vacuum_scale_factor = 0.05, autovacuum_analyze_scale_factor = 0.05'),
        ('open_tickets',          'fillfactor = 80, autovacuum_vacuum_scale_factor = 0.05, autovacuum_analyze_scale_factor = 0.05'),
        ('tills',                 'fillfactor = 80, autovacuum_vacuum_scale_factor = 0.05, autovacuum_analyze_scale_factor = 0.05'),
        ('branch_tables',         'fillfactor = 80, autovacuum_vacuum_scale_factor = 0.05, autovacuum_analyze_scale_factor = 0.05'),
        ('table_occupancies',     'autovacuum_vacuum_scale_factor = 0.05, autovacuum_analyze_scale_factor = 0.05'),
        ('kitchen_tickets',       'autovacuum_vacuum_scale_factor = 0.05, autovacuum_analyze_scale_factor = 0.05'),
        ('sync_changes',          'autovacuum_vacuum_scale_factor = 0.01, autovacuum_analyze_scale_factor = 0.02'),
        ('orders',                'autovacuum_vacuum_insert_scale_factor = 0.05'),
        ('order_items',           'autovacuum_vacuum_insert_scale_factor = 0.05'),
        ('order_payments',        'autovacuum_vacuum_insert_scale_factor = 0.05'),
        ('order_item_addons',     'autovacuum_vacuum_insert_scale_factor = 0.05'),
        ('inventory_movements',   'autovacuum_vacuum_insert_scale_factor = 0.05'),
        ('kitchen_ticket_items',  'autovacuum_vacuum_insert_scale_factor = 0.05')
    ) AS x(t, opts)
    LOOP
        -- A table a later cleanup turned into a view (or removed) is skipped.
        IF EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                    WHERE n.nspname = 'public' AND c.relname = r.t AND c.relkind = 'r') THEN
            EXECUTE format('ALTER TABLE public.%I SET (%s)', r.t, r.opts);
        END IF;
    END LOOP;
END
$optb$;
