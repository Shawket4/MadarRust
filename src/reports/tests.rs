use actix_web::{test, App, web};
use sqlx::PgPool;
use uuid::Uuid;
use chrono::Utc;
use rust_decimal::Decimal;
use serde_json::json;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::reports::routes;
use crate::reports::handlers::{
    ShiftSummary, InventoryDiscrepancy, DeductionLogRow, CategorySales, ItemSales, BranchSalesReport, 
    StockRow, BranchStockReport, TimeseriesPoint, TellerStats, AddonSalesRow, BranchComparison, OrgComparisonReport,
    BundleSalesRow, CombinedItemSalesRow
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
    sqlx::query("INSERT INTO categories (id, org_id, name, display_order) VALUES ($1, $2, 'Cat', 0)")
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

async fn seed_branch_inventory(pool: &PgPool, branch_id: Uuid, ing_id: Uuid, stock: f64) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branch_inventory (id, branch_id, org_ingredient_id, current_stock) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(branch_id)
        .bind(ing_id)
        .bind(stock)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_order(pool: &PgPool, branch_id: Uuid, teller_id: Uuid, shift_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO orders (id, branch_id, teller_id, shift_id, idempotency_key, customer_name, subtotal, discount_amount, tax_amount, total_amount, status, order_number, payment_method)
         VALUES ($1, $2, $3, $4, gen_random_uuid(), 'Customer', 500, 0, 70, 570, 'completed', 1, 'cash')"
    )
    .bind(id)
    .bind(branch_id)
    .bind(teller_id)
    .bind(shift_id)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query("INSERT INTO order_payments (order_id, method, amount) VALUES ($1, 'cash', 570)")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();

    id
}

