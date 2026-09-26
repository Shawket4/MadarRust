-- The shop's links page (owner design, 2026-09-26): the page a customer lands
-- on at the root of a shop's own address — from an Instagram bio, a QR on the
-- counter, or a receipt.
--
-- This table holds ONLY what the page adds. Everything else it shows is read
-- from where it already lives, and must not be copied here:
--   * name, logo, colours, card image  -> organizations (orgs::branding::load,
--                                          which applies the branding tier)
--   * social links                     -> organizations.social_links
--   * branch name / address / phone    -> branches
--   * whether a module can be offered  -> loyalty_settings.enabled (org scope),
--                                          branch_delivery_settings.*_enabled,
--                                          branch_booking_settings.enabled,
--                                          and any active branch (the menu)
-- So "is ordering on" has one answer, in the delivery settings, and this page
-- can only HIDE a module that is on — never show one that is off.
--
-- No row = the defaults (every module that is available, in the default
-- order, branches shown). A shop never has to visit the editor for the page
-- to work.

CREATE TABLE org_links_pages (
    org_id          uuid PRIMARY KEY REFERENCES organizations(id) ON DELETE CASCADE,
    -- The buttons, in order. One entry per module ("order", "menu",
    -- "rewards", "book") and per custom link ("custom", with id, titles and
    -- an https url). Validated in `orgs::links_page` — the vocabulary is
    -- closed there, the way social links are.
    items           jsonb   NOT NULL DEFAULT '[]'::jsonb,
    tagline_en      text    CHECK (tagline_en IS NULL OR char_length(tagline_en) <= 160),
    tagline_ar      text    CHECK (tagline_ar IS NULL OR char_length(tagline_ar) <= 160),
    -- The band across the top uses the shop's CARD IMAGE (the wide photo it
    -- already uploads for its wallet pass) when this is on. Off, or no image,
    -- and the band is the brand colour.
    show_cover      boolean NOT NULL DEFAULT true,
    -- The "Visit us" section.
    show_branches   boolean NOT NULL DEFAULT true,
    -- Per branch: {"<branch uuid>": {"hidden": bool, "maps_url": "https://…"}}.
    -- A map of settings rather than a column on `branches`: branches reach the
    -- POS through the changefeed, and nothing on a till needs a Maps link.
    -- Ids of deleted branches are harmless; the read path joins live branches.
    branches        jsonb   NOT NULL DEFAULT '{}'::jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT org_links_pages_items_is_array   CHECK (jsonb_typeof(items) = 'array'),
    CONSTRAINT org_links_pages_branches_is_map  CHECK (jsonb_typeof(branches) = 'object')
);

ALTER TABLE org_links_pages ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON org_links_pages
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE org_links_pages TO madar_app;
