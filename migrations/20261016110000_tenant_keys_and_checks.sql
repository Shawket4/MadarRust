-- DB architecture Phase 1.3 + 1.5 (DB_ARCHITECTURE_AUDIT_2026-10-16 §8).
-- ADDITIVE ONLY: constraints are added NOT VALID (they enforce new rows at
-- once) and then VALIDATEd. A VALIDATE that finds an existing row breaking the
-- rule does NOT fail the boot: the constraint stays NOT VALID, still guarding
-- every new row, and a WARNING names it (scripts/db_phase1_check.sql lists
-- what is left). Nothing here moves or changes a row.
--
-- 1.3 Composite tenant keys. Each child's (org_id, parent_id) must be a
-- (org_id, id) of its parent (unique indexes uq_<parent>_org_id_id, built
-- CONCURRENTLY just before this file), so a row can no longer point at a
-- parent in another org. Each new FK copies the ON DELETE action of the
-- existing single-column FK it shadows, so delete behaviour is unchanged.
--
-- 1.5 Discriminator CHECKs. `kitchen_tickets` needs none: its source_type is a
-- generated column and `kitchen_tickets_has_one_source` already enforces
-- exactly one of order_id / open_ticket_id.

SET LOCAL lock_timeout = '15s';

CREATE OR REPLACE FUNCTION pg_temp.p1_add(t text, con text, def text)
RETURNS void LANGUAGE plpgsql AS $fn$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint
                    WHERE conrelid = format('%I', t)::regclass AND conname = con) THEN
        EXECUTE format('ALTER TABLE %I ADD CONSTRAINT %I %s NOT VALID', t, con, def);
    END IF;
    BEGIN
        EXECUTE format('ALTER TABLE %I VALIDATE CONSTRAINT %I', t, con);
    EXCEPTION WHEN check_violation OR foreign_key_violation THEN
        RAISE WARNING '%.%: existing rows break it; left NOT VALID (new rows are still checked)',
            t, con;
    END;
END
$fn$;

-- ── 1.3 composite tenant FKs ────────────────────────────────────────────────
SELECT pg_temp.p1_add('orders', 'orders_org_branch_fk',
    'FOREIGN KEY (org_id, branch_id) REFERENCES branches (org_id, id)');
SELECT pg_temp.p1_add('orders', 'orders_org_till_fk',
    'FOREIGN KEY (org_id, till_id) REFERENCES tills (org_id, id)');
SELECT pg_temp.p1_add('tills', 'tills_org_branch_fk',
    'FOREIGN KEY (org_id, branch_id) REFERENCES branches (org_id, id)');
SELECT pg_temp.p1_add('order_items', 'order_items_org_order_fk',
    'FOREIGN KEY (org_id, order_id) REFERENCES orders (org_id, id) ON DELETE CASCADE');
SELECT pg_temp.p1_add('order_payments', 'order_payments_org_order_fk',
    'FOREIGN KEY (org_id, order_id) REFERENCES orders (org_id, id) ON DELETE CASCADE');
SELECT pg_temp.p1_add('order_item_addons', 'order_item_addons_org_item_fk',
    'FOREIGN KEY (org_id, order_item_id) REFERENCES order_items (org_id, id) ON DELETE CASCADE');
SELECT pg_temp.p1_add('order_item_optionals', 'order_item_optionals_org_item_fk',
    'FOREIGN KEY (org_id, order_item_id) REFERENCES order_items (org_id, id) ON DELETE CASCADE');
SELECT pg_temp.p1_add('till_cash_movements', 'till_cash_movements_org_till_fk',
    'FOREIGN KEY (org_id, till_id) REFERENCES tills (org_id, id) ON DELETE CASCADE');
SELECT pg_temp.p1_add('open_tickets', 'open_tickets_org_branch_fk',
    'FOREIGN KEY (org_id, branch_id) REFERENCES branches (org_id, id) ON DELETE CASCADE');
SELECT pg_temp.p1_add('open_ticket_rounds', 'open_ticket_rounds_org_ticket_fk',
    'FOREIGN KEY (org_id, open_ticket_id) REFERENCES open_tickets (org_id, id) ON DELETE CASCADE');
SELECT pg_temp.p1_add('open_ticket_items', 'open_ticket_items_org_ticket_fk',
    'FOREIGN KEY (org_id, open_ticket_id) REFERENCES open_tickets (org_id, id) ON DELETE CASCADE');
