#![allow(unused_imports, unused_variables, dead_code)]
use actix_web::{test, App, web};
use sqlx::PgPool;
use uuid::Uuid;
use chrono::Utc;
use rust_decimal::Decimal;
use serde_json::json;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::orders::routes;
use crate::orders::handlers::{
    OrderFull, PaginatedOrders, OrderItemInput, PaymentSplitInput, CreateOrderRequest,
    VoidOrderRequest, PreviewRecipeRequest, PreviewAddonInput, ExportResponse
};

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn generate_token(user_id: Uuid, org_id: Option<Uuid>, role: UserRole) -> String {
    crate::auth::jwt::create_token(&get_secret(), user_id, org_id, role, None, 24).unwrap()
}

fn generate_org_admin_token(user_id: Uuid, org_id: Uuid) -> String {
    generate_token(user_id, Some(org_id), UserRole::OrgAdmin)
}

fn generate_teller_token(user_id: Uuid, org_id: Uuid, branch_id: Uuid) -> String {
    crate::auth::jwt::create_token(&get_secret(), user_id, Some(org_id), UserRole::Teller, Some(branch_id), 24).unwrap()
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

    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) VALUES
        ($1, 'cash', '{}', 'emerald', 'payments_outlined', true, true),
        ($1, 'card', '{}', 'blue', 'credit_card_rounded', false, true)"
    )
    .bind(org_id)
    .execute(pool)
    .await
    .unwrap();

    org_id
}

async fn seed_branch(pool: &PgPool, org_id: Uuid) -> Uuid {
    let branch_id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Test Branch')")
        .bind(branch_id)
        .bind(org_id)
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

async fn seed_shift(pool: &PgPool, branch_id: Uuid, user_id: Uuid) -> Uuid {
    let shift_id = Uuid::new_v4();
    sqlx::query("INSERT INTO shifts (id, branch_id, teller_id, status, opening_cash) VALUES ($1, $2, $3, 'open', 10000)")
        .bind(shift_id)
        .bind(branch_id)
        .bind(user_id)
        .execute(pool)
        .await
        .unwrap();
    shift_id
}

async fn assign_user_to_branch(pool: &PgPool, user_id: Uuid, branch_id: Uuid) {
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(user_id)
        .bind(branch_id)
        .execute(pool)
        .await
        .unwrap();
}

async fn seed_category(pool: &PgPool, org_id: Uuid) -> Uuid {
    let cat_id = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'Cat')")
        .bind(cat_id)
        .bind(org_id)
        .execute(pool)
        .await
        .unwrap();
    cat_id
}

async fn seed_menu_item(pool: &PgPool, org_id: Uuid, cat_id: Uuid) -> Uuid {
    let item_id = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES ($1, $2, $3, 'Coffee', 500, true)")
        .bind(item_id)
        .bind(org_id)
        .bind(cat_id)
        .execute(pool)
        .await
        .unwrap();
    item_id
}

async fn seed_ingredient(pool: &PgPool, org_id: Uuid, name: &str, unit: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category) VALUES ($1, $2, $3, $4::inventory_unit, 100, 'general')")
        .bind(id)
        .bind(org_id)
        .bind(name)
        .bind(unit)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_branch_inventory(pool: &PgPool, branch_id: Uuid, ing_id: Uuid, stock: f64) {
    sqlx::query("INSERT INTO branch_inventory (branch_id, org_ingredient_id, current_stock) VALUES ($1, $2, $3)")
        .bind(branch_id)
        .bind(ing_id)
        .bind(stock)
        .execute(pool)
        .await
        .unwrap();
}

async fn add_menu_item_recipe(pool: &PgPool, menu_item_id: Uuid, ing_id: Uuid, qty: f64) {
    sqlx::query("INSERT INTO menu_item_recipes (menu_item_id, org_ingredient_id, quantity_used, size_label, ingredient_name, ingredient_unit) VALUES ($1, $2, $3, 'one_size', 'Test Ing', 'g')")
        .bind(menu_item_id)
        .bind(ing_id)
        .bind(qty)
        .execute(pool)
        .await
        .unwrap();
}

