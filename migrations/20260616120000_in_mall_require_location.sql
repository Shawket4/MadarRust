-- Per-branch toggle for the in-mall "confirm you're at the branch" GPS requirement.
-- Default true = existing branches keep the mandatory GPS check (no behavior change).
-- When false, in-mall orders may be placed without device coordinates (a location is
-- still captured opportunistically when the device shares it, for the teller's
-- anti-spam distance). Shop/company + floor + unit remain required regardless.
ALTER TABLE branch_delivery_settings
    ADD COLUMN IF NOT EXISTS in_mall_require_location boolean NOT NULL DEFAULT true;
