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

// ──────────────────────────────────────────────────────────────
// ── Public Menu Tests
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_public_menu_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await; // Need a branch for public menu if required, but the endpoint takes org_id usually
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;

    let cat_id = seed_category(&pool, org_id, "Mains").await;
    seed_menu_item(&pool, org_id, cat_id, "Coffee", 500).await;

    let req = test::TestRequest::get()
        .uri(&format!("/menu/public/{}", org_id))
        .to_request();

    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    assert!(status.is_success(), "Status: {}, Response: {:?}", status, body);

    // It should return PublicMenuResponse
    let resp_data: crate::menu::handlers::PublicMenuResponse = serde_json::from_slice(&body).unwrap();
    let cats = resp_data.categories;
    assert_eq!(cats.len(), 1);
    assert_eq!(cats[0].name, "Mains");
    assert_eq!(cats[0].items.len(), 1);
    assert_eq!(cats[0].items[0].name, "Coffee");
}

#[sqlx::test]
async fn test_public_menu_sets_etag_and_cache_headers(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    ).await;

    let org_id = seed_org(&pool).await;
    let cat_id = seed_category(&pool, org_id, "Mains").await;
    seed_menu_item(&pool, org_id, cat_id, "Coffee", 500).await;

    let req = test::TestRequest::get()
        .uri(&format!("/menu/public/{}", org_id))
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), actix_web::http::StatusCode::OK);
    assert!(
        resp.headers().get(actix_web::http::header::ETAG).is_some(),
        "response should carry an ETag",
    );
    let cache_control = resp
        .headers()
        .get(actix_web::http::header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        cache_control.contains("max-age"),
        "Cache-Control should set max-age, got {cache_control:?}",
    );
}

#[sqlx::test]
async fn test_public_menu_returns_304_for_matching_etag(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    ).await;

    let org_id = seed_org(&pool).await;
    let cat_id = seed_category(&pool, org_id, "Mains").await;
    seed_menu_item(&pool, org_id, cat_id, "Coffee", 500).await;

    // First request captures the ETag.
    let resp1 = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/menu/public/{}", org_id))
            .to_request(),
    ).await;
    let etag = resp1
        .headers()
        .get(actix_web::http::header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // Second request with If-None-Match should be 304 with no body.
    let resp2 = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/menu/public/{}", org_id))
            .insert_header((actix_web::http::header::IF_NONE_MATCH, etag))
            .to_request(),
    ).await;
    assert_eq!(resp2.status(), actix_web::http::StatusCode::NOT_MODIFIED);

    let body = test::read_body(resp2).await;
    assert!(body.is_empty(), "304 response body should be empty, got {body:?}");
}

#[sqlx::test]
async fn test_public_menu_inactive_org_returns_404(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    ).await;

    let org_id = seed_org(&pool).await;
    let cat_id = seed_category(&pool, org_id, "Mains").await;
    seed_menu_item(&pool, org_id, cat_id, "Coffee", 500).await;
    sqlx::query("UPDATE organizations SET is_active = false WHERE id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/menu/public/{}", org_id))
            .to_request(),
    ).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn test_public_menu_is_rate_limited(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    ).await;

    // The limiter runs before the handler, so a non-existent org still counts;
    // no seeding required. Burst is 30 — exceed it and expect a 429.
    let org_id = Uuid::new_v4();
    let mut got_429 = false;
    for _ in 0..40 {
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/menu/public/{}", org_id))
                .to_request(),
        ).await;
        if resp.status() == actix_web::http::StatusCode::TOO_MANY_REQUESTS {
            got_429 = true;
            break;
        }
    }
    assert!(got_429, "expected a 429 once the burst was exceeded");
}
