//! A sale queued offline must land even when the catalogue moved on before the
//! till synced: the item was deleted or switched off, or its tender was. The
//! bill was rung and the money taken; refusing the replay cannot un-take it, it
//! only loses the sale (the till dead-letters a rejected op). Same principle as
//! the dead discount preset (`sync::handlers`, "accept and flag").
//!
//! A LIVE sale of a deleted item is still refused: a till naming an item that
//! no longer exists, for a sale happening now, is told to re-sync.

mod common;

use actix_web::{App, test, web};
use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::models::UserRole;
use madar_rust::realtime::hub::BranchEventHub;
use sqlx::PgPool;
use uuid::Uuid;

fn secret() -> JwtSecret {
    JwtSecret("replay-deleted-catalog-test-secret".into())
}
fn token(uid: Uuid, org: Uuid, role: UserRole) -> String {
    create_token(&secret(), uid, Some(org), role, None, 1).unwrap()
}

struct Shop {
    org: Uuid,
    branch: Uuid,
    teller: Uuid,
    till: Uuid,
    item: Uuid,
}

async fn shop(pool: &PgPool) -> Shop {
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(org)
        .bind(format!("org-{}", org.simple()))
        .execute(pool)
        .await
        .unwrap();
    let branch = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name, code) VALUES ($1, $2, 'Main', $3)")
        .bind(branch)
        .bind(org)
        .bind(org.simple().to_string()[..6].to_uppercase())
        .execute(pool)
        .await
        .unwrap();
    let teller = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, name, role, org_id, pin_hash) VALUES ($1, 'Teller', 'teller', $2, 'x')",
    )
    .bind(teller)
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(teller)
        .bind(branch)
        .execute(pool)
        .await
        .unwrap();
    let till: Uuid = sqlx::query_scalar(
        "INSERT INTO tills (branch_id, teller_id, status, opening_cash) \
         VALUES ($1, $2, 'open', 0) RETURNING id",
    )
    .bind(branch)
    .bind(teller)
    .fetch_one(pool)
    .await
    .unwrap();
    let item = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO menu_items (id, org_id, name, base_price) VALUES ($1, $2, 'Latte', 500)",
    )
    .bind(item)
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, color, icon, is_cash, is_active) \
         VALUES ($1, 'cash', '#000', 'cash', true, true)",
    )
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    for (r, a) in [
        ("orders", "create"),
        ("orders", "read"),
        ("order_items", "create"),
        ("payments", "create"),
    ] {
        sqlx::query(
            "INSERT INTO role_permissions (role, resource, action, granted) \
             VALUES ('teller', $1::permission_resource, $2::permission_action, true) \
             ON CONFLICT DO NOTHING",
        )
        .bind(r)
        .bind(a)
        .execute(pool)
        .await
        .unwrap();
    }
    Shop {
        org,
        branch,
        teller,
        till,
        item,
    }
}

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

fn sale(s: &Shop, idempotency: Uuid) -> serde_json::Value {
    serde_json::json!({
        "op": "create_order",
        "teller_id": s.teller,
        "request": {
            "branch_id": s.branch,
            "shift_id": s.till,
            "payment_method": "cash",
            "idempotency_key": idempotency,
            "items": [{ "menu_item_id": s.item, "quantity": 2, "unit_price": 500 }],
            "total_amount": 1140
        }
    })
}

