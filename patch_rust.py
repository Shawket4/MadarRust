import re

with open("src/reports/handlers.rs", "r") as f:
    code = f.read()

# 1. Update ItemSales
code = code.replace(
    "pub struct ItemSales {\n    pub menu_item_id:  Uuid,\n    pub item_name:     String,",
    "pub struct ItemSales {\n    pub menu_item_id:  Uuid,\n    pub item_name:     String,\n    #[schema(value_type = Object)]\n    pub item_name_translations: serde_json::Value,"
)

# 2. Update CategorySales
code = code.replace(
    "pub struct CategorySales {\n    pub category_id:   Option<Uuid>,\n    pub category_name: Option<String>,",
    "pub struct CategorySales {\n    pub category_id:   Option<Uuid>,\n    pub category_name: Option<String>,\n    #[schema(value_type = Object)]\n    pub category_name_translations: Option<serde_json::Value>,"
)

# 3. Update AddonSalesRow
code = code.replace(
    "pub struct AddonSalesRow {\n    pub addon_item_id: Uuid,\n    pub addon_name:    String,",
    "pub struct AddonSalesRow {\n    pub addon_item_id: Uuid,\n    pub addon_name:    String,\n    #[schema(value_type = Object)]\n    pub addon_name_translations: serde_json::Value,"
)

# 4. Update CombinedItemSalesRow
code = code.replace(
    "pub struct CombinedItemSalesRow {\n    pub item_id:       Option<Uuid>,\n    pub item_name:     String,",
    "pub struct CombinedItemSalesRow {\n    pub item_id:       Option<Uuid>,\n    pub item_name:     String,\n    #[schema(value_type = Object)]\n    pub item_name_translations: serde_json::Value,"
)

# 5. Update CategoryItemRow
code = code.replace(
    "struct CategoryItemRow {\n        category_id:   Option<Uuid>,\n        category_name: Option<String>,\n        menu_item_id:  Uuid,\n        item_name:     String,",
    "struct CategoryItemRow {\n        category_id:   Option<Uuid>,\n        category_name: Option<String>,\n        category_name_translations: Option<serde_json::Value>,\n        menu_item_id:  Uuid,\n        item_name:     String,\n        item_name_translations: serde_json::Value,"
)

# 6. Update top_items SQL query
top_items_sql_old = """SELECT COALESCE(oi.menu_item_id, oi.bundle_id) AS menu_item_id, oi.item_name,
               SUM(oi.quantity)::bigint   AS quantity_sold,
               SUM(oi.line_total)::bigint AS revenue
        FROM order_items oi"""
top_items_sql_new = """SELECT COALESCE(oi.menu_item_id, oi.bundle_id) AS menu_item_id, oi.item_name,
               COALESCE((array_agg(oi.name_translations))[1], '{}'::jsonb) AS item_name_translations,
               SUM(oi.quantity)::bigint   AS quantity_sold,
               SUM(oi.line_total)::bigint AS revenue
        FROM order_items oi"""
code = code.replace(top_items_sql_old, top_items_sql_new)

# 7. Update CategoryItemRow SQL query
cat_items_sql_old = """COALESCE(c.name, CASE WHEN oi.bundle_id IS NOT NULL THEN 'Bundles' ELSE 'Uncategorized' END) AS category_name,
            COALESCE(oi.menu_item_id, oi.bundle_id) AS menu_item_id,
            oi.item_name,
            SUM(oi.quantity)::bigint   AS quantity_sold,"""
cat_items_sql_new = """COALESCE(c.name, CASE WHEN oi.bundle_id IS NOT NULL THEN 'Bundles' ELSE 'Uncategorized' END) AS category_name,
            (array_agg(c.name_translations))[1] AS category_name_translations,
            COALESCE(oi.menu_item_id, oi.bundle_id) AS menu_item_id,
            oi.item_name,
            COALESCE((array_agg(oi.name_translations))[1], '{}'::jsonb) AS item_name_translations,
            SUM(oi.quantity)::bigint   AS quantity_sold,"""
code = code.replace(cat_items_sql_old, cat_items_sql_new)

# 8. Update mapping in CategoryItemRow loop
cat_mapping_old = """let item = ItemSales {
            menu_item_id:  row.menu_item_id,
            item_name:     row.item_name,
            quantity_sold: row.quantity_sold,
            revenue:       row.revenue,
        };"""
cat_mapping_new = """let item = ItemSales {
            menu_item_id:  row.menu_item_id,
            item_name:     row.item_name,
            item_name_translations: row.item_name_translations,
            quantity_sold: row.quantity_sold,
            revenue:       row.revenue,
        };"""
code = code.replace(cat_mapping_old, cat_mapping_new)

cat_push_old = """by_category.push(CategorySales {
                    category_id:   row.category_id,
                    category_name: row.category_name,
                    item_count:    1,
                    quantity_sold: item.quantity_sold,
                    revenue:       item.revenue,
                    items:         vec![item],
                });"""
cat_push_new = """by_category.push(CategorySales {
                    category_id:   row.category_id,
                    category_name: row.category_name,
                    category_name_translations: row.category_name_translations,
                    item_count:    1,
                    quantity_sold: item.quantity_sold,
                    revenue:       item.revenue,
                    items:         vec![item],
                });"""
code = code.replace(cat_push_old, cat_push_new)

