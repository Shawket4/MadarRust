#![allow(unused_imports, unused_variables, dead_code)]
use actix_web::{App, test, web};
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::str::FromStr;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::inventory::handlers::{
    BranchStockRow, IngredientCategory, OrgIngredient, OrgInventorySettings, StockMovement,
    StockTransfer,
};
use crate::inventory::routes;
use crate::models::UserRole;

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn generate_token(user_id: Uuid, org_id: Option<Uuid>, role: UserRole) -> String {
    crate::auth::jwt::create_token(&get_secret(), user_id, org_id, role, None, 24).unwrap()
}

fn generate_org_admin_token(user_id: Uuid, org_id: Uuid) -> String {
    generate_token(user_id, Some(org_id), UserRole::OrgAdmin)
}

fn generate_branch_manager_token(user_id: Uuid, org_id: Uuid) -> String {
    generate_token(user_id, Some(org_id), UserRole::BranchManager)
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Test Org', $2)")
        .bind(org_id)
        .bind(format!("test-org-{org_id}"))
        .execute(pool)
        .await
        .unwrap();
    org_id
}

async fn seed_branch(pool: &PgPool, org_id: Uuid) -> Uuid {
    let branch_id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(branch_id)
        .bind(org_id)
        .bind(format!("Test Branch {branch_id}"))
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
    .bind(format!("user-{user_id}@test.com"))
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

async fn category(pool: &PgPool, org_id: Uuid, slug: &str) -> Uuid {
    sqlx::query_scalar("SELECT ingredient_category_id($1, $2)")
        .bind(org_id)
        .bind(slug)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn seed_ingredient(pool: &PgPool, org_id: Uuid, name: &str, unit: &str) -> Uuid {
    let id = Uuid::new_v4();
    let cat = category(pool, org_id, "veggies").await;
    sqlx::query(
        "INSERT INTO org_ingredients (id, org_id, name, unit, category_id, description, cost_per_unit) \
         VALUES ($1, $2, $3, $4::inventory_unit, $5, 'Fresh ingredient', 2.50)"
    )
    .bind(id)
    .bind(org_id)
    .bind(name)
    .bind(unit)
    .bind(cat)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Opening stock arrives the only way stock can: through the ledger.
async fn seed_stock(pool: &PgPool, branch_id: Uuid, org_ingredient_id: Uuid, qty: f64) {
    if qty != 0.0 {
        sqlx::query("INSERT INTO inventory_movements (branch_id, org_ingredient_id, type, quantity, source_type) VALUES ($1, $2, 'purchase_in', $3, 'seed')")
            .bind(branch_id).bind(org_ingredient_id).bind(qty).execute(pool).await.unwrap();
    } else {
        sqlx::query("INSERT INTO branch_stock (branch_id, org_ingredient_id, on_hand) VALUES ($1, $2, 0) ON CONFLICT DO NOTHING")
            .bind(branch_id).bind(org_ingredient_id).execute(pool).await.unwrap();
    }
}

async fn set_par(
    pool: &PgPool,
    branch_id: Uuid,
    org_ingredient_id: Uuid,
    par_min: Option<f64>,
    par_max: Option<f64>,
) {
    sqlx::query("INSERT INTO branch_stock (branch_id, org_ingredient_id, on_hand, par_min, par_max) VALUES ($1, $2, 0, $3, $4) ON CONFLICT (branch_id, org_ingredient_id) DO UPDATE SET par_min = EXCLUDED.par_min, par_max = EXCLUDED.par_max")
        .bind(branch_id).bind(org_ingredient_id).bind(par_min).bind(par_max).execute(pool).await.unwrap();
}

async fn on_hand(pool: &PgPool, branch_id: Uuid, org_ingredient_id: Uuid) -> Option<f64> {
    sqlx::query_scalar(
        "SELECT on_hand::float8 FROM branch_stock WHERE branch_id = $1 AND org_ingredient_id = $2",
    )
    .bind(branch_id)
    .bind(org_ingredient_id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

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

macro_rules! auth {
    ($token:expr) => {
        ("Authorization", format!("Bearer {}", $token))
    };
}

// ──────────────────────────────────────────────────────────────
// Categories
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_categories_crud_and_delete_protection(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    for a in ["create", "read", "update", "delete"] {
        grant_permission(&pool, "org_admin", "inventory", a).await;
    }
    let token = generate_org_admin_token(user_id, org_id);

    // A fresh org already has `general` (seeded by the organizations trigger).
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/orgs/{org_id}/categories"))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let cats: Vec<IngredientCategory> = test::read_body_json(resp).await;
    assert_eq!(cats.len(), 1);
    assert_eq!(cats[0].slug, "general");
    let general = cats[0].id;

    // Create from a display name → slug derived.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/inventory/orgs/{org_id}/categories"))
            .insert_header(auth!(token))
            .set_json(serde_json::json!({"name": "Fresh Dairy"}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let dairy: IngredientCategory = test::read_body_json(resp).await;
    assert_eq!(dairy.slug, "fresh_dairy");
    assert_eq!(dairy.ingredient_count, 0);

    // Duplicate slug → 409.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/inventory/orgs/{org_id}/categories"))
            .insert_header(auth!(token))
            .set_json(serde_json::json!({"name": "fresh dairy"}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 409);

    // Rename.
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/inventory/orgs/{org_id}/categories/{}", dairy.id))
            .insert_header(auth!(token))
            .set_json(serde_json::json!({"name": "Dairy", "sort_order": 3}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let renamed: IngredientCategory = test::read_body_json(resp).await;
    assert_eq!(renamed.name, "Dairy");
    assert_eq!(renamed.slug, "fresh_dairy", "slug is immutable");

    // An ingredient in it blocks deletion until reassigned.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/inventory/orgs/{org_id}/catalog"))
            .insert_header(auth!(token))
            .set_json(serde_json::json!({"name": "Milk", "unit": "l", "category_id": dairy.id}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let milk: OrgIngredient = test::read_body_json(resp).await;
    assert_eq!(milk.category_slug, "fresh_dairy");

    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/inventory/orgs/{org_id}/categories/{}", dairy.id))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 409);
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!(
                "/inventory/orgs/{org_id}/categories/{}?reassign_to={general}",
                dairy.id
            ))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);
    let slug: String = sqlx::query_scalar("SELECT ic.slug FROM org_ingredients oi JOIN ingredient_categories ic ON ic.id = oi.category_id WHERE oi.id = $1")
        .bind(milk.id).fetch_one(&pool).await.unwrap();
    assert_eq!(slug, "general");

    // `general` itself can never go.
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/inventory/orgs/{org_id}/categories/{general}"))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400);
}

#[sqlx::test]
async fn test_category_cross_org_rejected(pool: PgPool) {
    let app = init_app!(pool);
    let org_a = seed_org(&pool).await;
    let org_b = seed_org(&pool).await;
    let user_a = seed_user(&pool, org_a, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "create").await;
    let token = generate_org_admin_token(user_a, org_a);
    let foreign = category(&pool, org_b, "spices").await;

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/inventory/orgs/{org_a}/catalog"))
            .insert_header(auth!(token))
            .set_json(serde_json::json!({"name": "Cumin", "unit": "g", "category_id": foreign}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400);
}

// ──────────────────────────────────────────────────────────────
// Catalog
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_list_catalog_success(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "read").await;
    seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    seed_ingredient(&pool, org_id, "Lettuce", "g").await;
    let token = generate_org_admin_token(user_id, org_id);

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/orgs/{org_id}/catalog"))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());
    let catalog: Vec<OrgIngredient> = test::read_body_json(resp).await;
    assert_eq!(catalog.len(), 2);
    assert_eq!(catalog[0].name, "Lettuce");
    assert_eq!(catalog[1].name, "Tomato");
    assert_eq!(catalog[0].category_name, "Veggies");
}

#[sqlx::test]
async fn test_list_catalog_forbidden(pool: PgPool) {
    let app = init_app!(pool);
    let org_a = seed_org(&pool).await;
    let org_b = seed_org(&pool).await;
    let user_b = seed_user(&pool, org_b, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "read").await;
    let token = generate_org_admin_token(user_b, org_b);
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/orgs/{org_a}/catalog"))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn test_create_catalog_item_defaults_to_general(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "create").await;
    let token = generate_org_admin_token(user_id, org_id);

    let resp = test::call_service(&app, test::TestRequest::post().uri(&format!("/inventory/orgs/{org_id}/catalog")).insert_header(auth!(token)).set_json(serde_json::json!({"name": "Onion", "unit": "kg", "description": "Sweet onions", "cost_per_unit": 1.25})).to_request()).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::CREATED);
    let ingredient: OrgIngredient = test::read_body_json(resp).await;
    assert_eq!(ingredient.name, "Onion");
    assert_eq!(ingredient.category_slug, "general");
    assert_eq!(
        ingredient.cost_per_unit,
        Some(Decimal::from_str("1.25").unwrap())
    );

    let cost_history_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ingredient_cost_history WHERE org_ingredient_id = $1 AND changed_by = $2)")
        .bind(ingredient.id).bind(user_id).fetch_one(&pool).await.unwrap();
    assert!(cost_history_exists);
}

#[sqlx::test]
async fn test_create_catalog_item_validation(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "create").await;
    seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    let token = generate_org_admin_token(user_id, org_id);
    let post = |body: serde_json::Value| {
        test::TestRequest::post()
            .uri(&format!("/inventory/orgs/{org_id}/catalog"))
            .insert_header(auth!(token))
            .set_json(body)
            .to_request()
    };

    assert_eq!(
        test::call_service(
            &app,
            post(serde_json::json!({"name": "Onion", "unit": "invalid_unit"}))
        )
        .await
        .status(),
        400
    );
    assert_eq!(
        test::call_service(&app, post(serde_json::json!({"name": "  ", "unit": "kg"})))
            .await
            .status(),
        400
    );
    assert_eq!(
        test::call_service(
            &app,
            post(serde_json::json!({"name": "Tomato", "unit": "kg"}))
        )
        .await
        .status(),
        409
    );
}

#[sqlx::test]
async fn test_update_catalog_item_success(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "update").await;
    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    let spices = category(&pool, org_id, "spices").await;
    let token = generate_org_admin_token(user_id, org_id);

    let resp = test::call_service(&app, test::TestRequest::patch().uri(&format!("/inventory/orgs/{org_id}/catalog/{ing_id}")).insert_header(auth!(token)).set_json(serde_json::json!({"name": "Super Tomato", "cost_per_unit": 3.75, "category_id": spices})).to_request()).await;
    assert_eq!(resp.status(), 200);
    let ingredient: OrgIngredient = test::read_body_json(resp).await;
    assert_eq!(ingredient.name, "Super Tomato");
    assert_eq!(ingredient.category_slug, "spices");
    assert_eq!(
        ingredient.cost_per_unit,
        Some(Decimal::from_str("3.75").unwrap())
    );

    let cost_history_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ingredient_cost_history WHERE org_ingredient_id = $1",
    )
    .bind(ing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cost_history_count, 1);

    // Invalid unit → 400; unknown id → 404.
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/inventory/orgs/{org_id}/catalog/{ing_id}"))
            .insert_header(auth!(token))
            .set_json(serde_json::json!({"unit": "ounces"}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!(
                "/inventory/orgs/{org_id}/catalog/{}",
                Uuid::new_v4()
            ))
            .insert_header(auth!(token))
            .set_json(serde_json::json!({"name": "Missing"}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 404);
}

#[sqlx::test]
async fn test_delete_catalog_item_rules(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "delete").await;
    let token = generate_org_admin_token(user_id, org_id);

    // Untouched ingredient → soft-deleted.
    let free = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/inventory/orgs/{org_id}/catalog/{free}"))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);
    let deleted_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM org_ingredients WHERE id = $1")
            .bind(free)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(deleted_at.is_some());

    // Stock on a shelf somewhere → 409 (count or waste it down first).
    let stocked = seed_ingredient(&pool, org_id, "Lettuce", "kg").await;
    seed_stock(&pool, branch_id, stocked, 10.0).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/inventory/orgs/{org_id}/catalog/{stocked}"))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 409);

    // Ledger history but zero on hand → deletable (history stays readable).
    let zeroed = seed_ingredient(&pool, org_id, "Basil", "g").await;
    seed_stock(&pool, branch_id, zeroed, 5.0).await;
    sqlx::query("INSERT INTO inventory_movements (branch_id, org_ingredient_id, type, quantity, reason) VALUES ($1,$2,'waste',-5,'spoiled')")
        .bind(branch_id).bind(zeroed).execute(&pool).await.unwrap();
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/inventory/orgs/{org_id}/catalog/{zeroed}"))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);
}

// ──────────────────────────────────────────────────────────────
// Branch stock: the whole catalog, always
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_list_branch_stock_shows_whole_catalog(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "branch_manager").await;
    assign_branch(&pool, user_id, branch_id).await;
    grant_permission(&pool, "branch_manager", "inventory", "read").await;

    let tomato = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    let onion = seed_ingredient(&pool, org_id, "Onion", "kg").await; // never moved here
    seed_stock(&pool, branch_id, tomato, 1.5).await;
    set_par(&pool, branch_id, tomato, Some(2.0), Some(10.0)).await;
    let token = generate_branch_manager_token(user_id, org_id);

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{branch_id}/stock"))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());
    let stock: Vec<BranchStockRow> = test::read_body_json(resp).await;
    assert_eq!(stock.len(), 2, "every catalog ingredient appears");

    let t = stock
        .iter()
        .find(|r| r.org_ingredient_id == tomato)
        .unwrap();
    assert_eq!(t.on_hand, 1.5);
    assert_eq!(t.par_min, Some(2.0));
    assert!(t.below_par);
    assert!(t.has_activity);
    assert!(t.last_movement_at.is_some());

    let o = stock.iter().find(|r| r.org_ingredient_id == onion).unwrap();
    assert_eq!(o.on_hand, 0.0);
    assert!(!o.has_activity);
    assert!(!o.below_par, "no par set → never flagged");
    assert!(o.last_counted_at.is_none());
}

#[sqlx::test]
async fn test_list_branch_stock_forbidden_for_unassigned_manager(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "branch_manager").await;
    grant_permission(&pool, "branch_manager", "inventory", "read").await;
    let token = generate_branch_manager_token(user_id, org_id);
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{branch_id}/stock"))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn test_set_par_levels(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "update").await;
    let ing = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    let token = generate_org_admin_token(user_id, org_id);
    let put = |body: serde_json::Value| {
        test::TestRequest::put()
            .uri(&format!("/inventory/branches/{branch_id}/stock/{ing}/par"))
            .insert_header(auth!(token))
            .set_json(body)
            .to_request()
    };

    // Setting par on a never-moved ingredient creates its row at 0 on hand.
    let resp = test::call_service(
        &app,
        put(serde_json::json!({"par_min": 5.0, "par_max": 20.0})),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let row: BranchStockRow = test::read_body_json(resp).await;
    assert_eq!(row.par_min, Some(5.0));
    assert_eq!(row.par_max, Some(20.0));
    assert_eq!(row.on_hand, 0.0);
    assert!(row.below_par, "0 on hand with a par of 5 is below par");

    // Clearing.
    let resp = test::call_service(
        &app,
        put(serde_json::json!({"par_min": null, "par_max": null})),
    )
    .await;
    let row: BranchStockRow = test::read_body_json(resp).await;
    assert_eq!(row.par_min, None);
    assert!(!row.below_par);

    // Validation.
    assert_eq!(
        test::call_service(
            &app,
            put(serde_json::json!({"par_min": 10.0, "par_max": 5.0}))
        )
        .await
        .status(),
        400
    );
    assert_eq!(
        test::call_service(&app, put(serde_json::json!({"par_min": -1.0})))
            .await
            .status(),
        400
    );

    // A foreign ingredient is rejected.
    let other = seed_org(&pool).await;
    let foreign = seed_ingredient(&pool, other, "X", "g").await;
    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!(
                "/inventory/branches/{branch_id}/stock/{foreign}/par"
            ))
            .insert_header(auth!(token))
            .set_json(serde_json::json!({"par_min": 1.0}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400);
}

// ──────────────────────────────────────────────────────────────
// Waste
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_waste_posts_to_ledger_and_validates_on_hand(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory_waste", "create").await;
    let ing = seed_ingredient(&pool, org_id, "Cream", "ml").await;
    let untouched = seed_ingredient(&pool, org_id, "Sugar", "g").await;
    seed_stock(&pool, branch_id, ing, 100.0).await;
    let token = generate_org_admin_token(user_id, org_id);
    let post = |body: serde_json::Value| {
        test::TestRequest::post()
            .uri(&format!("/inventory/branches/{branch_id}/waste"))
            .insert_header(auth!(token))
            .set_json(body)
            .to_request()
    };

    let resp = test::call_service(
        &app,
        post(serde_json::json!({"org_ingredient_id": ing, "quantity": 10.0, "reason": "spoiled"})),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let mv: StockMovement = test::read_body_json(resp).await;
    assert_eq!(mv.movement_type, "waste");
    assert_eq!(mv.reason.as_deref(), Some("spoiled"));
    assert_eq!(mv.balance_after.to_string().parse::<f64>().unwrap(), 90.0);
    assert!(mv.branch_stock_id.is_some());
    assert_eq!(on_hand(&pool, branch_id, ing).await, Some(90.0));

    // More than on hand → 400 with the figure; never-moved ingredient → same.
    let resp = test::call_service(
        &app,
        post(
            serde_json::json!({"org_ingredient_id": ing, "quantity": 1000.0, "reason": "expired"}),
        ),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let resp = test::call_service(&app, post(serde_json::json!({"org_ingredient_id": untouched, "quantity": 1.0, "reason": "expired"}))).await;
    assert_eq!(resp.status(), 400);
    // Invalid reason → 400.
    let resp = test::call_service(
        &app,
        post(serde_json::json!({"org_ingredient_id": ing, "quantity": 1.0, "reason": "bogus"})),
    )
    .await;
    assert_eq!(resp.status(), 400);
}

// ──────────────────────────────────────────────────────────────
// Transfers
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_create_transfer_success(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let src_branch = seed_branch(&pool, org_id).await;
    let dst_branch = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory_transfers", "create").await;
    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    seed_stock(&pool, src_branch, ing_id, 20.0).await;
    let token = generate_org_admin_token(user_id, org_id);

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/inventory/transfers")
            .insert_header(auth!(token))
            .set_json(serde_json::json!({
                "source_branch_id": src_branch, "destination_branch_id": dst_branch,
                "org_ingredient_id": ing_id, "quantity": 5.0, "note": "Sending surplus tomatoes"
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let transfer: StockTransfer = test::read_body_json(resp).await;
    assert_eq!(
        transfer.quantity,
        sqlx::types::BigDecimal::from_str("5.000").unwrap()
    );

    assert_eq!(on_hand(&pool, src_branch, ing_id).await, Some(15.0));
    assert_eq!(
        on_hand(&pool, dst_branch, ing_id).await,
        Some(5.0),
        "destination row created by the ledger"
    );

    let mv_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM inventory_movements WHERE source_type = 'transfer' AND source_id = $1")
        .bind(transfer.id).fetch_one(&pool).await.unwrap();
    assert_eq!(mv_count, 2);
}

#[sqlx::test]
async fn test_create_transfer_rejections(pool: PgPool) {
    let app = init_app!(pool);
    let org_a = seed_org(&pool).await;
    let org_b = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_a).await;
    let branch_a2 = seed_branch(&pool, org_a).await;
    let branch_b = seed_branch(&pool, org_b).await;
    let user_a = seed_user(&pool, org_a, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory_transfers", "create").await;
    let ing_a = seed_ingredient(&pool, org_a, "Tomato", "kg").await;
    seed_stock(&pool, branch_a, ing_a, 3.0).await;
    let never_moved = seed_ingredient(&pool, org_a, "Onion", "kg").await;
    let token = generate_org_admin_token(user_a, org_a);
    let post = |body: serde_json::Value| {
        test::TestRequest::post()
            .uri("/inventory/transfers")
            .insert_header(auth!(token))
            .set_json(body)
            .to_request()
    };

    // Cross-org destination.
    let resp = test::call_service(&app, post(serde_json::json!({"source_branch_id": branch_a, "destination_branch_id": branch_b, "org_ingredient_id": ing_a, "quantity": 1.0}))).await;
    assert!(matches!(resp.status().as_u16(), 400 | 404));
    // Insufficient stock.
    let resp = test::call_service(&app, post(serde_json::json!({"source_branch_id": branch_a, "destination_branch_id": branch_a2, "org_ingredient_id": ing_a, "quantity": 10.0}))).await;
    assert_eq!(resp.status(), 400);
    // Never moved at the source → 0 on hand → 400.
    let resp = test::call_service(&app, post(serde_json::json!({"source_branch_id": branch_a, "destination_branch_id": branch_a2, "org_ingredient_id": never_moved, "quantity": 1.0}))).await;
    assert_eq!(resp.status(), 400);
    // Same branch.
    let resp = test::call_service(&app, post(serde_json::json!({"source_branch_id": branch_a, "destination_branch_id": branch_a, "org_ingredient_id": ing_a, "quantity": 1.0}))).await;
    assert_eq!(resp.status(), 400);
}

#[sqlx::test]
async fn test_list_transfers(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "branch_manager").await;
    assign_branch(&pool, user_id, branch_a).await;
    grant_permission(&pool, "branch_manager", "inventory_transfers", "read").await;
    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;

    sqlx::query(
        "INSERT INTO stock_transfers (org_id, source_branch_id, destination_branch_id, org_ingredient_id, quantity, note, initiated_by) \
         VALUES ($1, $2, $3, $4, 5.0, 'Outgoing', $5), ($1, $3, $2, $4, 3.0, 'Incoming', $5)"
    )
    .bind(org_id).bind(branch_a).bind(branch_b).bind(ing_id).bind(user_id).execute(&pool).await.unwrap();
    let token = generate_branch_manager_token(user_id, org_id);

    for (dir, expect_note, expect_len) in [
        ("?direction=incoming", Some("Incoming"), 1),
        ("?direction=outgoing", Some("Outgoing"), 1),
        ("", None, 2),
    ] {
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/inventory/branches/{branch_a}/transfers{dir}"))
                .insert_header(auth!(token))
                .to_request(),
        )
        .await;
        assert!(resp.status().is_success());
        let rows: Vec<StockTransfer> = test::read_body_json(resp).await;
        assert_eq!(rows.len(), expect_len);
        if let Some(n) = expect_note {
            assert_eq!(rows[0].note.as_deref(), Some(n));
        }
    }
}

/// nil {branch_id} = "All branches": list_transfers and list_waste both roll up
/// every branch in the caller's org (never another org's), while a specific
/// {branch_id} still scopes to that one branch.
#[sqlx::test]
async fn test_list_transfers_and_waste_all_branches(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;
    let admin = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory_transfers", "read").await;
    grant_permission(&pool, "org_admin", "inventory_waste", "read").await;
    let token = generate_org_admin_token(admin, org_id);
    let ing = seed_ingredient(&pool, org_id, "Tomato", "kg").await;

    sqlx::query(
        "INSERT INTO stock_transfers (org_id, source_branch_id, destination_branch_id, org_ingredient_id, quantity, note, initiated_by) \
         VALUES ($1, $2, $3, $4, 5.0, 'A to B', $5), ($1, $3, $2, $4, 3.0, 'B to A', $5)"
    )
    .bind(org_id).bind(branch_a).bind(branch_b).bind(ing).bind(admin).execute(&pool).await.unwrap();
    for branch in [branch_a, branch_b] {
        sqlx::query("INSERT INTO inventory_movements (branch_id, org_ingredient_id, type, quantity, created_by) VALUES ($1, $2, 'waste', -2.0, $3)")
            .bind(branch).bind(ing).bind(admin).execute(&pool).await.unwrap();
    }

    let other_org = seed_org(&pool).await;
    let other_branch = seed_branch(&pool, other_org).await;
    let other_branch2 = seed_branch(&pool, other_org).await;
    let other_admin = seed_user(&pool, other_org, "org_admin").await;
    let other_ing = seed_ingredient(&pool, other_org, "Onion", "kg").await;
    sqlx::query("INSERT INTO stock_transfers (org_id, source_branch_id, destination_branch_id, org_ingredient_id, quantity, note, initiated_by) VALUES ($1, $2, $3, $4, 1.0, 'other org', $5)")
        .bind(other_org).bind(other_branch).bind(other_branch2).bind(other_ing).bind(other_admin).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO inventory_movements (branch_id, org_ingredient_id, type, quantity, created_by) VALUES ($1, $2, 'waste', -1.0, $3)")
        .bind(other_branch).bind(other_ing).bind(other_admin).execute(&pool).await.unwrap();

    let nil = Uuid::nil();
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{nil}/transfers"))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let all_transfers: Vec<StockTransfer> = test::read_body_json(resp).await;
    assert_eq!(all_transfers.len(), 2);
    assert!(all_transfers.iter().all(|t| t.org_id == org_id));

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!(
                "/inventory/branches/{branch_a}/transfers?direction=outgoing"
            ))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    let out_a: Vec<StockTransfer> = test::read_body_json(resp).await;
    assert_eq!(out_a.len(), 1);
    assert_eq!(out_a[0].source_branch_id, branch_a);

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{nil}/waste"))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let all_waste: Vec<StockMovement> = test::read_body_json(resp).await;
    assert_eq!(all_waste.len(), 2);
    assert!(all_waste.iter().all(|m| m.branch_name.is_some()));

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{branch_a}/waste"))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    let waste_a: Vec<StockMovement> = test::read_body_json(resp).await;
    assert_eq!(waste_a.len(), 1);
    assert_eq!(waste_a[0].branch_id, branch_a);
}

#[sqlx::test]
async fn test_update_transfer_note(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory_transfers", "update").await;
    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    let transfer_id = Uuid::new_v4();
    sqlx::query("INSERT INTO stock_transfers (id, org_id, source_branch_id, destination_branch_id, org_ingredient_id, quantity, note, initiated_by) VALUES ($1, $2, $3, $4, $5, 5.0, 'Old Note', $6)")
        .bind(transfer_id).bind(org_id).bind(branch_a).bind(branch_b).bind(ing_id).bind(user_id).execute(&pool).await.unwrap();
    let token = generate_org_admin_token(user_id, org_id);

    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/inventory/transfers/{transfer_id}"))
            .insert_header(auth!(token))
            .set_json(serde_json::json!({"note": "Updated Note Content"}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let updated: StockTransfer = test::read_body_json(resp).await;
    assert_eq!(updated.note.as_deref(), Some("Updated Note Content"));
}

#[sqlx::test]
async fn test_delete_transfer_reverses_through_ledger(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let src_branch = seed_branch(&pool, org_id).await;
    let dst_branch = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory_transfers", "create").await;
    grant_permission(&pool, "org_admin", "inventory_transfers", "delete").await;
    let ing_id = seed_ingredient(&pool, org_id, "Tomato", "kg").await;
    seed_stock(&pool, src_branch, ing_id, 20.0).await;
    let token = generate_org_admin_token(user_id, org_id);

    let resp = test::call_service(&app, test::TestRequest::post().uri("/inventory/transfers").insert_header(auth!(token)).set_json(serde_json::json!({
        "source_branch_id": src_branch, "destination_branch_id": dst_branch, "org_ingredient_id": ing_id, "quantity": 5.0
    })).to_request()).await;
    assert_eq!(resp.status(), 201);
    let transfer: StockTransfer = test::read_body_json(resp).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/inventory/transfers/{}", transfer.id))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 204);
    assert_eq!(on_hand(&pool, src_branch, ing_id).await, Some(20.0));
    assert_eq!(on_hand(&pool, dst_branch, ing_id).await, Some(0.0));
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM stock_transfers WHERE id = $1)")
            .bind(transfer.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!exists);
    // Four ledger rows in total: two for the transfer, two for the reversal.
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM inventory_movements WHERE source_type='transfer' AND source_id=$1",
    )
    .bind(transfer.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 4);

    // A reversal the destination can no longer cover is refused.
    let resp = test::call_service(&app, test::TestRequest::post().uri("/inventory/transfers").insert_header(auth!(token)).set_json(serde_json::json!({
        "source_branch_id": src_branch, "destination_branch_id": dst_branch, "org_ingredient_id": ing_id, "quantity": 5.0
    })).to_request()).await;
    let t2: StockTransfer = test::read_body_json(resp).await;
    sqlx::query("INSERT INTO inventory_movements (branch_id, org_ingredient_id, type, quantity, reason) VALUES ($1,$2,'waste',-4,'spoiled')")
        .bind(dst_branch).bind(ing_id).execute(&pool).await.unwrap();
    let resp = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/inventory/transfers/{}", t2.id))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 409);
}

/// A transfer blends the source branch's cost into the destination's WAC (cost
/// travels with the goods); the source cost is unchanged.
#[sqlx::test]
async fn test_transfer_blends_destination_wac(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let src = seed_branch(&pool, org_id).await;
    let dst = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory_transfers", "create").await;
    let token = generate_org_admin_token(user_id, org_id);

    let ing = seed_ingredient(&pool, org_id, "Beans", "g").await;
    seed_stock(&pool, src, ing, 100.0).await;
    seed_stock(&pool, dst, ing, 100.0).await;
    sqlx::query(
        "UPDATE branch_stock SET cost_per_unit = 10 WHERE branch_id=$1 AND org_ingredient_id=$2",
    )
    .bind(src)
    .bind(ing)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE branch_stock SET cost_per_unit = 20 WHERE branch_id=$1 AND org_ingredient_id=$2",
    )
    .bind(dst)
    .bind(ing)
    .execute(&pool)
    .await
    .unwrap();

    let resp = test::call_service(&app, test::TestRequest::post().uri("/inventory/transfers").insert_header(auth!(token)).set_json(serde_json::json!({
        "source_branch_id": src, "destination_branch_id": dst, "org_ingredient_id": ing, "quantity": 100.0
    })).to_request()).await;
    assert!(resp.status().is_success());

    let dst_cost: f64 = sqlx::query_scalar("SELECT cost_per_unit::float8 FROM branch_stock WHERE branch_id=$1 AND org_ingredient_id=$2").bind(dst).bind(ing).fetch_one(&pool).await.unwrap();
    let src_cost: f64 = sqlx::query_scalar("SELECT cost_per_unit::float8 FROM branch_stock WHERE branch_id=$1 AND org_ingredient_id=$2").bind(src).bind(ing).fetch_one(&pool).await.unwrap();
    assert_eq!(
        dst_cost, 15.0,
        "destination WAC blends the incoming source cost"
    );
    assert_eq!(src_cost, 10.0, "source cost is unchanged by the transfer");
}

// ──────────────────────────────────────────────────────────────
// Movement ledger
// ──────────────────────────────────────────────────────────────

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
        ("inventory_waste", "read"),
    ] {
        grant_permission(&pool, "org_admin", r, a).await;
    }
    let ing = seed_ingredient(&pool, org_id, "Cream", "ml").await;
    seed_stock(&pool, branch_id, ing, 100.0).await; // one purchase_in movement
    let token = generate_org_admin_token(user_id, org_id);

    test::call_service(&app, test::TestRequest::post().uri("/inventory/transfers").insert_header(auth!(token))
        .set_json(serde_json::json!({"source_branch_id": branch_id, "destination_branch_id": dst, "org_ingredient_id": ing, "quantity": 10.0})).to_request()).await;
    test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/inventory/branches/{branch_id}/waste"))
            .insert_header(auth!(token))
            .set_json(
                serde_json::json!({"org_ingredient_id": ing, "quantity": 5.0, "reason": "spoiled"}),
            )
            .to_request(),
    )
    .await;

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{branch_id}/movements"))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let all: Vec<StockMovement> = test::read_body_json(resp).await;
    assert_eq!(all.len(), 3);
    // Newest first, and every row carries the running balance.
    assert_eq!(all[0].movement_type, "waste");
    assert_eq!(
        all[0].balance_after.to_string().parse::<f64>().unwrap(),
        85.0
    );
    assert_eq!(
        all[1].balance_after.to_string().parse::<f64>().unwrap(),
        90.0
    );

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!(
                "/inventory/branches/{branch_id}/movements?type=waste"
            ))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    let waste_only: Vec<StockMovement> = test::read_body_json(resp).await;
    assert_eq!(waste_only.len(), 1);

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{branch_id}/waste"))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    let waste_list: Vec<StockMovement> = test::read_body_json(resp).await;
    assert_eq!(waste_list.len(), 1);
}

/// Ledger integrity: SUM(movement.quantity) always equals on_hand, and a unit
/// change rebases ledger, balance, par levels and costs together.
#[sqlx::test]
async fn test_ledger_reconciles_through_unit_change(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "update").await;
    let token = generate_org_admin_token(user_id, org_id);

    let ing_id = seed_ingredient(&pool, org_id, "Flour", "g").await;
    sqlx::query("UPDATE org_ingredients SET cost_per_unit = 10 WHERE id = $1")
        .bind(ing_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO ingredient_cost_history (org_ingredient_id, cost_per_unit, effective_from) VALUES ($1, 10, now())").bind(ing_id).execute(&pool).await.unwrap();
    seed_stock(&pool, branch_id, ing_id, 1500.0).await;
    set_par(&pool, branch_id, ing_id, Some(1000.0), Some(5000.0)).await;

    let reconcile = |pool: PgPool| async move {
        let (stock, ledger, par_min, par_max): (f64, f64, f64, f64) = sqlx::query_as(
            "SELECT bs.on_hand::float8, \
                    COALESCE((SELECT SUM(quantity) FROM inventory_movements WHERE branch_id = $1 AND org_ingredient_id = $2), 0)::float8, \
                    bs.par_min::float8, bs.par_max::float8 \
             FROM branch_stock bs WHERE bs.branch_id = $1 AND bs.org_ingredient_id = $2",
        )
        .bind(branch_id).bind(ing_id).fetch_one(&pool).await.unwrap();
        (stock, ledger, par_min, par_max)
    };
    assert_eq!(
        reconcile(pool.clone()).await,
        (1500.0, 1500.0, 1000.0, 5000.0)
    );

    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/inventory/orgs/{org_id}/catalog/{ing_id}"))
            .insert_header(auth!(token))
            .set_json(serde_json::json!({"unit": "kg"}))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success(), "unit change must succeed");
    let updated: OrgIngredient = test::read_body_json(resp).await;
    assert_eq!(updated.unit, "kg");

    assert_eq!(
        reconcile(pool.clone()).await,
        (1.5, 1.5, 1.0, 5.0),
        "everything rebased by ÷1000"
    );
    let cost: f64 =
        sqlx::query_scalar("SELECT cost_per_unit::float8 FROM org_ingredients WHERE id=$1")
            .bind(ing_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(cost, 10000.0);
    let hist: f64 = sqlx::query_scalar(
        "SELECT cost_per_unit::float8 FROM ingredient_cost_history WHERE org_ingredient_id=$1",
    )
    .bind(ing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(hist, 10000.0);

    // Cross-measure changes and unit+cost in one request are rejected.
    for bad in ["l", "pcs"] {
        let resp = test::call_service(
            &app,
            test::TestRequest::patch()
                .uri(&format!("/inventory/orgs/{org_id}/catalog/{ing_id}"))
                .insert_header(auth!(token))
                .set_json(serde_json::json!({"unit": bad}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 400, "changing kg → {bad} must be rejected");
    }
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/inventory/orgs/{org_id}/catalog/{ing_id}"))
            .insert_header(auth!(token))
            .set_json(serde_json::json!({"unit": "g", "cost_per_unit": 5}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400);
}

#[sqlx::test]
async fn test_catalog_unit_change_rebases_recipes(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "update").await;
    let ing = seed_ingredient(&pool, org_id, "Flour", "g").await;
    let cat = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1,$2,'Bakery')")
        .bind(cat)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    let mi = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES ($1,$2,$3,'Bread',100,true)").bind(mi).bind(org_id).bind(cat).execute(&pool).await.unwrap();
    let size = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_item_sizes (id, menu_item_id, label, price, sort) VALUES ($1,$2,'one_size',100,0)").bind(size).bind(mi).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit) VALUES ('item_size',$1,$2,18,'g')").bind(size).bind(ing).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO menu_item_recipes (menu_item_id, size_label, ingredient_name, ingredient_unit, quantity_used, org_ingredient_id) VALUES ($1,'one_size','Flour','g',18,$2)").bind(mi).bind(ing).execute(&pool).await.unwrap();
    let token = generate_org_admin_token(user_id, org_id);

    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/inventory/orgs/{org_id}/catalog/{ing}"))
            .insert_header(auth!(token))
            .set_json(serde_json::json!({"unit": "kg"}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let (rqty, runit): (f64, String) =
        sqlx::query_as("SELECT quantity::float8, unit FROM recipe_lines WHERE ingredient_id=$1")
            .bind(ing)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rqty, 0.018);
    assert_eq!(runit, "kg");
    let (lqty, lunit): (f64, String) = sqlx::query_as("SELECT quantity_used::float8, ingredient_unit FROM menu_item_recipes WHERE org_ingredient_id=$1").bind(ing).fetch_one(&pool).await.unwrap();
    assert_eq!(lqty, 0.018);
    assert_eq!(lunit, "kg");
}

/// Changing an ingredient's yield rebases existing recipe quantities by old/new
/// so the effective consumption stays correct without re-saving recipes.
#[sqlx::test]
async fn test_yield_change_rebases_recipe_quantities(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "update").await;
    let token = generate_org_admin_token(user_id, org_id);

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
    let size = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_item_sizes (id, menu_item_id, label, price, sort) VALUES ($1,$2,'one_size',1000,0)").bind(size).bind(item).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO recipe_lines (owner_type, owner_id, ingredient_id, quantity, unit) VALUES ('item_size',$1,$2,200,'g')").bind(size).bind(ing).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO menu_item_recipes (menu_item_id, size_label, ingredient_name, ingredient_unit, quantity_used, org_ingredient_id) VALUES ($1,'one_size','Chicken','g',200,$2)").bind(item).bind(ing).execute(&pool).await.unwrap();

    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/inventory/orgs/{org_id}/catalog/{ing}"))
            .insert_header(auth!(token))
            .set_json(serde_json::json!({"yield_pct": 25}))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success(), "yield change must succeed");
    let qty: f64 =
        sqlx::query_scalar("SELECT quantity::float8 FROM recipe_lines WHERE ingredient_id=$1")
            .bind(ing)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(qty, 400.0);
    let lqty: f64 = sqlx::query_scalar(
        "SELECT quantity_used::float8 FROM menu_item_recipes WHERE org_ingredient_id=$1",
    )
    .bind(ing)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(lqty, 400.0);
}

// ──────────────────────────────────────────────────────────────
// Supplier link, settings, access
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_catalog_supplier_link(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    for a in ["create", "read", "update"] {
        grant_permission(&pool, "org_admin", "inventory", a).await;
    }
    let sup = Uuid::new_v4();
    sqlx::query("INSERT INTO suppliers (id, org_id, name) VALUES ($1, $2, 'Cairo Dairy')")
        .bind(sup)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    let token = generate_org_admin_token(user_id, org_id);

    let resp = test::call_service(&app, test::TestRequest::post().uri(&format!("/inventory/orgs/{org_id}/catalog")).insert_header(auth!(token))
        .set_json(serde_json::json!({"name": "Milk", "unit": "l", "cost_per_unit": null, "supplier_id": sup})).to_request()).await;
    assert_eq!(resp.status(), 201);
    let ing: OrgIngredient = test::read_body_json(resp).await;
    assert_eq!(ing.supplier_id, Some(sup));
    assert_eq!(ing.supplier_name.as_deref(), Some("Cairo Dairy"));

    let other_org = seed_org(&pool).await;
    let other_sup = Uuid::new_v4();
    sqlx::query("INSERT INTO suppliers (id, org_id, name) VALUES ($1, $2, 'Other')")
        .bind(other_sup)
        .bind(other_org)
        .execute(&pool)
        .await
        .unwrap();
    let resp = test::call_service(&app, test::TestRequest::post().uri(&format!("/inventory/orgs/{org_id}/catalog")).insert_header(auth!(token))
        .set_json(serde_json::json!({"name": "Sugar", "unit": "kg", "cost_per_unit": null, "supplier_id": other_sup})).to_request()).await;
    assert_eq!(resp.status(), 400);
}

#[sqlx::test]
async fn test_last_counted_at_from_finalized_stocktake(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "read").await;
    let ing = seed_ingredient(&pool, org_id, "Beans", "g").await;
    seed_stock(&pool, branch_id, ing, 100.0).await;
    let token = generate_org_admin_token(user_id, org_id);

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{branch_id}/stock"))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    let items: Vec<BranchStockRow> = test::read_body_json(resp).await;
    assert!(items[0].last_counted_at.is_none());

    sqlx::query("UPDATE branch_stock SET last_counted_at = now() WHERE branch_id=$1 AND org_ingredient_id=$2").bind(branch_id).bind(ing).execute(&pool).await.unwrap();
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/branches/{branch_id}/stock"))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    let items: Vec<BranchStockRow> = test::read_body_json(resp).await;
    assert!(items[0].last_counted_at.is_some());
}

