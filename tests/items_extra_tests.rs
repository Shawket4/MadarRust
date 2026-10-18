//! Reports > Operations > Items: each sold item's `menu_total`,
//! `extras_total` and `avg_extra` on `GET /reports/branches/{id}/sales`,
//! rung through the real `POST /orders` path.
//!
//! Per line: menu = unit_price × quantity (the size's price at the branch,
//! when it was sold); extras = add-ons and swaps as charged (a staff comp
//! added back) + optional fields × quantity; never a discount. Voided sales
//! are left out, like every other figure of the report. The closed-day
//! rollups answer the same.
#![allow(clippy::too_many_arguments)]

mod common;

use actix_web::{App, test, web};
use common::legacy_catalog as lc;
use madar_rust::auth::jwt::JwtSecret;
use madar_rust::models::UserRole;
use madar_rust::reports::handlers::{BranchSalesReport, ItemSales};
use madar_rust::reports::sales::{self, Source};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

const fn id(n: u128) -> Uuid {
    Uuid::from_u128(0xe7a0_0000_0000_4000_8000_0000_0000_0000 | n)
}

const ORG: Uuid = id(0x01);
const BRANCH: Uuid = id(0x02);
const USER: Uuid = id(0x03);
const TILL: Uuid = id(0x04);
const CATEGORY: Uuid = id(0x05);
const I_WHOLE: Uuid = id(0x20);
const I_OAT: Uuid = id(0x21);
const I_BEAN: Uuid = id(0x22);
const G_MILK: Uuid = id(0x40);
const G_EXTRAS: Uuid = id(0x41);
const A_WHOLE: Uuid = id(0x50);
const A_OAT: Uuid = id(0x51);
const A_SHOT: Uuid = id(0x52);
const M_LATTE: Uuid = id(0x70);
const M_COOKIE: Uuid = id(0x71);

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

async fn exec(pool: &PgPool, sql: &str, binds: &[Uuid]) {
    let mut q = sqlx::query(sql);
    for b in binds {
        q = q.bind(*b);
    }
    q.execute(pool).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// A café: a Latte in Small 30.00 / Large 38.00 (the branch sells Large at
/// 40.00), a milk swap (whole 0, oat +7.00), an extra shot (+1.50), an
/// optional "Cream" (+5.00), and a one-size cookie at 20.00.
async fn seed(pool: &PgPool) {
    exec(pool, "INSERT INTO organizations (id, name, slug) VALUES ($1, 'Extras Org', 'extras-org')", &[ORG]).await;
    exec(
        pool,
        "INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) \
         VALUES ($1, 'cash', '{}', 'emerald', 'payments_outlined', true, true)",
        &[ORG],
    )
    .await;
    exec(pool, "INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Extras Branch')", &[BRANCH, ORG]).await;
    exec(
        pool,
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, 'Owner', 'extras-owner@test.com', 'hash', 'org_admin'::user_role)",
        &[USER, ORG],
    )
    .await;
    for (res, act) in [("orders", "create"), ("menu_items", "read")] {
        exec(
            pool,
            &format!(
                "INSERT INTO role_permissions (role, resource, action, granted) \
                 VALUES ('org_admin'::user_role, '{res}'::permission_resource, '{act}'::permission_action, true) \
                 ON CONFLICT DO NOTHING"
            ),
            &[],
        )
        .await;
    }
    exec(
        pool,
        "INSERT INTO tills (id, branch_id, teller_id, status, opening_cash) VALUES ($1, $2, $3, 'open', 10000)",
        &[TILL, BRANCH, USER],
    )
    .await;
    exec(pool, "INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'Drinks')", &[CATEGORY, ORG]).await;
    for slug in ["milk", "coffee_bean"] {
        exec(
            pool,
            &format!(
                "INSERT INTO ingredient_categories (org_id, slug, name, sort_order) VALUES ($1, '{slug}', '{slug}', 10) \
                 ON CONFLICT (org_id, slug) DO NOTHING"
            ),
            &[ORG],
        )
        .await;
    }
    for (ing, name, unit, slug) in [
        (I_WHOLE, "Whole milk", "ml", "milk"),
        (I_OAT, "Oat milk", "ml", "milk"),
        (I_BEAN, "Espresso bean", "g", "coffee_bean"),
    ] {
        sqlx::query(
            "INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category_id) \
             SELECT $1, $2, $3, $4::inventory_unit, 1, id FROM ingredient_categories WHERE org_id = $2 AND slug = $5",
        )
        .bind(ing)
        .bind(ORG)
        .bind(name)
        .bind(unit)
        .bind(slug)
        .execute(pool)
        .await
        .unwrap();
    }
    // The milk family becomes a swap group by the table's trigger.
    for (g, name, legacy) in [(G_MILK, "Milk", "milk_type"), (G_EXTRAS, "Extras", "extra")] {
        sqlx::query(
            "INSERT INTO modifier_groups (id, org_id, name, legacy_addon_type, effect) VALUES ($1, $2, $3, $4, 'adds')",
        )
        .bind(g)
        .bind(ORG)
        .bind(name)
        .bind(legacy)
        .execute(pool)
        .await
        .unwrap();
    }
    for (o, g, name, price, sort, ing, qty, unit) in [
        (A_WHOLE, G_MILK, "Whole milk", 0, 1, I_WHOLE, 200.0, "ml"),
        (A_OAT, G_MILK, "Oat milk", 700, 2, I_OAT, 200.0, "ml"),
        (A_SHOT, G_EXTRAS, "Extra shot", 150, 1, I_BEAN, 9.0, "g"),
    ] {
        sqlx::query(
            "INSERT INTO modifier_options (id, group_id, name, price, sort, is_active) VALUES ($1, $2, $3, $4, $5, true)",
        )
        .bind(o)
        .bind(g)
        .bind(name)
        .bind(price)
        .bind(sort)
        .execute(pool)
        .await
        .unwrap();
        lc::addon_ingredient(pool, o, ing, qty, unit).await;
    }
    item(pool, M_LATTE, "Latte", &[("Small", 3000, 1), ("Large", 3800, 2)]).await;
    item(pool, M_COOKIE, "Cookie", &[]).await;
    for size in ["Small", "Large"] {
        lc::item_recipe(pool, M_LATTE, size, I_BEAN, 18.0, "g").await;
        lc::item_recipe(pool, M_LATTE, size, I_WHOLE, 200.0, "ml").await;
    }
    lc::branch_size_override(pool, BRANCH, M_LATTE, "Large", 4000).await;
}

