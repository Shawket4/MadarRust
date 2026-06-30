#![allow(unused_imports, unused_variables, dead_code)]
use actix_web::{App, test, web};
use chrono::Utc;
use rust_decimal::Decimal;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::reports::handlers::{
    AddonSalesRow, BranchComparison, BranchSalesReport, BranchStockReport, BundleSalesRow,
    CategorySales, CombinedItemSalesRow, ConsumptionRow, DeductionLogRow, InventoryValuationReport,
    ItemSales, LowStockRow, OrgComparisonReport, PeakHourPoint, ShiftSummary, ShrinkageRow,
    StockRow, TellerStats, TimeseriesPoint, WasteReportRow,
};
use crate::reports::routes;

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
    crate::auth::jwt::create_token(
        &get_secret(),
        user_id,
        Some(org_id),
        UserRole::Teller,
        Some(branch_id),
        24,
    )
    .unwrap()
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
        "INSERT INTO orders (id, branch_id, teller_id, shift_id, idempotency_key, customer_name, subtotal, discount_amount, tax_amount, total_amount, status, order_number, payment_method, order_ref)
         VALUES ($1, $2, $3, $4, gen_random_uuid(), 'Customer', 500, 0, 70, 570, 'completed', 1, 'cash', gen_random_uuid()::text)"
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
            .configure(|cfg| routes::configure(cfg, web::Data::new(pool.clone()))),
    )
    .await;

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
    assert_eq!(summary.revenue_by_method["cash"], json!(570));
}

#[sqlx::test]
async fn test_branch_sales(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(|cfg| routes::configure(cfg, web::Data::new(pool.clone()))),
    )
    .await;

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
            .configure(|cfg| routes::configure(cfg, web::Data::new(pool.clone()))),
    )
    .await;

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
async fn test_branch_sales_timeseries(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(|cfg| routes::configure(cfg, web::Data::new(pool.clone()))),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    grant_permission(&pool, "org_admin", "orders", "read").await;

    seed_order(&pool, branch_id, user_id, shift_id).await;

    let req = test::TestRequest::get()
        .uri(&format!(
            "/reports/branches/{}/sales/timeseries?granularity=daily",
            branch_id
        ))
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
async fn test_branch_sales_peak_hours(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(|cfg| routes::configure(cfg, web::Data::new(pool.clone()))),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    grant_permission(&pool, "org_admin", "orders", "read").await;

    seed_order(&pool, branch_id, user_id, shift_id).await;

    let req = test::TestRequest::get()
        .uri(&format!("/reports/branches/{}/sales/peak-hours", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let rows: Vec<PeakHourPoint> = test::read_body_json(resp).await;

    // Always returns exactly 24 rows (one per hour of day), even if some are empty.
    assert_eq!(rows.len(), 24, "peak hours must return exactly 24 buckets");

    // Hours are 0–23 in order.
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row.hour, i as i32, "hour at index {i} must equal {i}");
    }

    // The seeded order (revenue=570) must appear in exactly one bucket.
    let nonempty: Vec<&PeakHourPoint> = rows.iter().filter(|r| r.orders > 0).collect();
    assert_eq!(
        nonempty.len(),
        1,
        "exactly one hour bucket should have orders"
    );
    let hot = nonempty[0];
    assert_eq!(hot.orders, 1);
    assert_eq!(hot.revenue, 570);

    // Per-day averages: 1 order over 1 distinct day → avg equals total.
    assert_eq!(
        hot.avg_revenue_per_day, 570,
        "avg_revenue_per_day = total when days=1"
    );
    assert!(
        (hot.avg_orders_per_day - 1.0).abs() < 0.001,
        "avg_orders_per_day should be 1.0"
    );

    // Percentages: sole active bucket gets 100% of both revenue and orders.
    assert!(
        (hot.revenue_pct - 100.0).abs() < 0.1,
        "revenue_pct should be 100.0"
    );
    assert!(
        (hot.orders_pct - 100.0).abs() < 0.1,
        "orders_pct should be 100.0"
    );

    // All empty-hour buckets should have zero averages and zero percentages.
    let empty_nonzero_avg = rows
        .iter()
        .filter(|r| r.orders == 0 && r.avg_revenue_per_day != 0)
        .count();
    assert_eq!(
        empty_nonzero_avg, 0,
        "empty hour buckets must not carry non-zero averages"
    );

    // Voided orders must not count towards revenue.
    let total_voided: i64 = rows.iter().map(|r| r.voided).sum();
    assert_eq!(total_voided, 0, "no voided orders were seeded");
}

#[sqlx::test]
async fn test_branch_teller_stats(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(|cfg| routes::configure(cfg, web::Data::new(pool.clone()))),
    )
    .await;

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
            .configure(|cfg| routes::configure(cfg, web::Data::new(pool.clone()))),
    )
    .await;

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
            .configure(|cfg| routes::configure(cfg, web::Data::new(pool.clone()))),
    )
    .await;

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
            .configure(|cfg| routes::configure(cfg, web::Data::new(pool.clone()))),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    grant_permission(&pool, "org_admin", "orders", "read").await;

    let order_id = seed_order(&pool, branch_id, user_id, shift_id).await;

    let addon_id = Uuid::new_v4();
    sqlx::query("INSERT INTO addon_items (id, org_id, name, type, default_price) VALUES ($1, $2, 'Extra Cheese', 'ingredient', 50)")
        .bind(addon_id).bind(org_id).execute(&pool).await.unwrap();

    let category_id = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'Burgers')")
        .bind(category_id)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();

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
            .configure(|cfg| routes::configure(cfg, web::Data::new(pool.clone()))),
    )
    .await;

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
            .configure(|cfg| routes::configure(cfg, web::Data::new(pool.clone()))),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    grant_permission(&pool, "org_admin", "orders", "read").await;

    let order_id = seed_order(&pool, branch_id, user_id, shift_id).await;

    let category_id = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'Burgers')")
        .bind(category_id)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();

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

