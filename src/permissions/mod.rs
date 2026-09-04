pub mod checker;
pub mod handlers;
pub mod routes;
pub mod seeder;

#[cfg(test)]
mod tests;

/// Single source of truth for every permission resource the system knows about.
///
/// MUST match the `permission_resource` DB enum, minus the retired labels below.
/// `GET /auth/permissions` iterates THIS list, so a resource missing here is
/// invisible to every client that mirrors permissions — the grant exists in
/// `role_permissions` and the endpoint still enforces it, but the app believes
/// nobody holds it and hides the feature. That is exactly how the staff,
/// delivery, reservations and floor-plan resources went unreported for months.
/// `resources_match_the_db_enum` in `tests.rs` fails if the two drift again.
///
/// Retired labels stay in the DB enum (values cannot be dropped) but are
/// deliberately omitted here:
///   - `shift_counts` — shift-close counting was removed.
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
    // Reservations & floor plan.
    "reservations",
    "floor_plan",
    // Held orders (parked carts) + the table-transfer waitlist. Both are
    // seeded and enforced by the handlers; they belong here so
    // `GET /auth/permissions` lists them and an admin can actually grant or
    // revoke them from the dashboard.
    "held_orders",
    "table_transfers",
    // Delivery.
    "delivery_orders",
    "delivery_settings",
    // Staff / HR. `work_shifts` is the HR roster; `shifts` above is the
    // teller's cash drawer. Never conflate them.
    "staff",
    "work_shifts",
    "attendance",
    "leave",
    "payroll",
];

/// DB enum labels that exist but are intentionally not part of the matrix.
#[cfg(test)]
pub const RETIRED_RESOURCES: &[&str] = &["shift_counts"];

pub const ACTIONS: &[&str] = &["create", "read", "update", "delete"];
