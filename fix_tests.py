import re
import glob

def fix_file(filepath):
    with open(filepath, 'r') as f:
        content = f.read()

    # Fix CreateDiscountRequest / UpdateDiscountRequest
    content = re.sub(r'(CreateDiscountRequest|UpdateDiscountRequest)\s*\{([^}]+)\}', 
                     lambda m: m.group(0) if 'name_translations' in m.group(2) else f"{m.group(1)} {{{m.group(2).rstrip()}, name_translations: serde_json::json!({{\"en\": \"T\", \"ar\": \"T\"}}) }}", 
                     content)

    # Fix Category
    content = re.sub(r'(CreateCategoryRequest|UpdateCategoryRequest)\s*\{([^}]+)\}', 
                     lambda m: m.group(0) if 'name_translations' in m.group(2) else f"{m.group(1)} {{{m.group(2).rstrip()}, name_translations: serde_json::json!({{\"en\": \"T\", \"ar\": \"T\"}}) }}", 
                     content)

    # Fix MenuItem
    content = re.sub(r'(CreateMenuItemRequest|UpdateMenuItemRequest)\s*\{([^}]+)\}', 
                     lambda m: m.group(0) if 'name_translations' in m.group(2) else f"{m.group(1)} {{{m.group(2).rstrip()}, name_translations: serde_json::json!({{\"en\": \"T\", \"ar\": \"T\"}}), description_translations: None }}", 
                     content)

    # Fix AddonSlot
    content = re.sub(r'(CreateAddonSlotRequest|UpdateAddonSlotRequest)\s*\{([^}]+)\}', 
                     lambda m: m.group(0) if 'label_translations' in m.group(2) else f"{m.group(1)} {{{m.group(2).rstrip()}, label_translations: serde_json::json!({{\"en\": \"T\", \"ar\": \"T\"}}) }}", 
                     content)

    # Fix AddonItem
    content = re.sub(r'(CreateAddonItemRequest|UpdateAddonItemRequest)\s*\{([^}]+)\}', 
                     lambda m: m.group(0) if 'name_translations' in m.group(2) else f"{m.group(1)} {{{m.group(2).rstrip()}, name_translations: serde_json::json!({{\"en\": \"T\", \"ar\": \"T\"}}) }}", 
                     content)

    # Fix OptionalField
    content = re.sub(r'(CreateOptionalFieldRequest|UpdateOptionalFieldRequest)\s*\{([^}]+)\}', 
                     lambda m: m.group(0) if 'name_translations' in m.group(2) else f"{m.group(1)} {{{m.group(2).rstrip()}, name_translations: serde_json::json!({{\"en\": \"T\", \"ar\": \"T\"}}) }}", 
                     content)

    # Fix mismatched types for addon_type
    content = re.sub(r'addon_type:\s*"Milk"\.to_string\(\),', r'addon_type: Some("Milk".to_string()),', content)

    with open(filepath, 'w') as f:
        f.write(content)

for f in glob.glob('src/**/tests.rs', recursive=True):
    fix_file(f)
