use actix_web::{test, App, web};
use sqlx::PgPool;
use uuid::Uuid;
use rust_decimal::Decimal;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::bundles::routes;
use crate::bundles::handlers::*;

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

async fn seed_category(pool: &PgPool, org_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'Test Cat')")
        .bind(id)
        .bind(org_id)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_ingredient(pool: &PgPool, org_id: Uuid, cost_per_unit: Decimal) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit) VALUES ($1, $2, 'Ing', 'g'::inventory_unit, $3)")
        .bind(id)
        .bind(org_id)
        .bind(cost_per_unit)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_menu_item(pool: &PgPool, org_id: Uuid, cat_id: Uuid, price: i32) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES ($1, $2, $3, 'Item', $4, true)")
        .bind(id)
        .bind(org_id)
        .bind(cat_id)
        .bind(price)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn link_recipe(pool: &PgPool, item_id: Uuid, ing_id: Uuid, qty: f64) {
    sqlx::query("INSERT INTO menu_item_recipes (menu_item_id, org_ingredient_id, ingredient_name, ingredient_unit, quantity_used, size_label) VALUES ($1, $2, 'Ing', 'g'::inventory_unit, $3, 'one_size'::item_size)")
        .bind(item_id)
        .bind(ing_id)
        .bind(qty)
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test]
async fn test_bundles_crud(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;
    
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "create").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    grant_permission(&pool, "org_admin", "menu_items", "delete").await;
    let token = generate_org_admin_token(user_id, org_id);

    let cat_id = seed_category(&pool, org_id).await;
    let item1 = seed_menu_item(&pool, org_id, cat_id, 500).await;
    let item2 = seed_menu_item(&pool, org_id, cat_id, 600).await;

    // Create Draft Bundle
    let create_req = CreateBundleRequest {
        org_id,
        name: "Combo Meal".into(),
        name_translations: None,
        description: Some("Tasty".into()),
        description_translations: None,
        price: 900,
        image_url: None,
        available_from_time: None,
        available_until_time: None,
        available_from_date: None,
        available_until_date: None,
        branch_ids: None,
        components: vec![
            CreateBundleComponentInput { item_id: item1, quantity: 1, position: Some(0) },
            CreateBundleComponentInput { item_id: item2, quantity: 2, position: Some(1) },
        ],
    };

    let req1 = test::TestRequest::post().uri("/bundles")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&create_req).to_request();
    let resp1 = test::call_service(&app, req1).await;
    assert!(resp1.status().is_success());
    let bundle: BundleWithComponents = test::read_body_json(resp1).await;
    assert_eq!(bundle.bundle.name, "Combo Meal");
    assert_eq!(bundle.bundle.status, BundleStatus::Draft);
    assert_eq!(bundle.components.len(), 2);

    // Read Bundle
    let req2 = test::TestRequest::get().uri(&format!("/bundles/{}", bundle.bundle.id))
        .insert_header(("Authorization", format!("Bearer {}", token))).to_request();
    let resp2 = test::call_service(&app, req2).await;
    assert!(resp2.status().is_success());

    // List Bundles
    let req3 = test::TestRequest::get().uri(&format!("/bundles?org_id={}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token))).to_request();
    let resp3 = test::call_service(&app, req3).await;
    assert!(resp3.status().is_success());
    let list: PaginatedBundles = test::read_body_json(resp3).await;
    assert_eq!(list.total, 1);

    // Update Bundle
    let req4 = test::TestRequest::patch().uri(&format!("/bundles/{}", bundle.bundle.id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&UpdateBundleRequest {
            name: Some("Super Combo".into()),
            name_translations: None,
            description: None,
            description_translations: None,
            price: Some(1000),
            image_url: None,
            available_from_time: None,
            available_until_time: None,
            available_from_date: None,
            available_until_date: None,
            components: None,
            branch_ids: None,
        }).to_request();
    let resp4 = test::call_service(&app, req4).await;
    assert!(resp4.status().is_success());

    // Delete Bundle
    let req5 = test::TestRequest::delete().uri(&format!("/bundles/{}", bundle.bundle.id))
        .insert_header(("Authorization", format!("Bearer {}", token))).to_request();
    let resp5 = test::call_service(&app, req5).await;
    assert!(resp5.status().is_success());
}

#[sqlx::test]
async fn test_bundle_activation_and_rules(pool: PgPool) {
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
    grant_permission(&pool, "org_admin", "menu_items", "read").await;
    let token = generate_org_admin_token(user_id, org_id);

    let cat_id = seed_category(&pool, org_id).await;
    
    // Ingredient cost: 100 piastres per unit
    let ing1 = seed_ingredient(&pool, org_id, Decimal::from(100)).await;

    // Items priced at 500 piastres each, cost 100 piastres each (1 unit of ingredient)
    let item1 = seed_menu_item(&pool, org_id, cat_id, 500).await;
    let item2 = seed_menu_item(&pool, org_id, cat_id, 500).await;
    
    link_recipe(&pool, item1, ing1, 1.0).await;
    link_recipe(&pool, item2, ing1, 1.0).await;

    // Sum List Prices = 1000
    // Sum Costs = 200
    // Max Price (3% off list) = 970
    // Min Price (20% over cost) = 240

    // Create Draft
    let req1 = test::TestRequest::post().uri("/bundles")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&CreateBundleRequest {
            org_id,
            name: "Good Combo".into(),
            name_translations: None,
            description: None,
            description_translations: None,
            price: 800, // Valid (240 <= 800 <= 970)
            image_url: None,
            available_from_time: None,
            available_until_time: None,
            available_from_date: None,
            available_until_date: None,
            branch_ids: None,
            components: vec![
                CreateBundleComponentInput { item_id: item1, quantity: 1, position: None },
                CreateBundleComponentInput { item_id: item2, quantity: 1, position: None },
            ],
        }).to_request();
    let resp1 = test::call_service(&app, req1).await;
    let bundle: BundleWithComponents = test::read_body_json(resp1).await;

    // Activate should succeed
    let req2 = test::TestRequest::post().uri(&format!("/bundles/{}/activate", bundle.bundle.id))
        .insert_header(("Authorization", format!("Bearer {}", token))).to_request();
    let resp2 = test::call_service(&app, req2).await;
    assert!(resp2.status().is_success());

    // --- Validation failure checks ---

    // Create a bad bundle (Too cheap, violates margin floor)
    let req3 = test::TestRequest::post().uri("/bundles")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&CreateBundleRequest {
            org_id,
            name: "Too Cheap".into(),
            name_translations: None,
            description: None,
            description_translations: None,
            price: 200, // Invalid (200 < 240)
            image_url: None,
            available_from_time: None,
            available_until_time: None,
            available_from_date: None,
            available_until_date: None,
            branch_ids: None,
            components: vec![
                CreateBundleComponentInput { item_id: item1, quantity: 1, position: None },
                CreateBundleComponentInput { item_id: item2, quantity: 1, position: None },
            ],
        }).to_request();
    let resp3 = test::call_service(&app, req3).await;
    let cheap_bundle: BundleWithComponents = test::read_body_json(resp3).await;

    let req4 = test::TestRequest::post().uri(&format!("/bundles/{}/activate", cheap_bundle.bundle.id))
        .insert_header(("Authorization", format!("Bearer {}", token))).to_request();
    let resp4 = test::call_service(&app, req4).await;
    assert_eq!(resp4.status().as_u16(), 400);

    // Create a bad bundle (Too expensive, violates discount perceivability)
    let req5 = test::TestRequest::post().uri("/bundles")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&CreateBundleRequest {
            org_id,
            name: "Too Expensive".into(),
            name_translations: None,
            description: None,
            description_translations: None,
            price: 980, // Invalid (980 > 970)
            image_url: None,
            available_from_time: None,
            available_until_time: None,
            available_from_date: None,
            available_until_date: None,
            branch_ids: None,
            components: vec![
                CreateBundleComponentInput { item_id: item1, quantity: 1, position: None },
                CreateBundleComponentInput { item_id: item2, quantity: 1, position: None },
            ],
        }).to_request();
    let resp5 = test::call_service(&app, req5).await;
    let exp_bundle: BundleWithComponents = test::read_body_json(resp5).await;

    let req6 = test::TestRequest::post().uri(&format!("/bundles/{}/activate", exp_bundle.bundle.id))
        .insert_header(("Authorization", format!("Bearer {}", token))).to_request();
    let resp6 = test::call_service(&app, req6).await;
    assert_eq!(resp6.status().as_u16(), 400);
}