async fn seed_addon_item(pool: &PgPool, org_id: Uuid, name: &str, ptype: &str, price: i32) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO addon_items (id, org_id, name, type, default_price) VALUES ($1, $2, $3, $4, $5)")
        .bind(id)
        .bind(org_id)
        .bind(name)
        .bind(ptype)
        .bind(price)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn add_addon_ingredient(pool: &PgPool, addon_item_id: Uuid, ing_id: Uuid, qty: f64) {
    sqlx::query("INSERT INTO addon_item_ingredients (addon_item_id, org_ingredient_id, quantity_used, ingredient_name, ingredient_unit) VALUES ($1, $2, $3, 'Test Ing', 'ml')")
        .bind(addon_item_id)
        .bind(ing_id)
        .bind(qty)
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test]
async fn test_create_order_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "teller").await;
    assign_user_to_branch(&pool, user_id, branch_id).await;
    grant_permission(&pool, "teller", "orders", "create").await;
    let token = generate_teller_token(user_id, org_id, branch_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    let cat_id = seed_category(&pool, org_id).await;
    let menu_item_id = seed_menu_item(&pool, org_id, cat_id).await;
    
    let ing_id = seed_ingredient(&pool, org_id, "Coffee Beans", "g").await;
    seed_branch_inventory(&pool, branch_id, ing_id, 1000.0).await;
    add_menu_item_recipe(&pool, menu_item_id, ing_id, 20.0).await;

    let req_body = CreateOrderRequest {
        branch_id,
        shift_id,
        payment_method: "cash".to_string(),
        customer_name: Some("John Doe".to_string()),
        notes: None,
        discount_type: None,
        discount_value: None,
        discount_id: None,
        amount_tendered: Some(600),
        tip_amount: None,
        tip_payment_method: None,
        payment_splits: None,
        items: vec![
            OrderItemInput {
                menu_item_id: Some(menu_item_id),
                bundle_id: None,
                size_label: None,
                quantity: 1,
                addons: vec![],
                optional_field_ids: vec![],
                bundle_components: vec![],
                notes: None,
            }
        ],
        created_at: None,
    };

    let req = test::TestRequest::post()
        .uri("/orders")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "Expected success, got {:?}", resp.status());

    let order_full: OrderFull = test::read_body_json(resp).await;
    assert_eq!(order_full.order.subtotal, 500); // Coffee base price
    assert_eq!(order_full.order.tax_amount, 70);
    assert_eq!(order_full.order.status, "completed");
    
    // Verify inventory deduction
    let new_stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE org_ingredient_id = $1")
        .bind(ing_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(new_stock, 980.0); // 1000 - 20
}

#[sqlx::test]
async fn test_create_order_with_addons_and_discount(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "create").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    let cat_id = seed_category(&pool, org_id).await;
    let menu_item_id = seed_menu_item(&pool, org_id, cat_id).await; // 500
    
    let addon_id = seed_addon_item(&pool, org_id, "Extra Shot", "extra", 100).await;
    let ing_id = seed_ingredient(&pool, org_id, "Espresso", "ml").await;
    seed_branch_inventory(&pool, branch_id, ing_id, 1000.0).await;
    add_addon_ingredient(&pool, addon_id, ing_id, 30.0).await;

    let req_body = CreateOrderRequest {
        branch_id,
        shift_id,
        payment_method: "card".to_string(),
        customer_name: None,
        notes: None,
        discount_type: Some("fixed".to_string()),
        discount_value: Some(50),
        discount_id: None,
        amount_tendered: None,
        tip_amount: None,
        tip_payment_method: None,
        payment_splits: None,
        items: vec![
            OrderItemInput {
                menu_item_id: Some(menu_item_id),
                bundle_id: None,
                size_label: None,
                quantity: 2, // 2 items = 1000
                addons: vec![
                    crate::orders::component_resolve::AddonInput {
                        addon_item_id: addon_id,
                        quantity: 1, // 1 per item = 2 addons total = 200
                    }
                ],
                optional_field_ids: vec![],
                bundle_components: vec![],
                notes: None,
            }
        ],
        created_at: None,
    };

    let req = test::TestRequest::post()
        .uri("/orders")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    let status = resp.status(); if !status.is_success() { panic!("Status {:?}", status); }

    let order_full: OrderFull = test::read_body_json(resp).await;
    assert_eq!(order_full.order.subtotal, 1200); // (500 + 100) * 2
    assert_eq!(order_full.order.discount_amount, 50);
    
    // Verify inventory deduction
    let new_stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE org_ingredient_id = $1")
        .bind(ing_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(new_stock, 940.0); // 1000 - (30 * 2)
}

