-- Permissions Phase 2 (PERMISSIONS_ARCHITECTURE.md §3 E, §6 Phase 2).
--
-- The architecture E data model, backfilled from today's grants so every
-- person's effective permissions are EXACTLY what they are now:
--
--   capabilities            the registry (seeded here at spec v1, kept in sync at
--                           boot from madar_authz::CAPS)
--   org_roles               per-org roles; five system roles per org, one per
--                           legacy role kind; custom roles later
--   org_role_grants         a role's capabilities (+ limits)
--   role_assignments        a person holds a role everywhere or at branches
--   role_assignment_branches
--   user_overrides          per-person allow / deny, optionally per branch
--   org_capability_policy   per-org "ask a manager" per capability
--   authz_epoch             bumped on any change, for cache invalidation
--   users.is_owner
--
-- While the old endpoints still write `role_permissions`, `permissions`,
-- `users.role` and `user_branch_assignments`, triggers here mirror every such
-- write into the new tables (no path escapes). The server keeps SERVING the
-- legacy decision and logs any divergence (shadow mode) until Phase 3.
--
-- Mapping: a legacy cell `resource:action` maps to exactly one capability
-- (capabilities.legacy_*). A role holds a capability when its global
-- role_permissions row is granted. Capabilities with no legacy cell come from
-- the spec's role defaults. Core capabilities are not stored; the evaluator adds
-- them for the role kind.

-- ── Registry ────────────────────────────────────────────────────────────────
CREATE TABLE capabilities (
    id              smallint PRIMARY KEY,
    key             text NOT NULL UNIQUE,
    legacy_resource text NULL,
    legacy_action   text NULL,
    tier            text NOT NULL,
    defaults        text NOT NULL DEFAULT '',
    core            text NOT NULL DEFAULT '',
    approval        boolean NOT NULL DEFAULT false,
    protected       boolean NOT NULL DEFAULT false,
    spec_version    integer NOT NULL DEFAULT 1,
    deprecated_at   timestamptz NULL,
    UNIQUE (legacy_resource, legacy_action)
);
ALTER TABLE capabilities ENABLE ROW LEVEL SECURITY;
CREATE POLICY read_all ON capabilities FOR SELECT USING (true);
GRANT SELECT ON TABLE capabilities TO madar_app;

