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
    Order, OrderFull, PaginatedOrders, OrderItemInput, PaymentSplitInput, CreateOrderRequest,
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
                bundle_components: vec![], unit_price: None,
                notes: None,
            }
        ],
        created_at: None, ..Default::default()
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

/// V31: order_ref is minted as <BRANCHCODE>-<YYMMDD>-<NNNN>, increments per
/// (branch, business-day), and round-trips through every decode path
/// (create RETURNING, the shared ORDER_SELECT read, and the void RETURNING).
#[sqlx::test]
async fn test_order_ref_generated_and_decoded(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await; // name "Test Branch" -> code "TESTBR"
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "create").await;
    grant_permission(&pool, "org_admin", "orders", "update").await;
    grant_permission(&pool, "org_admin", "orders", "read").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    let cat_id = seed_category(&pool, org_id).await;
    let menu_item_id = seed_menu_item(&pool, org_id, cat_id).await;

    let make_body = || CreateOrderRequest {
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
            bundle_components: vec![], unit_price: None,
            notes: None,
        }],
        created_at: None, ..Default::default()
    };

    let create = |body: CreateOrderRequest| {
        let app = &app;
        let token = token.clone();
        async move {
            let req = test::TestRequest::post()
                .uri("/orders")
                .insert_header(("Authorization", format!("Bearer {}", token)))
                .set_json(&body)
                .to_request();
            let resp = test::call_service(app, req).await;
            assert!(resp.status().is_success(), "create failed: {:?}", resp.status());
            test::read_body_json::<OrderFull, _>(resp).await
        }
    };

    // First order in the branch/day -> ...-0001 (create RETURNING decode path).
    let o1 = create(make_body()).await;
    let ref1 = o1.order.order_ref.clone().expect("order_ref present on create");
    let parts: Vec<&str> = ref1.split('-').collect();
    assert_eq!(parts.len(), 3, "order_ref should be CODE-YYMMDD-NNNN, got {ref1}");
    assert_eq!(parts[0], "TESTBR", "branch code prefix");
    assert_eq!(parts[1].len(), 6, "YYMMDD segment");
    assert!(parts[1].chars().all(|c| c.is_ascii_digit()), "date digits in {ref1}");
    assert_eq!(parts[2], "0001", "first order of the (branch, day)");

    // Second order increments the per-(branch, day) counter -> ...-0002.
    let o2 = create(make_body()).await;
    let ref2 = o2.order.order_ref.clone().expect("order_ref present");
    assert!(ref2.ends_with("-0002"), "second order should be -0002, got {ref2}");
    assert_ne!(ref1, ref2, "refs must be unique");

    // Read-back via GET /orders/{id} (shared ORDER_SELECT decode path).
    let req = test::TestRequest::get()
        .uri(&format!("/orders/{}", o1.order.id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "get failed: {:?}", resp.status());
    let fetched: OrderFull = test::read_body_json(resp).await;
    assert_eq!(fetched.order.order_ref.as_deref(), Some(ref1.as_str()), "ref stable on read");

    // Void (void RETURNING decode path) — ref preserved on the voided row.
    let void_req = VoidOrderRequest {
        reason: "customer_request".to_string(),
        note: None,
        voided_at: None,
        restore_inventory: Some(false),
    };
    let req = test::TestRequest::post()
        .uri(&format!("/orders/{}/void", o1.order.id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&void_req)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "void failed: {:?}", resp.status());
    let voided: Order = test::read_body_json(resp).await; // void returns a bare Order
    assert_eq!(voided.order_ref.as_deref(), Some(ref1.as_str()), "ref preserved on void");
    assert_eq!(voided.status, "voided");
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
                        unit_price: None,
                    }
                ],
                optional_field_ids: vec![],
                bundle_components: vec![], unit_price: None,
                notes: None,
            }
        ],
        created_at: None, ..Default::default()
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
            addons: vec![crate::orders::component_resolve::AddonInput { addon_item_id: almond_addon, quantity: 1, unit_price: None }],
            optional_field_ids: vec![],
            bundle_components: vec![], unit_price: None,
            notes: None,
        }],
        created_at: None, ..Default::default()
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
                    bundle_components: vec![], unit_price: None,
                    notes: None,
                }
            ],
            created_at: None, ..Default::default()
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
async fn test_list_orders_all_branches(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id   = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    // Second branch in the same org (seed_branch hard-codes one name).
    let branch_b = {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1,$2,'Branch B')")
            .bind(id).bind(org_id).execute(&pool).await.unwrap();
        id
    };
    let admin = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "read").await;
    grant_permission(&pool, "org_admin", "orders", "create").await;
    let token = generate_org_admin_token(admin, org_id);

    // Two OPEN shifts need two different tellers (one open shift per teller).
    let teller_b = seed_user(&pool, org_id, "teller").await;
    let shift_a  = seed_shift(&pool, branch_a, admin).await;
    let shift_b  = seed_shift(&pool, branch_b, teller_b).await;

    let cat  = seed_category(&pool, org_id).await;
    let item = seed_menu_item(&pool, org_id, cat).await;

    // One order in each branch (org-admin may post to any open shift).
    for (branch_id, shift_id) in [(branch_a, shift_a), (branch_b, shift_b)] {
        let body = CreateOrderRequest {
            branch_id, shift_id,
            payment_method: "cash".to_string(),
            customer_name: None, notes: None,
            discount_type: None, discount_value: None, discount_id: None,
            amount_tendered: None, tip_amount: None, tip_payment_method: None,
            payment_splits: None,
            items: vec![OrderItemInput {
                menu_item_id: Some(item), bundle_id: None, size_label: None,
                quantity: 1, addons: vec![], optional_field_ids: vec![],
                bundle_components: vec![], unit_price: None, notes: None,
            }],
            created_at: None, ..Default::default()
        };
        let resp = test::call_service(&app, test::TestRequest::post()
            .uri("/orders")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(&body).to_request()).await;
        assert!(resp.status().is_success(), "create order failed: {:?}", resp.status());
    }

    let auth = ("Authorization", format!("Bearer {token}"));

    // "All branches" via absent branch_id → both branches' orders, summed.
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri("/orders").insert_header(auth.clone()).to_request()).await;
    assert_eq!(resp.status(), 200);
    let all: PaginatedOrders = test::read_body_json(resp).await;
    assert_eq!(all.total, 2, "all-branches (absent branch_id) sees both branches");
    assert_eq!(all.summary.completed, 2, "summary aggregates across branches");

    // "All branches" via the nil-UUID sentinel → same.
    let nil = Uuid::nil();
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/orders?branch_id={nil}")).insert_header(auth.clone()).to_request()).await;
    assert_eq!(resp.status(), 200);
    let all_nil: PaginatedOrders = test::read_body_json(resp).await;
    assert_eq!(all_nil.total, 2, "all-branches (nil UUID) sees both branches");

    // A specific branch still scopes to that one branch only.
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/orders?branch_id={branch_a}")).insert_header(auth.clone()).to_request()).await;
    assert_eq!(resp.status(), 200);
    let just_a: PaginatedOrders = test::read_body_json(resp).await;
    assert_eq!(just_a.total, 1, "single branch sees only its own orders");
    assert_eq!(just_a.data[0].branch_id, branch_a);
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
                bundle_components: vec![], unit_price: None,
                notes: None,
            }
        ],
        created_at: None, ..Default::default()
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
                unit_price: None,
            }],
            optional_field_ids: vec![],
            bundle_components: vec![], unit_price: None,
            notes: None,
        }],
        created_at: None, ..Default::default()
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
            bundle_components: vec![], unit_price: None,
            notes: None,
        }],
        created_at: None, ..Default::default()
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

