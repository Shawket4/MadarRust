-- Permission resources for held orders + the table-transfer waitlist.
-- Kept in its own migration and NOT used in-file: `ALTER TYPE ... ADD VALUE`
-- is allowed inside a migration transaction on PG 12+, but the new value can't
-- be USED until that transaction commits (same pattern as 20260630120200).
-- The role_permissions defaults are seeded at boot by permissions::seeder.

ALTER TYPE permission_resource ADD VALUE IF NOT EXISTS 'held_orders';
ALTER TYPE permission_resource ADD VALUE IF NOT EXISTS 'table_transfers';
