//! A combo on a table's bill (COMBOS_CONTRACT.md §1.2 "Open tickets", C12):
//! fired, each part goes to the kitchen tagged with the combo (the header
//! never fires); settled, the order gets the same header + parts as a till
//! sale, priced as the bill froze them. The QR intake refuses a combo that is
//! off for the `qr` channel.

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
                .configure(madar_rust::tickets::routes::configure)
                .configure(madar_rust::kitchen::routes::configure),
        )
        .await
    };
}

async fn table(pool: &PgPool, s: &Shop) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branch_tables (id, org_id, branch_id, label) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(s.org)
        .bind(s.branch)
        .bind(format!("T-{}", &id.to_string()[..6]))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn call<S, B>(app: &S, req: test::TestRequest, token: Option<&str>) -> (u16, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    let req = match token {
        Some(t) => req.insert_header(("Authorization", format!("Bearer {t}"))),
        None => req,
    };
    let resp = test::call_service(app, req.to_request()).await;
    let status = resp.status().as_u16();
    let bytes = test::read_body(resp).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[sqlx::test]
async fn a_combo_fires_its_parts_and_settles_into_header_and_parts(pool: PgPool) {
    let s = shop(&pool).await;
    let t = table(&pool, &s).await;
    let app = app!(pool);
    let (st, v) = call(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .set_json(json!({
                "branch_id": s.branch, "table_id": t,
                "items": [s.lunch_line(1), {"menu_item_id": s.cookie, "quantity": 1}],
            })),
        Some(&s.admin_token()),
    )
    .await;
    assert_eq!(st, 201, "{v:#}");
    assert_eq!(v["subtotal"], 17500 + 4000, "{v:#}");
    let ticket = v["id"].as_str().unwrap().to_string();
    let combo_line = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["menu_item_id"] == s.combo.to_string())
        .unwrap()
        .clone();
    assert_eq!(combo_line["line_total"], 17500);

    // The kitchen: three parts tagged with the combo, the cookie untagged,
    // no header.
    let klines: Vec<(Value, Option<Uuid>)> = sqlx::query_as(
        "SELECT kti.line, kti.open_ticket_item_id FROM kitchen_ticket_items kti \
           JOIN kitchen_tickets kt ON kt.id = kti.kitchen_ticket_id \
          WHERE kt.open_ticket_id = $1::uuid",
    )
    .bind(&ticket)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(klines.len(), 4, "{klines:#?}");
    let tagged: Vec<&(Value, Option<Uuid>)> = klines
        .iter()
        .filter(|(l, _)| !l["combo"].is_null())
        .collect();
    assert_eq!(tagged.len(), 3);
    for (l, bill_line) in &tagged {
        assert_eq!(l["combo"]["name"], "Lunch deal");
        assert_eq!(l["combo"]["line_id"], combo_line["id"]);
        assert_eq!(
            bill_line.map(|b| b.to_string()),
            combo_line["id"].as_str().map(str::to_string)
        );
        assert_ne!(l["menu_item_id"], s.combo.to_string());
    }
    let latte = tagged
        .iter()
        .find(|(l, _)| l["menu_item_id"] == s.latte.to_string())
        .unwrap();
    assert_eq!(latte.0["size_label"], "Large");
    assert_eq!(latte.0["modifiers"], json!(["Oat milk"]));

    // Settle: the same header + parts as a till sale, nothing flagged.
    let (st, o) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{ticket}/settle"))
            .set_json(json!({"shift_id": s.till, "payment_method": "cash"})),
        Some(&s.admin_token()),
    )
    .await;
    assert_eq!(st, 200, "{o:#}");
    assert_eq!(o["subtotal"], 21500);
    assert_eq!(o["price_flagged"], false, "{o:#}");
    let rows: Vec<(String, Uuid, i32)> = sqlx::query_as(
        "SELECT line_kind, menu_item_id, line_total FROM order_items WHERE order_id = $1::uuid \
          ORDER BY COALESCE(combo_line_id, id), combo_line_id IS NOT NULL, id",
    )
    .bind(o["id"].as_str().unwrap())
    .fetch_all(&pool)
    .await
    .unwrap();
    let combo: Vec<(String, Uuid, i32)> = rows.iter().filter(|r| r.0 != "item").cloned().collect();
    assert_eq!(
        combo,
        vec![
            ("combo".into(), s.combo, 0),
            ("combo_part".into(), s.burger, 8571),
            ("combo_part".into(), s.fries, 2858),
            ("combo_part".into(), s.latte, 4571),
        ]
    );
}

#[sqlx::test]
async fn voiding_a_combo_bill_line_takes_every_part_off_the_board(pool: PgPool) {
    let s = shop(&pool).await;
    let t = table(&pool, &s).await;
    let app = app!(pool);
    let (st, v) = call(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .set_json(json!({
                "branch_id": s.branch, "table_id": t, "items": [s.lunch_line(1)],
            })),
        Some(&s.admin_token()),
    )
    .await;
    assert_eq!(st, 201, "{v:#}");
    let ticket = v["id"].as_str().unwrap().to_string();
    let line = v["items"][0]["id"].as_str().unwrap().to_string();
    let (st, r) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{ticket}/items/{line}/void"))
            .set_json(json!({"reason": "customer_request"})),
        Some(&s.admin_token()),
    )
    .await;
    assert!(st < 300, "{st} {r:#}");
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM kitchen_ticket_items WHERE open_ticket_item_id = $1::uuid AND voided_at IS NULL",
    )
    .bind(&line)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(live, 0, "no part of the voided combo is left for the cook");
}

