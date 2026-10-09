//! The order path's bill, rung through the live routes, against madar-shared's
//! bill assembly (`madar_money::bill::price_bill_on`).
//!
//! `bill_vectors_tests.rs` rings plain counter sales. These are the pieces a
//! counter vector cannot express, each booked by `create_order_inner` and
//! compared with the crate's answer over the same lines — and with the figure
//! worked out by hand in the comment beside it:
//!
//! - a DISCOUNT (a rule, and an amount a person stated);
//! - a LOYALTY REWARD, and the discount taken after it;
//! - a STAFF DRINK, comped before anything is computed on the subtotal;
//! - a reward, a staff drink and a discount on one bill;
//! - a SPLIT TENDER and its change, and a cash sale's change falling back to
//!   `tendered − total`;
//! - a SERVICE CHARGE, which only a table's bill carries: a counter sale is
//!   priced under a zero rate whatever the branch says, and the ticket settle
//!   (the same `create_order_inner`) prices the dine-in bill with it.
use actix_web::{App, test, web};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_money::bill::{BillDiscount, BillLine, price_bill_on};
use madar_money::tax::{Discount, TaxPolicy};
use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::models::UserRole;
use madar_rust::realtime::hub::BranchEventHub;

mod common;

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(BranchEventHub::new()))
                .configure(madar_rust::orders::routes::configure)
                .configure(madar_rust::tickets::routes::configure),
        )
        .await
    };
}

macro_rules! post {
    ($app:expr, $uri:expr, $bearer:expr, $body:expr) => {{
        let resp = test::call_service(
            &$app,
            test::TestRequest::post()
                .uri($uri)
                .insert_header(("Authorization", format!("Bearer {}", $bearer)))
                .set_json($body)
                .to_request(),
        )
        .await;
        let status = resp.status();
        let body: Value = test::read_body_json(resp).await;
        assert!(status.is_success(), "{} → {status}: {body}", $uri);
        body
    }};
}

/// Tax 14% exclusive, service charge 12% (taxable), a latte at 50.00 and a
/// muffin at 30.00, cash and card.
struct Shop {
    org: Uuid,
    branch: Uuid,
    admin: Uuid,
    jwt: String,
    till: Uuid,
    latte: Uuid,
    muffin: Uuid,
}

const LATTE: i64 = 5000;
const MUFFIN: i64 = 3000;

fn policy(service: bool) -> TaxPolicy {
    TaxPolicy {
        tax_rate: dec!(0.14),
        tax_inclusive: false,
        service_charge_rate: if service { dec!(0.12) } else { Decimal::ZERO },
        service_charge_taxable: true,
    }
}

async fn shop(pool: &PgPool) -> Shop {
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    let org = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO organizations (id, name, slug, tax_rate, tax_inclusive, service_charge_rate) \
         VALUES ($1, 'Bill Org', $2, 0.14, false, 0.12)",
    )
    .bind(org)
    .bind(format!("bill-path-{org}"))
    .execute(pool)
    .await
    .unwrap();
    for (name, cash) in [("cash", true), ("card", false)] {
        sqlx::query(
            "INSERT INTO org_payment_methods (org_id, name, color, icon, is_cash, is_active) \
             VALUES ($1, $2, '#000', 'x', $3, true)",
        )
        .bind(org)
        .bind(name)
        .bind(cash)
        .execute(pool)
        .await
        .unwrap();
    }
    let branch = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO branches (id, org_id, name, code, timezone) \
         VALUES ($1, $2, 'Bill', 'BIL', 'Africa/Cairo')",
    )
    .bind(branch)
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    let admin = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, 'Owner', $3, 'h', 'org_admin'::user_role)",
    )
    .bind(admin)
    .bind(org)
    .bind(format!("{admin}@t.com"))
    .execute(pool)
    .await
    .unwrap();
    let till: Uuid = sqlx::query_scalar(
        "INSERT INTO tills (branch_id, teller_id, status, opening_cash) \
         VALUES ($1, $2, 'open', 0) RETURNING id",
    )
    .bind(branch)
    .bind(admin)
    .fetch_one(pool)
    .await
    .unwrap();
    let cat: Uuid =
        sqlx::query_scalar("INSERT INTO categories (org_id, name) VALUES ($1, 'All') RETURNING id")
            .bind(org)
            .fetch_one(pool)
            .await
            .unwrap();
    let mut items = Vec::new();
    for (name, price) in [("Latte", LATTE), ("Muffin", MUFFIN)] {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO menu_items (org_id, category_id, name, base_price, is_active) \
             VALUES ($1, $2, $3, $4, true) RETURNING id",
        )
        .bind(org)
        .bind(cat)
        .bind(name)
        .bind(price as i32)
        .fetch_one(pool)
        .await
        .unwrap();
        items.push(id);
    }
    let jwt = create_token(&secret(), admin, Some(org), UserRole::OrgAdmin, None, 24).unwrap();
    Shop {
        org,
        branch,
        admin,
        jwt,
        till,
        latte: items[0],
        muffin: items[1],
    }
}