async fn item(pool: &PgPool, id: Uuid, name: &str, sizes: &[(&str, i32, i32)]) {
    let mut tx = pool.begin().await.unwrap();
    let base = sizes.iter().map(|s| s.1).min().unwrap_or(2000);
    sqlx::query(
        "INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES ($1, $2, $3, $4, $5, true)",
    )
    .bind(id)
    .bind(ORG)
    .bind(CATEGORY)
    .bind(name)
    .bind(base)
    .execute(&mut *tx)
    .await
    .unwrap();
    for (label, price, sort) in sizes {
        sqlx::query(
            "INSERT INTO menu_item_sizes (menu_item_id, label, price, sort, is_active) VALUES ($1, $2, $3, $4, true)",
        )
        .bind(id)
        .bind(label)
        .bind(price)
        .bind(sort)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
}

fn line(item: Uuid, size: Option<&str>, qty: i32, addons: &[Uuid], optionals: &[Uuid]) -> Value {
    json!({
        "menu_item_id": item, "size_label": size, "quantity": qty,
        "addons": addons.iter().map(|a| json!({"addon_item_id": a, "quantity": 1})).collect::<Vec<_>>(),
        "optional_field_ids": optionals,
    })
}

/// `POST /orders`; the booked order.
async fn ring(pool: &PgPool, items: Value, extra: Value) -> Value {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .app_data(web::Data::new(madar_rust::realtime::hub::BranchEventHub::new()))
            .configure(madar_rust::orders::routes::configure),
    )
    .await;
    let token = madar_rust::auth::jwt::create_token(&secret(), USER, Some(ORG), UserRole::OrgAdmin, None, 24).unwrap();
    let mut body = json!({
        "branch_id": BRANCH, "till_id": TILL, "payment_method": "cash", "items": items,
        "idempotency_key": Uuid::new_v4(),
    });
    if let (Value::Object(b), Value::Object(e)) = (&mut body, extra) {
        b.extend(e);
    }
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/orders")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(&body)
            .to_request(),
    )
    .await;
    let status = resp.status();
    let order: Value = test::read_body_json(resp).await;
    assert!(status.is_success(), "POST /orders → {status}: {order}");
    order
}

