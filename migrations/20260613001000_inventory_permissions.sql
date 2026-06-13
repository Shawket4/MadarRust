-- New permission resources for the inventory overhaul: stocktakes (standalone
-- physical counts), inventory_waste (categorized waste), and the purchasing
-- pair suppliers / purchase_orders. Added in one enum rebuild (rename trick)
-- to avoid repeating the costly recreate per phase.
--
-- 'shift_counts' is retained as an enum label (dropping an enum value needs a
-- second rebuild and risks cascade failures) but is no longer used — its
-- seeded grants are deleted below now that shift-close counting is gone.

ALTER TYPE permission_resource RENAME TO permission_resource_old;

CREATE TYPE permission_resource AS ENUM (
    'orgs',
    'branches',
    'users',
    'categories',
    'menu_items',
    'addon_groups',
    'addon_items',
    'recipes',
    'inventory',
    'inventory_adjustments',
    'inventory_transfers',
    'orders',
    'order_items',
    'payments',
    'payment_methods',
    'shifts',
    'shift_counts',
    'soft_serve_batches',
    'discounts',
    'reports',
    'permissions',
    'stocktakes',
    'inventory_waste',
    'suppliers',
    'purchase_orders'
);

ALTER TABLE role_permissions
    ALTER COLUMN resource TYPE permission_resource
    USING resource::text::permission_resource;

ALTER TABLE permissions
    ALTER COLUMN resource TYPE permission_resource
    USING resource::text::permission_resource;

DROP TYPE permission_resource_old;

-- Retire shift_counts grants (feature removed).
DELETE FROM role_permissions WHERE resource = 'shift_counts';
DELETE FROM permissions      WHERE resource = 'shift_counts';

-- Seed role defaults for the new resources. org_admin = full CRUD;
-- branch_manager = operate (create/read/update); teller = read where useful.
INSERT INTO role_permissions (role, resource, action, granted) VALUES
    ('org_admin', 'stocktakes',      'create', true),
    ('org_admin', 'stocktakes',      'read',   true),
    ('org_admin', 'stocktakes',      'update', true),
    ('org_admin', 'stocktakes',      'delete', true),
    ('org_admin', 'inventory_waste', 'create', true),
    ('org_admin', 'inventory_waste', 'read',   true),
    ('org_admin', 'inventory_waste', 'update', true),
    ('org_admin', 'inventory_waste', 'delete', true),
    ('org_admin', 'suppliers',       'create', true),
    ('org_admin', 'suppliers',       'read',   true),
    ('org_admin', 'suppliers',       'update', true),
    ('org_admin', 'suppliers',       'delete', true),
    ('org_admin', 'purchase_orders', 'create', true),
    ('org_admin', 'purchase_orders', 'read',   true),
    ('org_admin', 'purchase_orders', 'update', true),
    ('org_admin', 'purchase_orders', 'delete', true),

    ('branch_manager', 'stocktakes',      'create', true),
    ('branch_manager', 'stocktakes',      'read',   true),
    ('branch_manager', 'stocktakes',      'update', true),
    ('branch_manager', 'inventory_waste', 'create', true),
    ('branch_manager', 'inventory_waste', 'read',   true),
    ('branch_manager', 'suppliers',       'read',   true),
    ('branch_manager', 'purchase_orders', 'create', true),
    ('branch_manager', 'purchase_orders', 'read',   true),
    ('branch_manager', 'purchase_orders', 'update', true)
ON CONFLICT (role, resource, action) DO NOTHING;
