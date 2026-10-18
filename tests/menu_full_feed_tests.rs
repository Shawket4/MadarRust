//! `GET /menu-items?full=true` loads its per-item parts in batches
//! (`menu::full_feed`), and old tills parse that body: every part must be
//! exactly what the per-item query it replaced returned, in the same order.
//!
//! The per-item queries are kept HERE, verbatim, as the reference. The fixture
//! has what makes their order fragile: several items, sizes, recipe lines on
//! two sizes, two add-on slots on one item (the slot view sorts on `now()`),
//! an allowlist spanning two groups (sort ties across groups), optional
//! fields, a branch size price and a milk add-on (`default_milk_addon_id`).

#[allow(dead_code)]
mod common;

use actix_web::{App, test as atest, web};
use common::legacy_catalog as lc;
use madar_rust::menu::full_feed::{FullParts, default_milk_ids};
use madar_rust::menu::handlers::{AddonSlot, ItemSize, MenuItemRecipe, OptionalField};
use sqlx::PgPool;
use uuid::Uuid;

const fn fid(n: u128) -> Uuid {
    Uuid::from_u128(0xfeed_0000_0000_4000_8000_0000_0000_0000 | n)
}
const ORG: Uuid = fid(1);
const BRANCH: Uuid = fid(2);
const ADMIN: Uuid = fid(3);
const CAT: Uuid = fid(4);
const LATTE: Uuid = fid(5);
const TEA: Uuid = fid(6);
const MOCHA: Uuid = fid(7);
const MILK: Uuid = fid(9);
const BEAN: Uuid = fid(10);
const SYRUP: Uuid = fid(11);
const OAT: Uuid = fid(12);
const SKIM: Uuid = fid(13);
const VANILLA: Uuid = fid(14);
const CARAMEL: Uuid = fid(15);
const CREAM: Uuid = fid(16);
const COCOA: Uuid = fid(17);

