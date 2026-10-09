-- Combos and deals: the new module (COMBOS_CONTRACT.md §1, adjusted by the
-- owner's answers in §11 on 2026-09-25).
--
-- A combo is a MENU ITEM with kind = 'combo'. It keeps the item's image,
-- category, active flag, KDS routes, sync type `menu_item` and receipts; its
-- price P is its `one_size` row (base_price mirrors it); the branch and
-- delivery-channel price and availability stay in menu_price_overrides on that
-- row. Its own tables add only slots, choices and windows.
--
-- A sold combo is 1 header line (line_kind 'combo', no money) plus N part
-- lines (line_kind 'combo_part'), each a real line of the chosen item carrying
-- its share of P plus its surcharges. Deal rules (mix & match, multi-buy) are
-- their own entity, sync type `deal_rule`.
--
-- §11 overrides of the contract's §1 sketch:
--   1. Channel toggles are ORG-WIDE per channel (combo_channel_settings), with
--      per-branch overrides (combo_channel_branch_overrides). They gate every
--      combo AND every deal. There are NO per-combo channel toggles, so
--      menu_item_combos carries no sell_* columns and combo_branch_overrides
--      does not exist (a combo's own branch availability is its one_size row
--      in menu_price_overrides, like any item).
--   2. Deals also apply on QR and online checkout (the server picks the best
--      deal); the tables need no channel column: the toggles above gate them.
--   3. Windows take optional valid_from / valid_to dates.
--
-- All money is integer piastres.
--
-- SOURCE TABLES (additions; machine-read by the migration tests together with
-- the earlier changefeed headers). Format:  table -> type[, type…]  (scope)
--   menu_item_combos               -> menu_item (the combo)       (org fan-out)
--   combo_slots                    -> menu_item (the combo)       (org fan-out)
--   combo_slot_choices             -> menu_item (the combo)       (org fan-out)
--   combo_choice_size_surcharges   -> menu_item (the combo)       (org fan-out)
--   combo_channel_settings         -> menu_item, deal_rule (every combo and deal of the org) (org fan-out)
--   combo_channel_branch_overrides -> menu_item, deal_rule (every combo and deal of the org) (that branch)
--   sale_windows                   -> menu_item, deal_rule (its combo or deal) (org fan-out)
--   deal_rules                     -> deal_rule                   (org fan-out)
--   deal_rule_items                -> deal_rule (parent)          (org fan-out)
--   deal_rule_branch_overrides     -> deal_rule (parent)          (that branch)
--   order_deals                    -> order (parent)              (branch, LEDGER)
--   order_deal_lines               -> order (parent)              (branch, LEDGER)

-- ══ 1. Catalog ═══════════════════════════════════════════════════════════════

-- The kind, and the "make it a meal" pointer (C14).
ALTER TABLE menu_items
    ADD COLUMN kind          text NOT NULL DEFAULT 'item' CHECK (kind IN ('item', 'combo')),
    ADD COLUMN meal_combo_id uuid NULL REFERENCES menu_items(id) ON DELETE SET NULL,
    ADD COLUMN meal_slot_id  uuid NULL;   -- FK below, after combo_slots
CREATE INDEX idx_menu_items_kind ON menu_items (org_id, kind) WHERE deleted_at IS NULL;
CREATE INDEX idx_menu_items_meal_combo ON menu_items (meal_combo_id) WHERE meal_combo_id IS NOT NULL;

-- 1:1 with a combo item: the anchor its slots and windows hang off.
CREATE TABLE menu_item_combos (
    menu_item_id uuid PRIMARY KEY REFERENCES menu_items(id) ON DELETE CASCADE,
    org_id       uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX idx_menu_item_combos_org ON menu_item_combos (org_id);

-- §11.1: the channel toggles, org-wide. No row = every channel on (C3's
-- default). They gate every combo and every deal of the org.
CREATE TABLE combo_channel_settings (
    org_id        uuid PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    sell_pos      boolean NOT NULL DEFAULT true,
    sell_qr       boolean NOT NULL DEFAULT true,
    sell_online   boolean NOT NULL DEFAULT true,
    sell_delivery boolean NOT NULL DEFAULT true,
    updated_at    timestamptz NOT NULL DEFAULT now()
);

-- §11.1: per-branch overrides; NULL = inherit the org's toggle.
CREATE TABLE combo_channel_branch_overrides (
    branch_id     uuid PRIMARY KEY REFERENCES branches(id) ON DELETE CASCADE,
    org_id        uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    sell_pos      boolean NULL,
    sell_qr       boolean NULL,
    sell_online   boolean NULL,
    sell_delivery boolean NULL,
    updated_at    timestamptz NOT NULL DEFAULT now(),
    CHECK (num_nonnulls(sell_pos, sell_qr, sell_online, sell_delivery) > 0)
);
CREATE INDEX idx_combo_channel_branch_overrides_org ON combo_channel_branch_overrides (org_id);

CREATE TABLE combo_slots (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id             uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    combo_item_id      uuid NOT NULL REFERENCES menu_item_combos(menu_item_id) ON DELETE CASCADE,
    name               text NOT NULL,
    name_translations  jsonb NOT NULL DEFAULT '{}'::jsonb,
    sort               integer NOT NULL DEFAULT 0,
    min_picks          smallint NOT NULL DEFAULT 1 CHECK (min_picks BETWEEN 0 AND 10),
    max_picks          smallint NOT NULL DEFAULT 1 CHECK (max_picks BETWEEN 1 AND 10 AND max_picks >= min_picks),
    default_item_id    uuid NULL REFERENCES menu_items(id) ON DELETE SET NULL,
    default_size_label text NULL,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX idx_combo_slots_combo ON combo_slots (combo_item_id, sort);
CREATE INDEX idx_combo_slots_org ON combo_slots (org_id);

ALTER TABLE menu_items ADD CONSTRAINT menu_items_meal_slot_fk
    FOREIGN KEY (meal_slot_id) REFERENCES combo_slots(id) ON DELETE SET NULL;

-- What a slot allows: an item, or every kind='item' item of a category.
CREATE TABLE combo_slot_choices (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id              uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    slot_id             uuid NOT NULL REFERENCES combo_slots(id) ON DELETE CASCADE,
    menu_item_id        uuid NULL REFERENCES menu_items(id) ON DELETE CASCADE,
    category_id         uuid NULL REFERENCES categories(id) ON DELETE CASCADE,
    surcharge           integer NOT NULL DEFAULT 0 CHECK (surcharge >= 0),   -- per pick unit (C9)
    included_size_label text NULL,   -- the size P covers; NULL = the item's cheapest active size
    sort                integer NOT NULL DEFAULT 0,
    CHECK (num_nonnulls(menu_item_id, category_id) = 1),
    UNIQUE (slot_id, menu_item_id),
    UNIQUE (slot_id, category_id)
);
CREATE INDEX idx_combo_slot_choices_slot ON combo_slot_choices (slot_id, sort);
CREATE INDEX idx_combo_slot_choices_item ON combo_slot_choices (menu_item_id) WHERE menu_item_id IS NOT NULL;
CREATE INDEX idx_combo_slot_choices_category ON combo_slot_choices (category_id) WHERE category_id IS NOT NULL;
CREATE INDEX idx_combo_slot_choices_org ON combo_slot_choices (org_id);

-- C9: a bigger size costs the owner's surcharge when a row exists, else the
-- size's price difference.
CREATE TABLE combo_choice_size_surcharges (
    choice_id  uuid NOT NULL REFERENCES combo_slot_choices(id) ON DELETE CASCADE,
    org_id     uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    size_label text NOT NULL,
    surcharge  integer NOT NULL CHECK (surcharge >= 0),
    PRIMARY KEY (choice_id, size_label)
);
CREATE INDEX idx_combo_choice_size_surcharges_org ON combo_choice_size_surcharges (org_id);

-- C4 + §11.3: optional windows, shared by combos and deals. No row = always.
CREATE TABLE sale_windows (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id        uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    combo_item_id uuid NULL REFERENCES menu_item_combos(menu_item_id) ON DELETE CASCADE,
    deal_rule_id  uuid NULL,                          -- FK below
    branch_id     uuid NULL REFERENCES branches(id) ON DELETE CASCADE,  -- NULL = every branch
    weekdays      smallint NOT NULL DEFAULT 127 CHECK (weekdays BETWEEN 1 AND 127), -- bit0=Sun … bit6=Sat
    starts_at     time NULL,
    ends_at       time NULL,                          -- ends_at < starts_at crosses midnight
    valid_from    date NULL,                          -- §11.3, inclusive
    valid_to      date NULL,                          -- §11.3, inclusive
    sort          integer NOT NULL DEFAULT 0,
    CHECK (num_nonnulls(combo_item_id, deal_rule_id) = 1),
    CHECK ((starts_at IS NULL) = (ends_at IS NULL) AND (starts_at IS NULL OR starts_at <> ends_at)),
    CHECK (valid_from IS NULL OR valid_to IS NULL OR valid_from <= valid_to)
);
CREATE INDEX idx_sale_windows_combo ON sale_windows (combo_item_id) WHERE combo_item_id IS NOT NULL;
CREATE INDEX idx_sale_windows_deal ON sale_windows (deal_rule_id) WHERE deal_rule_id IS NOT NULL;
CREATE INDEX idx_sale_windows_org ON sale_windows (org_id);

-- C8: deal rules.
CREATE TABLE deal_rules (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id            uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name              text NOT NULL,
    name_translations jsonb NOT NULL DEFAULT '{}'::jsonb,
    kind              text NOT NULL CHECK (kind IN ('n_for_price', 'buy_get')),
    qty               smallint NOT NULL CHECK (qty BETWEEN 1 AND 20),   -- N (n_for_price) or the "buy" count
    price             integer NULL CHECK (price >= 0),                  -- n_for_price: the price of N
    get_qty           smallint NULL CHECK (get_qty BETWEEN 1 AND 20),   -- buy_get
    get_percent       smallint NULL CHECK (get_percent BETWEEN 1 AND 100), -- buy_get: 100 = free
    max_per_order     smallint NULL CHECK (max_per_order >= 1),
    sort              integer NOT NULL DEFAULT 0,
    is_active         boolean NOT NULL DEFAULT true,
    deleted_at        timestamptz NULL,      -- soft delete: order_deals keep pointing at it
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    CHECK ((kind = 'n_for_price' AND price IS NOT NULL AND qty >= 2 AND get_qty IS NULL AND get_percent IS NULL)
        OR (kind = 'buy_get' AND price IS NULL AND get_qty IS NOT NULL AND get_percent IS NOT NULL))
);
CREATE INDEX idx_deal_rules_org ON deal_rules (org_id) WHERE deleted_at IS NULL;

ALTER TABLE sale_windows ADD CONSTRAINT sale_windows_deal_fk
    FOREIGN KEY (deal_rule_id) REFERENCES deal_rules(id) ON DELETE CASCADE;

CREATE TABLE deal_rule_items (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id       uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    deal_rule_id uuid NOT NULL REFERENCES deal_rules(id) ON DELETE CASCADE,
    role         text NOT NULL DEFAULT 'pool' CHECK (role IN ('pool', 'reward')), -- reward: buy_get only; none = same as pool
    menu_item_id uuid NULL REFERENCES menu_items(id) ON DELETE CASCADE,
    category_id  uuid NULL REFERENCES categories(id) ON DELETE CASCADE,
    size_label   text NULL,                           -- NULL = any size
    sort         integer NOT NULL DEFAULT 0,
    CHECK (num_nonnulls(menu_item_id, category_id) = 1)
);
CREATE INDEX idx_deal_rule_items_rule ON deal_rule_items (deal_rule_id, role, sort);
CREATE INDEX idx_deal_rule_items_org ON deal_rule_items (org_id);

CREATE TABLE deal_rule_branch_overrides (
    deal_rule_id uuid NOT NULL REFERENCES deal_rules(id) ON DELETE CASCADE,
    branch_id    uuid NOT NULL REFERENCES branches(id) ON DELETE CASCADE,
    org_id       uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    is_active    boolean NOT NULL,
    PRIMARY KEY (deal_rule_id, branch_id)
);
CREATE INDEX idx_deal_rule_branch_overrides_org ON deal_rule_branch_overrides (org_id);

-- C11: the owner's minimum margin, org-wide. NULL = no margin warning.
ALTER TABLE organizations ADD COLUMN combo_min_margin numeric(5,4) NULL
    CHECK (combo_min_margin IS NULL OR combo_min_margin BETWEEN 0 AND 1);

-- ══ 2. Orders ════════════════════════════════════════════════════════════════

ALTER TABLE order_items
    ADD COLUMN line_kind        text NOT NULL DEFAULT 'item' CHECK (line_kind IN ('item', 'combo', 'combo_part')),
    ADD COLUMN combo_line_id    uuid NULL REFERENCES order_items(id) ON DELETE CASCADE,  -- part -> its header
    ADD COLUMN combo_slot_id    uuid NULL,       -- soft: the slot may be edited or deleted later
    ADD COLUMN combo_slot_name  text NULL,       -- snapshot for receipts and reports
    ADD COLUMN combo_unit_price integer NULL,    -- header only: P per combo unit, as charged
    ADD COLUMN combo_share      integer NOT NULL DEFAULT 0,  -- part: its share of P, whole line
    ADD COLUMN combo_surcharge  integer NOT NULL DEFAULT 0,  -- part: choice + size surcharge, whole line
    ADD COLUMN deal_minor       integer NOT NULL DEFAULT 0,  -- a deal's discount taken off this line
    ADD CONSTRAINT order_items_combo_money_not_negative CHECK (
        combo_share >= 0 AND combo_surcharge >= 0 AND deal_minor >= 0
        AND (combo_unit_price IS NULL OR combo_unit_price >= 0)),
    ADD CONSTRAINT order_items_combo_shape_ck CHECK (
         (line_kind = 'combo_part') = (combo_line_id IS NOT NULL)
     AND (line_kind <> 'combo' OR (combo_unit_price IS NOT NULL AND unit_price = 0 AND line_total = 0))
     AND (line_kind = 'item' OR deal_minor = 0));
CREATE INDEX idx_order_items_combo_line ON order_items (combo_line_id) WHERE combo_line_id IS NOT NULL;

CREATE TABLE order_deals (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id            uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    order_id          uuid NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
    deal_rule_id      uuid NOT NULL,        -- soft (rules are soft-deleted)
    deal_name         text NOT NULL,        -- snapshot
    name_translations jsonb NOT NULL DEFAULT '{}'::jsonb,
    times             smallint NOT NULL CHECK (times >= 1),
    discount          integer NOT NULL CHECK (discount >= 0),  -- what came off the lines (the till's on replay)
    discount_server   integer NULL,         -- the server's verdict; = discount live; NULL when not computable
    created_at        timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX idx_order_deals_order ON order_deals (order_id);
CREATE INDEX idx_order_deals_rule ON order_deals (org_id, deal_rule_id);

CREATE TABLE order_deal_lines (
    order_deal_id uuid NOT NULL REFERENCES order_deals(id) ON DELETE CASCADE,
    order_item_id uuid NOT NULL REFERENCES order_items(id) ON DELETE CASCADE,
    org_id        uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    units         smallint NOT NULL CHECK (units >= 1),
    discount      integer NOT NULL CHECK (discount >= 0),
    PRIMARY KEY (order_deal_id, order_item_id)
);
CREATE INDEX idx_order_deal_lines_item ON order_deal_lines (order_item_id);
CREATE INDEX idx_order_deal_lines_org ON order_deal_lines (org_id);

-- ══ 3. RLS and grants (the current convention) ══════════════════════════════
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['menu_item_combos', 'combo_channel_settings', 'combo_channel_branch_overrides',
                             'combo_slots', 'combo_slot_choices', 'combo_choice_size_surcharges',
                             'sale_windows', 'deal_rules', 'deal_rule_items', 'deal_rule_branch_overrides',
                             'order_deals', 'order_deal_lines'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I FOR ALL '
                       'USING (org_id = (SELECT current_setting(''app.org_id'', true)::uuid))', t);
        EXECUTE format('GRANT ALL ON TABLE %I TO sufrix', t);
        EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE %I TO madar_app', t);
    END LOOP;
END $$;

-- ══ 4. Guard triggers (the API checks first; these are the backstop) ═══════
-- Each raises its contract code first in MESSAGE, as a check violation.

-- A slot choice is a kind='item' item or a category, in the slot's org. A
-- combo never contains a combo.
CREATE FUNCTION combo_slot_choices_guard() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    s_org uuid;
    i_org uuid;
    i_kind text;
BEGIN
    SELECT org_id INTO s_org FROM combo_slots WHERE id = NEW.slot_id;
    IF s_org IS DISTINCT FROM NEW.org_id THEN
        RAISE EXCEPTION 'COMBO_SLOT_INVALID: the choice and its slot are in different organizations'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.menu_item_id IS NOT NULL THEN
        SELECT org_id, kind INTO i_org, i_kind FROM menu_items WHERE id = NEW.menu_item_id;
        IF i_kind = 'combo' THEN
            RAISE EXCEPTION 'COMBO_NESTED: a combo can''t contain another combo'
                USING ERRCODE = '23514';
        END IF;
        IF i_org IS DISTINCT FROM NEW.org_id THEN
            RAISE EXCEPTION 'COMBO_CHOICE_NOT_ALLOWED: the item is in another organization'
                USING ERRCODE = '23514';
        END IF;
    END IF;
    IF NEW.category_id IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM categories WHERE id = NEW.category_id AND org_id = NEW.org_id) THEN
        RAISE EXCEPTION 'COMBO_CHOICE_NOT_ALLOWED: the category is in another organization'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER combo_slot_choices_guard BEFORE INSERT OR UPDATE ON combo_slot_choices
    FOR EACH ROW EXECUTE FUNCTION combo_slot_choices_guard();

-- A slot belongs to its combo's org; its default is a kind='item' item there.
CREATE FUNCTION combo_slots_guard() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    i_org uuid;
    i_kind text;
BEGIN
    IF NOT EXISTS (SELECT 1 FROM menu_item_combos WHERE menu_item_id = NEW.combo_item_id AND org_id = NEW.org_id) THEN
        RAISE EXCEPTION 'COMBO_SLOT_INVALID: the slot and its combo are in different organizations'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.default_item_id IS NOT NULL THEN
        SELECT org_id, kind INTO i_org, i_kind FROM menu_items WHERE id = NEW.default_item_id;
        IF i_kind = 'combo' THEN
            RAISE EXCEPTION 'COMBO_NESTED: a combo can''t contain another combo'
                USING ERRCODE = '23514';
        END IF;
        IF i_org IS DISTINCT FROM NEW.org_id THEN
            RAISE EXCEPTION 'COMBO_SLOT_INVALID: the default item is in another organization'
                USING ERRCODE = '23514';
        END IF;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER combo_slots_guard BEFORE INSERT OR UPDATE ON combo_slots
    FOR EACH ROW EXECUTE FUNCTION combo_slots_guard();

-- The combo anchor is a kind='combo' item of the same org.
CREATE FUNCTION menu_item_combos_guard() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM menu_items WHERE id = NEW.menu_item_id AND org_id = NEW.org_id AND kind = 'combo') THEN
        RAISE EXCEPTION 'COMBO_SLOT_INVALID: menu_item_combos must anchor a kind=combo item of its org'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER menu_item_combos_guard BEFORE INSERT OR UPDATE ON menu_item_combos
    FOR EACH ROW EXECUTE FUNCTION menu_item_combos_guard();

-- menu_items: the kind (locked after sales; nothing of its own when a combo;
-- never a combo while a combo names it) and the meal pointer (C14).
CREATE FUNCTION menu_items_combo_guard() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    c_org uuid;
    c_kind text;
BEGIN
    IF TG_OP = 'UPDATE' AND NEW.kind IS DISTINCT FROM OLD.kind THEN
        IF EXISTS (SELECT 1 FROM order_items WHERE menu_item_id = NEW.id) THEN
            RAISE EXCEPTION 'COMBO_KIND_LOCKED: this item has sales; its type can''t change'
                USING ERRCODE = '23514';
        END IF;
        IF NEW.kind = 'combo' THEN
            IF EXISTS (SELECT 1 FROM combo_slot_choices WHERE menu_item_id = NEW.id)
               OR EXISTS (SELECT 1 FROM combo_slots WHERE default_item_id = NEW.id) THEN
                RAISE EXCEPTION 'COMBO_NESTED: a combo names this item as a choice'
                    USING ERRCODE = '23514';
            END IF;
            IF EXISTS (SELECT 1 FROM menu_item_sizes WHERE menu_item_id = NEW.id AND label <> 'one_size')
               OR EXISTS (SELECT 1 FROM menu_item_modifier_groups WHERE menu_item_id = NEW.id)
               OR EXISTS (SELECT 1 FROM menu_item_recipe_steps WHERE menu_item_id = NEW.id)
               OR EXISTS (SELECT 1 FROM recipe_lines rl JOIN menu_item_sizes z ON z.id = rl.owner_id
                           WHERE rl.owner_type = 'item_size' AND z.menu_item_id = NEW.id) THEN
                RAISE EXCEPTION 'COMBO_NO_RECIPE: a combo has no sizes, choice groups or recipe of its own'
                    USING ERRCODE = '23514';
            END IF;
        ELSIF EXISTS (SELECT 1 FROM menu_item_combos WHERE menu_item_id = NEW.id) THEN
            RAISE EXCEPTION 'COMBO_KIND_LOCKED: delete the combo''s slots before changing its type'
                USING ERRCODE = '23514';
        END IF;
    END IF;

    IF NEW.meal_combo_id IS NULL AND NEW.meal_slot_id IS NOT NULL THEN
        RAISE EXCEPTION 'MEAL_TARGET_INVALID: a meal slot needs its combo'
            USING ERRCODE = '23514';
    END IF;
    IF NEW.meal_combo_id IS NOT NULL
       AND (TG_OP = 'INSERT'
            OR (NEW.meal_combo_id, NEW.meal_slot_id, NEW.kind) IS DISTINCT FROM (OLD.meal_combo_id, OLD.meal_slot_id, OLD.kind)) THEN
        IF NEW.kind <> 'item' THEN
            RAISE EXCEPTION 'MEAL_TARGET_INVALID: only an item can be made a meal'
                USING ERRCODE = '23514';
        END IF;
        SELECT org_id, kind INTO c_org, c_kind FROM menu_items WHERE id = NEW.meal_combo_id;
        IF c_kind IS DISTINCT FROM 'combo' OR c_org IS DISTINCT FROM NEW.org_id THEN
            RAISE EXCEPTION 'MEAL_TARGET_INVALID: the meal must be a combo of the same organization'
                USING ERRCODE = '23514';
        END IF;
        IF NEW.meal_slot_id IS NULL
           OR NOT EXISTS (
               SELECT 1 FROM combo_slots s JOIN combo_slot_choices c ON c.slot_id = s.id
                WHERE s.id = NEW.meal_slot_id AND s.combo_item_id = NEW.meal_combo_id
                  AND (c.menu_item_id = NEW.id OR (c.category_id IS NOT NULL AND c.category_id = NEW.category_id))) THEN
            RAISE EXCEPTION 'MEAL_TARGET_INVALID: that combo has no slot for this item'
                USING ERRCODE = '23514';
        END IF;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER menu_items_combo_guard BEFORE INSERT OR UPDATE ON menu_items
    FOR EACH ROW EXECUTE FUNCTION menu_items_combo_guard();

-- A combo has no sizes (only its one_size row), choice groups, recipe lines or
-- recipe steps of its own (COMBO_NO_RECIPE): each part uses its own item's.
CREATE FUNCTION combo_no_recipe_guard() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    item uuid;
BEGIN
    CASE TG_TABLE_NAME
        WHEN 'menu_item_sizes' THEN
            IF NEW.label = 'one_size' AND NEW.base_id IS NULL THEN
                RETURN NEW;
            END IF;
            item := NEW.menu_item_id;
        WHEN 'menu_item_modifier_groups', 'menu_item_recipe_steps' THEN
            item := NEW.menu_item_id;
        WHEN 'recipe_lines' THEN
            IF NEW.owner_type <> 'item_size' THEN
                RETURN NEW;
            END IF;
            SELECT z.menu_item_id INTO item FROM menu_item_sizes z WHERE z.id = NEW.owner_id;
        ELSE
            RAISE EXCEPTION 'combo_no_recipe_guard: unmapped table %', TG_TABLE_NAME;
    END CASE;
    IF EXISTS (SELECT 1 FROM menu_items WHERE id = item AND kind = 'combo') THEN
        RAISE EXCEPTION 'COMBO_NO_RECIPE: a combo has no recipe of its own; each item uses its own'
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER combo_no_recipe_guard BEFORE INSERT OR UPDATE ON menu_item_sizes
    FOR EACH ROW EXECUTE FUNCTION combo_no_recipe_guard();
CREATE TRIGGER combo_no_recipe_guard BEFORE INSERT OR UPDATE ON menu_item_modifier_groups
    FOR EACH ROW EXECUTE FUNCTION combo_no_recipe_guard();
CREATE TRIGGER combo_no_recipe_guard BEFORE INSERT OR UPDATE ON menu_item_recipe_steps
    FOR EACH ROW EXECUTE FUNCTION combo_no_recipe_guard();
CREATE TRIGGER combo_no_recipe_guard BEFORE INSERT OR UPDATE ON recipe_lines
    FOR EACH ROW EXECUTE FUNCTION combo_no_recipe_guard();

-- A deal's pool entries, windows and overrides live in the rule's org.
CREATE FUNCTION deal_children_guard() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM deal_rules WHERE id = NEW.deal_rule_id AND org_id = NEW.org_id) THEN
        RAISE EXCEPTION 'DEAL_INVALID: the entry and its deal are in different organizations'
            USING ERRCODE = '23514';
    END IF;
    IF TG_TABLE_NAME = 'deal_rule_items' THEN
        IF NEW.menu_item_id IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM menu_items WHERE id = NEW.menu_item_id AND org_id = NEW.org_id AND kind = 'item') THEN
            RAISE EXCEPTION 'DEAL_INVALID: a deal''s pool holds items of its organization, never a combo'
                USING ERRCODE = '23514';
        END IF;
        IF NEW.category_id IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM categories WHERE id = NEW.category_id AND org_id = NEW.org_id) THEN
            RAISE EXCEPTION 'DEAL_INVALID: the category is in another organization'
                USING ERRCODE = '23514';
        END IF;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER deal_children_guard BEFORE INSERT OR UPDATE ON deal_rule_items
    FOR EACH ROW EXECUTE FUNCTION deal_children_guard();