#[sqlx::test]
async fn test_milk_swap_converts_units_across_base_units(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "create").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    let cat_id = seed_category(&pool, org_id).await;
    let menu_item_id = seed_menu_item(&pool, org_id, cat_id).await;

    // Milk is stocked in GRAMS, almond milk in KILOGRAMS — both category 'milk'.
    let milk = Uuid::new_v4();
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category) VALUES ($1,$2,'Milk','g'::inventory_unit,5,'milk')")
        .bind(milk).bind(org_id).execute(&pool).await.unwrap();
    let almond = Uuid::new_v4();
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category) VALUES ($1,$2,'Almond Milk','kg'::inventory_unit,8000,'milk')")
        .bind(almond).bind(org_id).execute(&pool).await.unwrap();
    seed_branch_inventory(&pool, branch_id, milk, 5000.0).await;  // 5000 g
    seed_branch_inventory(&pool, branch_id, almond, 10.0).await;  // 10 kg

    // Recipe uses 250 g of milk (stored in milk's base unit).
    add_menu_item_recipe(&pool, menu_item_id, milk, 250.0).await; // ingredient_unit 'g'

    // A milk_type addon that swaps in almond milk (its ingredient is in kg).
    let almond_addon = seed_addon_item(&pool, org_id, "Almond Milk", "milk_type", 0).await;
    sqlx::query("INSERT INTO addon_item_ingredients (addon_item_id, org_ingredient_id, quantity_used, ingredient_name, ingredient_unit) VALUES ($1,$2,1,'Almond Milk','kg')")
        .bind(almond_addon).bind(almond).execute(&pool).await.unwrap();

    let req_body = CreateOrderRequest {
        branch_id,
        shift_id,
        payment_method: "cash".to_string(),
        customer_name: None, notes: None,
        discount_type: None, discount_value: None, discount_id: None,
        amount_tendered: None, tip_amount: None, tip_payment_method: None, payment_splits: None,
        items: vec![OrderItemInput {
            menu_item_id: Some(menu_item_id),
            bundle_id: None,
            size_label: None,
            quantity: 1,
            addons: vec![crate::orders::component_resolve::AddonInput { addon_item_id: almond_addon, quantity: 1 }],
            optional_field_ids: vec![],
            bundle_components: vec![],
            notes: None,
        }],
        created_at: None,
    };
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri("/orders").insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(&req_body).to_request()).await;
    assert_eq!(resp.status(), 201);

    // The 250 g the recipe called for is converted to the almond-milk base unit:
    // 0.25 kg deducted — NOT 250 (which would be a 1000× over-deduction).
    let almond_stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE org_ingredient_id=$1").bind(almond).fetch_one(&pool).await.unwrap();
    assert_eq!(almond_stock, 9.75);
    // Milk was swapped out → its stock is untouched.
    let milk_stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE org_ingredient_id=$1").bind(milk).fetch_one(&pool).await.unwrap();
    assert_eq!(milk_stock, 5000.0);
}