fn line(item: Uuid, qty: i32) -> Value {
    json!({ "menu_item_id": item, "quantity": qty })
}

fn order(s: &Shop, items: Vec<Value>) -> Value {
    json!({
        "branch_id": s.branch, "till_id": s.till, "payment_method": "cash",
        "items": items, "idempotency_key": Uuid::new_v4()
    })
}

/// The order in a response: `POST /orders` answers with the order itself, a
/// settle with it under `order`.
fn order_of(v: &Value) -> &Value {
    if v.get("order").is_some() {
        &v["order"]
    } else {
        v
    }
}

/// `(subtotal, discount, service charge, tax, total)` as booked.
fn booked(o: &Value) -> (i64, i64, i64, i64, i64) {
    let o = order_of(o);
    let f = |k: &str| o[k].as_i64().unwrap_or_else(|| panic!("{k} in {o}"));
    (
        f("subtotal"),
        f("discount_amount"),
        f("service_charge_amount"),
        f("tax_amount"),
        f("total_amount"),
    )
}

/// The crate's answer over `lines`.
fn crate_bill(
    lines: &[BillLine],
    discount: BillDiscount,
    service: bool,
) -> (i64, i64, i64, i64, i64) {
    let b = price_bill_on(lines, None, discount, &policy(service)).breakdown;
    (b.subtotal, b.discount, b.service_charge, b.tax, b.total)
}

fn plain(unit: i64, qty: i64) -> BillLine {
    BillLine {
        charged: unit * qty,
        per_unit: unit,
        reward_units: 0,
        staff_comp: 0,
    }
}

async fn enable_rewards(pool: &PgPool, s: &Shop, balance: i32) -> Uuid {
    sqlx::query(
        "INSERT INTO loyalty_settings \
            (org_id, branch_id, enabled, mode, earn_piastres_per_point, default_reward_cost, \
             require_otp, stamp_per_line_item) \
         VALUES ($1, NULL, true, 'points', 1000, 100, false, false)",
    )
    .bind(s.org)
    .execute(pool)
    .await
    .unwrap();
    for (item, cost) in [(s.latte, 5), (s.muffin, 3)] {
        sqlx::query(
            "INSERT INTO loyalty_reward_items (org_id, menu_item_id, cost_currency, cost_amount) \
             VALUES ($1, $2, 'points', $3)",
        )
        .bind(s.org)
        .bind(item)
        .bind(cost)
        .execute(pool)
        .await
        .unwrap();
    }
    let member = common::members::seed_loyalty_member(
        pool,
        s.org,
        "+201000000777",
        "Ali",
        "Mbillpath00000000000A",
    )
    .await;
    sqlx::query(
        "INSERT INTO loyalty_transactions (org_id, customer_id, branch_id, kind, currency, points) \
         VALUES ($1, $2, $3, 'adjust', 'points', $4)",
    )
    .bind(s.org)
    .bind(member)
    .bind(s.branch)
    .bind(balance)
    .execute(pool)
    .await
    .unwrap();
    member
}

async fn enable_staff_pool(pool: &PgPool, s: &Shop) {
    sqlx::query(
        "INSERT INTO staff_pool_settings (org_id, branch_id, enabled, daily_allowance, eligible_item_ids) \
         VALUES ($1, NULL, true, 10, $2)",
    )
    .bind(s.org)
    .bind(vec![s.latte])
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO user_overrides (org_id, user_id, capability_id, effect, reason) \
         VALUES ($1, $2, 223, 'allow', 'test')",
    )
    .bind(s.org)
    .bind(s.admin)
    .execute(pool)
    .await
    .unwrap();
}

fn staff_line(item: Uuid) -> Value {
    json!({
        "menu_item_id": item, "quantity": 1,
        "staff_drink": { "id": Uuid::new_v4(), "note": "Sara, on shift" }
    })
}

