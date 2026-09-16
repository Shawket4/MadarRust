-- B10: a staff / loyalty twin FOLLOWS its source's recipe. Every recipe save on the
-- source (own lines, base or rule expansion) re-copies the lines onto the copy by
-- size label (source='linked'). Unlinking turns the copied lines into own lines.
-- A link always points at a root (an item that is not itself a copy).
ALTER TABLE menu_items
    ADD COLUMN IF NOT EXISTS recipe_source_item_id uuid NULL REFERENCES menu_items(id) ON DELETE SET NULL;
ALTER TABLE menu_items ADD CONSTRAINT menu_items_recipe_source_not_self
    CHECK (recipe_source_item_id IS NULL OR recipe_source_item_id <> id);
CREATE INDEX IF NOT EXISTS idx_menu_items_recipe_source ON menu_items (recipe_source_item_id)
    WHERE recipe_source_item_id IS NOT NULL;