#[sqlx::test]
async fn test_list_orders(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "read").await;
    grant_permission(&pool, "org_admin", "orders", "create").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    let cat_id = seed_category(&pool, org_id).await;
    let menu_item_id = seed_menu_item(&pool, org_id, cat_id).await;

    // Create 2 orders
    for _ in 0..2 {
        let req_body = CreateOrderRequest {
            branch_id,
            shift_id,
            payment_method: "cash".to_string(),
            customer_name: None,
            notes: None,
            discount_type: None,
            discount_value: None,
            discount_id: None,
            amount_tendered: None,
            tip_amount: None,
            tip_payment_method: None,
            payment_splits: None,
            items: vec![
                OrderItemInput {
                    menu_item_id: Some(menu_item_id),
                    bundle_id: None,
                    size_label: None,
                    quantity: 1,
                    addons: vec![],
                    optional_field_ids: vec![],
                    bundle_components: vec![],
                    notes: None,
                }
            ],
            created_at: None,
        };

        let req = test::TestRequest::post()
            .uri("/orders")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&req_body)
            .to_request();

        let resp = test::call_service(&app, req).await;
        let status = resp.status(); if !status.is_success() { panic!("Status {:?}", status); }
    }

    let req = test::TestRequest::get()
        .uri(&format!("/orders?branch_id={}", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    let status = resp.status(); if !status.is_success() { panic!("Status {:?}", status); }

    let list: PaginatedOrders = test::read_body_json(resp).await;
    assert_eq!(list.data.len(), 2);
    assert_eq!(list.total, 2);
    assert_eq!(list.summary.completed, 2);
}

#[sqlx::test]
async fn test_void_order(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "create").await;
    grant_permission(&pool, "org_admin", "orders", "update").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    let cat_id = seed_category(&pool, org_id).await;
    let menu_item_id = seed_menu_item(&pool, org_id, cat_id).await;

    let ing_id = seed_ingredient(&pool, org_id, "Coffee Beans", "g").await;
    seed_branch_inventory(&pool, branch_id, ing_id, 1000.0).await;
    add_menu_item_recipe(&pool, menu_item_id, ing_id, 20.0).await;

    let req_body = CreateOrderRequest {
        branch_id,
        shift_id,
        payment_method: "cash".to_string(),
        customer_name: None,
        notes: None,
        discount_type: None,
        discount_value: None,
        discount_id: None,
        amount_tendered: None,
        tip_amount: None,
        tip_payment_method: None,
        payment_splits: None,
        items: vec![
            OrderItemInput {
                menu_item_id: Some(menu_item_id),
                bundle_id: None,
                size_label: None,
                quantity: 1,
                addons: vec![],
                optional_field_ids: vec![],
                bundle_components: vec![],
                notes: None,
            }
        ],
        created_at: None,
    };

    let req = test::TestRequest::post()
        .uri("/orders")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    let order_full: OrderFull = test::read_body_json(resp).await;
    let order_id = order_full.order.id;

    // Void the order
    let void_req = VoidOrderRequest {
        reason: "customer_request".to_string(),
        note: None,
        voided_at: None,
        restore_inventory: Some(true),
    };

    let req = test::TestRequest::post()
        .uri(&format!("/orders/{}/void", order_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&void_req)
        .to_request();

    let resp = test::call_service(&app, req).await;
    let status = resp.status(); if !status.is_success() { panic!("Status {:?}", status); }

    // Verify inventory restored
    let new_stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE org_ingredient_id = $1")
        .bind(ing_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(new_stock, 1000.0); // Restored 20
}

#[sqlx::test]
async fn test_preview_recipe(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "create").await;
    let token = generate_org_admin_token(user_id, org_id);

    let cat_id = seed_category(&pool, org_id).await;
    let menu_item_id = seed_menu_item(&pool, org_id, cat_id).await;

    let req = test::TestRequest::post()
        .uri("/orders/preview-recipe")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&PreviewRecipeRequest {
            menu_item_id,
            size_label: None,
            addons: vec![],
            optional_field_ids: vec![],
        })
        .to_request();

    let resp = test::call_service(&app, req).await;
    let status = resp.status(); if !status.is_success() { panic!("Status {:?}", status); }
}

// ═══════════════════════════════════════════════════════════════════
// Cost engine — sale-time snapshots
// ═══════════════════════════════════════════════════════════════════

