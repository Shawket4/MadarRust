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
use crate::reports::handlers::{
    ChannelBreakdownRow, MaterialCostTrendRow, PeakDayPoint, PoLeadTimeReport, SupplierSpendRow,
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
    sqlx::query("INSERT INTO tills (id, branch_id, teller_id, status, opening_cash) VALUES ($1, $2, $3, 'open', 10000)")
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
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category_id) VALUES ($1, $2, $3, $4::inventory_unit, 100, ingredient_category_id($2, 'general'))")
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
    sqlx::query("INSERT INTO branch_stock (id, branch_id, org_ingredient_id, on_hand) VALUES ($1, $2, $3, $4)")
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
        "INSERT INTO orders (id, branch_id, teller_id, till_id, idempotency_key, customer_name, subtotal, discount_amount, tax_amount, total_amount, status, order_number, payment_method, order_ref)
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

    grant_permission(&pool, "org_admin", "tills", "read").await;
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
    assert_eq!(sales.total_line_items, 1);
    assert_eq!(sales.top_items.len(), 1);
    assert_eq!(sales.by_category.len(), 1);

    // exclude_items drops the item from the units count ONLY.
    let req = test::TestRequest::get()
        .uri(&format!(
            "/reports/branches/{}/sales?exclude_items={}",
            branch_id, item_id
        ))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let excluded: BranchSalesReport = test::read_body_json(resp).await;
    assert_eq!(excluded.total_line_items, 0);
    assert_eq!(excluded.total_revenue, 570, "revenue must be untouched");
    assert_eq!(excluded.top_items.len(), 1, "top items must be untouched");
}