// ── Audit regression tests ───────────────────────────────────────────────

macro_rules! order_app {
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

/// One menu item (price 500), no recipe — minimal order request.
fn simple_order(branch_id: Uuid, shift_id: Uuid, menu_item_id: Uuid) -> CreateOrderRequest {
    CreateOrderRequest {
        branch_id, shift_id,
        payment_method: "cash".to_string(),
        customer_name: None, notes: None,
        discount_type: None, discount_value: None, discount_id: None,
        amount_tendered: None, tip_amount: None, tip_payment_method: None,
        payment_splits: None,
        items: vec![OrderItemInput {
            menu_item_id: Some(menu_item_id), bundle_id: None, size_label: None,
            quantity: 1, addons: vec![], optional_field_ids: vec![],
            bundle_components: vec![], unit_price: None, notes: None,
        }],
        created_at: None, ..Default::default()
    }
}

async fn seed_discount(pool: &PgPool, org_id: Uuid, dtype: &str, value: i32) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO discounts (id, org_id, name, type, value, is_active) VALUES ($1,$2,'D',$3::discount_type,$4,true)")
        .bind(id).bind(org_id).bind(dtype).bind(value).execute(pool).await.unwrap();
    id
}

/// V15: a percentage discount > 100 must clamp to subtotal (no negative total/tax).
#[sqlx::test]
async fn test_discount_percentage_over_100_is_clamped(pool: PgPool) {
    let app = order_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "create").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;
    let cat_id = seed_category(&pool, org_id).await;
    let menu_item_id = seed_menu_item(&pool, org_id, cat_id).await; // 500

    let mut req_body = simple_order(branch_id, shift_id, menu_item_id);
    req_body.discount_type = Some("percentage".to_string());
    req_body.discount_value = Some(150);

    let resp = test::call_service(&app, test::TestRequest::post().uri("/orders")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body).to_request()).await;
    assert!(resp.status().is_success());
    let o: OrderFull = test::read_body_json(resp).await;
    assert_eq!(o.order.discount_amount, 500, "150% discount clamps to subtotal");
    assert_eq!(o.order.tax_amount, 0, "tax must not go negative");
    assert_eq!(o.order.total_amount, 0, "total must not go negative");
}

