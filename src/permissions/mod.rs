pub mod handlers;
pub mod routes;
pub mod checker;
pub mod seeder;

#[cfg(test)]
mod tests;

/// Single source of truth for every permission resource the system knows about.
/// Keep this in sync with the `permission_resource` DB enum
/// (migrations/20260611010000_permissions_discounts.sql).
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
    "orders",
    "order_items",
    "payments",
    "payment_methods",
    "shifts",
    "shift_counts",
    "soft_serve_batches",
    "discounts",
    "reports",
    "permissions",
];

pub const ACTIONS: &[&str] = &["create", "read", "update", "delete"];