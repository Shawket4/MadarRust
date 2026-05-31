#![allow(unused_imports, unused_variables, dead_code)]
use actix_web::{test, App, web};
use sqlx::PgPool;
use uuid::Uuid;
use rust_decimal::Decimal;
use std::str::FromStr;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::inventory::routes;
use crate::inventory::handlers::{
    OrgIngredient, BranchInventoryItem, BranchInventoryAdjustment, BranchInventoryTransfer,
};

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn generate_token(user_id: Uuid, org_id: Option<Uuid>, role: UserRole) -> String {
    crate::auth::jwt::create_token(&get_secret(), user_id, org_id, role, None, 24).unwrap()
}

fn generate_super_admin_token() -> String {
    generate_token(Uuid::new_v4(), None, UserRole::SuperAdmin)
}

fn generate_org_admin_token(user_id: Uuid, org_id: Uuid) -> String {
    generate_token(user_id, Some(org_id), UserRole::OrgAdmin)
}

fn generate_branch_manager_token(user_id: Uuid, org_id: Uuid) -> String {
    generate_token(user_id, Some(org_id), UserRole::BranchManager)
}

fn generate_teller_token(user_id: Uuid, org_id: Uuid) -> String {
    generate_token(user_id, Some(org_id), UserRole::Teller)
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let org_id = Uuid::new_v4();
    let slug = format!("test-org-{}", org_id);
    sqlx::query(
        "INSERT INTO organizations (id, name, slug) VALUES ($1, 'Test Org', $2)"
    )
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

async fn assign_branch(pool: &PgPool, user_id: Uuid, branch_id: Uuid) {
    sqlx::query(
        "INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)"
    )
    .bind(user_id)
    .bind(branch_id)
    .execute(pool)
    .await
    .unwrap();
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

async fn seed_branch_inventory(
    pool: &PgPool,
    branch_id: Uuid,
    org_ingredient_id: Uuid,
    current_stock: f64,
    reorder_threshold: f64,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO branch_inventory (id, branch_id, org_ingredient_id, current_stock, reorder_threshold) \
         VALUES ($1, $2, $3, $4, $5)"
    )
    .bind(id)
    .bind(branch_id)
    .bind(org_ingredient_id)
    .bind(current_stock)
    .bind(reorder_threshold)
    .execute(pool)
    .await
    .unwrap();
    id
}

// ──────────────────────────────────────────────────────────────
// ── Org Catalog Tests
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_list_catalog_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "read").await;

    seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    seed_ingredient(&pool, org_id, "Lettuce", "g").await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::get()
        .uri(&format!("/inventory/orgs/{}/catalog", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let catalog: Vec<OrgIngredient> = test::read_body_json(resp).await;
    assert_eq!(catalog.len(), 2);
    // alphabetical ordering
    assert_eq!(catalog[0].name, "Lettuce");
    assert_eq!(catalog[1].name, "Tomato");
}

#[sqlx::test]
async fn test_list_catalog_forbidden(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_a = seed_org(&pool).await;
    let org_b = seed_org(&pool).await;

    let user_b = seed_user(&pool, org_b, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "read").await;

    let token = generate_org_admin_token(user_b, org_b);
    let req = test::TestRequest::get()
        .uri(&format!("/inventory/orgs/{}/catalog", org_a))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn test_create_catalog_item_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "create").await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::post()
        .uri(&format!("/inventory/orgs/{}/catalog", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "name": "Onion",
            "unit": "kg",
            "category": "veggies",
            "description": "Sweet onions",
            "cost_per_unit": 1.25
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::CREATED);

    let ingredient: OrgIngredient = test::read_body_json(resp).await;
    assert_eq!(ingredient.name, "Onion");
    assert_eq!(ingredient.unit, "kg");
    assert_eq!(ingredient.cost_per_unit, Decimal::from_str("1.25").unwrap());

    // Verify cost history was seeded
    let cost_history_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM ingredient_cost_history WHERE org_ingredient_id = $1 AND changed_by = $2)"
    )
    .bind(ingredient.id)
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(cost_history_exists);
}