/// V15: a negative discount_value must clamp to 0 (no inflated total).
#[sqlx::test]
async fn test_discount_negative_value_is_clamped(pool: PgPool) {
    let app = order_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "create").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;
    let cat_id = seed_category(&pool, org_id).await;
    let menu_item_id = seed_menu_item(&pool, org_id, cat_id).await; // 500

    let mut req_body = simple_order(branch_id, shift_id, menu_item_id);
    req_body.discount_type = Some("fixed".to_string());
    req_body.discount_value = Some(-100);

    let resp = test::call_service(&app, test::TestRequest::post().uri("/orders")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body).to_request()).await;
    assert!(resp.status().is_success());
    let o: OrderFull = test::read_body_json(resp).await;
    assert_eq!(o.order.discount_amount, 0, "negative discount clamps to 0");
    assert_eq!(o.order.total_amount, 570, "no inflation: 500 + 70 tax");
}

/// V2: an order may not reference a discount_id from a different org.
#[sqlx::test]
async fn test_discount_id_must_belong_to_caller_org(pool: PgPool) {
    let app = order_app!(pool);
    let org_a = seed_org(&pool).await;
    let org_b = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_a).await;
    let user_id = seed_user(&pool, org_a, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "create").await;
    let token = generate_org_admin_token(user_id, org_a);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;
    let cat_id = seed_category(&pool, org_a).await;
    let menu_item_id = seed_menu_item(&pool, org_a, cat_id).await;

    // A discount belonging to ORG B.
    let other_discount = seed_discount(&pool, org_b, "fixed", 100).await;

    let mut req_body = simple_order(branch_id, shift_id, menu_item_id);
    req_body.discount_id = Some(other_discount);

    let resp = test::call_service(&app, test::TestRequest::post().uri("/orders")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body).to_request()).await;
    assert_eq!(resp.status(), 400, "cross-org discount must be rejected");
}

/// V14: split-payment amounts must be positive.
#[sqlx::test]
async fn test_split_payment_rejects_nonpositive_amount(pool: PgPool) {
    let app = order_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "create").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;
    let cat_id = seed_category(&pool, org_id).await;
    let menu_item_id = seed_menu_item(&pool, org_id, cat_id).await;

    let mut req_body = simple_order(branch_id, shift_id, menu_item_id);
    req_body.payment_splits = Some(vec![
        PaymentSplitInput { method: "cash".to_string(), amount: 570, reference: None },
        PaymentSplitInput { method: "card".to_string(), amount: -10, reference: None },
    ]);

    let resp = test::call_service(&app, test::TestRequest::post().uri("/orders")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body).to_request()).await;
    assert_eq!(resp.status(), 400, "negative split amount must be rejected");
}

/// V6: voiding is idempotent — a second void does not double-restock inventory.
#[sqlx::test]
async fn test_void_is_idempotent_no_double_restock(pool: PgPool) {
    let app = order_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "create").await;
    grant_permission(&pool, "org_admin", "orders", "update").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;
    let cat_id = seed_category(&pool, org_id).await;
    let menu_item_id = seed_menu_item(&pool, org_id, cat_id).await;
    let ing_id = seed_ingredient(&pool, org_id, "Beans", "g").await;
    seed_branch_inventory(&pool, branch_id, ing_id, 1000.0).await;
    add_menu_item_recipe(&pool, menu_item_id, ing_id, 20.0).await;

    let resp = test::call_service(&app, test::TestRequest::post().uri("/orders")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&simple_order(branch_id, shift_id, menu_item_id)).to_request()).await;
    let created: OrderFull = test::read_body_json(resp).await;
    let order_id = created.order.id;

    let void = VoidOrderRequest { reason: "customer_request".into(), note: None, voided_at: None, restore_inventory: Some(true) };
    for _ in 0..2 {
        let resp = test::call_service(&app, test::TestRequest::post().uri(&format!("/orders/{}/void", order_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&void).to_request()).await;
        assert!(resp.status().is_success());
    }
    let stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE org_ingredient_id=$1")
        .bind(ing_id).fetch_one(&pool).await.unwrap();
    assert_eq!(stock, 1000.0, "double void must restore stock only once (1000, not 1020)");
}

/// V27: replaying with the same Idempotency-Key returns the SAME order.
#[sqlx::test]
async fn test_idempotency_key_replays_same_order(pool: PgPool) {
    let app = order_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "create").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;
    let cat_id = seed_category(&pool, org_id).await;
    let menu_item_id = seed_menu_item(&pool, org_id, cat_id).await;
    let key = Uuid::new_v4().to_string();
    let body = simple_order(branch_id, shift_id, menu_item_id);

    let mut ids = Vec::new();
    for _ in 0..2 {
        let resp = test::call_service(&app, test::TestRequest::post().uri("/orders")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .insert_header(("Idempotency-Key", key.clone()))
            .set_json(&body).to_request()).await;
        assert!(resp.status().is_success());
        let of: OrderFull = test::read_body_json(resp).await;
        ids.push(of.order.id);
    }
    assert_eq!(ids[0], ids[1], "same idempotency key must return the same order");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM orders WHERE shift_id=$1")
        .bind(shift_id).fetch_one(&pool).await.unwrap();
    assert_eq!(count, 1, "no duplicate order created");
}