INSERT INTO capabilities (id, key, legacy_resource, legacy_action, tier, defaults, core, approval, protected) VALUES
    (1, 'platform.orgs.create', 'orgs', 'create', 'legacy', 'o', '', false, false),
    (2, 'org.settings.read', 'orgs', 'read', 'advanced', 'o', '', false, true),
    (3, 'org.settings.edit', 'orgs', 'update', 'advanced', 'o', '', false, true),
    (4, 'platform.orgs.delete', 'orgs', 'delete', 'legacy', 'o', '', false, false),
    (5, 'branches.create', 'branches', 'create', 'advanced', 'o', '', false, false),
    (6, 'branches.read', 'branches', 'read', 'core', 'omtw', 'mtw', false, false),
    (7, 'branches.edit', 'branches', 'update', 'configurable', 'o', '', false, false),
    (8, 'branches.delete', 'branches', 'delete', 'advanced', 'o', '', false, false),
    (9, 'staff.users.create', 'users', 'create', 'configurable', 'om', '', false, true),
    (10, 'staff.users.read', 'users', 'read', 'configurable', 'om', '', false, true),
    (11, 'staff.users.edit', 'users', 'update', 'configurable', 'om', '', false, true),
    (12, 'staff.users.delete', 'users', 'delete', 'configurable', 'o', '', false, true),
    (13, 'menu.categories.create', 'categories', 'create', 'advanced', 'o', '', false, false),
    (14, 'menu.categories.read', 'categories', 'read', 'core', 'omtw', 'mtw', false, false),
    (15, 'menu.categories.edit', 'categories', 'update', 'advanced', 'o', '', false, false),
    (16, 'menu.categories.delete', 'categories', 'delete', 'advanced', 'o', '', false, false),
    (17, 'menu.items.create', 'menu_items', 'create', 'configurable', 'o', '', false, false),
    (18, 'menu.items.read', 'menu_items', 'read', 'core', 'omtw', 'mtw', false, false),
    (19, 'menu.items.edit', 'menu_items', 'update', 'configurable', 'o', '', false, false),
    (20, 'menu.items.delete', 'menu_items', 'delete', 'advanced', 'o', '', false, false),
    (21, 'legacy.addon_groups.create', 'addon_groups', 'create', 'legacy', 'o', '', false, false),
    (22, 'legacy.addon_groups.read', 'addon_groups', 'read', 'legacy', 'omtw', '', false, false),
    (23, 'legacy.addon_groups.update', 'addon_groups', 'update', 'legacy', 'o', '', false, false),
    (24, 'legacy.addon_groups.delete', 'addon_groups', 'delete', 'legacy', 'o', '', false, false),
    (25, 'legacy.addon_items.create', 'addon_items', 'create', 'legacy', 'o', '', false, false),
    (26, 'menu.addons.read', 'addon_items', 'read', 'core', 'omtw', 'mtw', false, false),
    (27, 'menu.addons.edit', 'addon_items', 'update', 'advanced', 'o', '', false, false),
    (28, 'legacy.addon_items.delete', 'addon_items', 'delete', 'legacy', 'o', '', false, false),
    (29, 'recipes.create', 'recipes', 'create', 'advanced', 'o', '', false, false),
    (30, 'recipes.read', 'recipes', 'read', 'configurable', 'om', '', false, false),
    (31, 'recipes.edit', 'recipes', 'update', 'advanced', 'o', '', false, false),
    (32, 'recipes.delete', 'recipes', 'delete', 'advanced', 'o', '', false, false),
    (33, 'inventory.items.create', 'inventory', 'create', 'advanced', 'o', '', false, false),
    (34, 'inventory.read', 'inventory', 'read', 'configurable', 'omt', '', false, false),
    (35, 'inventory.adjust', 'inventory', 'update', 'configurable', 'om', '', false, false),
    (36, 'inventory.items.delete', 'inventory', 'delete', 'advanced', 'o', '', false, false),
    (37, 'legacy.inventory_adjustments.create', 'inventory_adjustments', 'create', 'legacy', '', '', false, false),
    (38, 'legacy.inventory_adjustments.read', 'inventory_adjustments', 'read', 'legacy', '', '', false, false),
    (39, 'legacy.inventory_adjustments.update', 'inventory_adjustments', 'update', 'legacy', '', '', false, false),
    (40, 'legacy.inventory_adjustments.delete', 'inventory_adjustments', 'delete', 'legacy', '', '', false, false),
    (41, 'inventory.transfers.create', 'inventory_transfers', 'create', 'configurable', 'om', '', false, false),
    (42, 'inventory.transfers.read', 'inventory_transfers', 'read', 'configurable', 'om', '', false, false),
    (43, 'inventory.transfers.edit', 'inventory_transfers', 'update', 'configurable', 'om', '', false, false),
    (44, 'inventory.transfers.delete', 'inventory_transfers', 'delete', 'advanced', 'o', '', false, false),
    (45, 'inventory.counts.create', 'stocktakes', 'create', 'configurable', 'om', '', false, false),
    (46, 'inventory.counts.read', 'stocktakes', 'read', 'configurable', 'om', '', false, false),
    (47, 'inventory.counts.edit', 'stocktakes', 'update', 'configurable', 'om', '', false, false),
    (48, 'legacy.stocktakes.delete', 'stocktakes', 'delete', 'legacy', 'o', '', false, false),
    (49, 'inventory.waste.record', 'inventory_waste', 'create', 'configurable', 'om', '', true, false),
    (50, 'inventory.waste.read', 'inventory_waste', 'read', 'configurable', 'om', '', false, false),
    (51, 'legacy.inventory_waste.update', 'inventory_waste', 'update', 'legacy', 'o', '', false, false),
    (52, 'legacy.inventory_waste.delete', 'inventory_waste', 'delete', 'legacy', 'o', '', false, false),
    (53, 'purchasing.suppliers.create', 'suppliers', 'create', 'advanced', 'o', '', false, false),
    (54, 'purchasing.suppliers.read', 'suppliers', 'read', 'configurable', 'om', '', false, false),
    (55, 'purchasing.suppliers.edit', 'suppliers', 'update', 'advanced', 'o', '', false, false),
    (56, 'purchasing.suppliers.delete', 'suppliers', 'delete', 'advanced', 'o', '', false, false),
    (57, 'purchasing.orders.create', 'purchase_orders', 'create', 'configurable', 'om', '', false, false),
    (58, 'purchasing.orders.read', 'purchase_orders', 'read', 'configurable', 'om', '', false, false),
    (59, 'purchasing.orders.edit', 'purchase_orders', 'update', 'configurable', 'om', '', false, false),
    (60, 'legacy.purchase_orders.delete', 'purchase_orders', 'delete', 'legacy', 'o', '', false, false),
    (61, 'orders.create', 'orders', 'create', 'core', 'omt', 'mt', false, false),
    (62, 'orders.read', 'orders', 'read', 'core', 'omt', 'mt', false, false),
    (63, 'legacy.orders.update', 'orders', 'update', 'legacy', 'omt', '', false, false),
    (64, 'orders.void', 'orders', 'delete', 'configurable', 'omt', '', true, false),
    (65, 'legacy.order_items.create', 'order_items', 'create', 'legacy', 'omt', '', false, false),
    (66, 'legacy.order_items.read', 'order_items', 'read', 'legacy', 'omt', '', false, false),
    (67, 'legacy.order_items.update', 'order_items', 'update', 'legacy', 'om', '', false, false),
    (68, 'legacy.order_items.delete', 'order_items', 'delete', 'legacy', 'o', '', false, false),
    (69, 'refunds.create', 'refunds', 'create', 'configurable', 'omt', '', true, false),
    (70, 'refunds.read', 'refunds', 'read', 'configurable', 'omt', '', false, false),
    (71, 'legacy.refunds.update', 'refunds', 'update', 'legacy', '', '', false, false),
    (72, 'legacy.refunds.delete', 'refunds', 'delete', 'legacy', '', '', false, false),
    (73, 'payments.take', 'payments', 'create', 'core', 'omt', 'mt', false, false),
    (74, 'legacy.payments.read', 'payments', 'read', 'legacy', 'omt', '', false, false),
    (75, 'legacy.payments.update', 'payments', 'update', 'legacy', 'om', '', false, false),
    (76, 'legacy.payments.delete', 'payments', 'delete', 'legacy', 'o', '', false, false),
    (77, 'payment_methods.create', 'payment_methods', 'create', 'advanced', 'o', '', false, false),
    (78, 'payment_methods.read', 'payment_methods', 'read', 'core', 'omt', 'mt', false, false),
    (79, 'payment_methods.edit', 'payment_methods', 'update', 'configurable', 'o', '', false, false),
    (80, 'legacy.payment_methods.delete', 'payment_methods', 'delete', 'legacy', 'o', '', false, false),
    (81, 'till.open', 'tills', 'create', 'core', 'omt', 't', false, false),
    (82, 'till.read', 'tills', 'read', 'core', 'omt', 'mt', false, false),
    (83, 'till.operate', 'tills', 'update', 'core', 'omt', 't', false, false),
    (84, 'till.delete', 'tills', 'delete', 'advanced', 'o', '', false, false),
    (85, 'legacy.soft_serve_batches.create', 'soft_serve_batches', 'create', 'legacy', 'om', '', false, false),
    (86, 'legacy.soft_serve_batches.read', 'soft_serve_batches', 'read', 'legacy', 'om', '', false, false),
    (87, 'legacy.soft_serve_batches.update', 'soft_serve_batches', 'update', 'legacy', 'om', '', false, false),
    (88, 'legacy.soft_serve_batches.delete', 'soft_serve_batches', 'delete', 'legacy', 'o', '', false, false),
    (89, 'discounts.create', 'discounts', 'create', 'advanced', 'o', '', false, false),
    (90, 'discounts.read', 'discounts', 'read', 'core', 'omtw', 'mtw', false, false),
    (91, 'discounts.edit', 'discounts', 'update', 'advanced', 'om', '', false, false),
    (92, 'discounts.delete', 'discounts', 'delete', 'advanced', 'o', '', false, false),
    (93, 'legacy.reports.create', 'reports', 'create', 'legacy', '', '', false, false),
    (94, 'reports.read', 'reports', 'read', 'configurable', 'om', '', false, false),
    (95, 'legacy.reports.update', 'reports', 'update', 'legacy', '', '', false, false),
    (96, 'legacy.reports.delete', 'reports', 'delete', 'legacy', '', '', false, false),
    (97, 'legacy.permissions.create', 'permissions', 'create', 'legacy', 'o', '', false, false),
    (98, 'staff.permissions.read', 'permissions', 'read', 'configurable', 'o', '', false, true),
    (99, 'staff.permissions.edit', 'permissions', 'update', 'configurable', 'o', '', false, true),
    (100, 'staff.permissions.reset', 'permissions', 'delete', 'advanced', 'o', '', false, true),
    (101, 'kitchen.stations.create', 'kitchen_stations', 'create', 'advanced', 'om', '', false, false),
    (102, 'kitchen.stations.read', 'kitchen_stations', 'read', 'core', 'omk', 'mk', false, false),
    (103, 'kitchen.stations.edit', 'kitchen_stations', 'update', 'configurable', 'om', '', false, false),
    (104, 'kitchen.stations.delete', 'kitchen_stations', 'delete', 'advanced', 'om', '', false, false),
    (105, 'legacy.kitchen_orders.create', 'kitchen_orders', 'create', 'legacy', '', '', false, false),
    (106, 'kitchen.display.read', 'kitchen_orders', 'read', 'core', 'omtwk', 'mtwk', false, false),
    (107, 'kitchen.bump', 'kitchen_orders', 'update', 'core', 'omtk', 'k', false, false),
    (108, 'legacy.kitchen_orders.delete', 'kitchen_orders', 'delete', 'legacy', '', '', false, false),
    (109, 'tickets.open', 'open_tickets', 'create', 'core', 'omtw', 'tw', false, false),
    (110, 'tickets.read', 'open_tickets', 'read', 'core', 'omtw', 'mtw', false, false),
    (111, 'tickets.edit', 'open_tickets', 'update', 'core', 'omtw', 'tw', false, false),
    (112, 'tickets.void', 'open_tickets', 'delete', 'configurable', 'omtw', '', true, false),
    (113, 'floor.layout.create', 'floor_plan', 'create', 'advanced', 'om', '', false, false),
    (114, 'floor.layout.read', 'floor_plan', 'read', 'core', 'omtw', 'mtw', false, false),
    (115, 'floor.layout.edit', 'floor_plan', 'update', 'configurable', 'om', '', false, false),
    (116, 'floor.layout.delete', 'floor_plan', 'delete', 'advanced', 'om', '', false, false),
    (117, 'floor.transfers.create', 'table_transfers', 'create', 'core', 'omtw', 'tw', false, false),
    (118, 'floor.transfers.read', 'table_transfers', 'read', 'core', 'omtw', 'mtw', false, false),
    (119, 'floor.transfers.edit', 'table_transfers', 'update', 'core', 'omtw', 'tw', false, false),
    (120, 'legacy.table_transfers.delete', 'table_transfers', 'delete', 'legacy', 'o', '', false, false),
    (121, 'bookings.create', 'bookings', 'create', 'configurable', 'om', '', false, false),
    (122, 'bookings.read', 'bookings', 'read', 'configurable', 'omtw', '', false, false),
    (123, 'bookings.edit', 'bookings', 'update', 'configurable', 'omtw', '', false, false),
    (124, 'legacy.bookings.delete', 'bookings', 'delete', 'legacy', 'om', '', false, false),
    (125, 'legacy.loyalty.create', 'loyalty', 'create', 'legacy', 'om', '', false, false),
    (126, 'loyalty.read', 'loyalty', 'read', 'configurable', 'omt', '', false, false),
    (127, 'loyalty.use', 'loyalty', 'update', 'configurable', 'omt', '', false, false),
    (128, 'legacy.loyalty.delete', 'loyalty', 'delete', 'legacy', 'o', '', false, false),
    (129, 'legacy.delivery_orders.create', 'delivery_orders', 'create', 'legacy', 'o', '', false, false),
    (130, 'delivery.orders.read', 'delivery_orders', 'read', 'configurable', 'omt', '', false, false),
    (131, 'delivery.orders.manage', 'delivery_orders', 'update', 'configurable', 'omt', '', false, false),
    (132, 'legacy.delivery_orders.delete', 'delivery_orders', 'delete', 'legacy', 'o', '', false, false),
    (133, 'delivery.settings.create', 'delivery_settings', 'create', 'advanced', 'om', '', false, false),
    (134, 'delivery.settings.read', 'delivery_settings', 'read', 'configurable', 'om', '', false, false),
    (135, 'delivery.settings.edit', 'delivery_settings', 'update', 'configurable', 'om', '', false, false),
    (136, 'delivery.settings.delete', 'delivery_settings', 'delete', 'advanced', 'om', '', false, false),
    (137, 'hr.staff.create', 'staff', 'create', 'advanced', 'o', '', false, false),
    (138, 'hr.staff.read', 'staff', 'read', 'configurable', 'om', '', false, false),
    (139, 'hr.staff.edit', 'staff', 'update', 'advanced', 'o', '', false, false),
    (140, 'hr.staff.delete', 'staff', 'delete', 'advanced', 'o', '', false, false),
    (141, 'hr.schedule.create', 'work_shifts', 'create', 'configurable', 'o', '', false, false),
    (142, 'hr.schedule.read', 'work_shifts', 'read', 'configurable', 'om', '', false, false),
    (143, 'hr.schedule.edit', 'work_shifts', 'update', 'configurable', 'om', '', false, false),
    (144, 'hr.schedule.delete', 'work_shifts', 'delete', 'advanced', 'o', '', false, false),
    (145, 'hr.attendance.create', 'attendance', 'create', 'advanced', 'om', '', false, false),
    (146, 'hr.attendance.read', 'attendance', 'read', 'configurable', 'om', '', false, false),
    (147, 'hr.attendance.edit', 'attendance', 'update', 'configurable', 'om', '', false, false),
    (148, 'hr.attendance.delete', 'attendance', 'delete', 'advanced', 'o', '', false, false),
    (149, 'hr.leave.create', 'leave', 'create', 'advanced', 'o', '', false, false),
    (150, 'hr.leave.read', 'leave', 'read', 'configurable', 'om', '', false, false),
    (151, 'hr.leave.edit', 'leave', 'update', 'configurable', 'om', '', false, false),
    (152, 'hr.leave.delete', 'leave', 'delete', 'advanced', 'o', '', false, false),
    (153, 'hr.payroll.create', 'payroll', 'create', 'advanced', 'o', '', false, false),
    (154, 'hr.payroll.read', 'payroll', 'read', 'configurable', 'o', '', false, false),
    (155, 'hr.payroll.edit', 'payroll', 'update', 'advanced', 'o', '', false, false),
    (156, 'hr.payroll.delete', 'payroll', 'delete', 'advanced', 'o', '', false, false),
    (157, 'orders.service_charge.waive', 'orders', 'waive_service', 'configurable', 'om', '', true, false),
    (200, 'pos.sign_in', NULL, NULL, 'core', 'omtwk', 'tw', false, false),
    (201, 'till.force_close', NULL, NULL, 'configurable', 'om', '', false, false),
    (202, 'till.read.branch', NULL, NULL, 'configurable', 'om', '', false, false),
    (203, 'till.cash_spot_check', NULL, NULL, 'configurable', '', '', true, false),
    (204, 'orders.discount.preset', NULL, NULL, 'configurable', 'omtw', '', true, false),
    (205, 'orders.discount.manual_amount', NULL, NULL, 'configurable', 'omt', '', true, false),
    (206, 'orders.discount.manual_percent', NULL, NULL, 'configurable', 'omt', '', true, false),
    (207, 'reports.pos_metrics', NULL, NULL, 'configurable', 'om', '', false, false),
    (208, 'customers.attach', NULL, NULL, 'configurable', 'omtw', '', false, false),
    (209, 'customers.view', NULL, NULL, 'configurable', 'omt', '', false, false),
    (210, 'customers.create', NULL, NULL, 'configurable', 'omtw', '', false, false),
    (211, 'customers.edit', NULL, NULL, 'configurable', 'om', '', false, false),
    (212, 'staff.roles.manage', NULL, NULL, 'configurable', 'o', '', false, true),
    (213, 'staff.owners.manage', NULL, NULL, 'advanced', 'o', '', false, true),
    (214, 'approvals.review', NULL, NULL, 'configurable', 'o', '', false, false);

