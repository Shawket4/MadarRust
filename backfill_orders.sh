#!/bin/bash

# Ensure DATABASE_URL is set, or try to load from .env
if [ -z "$DATABASE_URL" ]; then
    if [ -f .env ]; then
        export $(grep -v '^#' .env | xargs)
    else
        echo "Error: DATABASE_URL is not set and no .env file found."
        exit 1
    fi
fi

echo "Starting order snapshot backfill..."

# 1. Backfill order_items from menu_items
echo "Backfilling order_items from menu_items..."
psql "$DATABASE_URL" -c "
UPDATE order_items oi
SET name_translations = mi.name_translations
FROM menu_items mi
WHERE oi.menu_item_id = mi.id
  AND oi.name_translations = '{}'::jsonb;
"

# 1b. Fallback for order_items that have no matching menu_item (deleted items)
echo "Applying English fallback for unmatched order_items..."
psql "$DATABASE_URL" -c "
UPDATE order_items
SET name_translations = jsonb_build_object('en', item_name)
WHERE name_translations = '{}'::jsonb;
"

# 2. Backfill order_item_addons from addon_items
echo "Backfilling order_item_addons from addon_items..."
psql "$DATABASE_URL" -c "
UPDATE order_item_addons oia
SET name_translations = ai.name_translations
FROM addon_items ai
WHERE oia.addon_item_id = ai.id
  AND oia.name_translations = '{}'::jsonb;
"

# 2b. Fallback for order_item_addons
echo "Applying English fallback for unmatched order_item_addons..."
psql "$DATABASE_URL" -c "
UPDATE order_item_addons
SET name_translations = jsonb_build_object('en', addon_name)
WHERE name_translations = '{}'::jsonb;
"

# 3. Backfill bundle components
echo "Backfilling order_line_bundle_components from menu_items..."
psql "$DATABASE_URL" -c "
UPDATE order_line_bundle_components olbc
SET name_translations = mi.name_translations
FROM menu_items mi
WHERE olbc.component_item_id = mi.id
  AND olbc.name_translations = '{}'::jsonb;
"

# 3b. Fallback for bundle components
echo "Applying English fallback for unmatched bundle components..."
psql "$DATABASE_URL" -c "
UPDATE order_line_bundle_components
SET name_translations = jsonb_build_object('en', item_name)
WHERE name_translations = '{}'::jsonb;
"

# 4. Backfill bundle component addons 
echo "Backfilling order_line_bundle_component_addons from addon_items..."
psql "$DATABASE_URL" -c "
UPDATE order_line_bundle_component_addons obca
SET name_translations = ai.name_translations
FROM addon_items ai
WHERE obca.addon_item_id = ai.id
  AND obca.name_translations = '{}'::jsonb;
"

# 4b. Fallback for bundle component addons
echo "Applying English fallback for unmatched bundle component addons..."
psql "$DATABASE_URL" -c "
UPDATE order_line_bundle_component_addons
SET name_translations = jsonb_build_object('en', addon_name)
WHERE name_translations = '{}'::jsonb;
"

echo "Order snapshot backfill complete!"