#[sqlx::test]
async fn test_order_cost_snapshot_with_recipe_and_addon(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "teller").await;
    assign_user_to_branch(&pool, user_id, branch_id).await;
    grant_permission(&pool, "teller", "orders", "create").await;
    let token = generate_teller_token(user_id, org_id, branch_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    let cat_id = seed_category(&pool, org_id).await;
    let menu_item_id = seed_menu_item(&pool, org_id, cat_id).await;

    // 20 g coffee @ 100 piastres/g → recipe cost 2 000 piastres
    let coffee = seed_ingredient(&pool, org_id, "Coffee Beans", "g").await;
    seed_branch_inventory(&pool, branch_id, coffee, 1000.0).await;
    add_menu_item_recipe(&pool, menu_item_id, coffee, 20.0).await;

    // Additive addon: 5 ml syrup @ 100 piastres/ml → 500 piastres
    let syrup = seed_ingredient(&pool, org_id, "Syrup", "ml").await;
    seed_branch_inventory(&pool, branch_id, syrup, 1000.0).await;
    let addon_id = seed_addon_item(&pool, org_id, "Vanilla Syrup", "extra", 100).await;
    add_addon_ingredient(&pool, addon_id, syrup, 5.0).await;

    let req_body = CreateOrderRequest {
        branch_id,
        shift_id,
        payment_method: "cash".to_string(),
        customer_name: None,
        notes: None,
        discount_type: None,
        discount_value: None,
        discount_id: None,
        amount_tendered: None,
        tip_amount: None,
        tip_payment_method: None,
        payment_splits: None,
        items: vec![OrderItemInput {
            menu_item_id: Some(menu_item_id),
            bundle_id: None,
            size_label: None,
            quantity: 2,
            addons: vec![crate::orders::component_resolve::AddonInput {
                addon_item_id: addon_id,
                quantity: 1,
            }],
            optional_field_ids: vec![],
            bundle_components: vec![],
            notes: None,
        }],
        created_at: None,
    };

    let req = test::TestRequest::post()
        .uri("/orders")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "got {:?}", resp.status());

    let order_full: OrderFull = test::read_body_json(resp).await;
    let item = &order_full.items[0].item;

    // Recipe scope per unit: 20 g × 100 piastres = 2 000 piastres / unit.
    assert_eq!(item.unit_cost, Some(2_000));
    // Full line: recipe 2 units (4 000) + addon 5 ml × 2 units (1 000).
    assert_eq!(item.line_cost, Some(5_000));
    assert!(!item.cost_missing);

    // Addon line cost: 5 ml × 100 piastres × qty 1 × item qty 2 = 1 000.
    let addon_row = &order_full.items[0].addons[0];
    assert_eq!(addon_row.line_cost, Some(1_000));

    // Snapshot entries carry per-entry costs for audit.
    let entries = item.deductions_snapshot.as_array().unwrap();
    assert!(entries.iter().all(|e| e.get("cost_per_unit").is_some()));
    assert!(entries.iter().all(|e| e.get("line_cost").is_some()));
}

#[sqlx::test]
async fn test_order_cost_missing_without_recipe(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "teller").await;
    assign_user_to_branch(&pool, user_id, branch_id).await;
    grant_permission(&pool, "teller", "orders", "create").await;
    let token = generate_teller_token(user_id, org_id, branch_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    let cat_id = seed_category(&pool, org_id).await;
    // No recipe rows at all → cost unknown, never zero.
    let menu_item_id = seed_menu_item(&pool, org_id, cat_id).await;

    let req_body = CreateOrderRequest {
        branch_id,
        shift_id,
        payment_method: "cash".to_string(),
        customer_name: None,
        notes: None,
        discount_type: None,
        discount_value: None,
        discount_id: None,
        amount_tendered: None,
        tip_amount: None,
        tip_payment_method: None,
        payment_splits: None,
        items: vec![OrderItemInput {
            menu_item_id: Some(menu_item_id),
            bundle_id: None,
            size_label: None,
            quantity: 1,
            addons: vec![],
            optional_field_ids: vec![],
            bundle_components: vec![],
            notes: None,
        }],
        created_at: None,
    };

    let req = test::TestRequest::post()
        .uri("/orders")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let order_full: OrderFull = test::read_body_json(resp).await;
    let item = &order_full.items[0].item;
    assert_eq!(item.line_cost, None);
    assert_eq!(item.unit_cost, None);
    assert!(item.cost_missing);
}
