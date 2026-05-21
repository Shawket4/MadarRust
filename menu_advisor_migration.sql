-- ============================================================
-- Menu Advisor Migration
-- Apply once against the `rue` database.
-- ============================================================

-- ── 1. ingredient_cost_history ───────────────────────────────
-- Already maintained in code (create_catalog_item / update_catalog_item).
-- This table was never applied to the DB.

CREATE TABLE public.ingredient_cost_history (
    id                uuid        DEFAULT gen_random_uuid() NOT NULL,
    org_ingredient_id uuid        NOT NULL,
    cost_per_unit     numeric(15,2) NOT NULL,
    effective_from    timestamptz NOT NULL,
    effective_until   timestamptz,          -- NULL = currently active row
    changed_by        uuid,                 -- user who triggered the change
    note              text,
    created_at        timestamptz DEFAULT now() NOT NULL
);

ALTER TABLE public.ingredient_cost_history OWNER TO rue;

ALTER TABLE ONLY public.ingredient_cost_history
    ADD CONSTRAINT ingredient_cost_history_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.ingredient_cost_history
    ADD CONSTRAINT ingredient_cost_history_org_ingredient_id_fkey
        FOREIGN KEY (org_ingredient_id) REFERENCES public.org_ingredients(id);

ALTER TABLE ONLY public.ingredient_cost_history
    ADD CONSTRAINT ingredient_cost_history_changed_by_fkey
        FOREIGN KEY (changed_by) REFERENCES public.users(id);

CREATE INDEX ingredient_cost_history_ingredient_from_idx
    ON public.ingredient_cost_history (org_ingredient_id, effective_from DESC);

-- Backfill: seed one row per existing ingredient using its current cost,
-- effective from its created_at so that any historical order lookups get
-- at least one epoch to fall back on.
INSERT INTO public.ingredient_cost_history
    (org_ingredient_id, cost_per_unit, effective_from, note)
SELECT id, cost_per_unit, created_at, 'Backfill from migration'
FROM   public.org_ingredients
WHERE  deleted_at IS NULL
ON CONFLICT DO NOTHING;


-- ── 2. menu_item_price_epochs ────────────────────────────────
-- Tracks every price change on a menu item (base price) or a specific
-- size variant.  NULL size_label = base price epoch.
-- The advisor uses this to flag when a price changed mid-window and
-- annotate the effective_price it computed from order data.

CREATE TABLE public.menu_item_price_epochs (
    id              uuid        DEFAULT gen_random_uuid() NOT NULL,
    menu_item_id    uuid        NOT NULL,
    size_label      text,                   -- NULL = base price; 'small' / 'medium' / etc. for size
    price           integer     NOT NULL,   -- minor units (piastres)
    effective_from  timestamptz NOT NULL,
    effective_until timestamptz,            -- NULL = currently active
    changed_by      uuid,
    created_at      timestamptz DEFAULT now() NOT NULL
);

ALTER TABLE public.menu_item_price_epochs OWNER TO rue;

ALTER TABLE ONLY public.menu_item_price_epochs
    ADD CONSTRAINT menu_item_price_epochs_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.menu_item_price_epochs
    ADD CONSTRAINT menu_item_price_epochs_menu_item_id_fkey
        FOREIGN KEY (menu_item_id) REFERENCES public.menu_items(id);

ALTER TABLE ONLY public.menu_item_price_epochs
    ADD CONSTRAINT menu_item_price_epochs_changed_by_fkey
        FOREIGN KEY (changed_by) REFERENCES public.users(id);

CREATE INDEX menu_item_price_epochs_item_from_idx
    ON public.menu_item_price_epochs (menu_item_id, effective_from DESC);

-- Backfill base prices for all existing active menu items.
INSERT INTO public.menu_item_price_epochs
    (menu_item_id, size_label, price, effective_from, changed_by)
SELECT id, NULL, base_price, created_at, NULL
FROM   public.menu_items
WHERE  deleted_at IS NULL
ON CONFLICT DO NOTHING;

-- Backfill size-level price overrides.
INSERT INTO public.menu_item_price_epochs
    (menu_item_id, size_label, price, effective_from, changed_by)
SELECT s.menu_item_id, s.label::text, s.price_override,
       COALESCE(mi.created_at, now()), NULL
FROM   public.item_sizes s
JOIN   public.menu_items mi ON mi.id = s.menu_item_id
WHERE  s.is_active = true
ON CONFLICT DO NOTHING;


-- ── 3. bundle_price_epochs ───────────────────────────────────
-- Tracks every price change on a bundle.

CREATE TABLE public.bundle_price_epochs (
    id              uuid        DEFAULT gen_random_uuid() NOT NULL,
    bundle_id       uuid        NOT NULL,
    price           integer     NOT NULL,   -- minor units (piastres)
    effective_from  timestamptz NOT NULL,
    effective_until timestamptz,            -- NULL = currently active
    changed_by      uuid,
    created_at      timestamptz DEFAULT now() NOT NULL
);

ALTER TABLE public.bundle_price_epochs OWNER TO rue;

ALTER TABLE ONLY public.bundle_price_epochs
    ADD CONSTRAINT bundle_price_epochs_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.bundle_price_epochs
    ADD CONSTRAINT bundle_price_epochs_bundle_id_fkey
        FOREIGN KEY (bundle_id) REFERENCES public.bundles(id);

ALTER TABLE ONLY public.bundle_price_epochs
    ADD CONSTRAINT bundle_price_epochs_changed_by_fkey
        FOREIGN KEY (changed_by) REFERENCES public.users(id);

CREATE INDEX bundle_price_epochs_bundle_from_idx
    ON public.bundle_price_epochs (bundle_id, effective_from DESC);

-- Backfill existing bundles.
INSERT INTO public.bundle_price_epochs
    (bundle_id, price, effective_from, changed_by)
SELECT id, price, created_at, created_by
FROM   public.bundles
ON CONFLICT DO NOTHING;