/// V24: a bundle whose component has an UNKNOWN ingredient cost (a recipe line
/// with no linked ingredient) must NOT be activatable — otherwise the
/// undercounted cost lets an under-priced bundle slip past the margin floor.
#[sqlx::test]
async fn test_bundle_activation_blocked_on_unknown_component_cost(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "create").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let token = generate_org_admin_token(user_id, org_id);
    let cat_id = seed_category(&pool, org_id).await;
    let ing = seed_ingredient(&pool, org_id, Decimal::from(100)).await;
    let item1 = seed_menu_item(&pool, org_id, cat_id, 500).await;
    let item2 = seed_menu_item(&pool, org_id, cat_id, 500).await;

    // item1: one linked recipe line + one UNLINKED line → true cost UNKNOWN.
    link_recipe(&pool, item1, ing, 1.0).await;
    sqlx::query("INSERT INTO menu_item_recipes (menu_item_id, org_ingredient_id, ingredient_name, ingredient_unit, quantity_used, size_label) VALUES ($1, NULL, 'Spice', 'g', 5.0, 'one_size'::item_size)")
        .bind(item1).execute(&pool).await.unwrap();
    // item2: fully linked → cost known.
    link_recipe(&pool, item2, ing, 1.0).await;

    // Draft creation succeeds (no cost check at create).
    let resp = test::call_service(&app, test::TestRequest::post().uri("/bundles")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&CreateBundleRequest {
            org_id, name: "Combo".into(), name_translations: None,
            description: None, description_translations: None,
            price: 800, image_url: None,
            available_from_time: None, available_until_time: None,
            available_from_date: None, available_until_date: None,
            branch_ids: None,
            components: vec![
                CreateBundleComponentInput { item_id: item1, quantity: 1, position: None },
                CreateBundleComponentInput { item_id: item2, quantity: 1, position: None },
            ],
        }).to_request()).await;
    assert!(resp.status().is_success(), "draft create should succeed");
    let bundle: BundleWithComponents = test::read_body_json(resp).await;

    // Activation is BLOCKED because item1's cost is unknown.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/bundles/{}/activate", bundle.bundle.id))
        .insert_header(("Authorization", format!("Bearer {}", token))).to_request()).await;
    assert_eq!(resp.status(), 400, "activation must be blocked on unknown component cost");
}