#[sqlx::test]
async fn test_inventory_settings_get_put(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    for a in ["read", "update"] {
        grant_permission(&pool, "org_admin", "inventory", a).await;
    }
    let token = generate_org_admin_token(user_id, org_id);

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/orgs/{org_id}/settings"))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let s: OrgInventorySettings = test::read_body_json(resp).await;
    assert_eq!(s.stocktake_variance_threshold_pct, 10.0);

    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/inventory/orgs/{org_id}/settings"))
            .insert_header(auth!(token))
            .set_json(serde_json::json!({"stocktake_variance_threshold_pct": 15.0}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let s: OrgInventorySettings = test::read_body_json(resp).await;
    assert_eq!(s.stocktake_variance_threshold_pct, 15.0);

    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/inventory/orgs/{org_id}/settings"))
            .insert_header(auth!(token))
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
    let token = generate_branch_manager_token(user_id, org_id);
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/orgs/{org_id}/settings"))
            .insert_header(auth!(token))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 403);
}

/// D13: tellers are org-scoped — a teller token minted for branch A may read
/// another branch of the same org.
#[sqlx::test]
async fn test_teller_token_org_scoped_on_inventory(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;
    grant_permission(&pool, "teller", "inventory", "read").await;
    let teller = seed_user(&pool, org_id, "teller").await;
    assign_branch(&pool, teller, branch_a).await;
    assign_branch(&pool, teller, branch_b).await;
    let token = crate::auth::jwt::create_token(
        &get_secret(),
        teller,
        Some(org_id),
        UserRole::Teller,
        Some(branch_a),
        24,
    )
    .unwrap();

    for branch in [branch_a, branch_b] {
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&format!("/inventory/branches/{branch}/stock"))
                .insert_header(auth!(token))
                .to_request(),
        )
        .await;
        assert!(resp.status().is_success());
    }
}
