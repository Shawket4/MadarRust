#![allow(unused_imports, unused_variables, dead_code)]
use actix_web::{App, test, web};
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::str::FromStr;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::inventory::handlers::{
    BranchInventoryItem, BranchInventoryMovement, BranchInventoryTransfer, OrgIngredient,
    OrgInventorySettings,
};
use crate::inventory::routes;
use crate::models::UserRole;

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
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)")
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
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
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
            .configure(routes::configure),
    )
    .await;

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
            .configure(routes::configure),
    )
    .await;

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
            .configure(routes::configure),
    )
    .await;

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
    assert_eq!(
        ingredient.cost_per_unit,
        Some(Decimal::from_str("1.25").unwrap())
    );

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
            .configure(routes::configure),
    )
    .await;

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
            .configure(routes::configure),
    )
    .await;

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
            .configure(routes::configure),
    )
    .await;

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
            .configure(routes::configure),
    )
    .await;

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
    assert_eq!(
        ingredient.cost_per_unit,
        Some(Decimal::from_str("3.75").unwrap())
    );

    // Verify a new cost history entry was made
    let cost_history_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ingredient_cost_history WHERE org_ingredient_id = $1",
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
            .configure(routes::configure),
    )
    .await;

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
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "update").await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::patch()
        .uri(&format!(
            "/inventory/orgs/{}/catalog/{}",
            org_id,
            Uuid::new_v4()
        ))
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
            .configure(routes::configure),
    )
    .await;

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
    let deleted_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM org_ingredients WHERE id = $1")
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
            .configure(routes::configure),
    )
    .await;

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
            .configure(routes::configure),
    )
    .await;

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
            .configure(routes::configure),
    )
    .await;

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
            .configure(routes::configure),
    )
    .await;

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
            .configure(routes::configure),
    )
    .await;

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
            .configure(routes::configure),
    )
    .await;

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
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "update").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    let bi_id = seed_branch_inventory(&pool, branch_id, ing_id, 10.0, 2.0).await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::patch()
        .uri(&format!(
            "/inventory/branches/{}/stock/{}",
            branch_id, bi_id
        ))
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
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "update").await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::patch()
        .uri(&format!(
            "/inventory/branches/{}/stock/{}",
            branch_id,
            Uuid::new_v4()
        ))
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
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "delete").await;

    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    let bi_id = seed_branch_inventory(&pool, branch_id, ing_id, 10.0, 2.0).await;

    let token = generate_org_admin_token(user_id, org_id);
    let req = test::TestRequest::delete()
        .uri(&format!(
            "/inventory/branches/{}/stock/{}",
            branch_id, bi_id
        ))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::NO_CONTENT);

    // Verify it is gone
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM branch_inventory WHERE id = $1)")
            .bind(bi_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!exists);
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
            .configure(routes::configure),
    )
    .await;

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
    assert_eq!(
        transfer.quantity,
        sqlx::types::BigDecimal::from_str("5.000").unwrap()
    );

    // Verify stock updates on both sides
    let src_stock: sqlx::types::BigDecimal = sqlx::query_scalar(
        "SELECT current_stock FROM branch_inventory WHERE branch_id = $1 AND org_ingredient_id = $2"
    )
    .bind(src_branch)
    .bind(ing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        src_stock,
        sqlx::types::BigDecimal::from_str("15.000").unwrap()
    );

    let dst_stock: sqlx::types::BigDecimal = sqlx::query_scalar(
        "SELECT current_stock FROM branch_inventory WHERE branch_id = $1 AND org_ingredient_id = $2"
    )
    .bind(dst_branch)
    .bind(ing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        dst_stock,
        sqlx::types::BigDecimal::from_str("5.000").unwrap()
    );

    // Verify two ledger movements (transfer_out + transfer_in) were posted —
    // the movement ledger is the audit trail for transfers.
    let mv_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM inventory_movements WHERE source_type = 'transfer' AND source_id = $1"
    )
    .bind(transfer.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(mv_count, 2);
}