#[sqlx::test]
async fn test_branch_menu_engineering_cost_basis(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(|cfg| routes::configure(cfg, web::Data::new(pool.clone()))),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let admin = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "read").await;
    let token = generate_org_admin_token(admin, org_id);

    let cat_id = seed_category(&pool, org_id).await;
    // Costed item: recipe = 1 unit of an ingredient @ 100 piastres (helper value).
    let coffee = seed_menu_item(&pool, org_id, cat_id).await;
    let beans = seed_ingredient(&pool, org_id, "Beans", "g").await;
    sqlx::query(
        "INSERT INTO menu_item_recipes (menu_item_id, size_label, quantity_used, ingredient_name, ingredient_unit, org_ingredient_id) \
         VALUES ($1, 'one_size', 1.0, 'Beans', 'g', $2)",
    )
    .bind(coffee)
    .bind(beans)
    .execute(&pool)
    .await
    .unwrap();
    // Recipe-less item: must be excluded under BOTH bases.
    let bare = seed_menu_item(&pool, org_id, cat_id).await;

    // Item whose recipe ingredient has NO entered cost (NULL): its sale-time
    // snapshots are fine, but TODAY its cost is unknown — included under
    // snapshot, excluded under current. (Regression: NULL used to be a
    // DEFAULT 0 that read as "genuinely free" and leaked into the report.)
    let ghost = seed_menu_item(&pool, org_id, cat_id).await;
    let costless_ing: Uuid = sqlx::query_scalar(
        "INSERT INTO org_ingredients (org_id, name, unit, cost_per_unit, category) \
         VALUES ($1, 'Mystery Dust', 'g'::inventory_unit, NULL, 'general') RETURNING id",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO menu_item_recipes (menu_item_id, size_label, quantity_used, ingredient_name, ingredient_unit, org_ingredient_id) \
         VALUES ($1, 'one_size', 1.0, 'Mystery Dust', 'g', $2)",
    )
    .bind(ghost)
    .bind(costless_ing)
    .execute(&pool)
    .await
    .unwrap();

    let teller = seed_user(&pool, org_id, "teller").await;
    let shift = seed_shift(&pool, branch_id, teller).await;
    let order = seed_order(&pool, branch_id, teller, shift).await;
    // Coffee line: qty 2 @ 1000, sale-time snapshot cost 100/unit → line_cost 200.
    sqlx::query(
        "INSERT INTO order_items (order_id, menu_item_id, item_name, unit_price, quantity, line_total, line_cost, unit_cost, cost_missing) \
         VALUES ($1, $2, 'Coffee', 1000, 2, 2000, 200, 100, false)",
    )
    .bind(order)
    .bind(coffee)
    .execute(&pool)
    .await
    .unwrap();
    // Bare line: cost unresolvable at sale time.
    sqlx::query(
        "INSERT INTO order_items (order_id, menu_item_id, item_name, unit_price, quantity, line_total, line_cost, unit_cost, cost_missing) \
         VALUES ($1, $2, 'Bare', 500, 1, 500, NULL, NULL, true)",
    )
    .bind(order)
    .bind(bare)
    .execute(&pool)
    .await
    .unwrap();
    // Ghost line: qty 1 with a KNOWN sale-time snapshot cost of 150.
    sqlx::query(
        "INSERT INTO order_items (order_id, menu_item_id, item_name, unit_price, quantity, line_total, line_cost, unit_cost, cost_missing) \
         VALUES ($1, $2, 'Ghost', 1000, 1, 1000, 150, 150, false)",
    )
    .bind(order)
    .bind(ghost)
    .execute(&pool)
    .await
    .unwrap();

    // Ingredient cost rises 100 → 400 piastres AFTER the sale (catalog +
    // open history epoch, mirroring the inventory PATCH path).
    sqlx::query("UPDATE org_ingredients SET cost_per_unit = 400 WHERE id = $1")
        .bind(beans)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO ingredient_cost_history (org_ingredient_id, cost_per_unit, effective_from) \
         VALUES ($1, 400, now())",
    )
    .bind(beans)
    .execute(&pool)
    .await
    .unwrap();

    let fetch = |basis: Option<&'static str>| {
        let app = &app;
        let token = token.clone();
        let uri = match basis {
            Some(b) => format!("/reports/branches/{branch_id}/menu-engineering?cost_basis={b}"),
            None => format!("/reports/branches/{branch_id}/menu-engineering"),
        };
        async move {
            let req = test::TestRequest::get()
                .uri(&uri)
                .insert_header(("Authorization", format!("Bearer {token}")))
                .to_request();
            let resp = test::call_service(app, req).await;
            let status = resp.status();
            let body: serde_json::Value = test::read_body_json(resp).await;
            (status, body)
        }
    };
    let coffee_row = |report: &serde_json::Value| -> serde_json::Value {
        report["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["menu_item_id"] == serde_json::json!(coffee.to_string()))
            .unwrap()
            .clone()
    };
    let has_item = |report: &serde_json::Value, id: Uuid| {
        report["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["menu_item_id"] == serde_json::json!(id.to_string()))
    };

    // Default (and explicit snapshot): sale-time costs — the cost edit is
    // invisible, and ghost (snapshot-costed) is included.
    for basis in [None, Some("snapshot")] {
        let (status, report) = fetch(basis).await;
        assert_eq!(status, 200);
        assert_eq!(report["cost_basis"], "snapshot");
        assert_eq!(report["rows"].as_array().unwrap().len(), 2);
        assert!(
            has_item(&report, ghost),
            "snapshot-costed ghost must be included"
        );
        assert_eq!(report["rows_cost_missing"], 1); // bare only
        assert_eq!(report["excluded_sales"], 500); // bare's revenue
        assert!(
            !has_item(&report, bare),
            "cost-missing row leaked into the report"
        );
        let row = coffee_row(&report);
        assert_eq!(row["total_cost"], 200);
        assert_eq!(row["total_profit"], 1800);
        assert!(row["class"].is_string());
        // Popularity over INCLUDED rows: coffee 2 of 3 units (bare excluded).
        assert!((row["popularity_pct"].as_f64().unwrap() - 2.0 / 3.0).abs() < 1e-9);
    }

    // Current: today's rollup (400 × qty 2) reclassifies immediately, and
    // ghost drops out — its ingredient cost was never entered (NULL ≠ free).
    let (status, report) = fetch(Some("current")).await;
    assert_eq!(status, 200);
    assert_eq!(report["cost_basis"], "current");
    assert_eq!(report["rows"].as_array().unwrap().len(), 1);
    assert!(
        !has_item(&report, ghost),
        "unentered-cost item leaked under current basis"
    );
    assert!(!has_item(&report, bare));
    assert_eq!(report["rows_cost_missing"], 2); // bare + ghost
    assert_eq!(report["excluded_sales"], 1500); // bare 500 + ghost 1000
    let row = coffee_row(&report);
    assert_eq!(row["total_cost"], 800);
    assert_eq!(row["total_profit"], 1200);
    assert!(row["class"].is_string());
    assert_eq!(row["popularity_pct"], 1.0);

    // Invalid basis → 400.
    let (status, _) = fetch(Some("bogus")).await;
    assert_eq!(status, 400);
}

/// Pinned invariant: right after `backfill-cost-snapshots`, the menu
/// engineering report returns IDENTICAL rows and totals under both cost
/// bases — including SKUs sold with addons (addon revenue/cost is excluded
/// from this report on BOTH sides; an uncosted addon must not knock the
/// item out of the as-sold view).
#[sqlx::test]
async fn test_menu_engineering_bases_match_after_backfill(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(|cfg| routes::configure(cfg, web::Data::new(pool.clone()))),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let admin = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "read").await;
    let token = generate_org_admin_token(admin, org_id);
    let cat_id = seed_category(&pool, org_id).await;

    // Ingredient @ 100 piastres (catalog value; backfill + current rollup
    // both resolve through it).
    let ing = seed_ingredient(&pool, org_id, "Beans", "g").await;

    // Item A: recipe 2 × ing → unit rollup 200.
    let item_a = seed_menu_item(&pool, org_id, cat_id).await;
    sqlx::query(
        "INSERT INTO menu_item_recipes (menu_item_id, size_label, quantity_used, ingredient_name, ingredient_unit, org_ingredient_id) \
         VALUES ($1, 'one_size', 2.0, 'Beans', 'g', $2)",
    )
    .bind(item_a)
    .bind(ing)
    .execute(&pool)
    .await
    .unwrap();

    // Item B: recipe 1 × ing → unit rollup 100; sold WITH addons, one of
    // which has no ingredient links (uncostable).
    let item_b = seed_menu_item(&pool, org_id, cat_id).await;
    sqlx::query(
        "INSERT INTO menu_item_recipes (menu_item_id, size_label, quantity_used, ingredient_name, ingredient_unit, org_ingredient_id) \
         VALUES ($1, 'one_size', 1.0, 'Beans', 'g', $2)",
    )
    .bind(item_b)
    .bind(ing)
    .execute(&pool)
    .await
    .unwrap();
    let costed_addon: Uuid = sqlx::query_scalar(
        "INSERT INTO addon_items (org_id, name, type, default_price) \
         VALUES ($1, 'Syrup', 'extra', 100) RETURNING id",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO addon_item_ingredients (addon_item_id, org_ingredient_id, quantity_used, ingredient_name, ingredient_unit) \
         VALUES ($1, $2, 1.0, 'Beans', 'g')",
    )
    .bind(costed_addon)
    .bind(ing)
    .execute(&pool)
    .await
    .unwrap();
    let uncosted_addon: Uuid = sqlx::query_scalar(
        "INSERT INTO addon_items (org_id, name, type, default_price) \
         VALUES ($1, 'Mystery', 'extra', 100) RETURNING id",
    )
    .bind(org_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let teller = seed_user(&pool, org_id, "teller").await;
    let shift = seed_shift(&pool, branch_id, teller).await;
    let order = seed_order(&pool, branch_id, teller, shift).await;

    // Stale garbage snapshots — the backfill must overwrite them.
    let _la: Uuid = sqlx::query_scalar(
        "INSERT INTO order_items (order_id, menu_item_id, item_name, unit_price, quantity, line_total, line_cost, unit_cost, cost_missing) \
         VALUES ($1, $2, 'A', 1000, 3, 3000, 77777, 9999, false) RETURNING id",
    )
    .bind(order)
    .bind(item_a)
    .fetch_one(&pool)
    .await
    .unwrap();
    let lb: Uuid = sqlx::query_scalar(
        "INSERT INTO order_items (order_id, menu_item_id, item_name, unit_price, quantity, line_total, line_cost, unit_cost, cost_missing) \
         VALUES ($1, $2, 'B', 1200, 2, 2400, 55555, 8888, false) RETURNING id",
    )
    .bind(order)
    .bind(item_b)
    .fetch_one(&pool)
    .await
    .unwrap();
    for addon in [costed_addon, uncosted_addon] {
        sqlx::query(
            "INSERT INTO order_item_addons (order_item_id, addon_item_id, addon_name, unit_price, quantity, line_total, line_cost) \
             VALUES ($1, $2, 'x', 100, 1, 100, 4444)",
        )
        .bind(lb)
        .bind(addon)
        .execute(&pool)
        .await
        .unwrap();
    }

    // Reprice history from current recipes.
    crate::costing::backfill::backfill_cost_snapshots(
        &pool,
        crate::costing::backfill::BackfillScope::Branch(branch_id),
        false,
    )
    .await
    .unwrap();

    let fetch = |basis: &'static str| {
        let app = &app;
        let token = token.clone();
        async move {
            let req = test::TestRequest::get()
                .uri(&format!(
                    "/reports/branches/{branch_id}/menu-engineering?cost_basis={basis}"
                ))
                .insert_header(("Authorization", format!("Bearer {token}")))
                .to_request();
            let resp = test::call_service(app, req).await;
            assert_eq!(resp.status(), 200);
            let body: serde_json::Value = test::read_body_json(resp).await;
            body
        }
    };

    let snapshot = fetch("snapshot").await;
    let current = fetch("current").await;

    // Same totals, same exclusion count.
    for key in [
        "total_sales",
        "total_cost",
        "total_profit",
        "rows_cost_missing",
        "excluded_sales",
    ] {
        assert_eq!(snapshot[key], current[key], "{key} diverged between bases");
    }

    // Same rows, field by field (order-independent).
    let row_map =
        |report: &serde_json::Value| -> std::collections::HashMap<String, serde_json::Value> {
            report["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| {
                    (
                        format!("{}|{}", r["menu_item_id"], r["size_label"]),
                        serde_json::json!({
                            "total_cost": r["total_cost"],
                            "item_profit": r["item_profit"],
                            "total_profit": r["total_profit"],
                            "popularity_pct": r["popularity_pct"],
                            "class": r["class"],
                        }),
                    )
                })
                .collect()
        };
    assert_eq!(
        row_map(&snapshot),
        row_map(&current),
        "rows diverged between bases"
    );

    // Both bases include BOTH items — the uncosted addon must not exclude
    // item B — and cost is recipe-scope (addon costs not folded in).
    assert_eq!(snapshot["rows"].as_array().unwrap().len(), 2);
    let b_row = snapshot["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["menu_item_id"] == serde_json::json!(item_b.to_string()))
        .expect("addon-bearing item missing from as-sold");
    assert_eq!(b_row["total_cost"], 200); // 100/unit × qty 2, no addon cost
    let a_row = snapshot["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["menu_item_id"] == serde_json::json!(item_a.to_string()))
        .unwrap();
    assert_eq!(a_row["total_cost"], 600); // 200/unit × qty 3
}

// ──────────────────────────────────────────────────────────────
// Inventory reports (valuation / low-stock / consumption / waste / shrinkage)
// ──────────────────────────────────────────────────────────────

macro_rules! init_app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(get_secret()))
                .configure(|cfg| routes::configure(cfg, web::Data::new($pool.clone()))),
        )
        .await
    };
}

