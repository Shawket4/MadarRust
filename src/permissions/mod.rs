pub mod handlers;
pub mod routes;
pub mod checker;
pub mod seeder;

#[cfg(test)]
mod tests;

/// Single source of truth for every permission resource the system knows about.
/// Keep this in sync with the `permission_resource` DB enum
/// (latest: migrations/20260613001000_inventory_permissions.sql).
/// Note: 'shift_counts' remains a DB enum label but is retired (shift-close
/// counting was removed), so it is intentionally omitted from the matrix.
pub const RESOURCES: &[&str] = &[
    "orgs",
    "branches",
    "users",
    "categories",
    "menu_items",
    "addon_groups",
    "addon_items",
    "recipes",
    "inventory",
    "inventory_adjustments",
    "inventory_transfers",
    "stocktakes",
    "inventory_waste",
    "suppliers",
    "purchase_orders",
    "orders",
    "order_items",
    "payments",
    "payment_methods",
    "shifts",
    "soft_serve_batches",
    "discounts",
    "reports",
    "permissions",
    "kitchen_stations",
    "kitchen_orders",
    "open_tickets",
];

pub const ACTIONS: &[&str] = &["create", "read", "update", "delete"];