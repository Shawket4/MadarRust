import re

with open("src/orders/handlers.rs", "r") as f:
    content = f.read()

# Fix 1: tuple map for catalog
content = content.replace(
    '.map(|(id, qty, _)| crate::orders::component_resolve::BundleComponentInput {',
    '.map(|(id, qty, _, _)| crate::orders::component_resolve::BundleComponentInput {'
)

# Fix 2: ResolvedAddon in component loop
content = content.replace(
    '.map(|a| ResolvedAddon {\n                        addon_item_id: a.addon_item_id,\n                        addon_name:    a.addon_name,\n                        unit_price:    a.unit_price,\n                        quantity:      a.quantity,\n                    })',
    '.map(|a| ResolvedAddon {\n                        addon_item_id: a.addon_item_id,\n                        addon_name:    a.addon_name,\n                        name_translations: a.name_translations,\n                        unit_price:    a.unit_price,\n                        quantity:      a.quantity,\n                    })'
)

# Fix 3: The resolved_items.push that the first python script missed?
# Let's see what the first script missed: it probably couldn't find the exact match due to whitespace or something.
# The python script was looking for:
# 'resolved_items.push(ResolvedItem {\n            menu_item_id:      item_input.menu_item_id,\n            item_name:         if item_input.bundle_id.is_some() { item_name } else { item_name.clone() },\n            size_label:        item_input.size_label.clone(),'
# Let's replace using regex just in case.
content = re.sub(
    r'resolved_items\.push\(ResolvedItem \{\s*menu_item_id:\s*item_input\.menu_item_id,\s*item_name:\s*if item_input\.bundle_id\.is_some\(\) \{ item_name \} else \{ item_name\.clone\(\) \},\s*size_label:\s*item_input\.size_label\.clone\(\),',
    r'resolved_items.push(ResolvedItem {\n            menu_item_id:      item_input.menu_item_id,\n            item_name:         if item_input.bundle_id.is_some() { item_name } else { item_name.clone() },\n            name_translations: if item_input.bundle_id.is_some() { serde_json::json!({}) } else { name_translations },\n            size_label:        item_input.size_label.clone(),',
    content
)

with open("src/orders/handlers.rs", "w") as f:
    f.write(content)