async fn seed_ingredient_nullcost(pool: &PgPool, org_id: Uuid, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category) VALUES ($1,$2,$3,'g'::inventory_unit,NULL,'general')")
        .bind(id).bind(org_id).bind(name).execute(pool).await.unwrap();
    id
}

async fn seed_stock_lvl(pool: &PgPool, branch_id: Uuid, ing: Uuid, stock: f64, reorder: f64) {
    sqlx::query("INSERT INTO branch_inventory (branch_id, org_ingredient_id, current_stock, reorder_threshold) VALUES ($1,$2,$3,$4)")
        .bind(branch_id).bind(ing).bind(stock).bind(reorder).execute(pool).await.unwrap();
}

async fn ins_movement(
    pool: &PgPool,
    branch_id: Uuid,
    ing: Uuid,
    mtype: &str,
    qty: f64,
    unit_cost: Option<i64>,
    reason: Option<&str>,
) {
    sqlx::query("INSERT INTO inventory_movements (branch_id, org_ingredient_id, type, quantity, unit_cost, reason) VALUES ($1,$2,$3::inventory_movement_type,$4,$5,$6)")
        .bind(branch_id).bind(ing).bind(mtype).bind(qty).bind(unit_cost).bind(reason).execute(pool).await.unwrap();
}

