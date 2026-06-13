-- Remove the unused `display_order` columns. They always defaulted to 0 (no
-- UI ever set them), so every list silently fell back to its name/created_at
-- tiebreaker — the column was dead weight. List ordering is unchanged: the
-- handlers now sort by that same tiebreaker directly (name, or label for
-- item_sizes so small→medium→large keeps the enum order, or created_at).

ALTER TABLE categories                DROP COLUMN IF EXISTS display_order;
ALTER TABLE menu_items                DROP COLUMN IF EXISTS display_order;
ALTER TABLE item_sizes                DROP COLUMN IF EXISTS display_order;
ALTER TABLE addon_items               DROP COLUMN IF EXISTS display_order;
ALTER TABLE menu_item_addon_slots     DROP COLUMN IF EXISTS display_order;
ALTER TABLE menu_item_optional_fields DROP COLUMN IF EXISTS display_order;
ALTER TABLE bundles                   DROP COLUMN IF EXISTS display_order;
ALTER TABLE org_payment_methods       DROP COLUMN IF EXISTS display_order;
