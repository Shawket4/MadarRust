-- ════════════════════════════════════════════════════════════════════════════
-- Menu / Recipe / Modifier unification — EXPAND phase (Wave 1).
-- ════════════════════════════════════════════════════════════════════════════
-- Strategy: expand/contract (parallel-change). This migration is ADDITIVE ONLY —
-- it creates the new unified tables and leaves EVERY legacy table in place as the
-- live source of truth. The stable-id backfill (bin: backfill-menu-unification)
-- populates the new tables from the old ones; the source-of-truth flip + legacy
-- compat views + handler rewrite land together in Wave 2 (see CONTRACT.md).
-- Nothing here drops, renames, or rewrites existing data or order history.
--
-- Invariants honoured: money is integer piastres; unknown cost stays NULL (never 0);
-- (menu_item_id, size_label) stays resolvable; recipe lines become id-keyed (rename-safe);
-- modifier_options.id is reserved to equal the old addon_items.id / optional_field.id
-- so immutable order-history FKs keep resolving (enforced by the backfill, not the DDL).
-- ════════════════════════════════════════════════════════════════════════════

-- ─────────────────────────────────────────────────────────────────────────────
-- 0. Reconstruct menu_item_addon_overrides (per-item addon ingredient swaps).
--    This table is referenced by src/menu/handlers.rs (list/upsert/delete override
--    endpoints) but was NEVER created by any migration — the endpoints 500 at runtime
--    and the table is absent from every DB dump. We materialize its exact shape here
--    (reconstructed from the handler's SELECT at handlers.rs:1769-1794 and INSERT at
--    :1948-1975) so the schema is self-consistent and the backfill can fold any rows
--    that a manual/out-of-tree DDL may have created into recipe_lines. IF NOT EXISTS
--    keeps this a no-op where such a table already exists.
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS menu_item_addon_overrides (
    id                          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    menu_item_id                UUID NOT NULL REFERENCES menu_items(id) ON DELETE CASCADE,
    addon_item_id               UUID NOT NULL REFERENCES addon_items(id) ON DELETE CASCADE,
    size_label                  TEXT,
    ingredient_name             TEXT NOT NULL,
    org_ingredient_id           UUID REFERENCES org_ingredients(id) ON DELETE SET NULL,
    ingredient_unit             TEXT NOT NULL,
    quantity_used               NUMERIC(12,3) NOT NULL,
    replaces_org_ingredient_id  UUID REFERENCES org_ingredients(id) ON DELETE SET NULL,
    combo_addon_item_id         UUID REFERENCES addon_items(id) ON DELETE SET NULL,
    created_at                  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at                  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
-- Four partial unique indexes mirror the handler's manual 4-way upsert over the two
-- nullable discriminators (size_label, combo_addon_item_id).
CREATE UNIQUE INDEX IF NOT EXISTS miao_uq_size_combo ON menu_item_addon_overrides
    (menu_item_id, addon_item_id, ingredient_name, size_label, combo_addon_item_id)
    WHERE size_label IS NOT NULL AND combo_addon_item_id IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS miao_uq_size ON menu_item_addon_overrides
    (menu_item_id, addon_item_id, ingredient_name, size_label)
    WHERE size_label IS NOT NULL AND combo_addon_item_id IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS miao_uq_combo ON menu_item_addon_overrides
    (menu_item_id, addon_item_id, ingredient_name, combo_addon_item_id)
    WHERE size_label IS NULL AND combo_addon_item_id IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS miao_uq_plain ON menu_item_addon_overrides
    (menu_item_id, addon_item_id, ingredient_name)
    WHERE size_label IS NULL AND combo_addon_item_id IS NULL;
CREATE INDEX IF NOT EXISTS idx_miao_item_addon ON menu_item_addon_overrides (menu_item_id, addon_item_id);

-- ─────────────────────────────────────────────────────────────────────────────
-- 1. menu_item_sizes — authoritative per-item size dictionary.
--    Supersedes item_sizes (which lacks a `sort` column and never had a synthesized
--    'one_size' row for size-less items). KEEP `label` so (menu_item_id, size_label)
--    stays resolvable across costing / reports / menu_advisor / orders. price is the
--    absolute per-size price in piastres (not a delta). The backfill copies item_sizes
--    verbatim (preserving id) and synthesizes a 'one_size' row for every item with none.
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE menu_item_sizes (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    menu_item_id  UUID NOT NULL REFERENCES menu_items(id) ON DELETE CASCADE,
    label         TEXT NOT NULL,
    price         INTEGER NOT NULL,
    sort          INTEGER NOT NULL DEFAULT 0,
    is_active     BOOLEAN NOT NULL DEFAULT TRUE,
    CONSTRAINT menu_item_sizes_price_nonneg CHECK (price >= 0),
    CONSTRAINT menu_item_sizes_item_label_key UNIQUE (menu_item_id, label)
);
CREATE INDEX idx_menu_item_sizes_item ON menu_item_sizes (menu_item_id);

-- ─────────────────────────────────────────────────────────────────────────────
-- 2. modifier_groups — reusable modifier groups.
--    legacy_addon_type preserves the old addon_items.type string, which is BOTH the
--    compat-shim join key AND the milk_type/coffee_type ingredient-swap contract
--    (orders/handlers.rs:2499 maps 'milk_type'->'milk', 'coffee_type'->'coffee_bean').
--    Item-private option sets (from optional fields) become groups with legacy_addon_type NULL.
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE modifier_groups (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id             UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name               TEXT NOT NULL,
    name_translations  JSONB NOT NULL DEFAULT '{}'::jsonb,
    selection_type     TEXT NOT NULL DEFAULT 'multi' CHECK (selection_type IN ('single','multi')),
    min_selections     INTEGER NOT NULL DEFAULT 0,
    max_selections     INTEGER,                 -- NULL = unlimited
    is_required        BOOLEAN NOT NULL DEFAULT FALSE,
    sort               INTEGER NOT NULL DEFAULT 0,
    is_active          BOOLEAN NOT NULL DEFAULT TRUE,
    legacy_addon_type  TEXT,                    -- old addon_items.type; NULL for optional-derived groups
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT modifier_groups_selcount_ck CHECK (min_selections >= 0 AND (max_selections IS NULL OR max_selections >= min_selections))
);
CREATE INDEX idx_modifier_groups_org ON modifier_groups (org_id);
CREATE INDEX idx_modifier_groups_legacy_type ON modifier_groups (org_id, legacy_addon_type)
    WHERE legacy_addon_type IS NOT NULL;

-- ─────────────────────────────────────────────────────────────────────────────
-- 3. modifier_options — UNIFIES addon_items AND menu_item_optional_fields.
--    NON-NEGOTIABLE: each option.id MUST equal the old addon_item.id or
--    optional_field.id it was migrated from, so order_item_addons.addon_item_id and
--    order_item_optionals.optional_field_id (immutable history) keep resolving.
--    Enforced by the backfill (INSERT ... SELECT old.id). legacy_source records which
--    legacy table a row came from so the shim can write-translate order creation.
--    replaces_ingredient_id: for a swap-style option, the org_ingredient it removes.
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE modifier_options (
    id                     UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    group_id               UUID NOT NULL REFERENCES modifier_groups(id) ON DELETE CASCADE,
    name                   TEXT NOT NULL,
    name_translations      JSONB NOT NULL DEFAULT '{}'::jsonb,
    price                  INTEGER NOT NULL DEFAULT 0,   -- piastres
    sort                   INTEGER NOT NULL DEFAULT 0,
    is_default             BOOLEAN NOT NULL DEFAULT FALSE,
    is_active              BOOLEAN NOT NULL DEFAULT TRUE,
    replaces_ingredient_id UUID REFERENCES org_ingredients(id) ON DELETE SET NULL,
    legacy_source          TEXT NOT NULL DEFAULT 'addon' CHECK (legacy_source IN ('addon','optional')),
    created_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT modifier_options_price_nonneg CHECK (price >= 0)
);
CREATE INDEX idx_modifier_options_group ON modifier_options (group_id);

-- ─────────────────────────────────────────────────────────────────────────────
-- 4. menu_item_modifier_groups — attaches a group to an item.
--    REPLACES menu_item_addon_slots (item<->group binding + min/max/required) AND
--    menu_item_allowed_addons (per-item allowlist, now included_option_ids).
--    included_option_ids NULL = offer all of the group's options; else = the subset.
--    *_override columns NULL = inherit the group default.
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE menu_item_modifier_groups (
    id                    UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    menu_item_id          UUID NOT NULL REFERENCES menu_items(id) ON DELETE CASCADE,
    group_id              UUID NOT NULL REFERENCES modifier_groups(id) ON DELETE CASCADE,
    sort                  INTEGER NOT NULL DEFAULT 0,
    min_override          INTEGER,
    max_override          INTEGER,
    is_required_override  BOOLEAN,
    included_option_ids   UUID[],               -- NULL = all options; else allowlist subset
    -- Provenance for the compat shim: which legacy construct produced this attachment,
    -- so the shim can reproject menu_item_addon_slots (origin='slot') separately from
    -- menu_item_allowed_addons (origin='allowlist'). 'options' = per-item optional group.
    legacy_origin         TEXT CHECK (legacy_origin IS NULL OR legacy_origin IN ('slot','allowlist','options')),
    CONSTRAINT menu_item_modifier_groups_item_group_key UNIQUE (menu_item_id, group_id)
);
CREATE INDEX idx_mimg_item ON menu_item_modifier_groups (menu_item_id);
CREATE INDEX idx_mimg_group ON menu_item_modifier_groups (group_id);

-- ─────────────────────────────────────────────────────────────────────────────
-- 5. recipe_lines — id-keyed recipe lines (rename-safe: FK to org_ingredients, not name).
--    REPLACES menu_item_recipes (owner_type='item_size'), addon_item_ingredients and the
--    inline optional recipe (owner_type='modifier_option'), and menu_item_addon_overrides
--    swaps. quantity/unit are the base-unit, yield-normalized values (same as today).
--    owner_id is polymorphic (menu_item_sizes.id | modifier_options.id) — no single FK;
--    integrity is guarded by the backfill + app layer + the (owner_type,owner_id,ingredient) UNIQUE.
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE recipe_lines (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_type     TEXT NOT NULL CHECK (owner_type IN ('item_size','modifier_option')),
    owner_id       UUID NOT NULL,
    ingredient_id  UUID NOT NULL REFERENCES org_ingredients(id) ON DELETE RESTRICT,
    quantity       NUMERIC(12,3) NOT NULL,
    unit           TEXT NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- quantity = 0 is a legitimate SWAP MARKER (milk_type/coffee_type options): it
    -- records which ingredient the option swaps in, with the deducted amount taken from
    -- the base item recipe at order time. Negative quantities are rejected.
    CONSTRAINT recipe_lines_qty_nonneg CHECK (quantity >= 0),
    CONSTRAINT recipe_lines_owner_ingredient_key UNIQUE (owner_type, owner_id, ingredient_id)
);
CREATE INDEX idx_recipe_lines_owner ON recipe_lines (owner_type, owner_id);
CREATE INDEX idx_recipe_lines_ingredient ON recipe_lines (ingredient_id);

-- ─────────────────────────────────────────────────────────────────────────────
-- 6. menu_price_overrides — one table merging the five legacy override tables:
--    branch_menu_overrides, branch_menu_size_overrides, branch_addon_overrides,
--    branch_channel_menu_overrides, branch_channel_addon_overrides.
--    target_type='menu_item_size' -> target_id is menu_item_sizes.id (per-size branch
--    availability is now expressible, unlike branch_menu_size_overrides which had none).
--    target_type='modifier_option' -> target_id is modifier_options.id.
--    Resolution order (documented in CONTRACT.md): most specific scope wins —
--    branch_channel > branch > (channel) > catalog default (menu_item_sizes.price /
--    modifier_options.price), first non-NULL wins per field, independently for price
--    and is_available.
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE menu_price_overrides (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    scope         TEXT NOT NULL CHECK (scope IN ('branch','channel','branch_channel')),
    branch_id     UUID REFERENCES branches(id) ON DELETE CASCADE,
    channel       delivery_channel,
    target_type   TEXT NOT NULL CHECK (target_type IN ('menu_item_size','modifier_option')),
    target_id     UUID NOT NULL,
    price         INTEGER,                      -- piastres; NULL = inherit
    is_available  BOOLEAN,                      -- NULL = inherit
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT mpo_scope_shape_ck CHECK (
        (scope = 'branch'         AND branch_id IS NOT NULL AND channel IS NULL) OR
        (scope = 'channel'        AND branch_id IS NULL     AND channel IS NOT NULL) OR
        (scope = 'branch_channel' AND branch_id IS NOT NULL AND channel IS NOT NULL)
    ),
    CONSTRAINT mpo_price_nonneg_ck CHECK (price IS NULL OR price >= 0),
    CONSTRAINT mpo_not_empty_ck CHECK (price IS NOT NULL OR is_available IS NOT NULL)
);
-- One unique key per scope shape. Partial indexes over only the columns that are
-- NOT NULL for that scope (guaranteed by mpo_scope_shape_ck) — all IMMUTABLE, no enum
-- casts, and NULL-safe within each scope. These are the natural upsert keys.
CREATE UNIQUE INDEX menu_price_overrides_branch_uq ON menu_price_overrides
    (target_type, target_id, branch_id) WHERE scope = 'branch';
CREATE UNIQUE INDEX menu_price_overrides_channel_uq ON menu_price_overrides
    (target_type, target_id, channel) WHERE scope = 'channel';
CREATE UNIQUE INDEX menu_price_overrides_bc_uq ON menu_price_overrides
    (target_type, target_id, branch_id, channel) WHERE scope = 'branch_channel';
CREATE INDEX idx_mpo_target ON menu_price_overrides (target_type, target_id);
CREATE INDEX idx_mpo_branch ON menu_price_overrides (branch_id) WHERE branch_id IS NOT NULL;

-- ─────────────────────────────────────────────────────────────────────────────
-- 7. catalog_revision — per-org monotonically-increasing catalog version so the POS
--    knows when to resync. Wave-2 catalog writes bump revision; the offline POS
--    compares its cached revision against this. Backfill seeds revision=1 per org.
-- ─────────────────────────────────────────────────────────────────────────────
CREATE TABLE catalog_revision (
    org_id      UUID PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    revision    BIGINT NOT NULL DEFAULT 1,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ─────────────────────────────────────────────────────────────────────────────
-- Grants — match the repo convention (legacy 'sufrix' cluster role; see CLAUDE.md).
-- ─────────────────────────────────────────────────────────────────────────────
GRANT ALL ON TABLE menu_item_addon_overrides   TO sufrix;
GRANT ALL ON TABLE menu_item_sizes             TO sufrix;
GRANT ALL ON TABLE modifier_groups             TO sufrix;
GRANT ALL ON TABLE modifier_options            TO sufrix;
GRANT ALL ON TABLE menu_item_modifier_groups   TO sufrix;
GRANT ALL ON TABLE recipe_lines                TO sufrix;
GRANT ALL ON TABLE menu_price_overrides        TO sufrix;
GRANT ALL ON TABLE catalog_revision            TO sufrix;