#[sqlx::test]
async fn test_inventory_valuation_branch_and_org(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "read").await;
    let known = seed_ingredient(&pool, org_id, "Known", "g").await; // cost 100
    let unknown = seed_ingredient_nullcost(&pool, org_id, "Unknown").await;
    seed_branch_inventory(&pool, branch_id, known, 10.0).await; // 10 × 100 = 1000
    seed_branch_inventory(&pool, branch_id, unknown, 5.0).await; // unknown → excluded
    let token = generate_org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    for url in [
        format!("/reports/branches/{branch_id}/inventory-valuation"),
        format!("/reports/orgs/{org_id}/inventory-valuation"),
    ] {
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&url)
                .insert_header(auth.clone())
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let report: InventoryValuationReport = test::read_body_json(resp).await;
        assert_eq!(report.total_value, 1000);
        assert_eq!(report.unknown_cost_count, 1);
        assert_eq!(report.items.len(), 2);
    }
}

#[sqlx::test]
async fn test_org_low_stock_guard_and_supplier(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "read").await;
    // Low item with a supplier.
    let sup = Uuid::new_v4();
    sqlx::query("INSERT INTO suppliers (id, org_id, name) VALUES ($1,$2,'Beans Co')")
        .bind(sup)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    let low = seed_ingredient(&pool, org_id, "Low", "g").await;
    sqlx::query("UPDATE org_ingredients SET supplier_id=$1 WHERE id=$2")
        .bind(sup)
        .bind(low)
        .execute(&pool)
        .await
        .unwrap();
    seed_stock_lvl(&pool, branch_id, low, 5.0, 10.0).await; // below → flagged
    // Zero/zero item must be excluded (G3).
    let zero = seed_ingredient(&pool, org_id, "Zero", "g").await;
    seed_stock_lvl(&pool, branch_id, zero, 0.0, 0.0).await;
    let token = generate_org_admin_token(user_id, org_id);

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/reports/orgs/{org_id}/low-stock"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let rows: Vec<LowStockRow> = test::read_body_json(resp).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].org_ingredient_id, low);
    assert_eq!(rows[0].deficit, 5.0);
    assert_eq!(rows[0].supplier_name.as_deref(), Some("Beans Co"));
}

