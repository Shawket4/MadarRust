use sqlx::PgPool;

/// Seed the default role permissions table on startup.
///
/// Uses ON CONFLICT DO NOTHING so any customisations made via the API
/// (`PUT /permissions/roles`) survive server restarts. Rows are only
/// inserted when they don't already exist — i.e. this is a first-run
/// initialiser, not a reset. To reset a role to defaults, delete the
/// rows in role_permissions for that role and restart.
pub async fn seed_role_permissions(pool: &PgPool) -> Result<(), sqlx::Error> {
    // (role, resource, action, granted)
    let defaults: &[(&str, &str, &str, bool)] = &[
        // ── org_admin: full access to everything ──────────────────
        // (generated: all resources × all actions = true)
        ("org_admin", "orgs",                  "create", true),
        ("org_admin", "orgs",                  "read",   true),
        ("org_admin", "orgs",                  "update", true),
        ("org_admin", "orgs",                  "delete", true),
        ("org_admin", "branches",              "create", true),
        ("org_admin", "branches",              "read",   true),
        ("org_admin", "branches",              "update", true),
        ("org_admin", "branches",              "delete", true),
        ("org_admin", "users",                 "create", true),
        ("org_admin", "users",                 "read",   true),
        ("org_admin", "users",                 "update", true),
        ("org_admin", "users",                 "delete", true),
        ("org_admin", "categories",            "create", true),
        ("org_admin", "categories",            "read",   true),
        ("org_admin", "categories",            "update", true),
        ("org_admin", "categories",            "delete", true),
        ("org_admin", "menu_items",            "create", true),
        ("org_admin", "menu_items",            "read",   true),
        ("org_admin", "menu_items",            "update", true),
        ("org_admin", "menu_items",            "delete", true),
        ("org_admin", "addon_groups",          "create", true),
        ("org_admin", "addon_groups",          "read",   true),
        ("org_admin", "addon_groups",          "update", true),
        ("org_admin", "addon_groups",          "delete", true),
        ("org_admin", "addon_items",           "create", true),
        ("org_admin", "addon_items",           "read",   true),
        ("org_admin", "addon_items",           "update", true),
        ("org_admin", "addon_items",           "delete", true),
        ("org_admin", "recipes",               "create", true),
        ("org_admin", "recipes",               "read",   true),
        ("org_admin", "recipes",               "update", true),
        ("org_admin", "recipes",               "delete", true),
        ("org_admin", "inventory",             "create", true),
        ("org_admin", "inventory",             "read",   true),
        ("org_admin", "inventory",             "update", true),
        ("org_admin", "inventory",             "delete", true),
        ("org_admin", "inventory_adjustments", "create", true),
        ("org_admin", "inventory_adjustments", "read",   true),
        ("org_admin", "inventory_adjustments", "update", true),
        ("org_admin", "inventory_adjustments", "delete", true),
        ("org_admin", "inventory_transfers",   "create", true),
        ("org_admin", "inventory_transfers",   "read",   true),
        ("org_admin", "inventory_transfers",   "update", true),
        ("org_admin", "inventory_transfers",   "delete", true),
        ("org_admin", "orders",                "create", true),
        ("org_admin", "orders",                "read",   true),
        ("org_admin", "orders",                "update", true),
        ("org_admin", "orders",                "delete", true),
        ("org_admin", "order_items",           "create", true),
        ("org_admin", "order_items",           "read",   true),
        ("org_admin", "order_items",           "update", true),
        ("org_admin", "order_items",           "delete", true),
        ("org_admin", "payments",              "create", true),
        ("org_admin", "payments",              "read",   true),
        ("org_admin", "payments",              "update", true),
        ("org_admin", "payments",              "delete", true),
        ("org_admin", "payment_methods",       "create", true),
        ("org_admin", "payment_methods",       "read",   true),
        ("org_admin", "payment_methods",       "update", true),
        ("org_admin", "payment_methods",       "delete", true),
        ("org_admin", "shifts",                "create", true),
        ("org_admin", "shifts",                "read",   true),
        ("org_admin", "shifts",                "update", true),
        ("org_admin", "shifts",                "delete", true),
        ("org_admin", "shift_counts",          "create", true),
        ("org_admin", "shift_counts",          "read",   true),
        ("org_admin", "shift_counts",          "update", true),
        ("org_admin", "shift_counts",          "delete", true),
        ("org_admin", "soft_serve_batches",    "create", true),
        ("org_admin", "soft_serve_batches",    "read",   true),
        ("org_admin", "soft_serve_batches",    "update", true),
        ("org_admin", "soft_serve_batches",    "delete", true),
        ("org_admin", "discounts",             "create", true),
        ("org_admin", "discounts",             "read",   true),
        ("org_admin", "discounts",             "update", true),
        ("org_admin", "discounts",             "delete", true),
        ("org_admin", "reports",               "read",   true),
        ("org_admin", "permissions",           "create", true),
        ("org_admin", "permissions",           "read",   true),
        ("org_admin", "permissions",           "update", true),
        ("org_admin", "permissions",           "delete", true),

        // ── branch_manager: operational access, no org-level management ─
        ("branch_manager", "branches",              "read",   true),
        ("branch_manager", "users",                 "create", true),
        ("branch_manager", "users",                 "read",   true),
        ("branch_manager", "users",                 "update", true),
        ("branch_manager", "categories",            "read",   true),
        ("branch_manager", "menu_items",            "read",   true),
        ("branch_manager", "addon_groups",          "read",   true),
        ("branch_manager", "addon_items",           "read",   true),
        ("branch_manager", "recipes",               "read",   true),
        ("branch_manager", "inventory",             "read",   true),
        ("branch_manager", "inventory",             "update", true),
        ("branch_manager", "inventory_adjustments", "create", true),
        ("branch_manager", "inventory_adjustments", "read",   true),
        ("branch_manager", "inventory_transfers",   "create", true),
        ("branch_manager", "inventory_transfers",   "read",   true),
        ("branch_manager", "inventory_transfers",   "update", true),
        ("branch_manager", "orders",                "create", true),
        ("branch_manager", "orders",                "read",   true),
        ("branch_manager", "orders",                "update", true),
        ("branch_manager", "order_items",           "create", true),
        ("branch_manager", "order_items",           "read",   true),
        ("branch_manager", "order_items",           "update", true),
        ("branch_manager", "payments",              "create", true),
        ("branch_manager", "payments",              "read",   true),
        ("branch_manager", "payments",              "update", true),
        ("branch_manager", "payment_methods",       "read",   true),
        ("branch_manager", "shifts",                "create", true),
        ("branch_manager", "shifts",                "read",   true),
        ("branch_manager", "shifts",                "update", true),
        ("branch_manager", "shift_counts",          "create", true),
        ("branch_manager", "shift_counts",          "read",   true),
        ("branch_manager", "shift_counts",          "update", true),
        ("branch_manager", "soft_serve_batches",    "create", true),
        ("branch_manager", "soft_serve_batches",    "read",   true),
        ("branch_manager", "soft_serve_batches",    "update", true),
        ("branch_manager", "discounts",             "read",   true),
        ("branch_manager", "discounts",             "update", true),
        ("branch_manager", "reports",               "read",   true),

        // ── teller: POS-level access only ─────────────────────────
        ("teller", "branches",           "read",   true),
        ("teller", "categories",         "read",   true),
        ("teller", "menu_items",         "read",   true),
        ("teller", "addon_groups",       "read",   true),
        ("teller", "addon_items",        "read",   true),
        ("teller", "inventory",          "read",   true),
        ("teller", "orders",             "create", true),
        ("teller", "orders",             "read",   true),
        ("teller", "order_items",        "create", true),
        ("teller", "order_items",        "read",   true),
        ("teller", "payments",           "create", true),
        ("teller", "payments",           "read",   true),
        ("teller", "payment_methods",    "read",   true),
        ("teller", "orders",             "update", true), // needed for void_order
        ("teller", "shifts",             "create", true),
        ("teller", "shifts",             "read",   true),
        ("teller", "shifts",             "update", true), // covers cash movements
        ("teller", "shift_counts",       "create", true),
        ("teller", "shift_counts",       "read",   true),
        ("teller", "discounts",          "read",   true),
    ];

    for &(role, resource, action, granted) in defaults {
        sqlx::query(
            r#"
            INSERT INTO role_permissions (role, resource, action, granted)
            VALUES ($1::user_role, $2::permission_resource, $3::permission_action, $4)
            ON CONFLICT (role, resource, action) DO NOTHING
            "#,
        )
        .bind(role)
        .bind(resource)
        .bind(action)
        .bind(granted)
        .execute(pool)
        .await?;
    }

    Ok(())
}