/// V13: an order cannot attach to a shift that is no longer open.
#[sqlx::test]
async fn test_order_rejected_on_closed_shift(pool: PgPool) {
    let app = order_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "create").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;
    let cat_id = seed_category(&pool, org_id).await;
    let menu_item_id = seed_menu_item(&pool, org_id, cat_id).await;

    sqlx::query("UPDATE shifts SET status='closed', closed_at=now() WHERE id=$1").bind(shift_id).execute(&pool).await.unwrap();

    let resp = test::call_service(&app, test::TestRequest::post().uri("/orders")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&simple_order(branch_id, shift_id, menu_item_id)).to_request()).await;
    assert!(!resp.status().is_success(), "order on a closed shift must be rejected");
}

/// V31: voided orders' discounts/tips must not inflate the order summary.
#[sqlx::test]
async fn test_summary_excludes_voided_discounts(pool: PgPool) {
    let app = order_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "create").await;
    grant_permission(&pool, "org_admin", "orders", "update").await;
    grant_permission(&pool, "org_admin", "orders", "read").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;
    let cat_id = seed_category(&pool, org_id).await;
    let menu_item_id = seed_menu_item(&pool, org_id, cat_id).await;

    // Two orders each with a fixed 100 discount; one will be voided.
    let mut ids = Vec::new();
    for _ in 0..2 {
        let mut body = simple_order(branch_id, shift_id, menu_item_id);
        body.discount_type = Some("fixed".to_string());
        body.discount_value = Some(100);
        let resp = test::call_service(&app, test::TestRequest::post().uri("/orders")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&body).to_request()).await;
        let of: OrderFull = test::read_body_json(resp).await;
        ids.push(of.order.id);
    }
    // Void the second one.
    let void = VoidOrderRequest { reason: "customer_request".into(), note: None, voided_at: None, restore_inventory: Some(false) };
    test::call_service(&app, test::TestRequest::post().uri(&format!("/orders/{}/void", ids[1]))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&void).to_request()).await;

    let resp = test::call_service(&app, test::TestRequest::get().uri(&format!("/orders?branch_id={}", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token))).to_request()).await;
    let list: PaginatedOrders = test::read_body_json(resp).await;
    assert_eq!(list.summary.completed, 1);
    assert_eq!(list.summary.voided, 1);
    assert_eq!(list.summary.discounts, 100, "voided order's discount must be excluded from the summary");
}

/// V19: the bundle-COMPONENT swap path (resolve_menu_item_configuration) must
/// convert the recipe quantity into the replacement ingredient's base unit just
/// like the direct-item path — otherwise a g↔kg component swap mis-deducts 1000×.
#[sqlx::test]
async fn test_bundle_component_swap_converts_units(pool: PgPool) {
    let org_id = seed_org(&pool).await;
    let cat_id = seed_category(&pool, org_id).await;
    let menu_item_id = seed_menu_item(&pool, org_id, cat_id).await;

    // Milk in GRAMS, almond milk in KILOGRAMS — both category 'milk'.
    let milk = Uuid::new_v4();
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category) VALUES ($1,$2,'Milk','g'::inventory_unit,5,'milk')")
        .bind(milk).bind(org_id).execute(&pool).await.unwrap();
    let almond = Uuid::new_v4();
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category) VALUES ($1,$2,'Almond Milk','kg'::inventory_unit,8000,'milk')")
        .bind(almond).bind(org_id).execute(&pool).await.unwrap();

    add_menu_item_recipe(&pool, menu_item_id, milk, 250.0).await; // 250 g milk
    let almond_addon = seed_addon_item(&pool, org_id, "Almond Milk", "milk_type", 0).await;
    sqlx::query("INSERT INTO addon_item_ingredients (addon_item_id, org_ingredient_id, quantity_used, ingredient_name, ingredient_unit) VALUES ($1,$2,1,'Almond Milk','kg')")
        .bind(almond_addon).bind(almond).execute(&pool).await.unwrap();

    let config = crate::orders::component_resolve::resolve_menu_item_configuration(
        &pool, menu_item_id, None, 1,
        &[crate::orders::component_resolve::AddonInput { addon_item_id: almond_addon, quantity: 1, unit_price: None }],
        &[],
        Uuid::new_v4(), // branch with no overrides — pricing unaffected
    ).await.unwrap();

    let swap = config.deductions.iter().find(|d| d.org_ingredient_id == Some(almond))
        .expect("almond swap deduction must be present");
    assert_eq!(swap.unit, "kg");
    assert!((swap.quantity - 0.25).abs() < 1e-9, "250 g must convert to 0.25 kg, got {}", swap.quantity);
    // Milk was swapped out — no milk deduction remains.
    assert!(config.deductions.iter().all(|d| d.org_ingredient_id != Some(milk)));
}

