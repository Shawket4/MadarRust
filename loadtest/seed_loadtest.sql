-- Load-test fixture, layered ON TOP OF scripts/seed_fuzz.sql.
--
-- scripts/loadtest.sh runs seed_fuzz.sql first (org/branch/users/menu/payment
-- methods, fixed UUIDs shared with src/bin/fuzz_token.rs), then this file, which
-- adds the two things the order-creation load path needs and the fuzz seed omits:
--   1. an OPEN shift to attach orders to, and
--   2. the role_permissions the seeded org_admin lacks by default (the test suite
--      grants these explicitly; the boot seeder only grants a conservative set).
-- Idempotent so reseeding a reused DB is safe.

-- An open shift owned by the org_admin (id ...004). order creation as org_admin
-- bypasses the teller-ownership check, so any open shift at the branch works.
INSERT INTO shifts (id, branch_id, teller_id, status, opening_cash)
VALUES ('00000000-0000-0000-0000-000000000020',
        '00000000-0000-0000-0000-000000000002',
        '00000000-0000-0000-0000-000000000004',
        'open', 10000)
ON CONFLICT (id) DO NOTHING;

-- A second priced item so the order mix isn't single-row-hot.
INSERT INTO menu_items (id, org_id, category_id, name, base_price)
VALUES ('00000000-0000-0000-0000-000000000007',
        '00000000-0000-0000-0000-000000000001',
        '00000000-0000-0000-0000-000000000005', 'Load Item 2', 2500)
ON CONFLICT (id) DO NOTHING;

-- Permissions the load scenarios exercise as org_admin. Global role grants
-- (role_permissions has no org scope); ON CONFLICT keeps boot-seeded rows.
INSERT INTO role_permissions (role, resource, action, granted)
VALUES
    ('org_admin', 'menu_items', 'read',   true),
    ('org_admin', 'categories', 'read',   true),
    ('org_admin', 'branches',   'read',   true),
    ('org_admin', 'orders',     'read',   true),
    ('org_admin', 'orders',     'create', true),
    ('org_admin', 'order_items','read',   true),
    ('org_admin', 'reports',    'read',   true)
ON CONFLICT (role, resource, action) DO NOTHING;