#[sqlx::test]
async fn test_create_catalog_item_invalid_unit(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "create").await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::post()
        .uri(&format!("/inventory/orgs/{}/catalog", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "name": "Onion",
            "unit": "invalid_unit_name",
            "category": "veggies",
            "cost_per_unit": null
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn test_create_catalog_item_empty_name(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "create").await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::post()
        .uri(&format!("/inventory/orgs/{}/catalog", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "name": "  ",
            "unit": "kg",
            "category": "veggies",
            "cost_per_unit": null
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn test_create_catalog_item_conflict(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "create").await;

    seed_ingredient(&pool, org_id, "Tomato", "kg").await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::post()
        .uri(&format!("/inventory/orgs/{}/catalog", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "name": "Tomato",
            "unit": "kg",
            "category": "veggies",
            "cost_per_unit": 2.50
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::CONFLICT);
}

#[sqlx::test]
async fn test_update_catalog_item_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "update").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::patch()
        .uri(&format!("/inventory/orgs/{}/catalog/{}", org_id, ing_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "name": "Super Tomato",
            "cost_per_unit": 3.75
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::OK);

    let ingredient: OrgIngredient = test::read_body_json(resp).await;
    assert_eq!(ingredient.name, "Super Tomato");
    assert_eq!(ingredient.cost_per_unit, Decimal::from_str("3.75").unwrap());

    // Verify a new cost history entry was made
    let cost_history_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ingredient_cost_history WHERE org_ingredient_id = $1"
    )
    .bind(ing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cost_history_count, 1); // 1 since seed_ingredient used standard insert (no history), and update made 1
}

#[sqlx::test]
async fn test_update_catalog_item_invalid_unit(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "update").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::patch()
        .uri(&format!("/inventory/orgs/{}/catalog/{}", org_id, ing_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "unit": "ounces",
            "cost_per_unit": null
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn test_update_catalog_item_not_found(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "update").await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::patch()
        .uri(&format!("/inventory/orgs/{}/catalog/{}", org_id, Uuid::new_v4()))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "name": "Missing ingredient",
            "cost_per_unit": null
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn test_delete_catalog_item_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "delete").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::delete()
        .uri(&format!("/inventory/orgs/{}/catalog/{}", org_id, ing_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::NO_CONTENT);

    // Verify it is softly deleted
    let deleted_at: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT deleted_at FROM org_ingredients WHERE id = $1"
    )
    .bind(ing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(deleted_at.is_some());
}

#[sqlx::test]
async fn test_delete_catalog_item_referenced_conflict(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "delete").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    let branch_id = seed_branch(&pool, org_id).await;
    seed_branch_inventory(&pool, branch_id, ing_id, 10.0, 2.0).await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::delete()
        .uri(&format!("/inventory/orgs/{}/catalog/{}", org_id, ing_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::CONFLICT);
}

// ──────────────────────────────────────────────────────────────
// ── Branch Stock Tests
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_list_branch_stock_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "branch_manager").await;
    assign_branch(&pool, user_id, branch_id).await;
    grant_permission(&pool, "branch_manager", "inventory", "read").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    seed_branch_inventory(&pool, branch_id, ing_id, 1.5, 2.0).await; // 1.5 <= 2.0 -> below_reorder should be true

    let token = generate_branch_manager_token(user_id, org_id);
    let req = test::TestRequest::get()
        .uri(&format!("/inventory/branches/{}/stock", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let stock: Vec<BranchInventoryItem> = test::read_body_json(resp).await;
    assert_eq!(stock.len(), 1);
    assert_eq!(stock[0].ingredient_name, "Tomato");
    assert!(stock[0].below_reorder);
}

#[sqlx::test]
async fn test_list_branch_stock_forbidden(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "branch_manager").await;
    // Do NOT assign user_id to branch_id
    grant_permission(&pool, "branch_manager", "inventory", "read").await;

    let token = generate_branch_manager_token(user_id, org_id);
    let req = test::TestRequest::get()
        .uri(&format!("/inventory/branches/{}/stock", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn test_add_to_branch_stock_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "create").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::post()
        .uri(&format!("/inventory/branches/{}/stock", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "org_ingredient_id": ing_id,
            "current_stock": 15.0,
            "reorder_threshold": 5.0
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::CREATED);

    let stock_item: BranchInventoryItem = test::read_body_json(resp).await;
    assert_eq!(stock_item.ingredient_name, "Tomato");
    assert!(!stock_item.below_reorder);
}

#[sqlx::test]
async fn test_add_to_branch_stock_wrong_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_a = seed_org(&pool).await;
    let org_b = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_a).await;

    let user_a = seed_user(&pool, org_a, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "create").await;

    let ing_b = seed_ingredient(&pool, org_b, "Tomato", "kg").await;

    let token = generate_org_admin_token(user_a, org_a);
    let req = test::TestRequest::post()
        .uri(&format!("/inventory/branches/{}/stock", branch_a))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "org_ingredient_id": ing_b,
            "current_stock": 10.0
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn test_add_to_branch_stock_conflict(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "create").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    seed_branch_inventory(&pool, branch_id, ing_id, 1.0, 1.0).await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::post()
        .uri(&format!("/inventory/branches/{}/stock", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "org_ingredient_id": ing_id,
            "current_stock": 15.0
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::CONFLICT);
}

#[sqlx::test]
async fn test_update_branch_stock_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "update").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    let bi_id = seed_branch_inventory(&pool, branch_id, ing_id, 10.0, 2.0).await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::patch()
        .uri(&format!("/inventory/branches/{}/stock/{}", branch_id, bi_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "current_stock": 1.0
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::OK);

    let stock_item: BranchInventoryItem = test::read_body_json(resp).await;
    assert!(stock_item.below_reorder);
}

#[sqlx::test]
async fn test_update_branch_stock_not_found(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "update").await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::patch()
        .uri(&format!("/inventory/branches/{}/stock/{}", branch_id, Uuid::new_v4()))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "current_stock": 5.0
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn test_remove_from_branch_stock_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "delete").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    let bi_id = seed_branch_inventory(&pool, branch_id, ing_id, 10.0, 2.0).await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::delete()
        .uri(&format!("/inventory/branches/{}/stock/{}", branch_id, bi_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::NO_CONTENT);

    // Verify it is gone
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM branch_inventory WHERE id = $1)"
    )
    .bind(bi_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!exists);
}