#[sqlx::test]
async fn a_discount_rule_and_a_stated_amount_are_the_crates(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let lines = [plain(LATTE, 2), plain(MUFFIN, 1)];

    // 2 × 50.00 + 30.00 = 130.00; 10% off = 13.00; tax 14% of 117.00 = 16.38.
    let mut body = order(&s, vec![line(s.latte, 2), line(s.muffin, 1)]);
    body["discount_type"] = json!("percentage");
    body["discount_value"] = json!(0.10);
    let got = booked(&post!(app, "/orders", s.jwt, &body));
    assert_eq!(got, (13_000, 1_300, 0, 1_638, 13_338));
    assert_eq!(
        got,
        crate_bill(
            &lines,
            BillDiscount::Rule(Discount::Percentage(dec!(0.10))),
            false
        )
    );

    // A manager's 5.00 off that no rule expresses: the till's word, taxed after.
    let mut body = order(&s, vec![line(s.latte, 2), line(s.muffin, 1)]);
    body["discount_amount"] = json!(500);
    let got = booked(&post!(app, "/orders", s.jwt, &body));
    assert_eq!(got, (13_000, 500, 0, 1_750, 14_250));
    assert_eq!(got, crate_bill(&lines, BillDiscount::Stated(500), false));

    // A stated amount larger than the bill is clamped to it.
    let mut body = order(&s, vec![line(s.muffin, 1)]);
    body["discount_amount"] = json!(99_999);
    let got = booked(&post!(app, "/orders", s.jwt, &body));
    assert_eq!(got, (3_000, 3_000, 0, 0, 0));
}

#[sqlx::test]
async fn a_reward_comes_off_before_the_discount(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let member = enable_rewards(&pool, &s, 20).await;

    // One of the two lattes is the reward: 130.00 − 50.00 = 80.00; the 10%
    // rule is taken on what is left (8.00), then tax 14% of 72.00 = 10.08.
    let mut body = order(&s, vec![line(s.latte, 2), line(s.muffin, 1)]);
    body["loyalty_customer_id"] = json!(member);
    body["loyalty_redemptions"] = json!([{ "item_index": 0, "units": 1 }]);
    body["discount_type"] = json!("percentage");
    body["discount_value"] = json!(0.10);
    let resp = post!(app, "/orders", s.jwt, &body);
    let got = booked(&resp);
    assert_eq!(got, (8_000, 800, 0, 1_008, 8_208));
    let lines = [
        BillLine {
            reward_units: 1,
            ..plain(LATTE, 2)
        },
        plain(MUFFIN, 1),
    ];
    assert_eq!(
        got,
        crate_bill(
            &lines,
            BillDiscount::Rule(Discount::Percentage(dec!(0.10))),
            false
        )
    );
    // The points moved: 20 − 5.
    let balance: i32 =
        sqlx::query_scalar("SELECT points_balance FROM loyalty_customers WHERE id = $1")
            .bind(member)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(balance, 15);
}

#[sqlx::test]
async fn a_staff_drink_is_comped_before_the_bill_is_priced(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    enable_staff_pool(&pool, &s).await;

    // The latte (no sizes, no groups) is free on the pool: its price is the
    // free amount. The muffin is paid: 30.00, tax 4.20.
    let body = order(&s, vec![staff_line(s.latte), line(s.muffin, 1)]);
    let got = booked(&post!(app, "/orders", s.jwt, &body));
    assert_eq!(got, (3_000, 0, 0, 420, 3_420));
    let lines = [
        BillLine {
            staff_comp: LATTE,
            ..plain(LATTE, 1)
        },
        plain(MUFFIN, 1),
    ];
    assert_eq!(
        got,
        crate_bill(&lines, BillDiscount::Rule(Discount::None), false)
    );
}

#[sqlx::test]
async fn a_reward_a_staff_drink_and_a_discount_on_one_bill(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    enable_staff_pool(&pool, &s).await;
    let member = enable_rewards(&pool, &s, 10).await;

    // A staff latte (comped 50.00), a muffin taken as a reward (30.00), a
    // paid latte: 50.00 left; 10% off = 5.00; tax 14% of 45.00 = 6.30.
    let mut body = order(
        &s,
        vec![staff_line(s.latte), line(s.muffin, 1), line(s.latte, 1)],
    );
    body["loyalty_customer_id"] = json!(member);
    body["loyalty_redemptions"] = json!([{ "item_index": 1, "units": 1 }]);
    body["discount_type"] = json!("percentage");
    body["discount_value"] = json!(0.10);
    let got = booked(&post!(app, "/orders", s.jwt, &body));
    assert_eq!(got, (5_000, 500, 0, 630, 5_130));
    let lines = [
        BillLine {
            staff_comp: LATTE,
            ..plain(LATTE, 1)
        },
        BillLine {
            reward_units: 1,
            ..plain(MUFFIN, 1)
        },
        plain(LATTE, 1),
    ];
    assert_eq!(
        got,
        crate_bill(
            &lines,
            BillDiscount::Rule(Discount::Percentage(dec!(0.10))),
            false
        )
    );
}

