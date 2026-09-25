//! The public cart quote (owner answer §11.2): the cart priced exactly as the
//! intake will price it, with the best deals applied, and nothing written.
//!
//! Cart: Lunch deal (the worked example) + Croissant × 2 + Cookie.
//! - line 0, the combo: P 15000, parts 8571 / 2858 / 4571, oat 1500 →
//!   unit_total 17500; à la carte 12000 + 4000 + 6000 + 1500 = 23500 →
//!   saving_unit 6000. Its line_total 17500, unit_price 0.
//! - line 1, Croissant × 2: 11000, deal cut 2000 ("any 2 bites for 90").
//! - line 2, Cookie: 4000 (the croissants are the better chunk).
//! items_total 32500, deal_discount 2000, total_after_deals 30500.

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
                .app_data(web::Data::new(
                    madar_rust::realtime::hub::BranchEventHub::new(),
                ))
                .configure(madar_rust::delivery::routes::configure)
                .configure(madar_rust::tickets::routes::configure),
        )
        .await
    };
}

async fn post<S, B>(app: &S, uri: &str, body: Value) -> (u16, Value)
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

fn cart(s: &Shop) -> Value {
    json!({"items": [
        s.lunch_line(1),
        {"menu_item_id": s.croissant, "quantity": 2},
        {"menu_item_id": s.cookie, "quantity": 1},
    ]})
}

fn check(q: &Value, s: &Shop, deal: Uuid) {
    let l = q["lines"].as_array().unwrap();
    assert_eq!(l.len(), 3, "{q:#}");
    assert_eq!(l[0]["index"], 0);
    assert_eq!(l[0]["unit_price"], 0);
    assert_eq!(l[0]["line_total"], 17500);
    assert_eq!(l[0]["deal_minor"], 0);
    let c = &l[0]["combo"];
    assert_eq!(
        (c["price"].as_i64(), c["unit_total"].as_i64()),
        (Some(15000), Some(17500))
    );
    assert_eq!(c["saving_unit"], 6000);
    let parts: Vec<(String, i64, i64, i64)> = c["parts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["menu_item_id"].as_str().unwrap().to_string(),
                p["line_total"].as_i64().unwrap(),
                p["combo_surcharge"].as_i64().unwrap(),
                p["addons_total"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        parts,
        vec![
            (s.burger.to_string(), 8571, 0, 0),
            (s.fries.to_string(), 2858, 0, 0),
            (s.latte.to_string(), 4571, 1000, 1500),
        ]
    );
    assert_eq!(c["parts"][2]["size_label"], "Large");
    assert_eq!(
        (l[1]["line_total"].as_i64(), l[1]["deal_minor"].as_i64()),
        (Some(11000), Some(2000))
    );
    assert_eq!(l[1]["unit_price"], 5500);
    assert_eq!(
        (l[2]["line_total"].as_i64(), l[2]["deal_minor"].as_i64()),
        (Some(4000), Some(0))
    );
    assert_eq!(q["items_total"], 32500);
    assert_eq!(q["deal_discount"], 2000);
    assert_eq!(q["total_after_deals"], 30500);
    let d = &q["deals"][0];
    assert_eq!(d["deal_rule_id"], deal.to_string());
    assert_eq!(
        (d["times"].as_i64(), d["discount"].as_i64()),
        (Some(1), Some(2000))
    );
    assert_eq!(d["lines"], json!([{"line_index": 1, "units": 2}]));
    assert_eq!(d["name"], "Any 2 bites for 90");
}

#[sqlx::test]
async fn the_online_quote_prices_combos_and_applies_the_best_deal(pool: PgPool) {
    let s = shop(&pool).await;
    let deal = bites_deal(&pool, &s).await;
    let app = app!(pool);
    let (st, q) = post(
        &app,
        &format!("/public/branches/{}/cart-quote", s.branch),
        cart(&s),
    )
    .await;
    assert_eq!(st, 200, "{q:#}");
    check(&q, &s, deal);
    // Nothing written.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_orders")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[sqlx::test]
async fn the_table_quote_prices_the_same_cart_on_the_qr_channel(pool: PgPool) {
    let s = shop(&pool).await;
    let deal = bites_deal(&pool, &s).await;
    let table: Uuid = sqlx::query_scalar(
        "INSERT INTO branch_tables (org_id, branch_id, label) VALUES ($1, $2, 'T1') RETURNING id",
    )
    .bind(s.org)
    .bind(s.branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    let app = app!(pool);
    let (st, q) = post(
        &app,
        &format!("/public/tables/{table}/cart-quote"),
        cart(&s),
    )
    .await;
    assert_eq!(st, 200, "{q:#}");
    check(&q, &s, deal);

    // The QR channel off: combos refuse, deals stop applying.
    sqlx::query("INSERT INTO combo_channel_settings (org_id, sell_qr) VALUES ($1, false)")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let (st, q) = post(
        &app,
        &format!("/public/tables/{table}/cart-quote"),
        cart(&s),
    )
    .await;
    assert_eq!(
        (st, q["code"].as_str()),
        (409, Some("COMBO_UNAVAILABLE")),
        "{q:#}"
    );
    let (st, q) = post(
        &app,
        &format!("/public/tables/{table}/cart-quote"),
        json!({"items": [{"menu_item_id": s.croissant, "quantity": 2}]}),
    )
    .await;
    assert_eq!(st, 200, "{q:#}");
    assert_eq!(q["deal_discount"], 0);
    assert_eq!(q["deals"], json!([]));
}

#[sqlx::test]
async fn an_unknown_branch_or_table_is_404(pool: PgPool) {
    let s = shop(&pool).await;
    let app = app!(pool);
    let (st, _) = post(
        &app,
        &format!("/public/branches/{}/cart-quote", Uuid::new_v4()),
        cart(&s),
    )
    .await;
    assert_eq!(st, 404);
    let (st, _) = post(
        &app,
        &format!("/public/tables/{}/cart-quote", Uuid::new_v4()),
        cart(&s),
    )
    .await;
    assert_eq!(st, 404);
}