#[sqlx::test]
async fn test_create_transfer_different_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

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
            .configure(routes::configure),
    )
    .await;

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
            .configure(routes::configure),
    )
    .await;

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
        .uri(&format!(
            "/inventory/branches/{}/transfers?direction=incoming",
            branch_a
        ))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp_in = test::call_service(&app, req_in).await;
    assert!(resp_in.status().is_success());
    let transfers_in: Vec<BranchInventoryTransfer> = test::read_body_json(resp_in).await;
    assert_eq!(transfers_in.len(), 1);
    assert_eq!(transfers_in[0].note, Some("Incoming".to_string()));

    // Filter outgoing
    let req_out = test::TestRequest::get()
        .uri(&format!(
            "/inventory/branches/{}/transfers?direction=outgoing",
            branch_a
        ))
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

/// nil {branch_id} = "All branches": list_transfers and list_waste both roll up
/// every branch in the caller's org (and never another org's rows), while a
/// specific {branch_id} still scopes to that one branch.
#[sqlx::test]
async fn test_list_transfers_and_waste_all_branches(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;
    let admin = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory_transfers", "read").await;
    grant_permission(&pool, "org_admin", "inventory_waste", "read").await;
    let token = generate_org_admin_token(admin, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    let ing = seed_ingredient(&pool, org_id, "Tomato", "kg").await;

    // One transfer touching each branch: A→B and B→A.
    sqlx::query(
        "INSERT INTO branch_inventory_transfers (org_id, source_branch_id, destination_branch_id, org_ingredient_id, quantity, note, initiated_by) \
         VALUES ($1, $2, $3, $4, 5.0, 'A to B', $5), \
                ($1, $3, $2, $4, 3.0, 'B to A', $5)"
    )
    .bind(org_id).bind(branch_a).bind(branch_b).bind(ing).bind(admin)
    .execute(&pool).await.unwrap();

    // One waste movement in each branch.
    for branch in [branch_a, branch_b] {
        sqlx::query(
            "INSERT INTO inventory_movements (id, branch_id, org_ingredient_id, type, quantity, created_by) \
             VALUES ($1, $2, $3, 'waste', -2.0, $4)"
        )
        .bind(Uuid::new_v4()).bind(branch).bind(ing).bind(admin)
        .execute(&pool).await.unwrap();
    }

    // A different org's transfer + waste must never appear in this org's roll-up.
    let other_org = seed_org(&pool).await;
    let other_branch = seed_branch(&pool, other_org).await;
    let other_branch2 = seed_branch(&pool, other_org).await;
    let other_admin = seed_user(&pool, other_org, "org_admin").await;
    let other_ing = seed_ingredient(&pool, other_org, "Onion", "kg").await;
    sqlx::query(
        "INSERT INTO branch_inventory_transfers (org_id, source_branch_id, destination_branch_id, org_ingredient_id, quantity, note, initiated_by) \
         VALUES ($1, $2, $3, $4, 1.0, 'other org', $5)"
    )
    .bind(other_org).bind(other_branch).bind(other_branch2).bind(other_ing).bind(other_admin)
    .execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO inventory_movements (id, branch_id, org_ingredient_id, type, quantity, created_by) \
         VALUES ($1, $2, $3, 'waste', -1.0, $4)"
    )
    .bind(Uuid::new_v4()).bind(other_branch).bind(other_ing).bind(other_admin)
    .execute(&pool).await.unwrap();

    let nil = Uuid::nil();

    // ── Transfers: all-branches sees both org transfers, org-isolated. ──
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{nil}/transfers"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let all_transfers: Vec<BranchInventoryTransfer> = test::read_body_json(resp).await;
    assert_eq!(
        all_transfers.len(),
        2,
        "all-branches sees both org transfers"
    );
    assert!(
        all_transfers.iter().all(|t| t.org_id == org_id),
        "other org excluded"
    );

    // A specific branch still scopes to transfers touching that one branch.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!(
                "/inventory/branches/{branch_a}/transfers?direction=outgoing"
            ))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let out_a: Vec<BranchInventoryTransfer> = test::read_body_json(resp).await;
    assert_eq!(out_a.len(), 1);
    assert_eq!(out_a[0].source_branch_id, branch_a);

    // ── Waste: all-branches rolls up both branches, branch-labelled, isolated. ──
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{nil}/waste"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let all_waste: Vec<BranchInventoryMovement> = test::read_body_json(resp).await;
    assert_eq!(all_waste.len(), 2, "all-branches sees both branches' waste");
    assert!(
        all_waste.iter().all(|m| m.branch_name.is_some()),
        "rows carry a branch label"
    );
    let seen: std::collections::HashSet<_> = all_waste.iter().map(|m| m.branch_id).collect();
    assert!(seen.contains(&branch_a) && seen.contains(&branch_b));

    // A specific branch still scopes waste to that one branch.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{branch_a}/waste"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let waste_a: Vec<BranchInventoryMovement> = test::read_body_json(resp).await;
    assert_eq!(waste_a.len(), 1);
    assert_eq!(waste_a[0].branch_id, branch_a);
}

