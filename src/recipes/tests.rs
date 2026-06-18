use actix_web::{test, App, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::recipes::routes;
use crate::recipes::handlers::*;

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn generate_token(user_id: Uuid, org_id: Option<Uuid>, role: UserRole) -> String {
    crate::auth::jwt::create_token(&get_secret(), user_id, org_id, role, None, 24).unwrap()
}

fn generate_org_admin_token(user_id: Uuid, org_id: Uuid) -> String {
    generate_token(user_id, Some(org_id), UserRole::OrgAdmin)
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let org_id = Uuid::new_v4();
    let slug = format!("test-org-{}", org_id);
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Test Org', $2)")
        .bind(org_id)
        .bind(slug)
        .execute(pool)
        .await
        .unwrap();
    org_id
}

async fn seed_user(pool: &PgPool, org_id: Uuid, role: &str) -> Uuid {
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES ($1, $2, 'Test User', $3, 'hash', $4::user_role)"
    )
    .bind(user_id)
    .bind(org_id)
    .bind(format!("user-{}@test.com", user_id))
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    user_id
}

async fn grant_permission(pool: &PgPool, role: &str, resource: &str, action: &str) {
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) VALUES ($1::user_role, $2::permission_resource, $3::permission_action, true) ON CONFLICT DO NOTHING"
    )
    .bind(role)
    .bind(resource)
    .bind(action)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_category(pool: &PgPool, org_id: Uuid, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(org_id)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_menu_item(pool: &PgPool, org_id: Uuid, category_id: Uuid, name: &str, price: i32) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price) VALUES ($1, $2, $3, $4, $5)")
        .bind(id)
        .bind(org_id)
        .bind(category_id)
        .bind(name)
        .bind(price)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_addon_item(pool: &PgPool, org_id: Uuid, name: &str, addon_type: &str, price: i32) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO addon_items (id, org_id, name, type, default_price) VALUES ($1, $2, $3, $4, $5)")
        .bind(id)
        .bind(org_id)
        .bind(name)
        .bind(addon_type)
        .bind(price)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_ingredient(pool: &PgPool, org_id: Uuid, name: &str, unit: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO org_ingredients (id, org_id, name, unit, category, description, cost_per_unit) \
         VALUES ($1, $2, $3, $4::inventory_unit, 'veggies', 'Fresh ingredient', 2.50)"
    )
    .bind(id)
    .bind(org_id)
    .bind(name)
    .bind(unit)
    .execute(pool)
    .await
    .unwrap();
    id
}

