-- A deleted thing should stop occupying its own name.

-- Five constraints in this schema compete with soft deletion, and three others
-- already know not to. `tills`, `kitchen_stations` and `users` express
-- uniqueness as a partial index filtered on `deleted_at IS NULL`; these five do
-- not, so a deleted branch keeps its name for ever and a deleted organisation
-- keeps its slug for ever. Deleting something and creating it again — the most
-- ordinary correction there is — failed with a conflict against a row the
-- person had already deleted and could not see.
--
-- Making a unique constraint partial is strictly more permissive, so this
-- cannot fail on existing data: everything the old constraint allowed, the new
-- one allows.
--
-- The identity does NOT come back with the name. A recreated branch is a new
-- row with a new id, and every order ever placed at the old one still points at
-- the old one — which is the honest outcome. Reports that group by branch id
-- keep the two apart; anything grouping by NAME would silently fuse two eras of
-- "Maadi", which is worth knowing about rather than discovering later.

ALTER TABLE branches DROP CONSTRAINT IF EXISTS branches_org_id_name_key;
CREATE UNIQUE INDEX IF NOT EXISTS uq_branches_org_name
    ON branches (org_id, name) WHERE deleted_at IS NULL;

ALTER TABLE branches DROP CONSTRAINT IF EXISTS branches_org_code_key;
CREATE UNIQUE INDEX IF NOT EXISTS uq_branches_org_code
    ON branches (org_id, code) WHERE deleted_at IS NULL;

ALTER TABLE categories DROP CONSTRAINT IF EXISTS categories_org_id_name_key;
CREATE UNIQUE INDEX IF NOT EXISTS uq_categories_org_name
    ON categories (org_id, name) WHERE deleted_at IS NULL;

ALTER TABLE org_ingredients DROP CONSTRAINT IF EXISTS org_ingredients_org_id_name_key;
CREATE UNIQUE INDEX IF NOT EXISTS uq_org_ingredients_org_name
    ON org_ingredients (org_id, name) WHERE deleted_at IS NULL;

-- The slug is a hostname on the branding tier, so a deleted org holding one
-- hostage is worse here than elsewhere: nobody else can ever have that address.
ALTER TABLE organizations DROP CONSTRAINT IF EXISTS organizations_slug_key;
CREATE UNIQUE INDEX IF NOT EXISTS uq_organizations_slug
    ON organizations (slug) WHERE deleted_at IS NULL;