CREATE TRIGGER deal_children_guard BEFORE INSERT OR UPDATE ON deal_rule_branch_overrides
    FOR EACH ROW EXECUTE FUNCTION deal_children_guard();

-- updated_at
CREATE TRIGGER trg_menu_item_combos_updated_at BEFORE UPDATE ON menu_item_combos
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
CREATE TRIGGER trg_combo_slots_updated_at BEFORE UPDATE ON combo_slots
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
CREATE TRIGGER trg_deal_rules_updated_at BEFORE UPDATE ON deal_rules
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
CREATE TRIGGER trg_combo_channel_settings_updated_at BEFORE UPDATE ON combo_channel_settings
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
CREATE TRIGGER trg_combo_channel_branch_overrides_updated_at BEFORE UPDATE ON combo_channel_branch_overrides
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- ══ 5. catalog_revision: the combo and deal tables bump their org ═══════════
CREATE OR REPLACE FUNCTION catalog_revision_touch() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    r record;
    org uuid;
BEGIN
    IF TG_OP = 'DELETE' THEN r := OLD; ELSE r := NEW; END IF;

    CASE TG_TABLE_NAME
        WHEN 'menu_items', 'categories', 'modifier_groups', 'org_ingredients',
             'ingredient_categories', 'menu_item_recipe_steps',
             'menu_item_combos', 'combo_slots', 'combo_slot_choices', 'combo_choice_size_surcharges',
             'combo_channel_settings', 'combo_channel_branch_overrides', 'sale_windows',
             'deal_rules', 'deal_rule_items', 'deal_rule_branch_overrides' THEN
            org := r.org_id;
        WHEN 'menu_item_sizes', 'menu_item_modifier_groups' THEN
            SELECT m.org_id INTO org FROM menu_items m WHERE m.id = r.menu_item_id;
        WHEN 'modifier_options' THEN
            SELECT g.org_id INTO org FROM modifier_groups g WHERE g.id = r.group_id;
        WHEN 'recipe_lines' THEN
            SELECT i.org_id INTO org FROM org_ingredients i WHERE i.id = r.ingredient_id;
        WHEN 'menu_price_overrides' THEN
            SELECT b.org_id INTO org FROM branches b WHERE b.id = r.branch_id;
        ELSE
            RAISE EXCEPTION 'catalog_revision_touch: unmapped table %', TG_TABLE_NAME;
    END CASE;

    PERFORM catalog_revision_bump(org);
    RETURN NULL;