#[sqlx::test]
async fn test_branch_low_stock_scope_and_all_branches(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    // Second branch in the SAME org — distinct name (branches are unique per
    // org name), so seed_branch's fixed name can't be reused here.
    let branch_b = {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1,$2,'Test Branch 2')")
            .bind(id)
            .bind(org_id)
            .execute(&pool)
            .await
            .unwrap();
        id
    };
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "read").await;

    let beans = seed_ingredient(&pool, org_id, "Beans", "g").await;
    let milk = seed_ingredient(&pool, org_id, "Milk", "ml").await;
    seed_stock_lvl(&pool, branch_a, beans, 2.0, 10.0).await; // A: below
    seed_stock_lvl(&pool, branch_a, milk, 50.0, 10.0).await; // A: ok
    seed_stock_lvl(&pool, branch_b, milk, 1.0, 5.0).await; // B: below
    seed_stock_lvl(&pool, branch_b, beans, 99.0, 10.0).await; // B: ok

    // A second org with its own low item — must never leak into org_id's view.
    let other_org = seed_org(&pool).await;
    let other_branch = seed_branch(&pool, other_org).await;
    let other_ing = seed_ingredient(&pool, other_org, "Sugar", "g").await;
    seed_stock_lvl(&pool, other_branch, other_ing, 0.5, 5.0).await;

    let token = generate_org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    // Branch A only: exactly Beans@A.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/reports/branches/{branch_a}/low-stock"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let rows: Vec<LowStockRow> = test::read_body_json(resp).await;
    assert_eq!(rows.len(), 1, "branch A has exactly one below-reorder item");
    assert_eq!(rows[0].branch_id, branch_a);
    assert_eq!(rows[0].org_ingredient_id, beans);

    // All branches (nil UUID): Beans@A + Milk@B, each attributed to its branch,
    // and the other org's Sugar excluded.
    let nil = Uuid::nil();
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/reports/branches/{nil}/low-stock"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let rows: Vec<LowStockRow> = test::read_body_json(resp).await;
    assert_eq!(
        rows.len(),
        2,
        "all-branches sees both org branches' low items"
    );
    assert!(
        rows.iter()
            .any(|r| r.branch_id == branch_a && r.org_ingredient_id == beans)
    );
    assert!(
        rows.iter()
            .any(|r| r.branch_id == branch_b && r.org_ingredient_id == milk)
    );
    assert!(
        !rows.iter().any(|r| r.org_ingredient_id == other_ing),
        "another org's low stock must never appear in all-branches scope"
    );
}