#[sqlx::test]
async fn the_qr_intake_refuses_a_combo_that_is_off_for_qr(pool: PgPool) {
    let s = shop(&pool).await;
    let t = table(&pool, &s).await;
    sqlx::query("INSERT INTO combo_channel_branch_overrides (branch_id, org_id, sell_qr) VALUES ($1, $2, false)")
        .bind(s.branch)
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let app = app!(pool);
    let (st, v) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/table-orders")
            .set_json(json!({
                "table_id": t, "idempotency_key": Uuid::new_v4(), "items": [s.lunch_line(1)],
            })),
        None,
    )
    .await;
    assert_eq!(
        (st, v["code"].as_str()),
        (409, Some("COMBO_UNAVAILABLE")),
        "{v:#}"
    );
    assert_eq!(v["vars"]["reason"], "channel");

    // On for QR: accepted and priced by the server.
    sqlx::query("DELETE FROM combo_channel_branch_overrides WHERE branch_id = $1")
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();
    let (st, v) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/table-orders")
            .set_json(json!({
                "table_id": t, "idempotency_key": Uuid::new_v4(), "items": [s.lunch_line(1)],
            })),
        None,
    )
    .await;
    assert!(st < 300, "{st} {v:#}");
    assert_eq!(v["subtotal"], 17500, "{v:#}");

    // The waiter's till is not the QR channel: the same override does not
    // stop a waiter.
    sqlx::query("INSERT INTO combo_channel_branch_overrides (branch_id, org_id, sell_qr) VALUES ($1, $2, false)")
        .bind(s.branch)
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let t2 = table(&pool, &s).await;
    let (st, v) = call(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .set_json(json!({
                "branch_id": s.branch, "table_id": t2, "items": [s.lunch_line(1)],
            })),
        Some(&s.admin_token()),
    )
    .await;
    assert_eq!(st, 201, "{v:#}");
}

#[sqlx::test]
async fn a_qr_bill_takes_the_best_deal_by_itself_at_the_settle(pool: PgPool) {
    let s = shop(&pool).await;
    let t = table(&pool, &s).await;
    // "Any 2 bites for 90" over Bakery (§5 two_bites): 2 × 5500 → 9000.
    let deal: Uuid = sqlx::query_scalar(
        "INSERT INTO deal_rules (org_id, name, kind, qty, price) \
         VALUES ($1, 'Any 2 bites for 90', 'n_for_price', 2, 9000) RETURNING id",
    )
    .bind(s.org)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO deal_rule_items (org_id, deal_rule_id, category_id) VALUES ($1, $2, $3)",
    )
    .bind(s.org)
    .bind(deal)
    .bind(s.bakery)
    .execute(&pool)
    .await
    .unwrap();
    let app = app!(pool);
    let (st, v) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/table-orders")
            .set_json(json!({
                "table_id": t, "idempotency_key": Uuid::new_v4(),
                "items": [{"menu_item_id": s.croissant, "quantity": 2},
                          {"menu_item_id": s.cookie, "quantity": 1}],
            })),
        None,
    )
    .await;
    assert!(st < 300, "{st} {v:#}");
    let ticket = v["id"].as_str().unwrap().to_string();
    let (st, o) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{ticket}/settle"))
            .set_json(json!({"shift_id": s.till, "payment_method": "cash"})),
        Some(&s.admin_token()),
    )
    .await;
    assert_eq!(st, 200, "{o:#}");
    assert_eq!(o["subtotal"], 13000, "{o:#}");
    let (discount, server, minor): (i32, Option<i32>, i32) = sqlx::query_as(
        "SELECT d.discount, d.discount_server, i.deal_minor FROM order_deals d \
           JOIN order_deal_lines l ON l.order_deal_id = d.id \
           JOIN order_items i ON i.id = l.order_item_id \
          WHERE d.order_id = $1::uuid",
    )
    .bind(o["id"].as_str().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((discount, server, minor), (2000, Some(2000), 2000));

    // A waiter's bill gets no deal by itself (the till applies them).
    let t2 = table(&pool, &s).await;
    let (_, v) = call(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .set_json(json!({
                "branch_id": s.branch, "table_id": t2,
                "items": [{"menu_item_id": s.croissant, "quantity": 2}],
            })),
        Some(&s.admin_token()),
    )
    .await;
    let (_, o) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!(
                "/open-tickets/{}/settle",
                v["id"].as_str().unwrap()
            ))
            .set_json(json!({"shift_id": s.till, "payment_method": "cash"})),
        Some(&s.admin_token()),
    )
    .await;
    assert_eq!(o["subtotal"], 11000, "{o:#}");
}
