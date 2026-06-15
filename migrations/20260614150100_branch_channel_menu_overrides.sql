-- Channel layer for the price/availability resolution chain.
-- Resolution order: org catalog (menu_items) -> branch_menu_overrides ->
-- branch_channel_menu_overrides (in_mall | outside). First non-NULL wins per
-- field; any layer can disable. NULL columns here inherit the branch layer,
-- which in turn inherits the org catalog. Sits on top of the existing
-- branch_menu_overrides table (price + availability) and its per-size sibling.
CREATE TABLE public.branch_channel_menu_overrides (
    branch_id      uuid    NOT NULL REFERENCES public.branches(id)   ON DELETE CASCADE,
    menu_item_id   uuid    NOT NULL REFERENCES public.menu_items(id) ON DELETE CASCADE,
    channel        public.delivery_channel NOT NULL,
    price_override integer,
    is_available   boolean,
    updated_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, menu_item_id, channel),
    CONSTRAINT branch_channel_menu_overrides_price_nonneg
        CHECK (price_override IS NULL OR price_override >= 0)
);
CREATE INDEX idx_branch_channel_menu_overrides_item
    ON public.branch_channel_menu_overrides (menu_item_id);

-- Per-channel addon override (the addon analogue, sitting on top of
-- branch_addon_overrides). NULL columns inherit the branch addon layer, which in
-- turn inherits the org addon default_price.
CREATE TABLE public.branch_channel_addon_overrides (
    branch_id      uuid    NOT NULL REFERENCES public.branches(id)    ON DELETE CASCADE,
    addon_item_id  uuid    NOT NULL REFERENCES public.addon_items(id) ON DELETE CASCADE,
    channel        public.delivery_channel NOT NULL,
    price_override integer,
    is_available   boolean,
    updated_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, addon_item_id, channel),
    CONSTRAINT branch_channel_addon_overrides_price_nonneg
        CHECK (price_override IS NULL OR price_override >= 0)
);
CREATE INDEX idx_branch_channel_addon_overrides_addon
    ON public.branch_channel_addon_overrides (addon_item_id);