#[sqlx::test]
async fn test_all_branches_nil_aggregates_consumption(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    // Second branch in the SAME org — distinct name (branches are unique per
    // org name), so seed_branch's fixed name can't be reused here.
    let branch_b = {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1,$2,'Test Branch 2')")
            .bind(id)
            .bind(org_id)
            .execute(&pool)
            .await
            .unwrap();
        id
    };
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "read").await;
    let ing = seed_ingredient(&pool, org_id, "Beans", "g").await;
    ins_movement(&pool, branch_a, ing, "sale", -10.0, Some(100), None).await;
    ins_movement(&pool, branch_b, ing, "sale", -6.0, Some(100), None).await;

    // Another org's consumption must not bleed into the all-branches roll-up.
    let other_org = seed_org(&pool).await;
    let other_branch = seed_branch(&pool, other_org).await;
    let other_ing = seed_ingredient(&pool, other_org, "Beans", "g").await;
    ins_movement(
        &pool,
        other_branch,
        other_ing,
        "sale",
        -99.0,
        Some(100),
        None,
    )
    .await;

    let token = generate_org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    // Single branch A: 10 consumed.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/reports/branches/{branch_a}/consumption"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    let rows: Vec<ConsumptionRow> = test::read_body_json(resp).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].consumed_qty, 10.0);

    // All branches (nil): one summed row, 16 = A(10) + B(6); org-isolated.
    let nil = Uuid::nil();
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/reports/branches/{nil}/consumption"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    let rows: Vec<ConsumptionRow> = test::read_body_json(resp).await;
    assert_eq!(rows.len(), 1, "consumption rolls up to one ingredient row");
    assert_eq!(rows[0].consumed_qty, 16.0);
    assert_eq!(rows[0].consumed_value, Some(1600));
}

#[sqlx::test]
async fn test_all_branches_super_admin_uses_org_header(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let ing = seed_ingredient(&pool, org_id, "Beans", "g").await;
    seed_stock_lvl(&pool, branch_a, ing, 1.0, 10.0).await; // below reorder

    // A super-admin token carries no org — the all-branches scope can't infer one.
    let token = generate_token(Uuid::new_v4(), None, UserRole::SuperAdmin);
    let nil = Uuid::nil();

    // Without X-Org-Id there is no org to roll up over → 403.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/reports/branches/{nil}/low-stock"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        403,
        "super-admin all-branches requires an org header"
    );

    // The dashboard pins the active org via X-Org-Id; a super admin may read any
    // org, so it is honoured and the roll-up is scoped to that org.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/reports/branches/{nil}/low-stock"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .insert_header(("X-Org-Id", org_id.to_string()))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let rows: Vec<LowStockRow> = test::read_body_json(resp).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].org_ingredient_id, ing);
}

