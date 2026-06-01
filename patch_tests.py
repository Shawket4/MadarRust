import re
import glob

for filename in ["src/discounts/tests.rs", "src/menu/tests.rs"]:
    with open(filename, "r") as f:
        content = f.read()

    # CreateDiscountRequest
    content = re.sub(r'CreateDiscountRequest\s*\{', r'CreateDiscountRequest { name_translations: None,', content)
    # UpdateDiscountRequest
    content = re.sub(r'UpdateDiscountRequest\s*\{', r'UpdateDiscountRequest { name_translations: None,', content)
    
    # CreateCategoryRequest
    content = re.sub(r'CreateCategoryRequest\s*\{', r'CreateCategoryRequest { name_translations: None,', content)
    # UpdateCategoryRequest
    content = re.sub(r'UpdateCategoryRequest\s*\{', r'UpdateCategoryRequest { name_translations: None,', content)

    # CreateMenuItemRequest
    content = re.sub(r'CreateMenuItemRequest\s*\{', r'CreateMenuItemRequest { name_translations: None, description_translations: None,', content)
    # UpdateMenuItemRequest
    content = re.sub(r'UpdateMenuItemRequest\s*\{', r'UpdateMenuItemRequest { name_translations: None, description_translations: None,', content)

    # CreateAddonSlotRequest
    content = re.sub(r'addon_type:\s*"([^"]+)"\.to_string\(\),', r'addon_type: Some("\1".to_string()),', content)
    content = re.sub(r'CreateAddonSlotRequest\s*\{', r'CreateAddonSlotRequest { label_translations: None,', content)
    # UpdateAddonSlotRequest
    content = re.sub(r'UpdateAddonSlotRequest\s*\{', r'UpdateAddonSlotRequest { label_translations: None,', content)

    # CreateAddonItemRequest
    content = re.sub(r'CreateAddonItemRequest\s*\{', r'CreateAddonItemRequest { name_translations: None,', content)
    # UpdateAddonItemRequest
    content = re.sub(r'UpdateAddonItemRequest\s*\{', r'UpdateAddonItemRequest { name_translations: None,', content)

    # CreateOptionalFieldRequest
    content = re.sub(r'CreateOptionalFieldRequest\s*\{', r'CreateOptionalFieldRequest { name_translations: None,', content)
    # UpdateOptionalFieldRequest
    content = re.sub(r'UpdateOptionalFieldRequest\s*\{', r'UpdateOptionalFieldRequest { name_translations: None,', content)

    with open(filename, "w") as f:
        f.write(content)

print("Tests patched successfully!")