-- ── Model ───────────────────────────────────────────────────────────────────
ALTER TABLE users ADD COLUMN IF NOT EXISTS is_owner boolean NOT NULL DEFAULT false;

CREATE TABLE org_roles (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id           uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    key              text NOT NULL CHECK (key ~ '^[a-z0-9_]{1,64}$'),
    name_en          text NOT NULL CHECK (length(trim(name_en)) BETWEEN 1 AND 80),
    name_ar          text NOT NULL CHECK (length(trim(name_ar)) BETWEEN 1 AND 80),
    -- What the role behaves like: core grants, and the role older tablets see.
    kind             user_role NOT NULL CHECK (kind <> 'super_admin'),
    template_key     text NULL,
    template_version integer NULL,
    is_system        boolean NOT NULL DEFAULT false,
    created_by       uuid NULL,
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    deleted_at       timestamptz NULL
);
CREATE UNIQUE INDEX org_roles_org_key ON org_roles (org_id, key) WHERE deleted_at IS NULL;

CREATE TABLE org_role_grants (
    org_role_id      uuid NOT NULL REFERENCES org_roles(id) ON DELETE CASCADE,
    org_id           uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    capability_id    smallint NOT NULL REFERENCES capabilities(id),
    limits           jsonb NOT NULL DEFAULT '{}',
    -- 'template': written by a template or this migration; 'custom': the owner edited it.
    source           text NOT NULL DEFAULT 'template' CHECK (source IN ('template', 'custom')),
    template_version integer NULL,
    updated_by       uuid NULL,
    updated_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (org_role_id, capability_id)
);

