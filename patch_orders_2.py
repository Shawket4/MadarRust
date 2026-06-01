import re

with open("src/orders/handlers.rs", "r") as f:
    content = f.read()

# Fix order_item_optionals SELECT
content = content.replace(
    "SELECT id, order_item_id, optional_field_id, field_name, price, \\",
    "SELECT id, order_item_id, optional_field_id, field_name, name_translations, price, \\"
)

# Fix order_line_bundle_component_optionals SELECT
content = content.replace(
    "SELECT id, order_line_id, component_item_id, optional_field_id, field_name, price \\",
    "SELECT id, order_line_id, component_item_id, optional_field_id, field_name, name_translations, price \\"
)

# Fix order_item_optionals INSERT
content = content.replace(
    "(order_item_id, optional_field_id, field_name, price,",
    "(order_item_id, optional_field_id, field_name, name_translations, price,"
)
content = content.replace(
    "VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    "VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"
)
content = content.replace(
    "RETURNING id, order_item_id, optional_field_id, field_name, price,",
    "RETURNING id, order_item_id, optional_field_id, field_name, name_translations, price,"
)

# We also need to add the bind call for name_translations
# .bind(&opt.field_name) -> .bind(&opt.field_name)\n            .bind(&opt.name_translations)
content = content.replace(
    ".bind(&opt.field_name)\n            .bind(opt.price)",
    ".bind(&opt.field_name)\n            .bind(&opt.name_translations)\n            .bind(opt.price)"
)

# Fix order_line_bundle_component_optionals INSERT
content = content.replace(
    "(order_line_id, component_item_id, optional_field_id, field_name, price)",
    "(order_line_id, component_item_id, optional_field_id, field_name, name_translations, price)"
)
content = content.replace(
    "VALUES ($1, $2, $3, $4, $5)",
    "VALUES ($1, $2, $3, $4, $5, $6)"
)

content = content.replace(
    ".bind(&opt.field_name)\n                        .bind(opt.price)",
    ".bind(&opt.field_name)\n                        .bind(&opt.name_translations)\n                        .bind(opt.price)"
)

with open("src/orders/handlers.rs", "w") as f:
    f.write(content)