#[sqlx::test]
async fn test_update_transfer_note(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

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
            .configure(routes::configure),
    )
    .await;

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
    assert_eq!(
        src_stock,
        sqlx::types::BigDecimal::from_str("20.000").unwrap()
    );

    let dst_stock: sqlx::types::BigDecimal = sqlx::query_scalar(
        "SELECT current_stock FROM branch_inventory WHERE branch_id = $1 AND org_ingredient_id = $2"
    )
    .bind(dst_branch)
    .bind(ing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        dst_stock,
        sqlx::types::BigDecimal::from_str("0.000").unwrap()
    );

    // Verify transfer record is deleted
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM branch_inventory_transfers WHERE id = $1)")
            .bind(transfer_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!exists);
}

#[sqlx::test]
async fn test_waste_deducts_stock_and_records_movement(pool: PgPool) {
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
    grant_permission(&pool, "org_admin", "inventory_waste", "create").await;

    let ing = seed_ingredient(&pool, org_id, "Cream", "ml").await;
    seed_branch_inventory(&pool, branch_id, ing, 100.0, 0.0).await;
    let token = generate_org_admin_token(user_id, org_id);

    // Waste 10 ml as spoiled.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/inventory/branches/{}/waste", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(serde_json::json!({"org_ingredient_id": ing, "quantity": 10.0, "reason": "spoiled"}))
        .to_request()).await;
    assert_eq!(resp.status(), 201);
    let mv: BranchInventoryMovement = test::read_body_json(resp).await;
    assert_eq!(mv.movement_type, "waste");
    assert_eq!(mv.reason.as_deref(), Some("spoiled"));

    // Stock dropped to 90.
    let stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE branch_id=$1 AND org_ingredient_id=$2")
        .bind(branch_id).bind(ing).fetch_one(&pool).await.unwrap();
    assert_eq!(stock, 90.0);

    // Wasting more than on hand is rejected.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/inventory/branches/{}/waste", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(serde_json::json!({"org_ingredient_id": ing, "quantity": 1000.0, "reason": "expired"}))
        .to_request()).await;
    assert_eq!(resp.status(), 400);

    // Invalid reason is rejected.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/inventory/branches/{}/waste", branch_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(
                serde_json::json!({"org_ingredient_id": ing, "quantity": 1.0, "reason": "bogus"}),
            )
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400);
}

// ──────────────────────────────────────────────────────────────
// Movement ledger + waste list
// ──────────────────────────────────────────────────────────────

macro_rules! init_app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(get_secret()))
                .configure(routes::configure),
        )
        .await
    };
}

#[sqlx::test]
async fn test_list_movements_and_filters(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let dst = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    for (r, a) in [
        ("inventory", "read"),
        ("inventory_transfers", "create"),
        ("inventory_waste", "create"),
    ] {
        grant_permission(&pool, "org_admin", r, a).await;
    }
    let ing = seed_ingredient(&pool, org_id, "Cream", "ml").await;
    let _bi = seed_branch_inventory(&pool, branch_id, ing, 100.0, 0.0).await;
    let token = generate_org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    // One transfer-out + one waste → two movements at this branch (different types).
    test::call_service(&app, test::TestRequest::post()
        .uri("/inventory/transfers").insert_header(auth.clone())
        .set_json(serde_json::json!({"source_branch_id": branch_id, "destination_branch_id": dst, "org_ingredient_id": ing, "quantity": 10.0})).to_request()).await;
    test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/inventory/branches/{branch_id}/waste"))
            .insert_header(auth.clone())
            .set_json(
                serde_json::json!({"org_ingredient_id": ing, "quantity": 5.0, "reason": "spoiled"}),
            )
            .to_request(),
    )
    .await;

    // All movements.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{branch_id}/movements"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let all: Vec<BranchInventoryMovement> = test::read_body_json(resp).await;
    assert_eq!(all.len(), 2);

    // Filter by type=waste.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!(
                "/inventory/branches/{branch_id}/movements?type=waste"
            ))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    let waste_only: Vec<BranchInventoryMovement> = test::read_body_json(resp).await;
    assert_eq!(waste_only.len(), 1);
    assert_eq!(waste_only[0].movement_type, "waste");

    // Filter by org_ingredient_id.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!(
                "/inventory/branches/{branch_id}/movements?org_ingredient_id={ing}"
            ))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    let by_ing: Vec<BranchInventoryMovement> = test::read_body_json(resp).await;
    assert_eq!(by_ing.len(), 2);

    // Waste list endpoint.
    grant_permission(&pool, "org_admin", "inventory_waste", "read").await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{branch_id}/waste"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    let waste_list: Vec<BranchInventoryMovement> = test::read_body_json(resp).await;
    assert_eq!(waste_list.len(), 1);
}