/// Top items rank by quantity, then revenue, then name — the same order as
/// the POS metrics endpoint and the POS core, so the till and the dashboard
/// never list the same window differently.
#[sqlx::test]
async fn branch_sales_top_items_rank_by_quantity_then_revenue_then_name(pool: PgPool) {
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

    // (name, quantity, revenue): the big-ticket single sells least; the two
    // 3 × 900 lines tie on both and fall back to the name.
    for (name, qty, revenue) in [
        ("Cake", 1, 5000),
        ("Tea", 3, 300),
        ("Latte", 3, 900),
        ("Espresso", 3, 900),
    ] {
        let item: Uuid = sqlx::query_scalar(
            "INSERT INTO menu_items (org_id, category_id, name, base_price, is_active) VALUES ($1, $2, $3, 100, true) RETURNING id",
        )
        .bind(org_id)
        .bind(cat_id)
        .bind(name)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO order_items (order_id, menu_item_id, item_name, quantity, unit_price, line_total) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(order_id).bind(item).bind(name).bind(qty).bind(revenue / qty).bind(revenue)
            .execute(&pool).await.unwrap();
    }

    let req = test::TestRequest::get()
        .uri(&format!("/reports/branches/{}/sales", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let sales: BranchSalesReport = test::call_and_read_body_json(&app, req).await;
    let names = |items: &[ItemSales]| {
        items
            .iter()
            .map(|i| i.item_name.clone())
            .collect::<Vec<_>>()
    };
    let expected = vec!["Espresso", "Latte", "Tea", "Cake"];
    assert_eq!(names(&sales.top_items), expected);
    assert_eq!(sales.by_category.len(), 1);
    assert_eq!(names(&sales.by_category[0].items), expected);
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
    assert_eq!(stock.items[0].on_hand, 50.0);
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

    let order_id = seed_order(&pool, branch_id, user_id, shift_id).await;

    let item_id = Uuid::new_v4();
    sqlx::query("INSERT INTO order_items (id, order_id, item_name, quantity, unit_price, line_total) VALUES ($1, $2, 'Burger', 2, 200, 400)")
        .bind(item_id)
        .bind(order_id)
        .execute(&pool)
        .await
        .unwrap();
    let addon_id = Uuid::new_v4();
    sqlx::query("INSERT INTO addon_items (id, org_id, name, type, default_price) VALUES ($1, $2, 'Extra Cheese', 'ingredient', 50)")
        .bind(addon_id)
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO order_item_addons (order_item_id, addon_item_id, addon_name, quantity, unit_price, line_total) VALUES ($1, $2, 'Extra Cheese', 3, 50, 150)")
        .bind(item_id)
        .bind(addon_id)
        .execute(&pool)
        .await
        .unwrap();

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
    assert_eq!(ts[0].line_items, 2, "SUM(order_items.quantity)");
    assert_eq!(ts[0].addons, 3, "SUM(order_item_addons.quantity)");
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
        "INSERT INTO orders (id, branch_id, teller_id, till_id, idempotency_key, customer_name, subtotal, discount_amount, tax_amount, total_amount, status, order_number, payment_method, order_ref)
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
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category_id) VALUES ($1, $2, $3, 'g'::inventory_unit, NULL, ingredient_category_id($2, 'general'))")
        .bind(id).bind(org_id).bind(name).execute(pool).await.unwrap();
    id
}

async fn seed_stock_lvl(pool: &PgPool, branch_id: Uuid, ing: Uuid, stock: f64, reorder: f64) {
    sqlx::query("INSERT INTO branch_stock (branch_id, org_ingredient_id, on_hand, par_min) VALUES ($1, $2, $3, NULLIF($4, 0))")
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
    assert_eq!(rows[0].suggested_qty, 5.0);
    assert_eq!(rows[0].par_min, 10.0);
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
    // Has reports/read but NOT inventory/read. An owner holds everything, so
    // the absence is a per-person deny (a non-protected capability).
    grant_permission(&pool, "org_admin", "reports", "read").await;
    sqlx::query("INSERT INTO permissions (user_id, resource, action, granted) VALUES ($1, 'inventory', 'read', false)")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
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

    // Lifting the deny unlocks it.
    sqlx::query("DELETE FROM permissions WHERE user_id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
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
    grant_permission(&pool, "org_admin", "tills", "read").await;
    grant_permission(&pool, "org_admin", "orders", "read").await;
    let shift_id = seed_shift(&pool, branch_id, user_id).await;
    let token = generate_org_admin_token(user_id, org_id);

    // One order, total 570, paid by cash 300 + card 270 → TWO order_payments rows.
    let order_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO orders (id, branch_id, teller_id, till_id, idempotency_key, subtotal, discount_amount, tax_amount, total_amount, status, order_number, payment_method, order_ref)
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
        "INSERT INTO orders (id, branch_id, teller_id, till_id, idempotency_key, subtotal, discount_amount, tax_amount, total_amount, status, order_number, payment_method, order_ref)
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

// ── Sales ↔ shift reconciliation ──────────────────────────────
//
// The regression these guard: for one branch, one shift, one day, the sales
// report and the shift report have to describe the SAME money. They used to
// disagree three ways at once — tips were folded into the shift's method
// buckets but absent from sales, split orders bucketed under a phantom 'mixed',
// and the two picked different order statuses.

/// Seeds an order plus its `order_payments` legs. `legs` is (method, amount);
/// a multi-leg order gets the nominal `'mixed'` label the POS actually sends.
#[allow(clippy::too_many_arguments)]
async fn seed_paid_order(
    pool: &PgPool,
    branch_id: Uuid,
    teller_id: Uuid,
    shift_id: Uuid,
    order_number: i32,
    status: &str,
    legs: &[(&str, i32)],
    tip: Option<(i32, &str, bool)>,
) -> Uuid {
    let id = Uuid::new_v4();
    let total: i32 = legs.iter().map(|(_, a)| *a).sum();
    let nominal = if legs.len() > 1 { "mixed" } else { legs[0].0 };
    let (tip_amount, tip_method, tip_is_cash) = match tip {
        Some((amt, m, is_cash)) => (amt, Some(m), Some(is_cash)),
        None => (0, None, None),
    };

    sqlx::query(
        "INSERT INTO orders (id, branch_id, teller_id, till_id, idempotency_key, subtotal,
             discount_amount, tax_amount, total_amount, status, order_number, payment_method,
             tip_amount, tip_payment_method, tip_is_cash, order_ref)
         VALUES ($1, $2, $3, $4, gen_random_uuid(), $5, 0, 0, $5, $6::order_status, $7, $8,
                 $9, $10, $11, gen_random_uuid()::text)",
    )
    .bind(id)
    .bind(branch_id)
    .bind(teller_id)
    .bind(shift_id)
    .bind(total)
    .bind(status)
    .bind(order_number)
    .bind(nominal)
    .bind(tip_amount)
    .bind(tip_method)
    .bind(tip_is_cash)
    .execute(pool)
    .await
    .unwrap();

    for (method, amount) in legs {
        sqlx::query(
            "INSERT INTO order_payments (order_id, method, amount, is_cash)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(method)
        .bind(amount)
        .bind(*method == "cash")
        .execute(pool)
        .await
        .unwrap();
    }
    id
}

/// One shift, one day, a realistic mix: a plain cash sale, a card sale with a
/// card tip, a cash sale with a cash tip, a SPLIT card+cash sale, a ticket still
/// open on the KDS, and a void. The sales report and the shift report must
/// agree on revenue, on every method bucket, and on tips.
#[sqlx::test]
async fn sales_and_shift_reports_reconcile(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(|cfg| routes::configure(cfg, web::Data::new(pool.clone())))
            .configure(crate::tills::legacy_routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;

    grant_permission(&pool, "org_admin", "orders", "read").await;
    grant_permission(&pool, "org_admin", "tills", "read").await;

    seed_paid_order(
        &pool,
        branch_id,
        user_id,
        shift_id,
        1,
        "completed",
        &[("cash", 570)],
        None,
    )
    .await;
    seed_paid_order(
        &pool,
        branch_id,
        user_id,
        shift_id,
        2,
        "completed",
        &[("card", 1000)],
        Some((50, "card", false)),
    )
    .await;
    seed_paid_order(
        &pool,
        branch_id,
        user_id,
        shift_id,
        3,
        "completed",
        &[("cash", 800)],
        Some((100, "cash", true)),
    )
    .await;
    // The split sale: nominal label 'mixed', real legs card 400 + cash 200.
    seed_paid_order(
        &pool,
        branch_id,
        user_id,
        shift_id,
        4,
        "completed",
        &[("card", 400), ("cash", 200)],
        None,
    )
    .await;
    // Still on the KDS — rung, paid for, not yet marked completed.
    seed_paid_order(
        &pool,
        branch_id,
        user_id,
        shift_id,
        5,
        "ready",
        &[("cash", 300)],
        None,
    )
    .await;
    // Voided: never collected, must not appear in either report's revenue.
    seed_paid_order(
        &pool,
        branch_id,
        user_id,
        shift_id,
        6,
        "voided",
        &[("card", 9999)],
        None,
    )
    .await;

    let sales: BranchSalesReport = {
        let req = test::TestRequest::get()
            .uri(&format!("/reports/branches/{branch_id}/sales"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        test::read_body_json(test::call_service(&app, req).await).await
    };
    let shift: crate::tills::legacy::ShiftReportResponse = {
        let req = test::TestRequest::get()
            .uri(&format!("/shifts/{shift_id}/report"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        test::read_body_json(test::call_service(&app, req).await).await
    };

    // 570 + 1000 + 800 + 600 + 300 (the open ticket counts; the void does not).
    let expected_revenue = 3270;
    assert_eq!(sales.total_revenue, expected_revenue);
    assert_eq!(
        shift.total_payments, expected_revenue,
        "the shift report must collect the same money the sales report reports"
    );
    assert_eq!(sales.total_orders, 5, "the open KDS ticket is a sale");

    // Tips: standalone on both, and identical.
    assert_eq!(sales.total_tips, 150);
    assert_eq!(shift.total_tips, 150);
    assert_eq!(sales.cash_tips, 100);
    assert_eq!(shift.cash_tips, 100);
    assert_eq!(shift.non_cash_tips, 50);
    assert_eq!(
        shift.net_payments, expected_revenue,
        "tips must NOT be added into net_payments"
    );

    // Method buckets: identical on both, split by the legs actually tendered,
    // with no tip money and no phantom 'mixed' bucket anywhere.
    let shift_buckets: std::collections::HashMap<String, i64> = shift
        .payment_summary
        .iter()
        .map(|r| (r.payment_method.clone(), r.total))
        .collect();
    // cash: 570 + 800 + 200 (split leg) + 300 (open ticket) = 1870
    // card: 1000 + 400 (split leg)                          = 1400
    assert_eq!(sales.revenue_by_method["cash"], json!(1870));
    assert_eq!(sales.revenue_by_method["card"], json!(1400));
    assert_eq!(shift_buckets.get("cash"), Some(&1870));
    assert_eq!(shift_buckets.get("card"), Some(&1400));
    assert!(
        sales.revenue_by_method.get("mixed").is_none(),
        "'mixed' is a nominal label, never a money bucket"
    );
    assert!(
        !shift_buckets.contains_key("mixed"),
        "a tip on a split order must not invent a 'mixed' bucket"
    );

    // The drawer still counts cash tips — they are physically in it — even
    // though they are not part of net_payments.
    // opening 10000 + cash sales 1870 + cash tip 100
    assert_eq!(shift.expected_cash, 11970);
}

/// Guard against a fourth revenue-status dialect appearing. Every money
/// aggregate must scope on [`crate::orders::SOLD`]; the historical variants
/// (`= 'completed'`, `!= 'voided'`) are what let three screens drift apart.
/// The one sanctioned exception is the DRAWER — `compute_system_cash` scopes
/// on [`crate::orders::TENDERED`] (defined beside `SOLD`, with the reason),
/// because a fully refunded sale's notes did enter the till.
// Fully qualified: `actix_web::test` is imported into this module, which would
// otherwise shadow the attribute and demand an async fn.
#[::core::prelude::v1::test]
fn status_predicates_are_unified() {
    let sources = [
        (
            "reports/handlers.rs",
            include_str!("../reports/handlers.rs"),
        ),
        ("reports/legal.rs", include_str!("../reports/legal.rs")),
        ("orders/handlers.rs", include_str!("../orders/handlers.rs")),
        ("tills/handlers.rs", include_str!("../tills/handlers.rs")),
        (
            "insights/handlers.rs",
            include_str!("../insights/handlers.rs"),
        ),
        (
            "bundles/handlers.rs",
            include_str!("../bundles/handlers.rs"),
        ),
        (
            "integrations/handlers.rs",
            include_str!("../integrations/handlers.rs"),
        ),
    ];
    for (name, src) in sources {
        for line in src.lines() {
            let l = line.trim();
            if l.starts_with("//") || l.starts_with("--") {
                continue;
            }
            // Single-ROW guards are not money scoping and may legitimately say
            // "anything but voided": the void handler's idempotency check
            // (`WHERE id = $1 AND status <> 'voided'`) and the delete-shift
            // "does this shift have any orders at all" EXISTS probe.
            let is_row_guard = l.contains("WHERE id = $") || l.contains("EXISTS(");
            if is_row_guard {
                continue;
            }
            for stale in [
                "status != 'voided'",
                "status <> 'voided'",
                "status::text = 'completed'",
                "status = 'completed'",
            ] {
                assert!(
                    !l.contains(stale),
                    "{name}: `{l}`\nscopes orders on `{stale}`. Use crate::orders::SOLD \
                     (status NOT IN ('voided', 'refunded')) so the sales report, the \
                     shift report and the orders KPI strip all count the same orders."
                );
            }
        }
    }
}

// ── Refunds ───────────────────────────────────────────────────
//
// A refund is a row against a sale (20260912090000). Two keys matter and the
// reports must not confuse them: the SALE it was against (the revenue lens —
// what did we keep of this shift's sales) and the SHIFT it was issued in (the
// drawer lens — what left this drawer). A refund issued tomorrow against
// today's sale restates today's revenue and tomorrow's drawer.

/// Money back against `order_id`, issued from `shift_id` by `issued_by`.
/// Straight into the table — the triggers fill org/branch and enforce the
/// ceiling; the refunds module's own tests cover the HTTP path.
async fn seed_refund(
    pool: &PgPool,
    order_id: Uuid,
    shift_id: Uuid,
    issued_by: Uuid,
    amount: i32,
    method: &str,
) {
    sqlx::query(
        "INSERT INTO order_refunds (order_id, till_id, amount, method, is_cash, reason, issued_by)
         VALUES ($1, $2, $3, $4, $4 = 'cash', 'customer_request', $5)",
    )
    .bind(order_id)
    .bind(shift_id)
    .bind(amount)
    .bind(method)
    .bind(issued_by)
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test]
async fn a_partial_refund_comes_off_revenue_but_not_off_money_in(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);
    grant_permission(&pool, "org_admin", "orders", "read").await;
    grant_permission(&pool, "org_admin", "tills", "read").await;

    // Two shifts on the branch: the sale is made in A, one of the refunds is
    // issued from B's drawer.
    let shift_a = seed_shift(&pool, branch_id, user_id).await;
    // A teller has one open shift at a time (`idx_shifts_one_open_per_teller`);
    // B is an earlier, closed one on the same branch.
    let shift_b = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO tills (id, branch_id, teller_id, status, opening_cash, closed_at)
         VALUES ($1, $2, $3, 'closed', 10000, now())",
    )
    .bind(shift_b)
    .bind(branch_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let sale = seed_paid_order(
        &pool,
        branch_id,
        user_id,
        shift_a,
        1,
        "completed",
        &[("cash", 570)],
        None,
    )
    .await;
    seed_paid_order(
        &pool,
        branch_id,
        user_id,
        shift_a,
        2,
        "completed",
        &[("card", 1000)],
        None,
    )
    .await;

    // 200 back in cash from A's own drawer, then 70 back onto a card from B.
    seed_refund(&pool, sale, shift_a, user_id, 200, "cash").await;
    seed_refund(&pool, sale, shift_b, user_id, 70, "card").await;

    let get = |uri: String| {
        test::TestRequest::get()
            .uri(&uri)
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request()
    };

    // ── Shift A: sold 1570, 270 of it went back; its own drawer paid out 200.
    let a: ShiftSummary = test::read_body_json(
        test::call_service(&app, get(format!("/reports/shifts/{shift_a}/summary"))).await,
    )
    .await;
    assert_eq!(
        a.total_orders, 2,
        "a partially refunded sale is still a sale"
    );
    assert_eq!(a.gross_sales, 1570);
    assert_eq!(
        a.refunded_amount, 270,
        "refunds against A's sales, whichever drawer paid"
    );
    assert_eq!(a.total_revenue, 1300, "gross_sales − refunded_amount");
    // Money IN is untouched: the buckets say how the customer paid.
    assert_eq!(a.revenue_by_method["cash"], json!(570));
    assert_eq!(a.revenue_by_method["card"], json!(1000));
    // Money OUT of THIS drawer: the cash refund only.
    assert_eq!(a.refunds_issued_count, 1);
    assert_eq!(a.refunds_issued_amount, 200);
    assert_eq!(a.refunds_issued_cash, 200);

    // ── Shift B: sold nothing, handed 70 back on a card.
    let b: ShiftSummary = test::read_body_json(
        test::call_service(&app, get(format!("/reports/shifts/{shift_b}/summary"))).await,
    )
    .await;
    assert_eq!(b.total_orders, 0);
    assert_eq!(b.total_revenue, 0);
    assert_eq!(
        b.refunded_amount, 0,
        "B made no sale, so nothing was refunded against one"
    );
    assert_eq!(b.refunds_issued_count, 1);
    assert_eq!(b.refunds_issued_amount, 70);
    assert_eq!(
        b.refunds_issued_cash, 0,
        "a card refund does not touch the drawer"
    );

    // ── The branch, over the period: same identity as the shift.
    let sales: BranchSalesReport = test::read_body_json(
        test::call_service(&app, get(format!("/reports/branches/{branch_id}/sales"))).await,
    )
    .await;
    assert_eq!(sales.gross_sales, 1570);
    assert_eq!(sales.refunded_amount, 270);
    assert_eq!(sales.total_revenue, 1300);
    assert_eq!(
        sales.total_revenue, a.total_revenue,
        "shift and sales report describe the same money"
    );
    assert_eq!(sales.revenue_by_method["cash"], json!(570));

    let series: Vec<TimeseriesPoint> = test::read_body_json(
        test::call_service(
            &app,
            get(format!(
                "/reports/branches/{branch_id}/sales/timeseries?granularity=daily"
            )),
        )
        .await,
    )
    .await;
    assert_eq!(series.len(), 1);
    assert_eq!(series[0].revenue, 1300);
    assert_eq!(series[0].refunded, 270);

    // ── Refund the rest. The status flips to `refunded` (the trigger's rule)
    // and the sale leaves every revenue figure; the drawer still remembers.
    seed_refund(&pool, sale, shift_a, user_id, 300, "cash").await;
    let a: ShiftSummary = test::read_body_json(
        test::call_service(&app, get(format!("/reports/shifts/{shift_a}/summary"))).await,
    )
    .await;
    assert_eq!(
        a.total_orders, 1,
        "a fully refunded order is no longer a sale"
    );
    assert_eq!(a.gross_sales, 1000);
    assert_eq!(
        a.refunded_amount, 0,
        "its refunds leave with it — nothing partial remains"
    );
    assert_eq!(a.total_revenue, 1000);
    assert_eq!(
        a.revenue_by_method.get("cash"),
        None,
        "and so does its cash leg"
    );
    assert_eq!(a.refunds_issued_count, 2);
    assert_eq!(a.refunds_issued_amount, 500);
    assert_eq!(a.refunds_issued_cash, 500);
}

#[sqlx::test]
async fn test_consumption_nets_refund_restock(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "read").await;
    let ing = seed_ingredient(&pool, org_id, "Beans", "g").await;
    // Sold 10; a refunded line whose goods came back restocks 4. Consumed: 6.
    ins_movement(&pool, branch_id, ing, "sale", -10.0, Some(100), None).await;
    ins_movement(
        &pool,
        branch_id,
        ing,
        "refund_restock",
        4.0,
        Some(100),
        None,
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
        let consumed = rows
            .iter()
            .find(|r| r.org_ingredient_id == ing)
            .map(|r| r.consumed_qty)
            .unwrap_or(0.0);
        assert_eq!(
            consumed, 6.0,
            "a restocked refund line must net out of consumption ({url})"
        );
    }
}

#[sqlx::test]
async fn delivery_sales_report_every_channel_and_the_fee_apart(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "orders", "read").await;
    let token = generate_org_admin_token(user_id, org_id);

    // One delivered order per channel. The fee is part of `total`; a pickup
    // carries none.
    for (channel, subtotal, fee) in [
        ("in_mall", 1000, 100),
        ("outside", 2000, 300),
        ("umbrella", 1500, 0),
        ("pickup", 800, 0),
    ] {
        sqlx::query(
            "INSERT INTO delivery_orders (org_id, branch_id, channel, status, customer_name, customer_phone,
                 cart, subtotal, delivery_fee, total, tax_amount, tax_rate_applied, tax_inclusive, delivered_at, payment_method)
             VALUES ($1, $2, $3::delivery_channel, 'delivered', 'C', '0100', '[]'::jsonb, $4, $5, $4 + $5, 0, 0, false, now(), 'cash')",
        )
        .bind(org_id)
        .bind(branch_id)
        .bind(channel)
        .bind(subtotal)
        .bind(fee)
        .execute(&pool)
        .await
        .unwrap();
    }

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/reports/branches/{branch_id}/delivery-sales"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = test::read_body_json(resp).await;

    // Until 2026-09 umbrella and pickup were dropped on the floor here.
    assert_eq!(body["total_orders"], 4);
    assert_eq!(body["total_revenue"], 5700);
    assert_eq!(body["total_delivery_fees"], 400);
    assert_eq!(body["total_goods_revenue"], 5300);

    let channels = body["channels"].as_array().unwrap();
    let names: Vec<&str> = channels
        .iter()
        .map(|c| c["channel"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["in_mall", "outside", "umbrella", "pickup"]);
    let outside = &channels[1];
    assert_eq!(outside["revenue"], 2300);
    assert_eq!(outside["delivery_fees"], 300);
    assert_eq!(outside["goods_revenue"], 2000);
    assert_eq!(channels[3]["delivery_fees"], 0, "a pickup has no fee");
}

// ── Org tax + legal audit reports ─────────────────────────────

/// A sale with explicit money columns. `status` is a real `order_status`.
#[allow(clippy::too_many_arguments)]
async fn seed_money_order(
    pool: &PgPool,
    branch_id: Uuid,
    teller_id: Uuid,
    till_id: Uuid,
    order_number: i32,
    subtotal: i32,
    discount: i32,
    tax: i32,
    total: i32,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO orders (id, branch_id, teller_id, till_id, idempotency_key, subtotal,
             discount_amount, tax_amount, total_amount, status, order_number, payment_method, order_ref)
         VALUES ($1, $2, $3, $4, gen_random_uuid(), $5, $6, $7, $8, 'completed', $9, 'cash',
                 gen_random_uuid()::text)",
    )
    .bind(id)
    .bind(branch_id)
    .bind(teller_id)
    .bind(till_id)
    .bind(subtotal)
    .bind(discount)
    .bind(tax)
    .bind(total)
    .bind(order_number)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO order_payments (order_id, method, amount) VALUES ($1, 'cash', $2)")
        .bind(id)
        .bind(total)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn get_json(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    uri: &str,
    token: &str,
) -> serde_json::Value {
    let req = test::TestRequest::get()
        .uri(uri)
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(app, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    assert!(
        status.is_success(),
        "{uri} -> {status}: {}",
        String::from_utf8_lossy(&body)
    );
    serde_json::from_slice(&body).unwrap()
}

/// The tax report's net figure agrees with the branch sales report, and the
/// gross pair keeps a sale refunded in full instead of dropping it from both.
/// The audits add up the same seeded events.
#[sqlx::test]
async fn org_tax_and_legal_audits_add_up_and_agree_with_branch_sales(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let admin = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(admin, org_id);
    grant_permission(&pool, "org_admin", "orders", "read").await;
    let till = seed_shift(&pool, branch_id, admin).await;

    // A: 1000 + 14% tax, half refunded (tax share 70).
    let a = seed_money_order(&pool, branch_id, admin, till, 1, 1000, 0, 140, 1140).await;
    seed_refund(&pool, a, till, admin, 570, "cash").await;
    // B: discounted, price flagged, service charge waived.
    let b = seed_money_order(&pool, branch_id, admin, till, 2, 500, 50, 63, 513).await;
    sqlx::query(
        "UPDATE orders SET price_flagged = true, service_charge_waived_by = $2,
             service_charge_waived_at = now(), service_charge_waived_amount = 60 WHERE id = $1",
    )
    .bind(b)
    .bind(admin)
    .execute(&pool)
    .await
    .unwrap();
    // C: voided.
    let c = seed_money_order(&pool, branch_id, admin, till, 3, 500, 0, 70, 570).await;
    sqlx::query(
        "UPDATE orders SET status = 'voided', voided_at = now(), voided_by = $2,
             void_reason = 'wrong_order' WHERE id = $1",
    )
    .bind(c)
    .bind(admin)
    .execute(&pool)
    .await
    .unwrap();
    // D: refunded in full.
    let d = seed_money_order(&pool, branch_id, admin, till, 4, 500, 0, 70, 570).await;
    seed_refund(&pool, d, till, admin, 570, "cash").await;

    let tax = get_json(&app, &format!("/reports/orgs/{org_id}/tax"), &token).await;
    assert_eq!(tax["tax_collected"], 140 + 63 + 70);
    assert_eq!(tax["refunded_tax"], 70 + 70);
    assert_eq!(tax["net_tax_due"], 133);
    assert_eq!(tax["voided_orders"], 1);
    assert_eq!(tax["discount_amount"], 50);
    assert_eq!(tax["net_revenue"], 570 + 513);

    let sales = get_json(
        &app,
        &format!("/reports/branches/{}/sales", Uuid::nil()),
        &token,
    )
    .await;
    assert_eq!(tax["net_tax_due"], sales["total_tax"]);
    assert_eq!(tax["net_revenue"], sales["total_revenue"]);
    assert_eq!(tax["order_count"], sales["total_orders"]);

    let refunds = get_json(
        &app,
        &format!("/reports/orgs/{org_id}/refunds-audit"),
        &token,
    )
    .await;
    assert_eq!(refunds["total_count"], 2);
    assert_eq!(refunds["total_amount_minor"], 1140);
    assert_eq!(refunds["by_reason"][0]["label"], "customer_request");
    assert_eq!(refunds["by_issuer"][0]["count"], 2);

    let voids = get_json(&app, &format!("/reports/orgs/{org_id}/voids-audit"), &token).await;
    assert_eq!(voids["total_count"], 1);
    assert_eq!(voids["total_amount_minor"], 570);
    assert_eq!(voids["by_reason"][0]["label"], "wrong_order");

    let discounts = get_json(
        &app,
        &format!("/reports/orgs/{org_id}/discounts-audit"),
        &token,
    )
    .await;
    assert_eq!(discounts["total_count"], 1);
    assert_eq!(discounts["total_amount_minor"], 50);
    // Attribution (phase 6): a sale from before it reads `unattributed`, and
    // the entry names the till operator as who applied it.
    assert_eq!(discounts["by_kind"][0]["label"], "unattributed");
    assert_eq!(discounts["entries"].as_array().map(Vec::len), Some(1));
    assert_eq!(discounts["entries"][0]["amount_minor"], 50);
    assert_eq!(discounts["entries"][0]["flagged"], false);
    assert!(discounts["entries"][0]["applied_by_name"].is_string());
    let voids_again = get_json(&app, &format!("/reports/orgs/{org_id}/voids-audit"), &token).await;
    assert!(
        voids_again.get("entries").is_none(),
        "other audits keep their shape"
    );

    let waivers = get_json(
        &app,
        &format!("/reports/orgs/{org_id}/waivers-audit"),
        &token,
    )
    .await;
    assert_eq!(waivers["total_count"], 1);
    assert_eq!(waivers["total_amount_minor"], 60);

    let overrides = get_json(
        &app,
        &format!("/reports/orgs/{org_id}/price-overrides"),
        &token,
    )
    .await;
    assert_eq!(overrides["total_count"], 1);
    assert_eq!(overrides["total_amount_minor"], 513);
}

/// An org report never shows a branch manager a branch they aren't assigned
/// to, and another org's caller is refused outright.
#[sqlx::test]
async fn org_tax_and_audits_are_scoped_to_the_callers_branches(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let mine = seed_branch(&pool, org_id).await;
    sqlx::query("UPDATE branches SET name = 'Mine' WHERE id = $1")
        .bind(mine)
        .execute(&pool)
        .await
        .unwrap();
    let other = seed_branch(&pool, org_id).await;
    let admin = seed_user(&pool, org_id, "org_admin").await;
    let manager = seed_user(&pool, org_id, "branch_manager").await;
    assign_user_to_branch(&pool, manager, mine).await;
    grant_permission(&pool, "org_admin", "orders", "read").await;
    grant_permission(&pool, "branch_manager", "orders", "read").await;

    let till_mine = seed_shift(&pool, mine, admin).await;
    let till_other = seed_shift(&pool, other, manager).await;
    seed_money_order(&pool, mine, admin, till_mine, 1, 1000, 0, 140, 1140).await;
    let o = seed_money_order(&pool, other, manager, till_other, 2, 2000, 0, 280, 2280).await;
    seed_refund(&pool, o, till_other, manager, 100, "cash").await;

    let admin_token = generate_org_admin_token(admin, org_id);
    let tax = get_json(&app, &format!("/reports/orgs/{org_id}/tax"), &admin_token).await;
    assert_eq!(tax["order_count"], 2);

    let manager_token = generate_token(manager, Some(org_id), UserRole::BranchManager);
    let tax = get_json(&app, &format!("/reports/orgs/{org_id}/tax"), &manager_token).await;
    assert_eq!(tax["order_count"], 1);
    assert_eq!(tax["tax_collected"], 140);
    let refunds = get_json(
        &app,
        &format!("/reports/orgs/{org_id}/refunds-audit"),
        &manager_token,
    )
    .await;
    assert_eq!(refunds["total_count"], 0);

    // reports.legal: a teller holds none of it, and an owner can take it away
    // from a manager.
    let teller = seed_user(&pool, org_id, "teller").await;
    let teller_token = generate_token(teller, Some(org_id), UserRole::Teller);
    for path in [
        "tax",
        "refunds-audit",
        "voids-audit",
        "discounts-audit",
        "waivers-audit",
        "price-overrides",
    ] {
        let req = test::TestRequest::get()
            .uri(&format!("/reports/orgs/{org_id}/{path}"))
            .insert_header(("Authorization", format!("Bearer {teller_token}")))
            .to_request();
        assert_eq!(
            test::call_service(&app, req).await.status(),
            403,
            "{path}: a teller holds no reports.legal"
        );
    }
    sqlx::query(
        "INSERT INTO user_overrides (org_id, user_id, capability_id, effect, reason) \
         VALUES ($1, $2, 220, 'deny', 'test')",
    )
    .bind(org_id)
    .bind(manager)
    .execute(&pool)
    .await
    .unwrap();
    let req = test::TestRequest::get()
        .uri(&format!("/reports/orgs/{org_id}/tax"))
        .insert_header(("Authorization", format!("Bearer {manager_token}")))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        403,
        "a manager whose reports.legal was denied"
    );

    let stranger_org = seed_org(&pool).await;
    let stranger = seed_user(&pool, stranger_org, "org_admin").await;
    let req = test::TestRequest::get()
        .uri(&format!("/reports/orgs/{org_id}/voids-audit"))
        .insert_header((
            "Authorization",
            format!(
                "Bearer {}",
                generate_org_admin_token(stranger, stranger_org)
            ),
        ))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), 403);
}

#[sqlx::test]
async fn test_channel_breakdown(pool: PgPool) {
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
    let shift_id_2 = seed_shift(&pool, branch_id, user_id).await;
    grant_permission(&pool, "org_admin", "orders", "read").await;

    // One dine_in (the seeded default) and one moved to delivery — separate
    // shifts since `seed_order` always numbers the order 1 for its shift.
    seed_order(&pool, branch_id, user_id, shift_id).await;
    let delivery_order = seed_order(&pool, branch_id, user_id, shift_id_2).await;
    sqlx::query("UPDATE orders SET order_type = 'delivery' WHERE id = $1")
        .bind(delivery_order)
        .execute(&pool)
        .await
        .unwrap();

    let req = test::TestRequest::get()
        .uri(&format!(
            "/reports/branches/{}/channel-breakdown",
            branch_id
        ))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let rows: Vec<ChannelBreakdownRow> = test::read_body_json(resp).await;
    assert_eq!(rows.len(), 2);
    let dine_in = rows.iter().find(|r| r.channel == "dine_in").unwrap();
    assert_eq!(dine_in.orders, 1);
    assert_eq!(dine_in.revenue, 570);
    assert_eq!(dine_in.avg_order_value, 570);
    let delivery = rows.iter().find(|r| r.channel == "delivery").unwrap();
    assert_eq!(delivery.orders, 1);
    assert_eq!(delivery.revenue, 570);
}

/// Weekdays and hours are read on the scope's wall clock: a branch that inherits its
/// org's Cairo zone counts 22:30 UTC on a Sunday as 00:30 Monday, and so does
/// the org-wide ("all branches") view.
#[sqlx::test]
async fn peak_days_and_hours_read_the_scope_timezone(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(|cfg| routes::configure(cfg, web::Data::new(pool.clone()))),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    sqlx::query("UPDATE organizations SET timezone = 'Africa/Cairo' WHERE id = $1")
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE branches SET timezone = NULL WHERE id = $1")
        .bind(branch_id)
        .execute(&pool)
        .await
        .unwrap();
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = seed_shift(&pool, branch_id, user_id).await;
    grant_permission(&pool, "org_admin", "orders", "read").await;
    let order = seed_order(&pool, branch_id, user_id, shift_id).await;
    // Sunday 7 Jan 2024 22:30 UTC = Monday 00:30 in Cairo.
    sqlx::query("UPDATE orders SET created_at = '2024-01-07T22:30:00Z' WHERE id = $1")
        .bind(order)
        .execute(&pool)
        .await
        .unwrap();

    for scope in [branch_id, Uuid::nil()] {
        let req = test::TestRequest::get()
            .uri(&format!(
                "/reports/branches/{scope}/sales/peak-days?from=2024-01-01T00:00:00Z&to=2024-01-31T00:00:00Z"
            ))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success(), "{scope}: {}", resp.status());
        let rows: Vec<PeakDayPoint> = test::read_body_json(resp).await;
        let busy: Vec<i32> = rows
            .iter()
            .filter(|r| r.orders > 0)
            .map(|r| r.day_of_week)
            .collect();
        assert_eq!(
            busy,
            vec![1],
            "{scope}: the order is a Monday order locally"
        );

        let req = test::TestRequest::get()
            .uri(&format!(
                "/reports/branches/{scope}/sales/peak-hours?from=2024-01-01T00:00:00Z&to=2024-01-31T00:00:00Z"
            ))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success(), "{scope}: {}", resp.status());
        let rows: Vec<PeakHourPoint> = test::read_body_json(resp).await;
        let busy: Vec<i32> = rows
            .iter()
            .filter(|r| r.orders > 0)
            .map(|r| r.hour)
            .collect();
        assert_eq!(
            busy,
            vec![0],
            "{scope}: 22:30 UTC is the 00:00 hour in Cairo"
        );
    }
}

#[sqlx::test]
async fn test_branch_sales_peak_days(pool: PgPool) {
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
        .uri(&format!("/reports/branches/{}/sales/peak-days", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let rows: Vec<PeakDayPoint> = test::read_body_json(resp).await;

    // Always returns exactly 7 rows (one per day of week), even if some are empty.
    assert_eq!(rows.len(), 7, "peak days must return exactly 7 buckets");

    // Days are 0–6 (Sunday–Saturday) in order.
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(
            row.day_of_week, i as i32,
            "day_of_week at index {i} must equal {i}"
        );
    }

    // The seeded order (revenue=570) must appear in exactly one bucket.
    let nonempty: Vec<&PeakDayPoint> = rows.iter().filter(|r| r.orders > 0).collect();
    assert_eq!(
        nonempty.len(),
        1,
        "exactly one weekday bucket should have orders"
    );
    let hot = nonempty[0];
    assert_eq!(hot.orders, 1);
    assert_eq!(hot.revenue, 570);

    // The order fell on this weekday exactly once in range → avg equals total.
    assert_eq!(
        hot.avg_revenue_per_day, 570,
        "avg_revenue_per_day = total when the weekday occurred once"
    );
    assert!(
        (hot.avg_orders_per_day - 1.0).abs() < 0.001,
        "avg_orders_per_day should be 1.0"
    );

    assert!(
        (hot.revenue_pct - 100.0).abs() < 0.1,
        "revenue_pct should be 100.0"
    );
    assert!(
        (hot.orders_pct - 100.0).abs() < 0.1,
        "orders_pct should be 100.0"
    );

    // All empty-day buckets should have zero averages.
    let empty_nonzero_avg = rows
        .iter()
        .filter(|r| r.orders == 0 && r.avg_revenue_per_day != 0)
        .count();
    assert_eq!(
        empty_nonzero_avg, 0,
        "empty weekday buckets must not carry non-zero averages"
    );
}

async fn seed_supplier(pool: &PgPool, org_id: Uuid, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO suppliers (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(org_id)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    id
}

#[allow(clippy::too_many_arguments)]
async fn seed_received_po(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    supplier_id: Option<Uuid>,
    created_by: Uuid,
    ing_id: Uuid,
    quantity_received: f64,
    unit_cost: i64,
    created_at: chrono::DateTime<Utc>,
    received_at: chrono::DateTime<Utc>,
) -> Uuid {
    let po_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO purchase_orders \
            (id, org_id, branch_id, supplier_id, status, created_by, created_at, received_at) \
         VALUES ($1, $2, $3, $4, 'received', $5, $6, $7)",
    )
    .bind(po_id)
    .bind(org_id)
    .bind(branch_id)
    .bind(supplier_id)
    .bind(created_by)
    .bind(created_at)
    .bind(received_at)
    .execute(pool)
    .await
    .unwrap();

    let line_id: Uuid = sqlx::query_scalar(
        "INSERT INTO purchase_order_lines \
            (purchase_order_id, org_ingredient_id, purchase_unit, quantity_ordered, quantity_received, unit_cost) \
         VALUES ($1, $2, 'unit', $3, $3, $4) RETURNING id",
    )
    .bind(po_id)
    .bind(ing_id)
    .bind(quantity_received)
    .bind(unit_cost)
    .fetch_one(pool)
    .await
    .unwrap();
    // The delivery itself, as `purchasing::receive` records it.
    seed_receipt_line(
        pool,
        org_id,
        branch_id,
        Some(po_id),
        Some(line_id),
        supplier_id,
        created_by,
        ing_id,
        quantity_received,
        unit_cost,
        received_at,
        false,
    )
    .await;

    po_id
}

/// One goods receipt with one line (`quantity` negative for a return).
#[allow(clippy::too_many_arguments)]
async fn seed_receipt_line(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    po_id: Option<Uuid>,
    po_line_id: Option<Uuid>,
    supplier_id: Option<Uuid>,
    by: Uuid,
    ing_id: Uuid,
    quantity: f64,
    unit_cost: i64,
    received_at: chrono::DateTime<Utc>,
    is_return: bool,
) {
    let gr: Uuid = sqlx::query_scalar(
        "INSERT INTO goods_receipts (org_id, branch_id, purchase_order_id, supplier_id, is_return, received_by, received_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(org_id)
    .bind(branch_id)
    .bind(po_id)
    .bind(supplier_id)
    .bind(is_return)
    .bind(by)
    .bind(received_at)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO goods_receipt_lines (goods_receipt_id, purchase_order_line_id, org_ingredient_id, quantity, unit_cost) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(gr)
    .bind(po_line_id)
    .bind(ing_id)
    .bind(quantity)
    .bind(unit_cost)
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test]
async fn test_supplier_spend_branch_and_org(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "read").await;
    let ing = seed_ingredient(&pool, org_id, "Flour", "g").await;
    let supplier_id = seed_supplier(&pool, org_id, "Acme Supplies").await;

    let now = Utc::now();
    // 10 units at 200 piastres = 2000 spend.
    seed_received_po(
        &pool,
        org_id,
        branch_id,
        Some(supplier_id),
        user_id,
        ing,
        10.0,
        200,
        now - chrono::Duration::days(2),
        now - chrono::Duration::days(1),
    )
    .await;
    // Unknown-supplier PO must still show up, bucketed separately.
    seed_received_po(
        &pool,
        org_id,
        branch_id,
        None,
        user_id,
        ing,
        5.0,
        100,
        now - chrono::Duration::days(2),
        now - chrono::Duration::days(1),
    )
    .await;

    let token = generate_org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    for url in [
        format!("/reports/branches/{branch_id}/supplier-spend"),
        format!("/reports/orgs/{org_id}/supplier-spend"),
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
        let rows: Vec<SupplierSpendRow> = test::read_body_json(resp).await;
        assert_eq!(rows.len(), 2);
        let named = rows
            .iter()
            .find(|r| r.supplier_id == Some(supplier_id))
            .unwrap();
        assert_eq!(named.supplier_name, "Acme Supplies");
        assert_eq!(named.orders, 1);
        assert_eq!(named.total_spend, 2000);
        let unknown = rows.iter().find(|r| r.supplier_id.is_none()).unwrap();
        assert_eq!(unknown.supplier_name, "Unknown supplier");
        assert_eq!(unknown.total_spend, 500);
    }
}

#[sqlx::test]
async fn test_po_lead_time_branch_and_org(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "read").await;
    let ing = seed_ingredient(&pool, org_id, "Sugar", "g").await;
    let supplier_id = seed_supplier(&pool, org_id, "Acme Supplies").await;

    let now = Utc::now();
    // Exactly 4 days lead time.
    seed_received_po(
        &pool,
        org_id,
        branch_id,
        Some(supplier_id),
        user_id,
        ing,
        1.0,
        100,
        now - chrono::Duration::days(10),
        now - chrono::Duration::days(6),
    )
    .await;

    let token = generate_org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    for url in [
        format!("/reports/branches/{branch_id}/po-lead-time"),
        format!("/reports/orgs/{org_id}/po-lead-time"),
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
        let report: PoLeadTimeReport = test::read_body_json(resp).await;
        assert!((report.overall_avg_days - 4.0).abs() < 0.01);
        assert_eq!(report.by_supplier.len(), 1);
        let row = &report.by_supplier[0];
        assert_eq!(row.orders_received, 1);
        assert!((row.avg_lead_time_days - 4.0).abs() < 0.01);
    }
}

#[allow(clippy::too_many_arguments)]
async fn seed_goods_receipt(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    supplier_id: Uuid,
    received_by: Uuid,
    ing_id: Uuid,
    unit_cost: i64,
    received_at: chrono::DateTime<Utc>,
) -> Uuid {
    let gr_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO goods_receipts (id, org_id, branch_id, supplier_id, is_return, received_by, received_at) \
         VALUES ($1, $2, $3, $4, false, $5, $6)",
    )
    .bind(gr_id)
    .bind(org_id)
    .bind(branch_id)
    .bind(supplier_id)
    .bind(received_by)
    .bind(received_at)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO goods_receipt_lines (goods_receipt_id, org_ingredient_id, quantity, unit_cost) \
         VALUES ($1, $2, 10, $3)",
    )
    .bind(gr_id)
    .bind(ing_id)
    .bind(unit_cost)
    .execute(pool)
    .await
    .unwrap();

    gr_id
}