SELECT pg_temp.p1_add('open_ticket_items', 'open_ticket_items_org_round_fk',
    'FOREIGN KEY (org_id, round_id) REFERENCES open_ticket_rounds (org_id, id) ON DELETE CASCADE');
SELECT pg_temp.p1_add('kitchen_tickets', 'kitchen_tickets_org_branch_fk',
    'FOREIGN KEY (org_id, branch_id) REFERENCES branches (org_id, id) ON DELETE CASCADE');
SELECT pg_temp.p1_add('kitchen_tickets', 'kitchen_tickets_org_order_fk',
    'FOREIGN KEY (org_id, order_id) REFERENCES orders (org_id, id) ON DELETE CASCADE');
SELECT pg_temp.p1_add('kitchen_tickets', 'kitchen_tickets_org_ticket_fk',
    'FOREIGN KEY (org_id, open_ticket_id) REFERENCES open_tickets (org_id, id) ON DELETE CASCADE');
SELECT pg_temp.p1_add('kitchen_ticket_items', 'kitchen_ticket_items_org_ticket_fk',
    'FOREIGN KEY (org_id, kitchen_ticket_id) REFERENCES kitchen_tickets (org_id, id) ON DELETE CASCADE');
SELECT pg_temp.p1_add('menu_item_sizes', 'menu_item_sizes_org_item_fk',
    'FOREIGN KEY (org_id, menu_item_id) REFERENCES menu_items (org_id, id) ON DELETE CASCADE');
SELECT pg_temp.p1_add('modifier_options', 'modifier_options_org_group_fk',
    'FOREIGN KEY (org_id, group_id) REFERENCES modifier_groups (org_id, id) ON DELETE CASCADE');
SELECT pg_temp.p1_add('recipe_lines', 'recipe_lines_org_ingredient_fk',
    'FOREIGN KEY (org_id, ingredient_id) REFERENCES org_ingredients (org_id, id) ON DELETE RESTRICT');
SELECT pg_temp.p1_add('branch_stock', 'branch_stock_org_branch_fk',
    'FOREIGN KEY (org_id, branch_id) REFERENCES branches (org_id, id) ON DELETE CASCADE');
SELECT pg_temp.p1_add('branch_stock', 'branch_stock_org_ingredient_fk',
    'FOREIGN KEY (org_id, org_ingredient_id) REFERENCES org_ingredients (org_id, id) ON DELETE RESTRICT');
SELECT pg_temp.p1_add('inventory_movements', 'inventory_movements_org_branch_fk',
    'FOREIGN KEY (org_id, branch_id) REFERENCES branches (org_id, id) ON DELETE CASCADE');
SELECT pg_temp.p1_add('inventory_movements', 'inventory_movements_org_ingredient_fk',
    'FOREIGN KEY (org_id, org_ingredient_id) REFERENCES org_ingredients (org_id, id) ON DELETE RESTRICT');
SELECT pg_temp.p1_add('sync_changes', 'sync_changes_org_branch_fk',
    'FOREIGN KEY (org_id, branch_id) REFERENCES branches (org_id, id) ON DELETE CASCADE');

-- ── 1.5 discriminator CHECKs ────────────────────────────────────────────────
-- Every `source_type` the code writes (record_movement callers, 2026-10-16:
-- order, refund, waste, stocktake, transfer, purchase, goods_receipt,
-- delivery_order). NULL stays allowed (manual adjustments carry none).
-- A NEW source_type must be added here in the same change, or its insert fails.
SELECT pg_temp.p1_add('inventory_movements', 'inventory_movements_source_type_check',
    $$CHECK (source_type IS NULL OR source_type IN (
        'order', 'refund', 'waste', 'stocktake', 'transfer', 'purchase',
        'goods_receipt', 'delivery_order'))$$);
-- madar-sync's ALL_TYPES (madar-shared v0.8.1). A NEW sync type must be added
-- here in the same change: `sync_changes_type_is_madar_sync_all_types` in
-- tests/db_phase1_tests.rs fails until it is.
SELECT pg_temp.p1_add('sync_changes', 'sync_changes_type_check',
    $$CHECK (type IN (
        'category', 'menu_item', 'bundle', 'ingredient', 'payment_method',
        'payment_availability', 'discount', 'branch_settings', 'device', 'teller',
        'floor_section', 'floor_table', 'table_occupancy', 'table_transfer',
        'open_ticket', 'kitchen_ticket', 'delivery', 'booking', 'till',
        'cash_movement', 'order', 'refund', 'addon_item', 'customer',
        'staff_drink', 'deal_rule'))$$);

DROP FUNCTION pg_temp.p1_add(text, text, text);
