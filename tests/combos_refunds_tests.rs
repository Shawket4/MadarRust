//! A combo is refunded as a whole (C13, COMBOS_CONTRACT.md §3.2).
//!
//! Sale: 2 × Lunch deal, the worked example × 2 (tax 0):
//!   header qty 2, 0; Burger qty 2, 17142; Fries qty 2, 5716;
//!   Latte L qty 2, 9142 + oat 3000. Total 35000.
//! Refunding the header with k = 1 expands to k/n = 1/2 of every part:
//!   header 1 × 0; Burger 1 × 8571; Fries 1 × 2858; Latte 1 × (4571 + 1500)
//!   = 6071. Σ = 17500, the price of one combo.
//! Stock: one combo is Burger 10 g + Fries 5 g + Latte L 14 g = 29 g of
//! Beans, re-filed from sale to waste (a refund never restocks).
//! A deal line: Croissant × 2 at 5500 with deal_minor 2000 → line_total 9000,
//! so one unit refunds at its net 4500.

mod common;

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use common::combos::{Shop, secret, shop};

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .configure(madar_rust::orders::routes::configure)
                .configure(madar_rust::refunds::routes::configure),
        )
        .await
    };
}

async fn call<S, B>(app: &S, s: &Shop, uri: &str, body: Value) -> (u16, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    let req = test::TestRequest::post()
        .uri(uri)
        .insert_header(("Authorization", format!("Bearer {}", s.admin_token())))
        .set_json(body)
        .to_request();
    let resp = test::call_service(app, req).await;
    let status = resp.status().as_u16();
    let bytes = test::read_body(resp).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn grant_refunds(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ('org_admin'::user_role, 'refunds'::permission_resource, 'create'::permission_action, true) \
         ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
}

fn refund(order: &str, amount: i32, lines: Value) -> Value {
    json!({"order_id": order, "amount": amount, "method": "cash",
           "reason": "customer_request", "lines": lines})
}

async fn lines_of(pool: &PgPool, order: &str) -> Vec<(Uuid, String, Option<Uuid>)> {
    sqlx::query_as("SELECT id, line_kind, menu_item_id FROM order_items WHERE order_id = $1::uuid")
        .bind(order)
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn refunded(pool: &PgPool, order: &str) -> Vec<(Option<Uuid>, String, i64, i64)> {
    sqlx::query_as(
        "SELECT i.menu_item_id, i.line_kind, SUM(l.quantity)::bigint, SUM(l.amount)::bigint \
           FROM order_refund_lines l JOIN order_items i ON i.id = l.order_item_id \
          WHERE i.order_id = $1::uuid GROUP BY i.menu_item_id, i.line_kind \
          ORDER BY i.line_kind, SUM(l.amount)",
    )
    .bind(order)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn refunding_one_of_two_combos_returns_half_of_every_part(pool: PgPool) {
    let s = shop(&pool).await;
    grant_refunds(&pool).await;
    let app = app!(pool);
    let (st, o) = call(&app, &s, "/orders", s.order_body(json!([s.lunch_line(2)]))).await;
    assert_eq!(st, 201, "{o:#}");
    let order = o["id"].as_str().unwrap().to_string();
    let header = lines_of(&pool, &order)
        .await
        .into_iter()
        .find(|l| l.1 == "combo")
        .unwrap()
        .0;

    // The header's line amount is the server's to set: whatever is sent.
    let (st, r) = call(
        &app,
        &s,
        "/refunds",
        refund(
            &order,
            17500,
            json!([{"order_item_id": header, "quantity": 1, "amount": 17500}]),
        ),
    )
    .await;
    assert_eq!(st, 201, "{r:#}");
    let got = refunded(&pool, &order).await;
    assert_eq!(
        got,
        vec![
            (Some(s.combo), "combo".into(), 1, 0),
            (Some(s.fries), "combo_part".into(), 1, 2858),
            (Some(s.latte), "combo_part".into(), 1, 6071),
            (Some(s.burger), "combo_part".into(), 1, 8571),
        ]
    );
    let total: i64 = got.iter().map(|g| g.3).sum();
    assert_eq!(total, 17500);

    // 29 g of Beans re-filed as waste for the refunded combo.
    let wasted: f64 = sqlx::query_scalar(
        "SELECT (-SUM(quantity))::float8 FROM inventory_movements \
          WHERE source_type = 'refund' AND type = 'waste'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!((wasted - 29.0).abs() < 1e-6, "{wasted}");

    // One more combo may go back, not two.
    let (st, r) = call(
        &app,
        &s,
        "/refunds",
        refund(
            &order,
            17500,
            json!([{"order_item_id": header, "quantity": 2, "amount": 0}]),
        ),
    )
    .await;
    assert_eq!(st, 400, "{r:#}");
    let (st, r) = call(
        &app,
        &s,
        "/refunds",
        refund(
            &order,
            17500,
            json!([{"order_item_id": header, "quantity": 1, "amount": 0}]),
        ),
    )
    .await;
    assert_eq!(st, 201, "{r:#}");
    let status: String = sqlx::query_scalar("SELECT status::text FROM orders WHERE id = $1::uuid")
        .bind(&order)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "refunded");
}

#[sqlx::test]
async fn a_part_is_never_refunded_alone(pool: PgPool) {
    let s = shop(&pool).await;
    grant_refunds(&pool).await;
    let app = app!(pool);
    let (st, o) = call(&app, &s, "/orders", s.order_body(json!([s.lunch_line(1)]))).await;
    assert_eq!(st, 201, "{o:#}");
    let order = o["id"].as_str().unwrap().to_string();
    let ls = lines_of(&pool, &order).await;
    let header = ls.iter().find(|l| l.1 == "combo").unwrap().0;
    let fries = ls.iter().find(|l| l.2 == Some(s.fries)).unwrap().0;
    let (st, r) = call(
        &app,
        &s,
        "/refunds",
        refund(
            &order,
            2858,
            json!([{"order_item_id": fries, "quantity": 1, "amount": 2858}]),
        ),
    )
    .await;
    assert_eq!(
        (st, r["code"].as_str()),
        (409, Some("COMBO_WHOLE_ONLY")),
        "{r:#}"
    );
    assert_eq!(r["vars"]["combo_line_id"], header.to_string());
    assert_eq!(r["error"], "A combo is refunded or voided as a whole.");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM order_refunds WHERE order_id = $1::uuid")
        .bind(&order)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);

    // The whole combo, too little money for its lines: refused in words.
    let (st, _) = call(
        &app,
        &s,
        "/refunds",
        refund(
            &order,
            10000,
            json!([{"order_item_id": header, "quantity": 1, "amount": 0}]),
        ),
    )
    .await;
    assert_eq!(st, 400);
}

#[sqlx::test]
async fn a_deal_line_refunds_at_its_net_figure(pool: PgPool) {
    let s = shop(&pool).await;
    grant_refunds(&pool).await;
    let app = app!(pool);
    // Seed the deal's result the way the order path stores it: sold as plain
    // lines, the croissant line 2 × 5500 less deal_minor 2000.
    let (st, o) = call(
        &app,
        &s,
        "/orders",
        s.order_body(json!([{"menu_item_id": s.croissant, "quantity": 2}])),
    )
    .await;
    assert_eq!(st, 201, "{o:#}");
    let order = o["id"].as_str().unwrap().to_string();
    let line = lines_of(&pool, &order).await[0].0;
    sqlx::query("UPDATE order_items SET deal_minor = 2000, line_total = 9000 WHERE id = $1")
        .bind(line)
        .execute(&pool)
        .await
        .unwrap();
    let (st, r) = call(
        &app,
        &s,
        "/refunds",
        refund(
            &order,
            4500,
            json!([{"order_item_id": line, "quantity": 1, "amount": 4500}]),
        ),
    )
    .await;
    assert_eq!(st, 201, "{r:#}");
    let got = refunded(&pool, &order).await;
    assert_eq!(got, vec![(Some(s.croissant), "item".into(), 1, 4500)]);
}
