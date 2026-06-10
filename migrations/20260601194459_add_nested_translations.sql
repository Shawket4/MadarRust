ALTER TABLE menu_item_addon_slots ADD COLUMN IF NOT EXISTS label_translations jsonb DEFAULT '{}'::jsonb NOT NULL;
ALTER TABLE menu_item_optional_fields ADD COLUMN IF NOT EXISTS name_translations jsonb DEFAULT '{}'::jsonb NOT NULL;
ALTER TABLE discounts ADD COLUMN IF NOT EXISTS name_translations jsonb DEFAULT '{}'::jsonb NOT NULL;
ALTER TABLE order_item_optionals ADD COLUMN IF NOT EXISTS name_translations jsonb DEFAULT '{}'::jsonb NOT NULL;
ALTER TABLE order_line_bundle_component_optionals ADD COLUMN IF NOT EXISTS name_translations jsonb DEFAULT '{}'::jsonb NOT NULL;
