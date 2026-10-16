-- DB audit 2026-10-16, Phase 6 (sync scale): a time-keyed ledger feed and an
-- incremental sweeper. Additive: the wire protocol tills speak is unchanged.
--
-- 1. LEDGER RETENTION. `sync_changes` is compacted per entity, but ledger
--    entities (till, cash_movement, order, refund, staff_drink) are never
--    tombstoned, so the feed held one row per sale for ever (9,929 order rows
--    for 9,929 orders on the 2026-09-05 copy). The sweeper now deletes ledger
--    rows older than the retention (the 48 h full-snapshot ledger window +
--    slack, `SYNC_LEDGER_RETENTION_SLACK_HOURS`, default 14 days) unless they
--    belong to a till that is open or closed inside the retention — exactly
--    the rows a full snapshot can still ask for — and raises the branch's
--    `sync_feed_watermarks.purged_through_seq` past them, so a device whose
--    cursor predates the purge is told to resync (the same path tombstone
--    purges already use). The source rows are never touched: this is the
--    changefeed, not the ledger.
--
-- 2. INCREMENTAL SWEEP. The gap pass ("live rows with no feed row") ran
--    `sync_live_rows()` NOT EXISTS against all of `sync_changes`, two seq
--    scans that grew with history. It now runs per branch over
--    `sync_live_rows_since(branch, retention start)` — the per-branch live set
--    with time-keyed ledger arms, so the work is the branch's state plus its
--    recent ledger, not its history — and only for a branch whose feed head
--    moved past its `sync_sweep_state.checked_through_seq` watermark, or whose
--    last check is older than `SYNC_SWEEP_IDLE_RECHECK_MINUTES` (default 360).
--    The time-based delete pass (kitchen_ticket / delivery / booking exits)
--    probes each feed row's own entity instead of computing the whole live set.
--
-- `sync_live_rows_since(b, '-infinity')` equals `sync_live_rows_for_branch(b)`
-- (test `sync_live_rows_since_matches_for_branch`): change both together.