CREATE TABLE role_assignments (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id       uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id      uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    org_role_id  uuid NOT NULL REFERENCES org_roles(id) ON DELETE CASCADE,
    all_branches boolean NOT NULL DEFAULT false,
    valid_from   timestamptz NULL,
    valid_to     timestamptz NULL,
    granted_by   uuid NULL,
    reason       text NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    revoked_at   timestamptz NULL
);
CREATE UNIQUE INDEX role_assignments_live ON role_assignments (user_id, org_role_id) WHERE revoked_at IS NULL;
CREATE INDEX role_assignments_user ON role_assignments (user_id) WHERE revoked_at IS NULL;

CREATE TABLE role_assignment_branches (
    assignment_id uuid NOT NULL REFERENCES role_assignments(id) ON DELETE CASCADE,
    branch_id     uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    org_id        uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    PRIMARY KEY (assignment_id, branch_id)
);

CREATE TABLE user_overrides (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id        uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id       uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    capability_id smallint NOT NULL REFERENCES capabilities(id),
    effect        text NOT NULL CHECK (effect IN ('allow', 'deny')),
    branch_id     uuid NULL REFERENCES branches(id) ON DELETE CASCADE,
    limits        jsonb NULL,
    valid_to      timestamptz NULL,
    reason        text NULL,
    granted_by    uuid NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    revoked_at    timestamptz NULL
);
CREATE UNIQUE INDEX user_overrides_live ON user_overrides
    (user_id, capability_id, COALESCE(branch_id, '00000000-0000-0000-0000-000000000000'::uuid))
    WHERE revoked_at IS NULL;

