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
    StockRow, TellerStats, TimeseriesPoint, WaiterStatsReport, WasteReportRow,
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
async fn test_branch_waiter_stats(pool: PgPool) {
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
    let waiter_id = seed_user(&pool, org_id, "waiter").await;

    grant_permission(&pool, "org_admin", "orders", "read").await;

    // One waiter-attributed order with 2 + 1 units, one direct teller sale.
    let attributed = seed_order(&pool, branch_id, user_id, shift_id).await;
    sqlx::query("UPDATE orders SET waiter_id = $1 WHERE id = $2")
        .bind(waiter_id)
        .bind(attributed)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO order_items (order_id, item_name, quantity, unit_price, line_total) VALUES ($1, 'Coffee', 2, 200, 400), ($1, 'Cake', 1, 170, 170)")
        .bind(attributed)
        .execute(&pool)
        .await
        .unwrap();
    // Direct teller sale (no waiter); inline because seed_order hardcodes
    // order_number 1 and shifts are unique per open teller.
    sqlx::query(
        "INSERT INTO orders (id, branch_id, teller_id, shift_id, idempotency_key, customer_name, subtotal, discount_amount, tax_amount, total_amount, status, order_number, payment_method, order_ref)
         VALUES (gen_random_uuid(), $1, $2, $3, gen_random_uuid(), 'Customer', 500, 0, 70, 570, 'completed', 2, 'cash', gen_random_uuid()::text)"
    )
    .bind(branch_id)
    .bind(user_id)
    .bind(shift_id)
    .execute(&pool)
    .await
    .unwrap();

    let req = test::TestRequest::get()
        .uri(&format!("/reports/branches/{}/waiters", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let report: WaiterStatsReport = test::read_body_json(resp).await;
    assert_eq!(report.attributed_orders, 1);
    assert_eq!(report.total_orders, 2);
    assert_eq!(report.waiters.len(), 1, "direct sale must not appear");
    let w = &report.waiters[0];
    assert_eq!(w.waiter_id, waiter_id);
    assert_eq!(w.orders, 1);
    assert_eq!(w.revenue, 570);
    assert_eq!(w.line_items, 3, "units sold, not distinct lines");
    assert!((w.avg_items_per_order - 3.0).abs() < f64::EPSILON);
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
