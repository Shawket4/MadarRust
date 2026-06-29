-- Multi-tenant load fixture: `:tenants` fully self-contained orgs, each with its
-- own branch, org-admin, open shift, category, menu item and payment method.
-- Deterministic UUIDs (prefix encodes the entity, last group encodes the index)
-- so scripts/loadtest.sh can mint a per-org token and build the k6 tenant list
-- without reading anything back. Idempotent. Invoke with: psql -v tenants=20 ...
--
-- Different prefix space from scripts/seed_fuzz.sql so the two never collide.
\if :{?tenants} \else \set tenants 20 \endif

INSERT INTO organizations (id, name, slug)
SELECT ('0a000000-0000-0000-0000-'||lpad(g::text,12,'0'))::uuid, 'LT Org '||g, 'lt-org-'||g
FROM generate_series(1, :tenants) g
ON CONFLICT (id) DO NOTHING;

INSERT INTO branches (id, org_id, name, code, latitude, longitude)
SELECT ('0b000000-0000-0000-0000-'||lpad(g::text,12,'0'))::uuid,
       ('0a000000-0000-0000-0000-'||lpad(g::text,12,'0'))::uuid,
       'LT Branch '||g, 'LB'||g, 30.0444, 31.2357
FROM generate_series(1, :tenants) g
ON CONFLICT (id) DO NOTHING;

INSERT INTO users (id, name, role, org_id, password_hash)
SELECT ('0c000000-0000-0000-0000-'||lpad(g::text,12,'0'))::uuid,
       'LT Admin '||g, 'org_admin',
       ('0a000000-0000-0000-0000-'||lpad(g::text,12,'0'))::uuid,
       '$2b$12$fuzzfuzzfuzzfuzzfuzzfuO0000000000000000000000000000000000'
FROM generate_series(1, :tenants) g
ON CONFLICT (id) DO NOTHING;

INSERT INTO user_branch_assignments (user_id, branch_id)
SELECT ('0c000000-0000-0000-0000-'||lpad(g::text,12,'0'))::uuid,
       ('0b000000-0000-0000-0000-'||lpad(g::text,12,'0'))::uuid
FROM generate_series(1, :tenants) g
ON CONFLICT DO NOTHING;

INSERT INTO org_payment_methods (id, org_id, name, color, icon, is_cash)
SELECT ('11000000-0000-0000-0000-'||lpad(g::text,12,'0'))::uuid,
       ('0a000000-0000-0000-0000-'||lpad(g::text,12,'0'))::uuid, 'Cash', '#22c55e', 'cash', true
FROM generate_series(1, :tenants) g
ON CONFLICT (id) DO NOTHING;

INSERT INTO categories (id, org_id, name)
SELECT ('0e000000-0000-0000-0000-'||lpad(g::text,12,'0'))::uuid,
       ('0a000000-0000-0000-0000-'||lpad(g::text,12,'0'))::uuid, 'LT Cat '||g
FROM generate_series(1, :tenants) g
ON CONFLICT (id) DO NOTHING;

-- One priced item per org (id prefix 0f). k6 derives it from the org index.
INSERT INTO menu_items (id, org_id, category_id, name, base_price)
SELECT ('0f000000-0000-0000-0000-'||lpad(g::text,12,'0'))::uuid,
       ('0a000000-0000-0000-0000-'||lpad(g::text,12,'0'))::uuid,
       ('0e000000-0000-0000-0000-'||lpad(g::text,12,'0'))::uuid, 'LT Item '||g, 1500
FROM generate_series(1, :tenants) g
ON CONFLICT (id) DO NOTHING;

-- One OPEN shift per org (teller = its org-admin). Writes to different orgs hit
-- different shift_ids → different advisory-lock keys → they parallelize.
INSERT INTO shifts (id, branch_id, teller_id, status, opening_cash)
SELECT ('0d000000-0000-0000-0000-'||lpad(g::text,12,'0'))::uuid,
       ('0b000000-0000-0000-0000-'||lpad(g::text,12,'0'))::uuid,
       ('0c000000-0000-0000-0000-'||lpad(g::text,12,'0'))::uuid, 'open', 10000
FROM generate_series(1, :tenants) g
ON CONFLICT (id) DO NOTHING;

-- Global org_admin grants (role_permissions has no org scope).
INSERT INTO role_permissions (role, resource, action, granted) VALUES
    ('org_admin', 'menu_items', 'read',   true),
    ('org_admin', 'categories', 'read',   true),
    ('org_admin', 'branches',   'read',   true),
    ('org_admin', 'orders',     'read',   true),
    ('org_admin', 'orders',     'create', true),
    ('org_admin', 'order_items','read',   true),
    ('org_admin', 'reports',    'read',   true)
ON CONFLICT (role, resource, action) DO NOTHING;