# 9. Update AddonSalesRow SQL query
addon_sql_old = """SELECT
            oia.addon_item_id,
            oia.addon_name,
            COALESCE(ai.type, 'extra') AS addon_type,
            SUM(oia.quantity)::bigint AS quantity_sold,
            SUM(oia.line_total)::bigint AS revenue
        FROM order_item_addons oia"""
addon_sql_new = """SELECT
            oia.addon_item_id,
            oia.addon_name,
            COALESCE((array_agg(oia.name_translations))[1], '{}'::jsonb) AS addon_name_translations,
            COALESCE(ai.type, 'extra') AS addon_type,
            SUM(oia.quantity)::bigint AS quantity_sold,
            SUM(oia.line_total)::bigint AS revenue
        FROM order_item_addons oia"""
code = code.replace(addon_sql_old, addon_sql_new)

# 10. Update branch_combined_item_sales SQL queries
combined_sql_old = """WITH standalone_sales AS (
            SELECT
                oi.menu_item_id AS item_id,
                oi.item_name    AS item_name,
                SUM(oi.quantity)::bigint AS standalone_qty,
                SUM(oi.line_total)::bigint AS standalone_rev
            FROM order_items oi
            JOIN orders o ON o.id = oi.order_id
            WHERE o.branch_id = $1 AND o.status != 'voided'
              AND oi.bundle_id IS NULL
              AND ($2::timestamptz IS NULL OR o.created_at >= $2)
              AND ($3::timestamptz IS NULL OR o.created_at <= $3)
            GROUP BY oi.menu_item_id, oi.item_name
        ),
        bundle_sales AS (
            SELECT
                bc.component_item_id AS item_id,
                bc.item_name         AS item_name,
                SUM(bc.quantity * oi.quantity)::bigint AS bundle_qty,
                0::bigint AS bundle_rev -- Revenue is at the bundle level, so we attribute 0 to individual components for now
            FROM order_line_bundle_components bc
            JOIN order_items oi ON oi.id = bc.order_item_id
            JOIN orders o ON o.id = oi.order_id
            WHERE o.branch_id = $1 AND o.status != 'voided'
              AND ($2::timestamptz IS NULL OR o.created_at >= $2)
              AND ($3::timestamptz IS NULL OR o.created_at <= $3)
            GROUP BY bc.component_item_id, bc.item_name
        )
        SELECT
            COALESCE(s.item_id, b.item_id) AS item_id,
            COALESCE(s.item_name, b.item_name) AS item_name,
            COALESCE(s.standalone_qty, 0) AS standalone_qty,
            COALESCE(b.bundle_qty, 0) AS bundle_qty,
            COALESCE(s.standalone_qty, 0) + COALESCE(b.bundle_qty, 0) AS total_qty,
            COALESCE(s.standalone_rev, 0) + COALESCE(b.bundle_rev, 0) AS revenue
        FROM standalone_sales s
        FULL OUTER JOIN bundle_sales b ON s.item_id = b.item_id"""
combined_sql_new = """WITH standalone_sales AS (
            SELECT
                oi.menu_item_id AS item_id,
                oi.item_name    AS item_name,
                COALESCE((array_agg(oi.name_translations))[1], '{}'::jsonb) AS item_name_translations,
                SUM(oi.quantity)::bigint AS standalone_qty,
                SUM(oi.line_total)::bigint AS standalone_rev
            FROM order_items oi
            JOIN orders o ON o.id = oi.order_id
            WHERE o.branch_id = $1 AND o.status != 'voided'
              AND oi.bundle_id IS NULL
              AND ($2::timestamptz IS NULL OR o.created_at >= $2)
              AND ($3::timestamptz IS NULL OR o.created_at <= $3)
            GROUP BY oi.menu_item_id, oi.item_name
        ),
        bundle_sales AS (
            SELECT
                bc.component_item_id AS item_id,
                bc.item_name         AS item_name,
                COALESCE((array_agg(bc.name_translations))[1], '{}'::jsonb) AS item_name_translations,
                SUM(bc.quantity * oi.quantity)::bigint AS bundle_qty,
                0::bigint AS bundle_rev -- Revenue is at the bundle level, so we attribute 0 to individual components for now
            FROM order_line_bundle_components bc
            JOIN order_items oi ON oi.id = bc.order_item_id
            JOIN orders o ON o.id = oi.order_id
            WHERE o.branch_id = $1 AND o.status != 'voided'
              AND ($2::timestamptz IS NULL OR o.created_at >= $2)
              AND ($3::timestamptz IS NULL OR o.created_at <= $3)
            GROUP BY bc.component_item_id, bc.item_name
        )
        SELECT
            COALESCE(s.item_id, b.item_id) AS item_id,
            COALESCE(s.item_name, b.item_name) AS item_name,
            COALESCE(s.item_name_translations, b.item_name_translations) AS item_name_translations,
            COALESCE(s.standalone_qty, 0) AS standalone_qty,
            COALESCE(b.bundle_qty, 0) AS bundle_qty,
            COALESCE(s.standalone_qty, 0) + COALESCE(b.bundle_qty, 0) AS total_qty,
            COALESCE(s.standalone_rev, 0) + COALESCE(b.bundle_rev, 0) AS revenue
        FROM standalone_sales s
        FULL OUTER JOIN bundle_sales b ON s.item_id = b.item_id"""
code = code.replace(combined_sql_old, combined_sql_new)

with open("src/reports/handlers.rs", "w") as f:
    f.write(code)

print("Patch applied to src/reports/handlers.rs")
