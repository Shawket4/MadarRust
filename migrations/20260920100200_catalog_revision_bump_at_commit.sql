-- Bump catalog_revision at COMMIT, not at the first catalog write.
--
-- The row triggers from 20260920100100 updated catalog_revision immediately, so a
-- transaction held the org's revision row lock from its first catalog write until it
-- committed: a slow dashboard transaction (or an open one, as in the changefeed's
-- in-flight test) blocked every other catalog writer of that org. As DEFERRABLE
-- INITIALLY DEFERRED constraint triggers the bump runs in the commit phase, so the lock
-- is held only for the instant before commit. Same function, same per-transaction
-- dedupe (bumped_xid); org resolution happens at commit, and a child whose parent was
-- deleted later in the same transaction is skipped (the parent's delete bumps).

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['menu_items', 'categories', 'modifier_groups', 'ingredient_categories',
                             'menu_item_recipe_steps', 'menu_item_sizes', 'menu_item_modifier_groups',
                             'modifier_options', 'recipe_lines', 'menu_price_overrides',
                             'org_ingredients'] LOOP
        EXECUTE format('DROP TRIGGER IF EXISTS catalog_revision_bump ON %I', t);
        EXECUTE format('CREATE CONSTRAINT TRIGGER catalog_revision_bump AFTER INSERT OR UPDATE OR DELETE ON %I '
                       'DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION catalog_revision_touch()', t);
    END LOOP;
END $$;

-- org_ingredients: INSERT/DELETE always, UPDATE only for catalog-visible fields.
DROP TRIGGER IF EXISTS catalog_revision_bump ON org_ingredients;
CREATE CONSTRAINT TRIGGER catalog_revision_bump
    AFTER INSERT OR DELETE ON org_ingredients
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION catalog_revision_touch();

DROP TRIGGER IF EXISTS catalog_revision_bump_update ON org_ingredients;
CREATE CONSTRAINT TRIGGER catalog_revision_bump_update
    AFTER UPDATE ON org_ingredients
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    WHEN ((OLD.name, OLD.unit, OLD.is_active, OLD.deleted_at, OLD.category_id, OLD.density_g_per_ml)
          IS DISTINCT FROM
          (NEW.name, NEW.unit, NEW.is_active, NEW.deleted_at, NEW.category_id, NEW.density_g_per_ml))
    EXECUTE FUNCTION catalog_revision_touch();