/// V25: bundle_performance net_profit must use the SALE-TIME COGS snapshot
/// (order_items.line_cost), not current ingredient costs.
#[sqlx::test]
async fn test_bundle_performance_uses_snapshot_cost(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;
    let token = generate_org_admin_token(user_id, org_id);
    let ing = seed_ingredient(&pool, org_id, Decimal::from(100)).await;

    // A bundle, a branch/shift, and a completed order with one bundle line whose
    // sale-time COGS snapshot (line_cost) is 100 and revenue (line_total) is 300.
    let bundle_id = Uuid::new_v4();
    sqlx::query("INSERT INTO bundles (id, org_id, name, price, status) VALUES ($1,$2,'B',300,'active'::bundle_status)")
        .bind(bundle_id).bind(org_id).execute(&pool).await.unwrap();
    let branch_id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1,$2,'Br')").bind(branch_id).bind(org_id).execute(&pool).await.unwrap();
    let shift_id = Uuid::new_v4();
    sqlx::query("INSERT INTO shifts (id, branch_id, teller_id, status, opening_cash) VALUES ($1,$2,$3,'open',0)")
        .bind(shift_id).bind(branch_id).bind(user_id).execute(&pool).await.unwrap();
    let order_id = Uuid::new_v4();
    sqlx::query("INSERT INTO orders (id, branch_id, teller_id, shift_id, idempotency_key, subtotal, tax_amount, total_amount, status, order_number, payment_method, order_ref) VALUES ($1,$2,$3,$4, gen_random_uuid(), 300, 0, 300, 'completed', 1, 'cash', gen_random_uuid()::text)")
        .bind(order_id).bind(branch_id).bind(user_id).bind(shift_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO order_items (order_id, bundle_id, item_name, unit_price, quantity, line_total, line_cost, cost_missing) VALUES ($1,$2,'B',300,1,300,100,false)")
        .bind(order_id).bind(bundle_id).execute(&pool).await.unwrap();

    // The ingredient's CURRENT cost changes — the report must IGNORE it.
    sqlx::query("UPDATE org_ingredients SET cost_per_unit = 500 WHERE id = $1").bind(ing).execute(&pool).await.unwrap();

    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/bundles/{}/performance", bundle_id))
        .insert_header(("Authorization", format!("Bearer {}", token))).to_request()).await;
    assert!(resp.status().is_success());
    let perf: BundlePerformanceResponse = test::read_body_json(resp).await;
    assert_eq!(perf.gross_revenue, 300);
    assert_eq!(perf.net_profit, 200, "net_profit must be 300 - sale-time 100, not affected by current cost");
}