CREATE OR REPLACE FUNCTION sync_live_rows_since(p_branch uuid, p_ledger_from timestamptz)
    RETURNS TABLE (branch_id uuid, type text, entity_id uuid)
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public
    AS $$
    WITH ob AS (SELECT id AS branch_id, org_id FROM branches WHERE deleted_at IS NULL AND id = p_branch)
    SELECT ob.branch_id, 'category', x.id FROM categories x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_category(x)
    UNION ALL
    SELECT ob.branch_id, 'menu_item', x.id FROM menu_items x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_menu_item(x)
    UNION ALL
    SELECT ob.branch_id, 'ingredient', x.id FROM org_ingredients x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_ingredient(x)
    UNION ALL
    SELECT ob.branch_id, 'payment_method', x.id FROM org_payment_methods x JOIN ob ON ob.org_id = x.org_id
    UNION ALL
    SELECT DISTINCT x.branch_id, 'payment_availability', x.branch_id FROM branch_payment_methods x JOIN ob ON ob.branch_id = x.branch_id
    UNION ALL
    SELECT DISTINCT ob.branch_id, 'payment_availability', x.user_id FROM user_payment_methods x JOIN users u ON u.id = x.user_id JOIN ob ON ob.org_id = u.org_id
    UNION ALL
    SELECT DISTINCT ob.branch_id, 'payment_availability', x.device_id FROM device_payment_methods x JOIN devices d ON d.id = x.device_id JOIN ob ON ob.org_id = d.org_id
    UNION ALL
    SELECT ob.branch_id, 'discount', x.id FROM discounts x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_discount(x)
    UNION ALL
    SELECT x.id, 'branch_settings', x.id FROM branches x WHERE x.id = p_branch AND sync_live_branch_settings(x)
    UNION ALL
    SELECT x.branch_id, 'device', x.id FROM devices x WHERE x.branch_id = p_branch AND sync_live_device(x)
    UNION ALL
    SELECT ob.branch_id, 'teller', x.id FROM users x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_teller(x)
    UNION ALL
    SELECT x.branch_id, 'floor_section', x.id FROM floor_sections x WHERE x.branch_id = p_branch
    UNION ALL
    SELECT x.branch_id, 'floor_table', x.id FROM branch_tables x WHERE x.branch_id = p_branch AND sync_live_floor_table(x)
    UNION ALL
    SELECT x.branch_id, 'table_occupancy', x.id FROM table_occupancies x WHERE x.branch_id = p_branch AND sync_live_table_occupancy(x)
    UNION ALL
    SELECT x.branch_id, 'table_transfer', x.id FROM table_transfer_requests x WHERE x.branch_id = p_branch AND sync_live_table_transfer(x)
    UNION ALL
    SELECT x.branch_id, 'open_ticket', x.id FROM open_tickets x WHERE x.branch_id = p_branch AND sync_live_open_ticket(x)
    UNION ALL
    SELECT x.branch_id, 'kitchen_ticket', x.id FROM kitchen_tickets x WHERE x.branch_id = p_branch AND sync_live_kitchen_ticket(x)
    UNION ALL
    SELECT x.branch_id, 'delivery', x.id FROM delivery_orders x WHERE x.branch_id = p_branch AND sync_live_delivery(x)
    UNION ALL
    SELECT x.branch_id, 'booking', x.id FROM bookings x WHERE x.branch_id = p_branch AND sync_live_booking(x)
    UNION ALL
    SELECT x.branch_id, 'till', x.id FROM tills x WHERE x.branch_id = p_branch
       AND (x.updated_at >= p_ledger_from OR x.status = 'open' OR x.closed_at >= p_ledger_from)
    UNION ALL
    SELECT t.branch_id, 'cash_movement', x.id FROM till_cash_movements x JOIN tills t ON t.id = x.till_id WHERE t.branch_id = p_branch
       AND (x.created_at >= p_ledger_from OR t.status = 'open' OR t.closed_at >= p_ledger_from)
    UNION ALL
    -- Orders: changed since the window start (idx_orders_branch_updated), or
    -- on a till that is open / closed since then and not already counted.
    SELECT x.branch_id, 'order', x.id FROM orders x WHERE x.branch_id = p_branch AND x.updated_at >= p_ledger_from
    UNION ALL
    SELECT x.branch_id, 'order', x.id FROM tills t JOIN orders x ON x.till_id = t.id
     WHERE t.branch_id = p_branch AND (t.status = 'open' OR t.closed_at >= p_ledger_from)
       AND NOT (x.updated_at >= p_ledger_from)
    UNION ALL
    SELECT x.branch_id, 'refund', x.id FROM order_refunds x JOIN tills t ON t.id = x.till_id AND t.branch_id = p_branch
     WHERE x.branch_id = p_branch
       AND (x.created_at >= p_ledger_from OR t.status = 'open' OR t.closed_at >= p_ledger_from)
    UNION ALL
    SELECT ob.branch_id, 'addon_item', x.id FROM addon_items x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_addon_item(x.id)
    UNION ALL
    SELECT ob.branch_id, 'customer', x.id FROM customers x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_customer(x)
    UNION ALL
    SELECT ob.branch_id, 'deal_rule', x.id FROM deal_rules x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_deal_rule(x)
    $$;


COMMENT ON FUNCTION sync_live_rows_since(uuid, timestamptz) IS
    'sync_live_rows_for_branch with the ledger arms cut to rows changed since p_ledger_from or on a till open/closed since then (DB audit Phase 6).';

-- Where the incremental gap pass stands per branch.
CREATE TABLE IF NOT EXISTS sync_sweep_state (
    branch_id           uuid        PRIMARY KEY REFERENCES branches(id) ON DELETE CASCADE,
    checked_through_seq bigint      NOT NULL DEFAULT 0,
    checked_at          timestamptz NOT NULL DEFAULT '-infinity'
);
ALTER TABLE sync_sweep_state ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON sync_sweep_state;
CREATE POLICY tenant_isolation ON sync_sweep_state FOR ALL
    USING (EXISTS (SELECT 1 FROM branches b WHERE b.id = sync_sweep_state.branch_id
                   AND b.org_id = (SELECT current_setting('app.org_id', true)::uuid)));

-- The two purges read by age.
CREATE INDEX IF NOT EXISTS idx_sync_changes_tombstone_age
    ON sync_changes (changed_at) WHERE op = 'delete';
CREATE INDEX IF NOT EXISTS idx_sync_changes_ledger_age
    ON sync_changes (branch_id, changed_at)
    WHERE type IN ('till', 'cash_movement', 'order', 'refund', 'staff_drink');