async fn report(pool: &PgPool, source: Source) -> BranchSalesReport {
    sales::branch_sales_report(pool, &[BRANCH], BRANCH, "Extras Branch".into(), None, None, None, None, source)
        .await
        .unwrap()
}

fn item_row(r: &BranchSalesReport, id: Uuid) -> &ItemSales {
    r.by_category
        .iter()
        .flat_map(|c| c.items.iter())
        .find(|i| i.menu_item_id == id)
        .unwrap_or_else(|| panic!("{id} not in the report"))
}

/// The bills: sizes, a swap, an add-on, an optional field, an order discount,
/// a voided sale, and a plain item with nothing on top.
async fn ring_the_day(pool: &PgPool) {
    // 2 × Small (30.00) with oat (+7.00, a swap) and a shot (+1.50), and a
    // branch-priced Large (40.00) with cream (+5.00), under a 10% discount.
    let cream = lc::optional_field(pool, lc::Optional::new(M_LATTE, "Cream", 500)).await;
    let a = ring(
        pool,
        json!([
            line(M_LATTE, Some("Small"), 2, &[A_OAT, A_SHOT], &[]),
            line(M_LATTE, Some("Large"), 1, &[], &[cream]),
        ]),
        json!({ "discount_type": "percentage", "discount_value": 10 }),
    )
    .await;
    assert!(a["discount_amount"].as_i64().unwrap() > 0, "the discount applied: {a}");
    let lines = a["items"].as_array().unwrap();
    assert_eq!(lines[0]["unit_price"], 3000, "the size's price, without its add-ons");
    assert_eq!(lines[1]["unit_price"], 4000, "the branch's price for the size");
    // A plain Large and two cookies (no extras).
    ring(
        pool,
        json!([line(M_LATTE, Some("Large"), 1, &[], &[]), line(M_COOKIE, None, 2, &[], &[])]),
        json!({}),
    )
    .await;
    // A voided sale: never counted.
    let v = ring(pool, json!([line(M_LATTE, Some("Small"), 3, &[A_OAT, A_SHOT], &[cream])]), json!({})).await;
    sqlx::query("UPDATE orders SET status = 'voided' WHERE id = $1")
        .bind(Uuid::parse_str(v["id"].as_str().unwrap()).unwrap())
        .execute(pool)
        .await
        .unwrap();
}

fn assert_figures(r: &BranchSalesReport, what: &str) {
    let latte = item_row(r, M_LATTE);
    assert_eq!(latte.quantity_sold, 4, "{what}");
    // 2 × 30.00 + 40.00 + 40.00, at the prices they were sold at.
    assert_eq!(latte.menu_total, Some(14_000), "{what}");
    // 2 × (7.00 + 1.50) + 1 × 5.00; the discount does not enter it.
    assert_eq!(latte.extras_total, Some(2_200), "{what}");
    assert_eq!(latte.avg_extra, Some(550), "{what}");
    let cookie = item_row(r, M_COOKIE);
    assert_eq!(cookie.quantity_sold, 2, "{what}");
    assert_eq!(cookie.menu_total, Some(4_000), "{what}");
    assert_eq!(cookie.extras_total, Some(0), "{what}");
    assert_eq!(cookie.avg_extra, Some(0), "{what}");
    // The leaderboard carries the same figures.
    let top = r.top_items.iter().find(|i| i.menu_item_id == M_LATTE).unwrap();
    assert_eq!((top.menu_total, top.extras_total, top.avg_extra), (Some(14_000), Some(2_200), Some(550)), "{what}");
}

#[sqlx::test]
async fn avg_extra_is_paid_minus_the_size_price(pool: PgPool) {
    seed(&pool).await;
    ring_the_day(&pool).await;
    assert_figures(&report(&pool, Source::Live).await, "live");

    // Today's menu moving does not rewrite what was sold.
    sqlx::query("UPDATE menu_item_sizes SET price = price * 2 WHERE menu_item_id = $1")
        .bind(M_LATTE)
        .execute(&pool)
        .await
        .unwrap();
    assert_figures(&report(&pool, Source::Live).await, "after a menu price change");

    // The JSON: the three fields are served on every item row.
    let v = serde_json::to_value(report(&pool, Source::Live).await).unwrap();
    for row in v["by_category"][0]["items"].as_array().unwrap() {
        for f in ["menu_total", "extras_total", "avg_extra"] {
            assert!(row[f].is_i64(), "{f} missing: {row}");
        }
    }
}

