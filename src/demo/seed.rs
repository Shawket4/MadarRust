//! Seeds a rich "full" demo café so the dashboard is alive on arrival:
//! one branch, payment methods, a small menu (categories → items → sizes),
//! an ingredient catalog with recipes (so cost-coverage is partial, not 0),
//! add-ons, an open shift, and a handful of recent orders so the reports and
//! sales chart have something to show.
//!
//! All values are constants (no user input), enums/jsonb are written as SQL
//! literal casts, and decimals are cast `::numeric` to avoid float surprises.
//! Runs inside the caller's transaction.

use chrono::{Duration, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::errors::AppError;

struct ItemDef {
    cat: usize,
    name: &'static str,
    base: i32,
    sizes: &'static [(&'static str, i32)],
}

pub async fn seed_full(
    conn: &mut PgConnection,
    org_id: Uuid,
    admin_id: Uuid,
) -> Result<(), AppError> {
    // ── Branch + the admin's assignment to it ───────────────────────────────
    let branch_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO branches (id, org_id, name, address, timezone, is_active) \
         VALUES ($1, $2, $3, $4, 'Africa/Cairo', true)",
    )
    .bind(branch_id)
    .bind(org_id)
    .bind("Downtown")
    .bind("12 Tahrir St, Cairo")
    .execute(&mut *conn)
    .await?;

    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(admin_id)
        .bind(branch_id)
        .execute(&mut *conn)
        .await?;

    // ── Payment methods ─────────────────────────────────────────────────────
    let pms = [
        (
            "Cash",
            "#22c55e",
            "cash",
            true,
            r#"{"en":"Cash","ar":"نقد"}"#,
        ),
        (
            "Visa",
            "#2563eb",
            "credit_card",
            false,
            r#"{"en":"Visa","ar":"فيزا"}"#,
        ),
        (
            "InstaPay",
            "#7c3aed",
            "wallet",
            false,
            r#"{"en":"InstaPay","ar":"إنستا باي"}"#,
        ),
    ];
    for (name, color, icon, is_cash, labels) in pms.iter() {
        sqlx::query(
            "INSERT INTO org_payment_methods \
             (id, org_id, name, label_translations, color, icon, is_cash) \
             VALUES ($1, $2, $3, $4::jsonb, $5, $6, $7)",
        )
        .bind(Uuid::new_v4())
        .bind(org_id)
        .bind(name)
        .bind(labels)
        .bind(color)
        .bind(icon)
        .bind(is_cash)
        .execute(&mut *conn)
        .await?;
    }

    // ── Categories ──────────────────────────────────────────────────────────
    let cat_names = ["Hot Drinks", "Cold Drinks", "Pastries"];
    let mut cat_ids = Vec::with_capacity(cat_names.len());
    for name in cat_names.iter() {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(org_id)
            .bind(name)
            .execute(&mut *conn)
            .await?;
        cat_ids.push(id);
    }

    // ── Menu items + sizes ──────────────────────────────────────────────────
    let items = [
        ItemDef {
            cat: 0,
            name: "Espresso",
            base: 3500,
            sizes: &[("Single", 3500), ("Double", 5000)],
        },
        ItemDef {
            cat: 0,
            name: "Latte",
            base: 6000,
            sizes: &[("Regular", 6000), ("Large", 7500)],
        },
        ItemDef {
            cat: 1,
            name: "Iced Latte",
            base: 7000,
            sizes: &[("Regular", 7000), ("Large", 8500)],
        },
        ItemDef {
            cat: 1,
            name: "Lemonade",
            base: 5000,
            sizes: &[("one_size", 5000)],
        },
        ItemDef {
            cat: 2,
            name: "Croissant",
            base: 4500,
            sizes: &[("one_size", 4500)],
        },
        ItemDef {
            cat: 2,
            name: "Cheesecake",
            base: 8000,
            sizes: &[("one_size", 8000)],
        },
    ];
    let mut item_ids = Vec::with_capacity(items.len());
    // (item index, size label) → menu_item_sizes.id, for the recipe_lines below.
    let mut size_ids: std::collections::HashMap<(usize, String), Uuid> =
        std::collections::HashMap::new();
    for it in items.iter() {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO menu_items (id, org_id, category_id, name, base_price) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(org_id)
        .bind(cat_ids[it.cat])
        .bind(it.name)
        .bind(it.base)
        .execute(&mut *conn)
        .await?;
        for (sort, (label, price)) in it.sizes.iter().enumerate() {
            let size_id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO menu_item_sizes (id, menu_item_id, label, price, sort) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(size_id)
            .bind(id)
            .bind(label)
            .bind(price)
            .bind(sort as i32)
            .execute(&mut *conn)
            .await?;
            size_ids.insert((item_ids.len(), (*label).to_string()), size_id);
        }
        item_ids.push(id);
    }

    // ── Ingredient catalog (cost_per_unit in piastres) ──────────────────────
    let ings: [(&str, &str, i64); 6] = [
        ("Espresso Beans", "kg", 30000),
        ("Whole Milk", "l", 2500),
        ("Vanilla Syrup", "l", 12000),
        ("Sugar", "kg", 2000),
        ("Lemon", "pcs", 300),
        ("Flour", "kg", 1500),
    ];
    let mut ing_ids: Vec<(Uuid, &str, &str)> = Vec::with_capacity(ings.len());
    for (name, unit, cost) in ings.iter() {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category) \
             VALUES ($1, $2, $3, $4::inventory_unit, $5, 'general')",
        )
        .bind(id)
        .bind(org_id)
        .bind(name)
        .bind(unit)
        .bind(cost)
        .execute(&mut *conn)
        .await?;
        ing_ids.push((id, name, unit));
    }

    // ── Recipes → cost coverage for 4 of 6 items ────────────────────────────
    let recipes: &[(usize, &str, usize, f64)] = &[
        (0, "Single", 0, 0.018),
        (1, "Regular", 0, 0.018),
        (1, "Regular", 1, 0.240),
        (1, "Regular", 2, 0.015),
        (2, "Regular", 0, 0.018),
        (2, "Regular", 1, 0.200),
        (3, "one_size", 4, 2.0),
        (3, "one_size", 3, 0.030),
    ];
    for (it, size, ing, qty) in recipes.iter() {
        let (ing_id, _ing_name, ing_unit) = ing_ids[*ing];
        // Unified model: id-keyed recipe lines owned by the size row.
        let size_id = size_ids
            .get(&(*it, (*size).to_string()))
            .copied()
            .expect("demo recipe references a seeded size");
        sqlx::query(
            "INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit) \
             VALUES ('item_size', $1, $2, $3::numeric, $4)",
        )
        .bind(size_id)
        .bind(ing_id)
        .bind(qty)
        .bind(ing_unit)
        .execute(&mut *conn)
        .await?;
    }

    // ── Add-ons (unified model: one reusable group + its options) ───────────
    let group_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO modifier_groups \
             (id, org_id, name, selection_type, min_selections, is_required, legacy_addon_type) \
         VALUES ($1, $2, 'Extras', 'multi', 0, false, 'extras')",
    )
    .bind(group_id)
    .bind(org_id)
    .execute(&mut *conn)
    .await?;
    let addons = [("Extra Shot", 1500i32, 0i32), ("Oat Milk", 1000, 1)];
    for (name, price, sort) in addons.iter() {
        sqlx::query(
            "INSERT INTO modifier_options (id, group_id, name, price, sort, legacy_source) \
             VALUES ($1, $2, $3, $4, $5, 'addon')",
        )
        .bind(Uuid::new_v4())
        .bind(group_id)
        .bind(name)
        .bind(price)
        .bind(sort)
        .execute(&mut *conn)
        .await?;
    }
    // Offer the group on the drink items (0..=3) so the demo shows grouped
    // modifiers in the new clients; old clients see the same options through
    // the shim's flat addon catalog.
    for item_id in item_ids.iter().take(4) {
        sqlx::query(
            "INSERT INTO menu_item_modifier_groups (menu_item_id, group_id, sort) \
             VALUES ($1, $2, 0)",
        )
        .bind(item_id)
        .bind(group_id)
        .execute(&mut *conn)
        .await?;
    }
    // Seed the org's catalog revision so new clients can revision-gate syncs.
    sqlx::query(
        "INSERT INTO catalog_revision (org_id, revision) VALUES ($1, 1) \
         ON CONFLICT (org_id) DO NOTHING",
    )
    .bind(org_id)
    .execute(&mut *conn)
    .await?;

    // ── An open shift (teller = the demo admin) ─────────────────────────────
    let shift_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO shifts (id, branch_id, teller_id, status, opening_cash, opened_at) \
         VALUES ($1, $2, $3, 'open'::shift_status, 50000, now())",
    )
    .bind(shift_id)
    .bind(branch_id)
    .bind(admin_id)
    .execute(&mut *conn)
    .await?;

    // ── A handful of recent completed orders for the reports/chart ──────────
    let order_specs: &[(usize, &str, i32, i32)] = &[
        (1, "Regular", 6000, 2),
        (0, "Double", 5000, 1),
        (2, "Regular", 7000, 1),
        (4, "one_size", 4500, 3),
        (5, "one_size", 8000, 1),
        (3, "one_size", 5000, 2),
        (1, "Large", 7500, 1),
        (0, "Single", 3500, 2),
    ];
    for (n, (it, size, price, qty)) in order_specs.iter().enumerate() {
        let order_id = Uuid::new_v4();
        let subtotal = price * qty;
        let tax = (subtotal as f64 * 0.14).round() as i32;
        let total = subtotal + tax;
        let created = Utc::now() - Duration::days((n as i64) % 10);
        // order_ref is NOT NULL with a global UNIQUE index and no generator trigger
        // (the POS mints it from order_ref_counters). Derive a unique one from the
        // per-order UUID so concurrent demo orgs never collide.
        let order_ref = format!("DMO-{}", order_id.simple());
        sqlx::query(
            "INSERT INTO orders \
             (id, branch_id, shift_id, teller_id, order_number, status, payment_method, \
              subtotal, tax_amount, total_amount, created_at, order_ref) \
             VALUES ($1, $2, $3, $4, $5, 'completed'::order_status, 'Cash', $6, $7, $8, $9, $10)",
        )
        .bind(order_id)
        .bind(branch_id)
        .bind(shift_id)
        .bind(admin_id)
        .bind((n as i32) + 1)
        .bind(subtotal)
        .bind(tax)
        .bind(total)
        .bind(created)
        .bind(order_ref)
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "INSERT INTO order_items \
             (id, order_id, menu_item_id, item_name, size_label, unit_price, quantity, line_total) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(Uuid::new_v4())
        .bind(order_id)
        .bind(item_ids[*it])
        .bind(items[*it].name)
        .bind(size)
        .bind(price)
        .bind(qty)
        .bind(subtotal)
        .execute(&mut *conn)
        .await?;
    }

    Ok(())
}