/// V30: order_payments snapshot is_cash at sale time (cash → true, card → false),
/// so a later method rename / is_cash flip can't rewrite shift cash history.
#[sqlx::test]
async fn test_order_payment_snapshots_is_cash(pool: PgPool) {
    let app = order_app!(pool);
    let org_id = seed_org(&pool).await; // seeds cash (is_cash true) + card (is_cash false)
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "create").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;
    let cat_id = seed_category(&pool, org_id).await;
    let menu_item_id = seed_menu_item(&pool, org_id, cat_id).await;

    let mut body = simple_order(branch_id, shift_id, menu_item_id);
    body.payment_splits = Some(vec![
        PaymentSplitInput { method: "cash".into(), amount: 300, reference: None },
        PaymentSplitInput { method: "card".into(), amount: 270, reference: None },
    ]);
    let resp = test::call_service(&app, test::TestRequest::post().uri("/orders")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&body).to_request()).await;
    assert!(resp.status().is_success());
    let of: OrderFull = test::read_body_json(resp).await;

    let cash_is_cash: bool = sqlx::query_scalar("SELECT is_cash FROM order_payments WHERE order_id=$1 AND method='cash'")
        .bind(of.order.id).fetch_one(&pool).await.unwrap();
    let card_is_cash: bool = sqlx::query_scalar("SELECT is_cash FROM order_payments WHERE order_id=$1 AND method='card'")
        .bind(of.order.id).fetch_one(&pool).await.unwrap();
    assert!(cash_is_cash, "cash payment must snapshot is_cash=true");
    assert!(!card_is_cash, "card payment must snapshot is_cash=false");
}

/// Alignment: a percentage discount is ROUNDED (matching the POS preview), not
/// truncated — 10% of 2995 = 299.5 must round to 300, not 299.
#[sqlx::test]
async fn test_percentage_discount_is_rounded_not_truncated(pool: PgPool) {
    let app = order_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "create").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;
    let cat_id = seed_category(&pool, org_id).await;
    let item = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES ($1,$2,$3,'P',2995,true)")
        .bind(item).bind(org_id).bind(cat_id).execute(&pool).await.unwrap();

    let mut body = simple_order(branch_id, shift_id, item);
    body.discount_type = Some("percentage".to_string());
    body.discount_value = Some(10);
    let resp = test::call_service(&app, test::TestRequest::post().uri("/orders")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&body).to_request()).await;
    assert!(resp.status().is_success());
    let o: OrderFull = test::read_body_json(resp).await;
    assert_eq!(o.order.subtotal, 2995);
    assert_eq!(o.order.discount_amount, 300, "10% of 2995 = 299.5 must round to 300");
}

/// Pricing integrity: the POS's charged prices are recorded VERBATIM and deviations
/// from the catalog are flagged (never rejected). A branch override feeds both the
/// "expected" price used for flagging and the legacy (no-client-price) fallback.
#[sqlx::test]
async fn test_create_order_records_charged_prices_and_flags(pool: PgPool) {
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
    grant_permission(&pool, "org_admin", "orders", "create").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;
    let cat_id = seed_category(&pool, org_id).await;
    let item = seed_menu_item(&pool, org_id, cat_id).await; // base_price 500

    let post = |body: CreateOrderRequest, t: String| {
        let app = &app;
        async move {
            let resp = test::call_service(app, test::TestRequest::post()
                .uri("/orders")
                .insert_header(("Authorization", format!("Bearer {}", t)))
                .set_json(&body)
                .to_request()).await;
            assert!(resp.status().is_success(), "order create failed: {:?}", resp.status());
            test::read_body_json::<OrderFull, _>(resp).await
        }
    };

    // (1) POS charges 600 for a 500-catalog item → recorded verbatim and flagged.
    let of = post(CreateOrderRequest {
        branch_id, shift_id,
        payment_method: "cash".to_string(),
        items: vec![OrderItemInput {
            menu_item_id: Some(item),
            quantity: 1,
            unit_price: Some(600),
            ..Default::default()
        }],
        ..Default::default()
    }, token.clone()).await;
    assert_eq!(of.order.subtotal, 600, "recorded subtotal = what was charged");
    assert_eq!(of.items[0].item.unit_price, 600, "recorded line price = what was charged");

    let (flagged, expected_total): (bool, Option<i32>) = sqlx::query_as(
        "SELECT price_flagged, price_expected_total FROM orders WHERE id = $1",
    )
    .bind(of.order.id).fetch_one(&pool).await.unwrap();
    assert!(flagged, "a charged price above the catalog must flag the order");
    assert_eq!(expected_total, Some(570), "expected = 500 + 14% tax");
    let line_flagged: bool = sqlx::query_scalar(
        "SELECT price_flagged FROM order_items WHERE order_id = $1",
    )
    .bind(of.order.id).fetch_one(&pool).await.unwrap();
    assert!(line_flagged, "the deviating line must be flagged");

    // (2) Branch override sets the price to 700. POS sends NO price → the branch-effective
    // expected (700) is recorded (NOT the 500 catalog) and the order is not flagged.
    sqlx::query(
        "INSERT INTO branch_menu_overrides (branch_id, menu_item_id, price_override, is_available)
         VALUES ($1, $2, 700, true)",
    )
    .bind(branch_id).bind(item).execute(&pool).await.unwrap();

    let of2 = post(CreateOrderRequest {
        branch_id, shift_id,
        payment_method: "cash".to_string(),
        items: vec![OrderItemInput { menu_item_id: Some(item), quantity: 1, ..Default::default() }],
        ..Default::default()
    }, token.clone()).await;
    assert_eq!(of2.items[0].item.unit_price, 700, "branch override feeds the fallback price");
    assert_eq!(of2.order.subtotal, 700);
    let flagged2: bool = sqlx::query_scalar("SELECT price_flagged FROM orders WHERE id = $1")
        .bind(of2.order.id).fetch_one(&pool).await.unwrap();
    assert!(!flagged2, "charged == branch-effective expected → not flagged");
}

