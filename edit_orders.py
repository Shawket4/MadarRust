import re

with open("src/orders/handlers.rs", "r") as f:
    content = f.read()

# 1. Update struct definitions
content = re.sub(
    r'item_name:\s*String,(\s*)quantity:\s*i32,',
    r'item_name:  String,\1name_translations: serde_json::Value,\1quantity:   i32,',
    content
)

content = re.sub(
    r'item_name:\s*String,(\s*)size_label:\s*Option<String>,',
    r'item_name:         String,\1name_translations: serde_json::Value,\1size_label:        Option<String>,',
    content
)

content = re.sub(
    r'addon_name:\s*String,(\s*)unit_price:\s*i32,',
    r'addon_name:    String,\1name_translations: serde_json::Value,\1unit_price:    i32,',
    content
)

# 2. Update variable unpacking for tuple
content = content.replace(
    'let (resolved_menu_item_id, item_name, unit_price, bundle_id, bundle_unit_price) = if let Some(b_id) = item_input.bundle_id {',
    'let (resolved_menu_item_id, item_name, name_translations, unit_price, bundle_id, bundle_unit_price) = if let Some(b_id) = item_input.bundle_id {'
)

# 3. Bundle catalog
content = content.replace(
    'let catalog: Vec<(Uuid, i32, String)> = sqlx::query_as(\n                "SELECT bc.item_id, bc.quantity, mi.name \\\n                 FROM bundle_components bc \\\n                 JOIN menu_items mi ON mi.id = bc.item_id \\\n                 WHERE bc.bundle_id = $1 \\\n                 ORDER BY bc.position ASC",\n            )',
    'let catalog: Vec<(Uuid, i32, String, serde_json::Value)> = sqlx::query_as(\n                "SELECT bc.item_id, bc.quantity, mi.name, mi.name_translations \\\n                 FROM bundle_components bc \\\n                 JOIN menu_items mi ON mi.id = bc.item_id \\\n                 WHERE bc.bundle_id = $1 \\\n                 ORDER BY bc.position ASC",\n            )'
)

content = content.replace(
    'let catalog_map: std::collections::HashMap<Uuid, (i32, String)> = catalog\n                .iter()\n                .map(|(id, qty, name)| (*id, (*qty, name.clone())))\n                .collect();',
    'let catalog_map: std::collections::HashMap<Uuid, (i32, String, serde_json::Value)> = catalog\n                .iter()\n                .map(|(id, qty, name, tr)| (*id, (*qty, name.clone(), tr.clone())))\n                .collect();'
)

content = content.replace(
    'let Some((catalog_qty, item_name)) = catalog_map.get(&comp_in.item_id) else {',
    'let Some((catalog_qty, item_name, name_translations)) = catalog_map.get(&comp_in.item_id) else {'
)

# Push to bundle_components
content = content.replace(
    'bundle_components.push(ResolvedBundleComponent {\n                    item_id:    comp_in.item_id,\n                    item_name:  item_name.clone(),\n                    quantity:   comp_in.quantity,',
    'bundle_components.push(ResolvedBundleComponent {\n                    item_id:    comp_in.item_id,\n                    item_name:  item_name.clone(),\n                    name_translations: name_translations.clone(),\n                    quantity:   comp_in.quantity,'
)

# Bundle tuple return
content = content.replace(
    '(None, bundle.1, bundle.2, Some(bundle.0), Some(bundle.2))',
    '(None, bundle.1, serde_json::json!({}), bundle.2, Some(bundle.0), Some(bundle.2))'
)

# 4. Menu Items
content = content.replace(
    'let (item_name, base_price): (String, i32) = sqlx::query_as(\n                "SELECT name, base_price FROM menu_items WHERE id = $1 AND deleted_at IS NULL",\n            )',
    'let (item_name, name_translations, base_price): (String, serde_json::Value, i32) = sqlx::query_as(\n                "SELECT name, name_translations, base_price FROM menu_items WHERE id = $1 AND deleted_at IS NULL",\n            )'
)

content = content.replace(
    'let (addon_name, default_price, addon_type): (String, i32, String) = sqlx::query_as(\n                    "SELECT name, default_price, type FROM addon_items WHERE id = $1"\n                )',
    'let (addon_name, addon_name_translations, default_price, addon_type): (String, serde_json::Value, i32, String) = sqlx::query_as(\n                    "SELECT name, name_translations, default_price, type FROM addon_items WHERE id = $1"\n                )'
)

content = content.replace(
    'resolved_addons.push(ResolvedAddon {\n                    addon_item_id: addon_input.addon_item_id,\n                    addon_name:    addon_name.clone(),\n                    unit_price:    default_price,',
    'resolved_addons.push(ResolvedAddon {\n                    addon_item_id: addon_input.addon_item_id,\n                    addon_name:    addon_name.clone(),\n                    name_translations: addon_name_translations.clone(),\n                    unit_price:    default_price,'
)

