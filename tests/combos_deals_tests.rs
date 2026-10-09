//! Deals the teller applies at the till (COMBOS_CONTRACT.md §5, C8).
//!
//! `two_bites` (§5): "Any 2 bites for 90" = n_for_price, qty 2, price 9000,
//! over the Bakery category. Croissant 5500 × 2 and a Cookie 4000:
//! the chunk {5500, 5500} saves 11000 − 9000 = 2000, split 1000/1000, so the
//! croissant line gets deal_minor 2000 and line_total 9000. Subtotal
//! 9000 + 4000 = 13000.

mod common;

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use common::combos::{Shop, secret, shop};
use madar_rust::realtime::hub::BranchEventHub;

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(BranchEventHub::new()))
                .configure(madar_rust::orders::routes::configure)
                .configure(madar_rust::sync::routes::configure),
        )
        .await
    };
}

async fn two_bites(pool: &PgPool, s: &Shop) -> Uuid {
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

async fn post<S, B>(app: &S, uri: &str, token: &str, body: Value) -> (u16, Value)
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
        .insert_header(("Authorization", format!("Bearer {token}")))
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

fn cart(s: &Shop) -> Value {
    json!([
        {"menu_item_id": s.croissant, "quantity": 2},
        {"menu_item_id": s.cookie, "quantity": 1},
    ])
}

#[sqlx::test]
async fn a_live_deal_comes_off_its_line_before_the_bill(pool: PgPool) {
    let s = shop(&pool).await;
    let deal = two_bites(&pool, &s).await;
    let app = app!(pool);
    let mut body = s.order_body(cart(&s));
    body["deals"] =
        json!([{"deal_rule_id": deal, "times": 1, "lines": [{"line_index": 0, "units": 2}]}]);
    let (st, v) = post(&app, "/orders", &s.admin_token(), body).await;
    assert_eq!(st, 201, "{v:#}");
    let croissant = &v["items"][0];
    assert_eq!(croissant["deal_minor"], 2000);
    assert_eq!(croissant["line_total"], 9000);
    assert_eq!(croissant["unit_price"], 5500);
    assert_eq!(v["items"][1]["deal_minor"], 0);
    assert_eq!(v["subtotal"], 13000);
    assert_eq!(v["total_amount"], 13000);
    assert_eq!(v["price_flagged"], false);
    let d = &v["deals"][0];
    assert_eq!(d["deal_rule_id"], deal.to_string());
    assert_eq!(d["name"], "Any 2 bites for 90");
    assert_eq!(
        (d["times"].as_i64(), d["discount"].as_i64()),
        (Some(1), Some(2000))
    );
    assert_eq!(d["discount_server"], 2000);
    assert_eq!(
        d["lines"],
        json!([{"order_item_id": croissant["id"], "units": 2, "discount": 2000}])
    );
    // GET reads the same deals back.
    let req = test::TestRequest::get()
        .uri(&format!("/orders/{}", v["id"].as_str().unwrap()))
        .insert_header(("Authorization", format!("Bearer {}", s.admin_token())))
        .to_request();
    let got: Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(got["deals"], v["deals"]);
}

#[sqlx::test]
async fn a_deal_that_does_not_hold_is_refused_live(pool: PgPool) {
    let s = shop(&pool).await;
    let deal = two_bites(&pool, &s).await;
    let app = app!(pool);
    // One unit is not a chunk of two.
    let mut body = s.order_body(cart(&s));
    body["deals"] =
        json!([{"deal_rule_id": deal, "times": 1, "lines": [{"line_index": 0, "units": 1}]}]);
    let (st, v) = post(&app, "/orders", &s.admin_token(), body).await;
    assert_eq!(
        (st, v["code"].as_str()),
        (409, Some("DEAL_NOT_ELIGIBLE")),
        "{v:#}"
    );
    assert_eq!(v["vars"]["reason"], "count");
    assert_eq!(v["vars"]["deal_rule_id"], deal.to_string());

    // The same two croissants claimed by two applications.
    let mut body = s.order_body(cart(&s));
    body["deals"] = json!([
        {"deal_rule_id": deal, "times": 1, "lines": [{"line_index": 0, "units": 2}]},
        {"deal_rule_id": deal, "times": 1, "lines": [{"line_index": 0, "units": 2}]},
    ]);
    let (st, v) = post(&app, "/orders", &s.admin_token(), body).await;
    assert_eq!(
        (st, v["code"].as_str()),
        (400, Some("DEAL_UNITS_OVERLAP")),
        "{v:#}"
    );

    // A combo line never enters a deal.
    let mut body =
        s.order_body(json!([s.lunch_line(1), {"menu_item_id": s.croissant, "quantity": 1}]));
    body["deals"] = json!([{"deal_rule_id": deal, "times": 1,
        "lines": [{"line_index": 0, "units": 1}, {"line_index": 1, "units": 1}]}]);
    let (st, v) = post(&app, "/orders", &s.admin_token(), body).await;
    assert_eq!(
        (st, v["code"].as_str()),
        (409, Some("DEAL_NOT_ELIGIBLE")),
        "{v:#}"
    );

    // Switched off at the branch.
    sqlx::query("INSERT INTO deal_rule_branch_overrides (deal_rule_id, branch_id, org_id, is_active) VALUES ($1, $2, $3, false)")
        .bind(deal)
        .bind(s.branch)
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let mut body = s.order_body(cart(&s));
    body["deals"] =
        json!([{"deal_rule_id": deal, "times": 1, "lines": [{"line_index": 0, "units": 2}]}]);
    let (st, v) = post(&app, "/orders", &s.admin_token(), body).await;
    assert_eq!(
        (st, v["vars"]["reason"].as_str()),
        (409, Some("inactive")),
        "{v:#}"
    );
}

#[sqlx::test]
async fn applying_a_deal_needs_the_capability(pool: PgPool) {
    let s = shop(&pool).await;
    let deal = two_bites(&pool, &s).await;
    // The teller is denied orders.deals.apply (id 252).
    madar_rust::authz::sync_catalogue(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO user_overrides (org_id, user_id, capability_id, effect, reason) \
         VALUES ($1, $2, 252, 'deny', 'test')",
    )
    .bind(s.org)
    .bind(s.teller)
    .execute(&pool)
    .await
    .unwrap();
    let app = app!(pool);
    let mut body = s.order_body(cart(&s));
    body["till_id"] = json!(s.teller_till);
    body["deals"] =
        json!([{"deal_rule_id": deal, "times": 1, "lines": [{"line_index": 0, "units": 2}]}]);
    let (st, v) = post(&app, "/orders", &s.teller_token(), body.clone()).await;
    assert_eq!(st, 403, "{v:#}");
    // Without the deal the same teller sells as usual.
    body["deals"] = json!([]);
    let (st, v) = post(&app, "/orders", &s.teller_token(), body).await;
    assert_eq!(st, 201, "{v:#}");
}

#[sqlx::test]
async fn a_replayed_deal_keeps_the_till_s_discount_and_flags_a_difference(pool: PgPool) {
    let s = shop(&pool).await;
    let deal = two_bites(&pool, &s).await;
    let app = app!(pool);
    let key = Uuid::new_v4();
    let body = json!({
        "op": "create_order",
        "teller_id": s.admin,
        "request": {
            "branch_id": s.branch, "shift_id": s.till, "payment_method": "cash",
            "idempotency_key": key,
            "items": [
                {"menu_item_id": s.croissant, "quantity": 2, "unit_price": 5500},
                {"menu_item_id": s.cookie, "quantity": 1, "unit_price": 4000},
            ],
            // The till still had the deal at 95.00: it took 1500 off.
            "deals": [{"deal_rule_id": deal, "times": 1, "discount": 1500,
                       "lines": [{"line_index": 0, "units": 2}]}],
        }
    });
    let (st, v) = post(&app, "/sync/replay", &s.admin_token(), body).await;
    assert!(st < 300, "{st} {v:#}");
    let (order, subtotal, flagged): (Uuid, i32, bool) =
        sqlx::query_as("SELECT id, subtotal, price_flagged FROM orders WHERE idempotency_key = $1")
            .bind(key)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(subtotal, 11000 - 1500 + 4000, "what the customer paid");
    assert!(flagged);
    let (discount, server): (i32, Option<i32>) =
        sqlx::query_as("SELECT discount, discount_server FROM order_deals WHERE order_id = $1")
            .bind(order)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((discount, server), (1500, Some(2000)));
    let minor: i32 = sqlx::query_scalar(
        "SELECT deal_minor FROM order_items WHERE order_id = $1 AND menu_item_id = $2",
    )
    .bind(order)
    .bind(s.croissant)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(minor, 1500);
    let flags: Vec<String> = sqlx::query_scalar(
        "SELECT capability FROM authz_replay_flags WHERE subject_id = $1 ORDER BY capability",
    )
    .bind(order)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(flags, vec!["orders.deals.apply:mismatch".to_string()]);
}

#[sqlx::test]
async fn a_replayed_deal_the_server_rejects_is_kept_and_flagged(pool: PgPool) {
    let s = shop(&pool).await;
    let deal = two_bites(&pool, &s).await;
    sqlx::query("UPDATE deal_rules SET is_active = false WHERE id = $1")
        .bind(deal)
        .execute(&pool)
        .await
        .unwrap();
    let app = app!(pool);
    let key = Uuid::new_v4();
    let body = json!({
        "op": "create_order", "teller_id": s.admin,
        "request": {
            "branch_id": s.branch, "shift_id": s.till, "payment_method": "cash",
            "idempotency_key": key,
            "items": [{"menu_item_id": s.croissant, "quantity": 2, "unit_price": 5500}],
            "deals": [{"deal_rule_id": deal, "times": 1, "discount": 2000,
                       "lines": [{"line_index": 0, "units": 2}]}],
        }
    });
    let (st, v) = post(&app, "/sync/replay", &s.admin_token(), body).await;
    assert!(st < 300, "{st} {v:#}");
    let (subtotal, server, name): (i32, Option<i32>, String) = sqlx::query_as(
        "SELECT o.subtotal, d.discount_server, d.deal_name FROM orders o \
           JOIN order_deals d ON d.order_id = o.id WHERE o.idempotency_key = $1",
    )
    .bind(key)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((subtotal, server), (9000, None));
    assert_eq!(name, "Any 2 bites for 90");
}