/// A staff comp on an add-on, a loyalty reward on a line and a deal never
/// lower the extra: it is what was charged before any of them.
#[sqlx::test]
async fn line_level_reductions_do_not_lower_the_extra(pool: PgPool) {
    seed(&pool).await;
    ring_the_day(&pool).await;
    // As the till writes them: a deal off the first line, a reward covering the
    // plain Large, and the shot comped for staff.
    sqlx::raw_sql(&format!(
        "UPDATE order_items SET deal_minor = 500, line_total = line_total - 500
          WHERE menu_item_id = '{M_LATTE}' AND size_label = 'Small'
            AND order_id IN (SELECT id FROM orders WHERE status <> 'voided');
         UPDATE order_items SET is_reward = true, reward_units = 1, reward_covered = 4000, line_total = 0
          WHERE menu_item_id = '{M_LATTE}' AND size_label = 'Large'
            AND NOT EXISTS (SELECT 1 FROM order_item_optionals f WHERE f.order_item_id = order_items.id);
         UPDATE order_item_addons SET staff_comp_minor = line_total, line_total = 0 WHERE addon_item_id = '{A_SHOT}';"
    ))
    .execute(&pool)
    .await
    .unwrap();
    assert_figures(&report(&pool, Source::Live).await, "with a deal, a reward and a staff comp");
}

/// Closed days: the rollups answer exactly what the live report does.
#[sqlx::test]
async fn closed_days_roll_up_the_same_figures(pool: PgPool) {
    seed(&pool).await;
    ring_the_day(&pool).await;
    sqlx::query("UPDATE orders SET created_at = created_at - interval '3 days' WHERE branch_id = $1")
        .bind(BRANCH)
        .execute(&pool)
        .await
        .unwrap();
    madar_rust::rollups::report_tick(&pool, std::time::Duration::ZERO, i64::MAX)
        .await
        .unwrap();
    let rolled_days: i64 = sqlx::query_scalar("SELECT count(*) FROM sales_daily_plan($1, NULL, NULL, true) WHERE day IS NOT NULL")
        .bind(vec![BRANCH])
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(rolled_days > 0, "the sale's day is read from the rollups");
    let (mt, ex): (i64, i64) = sqlx::query_as(
        "SELECT SUM(menu_total)::bigint, SUM(extras_total)::bigint FROM sales_daily_sold_item WHERE branch_id = $1 AND menu_item_id = $2",
    )
    .bind(BRANCH)
    .bind(M_LATTE)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((mt, ex), (14_000, 2_200), "the rollup row");
    let live = report(&pool, Source::Live).await;
    let rolled = report(&pool, Source::Rollups).await;
    assert_figures(&rolled, "rollups");
    assert_eq!(serde_json::to_value(live).unwrap(), serde_json::to_value(rolled).unwrap());

    // A late change to an optional field marks its day: read live, then
    // recomputed to agree again.
    sqlx::query("UPDATE order_item_optionals SET price = 900")
        .execute(&pool)
        .await
        .unwrap();
    let dirty: i64 = sqlx::query_scalar("SELECT count(*) FROM sales_daily_dirty WHERE branch_id = $1")
        .bind(BRANCH)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(dirty > 0, "an optional field's change marks its day dirty");
    let live = report(&pool, Source::Live).await;
    assert_eq!(item_row(&live, M_LATTE).extras_total, Some(2_600));
    assert_eq!(
        serde_json::to_value(&live).unwrap(),
        serde_json::to_value(report(&pool, Source::Rollups).await).unwrap(),
        "dirty: read live"
    );
    madar_rust::rollups::report_tick(&pool, std::time::Duration::ZERO, i64::MAX)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&live).unwrap(),
        serde_json::to_value(report(&pool, Source::Rollups).await).unwrap(),
        "recomputed"
    );
}
