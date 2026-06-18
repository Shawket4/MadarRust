-- Minimal fixture for the API-fuzz harness (scripts/api-fuzz.sh).
--
-- Run ONLY against the throwaway `sufrix_fuzz` database, after `sqlx migrate run`.
-- Fixed UUIDs match src/bin/fuzz_token.rs so the minted JWT resolves to real rows
-- and fuzzed requests reach handler logic instead of bouncing on empty-DB 404s.
-- Idempotent (ON CONFLICT DO NOTHING) so reseeding a reused DB is safe.

-- Organization (currency/tax/timezone/is_active take their defaults).
INSERT INTO organizations (id, name, slug)
VALUES ('00000000-0000-0000-0000-000000000001', 'Fuzz Org', 'fuzz-org')
ON CONFLICT (id) DO NOTHING;

-- Branch with coordinates so delivery-quote has data to work with.
INSERT INTO branches (id, org_id, name, code, latitude, longitude)
VALUES ('00000000-0000-0000-0000-000000000002',
        '00000000-0000-0000-0000-000000000001',
        'Fuzz Branch', 'FZ', 30.0444, 31.2357)
ON CONFLICT (id) DO NOTHING;

-- Users. password_hash is a non-null placeholder (the harness mints tokens
-- directly via fuzz-token, so login is never exercised). The CHECK constraint
-- only requires password_hash OR pin_hash to be present.
INSERT INTO users (id, name, role, org_id, password_hash)
VALUES ('00000000-0000-0000-0000-000000000003', 'Fuzz Super Admin', 'super_admin', NULL,
        '$2b$12$fuzzfuzzfuzzfuzzfuzzfuO0000000000000000000000000000000000')
ON CONFLICT (id) DO NOTHING;

INSERT INTO users (id, name, role, org_id, password_hash)
VALUES ('00000000-0000-0000-0000-000000000004', 'Fuzz Org Admin', 'org_admin',
        '00000000-0000-0000-0000-000000000001',
        '$2b$12$fuzzfuzzfuzzfuzzfuzzfuO0000000000000000000000000000000000')
ON CONFLICT (id) DO NOTHING;

-- Branch assignment so branch-scoped endpoints resolve for the org admin.
INSERT INTO user_branch_assignments (user_id, branch_id)
VALUES ('00000000-0000-0000-0000-000000000004', '00000000-0000-0000-0000-000000000002')
ON CONFLICT DO NOTHING;

-- Payment methods (cash + card).
INSERT INTO org_payment_methods (id, org_id, name, color, icon, is_cash)
VALUES ('00000000-0000-0000-0000-000000000010',
        '00000000-0000-0000-0000-000000000001', 'Cash', '#22c55e', 'cash', true)
ON CONFLICT (id) DO NOTHING;
INSERT INTO org_payment_methods (id, org_id, name, color, icon, is_cash)
VALUES ('00000000-0000-0000-0000-000000000011',
        '00000000-0000-0000-0000-000000000001', 'Card', '#3b82f6', 'card', false)
ON CONFLICT (id) DO NOTHING;

-- One category + one priced menu item so menu/order/costing handlers have a row.
INSERT INTO categories (id, org_id, name)
VALUES ('00000000-0000-0000-0000-000000000005',
        '00000000-0000-0000-0000-000000000001', 'Fuzz Category')
ON CONFLICT (id) DO NOTHING;

INSERT INTO menu_items (id, org_id, category_id, name, base_price)
VALUES ('00000000-0000-0000-0000-000000000006',
        '00000000-0000-0000-0000-000000000001',
        '00000000-0000-0000-0000-000000000005', 'Fuzz Item', 1000)
ON CONFLICT (id) DO NOTHING;
