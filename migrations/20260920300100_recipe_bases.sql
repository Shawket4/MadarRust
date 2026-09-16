-- B7: recipe bases. "Blended matcha" = Honey 10 g · Matcha 3 g · Milk 90 g (Cup) /
-- 110 g (Can). A size points at a base; the base's lines are EXPANDED into the size's
-- recipe_lines (source='base') whenever the base or the pointer changes. A base line
-- with size_label NULL applies to every size; a labelled line only to sizes with that
-- label, and wins over a NULL-label line for the same ingredient.
--
-- Dashboard-only: nothing here reaches a till except through the expanded
-- recipe_lines, which already ride the changefeed.
CREATE TABLE recipe_bases (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id      uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name        text NOT NULL CHECK (length(trim(name)) BETWEEN 1 AND 120),
    name_ar     text NULL,
    is_active   boolean NOT NULL DEFAULT true,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    deleted_at  timestamptz NULL
);
CREATE UNIQUE INDEX recipe_bases_org_name_key ON recipe_bases (org_id, lower(name)) WHERE deleted_at IS NULL;
CREATE INDEX idx_recipe_bases_org ON recipe_bases (org_id);

CREATE TABLE recipe_base_lines (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    base_id        uuid NOT NULL REFERENCES recipe_bases(id) ON DELETE CASCADE,
    size_label     text NULL CHECK (size_label IS NULL OR length(size_label) >= 1),
    ingredient_id  uuid NOT NULL REFERENCES org_ingredients(id) ON DELETE RESTRICT,
    quantity       numeric(12,3) NOT NULL CHECK (quantity >= 0),
    unit           text NOT NULL,
    sort           integer NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX recipe_base_lines_key ON recipe_base_lines (base_id, ingredient_id, size_label) NULLS NOT DISTINCT;
CREATE INDEX idx_recipe_base_lines_ingredient ON recipe_base_lines (ingredient_id);

ALTER TABLE menu_item_sizes
    ADD COLUMN IF NOT EXISTS base_id uuid NULL REFERENCES recipe_bases(id) ON DELETE SET NULL;
CREATE INDEX IF NOT EXISTS idx_menu_item_sizes_base ON menu_item_sizes (base_id) WHERE base_id IS NOT NULL;

CREATE TRIGGER trg_recipe_bases_updated_at BEFORE UPDATE ON recipe_bases
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

ALTER TABLE recipe_bases ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON recipe_bases FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
ALTER TABLE recipe_base_lines ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON recipe_base_lines FOR ALL
    USING (EXISTS (SELECT 1 FROM recipe_bases p WHERE p.id = recipe_base_lines.base_id));

GRANT SELECT, INSERT, UPDATE, DELETE ON recipe_bases, recipe_base_lines TO madar_app;
