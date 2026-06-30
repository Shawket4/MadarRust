-- Convert item_size enum columns to plain text so sizes can be any string.

ALTER TABLE item_sizes
    ALTER COLUMN label TYPE text USING label::text;

ALTER TABLE menu_item_recipes
    ALTER COLUMN size_label TYPE text USING size_label::text;

ALTER TABLE menu_item_optional_fields
    ALTER COLUMN size_label TYPE text USING size_label::text;

ALTER TABLE branch_menu_size_overrides
    ALTER COLUMN size_label TYPE text USING size_label::text;

DROP TYPE IF EXISTS item_size;
