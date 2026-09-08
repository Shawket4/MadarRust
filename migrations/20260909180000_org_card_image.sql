-- A photograph across the card, behind nothing.
--
-- Apple calls it the strip and Google calls it the hero image; both are a wide
-- band under the header, and it is the one place either wallet lets a shop put
-- a picture of its own on the card. Brand material, so it lives beside the logo
-- and the derived palette and is gated by the same `custom_branding` tier.
ALTER TABLE organizations
    ADD COLUMN IF NOT EXISTS brand_card_image text;
