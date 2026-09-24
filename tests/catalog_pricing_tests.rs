//! Catalogue pricing: the server's rule, pinned, and madar-shared's
//! `madar-catalog` vectors generated from it.
//!
//! A fixture catalogue covers every case where the till used to price a line
//! differently (discovery M4 / M5: an explicit `swaps` group, two options
//! sharing the recipe's ingredient, a multi-select swap pick, a size the item
//! no longer sells, a branch's item price) and the ordinary ones (sizes,
//! defaults, add-ons, optional fields, bundle components).
//!
//! `server_capture.json` is what the server's order path — `catalog_unit_price`
//! and `resolve_menu_item_configuration` — answered for every case BEFORE the
//! rule moved into madar-catalog, stock deductions included. It is the pin the
//! move is held to: regenerate it only for a deliberate change of the rule
//! (`MADAR_WRITE_CATALOG_CAPTURE=1`).
#![allow(clippy::too_many_arguments)]

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::JwtSecret;
use madar_rust::models::UserRole;
use madar_rust::orders::component_resolve::{AddonInput, resolve_menu_item_configuration};
use madar_rust::orders::handlers::catalog_unit_price;

const CAPTURE: &str = "tests/fixtures/catalog_pricing/server_capture.json";

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

/// Fixed ids, so every run writes the same bytes.
const fn id(n: u128) -> Uuid {
    Uuid::from_u128(0xc0de_0000_0000_4000_8000_0000_0000_0000 | n)
}

pub const ORG: Uuid = id(0x01);
pub const BRANCH: Uuid = id(0x02);
pub const USER: Uuid = id(0x03);
pub const TILL: Uuid = id(0x04);
pub const CATEGORY: Uuid = id(0x05);

// Ingredient categories.
const C_MILK: Uuid = id(0x10);
const C_COFFEE: Uuid = id(0x11);
const C_TEA: Uuid = id(0x12);
const C_SYRUP: Uuid = id(0x13);
const C_GENERAL: Uuid = id(0x14);

// Ingredients.
const I_WHOLE: Uuid = id(0x20);
const I_OAT: Uuid = id(0x21);
const I_ALMOND: Uuid = id(0x22);
const I_OAT_BARISTA: Uuid = id(0x23);
const I_SKIM: Uuid = id(0x24);
const I_SOY: Uuid = id(0x25);
const I_BEAN: Uuid = id(0x26);
const I_DECAF: Uuid = id(0x27);
const I_BLACK: Uuid = id(0x28);
const I_GREEN: Uuid = id(0x29);
const I_WHITE: Uuid = id(0x2a);
const I_VANILLA: Uuid = id(0x2b);
const I_CARAMEL: Uuid = id(0x2c);
const I_WATER: Uuid = id(0x2d);
const I_BUTTER: Uuid = id(0x2e);
const I_CREAM: Uuid = id(0x2f);
const I_BEAN_KG: Uuid = id(0x30);

// Modifier groups.
const G_MILK: Uuid = id(0x40);
const G_MILK_ALT: Uuid = id(0x41);
const G_COFFEE_A: Uuid = id(0x42);
const G_COFFEE_B: Uuid = id(0x43);
const G_TEA: Uuid = id(0x44);
const G_SYRUP: Uuid = id(0x45);
const G_EXTRAS: Uuid = id(0x46);

// Options (add-on items; a grouped one is also a modifier option, same id).
const A_WHOLE: Uuid = id(0x50);
const A_BARISTA_WHOLE: Uuid = id(0x51);
const A_OAT: Uuid = id(0x52);
const A_ALMOND: Uuid = id(0x53);
const A_OLD_MILK: Uuid = id(0x54);
const A_MYSTERY_MILK: Uuid = id(0x55);
const A_SKIM: Uuid = id(0x56);
const A_WHOLE_ALT: Uuid = id(0x57);
const A_SOY_ALT: Uuid = id(0x58);
const A_ESPRESSO: Uuid = id(0x59);
const A_DECAF: Uuid = id(0x5a);
const A_COLOMBIAN: Uuid = id(0x5b);
const A_BLACK: Uuid = id(0x5c);
const A_GREEN: Uuid = id(0x5d);
const A_WHITE: Uuid = id(0x5e);
const A_VANILLA: Uuid = id(0x5f);
const A_CARAMEL: Uuid = id(0x60);
const A_SHOT: Uuid = id(0x61);
const A_SPRINKLES: Uuid = id(0x62);
const A_KG_BEAN: Uuid = id(0x63);

// Menu items.
const M_LATTE: Uuid = id(0x70);
const M_VLATTE: Uuid = id(0x71);
const M_TEA: Uuid = id(0x72);
const M_AMERICANO: Uuid = id(0x73);
const M_CROISSANT: Uuid = id(0x74);
const M_CAPPUCCINO: Uuid = id(0x75);
const M_RETIRED: Uuid = id(0x76);

// Optional fields.
const O_HOT: Uuid = id(0x80);
const O_CREAM: Uuid = id(0x81);
const O_OFF: Uuid = id(0x82);

// Bundle.
pub const B_BREAKFAST: Uuid = id(0x90);

async fn exec(pool: &PgPool, sql: &str) {
    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        sqlx::query(stmt)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("{e}: {stmt}"));
    }
}

