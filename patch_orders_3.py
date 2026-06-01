import re

with open("src/orders/handlers.rs", "r") as f:
    content = f.read()

# 1. Add name_translations to ResolvedOptional
content = content.replace(
    "field_name:        String,",
    "field_name:        String,\n        name_translations: serde_json::Value,"
)

# 2. Add name_translations to mapping in component_resolve
content = content.replace(
    "field_name:        o.field_name,",
    "field_name:        o.field_name,\n                        name_translations: o.name_translations,"
)

# 3. Add name_translations to mapping in menu item resolve
content = content.replace(
    "let opt: (String, i32, Option<Uuid>, Option<String>, Option<String>, Option<f64>) = sqlx::query_as(",
    "let opt: (String, serde_json::Value, i32, Option<Uuid>, Option<String>, Option<String>, Option<f64>) = sqlx::query_as("
)
content = content.replace(
    "SELECT name, price, org_ingredient_id,",
    "SELECT name, name_translations, price, org_ingredient_id,"
)
content = content.replace(
    "field_name:        opt.0,\n                        price:             opt.1,",
    "field_name:        opt.0,\n                        name_translations: opt.1,\n                        price:             opt.2,"
)
content = content.replace(
    "org_ingredient_id: opt.2,\n                        ingredient_name:   opt.3,\n                        ingredient_unit:   opt.4,\n                        quantity_used:     opt.5,",
    "org_ingredient_id: opt.3,\n                        ingredient_name:   opt.4,\n                        ingredient_unit:   opt.5,\n                        quantity_used:     opt.6,"
)

# 4. Fix OrderBundleComponentFull mapping in fetch_order_items_full
content = content.replace(
    "item_name:  comp.2.clone(),",
    "item_name:  comp.2.clone(),\n                    name_translations: comp.3.clone(),"
)
content = content.replace(
    "quantity:   comp.3,",
    "quantity:   comp.4,"
)
content = content.replace(
    "size_label: comp.4.clone(),",
    "size_label: comp.5.clone(),"
)

# 5. The comps query in fetch_order_items_full needs name_translations
content = content.replace(
    "let comps: Vec<(Uuid, Uuid, String, i32, Option<String>)> = sqlx::query_as(",
    "let comps: Vec<(Uuid, Uuid, String, serde_json::Value, i32, Option<String>)> = sqlx::query_as("
)


with open("src/orders/handlers.rs", "w") as f:
    f.write(content)