// ── Pricing integrity — intensive coverage ────────────────────

macro_rules! create_order_ok {
    ($app:expr, $tok:expr, $body:expr) => {{
        let resp = test::call_service(&$app, test::TestRequest::post()
            .uri("/orders")
            .insert_header(("Authorization", format!("Bearer {}", $tok)))
            .set_json(&$body)
            .to_request()).await;
        assert!(resp.status().is_success(), "order create failed: {:?}", resp.status());
        test::read_body_json::<OrderFull, _>(resp).await
    }};
}

async fn pricing_ctx(pool: &PgPool) -> (Uuid, Uuid, String, Uuid, Uuid) {
    let org = seed_org(pool).await;
    let branch = seed_branch(pool, org).await;
    let user = seed_user(pool, org, "org_admin").await;
    grant_permission(pool, "org_admin", "orders", "create").await;
    let token = generate_org_admin_token(user, org);
    let shift = seed_shift(pool, branch, user).await;
    let cat = seed_category(pool, org).await;
    (org, branch, token, shift, cat)
}

async fn pricing_app(pool: PgPool) -> impl actix_web::dev::Service<actix_http::Request, Response = actix_web::dev::ServiceResponse, Error = actix_web::Error> {
    test::init_service(
        App::new()
            .app_data(web::Data::new(pool))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await
}

async fn order_flagged(pool: &PgPool, id: Uuid) -> bool {
    sqlx::query_scalar("SELECT price_flagged FROM orders WHERE id = $1").bind(id).fetch_one(pool).await.unwrap()
}
async fn order_expected_total(pool: &PgPool, id: Uuid) -> Option<i32> {
    sqlx::query_scalar("SELECT price_expected_total FROM orders WHERE id = $1").bind(id).fetch_one(pool).await.unwrap()
}
async fn line_flagged(pool: &PgPool, order_id: Uuid) -> bool {
    sqlx::query_scalar("SELECT price_flagged FROM order_items WHERE order_id = $1 LIMIT 1").bind(order_id).fetch_one(pool).await.unwrap()
}
async fn set_branch_override(pool: &PgPool, branch: Uuid, item: Uuid, price: Option<i32>, available: bool) {
    sqlx::query(
        "INSERT INTO branch_menu_overrides (branch_id, menu_item_id, price_override, is_available)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (branch_id, menu_item_id)
         DO UPDATE SET price_override = EXCLUDED.price_override, is_available = EXCLUDED.is_available",
    )
    .bind(branch).bind(item).bind(price).bind(available).execute(pool).await.unwrap();
}
async fn add_size(pool: &PgPool, item: Uuid, label: &str, price: i32) {
    sqlx::query(
        "INSERT INTO item_sizes (id, menu_item_id, label, price_override, is_active)
         VALUES (gen_random_uuid(), $1, $2::item_size, $3, true)",
    )
    .bind(item).bind(label).bind(price).execute(pool).await.unwrap();
}
async fn seed_item_priced(pool: &PgPool, org: Uuid, cat: Uuid, name: &str, price: i32) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES ($1, $2, $3, $4, $5, true)")
        .bind(id).bind(org).bind(cat).bind(name).bind(price).execute(pool).await.unwrap();
    id
}

/// A charged price BELOW the catalog is recorded verbatim and flagged.
#[sqlx::test]
async fn test_create_order_charged_below_catalog_flags(pool: PgPool) {
    let app = pricing_app(pool.clone()).await;
    let (org, branch, token, shift, cat) = pricing_ctx(&pool).await;
    let item = seed_menu_item(&pool, org, cat).await; // 500

    let mut body = simple_order(branch, shift, item);
    body.items[0].unit_price = Some(400);
    let of = create_order_ok!(app, token, body);

    assert_eq!(of.order.subtotal, 400, "recorded = charged (below catalog)");
    assert_eq!(of.items[0].item.unit_price, 400);
    assert!(order_flagged(&pool, of.order.id).await, "any deviation flags the order");
    assert!(line_flagged(&pool, of.order.id).await);
    assert_eq!(order_expected_total(&pool, of.order.id).await, Some(570));
}

