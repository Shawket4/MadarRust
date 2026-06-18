-- Per-menu-item addon allowlist. When rows exist for a menu item, only the
-- listed addon_items are offered on that item (dashboard config + public ordering).
-- When no rows exist, the item uses the org-wide catalog (POS default behaviour).
CREATE TABLE menu_item_allowed_addons (
    menu_item_id  UUID NOT NULL REFERENCES menu_items(id) ON DELETE CASCADE,
    addon_item_id UUID NOT NULL REFERENCES addon_items(id) ON DELETE CASCADE,
    sort_order    INTEGER NOT NULL DEFAULT 0,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (menu_item_id, addon_item_id)
);

CREATE INDEX menu_item_allowed_addons_item_idx ON menu_item_allowed_addons(menu_item_id);
