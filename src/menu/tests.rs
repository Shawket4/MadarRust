#![allow(unused_imports, unused_variables, dead_code)]
use actix_web::{test, App, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::menu::routes;
use crate::menu::handlers::*;

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn generate_token(user_id: Uuid, org_id: Option<Uuid>, role: UserRole) -> String {
    crate::auth::jwt::create_token(&get_secret(), user_id, org_id, role, None, 24).unwrap()
}

fn generate_org_admin_token(user_id: Uuid, org_id: Uuid) -> String {
    generate_token(user_id, Some(org_id), UserRole::OrgAdmin)
}

fn generate_teller_token(user_id: Uuid, org_id: Uuid) -> String {
    generate_token(user_id, Some(org_id), UserRole::Teller)
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

async fn seed_branch(pool: &PgPool, org_id: Uuid) -> Uuid {
    let branch_id = Uuid::new_v4();
    let name = format!("Test Branch {}", branch_id);
    sqlx::query(
        "INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)"
    )
    .bind(branch_id)
    .bind(org_id)
    .bind(name)
    .execute(pool)
    .await
    .unwrap();
    branch_id
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
// ── Categories Tests
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_list_categories_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "categories", "read").await;

    seed_category(&pool, org_id, "Beverages").await;
    seed_category(&pool, org_id, "Snacks").await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::get()
        .uri(&format!("/categories?org_id={}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let categories: Vec<Category> = test::read_body_json(resp).await;
    assert_eq!(categories.len(), 2);
}

#[sqlx::test]
async fn test_create_category_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "categories", "create").await;

    let req_body = CreateCategoryRequest {
        org_id,
        name_translations: None,
        name: "New Category".to_string(),
        image_url: None,
    };

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::post()
        .uri("/categories")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let category: Category = test::read_body_json(resp).await;
    assert_eq!(category.name, "New Category");
}

#[sqlx::test]
async fn test_update_category_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "categories", "update").await;

    let cat_id = seed_category(&pool, org_id, "Old Name").await;

    let req_body = UpdateCategoryRequest {
        name_translations: None,
        name: Some("Updated Name".to_string()),
        image_url: None,
        is_active: None,
    };

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::patch()
        .uri(&format!("/categories/{}", cat_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let category: Category = test::read_body_json(resp).await;
    assert_eq!(category.name, "Updated Name");
}

#[sqlx::test]
async fn test_delete_category_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "categories", "delete").await;

    let cat_id = seed_category(&pool, org_id, "To Delete").await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::delete()
        .uri(&format!("/categories/{}", cat_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
}

// ──────────────────────────────────────────────────────────────
// ── Menu Items Tests
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_list_menu_items_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;

    let cat_id = seed_category(&pool, org_id, "Mains").await;
    seed_menu_item(&pool, org_id, cat_id, "Burger", 1000).await;
    seed_menu_item(&pool, org_id, cat_id, "Pizza", 1500).await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::get()
        .uri(&format!("/menu-items?org_id={}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // /menu-items is a plain array (the POS contract) — not paginated.
    let items: Vec<MenuItem> = test::read_body_json(resp).await;
    assert_eq!(items.len(), 2);
}

#[sqlx::test]
async fn test_menu_catalog_paginated_with_costs(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
            .configure(crate::costing::routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;

    let cat_id = seed_category(&pool, org_id, "Mains").await;
    seed_menu_item(&pool, org_id, cat_id, "Burger", 1000).await;
    seed_menu_item(&pool, org_id, cat_id, "Pizza", 1500).await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::get()
        .uri(&format!("/costing/catalog?org_id={}&per_page=1&page=1", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // Deserializing into PaginatedMenuItems proves each row carries the
    // embedded `sku_costs` field (it has no serde default).
    let page: PaginatedMenuItems = test::read_body_json(resp).await;
    assert_eq!(page.total, 2);
    assert_eq!(page.total_pages, 2);
    assert_eq!(page.data.len(), 1); // per_page = 1
}

#[sqlx::test]
async fn test_create_menu_item_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "create").await;

    let cat_id = seed_category(&pool, org_id, "Mains").await;

    let req_body = CreateMenuItemRequest {
        org_id,
        name_translations: None,
        description_translations: None,
        category_id: cat_id,
        name: "New Item".to_string(),
        description: Some("Tasty".to_string()),
        base_price: 1200,
        image_url: None,
    };

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::post()
        .uri("/menu-items")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    assert!(status.is_success(), "Status: {}, Response: {:?}", status, body);
    
    // We can't read json from resp again, so we just parse the body bytes we read
    let item: MenuItem = serde_json::from_slice(&body).unwrap();
    assert_eq!(item.name, "New Item");
    assert_eq!(item.base_price, 1200);

    // Verify price epoch was created
    let epoch_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM menu_item_price_epochs WHERE menu_item_id = $1")
        .bind(item.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(epoch_count.0, 1);
}

#[sqlx::test]
async fn test_update_menu_item_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;

    let cat_id = seed_category(&pool, org_id, "Mains").await;
    let item_id = seed_menu_item(&pool, org_id, cat_id, "Old Item", 1000).await;

    let req_body = UpdateMenuItemRequest {
        name_translations: None,
        description_translations: None,
        category_id: None,
        name: Some("Updated Item".to_string()),
        description: None,
        base_price: Some(1500),
        image_url: None,
        is_active: None,
    };

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::patch()
        .uri(&format!("/menu-items/{}", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let item: MenuItem = test::read_body_json(resp).await;
    assert_eq!(item.name, "Updated Item");
    assert_eq!(item.base_price, 1500);

    // Verify a new price epoch was created for the update
    let epoch_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM menu_item_price_epochs WHERE menu_item_id = $1")
        .bind(item_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(epoch_count.0, 1);
}

#[sqlx::test]
async fn test_delete_menu_item_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "delete").await;

    let cat_id = seed_category(&pool, org_id, "Mains").await;
    let item_id = seed_menu_item(&pool, org_id, cat_id, "To Delete", 1000).await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::delete()
        .uri(&format!("/menu-items/{}", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
}

// ──────────────────────────────────────────────────────────────
// ── Sizes Tests
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_upsert_size_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;

    let cat_id = seed_category(&pool, org_id, "Mains").await;
    let item_id = seed_menu_item(&pool, org_id, cat_id, "Coffee", 500).await;

    let req_body = UpsertSizeRequest {
        label: "large".to_string(),
        price_override: 700,
    };

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::post()
        .uri(&format!("/menu-items/{}/sizes", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    assert!(status.is_success(), "Status: {}, Response: {:?}", status, body);
    let size: ItemSize = serde_json::from_slice(&body).unwrap();

    // Verify it was added
    assert_eq!(size.label, "large");
    assert_eq!(size.price_override, 700);

    // Verify a price epoch was created specifically for the size
    let epoch_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM menu_item_price_epochs WHERE menu_item_id = $1 AND size_label = $2")
        .bind(item_id)
        .bind("large")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(epoch_count.0, 1);
}

#[sqlx::test]
async fn test_delete_size_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;

    let cat_id = seed_category(&pool, org_id, "Mains").await;
    let item_id = seed_menu_item(&pool, org_id, cat_id, "Coffee", 500).await;

    let req_body = UpsertSizeRequest {
        label: "large".to_string(),
        price_override: 700,
    };

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::post()
        .uri(&format!("/menu-items/{}/sizes", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    let size: ItemSize = test::read_body_json(resp).await;

    let req_del = test::TestRequest::delete()
        .uri(&format!("/menu-items/{}/sizes/{}", item_id, size.id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp_del = test::call_service(&app, req_del).await;
    assert!(resp_del.status().is_success());
}

// ──────────────────────────────────────────────────────────────
// ── Addon Slots Tests
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_addon_slots_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;

    let cat_id = seed_category(&pool, org_id, "Mains").await;
    let item_id = seed_menu_item(&pool, org_id, cat_id, "Coffee", 500).await;

    // Create a slot
    let req_body = CreateAddonSlotRequest {
        addon_type: Some("Milk".to_string()),
        label_translations: None,
        max_selections: Some(1),
        min_selections: Some(0),
        label: None,
        is_required: None,
    };

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::post()
        .uri(&format!("/menu-items/{}/addon-slots", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let slot: AddonSlot = test::read_body_json(resp).await;
    assert_eq!(slot.addon_type, "Milk");

    // List slots
    let req_list = test::TestRequest::get()
        .uri(&format!("/menu-items/{}/addon-slots", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp_list = test::call_service(&app, req_list).await;
    let status = resp_list.status();
    let body = test::read_body(resp_list).await;
    assert!(status.is_success(), "Status: {}, Response: {:?}", status, body);
    let slots: Vec<AddonSlot> = serde_json::from_slice(&body).unwrap();
    assert_eq!(slots.len(), 1);
}

// ──────────────────────────────────────────────────────────────
// ── Addon Items Tests
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_addon_items_crud(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "create").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    grant_permission(&pool, "org_admin", "menu_items", "delete").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;

    let req_body = CreateAddonItemRequest {
        org_id,
        name_translations: None,
        name: "Extra Cheese".to_string(),
        addon_type: "Topping".to_string(),
        default_price: 200,
    };

    let token = generate_org_admin_token(user_id, org_id);
    
    // Create
    let req = test::TestRequest::post()
        .uri("/addon-items")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    assert!(status.is_success(), "Status: {}, Response: {:?}", status, body);
    let item: AddonItem = serde_json::from_slice(&body).unwrap();
    assert_eq!(item.name, "Extra Cheese");

    // List
    let req_list = test::TestRequest::get()
        .uri(&format!("/addon-items?org_id={}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp_list = test::call_service(&app, req_list).await;
    let items: Vec<AddonItem> = test::read_body_json(resp_list).await;
    assert_eq!(items.len(), 1);

    // Update
    let req_update = UpdateAddonItemRequest {
        name_translations: None,
        name: Some("Super Cheese".to_string()),
        addon_type: None,
        default_price: None,
        is_active: None,
    };
    let req_u = test::TestRequest::patch()
        .uri(&format!("/addon-items/{}", item.id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_update)
        .to_request();

    let resp_u = test::call_service(&app, req_u).await;
    assert!(resp_u.status().is_success());
    let u_item: AddonItem = test::read_body_json(resp_u).await;
    assert_eq!(u_item.name, "Super Cheese");

    // Delete
    let req_del = test::TestRequest::delete()
        .uri(&format!("/addon-items/{}", item.id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp_del = test::call_service(&app, req_del).await;
    assert!(resp_del.status().is_success());
}

// ──────────────────────────────────────────────────────────────
// ── Optional Fields Tests
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_optional_fields_crud(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    grant_permission(&pool, "org_admin", "menu_items", "delete").await;

    let cat_id = seed_category(&pool, org_id, "Mains").await;
    let item_id = seed_menu_item(&pool, org_id, cat_id, "Coffee", 500).await;

    let req_body = CreateOptionalFieldRequest {
        name_translations: None,
        name: "No Sugar".to_string(),
        price: Some(0),
        org_ingredient_id: None,
        ingredient_name: None,
        ingredient_unit: None,
        quantity_used: None,
        size_label: None,
    };

    let token = generate_org_admin_token(user_id, org_id);
    
    // Create
    let req = test::TestRequest::post()
        .uri(&format!("/menu-items/{}/optionals", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let field: OptionalField = test::read_body_json(resp).await;
    assert_eq!(field.name, "No Sugar");

    // Delete
    let req_del = test::TestRequest::delete()
        .uri(&format!("/menu-items/{}/optionals/{}", item_id, field.id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp_del = test::call_service(&app, req_del).await;
    let status = resp_del.status();
    let body = test::read_body(resp_del).await;
    assert!(status.is_success(), "Status: {}, Response: {:?}", status, body);
}

/// V21: changing an optional field's linked ingredient without resupplying
/// quantity_used must be rejected — the stored base-unit quantity would
/// otherwise be silently reinterpreted in the new ingredient's base unit.
#[sqlx::test]
async fn test_update_optional_field_swap_ingredient_requires_quantity(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let token = generate_org_admin_token(user_id, org_id);
    let cat_id = seed_category(&pool, org_id, "Mains").await;
    let item_id = seed_menu_item(&pool, org_id, cat_id, "Coffee", 500).await;
    let flour = seed_ingredient(&pool, org_id, "Flour", "g").await;
    let sugar = seed_ingredient(&pool, org_id, "Sugar", "kg").await;

    // Optional field linked to flour, 100 g.
    let create = CreateOptionalFieldRequest {
        name: "Extra".into(), name_translations: None, price: Some(0),
        org_ingredient_id: Some(flour), ingredient_name: Some("Flour".into()),
        ingredient_unit: Some("g".into()), quantity_used: Some(100.0), size_label: None,
    };
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/menu-items/{}/optionals", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&create).to_request()).await;
    assert!(resp.status().is_success());
    let field: OptionalField = test::read_body_json(resp).await;

    // Swap the linked ingredient to sugar (kg) WITHOUT a fresh quantity → 400.
    let resp = test::call_service(&app, test::TestRequest::patch()
        .uri(&format!("/menu-items/{}/optionals/{}", item_id, field.id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({"org_ingredient_id": sugar})).to_request()).await;
    assert_eq!(resp.status(), 400, "swapping the linked ingredient requires a fresh quantity_used");

    // Supplying a quantity makes the swap succeed.
    let resp = test::call_service(&app, test::TestRequest::patch()
        .uri(&format!("/menu-items/{}/optionals/{}", item_id, field.id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({"org_ingredient_id": sugar, "ingredient_unit": "g", "quantity_used": 50.0})).to_request()).await;
    assert!(resp.status().is_success(), "swap with a fresh quantity must succeed");
}

// ── Branch menu overrides ─────────────────────────────────────

#[sqlx::test]
async fn test_branch_menu_overrides_crud_and_injection(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let cat = seed_category(&pool, org_id, "Coffee").await;
    let item = seed_menu_item(&pool, org_id, cat, "Latte", 5000).await;
    let token = generate_org_admin_token(user_id, org_id);

    let branch_menu = |b: Uuid, t: &str| {
        test::TestRequest::get()
            .uri(&format!("/menu-items?org_id={}&branch_id={}", org_id, b))
            .insert_header(("Authorization", format!("Bearer {}", t)))
            .to_request()
    };

    // No override → branch menu shows the org base price.
    let resp = test::call_service(&app, branch_menu(branch_id, &token)).await;
    assert!(resp.status().is_success());
    let items: Vec<MenuItem> = test::read_body_json(resp).await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].base_price, 5000);

    // Upsert a branch price override.
    let body = BranchMenuOverrideInput { branch_id, menu_item_id: item, price_override: Some(6000), is_available: true, sizes: None };
    let resp = test::call_service(&app, test::TestRequest::put()
        .uri("/branch-menu-overrides")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&body).to_request()).await;
    assert!(resp.status().is_success());
    let ov: BranchMenuOverride = test::read_body_json(resp).await;
    assert_eq!(ov.price_override, Some(6000));

    // Branch menu reflects the override; the org catalog (no branch_id) does not.
    let resp = test::call_service(&app, branch_menu(branch_id, &token)).await;
    let items: Vec<MenuItem> = test::read_body_json(resp).await;
    assert_eq!(items[0].base_price, 6000);

    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/menu-items?org_id={}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token))).to_request()).await;
    let items: Vec<MenuItem> = test::read_body_json(resp).await;
    assert_eq!(items[0].base_price, 5000, "org catalog price unchanged");

    // Disable at this branch → excluded from the branch menu.
    let body = BranchMenuOverrideInput { branch_id, menu_item_id: item, price_override: Some(6000), is_available: false, sizes: None };
    let resp = test::call_service(&app, test::TestRequest::put()
        .uri("/branch-menu-overrides")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&body).to_request()).await;
    assert!(resp.status().is_success());

    let resp = test::call_service(&app, branch_menu(branch_id, &token)).await;
    let items: Vec<MenuItem> = test::read_body_json(resp).await;
    assert_eq!(items.len(), 0, "branch-disabled item must be excluded from the branch menu");

    // List the branch's override rows.
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/branch-menu-overrides?branch_id={}", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token))).to_request()).await;
    let rows: Vec<BranchMenuOverride> = test::read_body_json(resp).await;
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].is_available);

    // Delete → reverts to the org catalog.
    let resp = test::call_service(&app, test::TestRequest::delete()
        .uri(&format!("/branch-menu-overrides?branch_id={}&menu_item_id={}", branch_id, item))
        .insert_header(("Authorization", format!("Bearer {}", token))).to_request()).await;
    assert_eq!(resp.status(), 204);

    let resp = test::call_service(&app, branch_menu(branch_id, &token)).await;
    let items: Vec<MenuItem> = test::read_body_json(resp).await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].base_price, 5000);
}

#[sqlx::test]
async fn test_branch_menu_override_rejects_cross_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    // Branch belongs to org A; an org-B admin must not be able to override it.
    let org_a = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_a).await;
    let cat = seed_category(&pool, org_a, "Coffee").await;
    let item = seed_menu_item(&pool, org_a, cat, "Latte", 5000).await;

    let org_b = seed_org(&pool).await;
    let admin_b = seed_user(&pool, org_b, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let token_b = generate_org_admin_token(admin_b, org_b);

    let body = BranchMenuOverrideInput { branch_id: branch_a, menu_item_id: item, price_override: Some(1), is_available: true, sizes: None };
    let resp = test::call_service(&app, test::TestRequest::put()
        .uri("/branch-menu-overrides")
        .insert_header(("Authorization", format!("Bearer {}", token_b)))
        .set_json(&body).to_request()).await;
    assert_eq!(resp.status(), 403, "cross-org override must be forbidden");
}

// ── Branch menu overrides — intensive coverage ────────────────

macro_rules! auth_req {
    ($m:ident, $uri:expr, $tok:expr) => {
        test::TestRequest::$m().uri(&$uri)
            .insert_header(("Authorization", format!("Bearer {}", $tok))).to_request()
    };
    ($m:ident, $uri:expr, $tok:expr, $body:expr) => {
        test::TestRequest::$m().uri(&$uri)
            .insert_header(("Authorization", format!("Bearer {}", $tok)))
            .set_json(&$body).to_request()
    };
}

async fn override_app(pool: PgPool) -> impl actix_web::dev::Service<actix_http::Request, Response = actix_web::dev::ServiceResponse, Error = actix_web::Error> {
    test::init_service(
        App::new()
            .app_data(web::Data::new(pool))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await
}

async fn add_item_size(pool: &PgPool, item: Uuid, label: &str, price: i32) {
    sqlx::query(
        "INSERT INTO item_sizes (id, menu_item_id, label, price_override, is_active)
         VALUES (gen_random_uuid(), $1, $2::item_size, $3, true)",
    )
    .bind(item).bind(label).bind(price).execute(pool).await.unwrap();
}

/// A null price_override inherits the catalog base, but is_available=false still hides it.
#[sqlx::test]
async fn test_branch_override_null_price_inherits_but_can_disable(pool: PgPool) {
    let app = override_app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let cat = seed_category(&pool, org, "Coffee").await;
    let item = seed_menu_item(&pool, org, cat, "Latte", 5000).await;
    let token = generate_org_admin_token(user, org);

    // price_override = null, available → branch menu inherits the base price.
    let body = BranchMenuOverrideInput { branch_id: branch, menu_item_id: item, price_override: None, is_available: true, sizes: None };
    let resp = test::call_service(&app, auth_req!(put, "/branch-menu-overrides".to_string(), token, body)).await;
    assert!(resp.status().is_success());
    let ov: BranchMenuOverride = test::read_body_json(resp).await;
    assert_eq!(ov.price_override, None);

    let resp = test::call_service(&app, auth_req!(get, format!("/menu-items?org_id={}&branch_id={}", org, branch), token)).await;
    let items: Vec<MenuItem> = test::read_body_json(resp).await;
    assert_eq!(items[0].base_price, 5000, "null override inherits the base price");

    // Disable with a null price → still excluded.
    let body = BranchMenuOverrideInput { branch_id: branch, menu_item_id: item, price_override: None, is_available: false, sizes: None };
    test::call_service(&app, auth_req!(put, "/branch-menu-overrides".to_string(), token, body)).await;
    let resp = test::call_service(&app, auth_req!(get, format!("/menu-items?org_id={}&branch_id={}", org, branch), token)).await;
    let items: Vec<MenuItem> = test::read_body_json(resp).await;
    assert_eq!(items.len(), 0, "disabled-with-null-price item is excluded");
}

/// Upserting the same (branch,item) twice updates in place (no duplicate rows).
#[sqlx::test]
async fn test_branch_override_upsert_update_path(pool: PgPool) {
    let app = override_app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let cat = seed_category(&pool, org, "Coffee").await;
    let item = seed_menu_item(&pool, org, cat, "Latte", 5000).await;
    let token = generate_org_admin_token(user, org);

    for price in [6000, 7000] {
        let body = BranchMenuOverrideInput { branch_id: branch, menu_item_id: item, price_override: Some(price), is_available: true, sizes: None };
        let resp = test::call_service(&app, auth_req!(put, "/branch-menu-overrides".to_string(), token, body)).await;
        assert!(resp.status().is_success());
    }
    let resp = test::call_service(&app, auth_req!(get, format!("/branch-menu-overrides?branch_id={}", branch), token)).await;
    let rows: Vec<BranchMenuOverride> = test::read_body_json(resp).await;
    assert_eq!(rows.len(), 1, "upsert must update, not duplicate");
    assert_eq!(rows[0].price_override, Some(7000));
}

/// An override on one branch must not affect another branch's menu.
#[sqlx::test]
async fn test_branch_override_isolated_per_branch(pool: PgPool) {
    let app = override_app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org).await;
    let branch_b = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let cat = seed_category(&pool, org, "Coffee").await;
    let item = seed_menu_item(&pool, org, cat, "Latte", 5000).await;
    let token = generate_org_admin_token(user, org);

    let body = BranchMenuOverrideInput { branch_id: branch_a, menu_item_id: item, price_override: Some(9000), is_available: true, sizes: None };
    test::call_service(&app, auth_req!(put, "/branch-menu-overrides".to_string(), token, body)).await;

    let resp = test::call_service(&app, auth_req!(get, format!("/menu-items?org_id={}&branch_id={}", org, branch_a), token)).await;
    let a: Vec<MenuItem> = test::read_body_json(resp).await;
    assert_eq!(a[0].base_price, 9000);

    let resp = test::call_service(&app, auth_req!(get, format!("/menu-items?org_id={}&branch_id={}", org, branch_b), token)).await;
    let b: Vec<MenuItem> = test::read_body_json(resp).await;
    assert_eq!(b[0].base_price, 5000, "branch B is unaffected by branch A's override");
}

/// price_override = 0 is allowed (free item); a negative price is rejected.
#[sqlx::test]
async fn test_branch_override_zero_allowed_negative_rejected(pool: PgPool) {
    let app = override_app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let cat = seed_category(&pool, org, "Coffee").await;
    let item = seed_menu_item(&pool, org, cat, "Latte", 5000).await;
    let token = generate_org_admin_token(user, org);

    let body = BranchMenuOverrideInput { branch_id: branch, menu_item_id: item, price_override: Some(0), is_available: true, sizes: None };
    let resp = test::call_service(&app, auth_req!(put, "/branch-menu-overrides".to_string(), token, body)).await;
    assert!(resp.status().is_success(), "zero price (free) is allowed");

    let body = BranchMenuOverrideInput { branch_id: branch, menu_item_id: item, price_override: Some(-1), is_available: true, sizes: None };
    let resp = test::call_service(&app, auth_req!(put, "/branch-menu-overrides".to_string(), token, body)).await;
    assert_eq!(resp.status(), 400, "negative price is rejected");
}

/// Overriding with an item from a different org than the branch is a 404.
#[sqlx::test]
async fn test_branch_override_item_from_other_org_rejected(pool: PgPool) {
    let app = override_app(pool.clone()).await;
    let org_a = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_a).await;
    let admin_a = seed_user(&pool, org_a, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let token_a = generate_org_admin_token(admin_a, org_a);

    let org_b = seed_org(&pool).await;
    let cat_b = seed_category(&pool, org_b, "Coffee").await;
    let item_b = seed_menu_item(&pool, org_b, cat_b, "Latte", 5000).await;

    let body = BranchMenuOverrideInput { branch_id: branch_a, menu_item_id: item_b, price_override: Some(1), is_available: true, sizes: None };
    let resp = test::call_service(&app, auth_req!(put, "/branch-menu-overrides".to_string(), token_a, body)).await;
    assert_eq!(resp.status(), 404, "item from another org is not found for this branch");
}

/// Read needs menu_items/read; write needs menu_items/update.
#[sqlx::test]
async fn test_branch_override_permissions_enforced(pool: PgPool) {
    let app = override_app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let admin = seed_user(&pool, org, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await; // read only, no update
    let cat = seed_category(&pool, org, "Coffee").await;
    let item = seed_menu_item(&pool, org, cat, "Latte", 5000).await;
    let token = generate_org_admin_token(admin, org);

    // Has read → list works.
    let resp = test::call_service(&app, auth_req!(get, format!("/branch-menu-overrides?branch_id={}", branch), token)).await;
    assert!(resp.status().is_success());

    // Lacks update → upsert/delete forbidden.
    let body = BranchMenuOverrideInput { branch_id: branch, menu_item_id: item, price_override: Some(1), is_available: true, sizes: None };
    let resp = test::call_service(&app, auth_req!(put, "/branch-menu-overrides".to_string(), token, body)).await;
    assert_eq!(resp.status(), 403, "upsert needs menu_items/update");
    let resp = test::call_service(&app, auth_req!(delete, format!("/branch-menu-overrides?branch_id={}&menu_item_id={}", branch, item), token)).await;
    assert_eq!(resp.status(), 403, "delete needs menu_items/update");

    // A teller with no grants cannot even read.
    let teller = seed_user(&pool, org, "teller").await;
    let ttok = generate_teller_token(teller, org);
    let resp = test::call_service(&app, auth_req!(get, format!("/branch-menu-overrides?branch_id={}", branch), ttok)).await;
    assert_eq!(resp.status(), 403, "list needs menu_items/read");
}

/// Listing or deleting overrides for a branch in another org is forbidden.
#[sqlx::test]
async fn test_branch_override_list_delete_cross_org_rejected(pool: PgPool) {
    let app = override_app(pool.clone()).await;
    let org_a = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_a).await;
    let cat = seed_category(&pool, org_a, "Coffee").await;
    let item = seed_menu_item(&pool, org_a, cat, "Latte", 5000).await;

    let org_b = seed_org(&pool).await;
    let admin_b = seed_user(&pool, org_b, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let token_b = generate_org_admin_token(admin_b, org_b);

    let resp = test::call_service(&app, auth_req!(get, format!("/branch-menu-overrides?branch_id={}", branch_a), token_b)).await;
    assert_eq!(resp.status(), 403, "cross-org list forbidden");
    let resp = test::call_service(&app, auth_req!(delete, format!("/branch-menu-overrides?branch_id={}&menu_item_id={}", branch_a, item), token_b)).await;
    assert_eq!(resp.status(), 403, "cross-org delete forbidden");
}

/// An unknown branch is a 404.
#[sqlx::test]
async fn test_branch_override_unknown_branch_404(pool: PgPool) {
    let app = override_app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let admin = seed_user(&pool, org, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let token = generate_org_admin_token(admin, org);

    let body = BranchMenuOverrideInput { branch_id: Uuid::new_v4(), menu_item_id: Uuid::new_v4(), price_override: Some(1), is_available: true, sizes: None };
    let resp = test::call_service(&app, auth_req!(put, "/branch-menu-overrides".to_string(), token, body)).await;
    assert_eq!(resp.status(), 404, "unknown branch is not found");
}

/// ?full=true (the POS contract) honours branch overrides and still embeds sizes.
#[sqlx::test]
async fn test_list_menu_items_full_with_branch_override(pool: PgPool) {
    let app = override_app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let cat = seed_category(&pool, org, "Coffee").await;
    let item = seed_menu_item(&pool, org, cat, "Latte", 5000).await;
    add_item_size(&pool, item, "large", 8000).await;
    let token = generate_org_admin_token(user, org);

    let body = BranchMenuOverrideInput { branch_id: branch, menu_item_id: item, price_override: Some(6000), is_available: true, sizes: None };
    test::call_service(&app, auth_req!(put, "/branch-menu-overrides".to_string(), token, body)).await;

    let resp = test::call_service(&app, auth_req!(get, format!("/menu-items?org_id={}&branch_id={}&full=true", org, branch), token)).await;
    let full: Vec<MenuItemFull> = test::read_body_json(resp).await;
    assert_eq!(full.len(), 1);
    assert_eq!(full[0].item.base_price, 6000, "branch-effective base in the POS contract");
    assert_eq!(full[0].sizes.len(), 1, "sizes still embedded");
    assert_eq!(full[0].sizes[0].price_override, 8000, "size price is its own absolute value");
}

/// Category filter composes with branch overrides.
#[sqlx::test]
async fn test_branch_override_with_category_filter(pool: PgPool) {
    let app = override_app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let c1 = seed_category(&pool, org, "Coffee").await;
    let c2 = seed_category(&pool, org, "Bakery").await;
    let item = seed_menu_item(&pool, org, c1, "Latte", 5000).await;
    let _other = seed_menu_item(&pool, org, c2, "Croissant", 4000).await;
    let token = generate_org_admin_token(user, org);

    let body = BranchMenuOverrideInput { branch_id: branch, menu_item_id: item, price_override: Some(6000), is_available: true, sizes: None };
    test::call_service(&app, auth_req!(put, "/branch-menu-overrides".to_string(), token, body)).await;

    let resp = test::call_service(&app, auth_req!(get, format!("/menu-items?org_id={}&branch_id={}&category_id={}", org, branch, c1), token)).await;
    let items: Vec<MenuItem> = test::read_body_json(resp).await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].base_price, 6000);

    let resp = test::call_service(&app, auth_req!(get, format!("/menu-items?org_id={}&branch_id={}&category_id={}", org, branch, c2), token)).await;
    let items: Vec<MenuItem> = test::read_body_json(resp).await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].base_price, 4000, "other category unaffected");
}

// ── Per-size branch overrides ─────────────────────────────────

fn size_price(full: &MenuItemFull, label: &str) -> i32 {
    full.sizes.iter().find(|s| s.label == label).unwrap_or_else(|| panic!("size {label} missing")).price_override
}

/// Size overrides round-trip through CRUD and the POS branch menu, with replace/none semantics.
#[sqlx::test]
async fn test_branch_size_override_crud_and_menu_injection(pool: PgPool) {
    let app = override_app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let cat = seed_category(&pool, org, "Coffee").await;
    let item = seed_menu_item(&pool, org, cat, "Latte", 5000).await;
    add_item_size(&pool, item, "small", 4000).await;
    add_item_size(&pool, item, "large", 8000).await;
    let token = generate_org_admin_token(user, org);

    let menu_full = |t: &str| test::TestRequest::get()
        .uri(&format!("/menu-items?org_id={}&branch_id={}&full=true", org, branch))
        .insert_header(("Authorization", format!("Bearer {}", t))).to_request();

    // (a) base override 6000 + a 'large' size override 9000.
    let body = BranchMenuOverrideInput {
        branch_id: branch, menu_item_id: item, price_override: Some(6000), is_available: true,
        sizes: Some(vec![BranchSizeOverrideInput { size_label: "large".into(), price_override: 9000 }]),
    };
    let resp = test::call_service(&app, auth_req!(put, "/branch-menu-overrides".to_string(), token, body)).await;
    assert!(resp.status().is_success());
    let ov: BranchMenuOverride = test::read_body_json(resp).await;
    assert_eq!(ov.sizes, vec![BranchSizeOverride { size_label: "large".into(), price_override: 9000 }]);

    let resp = test::call_service(&app, menu_full(&token)).await;
    let full: Vec<MenuItemFull> = test::read_body_json(resp).await;
    assert_eq!(full[0].item.base_price, 6000, "branch base override");
    assert_eq!(size_price(&full[0], "large"), 9000, "branch size override applied");
    assert_eq!(size_price(&full[0], "small"), 4000, "un-overridden size keeps its catalog price");

    // (b) sizes = None leaves size overrides untouched while updating the base.
    let body = BranchMenuOverrideInput {
        branch_id: branch, menu_item_id: item, price_override: Some(6500), is_available: true, sizes: None,
    };
    test::call_service(&app, auth_req!(put, "/branch-menu-overrides".to_string(), token, body)).await;
    let resp = test::call_service(&app, auth_req!(get, format!("/branch-menu-overrides?branch_id={}", branch), token)).await;
    let rows: Vec<BranchMenuOverride> = test::read_body_json(resp).await;
    assert_eq!(rows[0].price_override, Some(6500));
    assert_eq!(rows[0].sizes.len(), 1, "sizes:None must not wipe existing size overrides");

    // (c) sizes = [] clears all size overrides; the size reverts to its catalog price.
    let body = BranchMenuOverrideInput {
        branch_id: branch, menu_item_id: item, price_override: Some(6500), is_available: true, sizes: Some(vec![]),
    };
    test::call_service(&app, auth_req!(put, "/branch-menu-overrides".to_string(), token, body)).await;
    let resp = test::call_service(&app, menu_full(&token)).await;
    let full: Vec<MenuItemFull> = test::read_body_json(resp).await;
    assert_eq!(size_price(&full[0], "large"), 8000, "cleared size override reverts to catalog");
}

/// A size override for a size the item doesn't have, or a negative price, is rejected.
#[sqlx::test]
async fn test_branch_size_override_validation(pool: PgPool) {
    let app = override_app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let cat = seed_category(&pool, org, "Coffee").await;
    let item = seed_menu_item(&pool, org, cat, "Latte", 5000).await;
    add_item_size(&pool, item, "large", 8000).await;
    let token = generate_org_admin_token(user, org);

    // 'small' is not a size of this item.
    let body = BranchMenuOverrideInput {
        branch_id: branch, menu_item_id: item, price_override: None, is_available: true,
        sizes: Some(vec![BranchSizeOverrideInput { size_label: "small".into(), price_override: 100 }]),
    };
    let resp = test::call_service(&app, auth_req!(put, "/branch-menu-overrides".to_string(), token, body)).await;
    assert_eq!(resp.status(), 400, "overriding a non-existent size is rejected");

    // Negative size price.
    let body = BranchMenuOverrideInput {
        branch_id: branch, menu_item_id: item, price_override: None, is_available: true,
        sizes: Some(vec![BranchSizeOverrideInput { size_label: "large".into(), price_override: -1 }]),
    };
    let resp = test::call_service(&app, auth_req!(put, "/branch-menu-overrides".to_string(), token, body)).await;
    assert_eq!(resp.status(), 400, "negative size price is rejected");
}

/// Deleting the item override also clears its size overrides.
#[sqlx::test]
async fn test_branch_size_overrides_cleared_on_delete(pool: PgPool) {
    let app = override_app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let cat = seed_category(&pool, org, "Coffee").await;
    let item = seed_menu_item(&pool, org, cat, "Latte", 5000).await;
    add_item_size(&pool, item, "large", 8000).await;
    let token = generate_org_admin_token(user, org);

    let body = BranchMenuOverrideInput {
        branch_id: branch, menu_item_id: item, price_override: Some(6000), is_available: true,
        sizes: Some(vec![BranchSizeOverrideInput { size_label: "large".into(), price_override: 9000 }]),
    };
    test::call_service(&app, auth_req!(put, "/branch-menu-overrides".to_string(), token, body)).await;

    let resp = test::call_service(&app, auth_req!(delete, format!("/branch-menu-overrides?branch_id={}&menu_item_id={}", branch, item), token)).await;
    assert_eq!(resp.status(), 204);

    let resp = test::call_service(&app, auth_req!(get, format!("/branch-menu-overrides?branch_id={}", branch), token)).await;
    let rows: Vec<BranchMenuOverride> = test::read_body_json(resp).await;
    assert!(rows.is_empty(), "delete clears both the item override and its size overrides");

    // And the branch menu reverts the size to its catalog price.
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/menu-items?org_id={}&branch_id={}&full=true", org, branch))
        .insert_header(("Authorization", format!("Bearer {}", token))).to_request()).await;
    let full: Vec<MenuItemFull> = test::read_body_json(resp).await;
    assert_eq!(size_price(&full[0], "large"), 8000);
}

// ── Branch addon overrides ────────────────────────────────────

#[sqlx::test]
async fn test_branch_addon_override_crud_and_injection(pool: PgPool) {
    let app = override_app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let addon = seed_addon_item(&pool, org, "Extra shot", "extra", 1000).await;
    let token = generate_org_admin_token(user, org);

    let branch_addons = |t: &str| test::TestRequest::get()
        .uri(&format!("/addon-items?org_id={}&branch_id={}", org, branch))
        .insert_header(("Authorization", format!("Bearer {}", t))).to_request();

    // No override → branch list shows the org default price.
    let resp = test::call_service(&app, branch_addons(&token)).await;
    let addons: Vec<AddonItem> = test::read_body_json(resp).await;
    assert_eq!(addons.len(), 1);
    assert_eq!(addons[0].default_price, 1000);

    // Upsert a branch addon price.
    let body = BranchAddonOverrideInput { branch_id: branch, addon_item_id: addon, price_override: Some(1500), is_available: true };
    let resp = test::call_service(&app, auth_req!(put, "/branch-addon-overrides".to_string(), token, body)).await;
    assert!(resp.status().is_success());
    let ov: BranchAddonOverride = test::read_body_json(resp).await;
    assert_eq!(ov.price_override, Some(1500));

    let resp = test::call_service(&app, branch_addons(&token)).await;
    let addons: Vec<AddonItem> = test::read_body_json(resp).await;
    assert_eq!(addons[0].default_price, 1500, "branch-effective addon price");

    // Org list (no branch) is unchanged.
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/addon-items?org_id={}", org))
        .insert_header(("Authorization", format!("Bearer {}", token))).to_request()).await;
    let addons: Vec<AddonItem> = test::read_body_json(resp).await;
    assert_eq!(addons[0].default_price, 1000, "org default unchanged");

    // Disable at this branch → excluded from the branch addon list.
    let body = BranchAddonOverrideInput { branch_id: branch, addon_item_id: addon, price_override: Some(1500), is_available: false };
    test::call_service(&app, auth_req!(put, "/branch-addon-overrides".to_string(), token, body)).await;
    let resp = test::call_service(&app, branch_addons(&token)).await;
    let addons: Vec<AddonItem> = test::read_body_json(resp).await;
    assert_eq!(addons.len(), 0, "branch-disabled addon is excluded");

    let resp = test::call_service(&app, auth_req!(get, format!("/branch-addon-overrides?branch_id={}", branch), token)).await;
    let rows: Vec<BranchAddonOverride> = test::read_body_json(resp).await;
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].is_available);

    // Delete → reverts to the org default.
    let resp = test::call_service(&app, auth_req!(delete, format!("/branch-addon-overrides?branch_id={}&addon_item_id={}", branch, addon), token)).await;
    assert_eq!(resp.status(), 204);
    let resp = test::call_service(&app, branch_addons(&token)).await;
    let addons: Vec<AddonItem> = test::read_body_json(resp).await;
    assert_eq!(addons.len(), 1);
    assert_eq!(addons[0].default_price, 1000);
}

// ── Server-side catalog: paginate / search / overridden filter + sort ─────────

#[sqlx::test]
async fn test_list_addon_catalog_paginate_search_filter_sort(pool: PgPool) {
    let app = override_app(pool.clone()).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;
    let _almond = seed_addon_item(&pool, org, "Almond Milk", "milk_type", 1200).await;
    let caramel = seed_addon_item(&pool, org, "Caramel Syrup", "extra", 500).await;
    let _oat = seed_addon_item(&pool, org, "Oat Milk", "milk_type", 1500).await;
    let token = generate_org_admin_token(user, org);

    let get = |uri: String| test::TestRequest::get().uri(&uri)
        .insert_header(("Authorization", format!("Bearer {}", token))).to_request();

    // All addons.
    let resp = test::call_service(&app, get(format!("/addon-items/catalog?org_id={}", org))).await;
    assert!(resp.status().is_success());
    let page: PaginatedAddonItems = test::read_body_json(resp).await;
    assert_eq!(page.total, 3);
    assert_eq!(page.data.len(), 3);

    // Search by name.
    let resp = test::call_service(&app, get(format!("/addon-items/catalog?org_id={}&search=caramel", org))).await;
    let page: PaginatedAddonItems = test::read_body_json(resp).await;
    assert_eq!(page.total, 1);
    assert_eq!(page.data[0].name, "Caramel Syrup");

    // Pagination envelope.
    let resp = test::call_service(&app, get(format!("/addon-items/catalog?org_id={}&per_page=2&page=1", org))).await;
    let page: PaginatedAddonItems = test::read_body_json(resp).await;
    assert_eq!(page.total, 3);
    assert_eq!(page.data.len(), 2);
    assert_eq!(page.total_pages, 2);

    // Override one addon at the branch.
    sqlx::query("INSERT INTO branch_addon_overrides (branch_id, addon_item_id, price_override, is_available) VALUES ($1,$2,800,true)")
        .bind(branch).bind(caramel).execute(&pool).await.unwrap();

    // Overridden-only filter.
    let resp = test::call_service(&app, get(format!("/addon-items/catalog?org_id={}&branch_id={}&overridden=true", org, branch))).await;
    let page: PaginatedAddonItems = test::read_body_json(resp).await;
    assert_eq!(page.total, 1);
    assert_eq!(page.data[0].id, caramel);
    assert_eq!(page.data[0].default_price, 500, "catalog returns the ORG price, not the branch override");

    // Overridden-first sort.
    let resp = test::call_service(&app, get(format!("/addon-items/catalog?org_id={}&branch_id={}&sort=overridden", org, branch))).await;
    let page: PaginatedAddonItems = test::read_body_json(resp).await;
    assert_eq!(page.data[0].id, caramel, "overridden addon sorts first");
}

#[sqlx::test]
async fn test_menu_catalog_overridden_filter_and_sort(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
            .configure(crate::costing::routes::configure),
    )
    .await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;
    let cat = seed_category(&pool, org, "Coffee").await;
    let _a = seed_menu_item(&pool, org, cat, "Americano", 4000).await;
    let latte = seed_menu_item(&pool, org, cat, "Latte", 5000).await;
    let _m = seed_menu_item(&pool, org, cat, "Mocha", 6000).await;
    let token = generate_org_admin_token(user, org);

    sqlx::query("INSERT INTO branch_menu_overrides (branch_id, menu_item_id, price_override, is_available) VALUES ($1,$2,5500,true)")
        .bind(branch).bind(latte).execute(&pool).await.unwrap();

    let get = |uri: String| test::TestRequest::get().uri(&uri)
        .insert_header(("Authorization", format!("Bearer {}", token))).to_request();

    // No branch_id → full org catalog (backward compatible).
    let resp = test::call_service(&app, get(format!("/costing/catalog?org_id={}", org))).await;
    assert!(resp.status().is_success());
    let page: PaginatedMenuItems = test::read_body_json(resp).await;
    assert_eq!(page.total, 3);

    // Overridden-only.
    let resp = test::call_service(&app, get(format!("/costing/catalog?org_id={}&branch_id={}&overridden=true", org, branch))).await;
    let page: PaginatedMenuItems = test::read_body_json(resp).await;
    assert_eq!(page.total, 1);
    assert_eq!(page.data[0].item.id, latte);

    // Overridden-first sort.
    let resp = test::call_service(&app, get(format!("/costing/catalog?org_id={}&branch_id={}&sort=overridden", org, branch))).await;
    let page: PaginatedMenuItems = test::read_body_json(resp).await;
    assert_eq!(page.data[0].item.id, latte, "overridden item sorts first");
}