#[sqlx::test]
async fn test_consumption_branch_and_org(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "read").await;
    let ing = seed_ingredient(&pool, org_id, "Beans", "g").await;
    // sale 10 + waste 5 consumed, at 100 piastres/unit.
    ins_movement(&pool, branch_id, ing, "sale", -10.0, Some(100), None).await;
    ins_movement(
        &pool,
        branch_id,
        ing,
        "waste",
        -5.0,
        Some(100),
        Some("spoiled"),
    )
    .await;
    let token = generate_org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    for url in [
        format!("/reports/branches/{branch_id}/consumption"),
        format!("/reports/orgs/{org_id}/consumption"),
    ] {
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&url)
                .insert_header(auth.clone())
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let rows: Vec<ConsumptionRow> = test::read_body_json(resp).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].consumed_qty, 15.0);
        assert_eq!(rows[0].consumed_value, Some(1500));
    }
}

#[sqlx::test]
async fn test_waste_report_branch_and_org(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "read").await;
    let ing = seed_ingredient(&pool, org_id, "Cream", "ml").await;
    ins_movement(
        &pool,
        branch_id,
        ing,
        "waste",
        -5.0,
        Some(100),
        Some("spoiled"),
    )
    .await;
    ins_movement(
        &pool,
        branch_id,
        ing,
        "waste",
        -3.0,
        Some(100),
        Some("expired"),
    )
    .await;
    let token = generate_org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    for url in [
        format!("/reports/branches/{branch_id}/waste-report"),
        format!("/reports/orgs/{org_id}/waste-report"),
    ] {
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&url)
                .insert_header(auth.clone())
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let rows: Vec<WasteReportRow> = test::read_body_json(resp).await;
        assert_eq!(rows.len(), 2);
        let spoiled = rows.iter().find(|r| r.reason == "spoiled").unwrap();
        assert_eq!(spoiled.waste_qty, 5.0);
        assert_eq!(spoiled.waste_value, Some(500));
    }
}

#[sqlx::test]
async fn test_shrinkage_branch_and_org(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "read").await;
    let ing = seed_ingredient(&pool, org_id, "Beans", "g").await;
    // stock_count negatives = shrinkage; a reason + an unexplained; one positive (overage, ignored).
    ins_movement(
        &pool,
        branch_id,
        ing,
        "stock_count",
        -8.0,
        Some(100),
        Some("theft"),
    )
    .await;
    ins_movement(&pool, branch_id, ing, "stock_count", -4.0, Some(100), None).await;
    ins_movement(&pool, branch_id, ing, "stock_count", 2.0, Some(100), None).await;
    let token = generate_org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    for url in [
        format!("/reports/branches/{branch_id}/shrinkage"),
        format!("/reports/orgs/{org_id}/shrinkage"),
    ] {
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&url)
                .insert_header(auth.clone())
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let rows: Vec<ShrinkageRow> = test::read_body_json(resp).await;
        // Two reason buckets: theft (8) + unexplained (4); the +2 overage is excluded.
        assert_eq!(rows.len(), 2);
        let theft = rows.iter().find(|r| r.reason == "theft").unwrap();
        assert_eq!(theft.shrinkage_qty, 8.0);
        assert_eq!(theft.shrinkage_value, Some(800));
        assert!(rows.iter().any(|r| r.reason == "unexplained"));
    }
}

#[sqlx::test]
async fn test_inventory_reports_require_inventory_read(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    // Has reports/read but NOT inventory/read.
    grant_permission(&pool, "org_admin", "reports", "read").await;
    let token = generate_org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));
    let url = format!("/reports/branches/{branch_id}/inventory-valuation");

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&url)
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        403,
        "inventory reports must require inventory/read"
    );

    // Granting inventory/read unlocks it.
    grant_permission(&pool, "org_admin", "inventory", "read").await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&url)
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
}

// ── Audit regression tests ───────────────────────────────────────────────

/// V17: an order paid by a split (multiple `order_payments` rows) must NOT
/// multiply order-level aggregates. Before the fix a fan-out `LEFT JOIN
/// order_payments` doubled total_orders / total_revenue / total_tax for a
/// 2-way split.
#[sqlx::test]
async fn test_shift_summary_split_payment_not_double_counted(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "shifts", "read").await;
    grant_permission(&pool, "org_admin", "orders", "read").await;
    let shift_id = seed_shift(&pool, branch_id, user_id).await;
    let token = generate_org_admin_token(user_id, org_id);

    // One order, total 570, paid by cash 300 + card 270 → TWO order_payments rows.
    let order_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO orders (id, branch_id, teller_id, shift_id, idempotency_key, subtotal, discount_amount, tax_amount, total_amount, status, order_number, payment_method, order_ref)
         VALUES ($1,$2,$3,$4, gen_random_uuid(), 500, 0, 70, 570, 'completed', 1, 'cash', gen_random_uuid()::text)"
    ).bind(order_id).bind(branch_id).bind(user_id).bind(shift_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO order_payments (order_id, method, amount) VALUES ($1,'cash',300),($1,'card',270)")
        .bind(order_id).execute(&pool).await.unwrap();

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/reports/shifts/{}/summary", shift_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let s: ShiftSummary = test::read_body_json(resp).await;
    assert_eq!(
        s.total_orders, 1,
        "split payment must not inflate order count"
    );
    assert_eq!(
        s.total_revenue, 570,
        "split payment must not double revenue"
    );
    assert_eq!(s.total_tax, 70, "split payment must not double tax");
    assert_eq!(s.revenue_by_method["cash"], json!(300));
    assert_eq!(s.revenue_by_method["card"], json!(270));
}