CREATE TABLE org_capability_policy (
    org_id        uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    capability_id smallint NOT NULL REFERENCES capabilities(id),
    -- A person without the capability sees "ask a manager" instead of nothing.
    ask_manager   boolean NOT NULL DEFAULT false,
    updated_by    uuid NULL,
    updated_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (org_id, capability_id)
);

CREATE TABLE authz_epoch (
    org_id uuid PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    epoch  bigint NOT NULL DEFAULT 1
);

DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['org_roles', 'org_role_grants', 'role_assignments',
                             'role_assignment_branches', 'user_overrides',
                             'org_capability_policy', 'authz_epoch'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I FOR ALL
                        USING (org_id = (SELECT current_setting(''app.org_id'', true)::uuid))', t);
        EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE %I TO madar_app', t);
    END LOOP;
END $$;

-- ── Helpers ─────────────────────────────────────────────────────────────────
CREATE OR REPLACE FUNCTION authz_kind_letter(p_kind text) RETURNS text
LANGUAGE sql IMMUTABLE AS $$
    SELECT CASE p_kind WHEN 'org_admin' THEN 'o' WHEN 'branch_manager' THEN 'm'
                       WHEN 'teller' THEN 't' WHEN 'waiter' THEN 'w' WHEN 'kitchen' THEN 'k' END
$$;

CREATE OR REPLACE FUNCTION authz_bump_epoch(p_org uuid) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF p_org IS NULL THEN RETURN; END IF;
    INSERT INTO authz_epoch (org_id, epoch) VALUES (p_org, 1)
    ON CONFLICT (org_id) DO UPDATE SET epoch = authz_epoch.epoch + 1;
END $$;

-- The five system roles of an org, seeded from today's global defaults.
CREATE OR REPLACE FUNCTION authz_ensure_org_roles(p_org uuid) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    k      text;
    v_role uuid;
    names  jsonb := '{"org_admin": ["Owner", "المالك"], "branch_manager": ["Branch manager", "مدير الفرع"],
                      "teller": ["Cashier", "كاشير"], "waiter": ["Waiter", "نادل"], "kitchen": ["Kitchen", "المطبخ"]}';
