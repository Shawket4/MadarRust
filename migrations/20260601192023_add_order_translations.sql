ALTER TABLE order_items
ADD COLUMN name_translations JSONB NOT NULL DEFAULT '{}'::jsonb;

ALTER TABLE order_item_addons
ADD COLUMN name_translations JSONB NOT NULL DEFAULT '{}'::jsonb;

ALTER TABLE order_line_bundle_components
ADD COLUMN name_translations JSONB NOT NULL DEFAULT '{}'::jsonb;

ALTER TABLE order_line_bundle_component_addons
ADD COLUMN name_translations JSONB NOT NULL DEFAULT '{}'::jsonb;