#[sqlx::test]
async fn test_remove_from_branch_stock_referenced_conflict(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "delete").await;
    grant_permission(&pool, "org_admin", "inventory_adjustments", "create").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    let bi_id = seed_branch_inventory(&pool, branch_id, ing_id, 10.0, 2.0).await;

    // Add adjustment history
    sqlx::query(
        "INSERT INTO branch_inventory_adjustments (branch_id, branch_inventory_id, type, quantity, note, adjusted_by) \
         VALUES ($1, $2, 'add'::inventory_adjustment_type, 5.0, 'Initial seed adjustment', $3)"
    )
    .bind(branch_id)
    .bind(bi_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::delete()
        .uri(&format!("/inventory/branches/{}/stock/{}", branch_id, bi_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    // Foreign key constraint branch_inventory_adjustments_branch_inventory_id_fkey blocks deletion
    assert_eq!(resp.status(), actix_web::http::StatusCode::CONFLICT);
}

// ──────────────────────────────────────────────────────────────
// ── Adjustments Tests
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_create_adjustment_add_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "branch_manager").await;
    assign_branch(&pool, user_id, branch_id).await;
    grant_permission(&pool, "branch_manager", "inventory_adjustments", "create").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    let bi_id = seed_branch_inventory(&pool, branch_id, ing_id, 10.0, 2.0).await;

    let token = generate_branch_manager_token(user_id, org_id);
    let req = test::TestRequest::post()
        .uri(&format!("/inventory/branches/{}/adjustments", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "branch_inventory_id": bi_id,
            "adjustment_type": "add",
            "quantity": 5.5,
            "note": "Received extra tomatoes"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::CREATED);

    let adj: BranchInventoryAdjustment = test::read_body_json(resp).await;
    assert_eq!(adj.adjustment_type, "add");
    assert_eq!(adj.quantity, sqlx::types::BigDecimal::from_str("5.500").unwrap());

    // Verify stock was updated
    let new_stock: sqlx::types::BigDecimal = sqlx::query_scalar(
        "SELECT current_stock FROM branch_inventory WHERE id = $1"
    )
    .bind(bi_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(new_stock, sqlx::types::BigDecimal::from_str("15.500").unwrap());
}

#[sqlx::test]
async fn test_create_adjustment_remove_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "branch_manager").await;
    assign_branch(&pool, user_id, branch_id).await;
    grant_permission(&pool, "branch_manager", "inventory_adjustments", "create").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    let bi_id = seed_branch_inventory(&pool, branch_id, ing_id, 10.0, 2.0).await;

    let token = generate_branch_manager_token(user_id, org_id);
    let req = test::TestRequest::post()
        .uri(&format!("/inventory/branches/{}/adjustments", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "branch_inventory_id": bi_id,
            "adjustment_type": "remove",
            "quantity": 4.0,
            "note": "Spoiled tomatoes thrown away"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::CREATED);

    let adj: BranchInventoryAdjustment = test::read_body_json(resp).await;
    assert_eq!(adj.adjustment_type, "remove");

    let new_stock: sqlx::types::BigDecimal = sqlx::query_scalar(
        "SELECT current_stock FROM branch_inventory WHERE id = $1"
    )
    .bind(bi_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(new_stock, sqlx::types::BigDecimal::from_str("6.000").unwrap());
}

#[sqlx::test]
async fn test_create_adjustment_invalid_type(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "branch_manager").await;
    assign_branch(&pool, user_id, branch_id).await;
    grant_permission(&pool, "branch_manager", "inventory_adjustments", "create").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    let bi_id = seed_branch_inventory(&pool, branch_id, ing_id, 10.0, 2.0).await;

    let token = generate_branch_manager_token(user_id, org_id);
    let req = test::TestRequest::post()
        .uri(&format!("/inventory/branches/{}/adjustments", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "branch_inventory_id": bi_id,
            "adjustment_type": "set", // Invalid!
            "quantity": 5.0,
            "note": "some note"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn test_create_adjustment_invalid_qty(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "branch_manager").await;
    assign_branch(&pool, user_id, branch_id).await;
    grant_permission(&pool, "branch_manager", "inventory_adjustments", "create").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    let bi_id = seed_branch_inventory(&pool, branch_id, ing_id, 10.0, 2.0).await;

    let token = generate_branch_manager_token(user_id, org_id);
    let req = test::TestRequest::post()
        .uri(&format!("/inventory/branches/{}/adjustments", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "branch_inventory_id": bi_id,
            "adjustment_type": "add",
            "quantity": -1.0, // Invalid!
            "note": "some note"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn test_create_adjustment_insufficient_stock(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "branch_manager").await;
    assign_branch(&pool, user_id, branch_id).await;
    grant_permission(&pool, "branch_manager", "inventory_adjustments", "create").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    let bi_id = seed_branch_inventory(&pool, branch_id, ing_id, 3.0, 1.0).await;

    let token = generate_branch_manager_token(user_id, org_id);
    let req = test::TestRequest::post()
        .uri(&format!("/inventory/branches/{}/adjustments", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "branch_inventory_id": bi_id,
            "adjustment_type": "remove",
            "quantity": 5.0, // Insufficient!
            "note": "excessive spoil"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn test_create_adjustment_wrong_branch(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;

    let user_a = seed_user(&pool, org_id, "branch_manager").await;
    assign_branch(&pool, user_a, branch_a).await;
    grant_permission(&pool, "branch_manager", "inventory_adjustments", "create").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    let bi_b_id = seed_branch_inventory(&pool, branch_b, ing_id, 10.0, 2.0).await;

    let token = generate_branch_manager_token(user_a, org_id);
    // Attempting to make adjustment in branch_a using branch_b's inventory item
    let req = test::TestRequest::post()
        .uri(&format!("/inventory/branches/{}/adjustments", branch_a))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "branch_inventory_id": bi_b_id,
            "adjustment_type": "add",
            "quantity": 5.0,
            "note": "some note"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn test_list_adjustments_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "branch_manager").await;
    assign_branch(&pool, user_id, branch_id).await;
    grant_permission(&pool, "branch_manager", "inventory_adjustments", "read").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    let bi_id = seed_branch_inventory(&pool, branch_id, ing_id, 10.0, 2.0).await;

    // Add multiple adjustments
    sqlx::query(
        "INSERT INTO branch_inventory_adjustments (branch_id, branch_inventory_id, type, quantity, note, adjusted_by) \
         VALUES ($1, $2, 'add'::inventory_adjustment_type, 1.0, 'Note 1', $3), \
                ($1, $2, 'remove'::inventory_adjustment_type, 2.0, 'Note 2', $3)"
    )
    .bind(branch_id)
    .bind(bi_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let token = generate_branch_manager_token(user_id, org_id);
    let req = test::TestRequest::get()
        .uri(&format!("/inventory/branches/{}/adjustments", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let adjs: Vec<BranchInventoryAdjustment> = test::read_body_json(resp).await;
    assert_eq!(adjs.len(), 2);
    // Sorted by created_at DESC (Note 2 was inserted last, so it's first)
    assert_eq!(adjs[0].note, "Note 2");
    assert_eq!(adjs[1].note, "Note 1");
}

// ──────────────────────────────────────────────────────────────
// ── Transfers Tests
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_create_transfer_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let src_branch = seed_branch(&pool, org_id).await;
    let dst_branch = seed_branch(&pool, org_id).await;

    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory_transfers", "create").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    // Source must track and have stock
    seed_branch_inventory(&pool, src_branch, ing_id, 20.0, 2.0).await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::post()
        .uri("/inventory/transfers")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "source_branch_id": src_branch,
            "destination_branch_id": dst_branch,
            "org_ingredient_id": ing_id,
            "quantity": 5.0,
            "note": "Sending surplus tomatoes"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::CREATED);

    let transfer: BranchInventoryTransfer = test::read_body_json(resp).await;
    assert_eq!(transfer.quantity, sqlx::types::BigDecimal::from_str("5.000").unwrap());

    // Verify stock updates on both sides
    let src_stock: sqlx::types::BigDecimal = sqlx::query_scalar(
        "SELECT current_stock FROM branch_inventory WHERE branch_id = $1 AND org_ingredient_id = $2"
    )
    .bind(src_branch)
    .bind(ing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(src_stock, sqlx::types::BigDecimal::from_str("15.000").unwrap());

    let dst_stock: sqlx::types::BigDecimal = sqlx::query_scalar(
        "SELECT current_stock FROM branch_inventory WHERE branch_id = $1 AND org_ingredient_id = $2"
    )
    .bind(dst_branch)
    .bind(ing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(dst_stock, sqlx::types::BigDecimal::from_str("5.000").unwrap());

    // Verify adjustment entries were created
    let adj_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM branch_inventory_adjustments WHERE transfer_id = $1"
    )
    .bind(transfer.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(adj_count, 2);
}

#[sqlx::test]
async fn test_create_transfer_different_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_a = seed_org(&pool).await;
    let org_b = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_a).await;
    let branch_b = seed_branch(&pool, org_b).await;

    let user_a = seed_user(&pool, org_a, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory_transfers", "create").await;

    let ing_a = seed_ingredient(&pool, org_a, "Tomato", "kg").await;
    seed_branch_inventory(&pool, branch_a, ing_a, 20.0, 2.0).await;

    let token = generate_org_admin_token(user_a, org_a);
    let req = test::TestRequest::post()
        .uri("/inventory/transfers")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "source_branch_id": branch_a,
            "destination_branch_id": branch_b,
            "org_ingredient_id": ing_a,
            "quantity": 5.0
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn test_create_transfer_insufficient_stock(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let src_branch = seed_branch(&pool, org_id).await;
    let dst_branch = seed_branch(&pool, org_id).await;

    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory_transfers", "create").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    seed_branch_inventory(&pool, src_branch, ing_id, 3.0, 1.0).await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::post()
        .uri("/inventory/transfers")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "source_branch_id": src_branch,
            "destination_branch_id": dst_branch,
            "org_ingredient_id": ing_id,
            "quantity": 10.0 // Insufficient!
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn test_list_transfers(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;

    let user_id = seed_user(&pool, org_id, "branch_manager").await;
    assign_branch(&pool, user_id, branch_a).await;
    grant_permission(&pool, "branch_manager", "inventory_transfers", "read").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;

    // Insert outgoing and incoming transfers for branch_a
    sqlx::query(
        "INSERT INTO branch_inventory_transfers (org_id, source_branch_id, destination_branch_id, org_ingredient_id, quantity, note, initiated_by) \
         VALUES ($1, $2, $3, $4, 5.0, 'Outgoing', $5), \
                ($1, $3, $2, $4, 3.0, 'Incoming', $5)"
    )
    .bind(org_id)
    .bind(branch_a)
    .bind(branch_b)
    .bind(ing_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let token = generate_branch_manager_token(user_id, org_id);

    // Filter incoming
    let req_in = test::TestRequest::get()
        .uri(&format!("/inventory/branches/{}/transfers?direction=incoming", branch_a))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp_in = test::call_service(&app, req_in).await;
    assert!(resp_in.status().is_success());
    let transfers_in: Vec<BranchInventoryTransfer> = test::read_body_json(resp_in).await;
    assert_eq!(transfers_in.len(), 1);
    assert_eq!(transfers_in[0].note, Some("Incoming".to_string()));

    // Filter outgoing
    let req_out = test::TestRequest::get()
        .uri(&format!("/inventory/branches/{}/transfers?direction=outgoing", branch_a))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp_out = test::call_service(&app, req_out).await;
    assert!(resp_out.status().is_success());
    let transfers_out: Vec<BranchInventoryTransfer> = test::read_body_json(resp_out).await;
    assert_eq!(transfers_out.len(), 1);
    assert_eq!(transfers_out[0].note, Some("Outgoing".to_string()));

    // Both
    let req_both = test::TestRequest::get()
        .uri(&format!("/inventory/branches/{}/transfers", branch_a))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp_both = test::call_service(&app, req_both).await;
    assert!(resp_both.status().is_success());
    let transfers_both: Vec<BranchInventoryTransfer> = test::read_body_json(resp_both).await;
    assert_eq!(transfers_both.len(), 2);
}

#[sqlx::test]
async fn test_update_transfer_note(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;

    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory_transfers", "update").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;

    let transfer_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO branch_inventory_transfers (id, org_id, source_branch_id, destination_branch_id, org_ingredient_id, quantity, note, initiated_by) \
         VALUES ($1, $2, $3, $4, $5, 5.0, 'Old Note', $6)"
    )
    .bind(transfer_id)
    .bind(org_id)
    .bind(branch_a)
    .bind(branch_b)
    .bind(ing_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::patch()
        .uri(&format!("/inventory/transfers/{}", transfer_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "note": "Updated Note Content"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::OK);

    let updated: BranchInventoryTransfer = test::read_body_json(resp).await;
    assert_eq!(updated.note, Some("Updated Note Content".to_string()));
}

#[sqlx::test]
async fn test_delete_transfer_reverse_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let src_branch = seed_branch(&pool, org_id).await;
    let dst_branch = seed_branch(&pool, org_id).await;

    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory_transfers", "delete").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;

    // Setup stocks: src started with 15 (now has 15), dst started with 5 (now has 5)
    seed_branch_inventory(&pool, src_branch, ing_id, 15.0, 2.0).await;
    seed_branch_inventory(&pool, dst_branch, ing_id, 5.0, 2.0).await;

    let transfer_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO branch_inventory_transfers (id, org_id, source_branch_id, destination_branch_id, org_ingredient_id, quantity, note, initiated_by) \
         VALUES ($1, $2, $3, $4, $5, 5.0, 'Reversible Transfer', $6)"
    )
    .bind(transfer_id)
    .bind(org_id)
    .bind(src_branch)
    .bind(dst_branch)
    .bind(ing_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::delete()
        .uri(&format!("/inventory/transfers/{}", transfer_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::NO_CONTENT);

    // Verify stocks are reversed: src should be 15 + 5 = 20, dst should be 5 - 5 = 0
    let src_stock: sqlx::types::BigDecimal = sqlx::query_scalar(
        "SELECT current_stock FROM branch_inventory WHERE branch_id = $1 AND org_ingredient_id = $2"
    )
    .bind(src_branch)
    .bind(ing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(src_stock, sqlx::types::BigDecimal::from_str("20.000").unwrap());

    let dst_stock: sqlx::types::BigDecimal = sqlx::query_scalar(
        "SELECT current_stock FROM branch_inventory WHERE branch_id = $1 AND org_ingredient_id = $2"
    )
    .bind(dst_branch)
    .bind(ing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(dst_stock, sqlx::types::BigDecimal::from_str("0.000").unwrap());

    // Verify transfer record is deleted
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM branch_inventory_transfers WHERE id = $1)"
    )
    .bind(transfer_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!exists);
}