#[sqlx::test]
async fn test_material_cost_trend_branch_and_org(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "inventory", "read").await;

    let acme = seed_supplier(&pool, org_id, "Acme Supplies").await;
    let cheap_co = seed_supplier(&pool, org_id, "Cheap Co").await;
    let now = Utc::now();

    // Beans: 4 receipts from Acme, each pricier than the last → a streak of 3
    // rises (100 -> 110 -> 120 -> 130). Cheap Co has since sold the same
    // ingredient for less, so it should surface as the switch candidate.
    let beans = seed_ingredient(&pool, org_id, "Beans", "g").await;
    seed_goods_receipt(
        &pool,
        org_id,
        branch_id,
        acme,
        user_id,
        beans,
        100,
        now - chrono::Duration::days(40),
    )
    .await;
    seed_goods_receipt(
        &pool,
        org_id,
        branch_id,
        acme,
        user_id,
        beans,
        110,
        now - chrono::Duration::days(30),
    )
    .await;
    seed_goods_receipt(
        &pool,
        org_id,
        branch_id,
        acme,
        user_id,
        beans,
        120,
        now - chrono::Duration::days(20),
    )
    .await;
    seed_goods_receipt(
        &pool,
        org_id,
        branch_id,
        acme,
        user_id,
        beans,
        130,
        now - chrono::Duration::days(10),
    )
    .await;
    seed_goods_receipt(
        &pool,
        org_id,
        branch_id,
        cheap_co,
        user_id,
        beans,
        90,
        now - chrono::Duration::days(25),
    )
    .await;

    // Milk: only 2 receipts, both rising — below the 3-rise threshold, must
    // not appear in the report at all.
    let milk = seed_ingredient(&pool, org_id, "Milk", "ml").await;
    seed_goods_receipt(
        &pool,
        org_id,
        branch_id,
        acme,
        user_id,
        milk,
        50,
        now - chrono::Duration::days(20),
    )
    .await;
    seed_goods_receipt(
        &pool,
        org_id,
        branch_id,
        acme,
        user_id,
        milk,
        60,
        now - chrono::Duration::days(10),
    )
    .await;

    let token = generate_org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    for url in [
        format!("/reports/branches/{branch_id}/material-cost-trend"),
        format!("/reports/orgs/{org_id}/material-cost-trend"),
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
        let rows: Vec<MaterialCostTrendRow> = test::read_body_json(resp).await;

        assert_eq!(rows.len(), 1, "only Beans crosses the 3-rise threshold");
        let row = &rows[0];
        assert_eq!(row.org_ingredient_id, beans);
        assert_eq!(row.current_supplier_id, Some(acme));
        assert_eq!(row.current_cost, 130);
        assert_eq!(row.streak_length, 3);
        assert_eq!(row.base_cost, 100);
        assert!((row.pct_increase - 30.0).abs() < 0.01);
        assert_eq!(row.cheaper_supplier_id, Some(cheap_co));
        assert_eq!(row.cheaper_cost, Some(90));
    }
}