// ──────────────────────────────────────────────────────────────
// G3: low-stock noise guard (reorder_threshold > 0)
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_below_reorder_requires_positive_threshold(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "read").await;
    // A: empty, no level set (0/0) → must NOT flag. B: 5 on hand, level 10 → flags.
    let a = seed_ingredient(&pool, org_id, "Zero", "g").await;
    let b = seed_ingredient(&pool, org_id, "Low", "g").await;
    seed_branch_inventory(&pool, branch_id, a, 0.0, 0.0).await;
    seed_branch_inventory(&pool, branch_id, b, 5.0, 10.0).await;
    let token = generate_org_admin_token(user_id, org_id);

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{branch_id}/stock"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    let items: Vec<BranchInventoryItem> = test::read_body_json(resp).await;
    let row_a = items.iter().find(|i| i.org_ingredient_id == a).unwrap();
    let row_b = items.iter().find(|i| i.org_ingredient_id == b).unwrap();
    assert!(
        !row_a.below_reorder,
        "zero-threshold item must not be flagged"
    );
    assert!(row_b.below_reorder, "genuinely-low item must be flagged");
    // Neither has ever been counted.
    assert!(row_a.last_counted_at.is_none());
}

// ──────────────────────────────────────────────────────────────
// G2: ingredient → supplier link
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_catalog_supplier_link(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    for a in ["create", "read", "update"] {
        grant_permission(&pool, "org_admin", "inventory", a).await;
    }
    // Supplier seeded directly (purchasing routes not mounted here).
    let sup = Uuid::new_v4();
    sqlx::query("INSERT INTO suppliers (id, org_id, name) VALUES ($1, $2, 'Cairo Dairy')")
        .bind(sup)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    let token = generate_org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    // Create ingredient with supplier_id → response carries supplier_id + name.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/inventory/orgs/{org_id}/catalog")).insert_header(auth.clone())
        .set_json(serde_json::json!({"name": "Milk", "unit": "l", "category": "dairy", "cost_per_unit": null, "supplier_id": sup})).to_request()).await;
    assert_eq!(resp.status(), 201);
    let ing: OrgIngredient = test::read_body_json(resp).await;
    assert_eq!(ing.supplier_id, Some(sup));
    assert_eq!(ing.supplier_name.as_deref(), Some("Cairo Dairy"));

    // Cross-org supplier rejected (400).
    let other_org = seed_org(&pool).await;
    let other_sup = Uuid::new_v4();
    sqlx::query("INSERT INTO suppliers (id, org_id, name) VALUES ($1, $2, 'Other')")
        .bind(other_sup)
        .bind(other_org)
        .execute(&pool)
        .await
        .unwrap();
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/inventory/orgs/{org_id}/catalog")).insert_header(auth.clone())
        .set_json(serde_json::json!({"name": "Sugar", "unit": "kg", "category": "dry", "cost_per_unit": null, "supplier_id": other_sup})).to_request()).await;
    assert_eq!(resp.status(), 400);
}