/// A plain order (no client prices, no override) behaves exactly as before and is not flagged.
#[sqlx::test]
async fn test_create_order_legacy_no_price_not_flagged(pool: PgPool) {
    let app = pricing_app(pool.clone()).await;
    let (org, branch, token, shift, cat) = pricing_ctx(&pool).await;
    let item = seed_menu_item(&pool, org, cat).await; // 500

    let of = create_order_ok!(app, token, simple_order(branch, shift, item));
    assert_eq!(of.order.subtotal, 500);
    assert_eq!(of.order.tax_amount, 70);
    assert_eq!(of.order.total_amount, 570);
    assert!(!order_flagged(&pool, of.order.id).await, "no deviation → not flagged");
    assert!(!line_flagged(&pool, of.order.id).await);
    assert_eq!(order_expected_total(&pool, of.order.id).await, Some(570));
}

/// The full money breakdown is recorded verbatim even when it diverges from a server recompute.
#[sqlx::test]
async fn test_create_order_full_breakdown_recorded_verbatim(pool: PgPool) {
    let app = pricing_app(pool.clone()).await;
    let (org, branch, token, shift, cat) = pricing_ctx(&pool).await;
    let item = seed_menu_item(&pool, org, cat).await; // 500

    let mut body = simple_order(branch, shift, item);
    body.items[0].unit_price = Some(500); // matches catalog → line itself not flagged
    body.amount_tendered = Some(1000);
    body.subtotal = Some(500);
    body.discount_amount = Some(50);
    body.tax_amount = Some(63);
    body.total_amount = Some(513);
    body.change_given = Some(480); // deliberately NOT 1000-513=487, to prove verbatim
    let of = create_order_ok!(app, token, body);

    assert_eq!(of.order.subtotal, 500);
    assert_eq!(of.order.discount_amount, 50);
    assert_eq!(of.order.tax_amount, 63);
    assert_eq!(of.order.total_amount, 513);
    assert_eq!(of.order.change_given, Some(480), "client change recorded verbatim, not recomputed");
    assert!(order_flagged(&pool, of.order.id).await, "recorded total != expected 570 → flagged");
    assert_eq!(order_expected_total(&pool, of.order.id).await, Some(570));
}

/// A charged ADDON price is recorded verbatim and flags the order.
#[sqlx::test]
async fn test_create_order_addon_charged_price_recorded_and_flags(pool: PgPool) {
    let app = pricing_app(pool.clone()).await;
    let (org, branch, token, shift, cat) = pricing_ctx(&pool).await;
    let item = seed_menu_item(&pool, org, cat).await; // 500
    let addon = seed_addon_item(&pool, org, "Extra Shot", "extra", 100).await;

    let mut body = simple_order(branch, shift, item);
    body.items[0].addons = vec![crate::orders::component_resolve::AddonInput {
        addon_item_id: addon, quantity: 1, unit_price: Some(150),
    }];
    let of = create_order_ok!(app, token, body);

    assert_eq!(of.items[0].addons[0].unit_price, 150, "charged addon price recorded");
    assert_eq!(of.order.subtotal, 650, "500 item + 150 charged addon");
    assert!(order_flagged(&pool, of.order.id).await, "addon deviation flags the order");
    assert_eq!(order_expected_total(&pool, of.order.id).await, Some(684), "expected 600 + 14% tax");
}

/// A branch override replaces the base price only — explicit size prices are untouched.
#[sqlx::test]
async fn test_create_order_branch_override_replaces_base_not_size(pool: PgPool) {
    let app = pricing_app(pool.clone()).await;
    let (org, branch, token, shift, cat) = pricing_ctx(&pool).await;
    let item = seed_menu_item(&pool, org, cat).await; // base 500
    add_size(&pool, item, "large", 800).await;
    set_branch_override(&pool, branch, item, Some(600), true).await;

    // Sized variant: the absolute size price (800) stands.
    let mut sized = simple_order(branch, shift, item);
    sized.items[0].size_label = Some("large".to_string());
    let of = create_order_ok!(app, token, sized);
    assert_eq!(of.items[0].item.unit_price, 800, "size price unchanged by a base override");
    assert!(!order_flagged(&pool, of.order.id).await);

    // Sizeless: the branch base override (600) applies, NOT the catalog 500.
    let of2 = create_order_ok!(app, token, simple_order(branch, shift, item));
    assert_eq!(of2.items[0].item.unit_price, 600, "branch base override applies to the sizeless line");
    assert!(!order_flagged(&pool, of2.order.id).await);
}