// ── PR #6 update: money figures, scoping and 403s ────────────

async fn status_of(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    uri: &str,
    token: &str,
) -> u16 {
    let req = test::TestRequest::get()
        .uri(uri)
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    test::call_service(app, req).await.status().as_u16()
}

/// Channels add up to branch_sales: partial refunds netted, a full refund and a
/// void dropped, a split-tender sale counted once.
#[sqlx::test]
async fn channel_breakdown_adds_up_to_branch_sales(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let admin = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(admin, org_id);
    grant_permission(&pool, "org_admin", "orders", "read").await;
    let till = seed_shift(&pool, branch_id, admin).await;

    // Dine-in 1140, 570 refunded.
    let a = seed_money_order(&pool, branch_id, admin, till, 1, 1000, 0, 140, 1140).await;
    seed_refund(&pool, a, till, admin, 570, "cash").await;
    // Takeaway 2280 paid half cash, half card.
    let b = seed_money_order(&pool, branch_id, admin, till, 2, 2000, 0, 280, 2280).await;
    sqlx::query("UPDATE orders SET order_type = 'takeaway' WHERE id = $1")
        .bind(b)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM order_payments WHERE order_id = $1")
        .bind(b)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO order_payments (order_id, method, amount) VALUES ($1, 'cash', 1140), ($1, 'card', 1140)",
    )
    .bind(b)
    .execute(&pool)
    .await
    .unwrap();
    // Takeaway refunded in full, and a voided dine-in: neither is revenue.
    let c = seed_money_order(&pool, branch_id, admin, till, 3, 500, 0, 70, 570).await;
    sqlx::query("UPDATE orders SET order_type = 'takeaway' WHERE id = $1")
        .bind(c)
        .execute(&pool)
        .await
        .unwrap();
    seed_refund(&pool, c, till, admin, 570, "cash").await;
    let d = seed_money_order(&pool, branch_id, admin, till, 4, 500, 0, 70, 570).await;
    sqlx::query("UPDATE orders SET status = 'voided', voided_at = now(), voided_by = $2, void_reason = 'wrong_order' WHERE id = $1")
        .bind(d)
        .bind(admin)
        .execute(&pool)
        .await
        .unwrap();

    let rows = get_json(
        &app,
        &format!("/reports/branches/{branch_id}/channel-breakdown"),
        &token,
    )
    .await;
    let rows = rows.as_array().unwrap();
    let find = |ch: &str| rows.iter().find(|r| r["channel"] == ch).unwrap().clone();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(find("dine_in")["orders"], 1);
    assert_eq!(find("dine_in")["revenue"], 570);
    assert_eq!(find("takeaway")["orders"], 1);
    assert_eq!(find("takeaway")["revenue"], 2280);
    assert_eq!(find("takeaway")["avg_order_value"], 2280);

    let sales = get_json(
        &app,
        &format!("/reports/branches/{branch_id}/sales"),
        &token,
    )
    .await;
    let sum: i64 = rows.iter().map(|r| r["revenue"].as_i64().unwrap()).sum();
    let orders: i64 = rows.iter().map(|r| r["orders"].as_i64().unwrap()).sum();
    assert_eq!(sum, sales["total_revenue"].as_i64().unwrap());
    assert_eq!(orders, sales["total_orders"].as_i64().unwrap());
}

