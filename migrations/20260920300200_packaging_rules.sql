-- B8: packaging is a FLAG on the ingredient category, not a magic slug, and cups /
-- lids / straws are filled in by RULES instead of being typed on every size.
--
-- is_packaging: a dine-in sale skips every ingredient whose category is flagged (the
-- slug 'packaging' stays a fallback, so nothing changes for shops that never touch
-- the flag).
ALTER TABLE ingredient_categories
    ADD COLUMN IF NOT EXISTS is_packaging boolean NOT NULL DEFAULT false;
UPDATE ingredient_categories SET is_packaging = true WHERE slug = 'packaging' AND NOT is_packaging;

-- A rule matches an item SIZE by any combination of menu item, menu category and
-- size label (at least one). The most specific active match wins, ranked
--   item (+label) > item > category + label > category > label,
-- ties broken by `sort` then age. Its lines expand into the size's recipe_lines with
-- source='rule' (see src/menu/packaging.rs).
CREATE TABLE packaging_rules (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id             uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name               text NOT NULL CHECK (length(trim(name)) BETWEEN 1 AND 120),
    match_category_id  uuid NULL REFERENCES categories(id) ON DELETE CASCADE,
    match_size_label   text NULL CHECK (match_size_label IS NULL OR length(match_size_label) >= 1),
    match_item_id      uuid NULL REFERENCES menu_items(id) ON DELETE CASCADE,
    sort               integer NOT NULL DEFAULT 0,
    is_active          boolean NOT NULL DEFAULT true,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT packaging_rules_matches_something
        CHECK (match_category_id IS NOT NULL OR match_size_label IS NOT NULL OR match_item_id IS NOT NULL)
);
CREATE INDEX idx_packaging_rules_org ON packaging_rules (org_id);

CREATE TABLE packaging_rule_lines (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    rule_id        uuid NOT NULL REFERENCES packaging_rules(id) ON DELETE CASCADE,
    ingredient_id  uuid NOT NULL REFERENCES org_ingredients(id) ON DELETE RESTRICT,
    quantity       numeric(12,3) NOT NULL CHECK (quantity >= 0),
    unit           text NOT NULL,
    sort           integer NOT NULL DEFAULT 0,
    CONSTRAINT packaging_rule_lines_key UNIQUE (rule_id, ingredient_id)
);
CREATE INDEX idx_packaging_rule_lines_ingredient ON packaging_rule_lines (ingredient_id);

CREATE TRIGGER trg_packaging_rules_updated_at BEFORE UPDATE ON packaging_rules
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

ALTER TABLE packaging_rules ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON packaging_rules FOR ALL
    USING (org_id = (SELECT current_setting('app.org_id', true)::uuid));
ALTER TABLE packaging_rule_lines ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON packaging_rule_lines FOR ALL
    USING (EXISTS (SELECT 1 FROM packaging_rules p WHERE p.id = packaging_rule_lines.rule_id));

GRANT SELECT, INSERT, UPDATE, DELETE ON packaging_rules, packaging_rule_lines TO madar_app;
