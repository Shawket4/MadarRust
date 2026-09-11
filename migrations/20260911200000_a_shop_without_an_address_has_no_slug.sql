-- A shop that has no address has NO slug — not an empty one.
--
-- `slug` was NOT NULL, so an organisation from before slugs existed carried
-- `''`. That is "no slug" wearing the costume of a slug, and it cost something
-- real three separate times:
--
--   * the branding-tier freeze read it as a name worth protecting, which made
--     the whole organisation uneditable — not its name, not its tax rate, not
--     its receipt footer;
--   * `GET /public/orgs/brand?slug=` MATCHED it, handing a caller the org that
--     is specifically meant to have no public identity;
--   * and `uq_organizations_slug` is UNIQUE, so `''` occupies a slot. A SECOND
--     org without a slug would have collided outright. Exactly one exists, and
--     only because creation has required a slug ever since — the constraint was
--     one legacy row away from refusing a legitimate insert.
--
-- NULL says the thing the empty string was pretending to say, and a btree
-- unique index treats NULLs as distinct, so any number of shops may have no
-- address at all.

ALTER TABLE organizations ALTER COLUMN slug DROP NOT NULL;

UPDATE organizations SET slug = NULL WHERE btrim(slug) = '';

-- The invariant that keeps the two states from diverging again. Deliberately
-- NOT a restatement of `orgs::slugs::validate` — the reserved-name list is a
-- property of our deployment, not of the data, and a CHECK that drifts from the
-- validator is worse than no CHECK. This is the one rule the database can hold
-- on its own: a slug that exists is not blank.
ALTER TABLE organizations
    ADD CONSTRAINT organizations_slug_is_not_blank
        CHECK (slug IS NULL OR btrim(slug) <> '');

COMMENT ON COLUMN organizations.slug IS
    'The shop''s first hostname label — `rue` for rue.madar-pos.cloud. NULL when
     the shop has no address of its own; never an empty string. Frozen once the
     shop is on the branding tier, because by then it is printed on QR codes
     nobody can recall.';
