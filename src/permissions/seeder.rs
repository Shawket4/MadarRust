use sqlx::PgPool;

pub async fn seed_role_permissions(pool: &PgPool) -> Result<(), sqlx::Error> {
    // ── CLEAR CUSTOM USER OVERRIDES (FORCE ROLE LEVEL DEFAULTS) ──
    sqlx::query("DELETE FROM permissions")
        .execute(pool)
        .await?;

    let resources = [
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
        "shifts",
        "shift_counts",
        "soft_serve_batches",
        "permissions",
    ];

    let actions = ["create", "read", "update", "delete"];

    // org_admin gets everything by default
    for &resource in &resources {
        for &action in &actions {
            sqlx::query(
                r#"
                INSERT INTO role_permissions (role, resource, action, granted)
                VALUES ('org_admin'::user_role, $1::permission_resource, $2::permission_action, true)
                ON CONFLICT (role, resource, action) DO UPDATE SET granted = EXCLUDED.granted
                "#
            )
            .bind(resource)
            .bind(action)
            .execute(pool)
            .await?;
        }
    }

    // branch_manager defaults
    let manager_perms = [
        ("branches", "read", true),
        ("users", "create", true),
        ("users", "read", true),
        ("users", "update", true),
        ("categories", "read", true),
        ("menu_items", "read", true),
        ("addon_groups", "read", true),
        ("addon_items", "read", true),
        ("recipes", "read", true),
        ("inventory", "read", true),
        ("inventory", "update", true),
        ("inventory_adjustments", "create", true),
        ("inventory_adjustments", "read", true),
        ("inventory_transfers", "create", true),
        ("inventory_transfers", "read", true),
        ("inventory_transfers", "update", true),
        ("orders", "create", true),
        ("orders", "read", true),
        ("orders", "update", true),
        ("order_items", "create", true),
        ("order_items", "read", true),
        ("order_items", "update", true),
        ("payments", "create", true),
        ("payments", "read", true),
        ("payments", "update", true),
        ("shifts", "create", true),
        ("shifts", "read", true),
        ("shifts", "update", true),
        ("shift_counts", "create", true),
        ("shift_counts", "read", true),
        ("shift_counts", "update", true),
        ("soft_serve_batches", "create", true),
        ("soft_serve_batches", "read", true),
        ("soft_serve_batches", "update", true),
    ];

    // Clear branch_manager defaults first to overwrite cleanly
    sqlx::query("DELETE FROM role_permissions WHERE role = 'branch_manager'::user_role")
        .execute(pool)
        .await?;

    for &(res, act, grant) in &manager_perms {
        sqlx::query(
            r#"
            INSERT INTO role_permissions (role, resource, action, granted)
            VALUES ('branch_manager'::user_role, $1::permission_resource, $2::permission_action, $3)
            ON CONFLICT (role, resource, action) DO UPDATE SET granted = EXCLUDED.granted
            "#
        )
        .bind(res)
        .bind(act)
        .bind(grant)
        .execute(pool)
        .await?;
    }

    // teller defaults
    let teller_perms = [
        ("branches", "read", true),
        ("categories", "read", true),
        ("menu_items", "read", true),
        ("addon_groups", "read", true),
        ("addon_items", "read", true),
        ("inventory", "read", true),
        ("orders", "create", true),
        ("orders", "read", true),
        ("order_items", "create", true),
        ("order_items", "read", true),
        ("payments", "create", true),
        ("payments", "read", true),
        ("shifts", "create", true),
        ("shifts", "read", true),
        ("shifts", "update", true),
        ("shift_counts", "create", true),
        ("shift_counts", "read", true),
    ];

    // Clear teller defaults first to overwrite cleanly
    sqlx::query("DELETE FROM role_permissions WHERE role = 'teller'::user_role")
        .execute(pool)
        .await?;

    for &(res, act, grant) in &teller_perms {
        sqlx::query(
            r#"
            INSERT INTO role_permissions (role, resource, action, granted)
            VALUES ('teller'::user_role, $1::permission_resource, $2::permission_action, $3)
            ON CONFLICT (role, resource, action) DO UPDATE SET granted = EXCLUDED.granted
            "#
        )
        .bind(res)
        .bind(act)
        .bind(grant)
        .execute(pool)
        .await?;
    }

    Ok(())
}
