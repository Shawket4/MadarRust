# Sufrix POS — Bundles Backend Investigation

## 1.1 The Menu Item Model
- **Existing Entity**: Called `menu_items`.
- **Columns**: `id` (uuid), `org_id` (uuid), `category_id` (uuid), `name` (text), `description` (text), `image_url` (text), `base_price` (integer), `is_active` (boolean), `display_order` (integer), and standard timestamps (`created_at`, `updated_at`, `deleted_at`).
- **Translations**: There are no JSONB translation columns or tables in the schema. Simple `text` columns are used for `name` and `description`.
- **Variants/Sizes/Modifiers Modeling**:
  - **Sizes**: Separate table `item_sizes` (references `menu_item_id`, maps labels like `small`, `medium` to `price_override`).
  - **Addons**: Separate tables `menu_item_addon_slots` (defines slot properties like `addon_type`, min/max selections, required flags) and `addon_items` (scoped to `org_id` with ingredients).
  - **Modifiers/Optional Fields**: Separate table `menu_item_optional_fields` (defines optional items, extra pricing, and specific ingredient deductions).
- **Active / Available Representation**:
  - `is_active` boolean column on `menu_items`, `item_sizes`, `menu_item_optional_fields`.
  - `is_available` boolean column on `branch_menu_overrides`.
- **Org/Branch Scoping**:
  - Scoping is done via `org_id` on the `menu_items` table.
  - Branch scoping/overrides are stored in the `branch_menu_overrides` table (`branch_id`, `menu_item_id`, `price_override`, `is_available`).

## 1.2 The Recipe / Ingredient Model
- **Linkage**: Menu items link to recipe ingredients through the `menu_item_recipes` table which maps a `(menu_item_id, size_label)` combination to `org_ingredient_id` and tracks a numeric `quantity_used`, `ingredient_name`, and `ingredient_unit`.
- **Deduction logic**: Standard order item processing fetches these recipe rows, computes required quantities, updates branch stock (`current_stock = current_stock - required_qty`), and records a snapshot of these deductions inside a `deductions_snapshot` JSONB field on the `order_items` row.
- **Where Deduction Happens**: Directly in the service layer / handlers (`src/orders/handlers.rs`) inside a single database transaction.

## 1.3 The Transaction / Order Model
- **Table**: Called `orders`.
- **Columns**: `id` (uuid), `branch_id` (uuid), `shift_id` (uuid), `teller_id` (uuid), `order_number` (integer), `status` (enum `order_status`), `payment_method` (enum `payment_method`), `subtotal` (integer), `discount_type` (enum `discount_type`), `discount_value` (integer), `discount_amount` (integer), `tax_amount` (integer), `total_amount` (integer), `customer_name` (text), `notes` (text), `voided_at`, `void_reason` (enum `void_reason`), `voided_by` (uuid), standard timestamps, `idempotency_key` (uuid), `amount_tendered` (integer), `change_given` (integer), `tip_amount` (integer), `discount_id` (uuid), `tip_payment_method` (text).
- **Line-item Table**: Called `order_items`.
- **Columns**: `id` (uuid), `order_id` (uuid), `menu_item_id` (uuid), `item_name` (text), `size_label` (text), `unit_price` (integer), `quantity` (integer), `line_total` (integer), `notes` (text), `deductions_snapshot` (jsonb).
- **Addons & Optionals**: 
  - `order_item_addons` tracks addons added to the item (`id`, `order_item_id`, `addon_item_id`, `addon_name`, `unit_price`, `quantity`, `line_total`).
  - `order_item_optionals` tracks optional modifiers chosen for the item (`id`, `order_item_id`, `optional_field_id`, `field_name`, `price`, `org_ingredient_id`, `ingredient_name`, `ingredient_unit`, `quantity_deducted`).
- **Grouping**: No grouping mechanism currently exists.

## 1.4 Migration Tool
- **Tool**: No automated migration runner or SQLx migrations exist in the codebase.
- **Convention**: The database schema is defined as a monolithic SQL file `Schemas.sql`. DDL changes will be updated directly in `Schemas.sql` and placed in a `migrations.sql` file at the repository root to be applied on the live database.

## 1.5 Money & Decimals
- **Money Type**: `i32` / `integer` represents prices in the lowest currency unit (e.g. cents/piastres). No float or decimal types are used for monetary values.
- **Decimals**: `numeric(12,3)` is used for inventory stock quantities and recipes.

## 1.6 Multi-Tenancy
- **Scoping**: strictly scoped via `org_id` on all major tables. Enforced at query time in handlers by matching against JWT claims.

## 1.7 Soft Delete vs. Hard Delete
- **Soft Delete**: `deleted_at` timestamp with time zone column is used on `menu_items`, `categories`, `org_ingredients`, `organizations`, `users`, and `branches`.

## 1.8 Reporting / Analytics Queries
- **Mechanism**: Raw SQL queries executed in `src/reports/handlers.rs`.

## 1.9 Co-purchase / Basket Queries
- **Mechanism**: No co-purchase queries exist in the codebase currently.
