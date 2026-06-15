-- Two new permission resources: 'delivery_settings' (config + zones, manager) and
-- 'delivery_orders' (the queue + status + the POS open/close override, teller).
-- ALTER TYPE ... ADD VALUE is allowed inside a migration transaction on PG 12+ as
-- long as the new value is not USED in the same transaction — the role grants are
-- seeded idempotently at startup by src/permissions/seeder.rs, not here.
ALTER TYPE public.permission_resource ADD VALUE IF NOT EXISTS 'delivery_settings';
ALTER TYPE public.permission_resource ADD VALUE IF NOT EXISTS 'delivery_orders';