async fn ingredient(pool: &PgPool, id: Uuid, name: &str, unit: &str, cat: Uuid) {
    sqlx::query(
        "INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category_id) \
         VALUES ($1, $2, $3, $4::inventory_unit, 1, $5)",
    )
    .bind(id)
    .bind(ORG)
    .bind(name)
    .bind(unit)
    .bind(cat)
    .execute(pool)
    .await
    .unwrap();
}

async fn group(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    legacy: Option<&str>,
    effect: &str,
    swap_category: Option<Uuid>,
) {
    sqlx::query(
        "INSERT INTO modifier_groups (id, org_id, name, legacy_addon_type, effect, swap_category_id) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(ORG)
    .bind(name)
    .bind(legacy)
    .bind(effect)
    .bind(swap_category)
    .execute(pool)
    .await
    .unwrap();
}

/// An add-on item; with a group, also its modifier option (same id).
async fn option(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    kind: &str,
    price: i32,
    group: Option<(Uuid, i32)>,
    active: bool,
    replaces: Option<Uuid>,
) {
    sqlx::query(
        "INSERT INTO addon_items (id, org_id, name, type, default_price, is_active) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(ORG)
    .bind(name)
    .bind(kind)
    .bind(price)
    .bind(active)
    .execute(pool)
    .await
    .unwrap();
    if let Some((g, sort)) = group {
        sqlx::query(
            "INSERT INTO modifier_options (id, group_id, name, price, sort, is_active, replaces_ingredient_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id)
        .bind(g)
        .bind(name)
        .bind(price)
        .bind(sort)
        .bind(active)
        .bind(replaces)
        .execute(pool)
        .await
        .unwrap();
    }
}

async fn option_line(pool: &PgPool, option: Uuid, ing: Uuid, name: &str, unit: &str, qty: f64) {
    sqlx::query(
        "INSERT INTO addon_item_ingredients (addon_item_id, org_ingredient_id, quantity_used, ingredient_name, ingredient_unit) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(option)
    .bind(ing)
    .bind(qty)
    .bind(name)
    .bind(unit)
    .execute(pool)
    .await
    .unwrap();
}

async fn sized_option_line(
    pool: &PgPool,
    option: Uuid,
    size: &str,
    ing: Uuid,
    unit: &str,
    qty: f64,
) {
    sqlx::query(
        "INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit, size_label) \
         VALUES ('modifier_option', $1, $2, $3, $4, $5)",
    )
    .bind(option)
    .bind(ing)
    .bind(qty)
    .bind(unit)
    .bind(size)
    .execute(pool)
    .await
    .unwrap();
}

/// An item with its sizes, created in one transaction so the synthetic
/// `one_size` row retires (an item with no sizes keeps it).
async fn item(pool: &PgPool, id: Uuid, name: &str, sizes: &[(&str, i32, i32, bool)]) {
    let mut tx = pool.begin().await.unwrap();
    let base = sizes.iter().map(|s| s.1).min().unwrap_or(2500);
    sqlx::query(
        "INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) \
         VALUES ($1, $2, $3, $4, $5, true)",
    )
    .bind(id)
    .bind(ORG)
    .bind(CATEGORY)
    .bind(name)
    .bind(base)
    .execute(&mut *tx)
    .await
    .unwrap();
    // Active sizes first: an item must always keep an active one.
    let mut ordered: Vec<_> = sizes.to_vec();
    ordered.sort_by_key(|s| !s.3);
    for (label, price, sort, active) in ordered {
        sqlx::query(
            "INSERT INTO menu_item_sizes (menu_item_id, label, price, sort, is_active) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(label)
        .bind(price)
        .bind(sort)
        .bind(active)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
}

async fn recipe(
    pool: &PgPool,
    item: Uuid,
    size: &str,
    ing: Uuid,
    name: &str,
    unit: &str,
    qty: f64,
) {
    sqlx::query(
        "INSERT INTO menu_item_recipes (menu_item_id, size_label, org_ingredient_id, quantity_used, ingredient_name, ingredient_unit) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(item)
    .bind(size)
    .bind(ing)
    .bind(qty)
    .bind(name)
    .bind(unit)
    .execute(pool)
    .await
    .unwrap();
}

async fn optional(
    pool: &PgPool,
    id: Uuid,
    item: Uuid,
    name: &str,
    price: i32,
    size: Option<&str>,
    ing: Option<(Uuid, &str, &str, f64)>,
    active: bool,
) {
    sqlx::query(
        "INSERT INTO menu_item_optional_fields \
             (id, menu_item_id, name, price, size_label, org_ingredient_id, ingredient_name, ingredient_unit, quantity_used, is_active) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(id)
    .bind(item)
    .bind(name)
    .bind(price)
    .bind(size)
    .bind(ing.map(|i| i.0))
    .bind(ing.map(|i| i.1))
    .bind(ing.map(|i| i.2))
    .bind(ing.map(|i| i.3))
    .bind(active)
    .execute(pool)
    .await
    .unwrap();
}

/// The fixture catalogue, at branch [`BRANCH`].
pub async fn seed(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO organizations (id, name, slug) VALUES ($1, 'Catalog Org', 'catalog-org')",
    )
    .bind(ORG)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) \
         VALUES ($1, 'cash', '{}', 'emerald', 'payments_outlined', true, true)",
    )
    .bind(ORG)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Catalog Branch')")
        .bind(BRANCH)
        .bind(ORG)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, 'Owner', 'catalog-owner@test.com', 'hash', 'org_admin'::user_role)",
    )
    .bind(USER)
    .bind(ORG)
    .execute(pool)
    .await
    .unwrap();
    for (res, act) in [("orders", "create"), ("menu_items", "read")] {
        sqlx::query(&format!(
            "INSERT INTO role_permissions (role, resource, action, granted) \
             VALUES ('org_admin'::user_role, '{res}'::permission_resource, '{act}'::permission_action, true) \
             ON CONFLICT DO NOTHING"
        ))
        .execute(pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO tills (id, branch_id, teller_id, status, opening_cash) VALUES ($1, $2, $3, 'open', 10000)",
    )
    .bind(TILL)
    .bind(BRANCH)
    .bind(USER)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'Drinks')")
        .bind(CATEGORY)
        .bind(ORG)
        .execute(pool)
        .await
        .unwrap();

    for (cid, slug) in [
        (C_MILK, "milk"),
        (C_COFFEE, "coffee_bean"),
        (C_TEA, "tea"),
        (C_SYRUP, "syrup"),
        (C_GENERAL, "general"),
    ] {
        sqlx::query(
            "INSERT INTO ingredient_categories (id, org_id, slug, name, sort_order) \
             VALUES ($1, $2, $3, $3, 10) \
             ON CONFLICT (org_id, slug) DO NOTHING",
        )
        .bind(cid)
        .bind(ORG)
        .bind(slug)
        .execute(pool)
        .await
        .unwrap();
    }
    // A category the org may have been given already keeps its id: read back.
    for (ing, name, unit, slug) in [
        (I_WHOLE, "Whole milk", "ml", "milk"),
        (I_OAT, "Oat milk", "ml", "milk"),
        (I_ALMOND, "Almond milk", "ml", "milk"),
        (I_OAT_BARISTA, "Oat barista", "ml", "milk"),
        (I_SKIM, "Skim milk", "ml", "milk"),
        (I_SOY, "Soy milk", "ml", "milk"),
        (I_BEAN, "Espresso bean", "g", "coffee_bean"),
        (I_DECAF, "Decaf bean", "g", "coffee_bean"),
        (I_BEAN_KG, "Bulk bean", "kg", "coffee_bean"),
        (I_BLACK, "Black tea", "g", "tea"),
        (I_GREEN, "Green tea", "g", "tea"),
        (I_WHITE, "White tea", "g", "tea"),
        (I_VANILLA, "Vanilla syrup", "ml", "syrup"),
        (I_CARAMEL, "Caramel syrup", "ml", "syrup"),
        (I_WATER, "Water", "ml", "general"),
        (I_BUTTER, "Butter", "g", "general"),
        (I_CREAM, "Cream", "ml", "general"),
    ] {
        let cat: Uuid = sqlx::query_scalar(
            "SELECT id FROM ingredient_categories WHERE org_id = $1 AND slug = $2",
        )
        .bind(ORG)
        .bind(slug)
        .fetch_one(pool)
        .await
        .unwrap();
        ingredient(pool, ing, name, unit, cat).await;
    }
    let cat_of = |slug: &'static str| async move {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM ingredient_categories WHERE org_id = $1 AND slug = $2",
        )
        .bind(ORG)
        .bind(slug)
        .fetch_one(pool)
        .await
        .unwrap()
    };
    let tea = cat_of("tea").await;
    let syrup = cat_of("syrup").await;

    // Groups. The two magic families get effect `swaps` + their category from
    // the table's trigger; tea and syrup are explicit swap groups.
    group(pool, G_MILK, "Milk", Some("milk_type"), "adds", None).await;
    group(
        pool,
        G_MILK_ALT,
        "Milk (alt)",
        Some("milk_type"),
        "adds",
        None,
    )
    .await;
    group(pool, G_COFFEE_A, "Beans", Some("coffee_type"), "adds", None).await;
    group(
        pool,
        G_COFFEE_B,
        "Espresso beans",
        Some("coffee_type"),
        "adds",
        None,
    )
    .await;
    group(pool, G_TEA, "Tea", Some("extra"), "swaps", Some(tea)).await;
    group(pool, G_SYRUP, "Syrup", Some("extra"), "swaps", Some(syrup)).await;
    group(pool, G_EXTRAS, "Extras", Some("extra"), "adds", None).await;

    // Milk: the recipe's whole milk twice (Whole at 0, Barista whole at 300,
    // drift M4b), an inactive one ahead of both, a branch-priced oat with a
    // per-size line, a branch-disabled almond, one with no lines, and a
    // legacy skim with no group.
    option(
        pool,
        A_OLD_MILK,
        "Old milk",
        "milk_type",
        100,
        Some((G_MILK, 0)),
        false,
        None,
    )
    .await;
    option(
        pool,
        A_WHOLE,
        "Whole milk",
        "milk_type",
        0,
        Some((G_MILK, 1)),
        true,
        None,
    )
    .await;
    option(
        pool,
        A_BARISTA_WHOLE,
        "Barista whole",
        "milk_type",
        300,
        Some((G_MILK, 2)),
        true,
        None,
    )
    .await;
    option(
        pool,
        A_OAT,
        "Oat milk",
        "milk_type",
        700,
        Some((G_MILK, 3)),
        true,
        None,
    )
    .await;
    option(
        pool,
        A_ALMOND,
        "Almond milk",
        "milk_type",
        650,
        Some((G_MILK, 4)),
        true,
        None,
    )
    .await;
    option(
        pool,
        A_MYSTERY_MILK,
        "Mystery milk",
        "milk_type",
        400,
        Some((G_MILK, 5)),
        true,
        None,
    )
    .await;
    option(
        pool,
        A_SKIM,
        "Skim milk",
        "milk_type",
        350,
        None,
        true,
        None,
    )
    .await;
    option(
        pool,
        A_WHOLE_ALT,
        "Whole (alt)",
        "milk_type",
        150,
        Some((G_MILK_ALT, 1)),
        true,
        None,
    )
    .await;
    option(
        pool,
        A_SOY_ALT,
        "Soy milk",
        "milk_type",
        600,
        Some((G_MILK_ALT, 2)),
        true,
        None,
    )
    .await;
    option_line(pool, A_OLD_MILK, I_WHOLE, "Whole milk", "ml", 200.0).await;
    option_line(pool, A_WHOLE, I_WHOLE, "Whole milk", "ml", 200.0).await;
    option_line(pool, A_BARISTA_WHOLE, I_WHOLE, "Whole milk", "ml", 200.0).await;
    option_line(pool, A_OAT, I_OAT, "Oat milk", "ml", 200.0).await;
    sized_option_line(pool, A_OAT, "Large", I_OAT_BARISTA, "ml", 300.0).await;
    option_line(pool, A_ALMOND, I_ALMOND, "Almond milk", "ml", 200.0).await;
    option_line(pool, A_SKIM, I_SKIM, "Skim milk", "ml", 200.0).await;
    option_line(pool, A_WHOLE_ALT, I_WHOLE, "Whole milk", "ml", 200.0).await;
    option_line(pool, A_SOY_ALT, I_SOY, "Soy milk", "ml", 200.0).await;

    // Coffee: two groups carry the recipe's bean (House at 0, Colombian at
    // 200); decaf sits beside Colombian, so it is charged over Colombian.
    option(
        pool,
        A_ESPRESSO,
        "House espresso",
        "coffee_type",
        0,
        Some((G_COFFEE_A, 1)),
        true,
        None,
    )
    .await;
    option(
        pool,
        A_COLOMBIAN,
        "Colombian espresso",
        "coffee_type",
        200,
        Some((G_COFFEE_B, 1)),
        true,
        None,
    )
    .await;
    option(
        pool,
        A_DECAF,
        "Decaf",
        "coffee_type",
        500,
        Some((G_COFFEE_B, 2)),
        true,
        None,
    )
    .await;
    option(
        pool,
        A_KG_BEAN,
        "Bulk bean",
        "coffee_type",
        250,
        Some((G_COFFEE_B, 3)),
        true,
        None,
    )
    .await;
    option_line(pool, A_ESPRESSO, I_BEAN, "Espresso bean", "g", 18.0).await;
    option_line(pool, A_COLOMBIAN, I_BEAN, "Espresso bean", "g", 18.0).await;
    option_line(pool, A_DECAF, I_DECAF, "Decaf bean", "g", 18.0).await;
    option_line(pool, A_KG_BEAN, I_BEAN_KG, "Bulk bean", "kg", 0.018).await;

    // Tea: an explicit swap group (drift M4a); White names its replacement
    // but carries only a water line.
    option(
        pool,
        A_BLACK,
        "Black tea",
        "extra",
        300,
        Some((G_TEA, 1)),
        true,
        Some(I_BLACK),
    )
    .await;
    option(
        pool,
        A_GREEN,
        "Green tea",
        "extra",
        700,
        Some((G_TEA, 2)),
        true,
        Some(I_GREEN),
    )
    .await;
    option(
        pool,
        A_WHITE,
        "White tea",
        "extra",
        550,
        Some((G_TEA, 3)),
        true,
        Some(I_WHITE),
    )
    .await;
    option_line(pool, A_BLACK, I_BLACK, "Black tea", "g", 3.0).await;
    option_line(pool, A_GREEN, I_GREEN, "Green tea", "g", 3.0).await;
    option_line(pool, A_WHITE, I_WATER, "Water", "ml", 50.0).await;

    // Syrup: an explicit swap group a till picked twice (drift M4c).
    option(
        pool,
        A_VANILLA,
        "Vanilla",
        "extra",
        200,
        Some((G_SYRUP, 1)),
        true,
        None,
    )
    .await;
    option(
        pool,
        A_CARAMEL,
        "Caramel",
        "extra",
        250,
        Some((G_SYRUP, 2)),
        true,
        None,
    )
    .await;
    option_line(pool, A_VANILLA, I_VANILLA, "Vanilla syrup", "ml", 15.0).await;
    option_line(pool, A_CARAMEL, I_CARAMEL, "Caramel syrup", "ml", 15.0).await;

    // Extras: an extra shot (follows the drink's bean), sprinkles (no lines).
    option(
        pool,
        A_SHOT,
        "Extra shot",
        "extra",
        150,
        Some((G_EXTRAS, 1)),
        true,
        None,
    )
    .await;
    option(
        pool,
        A_SPRINKLES,
        "Sprinkles",
        "extra",
        50,
        Some((G_EXTRAS, 2)),
        true,
        None,
    )
    .await;
    option_line(pool, A_SHOT, I_BEAN, "Espresso bean", "g", 9.0).await;

    // The branch: oat at 800, almond switched off (still priced if rung).
    exec(
        pool,
        &format!(
            "INSERT INTO branch_addon_overrides (branch_id, addon_item_id, price_override, is_available) VALUES \
             ('{BRANCH}', '{A_OAT}', 800, true), ('{BRANCH}', '{A_ALMOND}', NULL, false)"
        ),
    )
    .await;

    // Items.
    item(
        pool,
        M_LATTE,
        "Latte",
        &[
            ("Small", 3000, 1, true),
            ("Large", 3800, 2, true),
            ("Medium", 3400, 3, false),
        ],
    )
    .await;
    item(
        pool,
        M_VLATTE,
        "Vanilla latte",
        &[("Regular", 3500, 1, true)],
    )
    .await;
    item(
        pool,
        M_TEA,
        "Tea",
        &[("Pot", 2500, 2, true), ("Cup", 1500, 1, true)],
    )
    .await;
    item(
        pool,
        M_AMERICANO,
        "Americano",
        &[("Regular", 2000, 1, true)],
    )
    .await;
    item(pool, M_CROISSANT, "Croissant", &[]).await;
    item(
        pool,
        M_CAPPUCCINO,
        "Cappuccino",
        &[("Small", 2800, 1, true), ("Large", 3300, 2, true)],
    )
    .await;
    item(pool, M_RETIRED, "Retired", &[("Regular", 1000, 1, true)]).await;

    for size in ["Small", "Large", "Medium"] {
        recipe(pool, M_LATTE, size, I_BEAN, "Espresso bean", "g", 18.0).await;
        recipe(pool, M_LATTE, size, I_WHOLE, "Whole milk", "ml", 200.0).await;
    }
    // Inserted in name order: the order a recipe's lines are read in (see
    // `orders::catalog_view`).
    recipe(
        pool,
        M_VLATTE,
        "Regular",
        I_BEAN,
        "Espresso bean",
        "g",
        18.0,
    )
    .await;
    recipe(
        pool,
        M_VLATTE,
        "Regular",
        I_VANILLA,
        "Vanilla syrup",
        "ml",
        15.0,
    )
    .await;
    recipe(
        pool,
        M_VLATTE,
        "Regular",
        I_WHOLE,
        "Whole milk",
        "ml",
        200.0,
    )
    .await;
    for size in ["Cup", "Pot"] {
        recipe(pool, M_TEA, size, I_BLACK, "Black tea", "g", 3.0).await;
        recipe(pool, M_TEA, size, I_WATER, "Water", "ml", 250.0).await;
    }
    recipe(
        pool,
        M_AMERICANO,
        "Regular",
        I_BEAN,
        "Espresso bean",
        "g",
        18.0,
    )
    .await;
    recipe(pool, M_AMERICANO, "Regular", I_WATER, "Water", "ml", 200.0).await;
    recipe(pool, M_CROISSANT, "one_size", I_BUTTER, "Butter", "g", 10.0).await;
    recipe(
        pool,
        M_CAPPUCCINO,
        "Small",
        I_BEAN,
        "Espresso bean",
        "g",
        18.0,
    )
    .await;
    recipe(
        pool,
        M_CAPPUCCINO,
        "Small",
        I_WHOLE,
        "Whole milk",
        "ml",
        120.0,
    )
    .await;

    // The branch sells the latte at 3500 (M5), its Large at 4000, and prices a
    // "Jumbo" the item has no size for.
    exec(
        pool,
        &format!(
            "INSERT INTO branch_menu_overrides (branch_id, menu_item_id, price_override, is_available) \
             VALUES ('{BRANCH}', '{M_LATTE}', 3500, true); \
             INSERT INTO branch_menu_size_overrides (branch_id, menu_item_id, size_label, price_override) VALUES \
             ('{BRANCH}', '{M_LATTE}', 'Large', 4000), ('{BRANCH}', '{M_LATTE}', 'Jumbo', 4500)"
        ),
    )
    .await;

    optional(pool, O_HOT, M_LATTE, "Extra hot", 0, None, None, true).await;
    optional(
        pool,
        O_CREAM,
        M_LATTE,
        "Whipped cream",
        150,
        Some("Large"),
        Some((I_CREAM, "Cream", "ml", 20.0)),
        true,
    )
    .await;
    optional(
        pool,
        O_OFF,
        M_LATTE,
        "Retired topping",
        99,
        None,
        None,
        false,
    )
    .await;

    // An item whose every size is off: impossible through the schema's guard,
    // so the guard is stepped around for this one row.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE menu_item_sizes SET is_active = false WHERE menu_item_id = $1")
        .bind(M_RETIRED)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // A breakfast bundle: two lattes and a croissant.
    exec(
        pool,
        &format!(
            "INSERT INTO bundles (id, org_id, name, price, status) VALUES ('{B_BREAKFAST}', '{ORG}', 'Breakfast', 5000, 'active'); \
             INSERT INTO bundle_components (bundle_id, item_id, quantity, position) VALUES \
             ('{B_BREAKFAST}', '{M_LATTE}', 2, 0), ('{B_BREAKFAST}', '{M_CROISSANT}', 1, 1)"
        ),
    )
    .await;
}

/// One priced line of the fixture.
pub struct Case {
    pub name: &'static str,
    pub item: Uuid,
    /// `line` (a menu-item line) or `component` (a bundle component).
    pub part: &'static str,
    pub size: Option<&'static str>,
    pub options: Vec<(Uuid, i32)>,
    pub optionals: Vec<Uuid>,
}

fn case(
    name: &'static str,
    item: Uuid,
    size: Option<&'static str>,
    options: &[(Uuid, i32)],
    optionals: &[Uuid],
) -> Case {
    Case {
        name,
        item,
        part: "line",
        size,
        options: options.to_vec(),
        optionals: optionals.to_vec(),
    }
}

fn component(
    name: &'static str,
    item: Uuid,
    size: Option<&'static str>,
    options: &[(Uuid, i32)],
    optionals: &[Uuid],
) -> Case {
    Case {
        part: "component",
        ..case(name, item, size, options, optionals)
    }
}

pub fn cases() -> Vec<Case> {
    let s = Some("Small");
    let l = Some("Large");
    vec![
        // Sizes (M5).
        case("latte_small", M_LATTE, s, &[], &[]),
        case("latte_large_branch_size_price", M_LATTE, l, &[], &[]),
        case(
            "latte_inactive_size_falls_back_to_the_branch_item_price",
            M_LATTE,
            Some("Medium"),
            &[],
            &[],
        ),
        case(
            "latte_size_only_the_branch_prices",
            M_LATTE,
            Some("Jumbo"),
            &[],
            &[],
        ),
        case("latte_unknown_size", M_LATTE, Some("Venti"), &[], &[]),
        case(
            "latte_no_size_is_the_branch_item_price",
            M_LATTE,
            None,
            &[],
            &[],
        ),
        case(
            "cappuccino_no_size_is_the_lowest_size",
            M_CAPPUCCINO,
            None,
            &[],
            &[],
        ),
        case("cappuccino_large", M_CAPPUCCINO, Some("Large"), &[], &[]),
        case("croissant_one_size", M_CROISSANT, None, &[], &[]),
        case(
            "retired_has_no_priced_size",
            M_RETIRED,
            Some("Regular"),
            &[],
            &[],
        ),
        // Milk.
        case(
            "latte_whole_is_the_recipe",
            M_LATTE,
            s,
            &[(A_WHOLE, 1)],
            &[],
        ),
        case(
            "latte_barista_whole_shares_the_recipe_milk",
            M_LATTE,
            s,
            &[(A_BARISTA_WHOLE, 1)],
            &[],
        ),
        case(
            "latte_oat_at_the_branch_price",
            M_LATTE,
            s,
            &[(A_OAT, 1)],
            &[],
        ),
        case(
            "latte_large_oat_per_size_line",
            M_LATTE,
            l,
            &[(A_OAT, 1)],
            &[],
        ),
        case("latte_oat_no_size", M_LATTE, None, &[(A_OAT, 1)], &[]),
        case(
            "latte_almond_switched_off_at_the_branch",
            M_LATTE,
            s,
            &[(A_ALMOND, 1)],
            &[],
        ),
        case(
            "latte_mystery_milk_has_no_lines",
            M_LATTE,
            s,
            &[(A_MYSTERY_MILK, 1)],
            &[],
        ),
        case("latte_skim_no_group", M_LATTE, s, &[(A_SKIM, 1)], &[]),
        case(
            "latte_soy_over_its_own_groups_whole",
            M_LATTE,
            s,
            &[(A_SOY_ALT, 1)],
            &[],
        ),
        case(
            "latte_whole_alt_is_the_recipe",
            M_LATTE,
            s,
            &[(A_WHOLE_ALT, 1)],
            &[],
        ),
        case(
            "latte_old_milk_inactive",
            M_LATTE,
            s,
            &[(A_OLD_MILK, 1)],
            &[],
        ),
        case(
            "latte_oat_then_almond_keeps_almond",
            M_LATTE,
            s,
            &[(A_OAT, 1), (A_ALMOND, 1)],
            &[],
        ),
        case("latte_one_oat_twice", M_LATTE, s, &[(A_OAT, 2)], &[]),
        case(
            "latte_oat_twice_beside_a_shot",
            M_LATTE,
            s,
            &[(A_OAT, 2), (A_SHOT, 1)],
            &[],
        ),
        case(
            "latte_zero_quantity_counts_one",
            M_LATTE,
            s,
            &[(A_SHOT, 0)],
            &[],
        ),
        // Coffee.
        case(
            "latte_house_espresso_is_the_recipe",
            M_LATTE,
            s,
            &[(A_ESPRESSO, 1)],
            &[],
        ),
        case(
            "latte_colombian_shares_the_recipe_bean",
            M_LATTE,
            s,
            &[(A_COLOMBIAN, 1)],
            &[],
        ),
        case(
            "latte_decaf_over_its_own_groups_bean",
            M_LATTE,
            s,
            &[(A_DECAF, 1)],
            &[],
        ),
        case("latte_bulk_bean_in_kg", M_LATTE, s, &[(A_KG_BEAN, 1)], &[]),
        case(
            "americano_oat_no_milk_to_swap",
            M_AMERICANO,
            Some("Regular"),
            &[(A_OAT, 1)],
            &[],
        ),
        // Tea: an explicit swap group (M4a).
        case("tea_cup_green", M_TEA, Some("Cup"), &[(A_GREEN, 1)], &[]),
        case(
            "tea_cup_black_is_the_recipe",
            M_TEA,
            Some("Cup"),
            &[(A_BLACK, 1)],
            &[],
        ),
        case(
            "tea_no_size_green_reads_the_first_size",
            M_TEA,
            None,
            &[(A_GREEN, 1)],
            &[],
        ),
        case(
            "tea_pot_white_named_replacement",
            M_TEA,
            Some("Pot"),
            &[(A_WHITE, 1)],
            &[],
        ),
        case(
            "tea_green_then_black_keeps_black",
            M_TEA,
            Some("Cup"),
            &[(A_GREEN, 1), (A_BLACK, 1)],
            &[],
        ),
        case(
            "latte_green_tea_no_tea_line",
            M_LATTE,
            s,
            &[(A_GREEN, 1)],
            &[],
        ),
        // Syrup: a swap group picked twice (M4c).
        case(
            "vlatte_vanilla_is_the_recipe",
            M_VLATTE,
            Some("Regular"),
            &[(A_VANILLA, 1)],
            &[],
        ),
        case(
            "vlatte_caramel",
            M_VLATTE,
            Some("Regular"),
            &[(A_CARAMEL, 1)],
            &[],
        ),
        case(
            "vlatte_two_vanilla_and_a_caramel",
            M_VLATTE,
            Some("Regular"),
            &[(A_VANILLA, 2), (A_CARAMEL, 1)],
            &[],
        ),
        case(
            "latte_caramel_no_syrup_line",
            M_LATTE,
            s,
            &[(A_CARAMEL, 1)],
            &[],
        ),
        // Add-ons, optional fields, everything at once.
        case("latte_two_shots", M_LATTE, s, &[(A_SHOT, 2)], &[]),
        case(
            "latte_shot_follows_decaf",
            M_LATTE,
            s,
            &[(A_DECAF, 1), (A_SHOT, 1)],
            &[],
        ),
        case(
            "latte_sprinkles_no_lines",
            M_LATTE,
            s,
            &[(A_SPRINKLES, 1)],
            &[],
        ),
        case("latte_extra_hot", M_LATTE, s, &[], &[O_HOT]),
        case(
            "latte_small_cream_is_large_only",
            M_LATTE,
            s,
            &[],
            &[O_CREAM],
        ),
        case("latte_large_cream", M_LATTE, l, &[], &[O_CREAM]),
        case("latte_retired_topping", M_LATTE, s, &[], &[O_OFF]),
        case("latte_extra_hot_twice", M_LATTE, s, &[], &[O_HOT, O_HOT]),
        case("latte_unknown_option", M_LATTE, s, &[(id(0xdead), 1)], &[]),
        case(
            "latte_everything",
            M_LATTE,
            l,
            &[
                (A_WHOLE, 1),
                (A_OAT, 1),
                (A_DECAF, 1),
                (A_SHOT, 2),
                (A_SPRINKLES, 1),
                (A_CARAMEL, 1),
            ],
            &[O_HOT, O_CREAM],
        ),
        // Bundle components: the options only.
        component(
            "component_latte_small_oat_shot",
            M_LATTE,
            s,
            &[(A_OAT, 1), (A_SHOT, 1)],
            &[],
        ),
        component(
            "component_latte_no_size_barista",
            M_LATTE,
            None,
            &[(A_BARISTA_WHOLE, 1)],
            &[],
        ),
        component("component_croissant", M_CROISSANT, None, &[], &[]),
        component(
            "component_retired_is_still_resolved",
            M_RETIRED,
            Some("Regular"),
            &[(A_SHOT, 1)],
            &[],
        ),
    ]
}

/// The server's answer for one case, as the pin records it.
async fn capture(pool: &PgPool, c: &Case) -> Value {
    let addons: Vec<AddonInput> = c
        .options
        .iter()
        .map(|(id, q)| AddonInput {
            addon_item_id: *id,
            quantity: *q,
            unit_price: None,
        })
        .collect();
    let unit = if c.part == "line" {
        match catalog_unit_price(pool, c.item, c.size, BRANCH).await {
            Ok((_, _, p, _)) => Some(p),
            Err(e) => return json!({ "error": e.to_string() }),
        }
    } else {
        None
    };
    // A line of two, so every deduction shows its scaling.
    let res = match resolve_menu_item_configuration(
        pool,
        c.item,
        c.size.map(str::to_string),
        2,
        &addons,
        &c.optionals,
        BRANCH,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return json!({ "error": e.to_string() }),
    };
    json!({
        "unit_price": unit,
        "addons": res.addons.iter().map(|a| json!({
            "id": a.addon_item_id, "name": a.addon_name, "unit_price": a.unit_price,
            "quantity": a.quantity, "is_swap": a.is_swap, "has_ingredients": a.has_ingredients,
            "swap_over": a.swap_over,
        })).collect::<Vec<_>>(),
        "optionals": res.optionals.iter().map(|o| json!({
            "id": o.optional_field_id, "price": o.price,
        })).collect::<Vec<_>>(),
        "addon_line": res.addon_line,
        "optional_line": res.optional_line,
        "deductions": res.deductions.iter().map(|d| json!({
            "ingredient": d.org_ingredient_id, "name": d.ingredient_name, "unit": d.unit,
            "quantity": d.quantity, "source": d.source, "category": d.category,
            "addon": d.addon_item_id, "optional": d.optional_field_id, "note": d.note,
            "undeducted": d.undeducted,
        })).collect::<Vec<_>>(),
        "warnings": res.warnings.iter().map(|w| json!({ "rule": w.rule, "message": w.message })).collect::<Vec<_>>(),
    })
}

/// Orders rung through `POST /orders`: what the server books.
fn bills() -> Vec<(&'static str, Value)> {
    let line = |item: Uuid,
                size: Option<&str>,
                qty: i32,
                addons: &[(Uuid, i32)],
                optionals: &[Uuid]| {
        json!({
            "menu_item_id": item, "size_label": size, "quantity": qty,
            "addons": addons.iter().map(|(a, q)| json!({"addon_item_id": a, "quantity": q})).collect::<Vec<_>>(),
            "optional_field_ids": optionals,
        })
    };
    vec![
        (
            "swaps_and_sizes",
            json!([
                line(M_LATTE, Some("Small"), 2, &[(A_OAT, 1), (A_SHOT, 1)], &[]),
                line(M_TEA, Some("Cup"), 1, &[(A_GREEN, 1)], &[]),
                line(M_LATTE, None, 1, &[(A_DECAF, 1)], &[O_HOT]),
            ]),
        ),
        (
            "drift_lines",
            json!([
                line(M_LATTE, Some("Small"), 1, &[(A_BARISTA_WHOLE, 1)], &[]),
                line(
                    M_VLATTE,
                    Some("Regular"),
                    1,
                    &[(A_VANILLA, 2), (A_CARAMEL, 1)],
                    &[]
                ),
                line(M_LATTE, Some("Large"), 1, &[], &[O_CREAM]),
            ]),
        ),
        (
            "bundle_with_component_options",
            json!([{
                "bundle_id": B_BREAKFAST, "quantity": 2,
                "bundle_components": [
                    {"item_id": M_LATTE, "quantity": 2, "size_label": "Small",
                     "addons": [{"addon_item_id": A_OAT, "quantity": 1}, {"addon_item_id": A_SHOT, "quantity": 1}],
                     "optional_field_ids": []},
                    {"item_id": M_CROISSANT, "quantity": 1, "addons": [], "optional_field_ids": []}
                ]
            }]),
        ),
    ]
}

async fn ring(pool: &PgPool, items: &Value, extra: Value) -> (u16, Value) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .configure(madar_rust::orders::routes::configure),
    )
    .await;
    let token = madar_rust::auth::jwt::create_token(
        &secret(),
        USER,
        Some(ORG),
        UserRole::OrgAdmin,
        None,
        24,
    )
    .unwrap();
    let mut body = json!({
        "branch_id": BRANCH, "till_id": TILL, "payment_method": "cash", "items": items,
    });
    if let (Value::Object(b), Value::Object(e)) = (&mut body, extra) {
        b.extend(e);
    }
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/orders")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(&body)
            .to_request(),
    )
    .await;
    let status = resp.status().as_u16();
    let body: Value = test::read_body_json(resp).await;
    (status, body)
}

