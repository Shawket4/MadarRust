#!/bin/bash
# Backfill translations for AddonSlots, OptionalFields, Discounts, and Order tables
# Uses Google Translate API to generate Arabic translations.
# Reads GOOGLE_TRANSLATE_API_KEY and DATABASE_URL from .env file.

# Load environment variables
if [ -f .env ]; then
    export $(grep -v '^#' .env | xargs)
else
    echo "Error: .env file not found."
    exit 1
fi

if [ -z "$GOOGLE_TRANSLATE_API_KEY" ]; then
    echo "Error: GOOGLE_TRANSLATE_API_KEY is not set in .env"
    exit 1
fi

if [ -z "$DATABASE_URL" ]; then
    echo "Error: DATABASE_URL is not set in .env"
    exit 1
fi

translate_text() {
    local source_text="$1"
    
    if [ -z "$source_text" ] || [ "$source_text" == "null" ]; then
        echo ""
        return
    fi
    
    # URL encode using Python
    local encoded=$(python3 -c "import urllib.parse; print(urllib.parse.quote('''$source_text'''))")
    
    local url="https://translation.googleapis.com/language/translate/v2?key=$GOOGLE_TRANSLATE_API_KEY"
    
    local response=$(curl -s -X POST \
        -H "Content-Type: application/json" \
        -d "{\"q\": [\"$source_text\"], \"target\": \"ar\", \"source\": \"en\", \"format\": \"text\"}" \
        "$url")
        
    local translation=$(echo "$response" | python3 -c "import sys, json; print(json.load(sys.stdin).get('data', {}).get('translations', [{}])[0].get('translatedText', ''))" 2>/dev/null)
    
    if [ -z "$translation" ]; then
        echo "Error translating: $source_text" >&2
        echo "$response" >&2
        # Fallback to source
        echo "$source_text"
    else
        echo "$translation"
    fi
}

echo "=== Backfilling Addon Slots ==="
psql "$DATABASE_URL" -t -A -c "SELECT id, label FROM menu_item_addon_slots WHERE label IS NOT NULL AND label != '';" | while IFS='|' read -r id label; do
    echo "Processing Addon Slot: $label"
    ar_text=$(translate_text "$label")
    json_val=$(python3 -c "import json; print(json.dumps({'en': '''$label''', 'ar': '''$ar_text'''}))")
    escaped_json=$(echo "$json_val" | sed "s/'/''/g")
    psql "$DATABASE_URL" -c "UPDATE menu_item_addon_slots SET label_translations = '$escaped_json'::jsonb WHERE id = '$id';" > /dev/null
done

echo "=== Backfilling Optional Fields ==="
psql "$DATABASE_URL" -t -A -c "SELECT id, name FROM menu_item_optional_fields WHERE name IS NOT NULL AND name != '';" | while IFS='|' read -r id name; do
    echo "Processing Optional Field: $name"
    ar_text=$(translate_text "$name")
    json_val=$(python3 -c "import json; print(json.dumps({'en': '''$name''', 'ar': '''$ar_text'''}))")
    escaped_json=$(echo "$json_val" | sed "s/'/''/g")
    psql "$DATABASE_URL" -c "UPDATE menu_item_optional_fields SET name_translations = '$escaped_json'::jsonb WHERE id = '$id';" > /dev/null
done

echo "=== Backfilling Discounts ==="
psql "$DATABASE_URL" -t -A -c "SELECT id, name FROM discounts WHERE name IS NOT NULL AND name != '';" | while IFS='|' read -r id name; do
    echo "Processing Discount: $name"
    ar_text=$(translate_text "$name")
    json_val=$(python3 -c "import json; print(json.dumps({'en': '''$name''', 'ar': '''$ar_text'''}))")
    escaped_json=$(echo "$json_val" | sed "s/'/''/g")
    psql "$DATABASE_URL" -c "UPDATE discounts SET name_translations = '$escaped_json'::jsonb WHERE id = '$id';" > /dev/null
done

echo "=== Backfilling Order Item Optionals ==="
psql "$DATABASE_URL" -t -A -c "SELECT id, field_name FROM order_item_optionals WHERE field_name IS NOT NULL AND field_name != '';" | while IFS='|' read -r id name; do
    echo "Processing Order Item Optional: $name"
    ar_text=$(translate_text "$name")
    json_val=$(python3 -c "import json; print(json.dumps({'en': '''$name''', 'ar': '''$ar_text'''}))")
    escaped_json=$(echo "$json_val" | sed "s/'/''/g")
    psql "$DATABASE_URL" -c "UPDATE order_item_optionals SET name_translations = '$escaped_json'::jsonb WHERE id = '$id';" > /dev/null
done

echo "=== Backfilling Order Bundle Component Optionals ==="
psql "$DATABASE_URL" -t -A -c "SELECT id, field_name FROM order_line_bundle_component_optionals WHERE field_name IS NOT NULL AND field_name != '';" | while IFS='|' read -r id name; do
    echo "Processing Order Bundle Component Optional: $name"
    ar_text=$(translate_text "$name")
    json_val=$(python3 -c "import json; print(json.dumps({'en': '''$name''', 'ar': '''$ar_text'''}))")
    escaped_json=$(echo "$json_val" | sed "s/'/''/g")
    psql "$DATABASE_URL" -c "UPDATE order_line_bundle_component_optionals SET name_translations = '$escaped_json'::jsonb WHERE id = '$id';" > /dev/null
done

echo "Backfill complete!"
