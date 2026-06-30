-- Reservations & waitlist — part 3: new permission resources.
--
-- `floor_plan`   — section + table-geometry authoring (managers, dashboard-only).
-- `reservations` — booking host ops + table status (managers, host/teller).
--
-- Kept in its own migration and NOT used in-file: `ALTER TYPE ... ADD VALUE`
-- values can't be referenced in the same transaction they're added in. The
-- role-permission rows are seeded at startup by `permissions::seeder`.
ALTER TYPE permission_resource ADD VALUE IF NOT EXISTS 'floor_plan';
ALTER TYPE permission_resource ADD VALUE IF NOT EXISTS 'reservations';
