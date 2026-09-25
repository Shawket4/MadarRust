//! Combos and deals on the online storefront (COMBOS_CONTRACT.md §2.5, §3.1;
//! owner answer §11.2): the server prices a combo line at intake, freezes it
//! into the snapshot, and finalize books a header plus its parts; the best
//! deals are applied automatically.
//!
//! Figures (shop of `common::combos`, tax 0, in-mall fee 0):
//! - Lunch deal, the worked example: parts 8571 / 2858 / 4571, oat 1500 →
//!   subtotal 17500.
//! - "Any 2 bites for 90" (n_for_price 2 for 9000 over Bakery): Croissant
//!   5500 × 2 + Cookie 4000. The best chunk is the two croissants (11000 −
//!   9000 = 2000; croissant + cookie would save only 500), split 1000 / 1000
//!   → the croissant line's deal_minor 2000, its line 9000; subtotal
//!   9000 + 4000 = 13000.

mod common;

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use common::combos::{Shop, secret, shop};

const PHONE: &str = "01000000000";

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(
                    madar_rust::realtime::hub::BranchEventHub::new(),
                ))
                .configure(madar_rust::delivery::routes::configure),
        )
        .await
    };
}

async fn send<S, B>(app: &S, req: test::TestRequest) -> (u16, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    let resp = test::call_service(app, req.to_request()).await;
    let status = resp.status().as_u16();
    let bytes = test::read_body(resp).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn device_token() -> String {
    let norm = madar_rust::delivery::normalize_phone(PHONE).unwrap();
    madar_rust::delivery::whatsapp::issue_device_token(&secret().0, &norm).unwrap()
}

async fn online_shop(pool: &PgPool) -> Shop {
    let s = shop(pool).await;
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO branch_delivery_settings (branch_id, in_mall_enabled, outside_enabled, in_mall_fee, in_mall_require_location) \
         VALUES ($1, true, false, 0, false)",
    )
    .bind(s.branch)
    .execute(pool)
    .await
    .unwrap();
    s
}

fn intake(s: &Shop, items: Value) -> Value {
    json!({
        "branch_id": s.branch, "channel": "in_mall",
        "customer_name": "Sara", "customer_phone": PHONE,
        "place_name": "Shop 12", "floor": "2", "unit_number": "B4",
        "payment_method_hint": "cash", "device_token": device_token(),
        "items": items,
    })
}

async fn bites_deal(pool: &PgPool, s: &Shop) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO deal_rules (org_id, name, kind, qty, price) \
         VALUES ($1, 'Any 2 bites for 90', 'n_for_price', 2, 9000) RETURNING id",
    )
    .bind(s.org)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO deal_rule_items (org_id, deal_rule_id, category_id) VALUES ($1, $2, $3)",
    )
    .bind(s.org)
    .bind(id)
    .bind(s.bakery)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Place, walk through the kitchen states, finalize; the order id.
async fn place_and_finalize(pool: &PgPool, s: &Shop, items: Value) -> (Value, Uuid) {
    let app = app!(pool);
    let (st, b) = send(
        &app,
        test::TestRequest::post()
            .uri("/public/delivery-orders")
            .set_json(intake(s, items)),
    )
    .await;
    assert_eq!(st, 201, "intake: {b}");
    let id = b["id"].as_str().unwrap().to_string();
    let tok = s.admin_token();
    for status in ["confirmed", "preparing", "ready", "out_for_delivery"] {
        let (st, b) = send(
            &app,
            test::TestRequest::post()
                .uri(&format!("/delivery-orders/{id}/status"))
                .insert_header(("Authorization", format!("Bearer {tok}")))
                .set_json(json!({ "status": status })),
        )
        .await;
        assert_eq!(st, 200, "status {status}: {b}");
    }
    let (st, f) = send(
        &app,
        test::TestRequest::post()
            .uri(&format!("/delivery-orders/{id}/finalize"))
            .insert_header(("Authorization", format!("Bearer {tok}")))
            .set_json(json!({ "shift_id": s.till, "payment_method": "cash" })),
    )
    .await;
    assert_eq!(st, 200, "finalize: {f}");
    (b, Uuid::parse_str(f["order_id"].as_str().unwrap()).unwrap())
}

