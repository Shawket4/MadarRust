-- Where else to find the shop.

-- A map rather than a column each, because the list of places a café wants to
-- be found is not a thing we can finish guessing at. Facebook, Instagram and
-- TikTok today; whatever replaces TikTok gets added to a list in code and needs
-- no migration, no downtime and no coordination with a release.
--
-- Keys are validated against a known set on the way in (see `orgs::social`), so
-- this is a map with a closed vocabulary rather than a bag anyone can put
-- anything in — the card renders what is in here, and a card that renders
-- arbitrary text supplied by an org admin is a card that can say anything.
ALTER TABLE organizations
    ADD COLUMN IF NOT EXISTS social_links jsonb NOT NULL DEFAULT '{}'::jsonb;