BEGIN
    FOREACH k IN ARRAY ARRAY['org_admin', 'branch_manager', 'teller', 'waiter', 'kitchen'] LOOP
        SELECT id INTO v_role FROM org_roles
         WHERE org_id = p_org AND key = k AND deleted_at IS NULL;
        IF v_role IS NULL THEN
            INSERT INTO org_roles (org_id, key, name_en, name_ar, kind, template_key, template_version, is_system)
            VALUES (p_org, k, names -> k ->> 0, names -> k ->> 1, k::user_role, 'legacy', 0, true)
            RETURNING id INTO v_role;
            INSERT INTO org_role_grants (org_role_id, org_id, capability_id, source, template_version)
            SELECT v_role, p_org, c.id, 'template', 0
              FROM capabilities c
             WHERE (c.legacy_resource IS NOT NULL AND EXISTS (
                        SELECT 1 FROM role_permissions rp
                         WHERE rp.role = k::user_role AND rp.granted
                           AND rp.resource::text = c.legacy_resource
                           AND rp.action::text = c.legacy_action))
                OR (c.legacy_resource IS NULL AND position(authz_kind_letter(k) IN c.defaults) > 0);
        END IF;
    END LOOP;
END $$;

-- A person's system-role assignment follows users.role and their branches.
CREATE OR REPLACE FUNCTION authz_sync_user(p_user uuid) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    u        users%ROWTYPE;
    v_role   uuid;
    v_assign uuid;