/// A branch-disabled item that an (offline/stale) POS still sells is flagged, never rejected.
#[sqlx::test]
async fn test_create_order_branch_disabled_item_flagged_not_rejected(pool: PgPool) {
    let app = pricing_app(pool.clone()).await;
    let (org, branch, token, shift, cat) = pricing_ctx(&pool).await;
    let item = seed_menu_item(&pool, org, cat).await; // 500
    set_branch_override(&pool, branch, item, None, false).await; // disabled at this branch

    let of = create_order_ok!(app, token, simple_order(branch, shift, item));
    assert_eq!(of.order.subtotal, 500, "still priced from the (inherited) catalog");
    assert!(order_flagged(&pool, of.order.id).await, "selling a branch-disabled item flags the order");
    assert!(line_flagged(&pool, of.order.id).await);
}

/// In a multi-line order, one deviating line flags the order but not the compliant line.
#[sqlx::test]
async fn test_create_order_multiline_only_deviating_line_flagged(pool: PgPool) {
    let app = pricing_app(pool.clone()).await;
    let (org, branch, token, shift, cat) = pricing_ctx(&pool).await;
    let item1 = seed_item_priced(&pool, org, cat, "Espresso", 500).await;
    let item2 = seed_item_priced(&pool, org, cat, "Mocha", 700).await;

    let mut body = simple_order(branch, shift, item1);
    body.items[0].unit_price = Some(600); // deviation on line 1
    body.items.push(OrderItemInput { menu_item_id: Some(item2), quantity: 1, ..Default::default() }); // line 2 compliant
    let of = create_order_ok!(app, token, body);

    assert_eq!(of.order.subtotal, 1300, "600 + 700");
    assert!(order_flagged(&pool, of.order.id).await, "one deviating line flags the whole order");

    let rows: Vec<(i32, bool)> = sqlx::query_as(
        "SELECT unit_price, price_flagged FROM order_items WHERE order_id = $1 ORDER BY unit_price",
    )
    .bind(of.order.id).fetch_all(&pool).await.unwrap();
    assert_eq!(rows, vec![(600, true), (700, false)], "only the deviating line is flagged");
}

/// A per-(branch, item, size) override is the price for that size — winning over the catalog
/// size price and the branch base override.
#[sqlx::test]
async fn test_create_order_branch_size_override_applied(pool: PgPool) {
    let app = pricing_app(pool.clone()).await;
    let (org, branch, token, shift, cat) = pricing_ctx(&pool).await;
    let item = seed_menu_item(&pool, org, cat).await; // base 500
    add_size(&pool, item, "large", 800).await;
    set_branch_override(&pool, branch, item, Some(600), true).await; // branch base 600
    sqlx::query(
        "INSERT INTO branch_menu_size_overrides (branch_id, menu_item_id, size_label, price_override)
         VALUES ($1, $2, 'large'::item_size, 950)",
    )
    .bind(branch).bind(item).execute(&pool).await.unwrap();

    // No client price → the branch size override (950) is used, not the catalog 800 or base 600.
    let mut sized = simple_order(branch, shift, item);
    sized.items[0].size_label = Some("large".to_string());
    let of = create_order_ok!(app, token, sized);
    assert_eq!(of.items[0].item.unit_price, 950, "branch size override is the size price");
    assert!(!order_flagged(&pool, of.order.id).await, "charged matches expected → not flagged");

    // Charging something else for that size is recorded verbatim and flagged.
    let mut sized = simple_order(branch, shift, item);
    sized.items[0].size_label = Some("large".to_string());
    sized.items[0].unit_price = Some(1000);
    let of2 = create_order_ok!(app, token, sized);
    assert_eq!(of2.items[0].item.unit_price, 1000);
    assert!(order_flagged(&pool, of2.order.id).await, "1000 != expected 950 → flagged");
}

/// A branch addon override feeds the expected addon price, so an order charging that
/// branch price is recorded at it and not flagged.
#[sqlx::test]
async fn test_create_order_branch_addon_override_applied(pool: PgPool) {
    let app = pricing_app(pool.clone()).await;
    let (org, branch, token, shift, cat) = pricing_ctx(&pool).await;
    let item = seed_menu_item(&pool, org, cat).await; // 500
    let addon = seed_addon_item(&pool, org, "Extra Shot", "extra", 100).await;
    // Branch reprices the addon to 150.
    sqlx::query(
        "INSERT INTO branch_addon_overrides (branch_id, addon_item_id, price_override, is_available)
         VALUES ($1, $2, 150, true)",
    )
    .bind(branch).bind(addon).execute(&pool).await.unwrap();

    let mut body = simple_order(branch, shift, item);
    body.items[0].addons = vec![crate::orders::component_resolve::AddonInput {
        addon_item_id: addon, quantity: 1, unit_price: None,
    }];
    let of = create_order_ok!(app, token, body);

    assert_eq!(of.items[0].addons[0].unit_price, 150, "branch addon override feeds the addon price");
    assert_eq!(of.order.subtotal, 650, "500 item + 150 branch addon");
    assert!(!order_flagged(&pool, of.order.id).await, "charged == branch-effective expected → not flagged");
}