// ──────────────────────────────────────────────────────────────
// G4: last_counted_at populated by a finalized count
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_last_counted_at_from_finalized_stocktake(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "read").await;
    let ing = seed_ingredient(&pool, org_id, "Beans", "g").await;
    seed_branch_inventory(&pool, branch_id, ing, 100.0, 0.0).await;
    let token = generate_org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    // Initially never counted.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{branch_id}/stock"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    let items: Vec<BranchInventoryItem> = test::read_body_json(resp).await;
    assert!(items[0].last_counted_at.is_none());

    // Seed a finalized stocktake that counted this ingredient.
    let st = Uuid::new_v4();
    sqlx::query("INSERT INTO stocktakes (id, org_id, branch_id, status, started_by, finalized_at) VALUES ($1,$2,$3,'finalized',$4, now())")
        .bind(st).bind(org_id).bind(branch_id).bind(user_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO stocktake_items (stocktake_id, org_ingredient_id, expected_qty, counted_qty) VALUES ($1,$2,100,98)")
        .bind(st).bind(ing).execute(&pool).await.unwrap();

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{branch_id}/stock"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    let items: Vec<BranchInventoryItem> = test::read_body_json(resp).await;
    assert!(
        items[0].last_counted_at.is_some(),
        "finalized count should populate last_counted_at"
    );
}