BEGIN
    SELECT * INTO u FROM users WHERE id = p_user;
    IF NOT FOUND OR u.org_id IS NULL OR u.role = 'super_admin' OR u.is_guest_principal
       OR u.deleted_at IS NOT NULL THEN
        UPDATE role_assignments SET revoked_at = now()
         WHERE user_id = p_user AND revoked_at IS NULL;
        RETURN;
    END IF;
    PERFORM authz_ensure_org_roles(u.org_id);
    SELECT id INTO v_role FROM org_roles
     WHERE org_id = u.org_id AND key = u.role::text AND is_system AND deleted_at IS NULL;

    UPDATE role_assignments ra SET revoked_at = now()
      FROM org_roles r
     WHERE ra.org_role_id = r.id AND r.is_system AND ra.user_id = p_user
       AND ra.revoked_at IS NULL AND ra.org_role_id <> v_role;

    SELECT id INTO v_assign FROM role_assignments
     WHERE user_id = p_user AND org_role_id = v_role AND revoked_at IS NULL;
    IF v_assign IS NULL THEN
        INSERT INTO role_assignments (org_id, user_id, org_role_id, all_branches, reason)
        VALUES (u.org_id, p_user, v_role, u.role <> 'branch_manager', 'follows the account role')
        RETURNING id INTO v_assign;
    ELSE
        UPDATE role_assignments SET all_branches = (u.role <> 'branch_manager')
         WHERE id = v_assign AND all_branches IS DISTINCT FROM (u.role <> 'branch_manager');
    END IF;

    DELETE FROM role_assignment_branches rab
     WHERE rab.assignment_id = v_assign
       AND NOT EXISTS (SELECT 1 FROM user_branch_assignments a
                        WHERE a.user_id = p_user AND a.branch_id = rab.branch_id);
    INSERT INTO role_assignment_branches (assignment_id, branch_id, org_id)
    SELECT v_assign, a.branch_id, u.org_id
      FROM user_branch_assignments a JOIN branches b ON b.id = a.branch_id AND b.org_id = u.org_id
     WHERE a.user_id = p_user
    ON CONFLICT DO NOTHING;
END $$;

-- A legacy per-user override follows `permissions`.
CREATE OR REPLACE FUNCTION authz_sync_override(p_user uuid, p_resource text, p_action text) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    v_cap     smallint;
    v_org     uuid;
    v_granted boolean;
BEGIN
    SELECT id INTO v_cap FROM capabilities
     WHERE legacy_resource = p_resource AND legacy_action = p_action;
    SELECT org_id INTO v_org FROM users WHERE id = p_user;
    IF v_cap IS NULL OR v_org IS NULL THEN RETURN; END IF;
    UPDATE user_overrides SET revoked_at = now()
     WHERE user_id = p_user AND capability_id = v_cap AND branch_id IS NULL AND revoked_at IS NULL;
    SELECT granted INTO v_granted FROM permissions
     WHERE user_id = p_user AND resource::text = p_resource AND action::text = p_action;
    IF FOUND THEN
        INSERT INTO user_overrides (org_id, user_id, capability_id, effect, reason)
        VALUES (v_org, p_user, v_cap, CASE WHEN v_granted THEN 'allow' ELSE 'deny' END,
                'legacy override');
    END IF;
END $$;

-- ── Backfill (before the triggers exist) ────────────────────────────────────
SELECT authz_ensure_org_roles(id) FROM organizations;

UPDATE users SET is_owner = (role = 'org_admin') WHERE is_owner IS DISTINCT FROM (role = 'org_admin');

SELECT authz_sync_user(id) FROM users
 WHERE org_id IS NOT NULL AND deleted_at IS NULL AND role <> 'super_admin';

INSERT INTO user_overrides (org_id, user_id, capability_id, effect, reason)
SELECT u.org_id, p.user_id, c.id, CASE WHEN p.granted THEN 'allow' ELSE 'deny' END,
       'migrated from legacy override'
  FROM permissions p
  JOIN users u ON u.id = p.user_id AND u.deleted_at IS NULL AND u.org_id IS NOT NULL
  JOIN capabilities c ON c.legacy_resource = p.resource::text AND c.legacy_action = p.action::text;

INSERT INTO authz_epoch (org_id, epoch) SELECT id, 1 FROM organizations
ON CONFLICT DO NOTHING;

-- ── Mirror triggers: the legacy writers keep the new model current ──────────
CREATE OR REPLACE FUNCTION authz_users_before() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    NEW.is_owner := (NEW.role = 'org_admin');
    RETURN NEW;
END $$;
CREATE TRIGGER authz_users_owner BEFORE INSERT OR UPDATE OF role ON users
    FOR EACH ROW EXECUTE FUNCTION authz_users_before();

CREATE OR REPLACE FUNCTION authz_users_after() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    PERFORM authz_sync_user(NEW.id);
    PERFORM authz_bump_epoch(NEW.org_id);
    RETURN NULL;
END $$;
CREATE TRIGGER authz_users_sync AFTER INSERT OR UPDATE OF role, org_id, deleted_at, is_active ON users
    FOR EACH ROW EXECUTE FUNCTION authz_users_after();

CREATE OR REPLACE FUNCTION authz_branch_assignment_after() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE v_user uuid := CASE WHEN TG_OP = 'DELETE' THEN OLD.user_id ELSE NEW.user_id END;
BEGIN
    PERFORM authz_sync_user(v_user);
    PERFORM authz_bump_epoch((SELECT org_id FROM users WHERE id = v_user));
    RETURN NULL;