#[sqlx::test]
async fn a_split_tender_records_its_change_over_the_cash_legs(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;

    // 50.00 + 30.00 = 80.00, tax 11.20 → 91.20: 50.00 on the card, 41.20 in
    // cash, paid with a 50.00 note → 8.80 change.
    let mut body = order(&s, vec![line(s.latte, 1), line(s.muffin, 1)]);
    body["payment_splits"] = json!([
        { "method": "card", "amount": 5_000 },
        { "method": "cash", "amount": 4_120 }
    ]);
    body["amount_tendered"] = json!(5_000);
    body["total_amount"] = json!(9_120);
    let resp = post!(app, "/orders", s.jwt, &body);
    assert_eq!(booked(&resp), (8_000, 0, 0, 1_120, 9_120));
    let o = order_of(&resp);
    assert_eq!(o["amount_tendered"], json!(5_000));
    assert_eq!(o["change_given"], json!(880));
    let legs = [
        madar_money::bill::Leg {
            amount: 5_000,
            is_cash: false,
        },
        madar_money::bill::Leg {
            amount: 4_120,
            is_cash: true,
        },
    ];
    assert_eq!(
        madar_money::bill::recorded_tender(&legs, Some(5_000), None, 9_120),
        (Some(5_000), Some(880))
    );
    let paid: Vec<(String, i32)> = sqlx::query_as(
        "SELECT method::text, amount FROM order_payments WHERE order_id = $1::uuid ORDER BY amount DESC",
    )
    .bind(o["id"].as_str().unwrap())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(paid, [("card".into(), 5_000), ("cash".into(), 4_120)]);

    // Legs that do not add up to the bill are refused, naming both figures.
    let mut body = order(&s, vec![line(s.latte, 1), line(s.muffin, 1)]);
    body["payment_splits"] = json!([
        { "method": "card", "amount": 5_000 },
        { "method": "cash", "amount": 4_000 }
    ]);
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/orders")
            .insert_header(("Authorization", format!("Bearer {}", s.jwt)))
            .set_json(&body)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let err: Value = test::read_body_json(resp).await;
    assert!(
        err.to_string().contains("9000") && err.to_string().contains("9120"),
        "{err}"
    );

    // A plain cash sale with no change sent: tendered − total.
    let mut body = order(&s, vec![line(s.latte, 1), line(s.muffin, 1)]);
    body["amount_tendered"] = json!(10_000);
    let resp = post!(app, "/orders", s.jwt, &body);
    assert_eq!(order_of(&resp)["change_given"], json!(880));
    assert_eq!(
        madar_money::bill::recorded_tender(&[], Some(10_000), None, 9_120),
        (Some(10_000), Some(880))
    );
}

#[sqlx::test]
async fn a_service_charge_is_on_the_table_bill_only(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let lines = [plain(LATTE, 2)];

    // At the counter the branch's 12% is not charged: 100.00, tax 14.00.
    let got = booked(&post!(
        app,
        "/orders",
        s.jwt,
        &order(&s, vec![line(s.latte, 2)])
    ));
    assert_eq!(got, (10_000, 0, 0, 1_400, 11_400));
    assert_eq!(
        got,
        crate_bill(&lines, BillDiscount::Rule(Discount::None), false)
    );

    // The same two lattes on a table: 12.00 service, tax 14% of 112.00.
    let ticket = post!(
        app,
        "/open-tickets",
        s.jwt,
        &json!({ "branch_id": s.branch, "items": [line(s.latte, 2)] })
    );
    let id = ticket["id"].as_str().unwrap().to_string();
    let got = booked(&post!(
        app,
        &format!("/open-tickets/{id}/settle"),
        s.jwt,
        &json!({ "till_id": s.till, "payment_method": "cash" })
    ));
    assert_eq!(got, (10_000, 0, 1_200, 1_568, 12_768));
    assert_eq!(
        got,
        crate_bill(&lines, BillDiscount::Rule(Discount::None), true)
    );

    // A table bill with a 10% rule: the charge is on what is left.
    let ticket = post!(
        app,
        "/open-tickets",
        s.jwt,
        &json!({ "branch_id": s.branch, "items": [line(s.latte, 2)] })
    );
    let id = ticket["id"].as_str().unwrap().to_string();
    let got = booked(&post!(
        app,
        &format!("/open-tickets/{id}/settle"),
        s.jwt,
        &json!({
            "till_id": s.till, "payment_method": "cash",
            "discount_type": "percentage", "discount_value": 0.10
        })
    ));
    assert_eq!(got, (10_000, 1_000, 1_080, 1_411, 11_491));
    assert_eq!(
        got,
        crate_bill(
            &lines,
            BillDiscount::Rule(Discount::Percentage(dec!(0.10))),
            true
        )
    );
}