// ──────────────────────────────────────────────────────────────
// ── Drink Recipes Tests
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_drink_recipes_crud(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "recipes", "create").await;
    grant_permission(&pool, "org_admin", "recipes", "read").await;
    grant_permission(&pool, "org_admin", "recipes", "update").await;
    grant_permission(&pool, "org_admin", "recipes", "delete").await;

    let cat_id = seed_category(&pool, org_id, "Drinks").await;
    let item_id = seed_menu_item(&pool, org_id, cat_id, "Latte", 500).await;
    let ingredient_id = seed_ingredient(&pool, org_id, "Milk", "ml").await;

    let token = generate_org_admin_token(user_id, org_id);

    // 1. Upsert Drink Recipe
    let req_body = UpsertDrinkRecipeRequest {
        size_label: "large".to_string(),
        org_ingredient_id: Some(ingredient_id),
        ingredient_name: "Milk".to_string(),
        ingredient_unit: "ml".to_string(),
        quantity_used: 250.0,
    };

    let req = test::TestRequest::post()
        .uri(&format!("/recipes/drinks/{}", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    assert!(status.is_success(), "Status: {}, Response: {:?}", status, body);

    // 2. List Drink Recipes
    let req_list = test::TestRequest::get()
        .uri(&format!("/recipes/drinks/{}", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp_list = test::call_service(&app, req_list).await;
    let list_status = resp_list.status();
    let list_body = test::read_body(resp_list).await;
    assert!(list_status.is_success());
    let recipes: Vec<DrinkRecipe> = serde_json::from_slice(&list_body).unwrap();
    assert_eq!(recipes.len(), 1);
    assert_eq!(recipes[0].ingredient_name, "Milk");

    // 3. Delete Drink Recipe
    let req_del = test::TestRequest::delete()
        .uri(&format!("/recipes/drinks/{}/large?ingredient_name=Milk", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp_del = test::call_service(&app, req_del).await;
    assert!(resp_del.status().is_success());
}

#[sqlx::test]
async fn test_drink_recipes_negative_quantity(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "recipes", "create").await;

    let cat_id = seed_category(&pool, org_id, "Drinks").await;
    let item_id = seed_menu_item(&pool, org_id, cat_id, "Latte", 500).await;

    let token = generate_org_admin_token(user_id, org_id);

    let req_body = UpsertDrinkRecipeRequest {
        size_label: "large".to_string(),
        org_ingredient_id: None,
        ingredient_name: "Water".to_string(),
        ingredient_unit: "ml".to_string(),
        quantity_used: -50.0,
    };

    let req = test::TestRequest::post()
        .uri(&format!("/recipes/drinks/{}", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 400);
}

#[sqlx::test]
async fn test_drink_recipes_wrong_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let org2_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "recipes", "read").await;

    let cat_id = seed_category(&pool, org2_id, "Drinks").await;
    let item_id = seed_menu_item(&pool, org2_id, cat_id, "Latte", 500).await;

    let token = generate_org_admin_token(user_id, org_id); // User is in org 1

    let req = test::TestRequest::get()
        .uri(&format!("/recipes/drinks/{}", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 403);
}

// ──────────────────────────────────────────────────────────────
// ── Addon Ingredients Tests
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_addon_ingredients_crud(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "recipes", "create").await;
    grant_permission(&pool, "org_admin", "recipes", "read").await;
    grant_permission(&pool, "org_admin", "recipes", "update").await;
    grant_permission(&pool, "org_admin", "recipes", "delete").await;

    let addon_id = seed_addon_item(&pool, org_id, "Vanilla Syrup", "syrup", 50).await;
    let ingredient_id = seed_ingredient(&pool, org_id, "Syrup", "ml").await;

    let token = generate_org_admin_token(user_id, org_id);

    // 1. Upsert Addon Ingredient
    let req_body = UpsertAddonIngredientRequest {
        org_ingredient_id: Some(ingredient_id),
        ingredient_name: "Syrup".to_string(),
        ingredient_unit: "ml".to_string(),
        quantity_used: 15.0,
    };

    let req = test::TestRequest::post()
        .uri(&format!("/recipes/addons/{}", addon_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    assert!(status.is_success(), "Status: {}, Response: {:?}", status, body);

    // 2. List Addon Ingredients
    let req_list = test::TestRequest::get()
        .uri(&format!("/recipes/addons/{}", addon_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp_list = test::call_service(&app, req_list).await;
    let list_status = resp_list.status();
    let list_body = test::read_body(resp_list).await;
    assert!(list_status.is_success());
    let recipes: Vec<AddonIngredient> = serde_json::from_slice(&list_body).unwrap();
    assert_eq!(recipes.len(), 1);
    assert_eq!(recipes[0].ingredient_name, "Syrup");

    // 3. Delete Addon Ingredient
    let req_del = test::TestRequest::delete()
        .uri(&format!("/recipes/addons/{}?ingredient_name=Syrup", addon_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp_del = test::call_service(&app, req_del).await;
    assert!(resp_del.status().is_success());
}

#[sqlx::test]
async fn test_addon_ingredients_wrong_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let org2_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "recipes", "read").await;

    let addon_id = seed_addon_item(&pool, org2_id, "Vanilla Syrup", "syrup", 50).await;

    let token = generate_org_admin_token(user_id, org_id); // User is in org 1

    let req = test::TestRequest::get()
        .uri(&format!("/recipes/addons/{}", addon_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 403);
}

/// V22: a positive recipe quantity that rounds to 0 in the ingredient's base
/// unit (0.4 g into a kg-base ingredient → 0.000 kg) must be rejected, not
/// silently stored as a no-op recipe line (no deduction, no COGS).
#[sqlx::test]
async fn test_drink_recipe_subunit_rounding_to_zero_rejected(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "recipes", "create").await;
    let cat_id = seed_category(&pool, org_id, "Drinks").await;
    let item_id = seed_menu_item(&pool, org_id, cat_id, "Latte", 500).await;
    let token = generate_org_admin_token(user_id, org_id);

    // Ingredient base unit is KILOGRAMS.
    let ing = seed_ingredient(&pool, org_id, "Almond Milk", "kg").await;

    // 0.4 g = 0.0004 kg → rounds to 0.000 kg in the numeric(12,3) column.
    let req_body = UpsertDrinkRecipeRequest {
        size_label: "large".to_string(),
        org_ingredient_id: Some(ing),
        ingredient_name: "Almond Milk".to_string(),
        ingredient_unit: "g".to_string(),
        quantity_used: 0.4,
    };
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/recipes/drinks/{}", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body).to_request()).await;
    assert_eq!(resp.status().as_u16(), 400, "sub-unit quantity rounding to 0 must be rejected");

    let stored: Option<sqlx::types::BigDecimal> = sqlx::query_scalar(
        "SELECT quantity_used FROM menu_item_recipes WHERE org_ingredient_id=$1"
    ).bind(ing).fetch_optional(&pool).await.unwrap();
    assert!(stored.is_none(), "no recipe row should be stored for a rounds-to-zero quantity");
}

/// Recipe depth: an ml recipe line against a gram-based ingredient converts via
/// density, and the per-ingredient yield grosses up the stored consumption.
#[sqlx::test]
async fn test_recipe_density_and_yield_applied_at_save(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "recipes", "create").await;
    let cat_id = seed_category(&pool, org_id, "Drinks").await;
    let item_id = seed_menu_item(&pool, org_id, cat_id, "Fried Dish", 500).await;

    // Ingredient bought by WEIGHT (g), density 0.92 g/ml, 50% usable yield.
    let ing = Uuid::new_v4();
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, category, cost_per_unit, density_g_per_ml, yield_pct) \
                 VALUES ($1,$2,'Olive Oil','g'::inventory_unit,'fats',3.0,0.92,50)")
        .bind(ing).bind(org_id).execute(&pool).await.unwrap();
    let token = generate_org_admin_token(user_id, org_id);

    // Recipe authored in millilitres.
    let req_body = UpsertDrinkRecipeRequest {
        size_label: "one_size".to_string(),
        org_ingredient_id: Some(ing),
        ingredient_name: "Olive Oil".to_string(),
        ingredient_unit: "ml".to_string(),
        quantity_used: 1000.0, // 1000 ml
    };
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/recipes/drinks/{}", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body).to_request()).await;
    assert!(resp.status().is_success());

    // 1000 ml × 0.92 = 920 g usable; grossed up by 50% yield → 1840 g stored.
    let (unit, qty): (String, f64) = sqlx::query_as(
        "SELECT ingredient_unit, quantity_used::float8 FROM menu_item_recipes WHERE org_ingredient_id=$1"
    ).bind(ing).fetch_one(&pool).await.unwrap();
    assert_eq!(unit, "g", "stored in the ingredient's base unit");
    assert_eq!(qty, 1840.0, "density bridge + yield gross-up applied at save");
}