#[sqlx::test]
async fn test_shift_summary(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;
    
    grant_permission(&pool, "org_admin", "shifts", "read").await;
    grant_permission(&pool, "org_admin", "orders", "read").await;

    // Seed an order
    seed_order(&pool, branch_id, user_id, shift_id).await;

    let req = test::TestRequest::get()
        .uri(&format!("/reports/shifts/{}/summary", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "Failed to get shift summary");

    let summary: ShiftSummary = test::read_body_json(resp).await;
    assert_eq!(summary.shift_id, shift_id);
    assert_eq!(summary.total_orders, 1);
    assert_eq!(summary.total_revenue, 570);
    assert_eq!(summary.cash_revenue, 570);
}

#[sqlx::test]
async fn test_branch_sales(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    grant_permission(&pool, "org_admin", "orders", "read").await;

    let order_id = seed_order(&pool, branch_id, user_id, shift_id).await;
    let cat_id = seed_category(&pool, org_id).await;
    let item_id = seed_menu_item(&pool, org_id, cat_id).await;
    sqlx::query("INSERT INTO order_items (order_id, menu_item_id, item_name, quantity, unit_price, line_total) VALUES ($1, $2, 'Coffee', 1, 500, 500)")
        .bind(order_id).bind(item_id).execute(&pool).await.unwrap();

    let req = test::TestRequest::get()
        .uri(&format!("/reports/branches/{}/sales", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "Failed to get branch sales");

    let sales: BranchSalesReport = test::read_body_json(resp).await;
    assert_eq!(sales.total_orders, 1);
    assert_eq!(sales.total_revenue, 570);
    assert_eq!(sales.top_items.len(), 1);
    assert_eq!(sales.by_category.len(), 1);
}

#[sqlx::test]
async fn test_branch_stock(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);

    grant_permission(&pool, "org_admin", "inventory", "read").await;

    let ing_id = seed_ingredient(&pool, org_id, "Milk", "ml").await;
    seed_branch_inventory(&pool, branch_id, ing_id, 50.0).await;

    let req = test::TestRequest::get()
        .uri(&format!("/reports/branches/{}/stock", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "Failed to get branch stock");

    let stock: BranchStockReport = test::read_body_json(resp).await;
    assert_eq!(stock.items.len(), 1);
    assert_eq!(stock.items[0].current_stock, 50.0);
}

#[sqlx::test]
async fn test_shift_inventory_discrepancies(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    grant_permission(&pool, "org_admin", "shift_counts", "read").await;

    let ing_id = seed_ingredient(&pool, org_id, "Milk", "ml").await;
    let branch_inv_id = seed_branch_inventory(&pool, branch_id, ing_id, 50.0).await;

    sqlx::query("INSERT INTO shift_inventory_counts (shift_id, branch_inventory_id, expected_stock, actual_stock, counted_by) VALUES ($1, $2, 50.0, 48.0, $3)")
        .bind(shift_id).bind(branch_inv_id).bind(user_id).execute(&pool).await.unwrap();

    let req = test::TestRequest::get()
        .uri(&format!("/reports/shifts/{}/inventory", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let discrepancies: Vec<InventoryDiscrepancy> = test::read_body_json(resp).await;
    assert_eq!(discrepancies.len(), 1);
    assert_eq!(discrepancies[0].expected_stock, 50.0);
    assert_eq!(discrepancies[0].actual_count, Some(48.0));
}

#[sqlx::test]
async fn test_branch_sales_timeseries(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    grant_permission(&pool, "org_admin", "orders", "read").await;

    seed_order(&pool, branch_id, user_id, shift_id).await;

    let req = test::TestRequest::get()
        .uri(&format!("/reports/branches/{}/sales/timeseries?granularity=daily", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let ts: Vec<TimeseriesPoint> = test::read_body_json(resp).await;
    assert_eq!(ts.len(), 1);
    assert_eq!(ts[0].orders, 1);
    assert_eq!(ts[0].revenue, 570);
}

#[sqlx::test]
async fn test_branch_teller_stats(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    grant_permission(&pool, "org_admin", "orders", "read").await;

    seed_order(&pool, branch_id, user_id, shift_id).await;

    let req = test::TestRequest::get()
        .uri(&format!("/reports/branches/{}/tellers", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let stats: Vec<TellerStats> = test::read_body_json(resp).await;
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].orders, 1);
    assert_eq!(stats[0].revenue, 570);
}

#[sqlx::test]
async fn test_org_branch_comparison(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    grant_permission(&pool, "org_admin", "orders", "read").await;

    seed_order(&pool, branch_id, user_id, shift_id).await;

    let req = test::TestRequest::get()
        .uri(&format!("/reports/orgs/{}/comparison", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let comparison: OrgComparisonReport = test::read_body_json(resp).await;
    assert_eq!(comparison.branches.len(), 1);
    assert_eq!(comparison.branches[0].total_orders, 1);
}

#[sqlx::test]
async fn test_shift_deductions(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    grant_permission(&pool, "org_admin", "inventory", "read").await;

    let req = test::TestRequest::get()
        .uri(&format!("/reports/shifts/{}/deductions", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let deductions: Vec<DeductionLogRow> = test::read_body_json(resp).await;
    assert_eq!(deductions.len(), 0);
}

#[sqlx::test]
async fn test_branch_addon_sales(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    grant_permission(&pool, "org_admin", "orders", "read").await;

    let order_id = seed_order(&pool, branch_id, user_id, shift_id).await;

    let addon_id = Uuid::new_v4();
    sqlx::query("INSERT INTO addon_items (id, org_id, name, type, default_price, display_order) VALUES ($1, $2, 'Extra Cheese', 'ingredient', 50, 0)")
        .bind(addon_id).bind(org_id).execute(&pool).await.unwrap();

    let category_id = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'Burgers')")
        .bind(category_id).bind(org_id).execute(&pool).await.unwrap();

    let recipe_id = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price) VALUES ($1, $2, $3, 'Burger', 500)")
        .bind(recipe_id).bind(org_id).bind(category_id).execute(&pool).await.unwrap();

    let order_item_id = Uuid::new_v4();
    sqlx::query("INSERT INTO order_items (id, order_id, menu_item_id, item_name, quantity, unit_price, line_total) VALUES ($1, $2, $3, 'Burger', 1, 500, 500)")
        .bind(order_item_id).bind(order_id).bind(recipe_id).execute(&pool).await.unwrap();

    sqlx::query("INSERT INTO order_item_addons (order_item_id, addon_item_id, addon_name, quantity, unit_price, line_total) VALUES ($1, $2, 'Extra Cheese', 1, 50, 50)")
        .bind(order_item_id).bind(addon_id).execute(&pool).await.unwrap();

    let req = test::TestRequest::get()
        .uri(&format!("/reports/branches/{}/addons", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let addons: Vec<AddonSalesRow> = test::read_body_json(resp).await;
    assert_eq!(addons.len(), 1);
    assert_eq!(addons[0].addon_name, "Extra Cheese");
    assert_eq!(addons[0].quantity_sold, 1);
    assert_eq!(addons[0].revenue, 50);
}

#[sqlx::test]
async fn test_branch_bundle_sales(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    grant_permission(&pool, "org_admin", "orders", "read").await;

    let order_id = seed_order(&pool, branch_id, user_id, shift_id).await;

    let bundle_id = Uuid::new_v4();
    sqlx::query("INSERT INTO bundles (id, org_id, name, description, price) VALUES ($1, $2, 'Lunch Deal', 'x', 400)")
        .bind(bundle_id).bind(org_id).execute(&pool).await.unwrap();

    sqlx::query("INSERT INTO order_items (id, order_id, bundle_id, item_name, quantity, unit_price, line_total) VALUES ($1, $2, $3, 'Lunch Deal', 1, 400, 400)")
        .bind(Uuid::new_v4()).bind(order_id).bind(bundle_id).execute(&pool).await.unwrap();

    let req = test::TestRequest::get()
        .uri(&format!("/reports/branches/{}/bundles", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let bundles: Vec<BundleSalesRow> = test::read_body_json(resp).await;
    assert_eq!(bundles.len(), 1);
    assert_eq!(bundles[0].bundle_name, "Lunch Deal");
    assert_eq!(bundles[0].quantity_sold, 1);
    assert_eq!(bundles[0].revenue, 400);
}

#[sqlx::test]
async fn test_branch_combined_item_sales(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    grant_permission(&pool, "org_admin", "orders", "read").await;

    let order_id = seed_order(&pool, branch_id, user_id, shift_id).await;

    let category_id = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'Burgers')")
        .bind(category_id).bind(org_id).execute(&pool).await.unwrap();

    let recipe_id = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price) VALUES ($1, $2, $3, 'Burger', 500)")
        .bind(recipe_id).bind(org_id).bind(category_id).execute(&pool).await.unwrap();
    
    sqlx::query("INSERT INTO menu_item_price_epochs (id, menu_item_id, price, effective_from) VALUES ($1, $2, 500, now())")
        .bind(Uuid::new_v4()).bind(recipe_id).execute(&pool).await.unwrap();

    let bundle_id = Uuid::new_v4();
    sqlx::query("INSERT INTO bundles (id, org_id, name, description, price) VALUES ($1, $2, 'Lunch Deal', 'x', 400)")
        .bind(bundle_id).bind(org_id).execute(&pool).await.unwrap();

    sqlx::query("INSERT INTO bundle_price_epochs (id, bundle_id, price, effective_from) VALUES ($1, $2, 400, now())")
        .bind(Uuid::new_v4()).bind(bundle_id).execute(&pool).await.unwrap();

    sqlx::query("INSERT INTO order_items (id, order_id, menu_item_id, item_name, quantity, unit_price, line_total) VALUES ($1, $2, $3, 'Burger', 2, 500, 1000)")
        .bind(Uuid::new_v4()).bind(order_id).bind(recipe_id).execute(&pool).await.unwrap();

    let order_item_bundle_id = Uuid::new_v4();
    sqlx::query("INSERT INTO order_items (id, order_id, bundle_id, item_name, quantity, unit_price, line_total) VALUES ($1, $2, $3, 'Lunch Deal', 1, 400, 400)")
        .bind(order_item_bundle_id).bind(order_id).bind(bundle_id).execute(&pool).await.unwrap();

    sqlx::query("INSERT INTO order_line_bundle_components (order_line_id, item_id, quantity) VALUES ($1, $2, 1)")
        .bind(order_item_bundle_id).bind(recipe_id).execute(&pool).await.unwrap();

    let req = test::TestRequest::get()
        .uri(&format!("/reports/branches/{}/items-combined", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let combined: Vec<CombinedItemSalesRow> = test::read_body_json(resp).await;
    assert_eq!(combined.len(), 1);
    
    let recipe_sale = combined.iter().find(|c| c.item_name == "Burger").unwrap();
    assert_eq!(recipe_sale.standalone_qty, 2);
    assert_eq!(recipe_sale.bundle_qty, 1);
    assert_eq!(recipe_sale.total_qty, 3);
}

