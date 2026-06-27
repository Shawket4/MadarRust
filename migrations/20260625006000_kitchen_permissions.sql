-- New permission resources for the KDS + waiter features. Kept separate from any
-- use of the values (the role-permission seed runs at startup via the seeder), and
-- separate from the order_type/role enum changes — `ALTER TYPE ... ADD VALUE`
-- values can't be used in the same transaction they're added in.
--   kitchen_stations — station + routing config (managers)
--   kitchen_orders   — the KDS feed + bump (kitchen-display / till devices)
--   open_tickets     — waiter fire/round + cashier settle
ALTER TYPE permission_resource ADD VALUE IF NOT EXISTS 'kitchen_stations';
ALTER TYPE permission_resource ADD VALUE IF NOT EXISTS 'kitchen_orders';
ALTER TYPE permission_resource ADD VALUE IF NOT EXISTS 'open_tickets';
