-- V32 — per-branch menu item overrides (price + availability) + order price-integrity flags
--
-- 1. branch_menu_overrides: the per-branch layer over the org catalog. Absence of a row
--    means "inherit the org menu_items.base_price and is fully available". A row may set a
--    branch price (price_override, piastres — NULL inherits base_price) and/or disable the
--    item at that branch (is_available=false). Per-size branch pricing is intentionally NOT
--    modelled here yet (override replaces base_price; sized items keep their absolute size
--    prices) — additive when needed.
--
--    NOTE: an earlier branch_menu_overrides table was dropped by the cost-engine migration
--    (20260610090000) as "dead branch-level price overrides". This recreates it as a
--    first-class, wired feature.
CREATE TABLE public.branch_menu_overrides (
    branch_id      uuid    NOT NULL REFERENCES public.branches(id)    ON DELETE CASCADE,
    menu_item_id   uuid    NOT NULL REFERENCES public.menu_items(id)  ON DELETE CASCADE,
    price_override integer,
    is_available   boolean NOT NULL DEFAULT true,
    updated_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (branch_id, menu_item_id),
    CONSTRAINT branch_menu_overrides_price_nonneg
        CHECK (price_override IS NULL OR price_override >= 0)
);
CREATE INDEX idx_branch_menu_overrides_item ON public.branch_menu_overrides (menu_item_id);

-- 2. Pricing integrity. The POS (offline-first) is the source of truth for what the customer
--    actually paid: it now sends the priced breakdown and the backend records it verbatim. The
--    central catalog is used only to compute an EXPECTED total and FLAG deviations (stale sync /
--    offline / mid-flight price change) for reconciliation — orders are never rejected on a
--    price mismatch. price_expected_total is the server's catalog+override total at order time;
--    compare against total_amount to quantify the deviation.
ALTER TABLE public.orders
    ADD COLUMN price_flagged        boolean NOT NULL DEFAULT false,
    ADD COLUMN price_expected_total integer;

ALTER TABLE public.order_items
    ADD COLUMN price_flagged boolean NOT NULL DEFAULT false;