async fn seed(pool: &PgPool) {
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations (id, name, slug) VALUES ('{ORG}', 'Feed', 'full-feed');
         INSERT INTO branches (id, org_id, name, code) VALUES ('{BRANCH}', '{ORG}', 'Main', 'MAIN');
         INSERT INTO users (id, org_id, name, email, password_hash, role)
              VALUES ('{ADMIN}', '{ORG}', 'Admin', 'admin@feed.test', 'x', 'org_admin');
         INSERT INTO categories (id, org_id, name) VALUES ('{CAT}', '{ORG}', 'Coffee');
         INSERT INTO menu_items (id, org_id, category_id, name, base_price) VALUES
              ('{LATTE}', '{ORG}', '{CAT}', 'Latte', 4000), ('{TEA}', '{ORG}', '{CAT}', 'Tea', 2000),
              ('{MOCHA}', '{ORG}', '{CAT}', 'Mocha', 4500);
         INSERT INTO menu_item_sizes (menu_item_id, label, price, sort) VALUES
              ('{LATTE}', 'Small', 4000, 0), ('{LATTE}', 'Large', 5000, 1),
              ('{MOCHA}', 'Regular', 4500, 0), ('{MOCHA}', 'Big', 5500, 0);
         INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category_id) VALUES
              ('{MILK}', '{ORG}', 'Whole milk', 'ml', 0.05, ingredient_category_id('{ORG}', 'milk')),
              ('{BEAN}', '{ORG}', 'Espresso bean', 'g', 0.9, ingredient_category_id('{ORG}', 'coffee_bean')),
              ('{SYRUP}', '{ORG}', 'Vanilla syrup', 'ml', 0.2, ingredient_category_id('{ORG}', 'general')),
              ('{CREAM}', '{ORG}', 'Cream', 'ml', 0.3, ingredient_category_id('{ORG}', 'general')),
              ('{COCOA}', '{ORG}', 'Cocoa', 'g', 0.4, ingredient_category_id('{ORG}', 'general'));
         UPDATE org_ingredients SET yield_pct = 92.5 WHERE id = '{MILK}';"
    ))
    .execute(pool)
    .await
    .unwrap();
    for (item, size, ing, qty, unit) in [
        (LATTE, "Small", MILK, 200, "ml"),
        (LATTE, "Small", BEAN, 18, "g"),
        (LATTE, "Large", MILK, 300, "ml"),
        (LATTE, "Large", BEAN, 18, "g"),
        (LATTE, "Large", SYRUP, 10, "ml"),
        (MOCHA, "Regular", COCOA, 20, "g"),
        (MOCHA, "Regular", MILK, 250, "ml"),
        (MOCHA, "Big", MILK, 350, "ml"),
        (MOCHA, "Big", BEAN, 18, "g"),
    ] {
        lc::item_recipe(pool, item, size, ing, qty, unit).await;
    }
    lc::addon(pool, lc::Addon::new(ORG, "Oat", "milk_type", 800).id(OAT)).await;
    lc::addon(pool, lc::Addon::new(ORG, "Skim", "milk_type", 0).id(SKIM)).await;
    lc::addon(pool, lc::Addon::new(ORG, "Vanilla", "extra", 500).id(VANILLA)).await;
    lc::addon(pool, lc::Addon::new(ORG, "Caramel", "extra", 600).id(CARAMEL)).await;
    lc::addon_ingredient(pool, OAT, MILK, 200, "ml").await;
    lc::addon_ingredient(pool, SKIM, MILK, 200, "ml").await;
    lc::addon_ingredient(pool, VANILLA, SYRUP, 15, "ml").await;
    lc::optional_field(pool, lc::Optional::new(LATTE, "Whipped cream", 300).recipe(CREAM, 40, "ml")).await;
    lc::optional_field(pool, lc::Optional::new(LATTE, "Extra hot", 0)).await;
    lc::optional_field(pool, lc::Optional::new(MOCHA, "Marshmallow", 200)).await;
    lc::addon_slot(pool, ORG, LATTE, "milk_type", true, 1, Some(1)).await;
    lc::addon_slot(pool, ORG, LATTE, "extra", false, 0, Some(3)).await;
    lc::addon_slot(pool, ORG, MOCHA, "extra", false, 0, None).await;
    lc::allowed_addons(pool, LATTE, &[OAT, VANILLA, SKIM, CARAMEL]).await;
    lc::allowed_addons(pool, MOCHA, &[CARAMEL, VANILLA]).await;
    lc::branch_size_override(pool, BRANCH, LATTE, "Large", 5300).await;
    lc::branch_size_override(pool, BRANCH, MOCHA, "Big", 5900).await;
}