/// Supplier spend is what was delivered, at the invoiced cost, returns netted,
/// in the window it arrived. A branch manager sees only their branch on the org
/// report; a teller and a manager denied purchasing.orders.read get 403.
#[sqlx::test]
async fn supplier_spend_follows_receipts_and_is_scoped(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let mine = seed_branch(&pool, org_id).await;
    rename_branch(&pool, mine, "Mine").await;
    let other = seed_branch(&pool, org_id).await;
    let admin = seed_user(&pool, org_id, "org_admin").await;
    let manager = seed_user(&pool, org_id, "branch_manager").await;
    let teller = seed_user(&pool, org_id, "teller").await;
    assign_user_to_branch(&pool, manager, mine).await;
    assign_user_to_branch(&pool, teller, mine).await;
    let ing = seed_ingredient(&pool, org_id, "Beans", "g").await;
    let acme = seed_supplier(&pool, org_id, "Acme").await;
    let now = Utc::now();

    // A PO for 10 at 200 ordered, delivered 6 @ 210 then 4 @ 190 (invoice
    // prices), then 2 returned @ 210. Spend = 1260 + 760 - 420 = 1600.
    let po = seed_received_po(
        &pool,
        org_id,
        mine,
        Some(acme),
        admin,
        ing,
        6.0,
        210,
        now - chrono::Duration::days(5),
        now - chrono::Duration::days(3),
    )
    .await;
    let line: Uuid =
        sqlx::query_scalar("SELECT id FROM purchase_order_lines WHERE purchase_order_id = $1")
            .bind(po)
            .fetch_one(&pool)
            .await
            .unwrap();
    seed_receipt_line(
        &pool,
        org_id,
        mine,
        Some(po),
        Some(line),
        Some(acme),
        admin,
        ing,
        4.0,
        190,
        now - chrono::Duration::days(2),
        false,
    )
    .await;
    seed_receipt_line(
        &pool,
        org_id,
        mine,
        None,
        None,
        Some(acme),
        admin,
        ing,
        -2.0,
        210,
        now - chrono::Duration::days(1),
        true,
    )
    .await;
    // Delivered last month: outside the window.
    seed_receipt_line(
        &pool,
        org_id,
        mine,
        None,
        None,
        Some(acme),
        admin,
        ing,
        100.0,
        999,
        now - chrono::Duration::days(40),
        false,
    )
    .await;
    // The other branch spends 500.
    seed_received_po(
        &pool,
        org_id,
        other,
        Some(acme),
        admin,
        ing,
        5.0,
        100,
        now - chrono::Duration::days(5),
        now - chrono::Duration::days(3),
    )
    .await;

    let from = (now - chrono::Duration::days(10)).format("%Y-%m-%dT%H:%M:%SZ");
    let to = now.format("%Y-%m-%dT%H:%M:%SZ");
    let q = format!("from={from}&to={to}");
    let admin_t = generate_org_admin_token(admin, org_id);
    let rows = get_json(
        &app,
        &format!("/reports/branches/{mine}/supplier-spend?{q}"),
        &admin_t,
    )
    .await;
    assert_eq!(rows[0]["total_spend"], 1600, "{rows}");
    assert_eq!(rows[0]["orders"], 1);
    let rows = get_json(
        &app,
        &format!("/reports/orgs/{org_id}/supplier-spend?{q}"),
        &admin_t,
    )
    .await;
    assert_eq!(
        rows[0]["total_spend"], 2100,
        "the owner sees both branches: {rows}"
    );

    let mgr_t = generate_token(manager, Some(org_id), UserRole::BranchManager);
    for path in ["supplier-spend", "po-lead-time", "material-cost-trend"] {
        assert_eq!(
            status_of(&app, &format!("/reports/orgs/{org_id}/{path}"), &mgr_t).await,
            200,
            "{path}"
        );
    }
    let rows = get_json(
        &app,
        &format!("/reports/orgs/{org_id}/supplier-spend?{q}"),
        &mgr_t,
    )
    .await;
    assert_eq!(
        rows[0]["total_spend"], 1600,
        "a manager rolls up only their branch: {rows}"
    );
    let lead = get_json(
        &app,
        &format!("/reports/orgs/{org_id}/po-lead-time?{q}"),
        &mgr_t,
    )
    .await;
    assert_eq!(lead["by_supplier"][0]["orders_received"], 1, "{lead}");
    assert_eq!(
        status_of(
            &app,
            &format!("/reports/branches/{other}/supplier-spend"),
            &mgr_t
        )
        .await,
        403
    );

    let teller_t = generate_token(teller, Some(org_id), UserRole::Teller);
    for path in ["supplier-spend", "po-lead-time", "material-cost-trend"] {
        assert_eq!(
            status_of(&app, &format!("/reports/orgs/{org_id}/{path}"), &teller_t).await,
            403,
            "org {path}"
        );
        assert_eq!(
            status_of(&app, &format!("/reports/branches/{mine}/{path}"), &teller_t).await,
            403,
            "branch {path}"
        );
    }
    // purchasing.orders.read (58) denied to the manager.
    sqlx::query("INSERT INTO user_overrides (org_id, user_id, capability_id, effect, reason) VALUES ($1, $2, 58, 'deny', 'test')")
        .bind(org_id)
        .bind(manager)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        status_of(
            &app,
            &format!("/reports/orgs/{org_id}/supplier-spend"),
            &mgr_t
        )
        .await,
        403
    );
    assert_eq!(
        status_of(
            &app,
            &format!("/reports/branches/{mine}/material-cost-trend"),
            &mgr_t
        )
        .await,
        403
    );

    let stranger_org = seed_org(&pool).await;
    let stranger = seed_user(&pool, stranger_org, "org_admin").await;
    assert_eq!(
        status_of(
            &app,
            &format!("/reports/orgs/{org_id}/supplier-spend"),
            &generate_org_admin_token(stranger, stranger_org)
        )
        .await,
        403
    );
}