async fn post(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    uri: &str,
    bearer: &str,
    body: &serde_json::Value,
) -> (u16, String) {
    let resp = test::call_service(
        app,
        test::TestRequest::post()
            .uri(uri)
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(body)
            .to_request(),
    )
    .await;
    let status = resp.status().as_u16();
    let body = test::read_body(resp).await;
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// What the order recorded for the item: (quantity, unit price, item name).
async fn recorded_line(pool: &PgPool, branch: Uuid) -> Option<(i32, i32, String)> {
    sqlx::query_as(
        "SELECT oi.quantity, oi.unit_price, oi.item_name FROM order_items oi \
           JOIN orders o ON o.id = oi.order_id WHERE o.branch_id = $1",
    )
    .bind(branch)
    .fetch_optional(pool)
    .await
    .unwrap()
}

/// The owner's review flags recorded for this org's replays.
async fn flags(pool: &PgPool, org: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT capability FROM authz_replay_flags WHERE org_id = $1 ORDER BY capability",
    )
    .bind(org)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn a_queued_sale_of_an_item_deleted_since_still_lands(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    // Sold offline; the owner deleted the item on the dashboard before the
    // till came back online.
    sqlx::query("UPDATE menu_items SET deleted_at = now() WHERE id = $1")
        .bind(s.item)
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = post(
        &app,
        "/sync/replay",
        &token(s.teller, s.org, UserRole::Teller),
        &sale(&s, Uuid::new_v4()),
    )
    .await;
    assert!((200..300).contains(&status), "{status}: {body}");
    assert_eq!(
        recorded_line(&pool, s.branch).await,
        Some((2, 500, "Latte".to_string())),
        "the line as rung, under the item's last name"
    );
    assert_eq!(
        flags(&pool, s.org).await,
        vec!["orders.create:menu_item_deleted".to_string()],
        "and the owner is told"
    );
}

#[sqlx::test]
async fn a_queued_sale_of_an_item_switched_off_since_still_lands(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    sqlx::query("UPDATE menu_items SET is_active = false WHERE id = $1")
        .bind(s.item)
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = post(
        &app,
        "/sync/replay",
        &token(s.teller, s.org, UserRole::Teller),
        &sale(&s, Uuid::new_v4()),
    )
    .await;
    assert!((200..300).contains(&status), "{status}: {body}");
    assert_eq!(recorded_line(&pool, s.branch).await.map(|l| l.0), Some(2));
}

#[sqlx::test]
async fn a_queued_sale_paid_by_a_tender_switched_off_since_still_lands(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    sqlx::query("UPDATE org_payment_methods SET is_active = false WHERE org_id = $1")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = post(
        &app,
        "/sync/replay",
        &token(s.teller, s.org, UserRole::Teller),
        &sale(&s, Uuid::new_v4()),
    )
    .await;
    assert!((200..300).contains(&status), "{status}: {body}");
    assert_eq!(
        flags(&pool, s.org).await,
        vec!["orders.create:payment_method_inactive".to_string()]
    );
}

#[sqlx::test]
async fn a_queued_sale_with_an_extra_switched_off_since_still_lands(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    // An optional field that has since been switched off.
    let extra = common::legacy_catalog::optional_field(
        &pool,
        common::legacy_catalog::Optional::new(s.item, "Extra shot", 100).active(false),
    )
    .await;
    let mut op = sale(&s, Uuid::new_v4());
    op["request"]["items"][0]["optional_field_ids"] = serde_json::json!([extra]);
    // (500 + the extra's 100) × 2, + 14%.
    op["request"]["total_amount"] = serde_json::json!(1368);
    let (status, body) = post(
        &app,
        "/sync/replay",
        &token(s.teller, s.org, UserRole::Teller),
        &op,
    )
    .await;
    assert!((200..300).contains(&status), "{status}: {body}");
}

#[sqlx::test]
async fn a_live_sale_of_a_deleted_item_is_still_refused(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    sqlx::query("UPDATE menu_items SET deleted_at = now() WHERE id = $1")
        .bind(s.item)
        .execute(&pool)
        .await
        .unwrap();
    let (status, _) = post(
        &app,
        "/orders",
        &token(s.teller, s.org, UserRole::Teller),
        &sale(&s, Uuid::new_v4())["request"],
    )
    .await;
    assert!(
        (400..500).contains(&status),
        "a sale happening now cannot name a deleted item: {status}"
    );
    assert_eq!(recorded_line(&pool, s.branch).await, None);
}
