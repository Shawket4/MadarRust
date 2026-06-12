-- Add missing permission resources to the enum
-- Using the rename trick: rename old, create new with all values, migrate, drop old.

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
    'permissions'
);

-- Migrate all columns that use this enum
ALTER TABLE role_permissions
    ALTER COLUMN resource TYPE permission_resource
    USING resource::text::permission_resource;

ALTER TABLE permissions
    ALTER COLUMN resource TYPE permission_resource
    USING resource::text::permission_resource;

DROP TYPE permission_resource_old;

-- Seed default permissions for the two new resources.
-- Use ON CONFLICT DO NOTHING so existing customisations are not overwritten.

-- discounts: org_admin gets full CRUD
INSERT INTO role_permissions (role, resource, action, granted)
VALUES
    ('org_admin', 'discounts', 'create', true),
    ('org_admin', 'discounts', 'read',   true),
    ('org_admin', 'discounts', 'update', true),
    ('org_admin', 'discounts', 'delete', true)
ON CONFLICT (role, resource, action) DO NOTHING;

-- discounts: branch_manager can read and update (apply discounts to orders)
INSERT INTO role_permissions (role, resource, action, granted)
VALUES
    ('branch_manager', 'discounts', 'read',   true),
    ('branch_manager', 'discounts', 'update', true)
ON CONFLICT (role, resource, action) DO NOTHING;

-- discounts: teller can only read (see available discounts)
INSERT INTO role_permissions (role, resource, action, granted)
VALUES
    ('teller', 'discounts', 'read', true)
ON CONFLICT (role, resource, action) DO NOTHING;

-- payment_methods: org_admin gets full CRUD
INSERT INTO role_permissions (role, resource, action, granted)
VALUES
    ('org_admin', 'payment_methods', 'create', true),
    ('org_admin', 'payment_methods', 'read',   true),
    ('org_admin', 'payment_methods', 'update', true),
    ('org_admin', 'payment_methods', 'delete', true)
ON CONFLICT (role, resource, action) DO NOTHING;

-- payment_methods: branch_manager can read
INSERT INTO role_permissions (role, resource, action, granted)
VALUES
    ('branch_manager', 'payment_methods', 'read', true)
ON CONFLICT (role, resource, action) DO NOTHING;

-- payment_methods: teller can read
INSERT INTO role_permissions (role, resource, action, granted)
VALUES
    ('teller', 'payment_methods', 'read', true)
ON CONFLICT (role, resource, action) DO NOTHING;

-- reports: org_admin gets read
INSERT INTO role_permissions (role, resource, action, granted)
VALUES
    ('org_admin', 'reports', 'read', true)
ON CONFLICT (role, resource, action) DO NOTHING;

-- reports: branch_manager gets read
INSERT INTO role_permissions (role, resource, action, granted)
VALUES
    ('branch_manager', 'reports', 'read', true)
ON CONFLICT (role, resource, action) DO NOTHING;