#[sqlx::test]
async fn an_online_combo_is_priced_by_the_server_and_booked_as_header_and_parts(pool: PgPool) {
    let s = online_shop(&pool).await;
    let (_, order) = place_and_finalize(&pool, &s, json!([s.lunch_line(1)])).await;
    let subtotal: i32 = sqlx::query_scalar("SELECT subtotal FROM orders WHERE id = $1")
        .bind(order)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(subtotal, 17500);
    #[allow(clippy::type_complexity)]
    let lines: Vec<(
        Uuid,
        String,
        Option<Uuid>,
        i32,
        i32,
        i32,
        Option<i32>,
        Option<i64>,
        bool,
    )> = sqlx::query_as(
        "SELECT id, line_kind, menu_item_id, unit_price, line_total, combo_surcharge, \
                    combo_unit_price, line_cost, cost_missing \
               FROM order_items WHERE order_id = $1 \
              ORDER BY COALESCE(combo_line_id, id), combo_line_id IS NOT NULL, id",
    )
    .bind(order)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(lines.len(), 4, "{lines:?}");
    let h = &lines[0];
    assert_eq!(
        (h.1.as_str(), h.2, h.3, h.4, h.6),
        ("combo", Some(s.combo), 0, 0, Some(15000))
    );
    assert_eq!((h.7, h.8), (Some(0), false));
    let parts: Vec<(Option<Uuid>, i32, i32, i32)> =
        lines[1..].iter().map(|l| (l.2, l.3, l.4, l.5)).collect();
    assert_eq!(
        parts,
        vec![
            (Some(s.burger), 12000, 8571, 0),
            (Some(s.fries), 4000, 2858, 0),
            (Some(s.latte), 6000, 4571, 1000),
        ]
    );
    let heads: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM order_items WHERE order_id = $1 AND combo_line_id = $2",
    )
    .bind(order)
    .bind(h.0)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(heads, 3);
    let oat: i32 = sqlx::query_scalar(
        "SELECT a.line_total FROM order_item_addons a JOIN order_items i ON i.id = a.order_item_id \
          WHERE i.order_id = $1",
    )
    .bind(order)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(oat, 1500);
    // Stock: Burger 10 + Fries 5 + Latte Large 14 grams.
    let moved: f64 = sqlx::query_scalar(
        "SELECT (-SUM(quantity))::float8 FROM inventory_movements WHERE source_id = $1",
    )
    .bind(order)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(moved, 29.0);
}

#[sqlx::test]
async fn a_combo_off_online_is_refused_at_intake(pool: PgPool) {
    let s = online_shop(&pool).await;
    sqlx::query("INSERT INTO combo_channel_settings (org_id, sell_online) VALUES ($1, false)")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let app = app!(pool);
    let (st, b) = send(
        &app,
        test::TestRequest::post()
            .uri("/public/delivery-orders")
            .set_json(intake(&s, json!([s.lunch_line(1)]))),
    )
    .await;
    assert_eq!(
        (st, b["code"].as_str()),
        (409, Some("COMBO_UNAVAILABLE")),
        "{b}"
    );
    assert_eq!(b["vars"]["reason"], "channel");

    // A pick switched off at the branch: COMBO_ITEM_UNAVAILABLE.
    sqlx::query("UPDATE combo_channel_settings SET sell_online = true WHERE org_id = $1")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO branch_menu_overrides (branch_id, menu_item_id, is_available) VALUES ($1, $2, false)",
    )
    .bind(s.branch)
    .bind(s.fries)
    .execute(&pool)
    .await
    .unwrap();
    let (st, b) = send(
        &app,
        test::TestRequest::post()
            .uri("/public/delivery-orders")
            .set_json(intake(&s, json!([s.lunch_line(1)]))),
    )
    .await;
    assert_eq!(
        (st, b["code"].as_str()),
        (409, Some("COMBO_UNAVAILABLE")),
        "{b}"
    );
    assert_eq!(b["vars"]["reason"], "slot_empty");

    // No picks: refused, not defaulted.
    sqlx::query("DELETE FROM branch_menu_overrides WHERE branch_id = $1")
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();
    let (st, b) = send(
        &app,
        test::TestRequest::post()
            .uri("/public/delivery-orders")
            .set_json(intake(
                &s,
                json!([{"menu_item_id": s.combo, "quantity": 1}]),
            )),
    )
    .await;
    assert_eq!(
        (st, b["code"].as_str()),
        (400, Some("COMBO_PICKS_REQUIRED")),
        "{b}"
    );
}

#[sqlx::test]
async fn the_best_deal_is_applied_online_and_booked_on_the_order(pool: PgPool) {
    let s = online_shop(&pool).await;
    let deal = bites_deal(&pool, &s).await;
    let (_, order) = place_and_finalize(
        &pool,
        &s,
        json!([
            {"menu_item_id": s.croissant, "quantity": 2},
            {"menu_item_id": s.cookie, "quantity": 1},
        ]),
    )
    .await;
    let subtotal: i32 = sqlx::query_scalar("SELECT subtotal FROM orders WHERE id = $1")
        .bind(order)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(subtotal, 13000);
    let (minor, total): (i32, i32) = sqlx::query_as(
        "SELECT deal_minor, line_total FROM order_items WHERE order_id = $1 AND menu_item_id = $2",
    )
    .bind(order)
    .bind(s.croissant)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((minor, total), (2000, 9000));
    let (rule, times, discount, server): (Uuid, i16, i32, Option<i32>) = sqlx::query_as(
        "SELECT deal_rule_id, times, discount, discount_server FROM order_deals WHERE order_id = $1",
    )
    .bind(order)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((rule, times, discount, server), (deal, 1, 2000, Some(2000)));
    let (units, cut): (i16, i32) = sqlx::query_as(
        "SELECT l.units, l.discount FROM order_deal_lines l JOIN order_items i ON i.id = l.order_item_id \
          WHERE i.order_id = $1 AND i.menu_item_id = $2",
    )
    .bind(order)
    .bind(s.croissant)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((units, cut), (2, 2000));
}