/// The four newer legal audits: figures, the caller's branches, reports.legal
/// and the extra HR capability.
#[sqlx::test]
async fn new_legal_audits_add_up_and_are_scoped(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let mine = seed_branch(&pool, org_id).await;
    rename_branch(&pool, mine, "Mine").await;
    let other = seed_branch(&pool, org_id).await;
    let admin = seed_user(&pool, org_id, "org_admin").await;
    let manager = seed_user(&pool, org_id, "branch_manager").await;
    let staff_mine = seed_user(&pool, org_id, "teller").await;
    let staff_other = seed_user(&pool, org_id, "kitchen").await;
    // The template grants as a fresh org gets them (the legacy cells feed the
    // role templates in tests).
    for role in ["org_admin", "branch_manager"] {
        grant_permission(&pool, role, "attendance", "read").await;
    }
    assign_user_to_branch(&pool, manager, mine).await;
    assign_user_to_branch(&pool, staff_mine, mine).await;
    assign_user_to_branch(&pool, staff_other, other).await;

    // Manual deductions: 500 fixed (mine), a 10% one (no amount), 700 (other).
    for (who, amount, pct) in [
        (staff_mine, Some(500i64), None::<f64>),
        (staff_mine, None, Some(10.0)),
        (staff_other, Some(700), None),
    ] {
        sqlx::query(
            "INSERT INTO payroll_deductions (org_id, user_id, amount_piastres, percent_of_base, reason, effective_date, source, created_by)
             VALUES ($1, $2, $3, $4::numeric, 'uniform', CURRENT_DATE, 'manual', $5)",
        )
        .bind(org_id).bind(who).bind(amount).bind(pct).bind(admin)
        .execute(&pool).await.unwrap();
    }
    // Late penalty 300 waived (forgives 300); absence 400 overridden to 100 (forgives 300).
    sqlx::query(
        "INSERT INTO payroll_deductions (org_id, user_id, amount_piastres, original_amount_piastres, reason, effective_date, source, waived_at, waived_by, waive_reason)
         VALUES ($1, $2, 300, 300, 'late', CURRENT_DATE, 'late_penalty', now(), $3, 'traffic')",
    )
    .bind(org_id).bind(staff_mine).bind(admin).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO payroll_deductions (org_id, user_id, amount_piastres, original_amount_piastres, reason, effective_date, source, overridden_at, overridden_by, override_reason)
         VALUES ($1, $2, 100, 400, 'absent', CURRENT_DATE, 'absence', now(), $3, 'sick note')",
    )
    .bind(org_id).bind(staff_other).bind(admin).execute(&pool).await.unwrap();

    // Loyalty: a manual +40 at mine, a manual -15 at other, a birthday reward (not audited).
    let member: Uuid =
        crate::test_support::seed_loyalty_member(&pool, org_id, "0100", "M", "tok-legal").await;
    for (branch, pts, source) in [
        (mine, 40, "manual"),
        (other, -15, "manual"),
        (mine, 100, "birthday"),
    ] {
        sqlx::query(
            "INSERT INTO loyalty_transactions (org_id, customer_id, branch_id, kind, points, created_by, source)
             VALUES ($1, $2, $3, 'adjust', $4, $5, $6)",
        )
        .bind(org_id).bind(member).bind(branch).bind(pts).bind(admin).bind(source)
        .execute(&pool).await.unwrap();
    }
    // Attendance: one correction at each branch, one untouched record.
    for (branch, who, edited) in [
        (mine, staff_mine, true),
        (other, staff_other, true),
        (mine, staff_mine, false),
    ] {
        sqlx::query(
            "INSERT INTO attendance_records (org_id, user_id, branch_id, business_date, edited_by, edit_reason)
             VALUES ($1, $2, $3, CURRENT_DATE - (random() * 100)::int, $4, $5)",
        )
        .bind(org_id).bind(who).bind(branch)
        .bind(edited.then_some(admin)).bind(edited.then_some("forgot to check out"))
        .execute(&pool).await.unwrap();
    }

    let admin_t = generate_org_admin_token(admin, org_id);
    let r = get_json(
        &app,
        &format!("/reports/orgs/{org_id}/manual-deductions-audit"),
        &admin_t,
    )
    .await;
    assert_eq!(
        (r["total_count"].as_i64(), r["total_amount_minor"].as_i64()),
        (Some(3), Some(1200)),
        "{r}"
    );
    let r = get_json(
        &app,
        &format!("/reports/orgs/{org_id}/deduction-overrides-audit"),
        &admin_t,
    )
    .await;
    assert_eq!(
        (r["total_count"].as_i64(), r["total_amount_minor"].as_i64()),
        (Some(2), Some(600)),
        "{r}"
    );
    let r = get_json(
        &app,
        &format!("/reports/orgs/{org_id}/loyalty-adjustments-audit"),
        &admin_t,
    )
    .await;
    assert_eq!(
        (r["total_count"].as_i64(), r["total_amount_minor"].as_i64()),
        (Some(2), Some(55)),
        "{r}"
    );
    let r = get_json(
        &app,
        &format!("/reports/orgs/{org_id}/attendance-corrections-audit"),
        &admin_t,
    )
    .await;
    assert_eq!(r["total_count"], 2, "{r}");
    assert_eq!(r["by_reason"][0]["label"], "forgot to check out");

    // The manager: reports.legal + hr.attendance.read by default, NOT hr.payroll.read.
    let mgr_t = generate_token(manager, Some(org_id), UserRole::BranchManager);
    let r = get_json(
        &app,
        &format!("/reports/orgs/{org_id}/loyalty-adjustments-audit"),
        &mgr_t,
    )
    .await;
    assert_eq!(
        (r["total_count"].as_i64(), r["total_amount_minor"].as_i64()),
        (Some(1), Some(40)),
        "{r}"
    );
    let r = get_json(
        &app,
        &format!("/reports/orgs/{org_id}/attendance-corrections-audit"),
        &mgr_t,
    )
    .await;
    assert_eq!(r["total_count"], 1, "{r}");
    for path in ["manual-deductions-audit", "deduction-overrides-audit"] {
        assert_eq!(
            status_of(&app, &format!("/reports/orgs/{org_id}/{path}"), &mgr_t).await,
            403,
            "{path}: no hr.payroll.read"
        );
    }
    // Given hr.payroll.read (154), the manager sees only their branch's people.
    sqlx::query("INSERT INTO user_overrides (org_id, user_id, capability_id, effect, reason) VALUES ($1, $2, 154, 'allow', 'test')")
        .bind(org_id).bind(manager).execute(&pool).await.unwrap();
    let r = get_json(
        &app,
        &format!("/reports/orgs/{org_id}/manual-deductions-audit"),
        &mgr_t,
    )
    .await;
    assert_eq!(
        (r["total_count"].as_i64(), r["total_amount_minor"].as_i64()),
        (Some(2), Some(500)),
        "{r}"
    );
    let r = get_json(
        &app,
        &format!("/reports/orgs/{org_id}/deduction-overrides-audit"),
        &mgr_t,
    )
    .await;
    assert_eq!(
        (r["total_count"].as_i64(), r["total_amount_minor"].as_i64()),
        (Some(1), Some(300)),
        "{r}"
    );

    let teller_t = generate_token(staff_mine, Some(org_id), UserRole::Teller);
    let stranger_org = seed_org(&pool).await;
    let stranger = seed_user(&pool, stranger_org, "org_admin").await;
    let stranger_t = generate_org_admin_token(stranger, stranger_org);
    for path in [
        "manual-deductions-audit",
        "deduction-overrides-audit",
        "loyalty-adjustments-audit",
        "attendance-corrections-audit",
    ] {
        let uri = format!("/reports/orgs/{org_id}/{path}");
        assert_eq!(
            status_of(&app, &uri, &teller_t).await,
            403,
            "{path}: teller"
        );
        assert_eq!(
            status_of(&app, &uri, &stranger_t).await,
            403,
            "{path}: another org"
        );
    }
    // reports.legal (220) denied: everything closes, even with hr.payroll.read.
    sqlx::query("INSERT INTO user_overrides (org_id, user_id, capability_id, effect, reason) VALUES ($1, $2, 220, 'deny', 'test')")
        .bind(org_id).bind(manager).execute(&pool).await.unwrap();
    for path in [
        "manual-deductions-audit",
        "loyalty-adjustments-audit",
        "attendance-corrections-audit",
    ] {
        assert_eq!(
            status_of(&app, &format!("/reports/orgs/{org_id}/{path}"), &mgr_t).await,
            403,
            "{path}"
        );
    }
}