/// V17 (org branch comparison): same fan-out, `total_revenue` was inflated even
/// though `COUNT(DISTINCT)` protected the counts.
#[sqlx::test]
async fn test_org_branch_comparison_split_payment_revenue(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "read").await;
    let shift_id = seed_shift(&pool, branch_id, user_id).await;
    let token = generate_org_admin_token(user_id, org_id);

    let order_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO orders (id, branch_id, teller_id, shift_id, idempotency_key, subtotal, discount_amount, tax_amount, total_amount, status, order_number, payment_method, order_ref)
         VALUES ($1,$2,$3,$4, gen_random_uuid(), 500, 0, 70, 570, 'completed', 1, 'cash', gen_random_uuid()::text)"
    ).bind(order_id).bind(branch_id).bind(user_id).bind(shift_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO order_payments (order_id, method, amount) VALUES ($1,'cash',300),($1,'card',270)")
        .bind(order_id).execute(&pool).await.unwrap();

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/reports/orgs/{}/comparison", org_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let report: OrgComparisonReport = test::read_body_json(resp).await;
    let b = report
        .branches
        .iter()
        .find(|b| b.branch_id == branch_id)
        .unwrap();
    assert_eq!(b.total_orders, 1);
    assert_eq!(
        b.total_revenue, 570,
        "split payment must not double branch revenue"
    );
}

/// V18: a voided-and-restocked sale must net to zero consumption (the
/// `void_restock` movement cancels the `sale` movement).
#[sqlx::test]
async fn test_consumption_nets_voided_restock(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "read").await;
    let ing = seed_ingredient(&pool, org_id, "Beans", "g").await;
    // Sale of 10 then a void_restock of the same 10 → net zero consumed.
    ins_movement(&pool, branch_id, ing, "sale", -10.0, Some(100), None).await;
    ins_movement(&pool, branch_id, ing, "void_restock", 10.0, Some(100), None).await;
    let token = generate_org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    for url in [
        format!("/reports/branches/{branch_id}/consumption"),
        format!("/reports/orgs/{org_id}/consumption"),
    ] {
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&url)
                .insert_header(auth.clone())
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let rows: Vec<ConsumptionRow> = test::read_body_json(resp).await;
        let consumed = rows
            .iter()
            .find(|r| r.org_ingredient_id == ing)
            .map(|r| r.consumed_qty)
            .unwrap_or(0.0);
        assert_eq!(
            consumed, 0.0,
            "voided+restocked sale must net to zero consumption ({url})"
        );
    }
}

/// The timeseries timezone flows as a bound parameter (not interpolated) AND the
/// column is the `timezone_name` enum, so a valid non-default IANA tz is honored
/// while an injection payload can't even be stored — the DB rejects it at write
/// time, so a crafted tz never reaches the report query.
#[sqlx::test]
async fn test_timeseries_timezone_is_bound(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "read").await;
    let shift_id = seed_shift(&pool, branch_id, user_id).await;
    seed_order(&pool, branch_id, user_id, shift_id).await;
    let token = generate_org_admin_token(user_id, org_id);
    let url = format!(
        "/reports/branches/{}/sales/timeseries?granularity=daily",
        branch_id
    );

    // A valid non-default IANA timezone is honored (proves the value flows as data).
    sqlx::query("UPDATE branches SET timezone='America/New_York' WHERE id=$1")
        .bind(branch_id)
        .execute(&pool)
        .await
        .unwrap();
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&url)
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        200,
        "valid timezone must still work after parameterization"
    );

    // An injection payload can't even be stored: the timezone_name enum rejects
    // any non-member value at write time (stronger than the bound-param defense —
    // a crafted tz never exists to reach the report query).
    let bad = sqlx::query("UPDATE branches SET timezone=$2::timezone_name WHERE id=$1")
        .bind(branch_id)
        .bind("Africa/Cairo' UNION SELECT version() --")
        .execute(&pool)
        .await;
    assert!(
        bad.is_err(),
        "an invalid/injection timezone must be rejected by the timezone_name enum"
    );
}