# Menu Item tuple return
content = content.replace(
    '(Some(m_item_id), item_name, unit_price, None, None)',
    '(Some(m_item_id), item_name, name_translations, unit_price, None, None)'
)

# 5. Resolved items push
content = content.replace(
    'resolved_items.push(ResolvedItem {\n            menu_item_id:      item_input.menu_item_id,\n            item_name:         if item_input.bundle_id.is_some() { item_name } else { item_name.clone() },\n            size_label:        item_input.size_label.clone(),',
    'resolved_items.push(ResolvedItem {\n            menu_item_id:      item_input.menu_item_id,\n            item_name:         if item_input.bundle_id.is_some() { item_name } else { item_name.clone() },\n            name_translations: name_translations,\n            size_label:        item_input.size_label.clone(),'
)

# 6. Inserts
content = content.replace(
    'r#"INSERT INTO order_items\n                (order_id, menu_item_id, item_name, size_label,\n                 unit_price, quantity, line_total, notes, deductions_snapshot,\n                 bundle_id, bundle_unit_price)\n               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)\n               RETURNING id, order_id, menu_item_id, item_name, size_label,\n                         unit_price, quantity, line_total, notes, deductions_snapshot,\n                         bundle_id, bundle_unit_price"#,',
    'r#"INSERT INTO order_items\n                (order_id, menu_item_id, item_name, name_translations, size_label,\n                 unit_price, quantity, line_total, notes, deductions_snapshot,\n                 bundle_id, bundle_unit_price)\n               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)\n               RETURNING id, order_id, menu_item_id, item_name, size_label,\n                         unit_price, quantity, line_total, notes, deductions_snapshot,\n                         bundle_id, bundle_unit_price"#, // Did not fetch translations back explicitly'
)
content = content.replace(
    '.bind(&resolved.item_name)\n        .bind(&resolved.size_label)',
    '.bind(&resolved.item_name)\n        .bind(&resolved.name_translations)\n        .bind(&resolved.size_label)'
)

content = content.replace(
    '"INSERT INTO order_line_bundle_components \\\n                        (order_line_id, item_id, quantity, size_label) \\\n                     VALUES ($1, $2, $3, $4)",\n                )\n                .bind(order_item.id)\n                .bind(comp.item_id)\n                .bind(comp.quantity)\n                .bind(&comp.size_label)',
    '"INSERT INTO order_line_bundle_components \\\n                        (order_line_id, item_id, quantity, size_label, name_translations) \\\n                     VALUES ($1, $2, $3, $4, $5)",\n                )\n                .bind(order_item.id)\n                .bind(comp.item_id)\n                .bind(comp.quantity)\n                .bind(&comp.size_label)\n                .bind(&comp.name_translations)'
)

content = content.replace(
    '"INSERT INTO order_line_bundle_component_addons \\\n                            (order_line_id, component_item_id, addon_item_id, addon_name, \\\n                             unit_price, quantity, line_total) \\\n                         VALUES ($1, $2, $3, $4, $5, $6, $7)",\n                    )\n                    .bind(order_item.id)\n                    .bind(comp.item_id)\n                    .bind(addon.addon_item_id)\n                    .bind(&addon.addon_name)',
    '"INSERT INTO order_line_bundle_component_addons \\\n                            (order_line_id, component_item_id, addon_item_id, addon_name, name_translations, \\\n                             unit_price, quantity, line_total) \\\n                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",\n                    )\n                    .bind(order_item.id)\n                    .bind(comp.item_id)\n                    .bind(addon.addon_item_id)\n                    .bind(&addon.addon_name)\n                    .bind(&addon.name_translations)'
)

content = content.replace(
    'r#"INSERT INTO order_item_addons\n                    (order_item_id, addon_item_id, addon_name, unit_price, quantity, line_total)\n                   VALUES ($1, $2, $3, $4, $5, $6)\n                   RETURNING id, order_item_id, addon_item_id, addon_name,\n                             unit_price, quantity, line_total"#,',
    'r#"INSERT INTO order_item_addons\n                    (order_item_id, addon_item_id, addon_name, name_translations, unit_price, quantity, line_total)\n                   VALUES ($1, $2, $3, $4, $5, $6, $7)\n                   RETURNING id, order_item_id, addon_item_id, addon_name,\n                             unit_price, quantity, line_total"#,'
)
content = content.replace(
    '.bind(&addon.addon_name)\n            .bind(addon.unit_price)',
    '.bind(&addon.addon_name)\n            .bind(&addon.name_translations)\n            .bind(addon.unit_price)'
)

with open("src/orders/handlers.rs", "w") as f:
    f.write(content)