END $$;

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['menu_item_combos', 'combo_slots', 'combo_slot_choices', 'combo_choice_size_surcharges',
                             'combo_channel_settings', 'combo_channel_branch_overrides', 'sale_windows',
                             'deal_rules', 'deal_rule_items', 'deal_rule_branch_overrides'] LOOP
        EXECUTE format('CREATE CONSTRAINT TRIGGER catalog_revision_bump AFTER INSERT OR UPDATE OR DELETE ON %I '
                       'DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION catalog_revision_touch()', t);
    END LOOP;
END $$;

-- ══ 6. The changefeed ════════════════════════════════════════════════════════

CREATE FUNCTION sync_live_deal_rule(r deal_rules) RETURNS boolean LANGUAGE sql STABLE
    AS $$ SELECT r.is_active AND r.deleted_at IS NULL $$;

CREATE FUNCTION sync_touch_deal_rule(p_rule uuid, p_branch uuid DEFAULT NULL) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    r deal_rules;
BEGIN
    SELECT * INTO r FROM deal_rules WHERE id = p_rule;
    IF NOT FOUND THEN RETURN; END IF;
    IF p_branch IS NULL THEN
        PERFORM sync_emit_org(r.org_id, 'deal_rule', r.id, sync_op(sync_live_deal_rule(r)));
    ELSIF EXISTS (SELECT 1 FROM branches WHERE id = p_branch AND org_id = r.org_id AND deleted_at IS NULL) THEN
        PERFORM sync_emit(p_branch, 'deal_rule', r.id, sync_op(sync_live_deal_rule(r)));
    END IF;