/// The booked order, reduced to its money.
fn booked(order: &Value) -> Value {
    let lines = order["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|i| {
                    json!({
                        "unit_price": i["unit_price"], "quantity": i["quantity"],
                        "line_total": i["line_total"], "price_flagged": i["price_flagged"],
                        "addons": i["addons"].as_array().map(|a| a.iter().map(|x| json!({
                            "id": x["addon_item_id"], "unit_price": x["unit_price"],
                            "quantity": x["quantity"], "line_total": x["line_total"],
                        })).collect::<Vec<_>>()),
                        "bundle_components": i["bundle_components"].as_array().map(|a| a.iter().map(|c| json!({
                            "item_id": c["item_id"], "quantity": c["quantity"],
                            "addons": c["addons"].as_array().map(|a| a.iter().map(|x| json!({
                                "id": x["addon_item_id"], "unit_price": x["unit_price"],
                                "quantity": x["quantity"], "line_total": x["line_total"],
                            })).collect::<Vec<_>>()),
                        })).collect::<Vec<_>>()),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({
        "subtotal": order["subtotal"], "tax_amount": order["tax_amount"],
        "total_amount": order["total_amount"], "price_flagged": order["price_flagged"],
        "items": lines,
    })
}

/// Every case and every bill, as the server answers them now.
pub async fn capture_all(pool: &PgPool) -> Value {
    let mut priced = serde_json::Map::new();
    for c in cases() {
        priced.insert(c.name.to_string(), capture(pool, &c).await);
    }
    let mut rung = serde_json::Map::new();
    for (name, items) in bills() {
        let (status, body) = ring(pool, &items, json!({})).await;
        assert_eq!(status, 201, "{name}: {body}");
        rung.insert(name.to_string(), booked(&body));
    }
    json!({ "cases": priced, "bills": rung })
}

fn pretty(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).unwrap();
    s.push('\n');
    s
}

#[sqlx::test]
async fn the_server_prices_the_fixture_as_pinned(pool: PgPool) {
    seed(&pool).await;
    let now = capture_all(&pool).await;
    if std::env::var("MADAR_WRITE_CATALOG_CAPTURE").is_ok() {
        std::fs::create_dir_all("tests/fixtures/catalog_pricing").unwrap();
        std::fs::write(CAPTURE, pretty(&now)).unwrap();
        return;
    }
    let pinned: Value = serde_json::from_str(&std::fs::read_to_string(CAPTURE).unwrap()).unwrap();
    for (name, want) in pinned["cases"].as_object().unwrap() {
        assert_eq!(&now["cases"][name], want, "case {name}");
    }
    for (name, want) in pinned["bills"].as_object().unwrap() {
        assert_eq!(&now["bills"][name], want, "bill {name}");
    }
    assert_eq!(now, pinned);
}
