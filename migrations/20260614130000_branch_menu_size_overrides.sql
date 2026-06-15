-- V33 — per-(branch, item, size) price overrides.
--
-- Layers on top of branch_menu_overrides: where the item-level row sets the branch base
-- price + availability, this sets the branch price for a specific SIZE. Absence of a row
-- means the size keeps its catalog price (item_sizes.price_override); a branch base
-- override never touches an explicitly-priced size — only a row here does.
--
-- Availability stays item-level (branch_menu_overrides.is_available); there is no per-size
-- enable/disable.
CREATE TABLE public.branch_menu_size_overrides (
    branch_id      uuid NOT NULL REFERENCES public.branches(id)   ON DELETE CASCADE,
    menu_item_id   uuid NOT NULL REFERENCES public.menu_items(id) ON DELETE CASCADE,
    size_label     public.item_size NOT NULL,
    price_override integer NOT NULL,
    updated_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, menu_item_id, size_label),
    CONSTRAINT branch_menu_size_overrides_price_nonneg CHECK (price_override >= 0)
);
CREATE INDEX idx_branch_menu_size_overrides_item ON public.branch_menu_size_overrides (menu_item_id);
