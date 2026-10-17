-- The shop's menu WITHOUT prices (owner, 2026-10-17): a public page at
-- `<slug>.madar-pos.cloud/menu` (or the generic loyalty host's
-- `/menu/<org id>`), for a shop that wants its menu on a screen, a table card
-- or a bio link with no figures on it. It is configured beside the links page
-- and stored with it, but it is NOT one of the links page's buttons: nothing
-- here changes what `/public/orgs/links` returns.
--
-- Additive only: four columns with defaults, so every existing row (and every
-- shop with no row) reads as "off", and the links page's own columns are
-- untouched. A shop on any plan may switch it on (Free included); the plan
-- gates live in code, and this feature has none.

ALTER TABLE org_links_pages
    ADD COLUMN menu_display_enabled   boolean NOT NULL DEFAULT false,
    ADD COLUMN menu_display_title_en  text CHECK (menu_display_title_en IS NULL OR char_length(menu_display_title_en) <= 60),
    ADD COLUMN menu_display_title_ar  text CHECK (menu_display_title_ar IS NULL OR char_length(menu_display_title_ar) <= 60),
    -- Which branch's menu the page shows. NULL = the shop's first active
    -- branch (the rule the read-only menu already follows); a branch that is
    -- deleted later falls back to the same.
    ADD COLUMN menu_display_branch_id uuid REFERENCES branches(id) ON DELETE SET NULL;
