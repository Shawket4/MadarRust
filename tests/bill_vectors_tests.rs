//! The SERVER's bill, rung through `POST /orders`, against madar-shared's bill
//! vectors (`madar_money::vectors::BILL`).
//!
//! `madar_money::bill` is the assembly the till runs (`price_cart`) and the
//! pieces this server calls (`net_line`, `discount_on`, the tender rules). The
//! composition on this side still lives in `create_order_inner`, interleaved
//! with the catalogue reads — so this test rings every vector the live route
//! can express (plain lines, a discount rule or a stated amount, a counter
//! sale's policy: no service charge) and checks the order the server BOOKS
//! against the figures the vector file states. A reorder of the steps in
//! `create_order_inner` that the crate did not make fails here.
#![allow(unused_imports)]
use std::collections::HashMap;
use std::str::FromStr;

use actix_web::{App, test, web};
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use madar_money::bill::vectors::{BillVector, Vectors};
use madar_rust::auth::jwt::JwtSecret;
use madar_rust::models::UserRole;
use madar_rust::orders::handlers::{CreateOrderRequest, OrderFull, OrderItemInput};
use madar_rust::orders::routes;

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

async fn ctx(pool: &PgPool) -> (Uuid, Uuid, String, Uuid, Uuid) {
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Bill Org', $2)")
        .bind(org)
        .bind(format!("bill-org-{org}"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) \
         VALUES ($1, 'cash', '{}', 'emerald', 'payments_outlined', true, true)",
    )
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    let branch = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Bill Branch')")
        .bind(branch)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    let user = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, 'Owner', $3, 'hash', 'org_admin'::user_role)",
    )
    .bind(user)
    .bind(org)
    .bind(format!("owner-{user}@test.com"))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ('org_admin'::user_role, 'orders'::permission_resource, 'create'::permission_action, true) \
         ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .unwrap();
    let till = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO tills (id, branch_id, teller_id, status, opening_cash) VALUES ($1, $2, $3, 'open', 10000)",
    )
    .bind(till)
    .bind(branch)
    .bind(user)
    .execute(pool)
    .await
    .unwrap();
    let cat = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'Cat')")
        .bind(cat)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    let token = madar_rust::auth::jwt::create_token(
        &secret(),
        user,
        Some(org),
        UserRole::OrgAdmin,
        None,
        24,
    )
    .unwrap();
    (org, branch, token, till, cat)
}

/// Whether the live route can ring this vector as a counter sale.
fn ringable(v: &BillVector) -> bool {
    let plain_lines = !v.lines.is_empty()
        && v.lines.iter().all(|l| {
            l.reward_units == 0
                && l.staff_comp == 0
                && l.per_unit > 0
                && l.charged % l.per_unit == 0
                && l.charged / l.per_unit > 0
        });
    let counter_policy = v.policy.service_charge_rate == "0";
    // The live route reads a percentage above 1 as the legacy 0-100 spelling
    // and refuses a negative one; a stated amount below zero is refused too.
    let discount = match v.discount.kind.as_str() {
        "none" => true,
        "percentage" | "fixed" => {
            let d = Decimal::from_str(&v.discount.value).unwrap();
            d > Decimal::ZERO && (v.discount.kind == "fixed" || d <= Decimal::ONE)
        }
        "stated" => v.discount.value.parse::<i64>().unwrap() >= 0,
        _ => false,
    };
    plain_lines && counter_policy && discount && v.refusal.is_none()
}

#[sqlx::test]
async fn the_servers_booked_bill_is_the_crates(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .configure(routes::configure),
    )
    .await;
    let (org, branch, token, till, cat) = ctx(&pool).await;

    let vectors: Vectors = serde_json::from_str(madar_money::vectors::BILL).unwrap();
    let mut items: HashMap<i64, Uuid> = HashMap::new();
    let mut rung = 0;
    for v in vectors.bills.iter().filter(|v| ringable(v)) {
        sqlx::query(
            "UPDATE organizations SET tax_rate = $2::numeric, tax_inclusive = $3 WHERE id = $1",
        )
        .bind(org)
        .bind(&v.policy.tax_rate)
        .bind(v.policy.tax_inclusive)
        .execute(&pool)
        .await
        .unwrap();

        let mut lines = Vec::new();
        for l in &v.lines {
            let item = match items.get(&l.per_unit) {
                Some(id) => *id,
                None => {
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) \
                         VALUES ($1, $2, $3, $4, $5, true)",
                    )
                    .bind(id)
                    .bind(org)
                    .bind(cat)
                    .bind(format!("Item {}", l.per_unit))
                    .bind(l.per_unit as i32)
                    .execute(&pool)
                    .await
                    .unwrap();
                    items.insert(l.per_unit, id);
                    id
                }
            };
            lines.push(OrderItemInput {
                menu_item_id: Some(item),
                quantity: (l.charged / l.per_unit) as i32,
                ..Default::default()
            });
        }
        let mut body = CreateOrderRequest {
            branch_id: branch,
            till_id: till,
            payment_method: "cash".to_string(),
            items: lines,
            ..Default::default()
        };
        match v.discount.kind.as_str() {
            "percentage" | "fixed" => {
                body.discount_type = Some(v.discount.kind.clone());
                body.discount_value = Some(Decimal::from_str(&v.discount.value).unwrap());
            }
            "stated" => {
                body.discount_amount = Some(v.discount.value.parse().unwrap());
            }
            _ => {}
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
        assert!(status.is_success(), "{v:?} was refused: {status}");
        let of: OrderFull = test::read_body_json(resp).await;
        let o = &of.order;
        let got = (
            i64::from(o.subtotal),
            i64::from(o.discount_amount),
            i64::from(o.service_charge_amount),
            i64::from(o.tax_amount),
            i64::from(o.total_amount),
        );
        let want = (
            v.subtotal,
            v.discount_amount,
            v.service_charge,
            v.tax,
            v.total,
        );
        assert_eq!(
            got, want,
            "the server booked a different bill than the vector: {v:?}"
        );
        rung += 1;
    }
    assert!(rung >= 50, "rang only {rung} vectors");
}