/// The per-item queries `list_menu_items` ran before `full_feed`, verbatim.
async fn per_item(pool: &PgPool, id: Uuid, branch: Option<Uuid>) -> serde_json::Value {
    let mut sizes = sqlx::query_as::<_, ItemSize>(
        "SELECT id, menu_item_id, label::text, price_override, is_active
         FROM compat.item_sizes
         WHERE menu_item_id = $1
         ORDER BY label ASC",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap();
    let mut all_sizes = sqlx::query_as::<_, ItemSize>(
        "SELECT id, menu_item_id, label::text, price AS price_override, is_active
         FROM menu_item_sizes
         WHERE menu_item_id = $1
         ORDER BY sort ASC, label ASC",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap();
    if let Some(b) = branch {
        for sizes in [&mut sizes, &mut all_sizes] {
            if sizes.is_empty() {
                continue;
            }
            let o: std::collections::HashMap<String, i32> = sqlx::query_as::<_, (String, i32)>(
                "SELECT size_label::text AS size_label, price_override
                 FROM compat.branch_menu_size_overrides
                 WHERE branch_id = $1 AND menu_item_id = $2
                 ORDER BY size_label",
            )
            .bind(b)
            .bind(id)
            .fetch_all(pool)
            .await
            .unwrap()
            .into_iter()
            .collect();
            for s in sizes.iter_mut() {
                if let Some(p) = o.get(&s.label) {
                    s.price_override = *p;
                }
            }
        }
    }
    let slots = sqlx::query_as::<_, AddonSlot>(
        "SELECT id, menu_item_id, addon_type, label, label_translations, is_required,
                min_selections, max_selections, created_at
         FROM compat.menu_item_addon_slots
         WHERE menu_item_id = $1
         ORDER BY created_at ASC",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap();
    let optionals = sqlx::query_as::<_, OptionalField>(
        "SELECT id, menu_item_id, name, name_translations, price,
                org_ingredient_id, ingredient_name, ingredient_unit,
                quantity_used, size_label::text,
                is_active, created_at, updated_at
         FROM compat.menu_item_optional_fields
         WHERE menu_item_id = $1 AND is_active = true
         ORDER BY name ASC",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap();
    let recipes: Vec<MenuItemRecipe> = sqlx::query_as::<_, MenuItemRecipe>(
        r#"SELECT r.org_ingredient_id, r.quantity_used,
                  r.ingredient_name, r.ingredient_unit,
                  (SELECT ic.slug FROM ingredient_categories ic WHERE ic.id = i.category_id) as category,
                  r.size_label::text,
                  i.yield_pct::float8 AS yield_pct
           FROM   compat.menu_item_recipes r
           LEFT JOIN org_ingredients i ON i.id = r.org_ingredient_id
           WHERE  r.menu_item_id = $1"#,
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|mut r| {
        use bigdecimal::ToPrimitive;
        r.usable_quantity =
            madar_units::usable_qty(r.quantity_used.to_f64().unwrap_or(0.0), r.yield_pct);
        r
    })
    .collect();
    let allowed: Vec<Uuid> = sqlx::query_scalar(
        "SELECT addon_item_id FROM compat.menu_item_allowed_addons
         WHERE menu_item_id = $1
         ORDER BY sort_order ASC, created_at ASC",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap();
    let milk: Option<String> = sqlx::query_scalar(
        "SELECT a.id::text
         FROM compat.menu_item_recipes r
         JOIN compat.addon_item_ingredients ai ON ai.org_ingredient_id = r.org_ingredient_id
         JOIN compat.addon_items a ON a.id = ai.addon_item_id
         WHERE r.menu_item_id = $1 AND a.type = 'milk_type'
         ORDER BY (SELECT mo.sort FROM modifier_options mo WHERE mo.id = a.id) NULLS LAST, a.name, a.id
         LIMIT 1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .unwrap();
    serde_json::json!({
        "sizes": sizes, "all_sizes": all_sizes, "addon_slots": slots,
        "optional_fields": optionals, "recipes": recipes, "allowed_addon_ids": allowed,
        "default_milk_addon_id": milk,
    })
}

/// The slot views' `created_at` is `now()`: request time, not data.
fn blank_slot_times(v: &mut serde_json::Value) {
    for s in v["addon_slots"].as_array_mut().unwrap() {
        s["created_at"] = serde_json::Value::Null;
    }
}

#[sqlx::test]
async fn the_batch_is_the_per_item_queries_row_for_row(pool: PgPool) {
    seed(&pool).await;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM menu_items WHERE org_id = $1 AND deleted_at IS NULL ORDER BY name",
    )
    .bind(ORG)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(ids.len(), 3);
    for branch in [None, Some(BRANCH)] {
        let mut conn = pool.acquire().await.unwrap();
        let mut parts = FullParts::load(&mut conn, &ids, branch).await.unwrap();
        let mut milk = default_milk_ids(&mut conn, &ids).await.unwrap();
        for &id in &ids {
            let mut want = per_item(&pool, id, branch).await;
            let mut got = serde_json::json!({
                "sizes": parts.sizes.remove(&id).unwrap_or_default(),
                "all_sizes": parts.all_sizes.remove(&id).unwrap_or_default(),
                "addon_slots": parts.addon_slots.remove(&id).unwrap_or_default(),
                "optional_fields": parts.optional_fields.remove(&id).unwrap_or_default(),
                "recipes": parts.recipes.remove(&id).unwrap_or_default(),
                "allowed_addon_ids": parts.allowed_addon_ids.remove(&id).unwrap_or_default(),
                "default_milk_addon_id": milk.remove(&id),
            });
            blank_slot_times(&mut want);
            blank_slot_times(&mut got);
            // Byte for byte, not just as JSON values: key order and number
            // spelling are part of what an old till parses.
            assert_eq!(
                serde_json::to_string(&got).unwrap(),
                serde_json::to_string(&want).unwrap(),
                "item {id}, branch {branch:?}"
            );
        }
    }
    // The fixture exercises what it claims to.
    let latte = per_item(&pool, LATTE, Some(BRANCH)).await;
    assert_eq!(latte["addon_slots"].as_array().unwrap().len(), 2);
    assert_eq!(latte["allowed_addon_ids"].as_array().unwrap().len(), 4);
    assert_eq!(latte["recipes"].as_array().unwrap().len(), 5);
    assert_eq!(latte["optional_fields"].as_array().unwrap().len(), 2);
    assert_eq!(latte["default_milk_addon_id"], serde_json::json!(OAT.to_string()));
    assert!(latte["sizes"].to_string().contains("5300"));
}

fn token() -> String {
    madar_rust::auth::jwt::create_token(
        &madar_rust::auth::jwt::JwtSecret("secret".to_string()),
        ADMIN,
        Some(ORG),
        madar_rust::models::UserRole::OrgAdmin,
        None,
        24,
    )
    .unwrap()
}

/// The opt-in compact shape carries the default rows' data: putting every
/// group's options back gives the default `pricing`, and only the fields no
/// client reads are gone. A client that does not ask gets the default array.
#[sqlx::test]
async fn the_compact_shape_is_opt_in_and_loses_nothing_read(pool: PgPool) {
    seed(&pool).await;
    let app = atest::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(madar_rust::auth::jwt::JwtSecret(
                "secret".to_string(),
            )))
            .configure(madar_rust::menu::routes::configure),
    )
    .await;
    let get = |q: &str| {
        atest::TestRequest::get()
            .uri(&format!("/menu-items?org_id={ORG}&full=true&branch_id={BRANCH}{q}"))
            .insert_header(("Authorization", format!("Bearer {}", token())))
            .insert_header(("X-Madar-Client", "pos/0.14.0 (android)"))
            .to_request()
    };
    // Slot `created_at` is `now()` (request time): blanked before comparing.
    let blank = |b: &[u8]| -> serde_json::Value {
        let mut v: serde_json::Value = serde_json::from_slice(b).unwrap();
        let items = if v.is_array() {
            v.as_array_mut().unwrap()
        } else {
            v["items"].as_array_mut().unwrap()
        };
        items.iter_mut().for_each(blank_slot_times);
        v
    };
    let default = atest::read_body(atest::call_service(&app, get("")).await).await;
    let unknown = atest::read_body(atest::call_service(&app, get("&shape=other")).await).await;
    assert_eq!(blank(&default), blank(&unknown), "an unknown shape is the default body");
    let default = blank(&default);
    assert!(default.is_array());
    let compact = blank(&atest::read_body(atest::call_service(&app, get("&shape=compact")).await).await);
    assert_eq!(compact["shape"], "compact-1");
    let items = compact["items"].as_array().unwrap();
    assert_eq!(items.len(), default.as_array().unwrap().len());
    for (d, c) in default.as_array().unwrap().iter().zip(items) {
        let mut d = d.clone();
        let mut c = c.clone();
        for k in ["org_id", "image", "created_at", "updated_at"] {
            d.as_object_mut().unwrap().remove(k);
        }
        if let Some(groups) = c.pointer_mut("/pricing/groups").and_then(|g| g.as_array_mut()) {
            for g in groups {
                let id = g["id"].as_str().unwrap().to_string();
                g["options"] = compact["group_options"][&id].clone();
            }
        }
        assert_eq!(c, d, "item {}", d["id"]);
    }
}
