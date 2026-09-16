-- catalog_revision is bumped by the database, not by each handler.
--
-- The POS gates `/catalog/sync` on `catalog_revision`: an unchanged revision means
-- "keep your cached catalog". Only the Studio/modifier handlers bumped it, so an
-- ingredient rename, a category edit, a recipe changed through the legacy recipe
-- handlers or raw SQL left every cached till stale (audit F18 / B11).
--
-- Every catalog table now bumps its org's revision from a row trigger. The bump is
-- idempotent per TRANSACTION (catalog_revision.bumped_xid): a bulk edit of 500 rows
-- is one bump. The handlers' own explicit bump stays and simply adds one more,
-- which is harmless (the POS only compares for equality).
--
-- Org resolution per table:
--   org_id column          menu_items, categories, modifier_groups, org_ingredients,
--                          ingredient_categories, menu_item_recipe_steps
--   menu_item_id → item    menu_item_sizes, menu_item_modifier_groups
--   group_id → group       modifier_options
--   ingredient_id → ing    recipe_lines (FK RESTRICT, always resolvable)
--   branch_id → branch     menu_price_overrides
-- A parent already gone (cascade delete) resolves to NULL and is skipped; the
-- parent's own delete bumped. The trigger never creates a revision row for an org
-- that no longer exists (organizations cascade).
--
-- org_ingredients only bumps for fields the catalog shows (name, unit, active,
-- deleted, category, density), so purchasing/cost churn doesn't force resyncs.

ALTER TABLE catalog_revision ADD COLUMN IF NOT EXISTS bumped_xid xid8;

CREATE OR REPLACE FUNCTION catalog_revision_bump(p_org uuid) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
BEGIN
    IF p_org IS NULL THEN
        RETURN;
    END IF;
    UPDATE catalog_revision
       SET revision = revision + 1, updated_at = now(), bumped_xid = pg_current_xact_id()
     WHERE org_id = p_org
       AND bumped_xid IS DISTINCT FROM pg_current_xact_id();
    IF NOT FOUND THEN
        INSERT INTO catalog_revision (org_id, revision, bumped_xid)
        SELECT p_org, 1, pg_current_xact_id()
         WHERE EXISTS (SELECT 1 FROM organizations WHERE id = p_org)
        ON CONFLICT (org_id) DO NOTHING;
    END IF;
END $$;

CREATE OR REPLACE FUNCTION catalog_revision_touch() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public AS $$
DECLARE
    r record;
    org uuid;
BEGIN
    IF TG_OP = 'DELETE' THEN r := OLD; ELSE r := NEW; END IF;

    CASE TG_TABLE_NAME
        WHEN 'menu_items', 'categories', 'modifier_groups', 'org_ingredients',
             'ingredient_categories', 'menu_item_recipe_steps' THEN
            org := r.org_id;
        WHEN 'menu_item_sizes', 'menu_item_modifier_groups' THEN
            SELECT m.org_id INTO org FROM menu_items m WHERE m.id = r.menu_item_id;
        WHEN 'modifier_options' THEN
            SELECT g.org_id INTO org FROM modifier_groups g WHERE g.id = r.group_id;
        WHEN 'recipe_lines' THEN
            SELECT i.org_id INTO org FROM org_ingredients i WHERE i.id = r.ingredient_id;
        WHEN 'menu_price_overrides' THEN
            SELECT b.org_id INTO org FROM branches b WHERE b.id = r.branch_id;
        ELSE
            RAISE EXCEPTION 'catalog_revision_touch: unmapped table %', TG_TABLE_NAME;
    END CASE;

    PERFORM catalog_revision_bump(org);
    RETURN NULL;
END $$;

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['menu_items', 'categories', 'modifier_groups', 'ingredient_categories',
                             'menu_item_recipe_steps', 'menu_item_sizes', 'menu_item_modifier_groups',
                             'modifier_options', 'recipe_lines', 'menu_price_overrides'] LOOP
        EXECUTE format('CREATE TRIGGER catalog_revision_bump AFTER INSERT OR UPDATE OR DELETE ON %I '
                       'FOR EACH ROW EXECUTE FUNCTION catalog_revision_touch()', t);
    END LOOP;
END $$;

CREATE TRIGGER catalog_revision_bump
    AFTER INSERT OR DELETE ON org_ingredients
    FOR EACH ROW EXECUTE FUNCTION catalog_revision_touch();

CREATE TRIGGER catalog_revision_bump_update
    AFTER UPDATE ON org_ingredients
    FOR EACH ROW
    WHEN ((OLD.name, OLD.unit, OLD.is_active, OLD.deleted_at, OLD.category_id, OLD.density_g_per_ml)
          IS DISTINCT FROM
          (NEW.name, NEW.unit, NEW.is_active, NEW.deleted_at, NEW.category_id, NEW.density_g_per_ml))
    EXECUTE FUNCTION catalog_revision_touch();