/// Peak days and the timeseries count a voided sale's items nowhere, and the
/// new sales analytics refuse a manager at a branch they don't work at.
#[sqlx::test]
async fn sales_analytics_items_skip_voids_and_are_scoped(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let mine = seed_branch(&pool, org_id).await;
    rename_branch(&pool, mine, "Mine").await;
    let other = seed_branch(&pool, org_id).await;
    let admin = seed_user(&pool, org_id, "org_admin").await;
    let manager = seed_user(&pool, org_id, "branch_manager").await;
    assign_user_to_branch(&pool, manager, mine).await;
    grant_permission(&pool, "org_admin", "orders", "read").await;
    let till = seed_shift(&pool, mine, admin).await;
    let sold = seed_money_order(&pool, mine, admin, till, 1, 1000, 0, 0, 1000).await;
    let voided = seed_money_order(&pool, mine, admin, till, 2, 1000, 0, 0, 1000).await;
    sqlx::query("UPDATE orders SET status = 'voided', voided_at = now(), voided_by = $2, void_reason = 'wrong_order' WHERE id = $1")
        .bind(voided).bind(admin).execute(&pool).await.unwrap();
    for o in [sold, voided] {
        sqlx::query("INSERT INTO order_items (order_id, item_name, quantity, unit_price, line_total) VALUES ($1, 'Latte', 2, 500, 1000)")
            .bind(o).execute(&pool).await.unwrap();
    }
    let token = generate_org_admin_token(admin, org_id);
    let days = get_json(
        &app,
        &format!("/reports/branches/{mine}/sales/peak-days"),
        &token,
    )
    .await;
    let items: i64 = days
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["line_items"].as_i64().unwrap())
        .sum();
    let revenue: i64 = days
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["revenue"].as_i64().unwrap())
        .sum();
    assert_eq!((items, revenue), (2, 1000));
    let ts = get_json(
        &app,
        &format!("/reports/branches/{mine}/sales/timeseries"),
        &token,
    )
    .await;
    assert_eq!(ts[0]["line_items"], 2, "{ts}");

    let mgr_t = generate_token(manager, Some(org_id), UserRole::BranchManager);
    for path in ["sales/peak-days", "channel-breakdown"] {
        assert_eq!(
            status_of(&app, &format!("/reports/branches/{other}/{path}"), &mgr_t).await,
            403,
            "{path}"
        );
    }
}

async fn rename_branch(pool: &PgPool, branch: Uuid, name: &str) {
    sqlx::query("UPDATE branches SET name = $2 WHERE id = $1")
        .bind(branch)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
}