END;
$$;

-- Every combo and deal of the org (the channel toggles gate all of them).
CREATE FUNCTION sync_touch_combos_and_deals(p_org uuid, p_branch uuid DEFAULT NULL) RETURNS void
    LANGUAGE plpgsql SECURITY DEFINER SET search_path = public
    AS $$
DECLARE
    i uuid;
BEGIN
    IF p_org IS NULL THEN RETURN; END IF;
    FOR i IN SELECT menu_item_id FROM menu_item_combos WHERE org_id = p_org ORDER BY 1 LOOP
        PERFORM sync_touch_menu_item(i, p_branch);
    END LOOP;
    FOR i IN SELECT id FROM deal_rules WHERE org_id = p_org ORDER BY 1 LOOP
        PERFORM sync_touch_deal_rule(i, p_branch);
    END LOOP;
END;
$$;

CREATE FUNCTION sync_emit_menu_item_combos() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_menu_item(OLD.menu_item_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.menu_item_id IS DISTINCT FROM OLD.menu_item_id) THEN
        PERFORM sync_touch_menu_item(NEW.menu_item_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_combo_slots() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_menu_item(OLD.combo_item_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.combo_item_id IS DISTINCT FROM OLD.combo_item_id) THEN
        PERFORM sync_touch_menu_item(NEW.combo_item_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_combo_slot_choices() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    -- A slot deleted in the same statement resolves to nothing: its own
    -- delete touched the combo.
    IF TG_OP IN ('UPDATE','DELETE') THEN
        PERFORM sync_touch_menu_item((SELECT combo_item_id FROM combo_slots WHERE id = OLD.slot_id));
    END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.slot_id IS DISTINCT FROM OLD.slot_id) THEN
        PERFORM sync_touch_menu_item((SELECT combo_item_id FROM combo_slots WHERE id = NEW.slot_id));
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_combo_choice_size_surcharges() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN
        PERFORM sync_touch_menu_item((SELECT s.combo_item_id FROM combo_slot_choices c JOIN combo_slots s ON s.id = c.slot_id
                                       WHERE c.id = OLD.choice_id));
    END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.choice_id IS DISTINCT FROM OLD.choice_id) THEN
        PERFORM sync_touch_menu_item((SELECT s.combo_item_id FROM combo_slot_choices c JOIN combo_slots s ON s.id = c.slot_id
                                       WHERE c.id = NEW.choice_id));
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_combo_channel_settings() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_touch_combos_and_deals(OLD.org_id);
    ELSE
        PERFORM sync_touch_combos_and_deals(NEW.org_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_combo_channel_branch_overrides() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_combos_and_deals(OLD.org_id, OLD.branch_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.branch_id IS DISTINCT FROM OLD.branch_id) THEN
        PERFORM sync_touch_combos_and_deals(NEW.org_id, NEW.branch_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_sale_windows() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN
        PERFORM sync_touch_menu_item(OLD.combo_item_id);
        PERFORM sync_touch_deal_rule(OLD.deal_rule_id);
    END IF;
    IF TG_OP IN ('INSERT','UPDATE') THEN
        IF TG_OP = 'INSERT' OR NEW.combo_item_id IS DISTINCT FROM OLD.combo_item_id THEN
            PERFORM sync_touch_menu_item(NEW.combo_item_id);
        END IF;
        IF TG_OP = 'INSERT' OR NEW.deal_rule_id IS DISTINCT FROM OLD.deal_rule_id THEN
            PERFORM sync_touch_deal_rule(NEW.deal_rule_id);
        END IF;
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_deal_rules() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM sync_emit_org(OLD.org_id, 'deal_rule', OLD.id, 'delete');
    ELSE
        PERFORM sync_emit_org(NEW.org_id, 'deal_rule', NEW.id, sync_op(sync_live_deal_rule(NEW)));
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_deal_rule_items() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_deal_rule(OLD.deal_rule_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.deal_rule_id IS DISTINCT FROM OLD.deal_rule_id) THEN
        PERFORM sync_touch_deal_rule(NEW.deal_rule_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_deal_rule_branch_overrides() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_deal_rule(OLD.deal_rule_id, OLD.branch_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR (NEW.deal_rule_id, NEW.branch_id) IS DISTINCT FROM (OLD.deal_rule_id, OLD.branch_id)) THEN
        PERFORM sync_touch_deal_rule(NEW.deal_rule_id, NEW.branch_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_order_deals() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN PERFORM sync_touch_order(OLD.order_id); END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.order_id IS DISTINCT FROM OLD.order_id) THEN
        PERFORM sync_touch_order(NEW.order_id);
    END IF;
    RETURN NULL;
END $$;

CREATE FUNCTION sync_emit_order_deal_lines() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF TG_OP IN ('UPDATE','DELETE') THEN
        PERFORM sync_touch_order((SELECT order_id FROM order_deals WHERE id = OLD.order_deal_id));
    END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND (TG_OP = 'INSERT' OR NEW.order_deal_id IS DISTINCT FROM OLD.order_deal_id) THEN
        PERFORM sync_touch_order((SELECT order_id FROM order_deals WHERE id = NEW.order_deal_id));
    END IF;
    RETURN NULL;
END $$;

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['menu_item_combos', 'combo_slots', 'combo_slot_choices', 'combo_choice_size_surcharges',
                             'combo_channel_settings', 'combo_channel_branch_overrides', 'sale_windows',
                             'deal_rules', 'deal_rule_items', 'deal_rule_branch_overrides',
                             'order_deals', 'order_deal_lines'] LOOP
        EXECUTE format(
            'CREATE TRIGGER sync_emit AFTER INSERT OR UPDATE OR DELETE ON %I FOR EACH ROW EXECUTE FUNCTION %I()',
            t, 'sync_emit_' || t);
    END LOOP;
END $$;

-- New sync type `deal_rule` (org fan-out, state type): its live set, the
-- source registry (every existing row kept) and the type catalogue.
CREATE OR REPLACE FUNCTION sync_live_rows() RETURNS TABLE (branch_id uuid, type text, entity_id uuid)
    LANGUAGE sql STABLE SECURITY DEFINER SET search_path = public
    AS $$
    WITH ob AS (SELECT id AS branch_id, org_id FROM branches WHERE deleted_at IS NULL)
    SELECT ob.branch_id, 'category', x.id FROM categories x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_category(x)
    UNION ALL
    SELECT ob.branch_id, 'menu_item', x.id FROM menu_items x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_menu_item(x)
    UNION ALL
    SELECT ob.branch_id, 'ingredient', x.id FROM org_ingredients x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_ingredient(x)
    UNION ALL
    SELECT ob.branch_id, 'payment_method', x.id FROM org_payment_methods x JOIN ob ON ob.org_id = x.org_id
    UNION ALL
    SELECT DISTINCT x.branch_id, 'payment_availability', x.branch_id FROM branch_payment_methods x JOIN ob ON ob.branch_id = x.branch_id
    UNION ALL
    SELECT DISTINCT ob.branch_id, 'payment_availability', x.user_id FROM user_payment_methods x JOIN users u ON u.id = x.user_id JOIN ob ON ob.org_id = u.org_id
    UNION ALL
    SELECT DISTINCT ob.branch_id, 'payment_availability', x.device_id FROM device_payment_methods x JOIN devices d ON d.id = x.device_id JOIN ob ON ob.org_id = d.org_id
    UNION ALL
    SELECT ob.branch_id, 'discount', x.id FROM discounts x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_discount(x)
    UNION ALL
    SELECT x.id, 'branch_settings', x.id FROM branches x WHERE sync_live_branch_settings(x)
    UNION ALL
    SELECT x.branch_id, 'device', x.id FROM devices x WHERE x.branch_id IS NOT NULL AND sync_live_device(x)
    UNION ALL
    SELECT ob.branch_id, 'teller', x.id FROM users x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_teller(x)
    UNION ALL
    SELECT x.branch_id, 'floor_section', x.id FROM floor_sections x
    UNION ALL
    SELECT x.branch_id, 'floor_table', x.id FROM branch_tables x WHERE sync_live_floor_table(x)
    UNION ALL
    SELECT x.branch_id, 'table_occupancy', x.id FROM table_occupancies x WHERE sync_live_table_occupancy(x)
    UNION ALL
    SELECT x.branch_id, 'table_transfer', x.id FROM table_transfer_requests x WHERE sync_live_table_transfer(x)
    UNION ALL
    SELECT x.branch_id, 'open_ticket', x.id FROM open_tickets x WHERE sync_live_open_ticket(x)
    UNION ALL
    SELECT x.branch_id, 'kitchen_ticket', x.id FROM kitchen_tickets x WHERE sync_live_kitchen_ticket(x)
    UNION ALL
    SELECT x.branch_id, 'delivery', x.id FROM delivery_orders x WHERE sync_live_delivery(x)
    UNION ALL
    SELECT x.branch_id, 'booking', x.id FROM bookings x WHERE sync_live_booking(x)
    UNION ALL
    SELECT x.branch_id, 'till', x.id FROM tills x
    UNION ALL
    SELECT t.branch_id, 'cash_movement', x.id FROM till_cash_movements x JOIN tills t ON t.id = x.till_id
    UNION ALL
    SELECT x.branch_id, 'order', x.id FROM orders x
    UNION ALL
    SELECT x.branch_id, 'refund', x.id FROM order_refunds x
    UNION ALL
    SELECT ob.branch_id, 'addon_item', x.id FROM addon_items x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_addon_item(x.id)
    UNION ALL
    SELECT ob.branch_id, 'customer', x.id FROM customers x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_customer(x)
    UNION ALL
    SELECT ob.branch_id, 'deal_rule', x.id FROM deal_rules x JOIN ob ON ob.org_id = x.org_id WHERE sync_live_deal_rule(x)
    $$;

CREATE OR REPLACE FUNCTION sync_source_tables() RETURNS TABLE (source_table text, types text[])
    LANGUAGE sql IMMUTABLE
    AS $$
    VALUES
        ('categories',                 ARRAY['category']),
        ('menu_items',                 ARRAY['menu_item']),
        ('menu_item_sizes',            ARRAY['menu_item']),
        ('menu_item_modifier_groups',  ARRAY['menu_item']),
        ('modifier_groups',            ARRAY['menu_item','addon_item']),
        ('modifier_options',           ARRAY['menu_item','addon_item']),
        ('recipe_lines',               ARRAY['menu_item','addon_item']),
        ('menu_price_overrides',       ARRAY['menu_item','addon_item']),
        ('menu_item_recipe_steps',     ARRAY['menu_item']),
        ('recipe_step_presets',        ARRAY['menu_item']),
        ('menu_item_station_routes',   ARRAY['menu_item']),
        ('category_station_routes',    ARRAY['menu_item']),
        ('org_ingredients',            ARRAY['ingredient','addon_item']),
        ('org_payment_methods',        ARRAY['payment_method']),
        ('branch_payment_methods',     ARRAY['payment_availability']),
        ('user_payment_methods',       ARRAY['payment_availability']),
        ('device_payment_methods',     ARRAY['payment_availability']),
        ('discounts',                  ARRAY['discount']),
        ('branches',                   ARRAY['branch_settings']),
        ('kitchen_stations',           ARRAY['branch_settings']),
        ('devices',                    ARRAY['device']),
        ('users',                      ARRAY['teller']),
        ('user_branch_assignments',    ARRAY['teller']),
        ('permissions',                ARRAY['teller']),
        ('floor_sections',             ARRAY['floor_section']),
        ('branch_tables',              ARRAY['floor_table']),
        ('table_occupancies',          ARRAY['table_occupancy','floor_table']),
        ('booking_tables',             ARRAY['booking','floor_table']),
        ('bookings',                   ARRAY['booking','floor_table']),
        ('table_transfer_requests',    ARRAY['table_transfer']),
        ('open_tickets',               ARRAY['open_ticket']),
        ('open_ticket_items',          ARRAY['open_ticket']),
        ('open_ticket_rounds',         ARRAY['open_ticket']),
        ('kitchen_tickets',            ARRAY['kitchen_ticket']),
        ('kitchen_ticket_items',       ARRAY['kitchen_ticket']),
        ('delivery_orders',            ARRAY['delivery']),
        ('tills',                      ARRAY['till']),
        ('till_reconciliations',       ARRAY['till']),
        ('till_cash_movements',        ARRAY['cash_movement','till']),
        ('orders',                     ARRAY['order']),
        ('order_items',                ARRAY['order']),
        ('order_payments',             ARRAY['order']),
        ('order_refunds',              ARRAY['refund']),
        ('order_refund_lines',         ARRAY['refund']),
        ('addon_items',                ARRAY['addon_item']),
        ('addon_item_ingredients',     ARRAY['addon_item']),
        ('branch_addon_overrides',     ARRAY['addon_item']),
        ('branch_delivery_settings',   ARRAY['branch_settings']),
        ('role_permissions',           ARRAY['teller']),
        ('loyalty_settings',           ARRAY['branch_settings']),
        ('organizations',              ARRAY['branch_settings']),
        ('role_assignments',           ARRAY['teller']),
        ('role_assignment_branches',   ARRAY['teller']),
        ('user_overrides',             ARRAY['teller']),
        ('org_role_grants',            ARRAY['teller']),
        ('org_capability_policy',      ARRAY['teller']),
        ('customers',                  ARRAY['customer']),
        ('till_spot_views',           ARRAY['till']),
        ('staff_pool_settings',        ARRAY['branch_settings']),
        ('staff_drinks',               ARRAY['staff_drink']),
        ('loyalty_customers',          ARRAY['customer']),
        ('menu_item_combos',               ARRAY['menu_item']),
        ('combo_slots',                    ARRAY['menu_item']),
        ('combo_slot_choices',             ARRAY['menu_item']),
        ('combo_choice_size_surcharges',   ARRAY['menu_item']),
        ('combo_channel_settings',         ARRAY['menu_item','deal_rule']),
        ('combo_channel_branch_overrides', ARRAY['menu_item','deal_rule']),
        ('sale_windows',                   ARRAY['menu_item','deal_rule']),
        ('deal_rules',                     ARRAY['deal_rule']),
        ('deal_rule_items',                ARRAY['deal_rule']),
        ('deal_rule_branch_overrides',     ARRAY['deal_rule']),
        ('order_deals',                    ARRAY['order']),
        ('order_deal_lines',               ARRAY['order'])
    $$;

CREATE OR REPLACE FUNCTION sync_types() RETURNS TABLE (type text, is_ledger boolean)
    LANGUAGE sql IMMUTABLE
    AS $$
    VALUES ('category', false), ('menu_item', false), ('bundle', false), ('ingredient', false),
           ('payment_method', false), ('payment_availability', false), ('discount', false),
           ('branch_settings', false), ('device', false), ('teller', false),
           ('floor_section', false), ('floor_table', false), ('table_occupancy', false),
           ('table_transfer', false), ('open_ticket', false), ('kitchen_ticket', false),
           ('delivery', false), ('booking', false),
           ('till', true), ('cash_movement', true), ('order', true), ('refund', true),
           ('addon_item', false), ('customer', false),
           ('deal_rule', false)
    $$;

-- Backfill: every live deal rule is in the feed (none exist yet on a fresh
-- deploy; kept so the invariant holds on any database).
INSERT INTO sync_changes (branch_id, type, entity_id, op)
SELECT l.branch_id, l.type, l.entity_id, 'upsert'
  FROM sync_live_rows() l
 WHERE l.type = 'deal_rule'
 ORDER BY l.branch_id, l.entity_id
ON CONFLICT DO NOTHING;
