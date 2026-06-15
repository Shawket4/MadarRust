-- V34 — per-branch addon item overrides (price + availability).
--
-- The addon analogue of branch_menu_overrides: a branch may reprice an addon
-- (price_override, piastres — NULL inherits addon_items.default_price) and/or
-- disable it at that branch (is_available=false → excluded from the branch's
-- addon list and from the branch menu). Addons have no sizes, so there is no
-- size layer here.
CREATE TABLE public.branch_addon_overrides (
    branch_id      uuid    NOT NULL REFERENCES public.branches(id)     ON DELETE CASCADE,
    addon_item_id  uuid    NOT NULL REFERENCES public.addon_items(id)  ON DELETE CASCADE,
    price_override integer,
    is_available   boolean NOT NULL DEFAULT true,
    updated_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, addon_item_id),
    CONSTRAINT branch_addon_overrides_price_nonneg
        CHECK (price_override IS NULL OR price_override >= 0)
);
CREATE INDEX idx_branch_addon_overrides_addon ON public.branch_addon_overrides (addon_item_id);