END $$;
CREATE TRIGGER authz_branch_assignment_sync AFTER INSERT OR DELETE ON user_branch_assignments
    FOR EACH ROW EXECUTE FUNCTION authz_branch_assignment_after();

CREATE OR REPLACE FUNCTION authz_permissions_after() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        PERFORM authz_sync_override(OLD.user_id, OLD.resource::text, OLD.action::text);
    END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        PERFORM authz_sync_override(NEW.user_id, NEW.resource::text, NEW.action::text);
    END IF;
    PERFORM authz_bump_epoch((SELECT org_id FROM users
                               WHERE id = CASE WHEN TG_OP = 'DELETE' THEN OLD.user_id ELSE NEW.user_id END));
    RETURN NULL;
END $$;
CREATE TRIGGER authz_permissions_sync AFTER INSERT OR UPDATE OR DELETE ON permissions
    FOR EACH ROW EXECUTE FUNCTION authz_permissions_after();

-- A global default change reaches every org's system role, unless that org's
-- owner customised the grant.
CREATE OR REPLACE FUNCTION authz_role_permissions_after() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    r       role_permissions%ROWTYPE := CASE WHEN TG_OP = 'DELETE' THEN OLD ELSE NEW END;
    v_cap   smallint;
    v_grant boolean;
BEGIN
    SELECT id INTO v_cap FROM capabilities
     WHERE legacy_resource = r.resource::text AND legacy_action = r.action::text;
    IF v_cap IS NULL OR r.role = 'super_admin' THEN RETURN NULL; END IF;
    SELECT granted INTO v_grant FROM role_permissions
     WHERE role = r.role AND resource = r.resource AND action = r.action;
    IF COALESCE(v_grant, false) THEN
        INSERT INTO org_role_grants (org_role_id, org_id, capability_id, source, template_version)
        SELECT o.id, o.org_id, v_cap, 'template', 0 FROM org_roles o
         WHERE o.is_system AND o.key = r.role::text AND o.deleted_at IS NULL
        ON CONFLICT DO NOTHING;
    ELSE
        DELETE FROM org_role_grants g USING org_roles o
         WHERE g.org_role_id = o.id AND o.is_system AND o.key = r.role::text
           AND g.capability_id = v_cap AND g.source = 'template';
    END IF;
    UPDATE authz_epoch SET epoch = epoch + 1;
    RETURN NULL;
END $$;
CREATE TRIGGER authz_role_permissions_sync AFTER INSERT OR UPDATE OR DELETE ON role_permissions
    FOR EACH ROW EXECUTE FUNCTION authz_role_permissions_after();

-- New-model writes bump the epoch and leave grant history.
CREATE OR REPLACE FUNCTION authz_model_after() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    PERFORM authz_bump_epoch(CASE WHEN TG_OP = 'DELETE' THEN OLD.org_id ELSE NEW.org_id END);
    RETURN NULL;
END $$;

CREATE OR REPLACE FUNCTION authz_record_grant_event() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    v_row   jsonb := to_jsonb(CASE WHEN TG_OP = 'DELETE' THEN OLD ELSE NEW END);
    v_org   uuid := NULLIF(v_row ->> 'org_id', '')::uuid;
    v_actor uuid;
BEGIN
    IF v_org IS NULL AND v_row ? 'user_id' THEN
        SELECT org_id INTO v_org FROM users WHERE id = (v_row ->> 'user_id')::uuid;
    END IF;
    BEGIN
        v_actor := NULLIF(current_setting('app.actor_id', true), '')::uuid;
    EXCEPTION WHEN others THEN
        v_actor := NULL;
    END;
    INSERT INTO authz_grant_events (org_id, table_name, op, actor_id, before, after)
    VALUES (v_org, TG_TABLE_NAME, TG_OP, v_actor,
            CASE WHEN TG_OP IN ('UPDATE', 'DELETE') THEN to_jsonb(OLD) END,
            CASE WHEN TG_OP IN ('INSERT', 'UPDATE') THEN to_jsonb(NEW) END);
    RETURN NULL;
END $$;

DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['org_roles', 'org_role_grants', 'role_assignments',
                             'role_assignment_branches', 'user_overrides', 'org_capability_policy'] LOOP
        EXECUTE format('CREATE TRIGGER authz_epoch_bump AFTER INSERT OR UPDATE OR DELETE ON %I
                        FOR EACH ROW EXECUTE FUNCTION authz_model_after()', t);
        EXECUTE format('CREATE TRIGGER authz_grant_event AFTER INSERT OR UPDATE OR DELETE ON %I
                        FOR EACH ROW EXECUTE FUNCTION authz_record_grant_event()', t);
    END LOOP;
END $$;