// ──────────────────────────────────────────────────────────────
// Org inventory settings (variance threshold)
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_inventory_settings_get_put(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    for a in ["read", "update"] {
        grant_permission(&pool, "org_admin", "inventory", a).await;
    }
    let token = generate_org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    // Default 10.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/orgs/{org_id}/settings"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let s: OrgInventorySettings = test::read_body_json(resp).await;
    assert_eq!(s.stocktake_variance_threshold_pct, 10.0);

    // Update to 15.
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/inventory/orgs/{org_id}/settings"))
            .insert_header(auth.clone())
            .set_json(serde_json::json!({"stocktake_variance_threshold_pct": 15.0}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let s: OrgInventorySettings = test::read_body_json(resp).await;
    assert_eq!(s.stocktake_variance_threshold_pct, 15.0);

    // Out of range → 400.
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/inventory/orgs/{org_id}/settings"))
            .insert_header(auth.clone())
            .set_json(serde_json::json!({"stocktake_variance_threshold_pct": 150.0}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400);
}

#[sqlx::test]
async fn test_inventory_settings_permission_denied(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "branch_manager").await;
    // branch_manager without inventory/read grant.
    let token = generate_branch_manager_token(user_id, org_id);
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/orgs/{org_id}/settings"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 403);
}

#[sqlx::test]
async fn test_catalog_unit_change_rebases_all_references(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    for a in ["read", "update"] {
        grant_permission(&pool, "org_admin", "inventory", a).await;
    }
    let ing = seed_ingredient(&pool, org_id, "Flour", "g").await; // grams
    sqlx::query("UPDATE org_ingredients SET cost_per_unit = 10 WHERE id = $1")
        .bind(ing)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO ingredient_cost_history (org_ingredient_id, cost_per_unit, effective_from) VALUES ($1, 10, now())")
        .bind(ing).execute(&pool).await.unwrap();
    // Branch stock 5000 g, reorder 1000 g.
    seed_branch_inventory(&pool, branch_id, ing, 5000.0, 1000.0).await;
    // A drink recipe using 18 g of it.
    let cat = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1,$2,'Bakery')")
        .bind(cat)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    let mi = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES ($1,$2,$3,'Bread',100,true)")
        .bind(mi).bind(org_id).bind(cat).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO menu_item_recipes (menu_item_id, size_label, ingredient_name, ingredient_unit, quantity_used, org_ingredient_id) VALUES ($1,'one_size','Flour','g',18,$2)")
        .bind(mi).bind(ing).execute(&pool).await.unwrap();
    let token = generate_org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    // Change the base unit g → kg.
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/inventory/orgs/{org_id}/catalog/{ing}"))
            .insert_header(auth.clone())
            .set_json(serde_json::json!({"unit": "kg"}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let updated: OrgIngredient = test::read_body_json(resp).await;
    assert_eq!(updated.unit, "kg");

    // Every reference is rebased by ÷1000 (quantities/stock) or ×1000 (cost).
    let cost: f64 =
        sqlx::query_scalar("SELECT cost_per_unit::float8 FROM org_ingredients WHERE id=$1")
            .bind(ing)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(cost, 10000.0); // 10 piastres/g → 10000 piastres/kg
    let hist: f64 = sqlx::query_scalar(
        "SELECT cost_per_unit::float8 FROM ingredient_cost_history WHERE org_ingredient_id=$1",
    )
    .bind(ing)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(hist, 10000.0);
    let (stock, reorder): (f64, f64) = sqlx::query_as("SELECT current_stock::float8, reorder_threshold::float8 FROM branch_inventory WHERE org_ingredient_id=$1").bind(ing).fetch_one(&pool).await.unwrap();
    assert_eq!(stock, 5.0);
    assert_eq!(reorder, 1.0);
    let (rqty, runit): (f64, String) = sqlx::query_as("SELECT quantity_used::float8, ingredient_unit FROM menu_item_recipes WHERE org_ingredient_id=$1").bind(ing).fetch_one(&pool).await.unwrap();
    assert_eq!(rqty, 0.018); // 18 g → 0.018 kg
    assert_eq!(runit, "kg");

    // Cross-measure changes are rejected (kg → l, kg → pcs).
    for bad in ["l", "pcs"] {
        let resp = test::call_service(
            &app,
            test::TestRequest::patch()
                .uri(&format!("/inventory/orgs/{org_id}/catalog/{ing}"))
                .insert_header(auth.clone())
                .set_json(serde_json::json!({"unit": bad}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 400, "changing kg → {bad} must be rejected");
    }

    // Changing the unit AND the cost in one request is rejected (ambiguous).
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/inventory/orgs/{org_id}/catalog/{ing}"))
            .insert_header(auth.clone())
            .set_json(serde_json::json!({"unit": "g", "cost_per_unit": 5}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400);
}

/// V26: a teller token bound to branch A must not read another branch's
/// inventory, even when the teller is assigned to both branches.
#[sqlx::test]
async fn test_teller_token_org_scoped_on_inventory(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;
    grant_permission(&pool, "teller", "inventory", "read").await;
    let teller = seed_user(&pool, org_id, "teller").await;
    assign_branch(&pool, teller, branch_a).await;
    assign_branch(&pool, teller, branch_b).await;

    // Token is bound to branch A only.
    let token = crate::auth::jwt::create_token(
        &get_secret(),
        teller,
        Some(org_id),
        UserRole::Teller,
        Some(branch_a),
        24,
    )
    .unwrap();

    // Branch A → allowed.
    let resp_a = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{}/stock", branch_a))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request(),
    )
    .await;
    assert!(resp_a.status().is_success(), "own branch must work");

    // D13: org-scoped — a teller token from branch A may read branch B in the
    // same org (no token-branch binding).
    let resp_b = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{}/stock", branch_b))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request(),
    )
    .await;
    assert!(
        resp_b.status().is_success(),
        "org teller may read any org branch's stock"
    );
}

/// Ledger integrity: SUM(movement.quantity) reconciles with current_stock, and
/// a unit change rebases the ledger alongside stock (it used to mutate stock
/// without touching the ledger, drifting the two apart permanently).
#[sqlx::test]
async fn test_ledger_reconciles_through_unit_change(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let src = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory_transfers", "create").await;
    grant_permission(&pool, "org_admin", "inventory", "update").await;
    let token = generate_org_admin_token(user_id, org_id);

    // branch_id starts at 0; all of its stock arrives through the ledger as a
    // transfer-in (the source's initial stock is on the OTHER branch).
    let ing_id = seed_ingredient(&pool, org_id, "Flour", "g").await;
    let bi_id = seed_branch_inventory(&pool, branch_id, ing_id, 0.0, 0.0).await;
    seed_branch_inventory(&pool, src, ing_id, 1500.0, 0.0).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/inventory/transfers")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&serde_json::json!({
                "source_branch_id": src, "destination_branch_id": branch_id,
                "org_ingredient_id": ing_id, "quantity": 1500.0
            }))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());

    // Reconciliation invariant: ledger sum == live stock (1500 g).
    let (stock, ledger): (f64, f64) = sqlx::query_as(
        "SELECT bi.current_stock::float8, \
                COALESCE((SELECT SUM(quantity) FROM inventory_movements \
                          WHERE branch_id = $1 AND org_ingredient_id = $2), 0)::float8 \
         FROM branch_inventory bi WHERE bi.id = $3",
    )
    .bind(branch_id)
    .bind(ing_id)
    .bind(bi_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stock, 1500.0);
    assert_eq!(ledger, 1500.0);

    // Change unit g → kg (factor 1000): stock and ledger both rebase to 1.5.
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/inventory/orgs/{}/catalog/{}", org_id, ing_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&serde_json::json!({"unit": "kg"}))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success(), "unit change must succeed");

    let (stock2, ledger2): (f64, f64) = sqlx::query_as(
        "SELECT bi.current_stock::float8, \
                COALESCE((SELECT SUM(quantity) FROM inventory_movements \
                          WHERE branch_id = $1 AND org_ingredient_id = $2), 0)::float8 \
         FROM branch_inventory bi WHERE bi.id = $3",
    )
    .bind(branch_id)
    .bind(ing_id)
    .bind(bi_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stock2, 1.5, "stock rebased to kg");
    assert_eq!(ledger2, 1.5, "ledger rebased with stock — still reconciles");
}

