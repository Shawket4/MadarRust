-- Waste from the teller app (PERMISSIONS_RESUME phase 6, capability 49
-- `inventory.waste.record`).
--
-- A waste is still ledger rows: one `inventory_movements` row of type `waste`
-- per ingredient (source_type `waste`), which is what moves `branch_stock`.
-- This table is the HEADER a till's waste needs on top of that:
--   * a client-minted id, so a queued waste replayed twice posts once;
--   * what the person picked (an ingredient, or a menu item that the server
--     exploded through its recipe) with the quantity and unit they typed;
--   * where it came from (`pos` / `dashboard`), the device and till, when it
--     happened on the device, and the manager approval it carried.
-- Movement rows point at it through `source_id`. A dashboard waste recorded by
-- the older route has no header (source_id NULL) and reads as `dashboard`.
--
-- Not POS-visible: no changefeed trigger.

CREATE TABLE IF NOT EXISTS waste_events (
    id                uuid PRIMARY KEY,
    org_id            uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    branch_id         uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    source            text NOT NULL CHECK (source IN ('pos', 'dashboard')),
    subject_kind      text NOT NULL CHECK (subject_kind IN ('ingredient', 'menu_item')),
    org_ingredient_id uuid NULL REFERENCES org_ingredients(id) ON DELETE SET NULL,
    menu_item_id      uuid NULL REFERENCES menu_items(id) ON DELETE SET NULL,
    subject_name      text NOT NULL,
    size_label        text NULL,
    quantity          numeric(12,3) NOT NULL CHECK (quantity > 0),
    unit              text NOT NULL,
    reason            text NOT NULL,
    note              text NULL,
    -- Piastres at the branch's unit costs when recorded; NULL when no line had a cost.
    value_minor       bigint NULL,
    -- Some ingredient on the waste had no cost, so `value_minor` is partial.
    value_partial     boolean NOT NULL DEFAULT false,
    recorded_by       uuid NULL REFERENCES users(id) ON DELETE SET NULL,
    device_id         uuid NULL,
    till_id           uuid NULL,
    approval_id       uuid NULL,
    occurred_at       timestamptz NOT NULL,
    received_at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS waste_events_branch_time ON waste_events (branch_id, occurred_at DESC);

ALTER TABLE waste_events ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS waste_events_tenant ON waste_events;
CREATE POLICY waste_events_tenant ON waste_events
    USING (org_id = NULLIF(current_setting('app.org_id', true), '')::uuid);
GRANT SELECT, INSERT ON waste_events TO madar_app;

-- The movement rows of one waste, for the log's join.
CREATE INDEX IF NOT EXISTS inventory_movements_waste_source
    ON inventory_movements (source_id) WHERE source_type = 'waste' AND source_id IS NOT NULL;

-- A till decides the `max_value` limit offline, so the feed's ingredient row now
-- carries the org cost (additive; projection in src/sync/pull/projection.rs).
-- Re-emit every live ingredient once so devices pick the field up.
SELECT sync_emit_org(oi.org_id, 'ingredient', oi.id, sync_op(sync_live_ingredient(oi)))
  FROM org_ingredients oi
 WHERE oi.deleted_at IS NULL;