/// Changing an ingredient's yield rebases existing recipe quantities by old/new
/// so the effective consumption stays correct without re-saving recipes.
#[sqlx::test]
async fn test_yield_change_rebases_recipe_quantities(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "update").await;
    let token = generate_org_admin_token(user_id, org_id);

    // Ingredient at 50% yield; a recipe row already stores 200 g (= 100 g needed
    // grossed up by 1/0.5).
    let ing = seed_ingredient(&pool, org_id, "Chicken", "g").await;
    sqlx::query("UPDATE org_ingredients SET yield_pct = 50 WHERE id = $1")
        .bind(ing)
        .execute(&pool)
        .await
        .unwrap();
    let item = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO menu_items (id, org_id, name, base_price) VALUES ($1,$2,'Grill',1000)",
    )
    .bind(item)
    .bind(org_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO menu_item_recipes (menu_item_id, size_label, org_ingredient_id, ingredient_name, ingredient_unit, quantity_used) \
                 VALUES ($1,'one_size',$2,'Chicken','g',200)")
        .bind(item).bind(ing).execute(&pool).await.unwrap();

    // Drop yield to 25% → consumption doubles → stored qty 200 → 400.
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/inventory/orgs/{}/catalog/{}", org_id, ing))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&serde_json::json!({"yield_pct": 25}))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success(), "yield change must succeed");

    let qty: f64 = sqlx::query_scalar(
        "SELECT quantity_used::float8 FROM menu_item_recipes WHERE org_ingredient_id=$1",
    )
    .bind(ing)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        qty, 400.0,
        "recipe quantity rebased by old/new yield (0.5/0.25 = 2×)"
    );
}

/// A transfer blends the source branch's cost into the destination's WAC (cost
/// travels with the goods); the source cost is unchanged.
#[sqlx::test]
async fn test_transfer_blends_destination_wac(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let src = seed_branch(&pool, org_id).await;
    let dst = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory_transfers", "create").await;
    let token = generate_org_admin_token(user_id, org_id);

    let ing = seed_ingredient(&pool, org_id, "Beans", "g").await;
    seed_branch_inventory(&pool, src, ing, 100.0, 0.0).await;
    seed_branch_inventory(&pool, dst, ing, 100.0, 0.0).await;
    // Source actual cost 10/g, destination 20/g.
    sqlx::query("UPDATE branch_inventory SET cost_per_unit = 10 WHERE branch_id=$1 AND org_ingredient_id=$2").bind(src).bind(ing).execute(&pool).await.unwrap();
    sqlx::query("UPDATE branch_inventory SET cost_per_unit = 20 WHERE branch_id=$1 AND org_ingredient_id=$2").bind(dst).bind(ing).execute(&pool).await.unwrap();

    // Transfer 100 g src → dst.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/inventory/transfers")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&serde_json::json!({
                "source_branch_id": src, "destination_branch_id": dst,
                "org_ingredient_id": ing, "quantity": 100.0
            }))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());

    // Destination WAC = (100×20 + 100×10) / 200 = 15; source cost unchanged.
    let dst_cost: f64 = sqlx::query_scalar("SELECT cost_per_unit::float8 FROM branch_inventory WHERE branch_id=$1 AND org_ingredient_id=$2").bind(dst).bind(ing).fetch_one(&pool).await.unwrap();
    let src_cost: f64 = sqlx::query_scalar("SELECT cost_per_unit::float8 FROM branch_inventory WHERE branch_id=$1 AND org_ingredient_id=$2").bind(src).bind(ing).fetch_one(&pool).await.unwrap();
    assert_eq!(
        dst_cost, 15.0,
        "destination WAC blends the incoming source cost"
    );
    assert_eq!(src_cost, 10.0, "source cost is unchanged by the transfer");
}
