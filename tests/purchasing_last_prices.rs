//! `GET /purchasing/orgs/{org}/last-prices`: purchase orders remember the last
//! price paid per ingredient, the default for a new PO line.
#![allow(unused_imports, dead_code, clippy::too_many_arguments)]
use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::JwtSecret;
use madar_rust::models::UserRole;
use madar_rust::purchasing::handlers::{LastPrice, LastPriceSource, PurchaseOrderFull};
use madar_rust::purchasing::routes;

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}
fn org_admin_token(user_id: Uuid, org_id: Uuid) -> String {
    madar_rust::auth::jwt::create_token(
        &get_secret(),
        user_id,
        Some(org_id),
        UserRole::OrgAdmin,
        None,
        24,
    )
    .unwrap()
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(id)
        .bind(format!("org-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn seed_branch(pool: &PgPool, org_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Branch')")
        .bind(id)
        .bind(org_id)
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn seed_user(pool: &PgPool, org_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES ($1, $2, 'U', $3, 'h', 'org_admin'::user_role)")
        .bind(id).bind(org_id).bind(format!("u-{id}@t.com")).execute(pool).await.unwrap();
    id
}
async fn grant(pool: &PgPool, resource: &str, action: &str) {
    sqlx::query("INSERT INTO role_permissions (role, resource, action, granted) VALUES ('org_admin'::user_role, $1::permission_resource, $2::permission_action, true) ON CONFLICT DO NOTHING")
        .bind(resource).bind(action).execute(pool).await.unwrap();
}
async fn deny_user(pool: &PgPool, user_id: Uuid, resource: &str, action: &str) {
    sqlx::query("INSERT INTO permissions (user_id, resource, action, granted) VALUES ($1, $2::permission_resource, $3::permission_action, false)")
        .bind(user_id).bind(resource).bind(action).execute(pool).await.unwrap();
}
/// A gram-stocked ingredient, with a named pack "sack" of 25 kg.
async fn seed_ingredient_g(pool: &PgPool, org_id: Uuid, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, category_id, cost_per_unit, pack_unit, pack_size) VALUES ($1, $2, $3, 'g'::inventory_unit, ingredient_category_id($2, 'dry'), 3, 'sack', 25000)")
        .bind(id).bind(org_id).bind(name).execute(pool).await.unwrap();
    id
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

struct Ctx {
    org: Uuid,
    branch: Uuid,
    token: String,
    user: Uuid,
}

async fn setup(pool: &PgPool) -> Ctx {
    let org = seed_org(pool).await;
    let branch = seed_branch(pool, org).await;
    let user = seed_user(pool, org).await;
    for a in ["create", "read", "update"] {
        grant(pool, "purchase_orders", a).await;
    }
    Ctx {
        org,
        branch,
        token: org_admin_token(user, org),
        user,
    }
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(get_secret()))
                .configure(routes::configure),
        )
        .await
    };
}

/// Raise a PO with one line and back-date it `days_ago` so the order of
/// purchases is explicit, not left to `now()` within one test.
async fn order<S>(
    app: &S,
    pool: &PgPool,
    c: &Ctx,
    supplier: Option<Uuid>,
    ingredient: Uuid,
    unit: &str,
    qty: f64,
    line_cost: i64,
    days_ago: i32,
) -> PurchaseOrderFull
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let resp = test::call_service(
        app,
        test::TestRequest::post()
            .uri(&format!("/purchasing/branches/{}/orders", c.branch))
            .insert_header(("Authorization", format!("Bearer {}", c.token)))
            .set_json(json!({
                "supplier_id": supplier,
                "lines": [{
                    "org_ingredient_id": ingredient,
                    "purchase_unit": unit,
                    "quantity_ordered": qty,
                    "line_cost": line_cost
                }]
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let po: PurchaseOrderFull = test::read_body_json(resp).await;
    sqlx::query("UPDATE purchase_orders SET created_at = now() - make_interval(days => $2) WHERE id = $1")
        .bind(po.order.id)
        .bind(days_ago)
        .execute(pool)
        .await
        .unwrap();
    po
}

async fn last_prices<S>(app: &S, c: &Ctx, query: &str) -> Vec<LastPrice>
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let resp = test::call_service(
        app,
        test::TestRequest::get()
            .uri(&format!("/purchasing/orgs/{}/last-prices{query}", c.org))
            .insert_header(("Authorization", format!("Bearer {}", c.token)))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    test::read_body_json(resp).await
}

fn for_ingredient(v: &[LastPrice], id: Uuid) -> Option<&LastPrice> {
    v.iter().find(|p| p.org_ingredient_id == id)
}

fn price_in(p: &LastPrice, unit: &str) -> Option<f64> {
    use rust_decimal::prelude::ToPrimitive;
    p.prices
        .iter()
        .find(|u| u.unit == unit)
        .and_then(|u| u.unit_price.to_f64())
}

#[sqlx::test]
async fn last_price_is_per_supplier(pool: PgPool) {
    let app = app!(pool);
    let c = setup(&pool).await;
    let flour = seed_ingredient_g(&pool, c.org, "Flour").await;
    let a = seed_supplier(&pool, c.org, "Alpha").await;
    let b = seed_supplier(&pool, c.org, "Beta").await;

    // Alpha: 2 kg for 90.00 (4500/kg) ten days ago; Beta: 2 kg for 100.00
    // (5000/kg) yesterday.
    let po_a = order(&app, &pool, &c, Some(a), flour, "kg", 2.0, 9000, 10).await;
    let po_b = order(&app, &pool, &c, Some(b), flour, "kg", 2.0, 10000, 1).await;

    let v = last_prices(&app, &c, &format!("?supplier_id={a}")).await;
    let p = for_ingredient(&v, flour).expect("a price for flour");
    assert_eq!(p.source, LastPriceSource::Supplier);
    assert_eq!(p.supplier_id, Some(a));
    assert_eq!(p.supplier_name.as_deref(), Some("Alpha"));
    assert_eq!(p.po_id, po_a.order.id);
    assert_eq!(p.unit, "kg");
    assert_eq!(price_in(p, "kg"), Some(4500.0));

    let v = last_prices(&app, &c, &format!("?supplier_id={b}")).await;
    let p = for_ingredient(&v, flour).unwrap();
    assert_eq!(p.source, LastPriceSource::Supplier);
    assert_eq!(p.po_id, po_b.order.id);
    assert_eq!(price_in(p, "kg"), Some(5000.0));
}

#[sqlx::test]
async fn falls_back_to_any_supplier_and_absent_when_never_bought(pool: PgPool) {
    let app = app!(pool);
    let c = setup(&pool).await;
    let flour = seed_ingredient_g(&pool, c.org, "Flour").await;
    let sugar = seed_ingredient_g(&pool, c.org, "Sugar").await;
    let a = seed_supplier(&pool, c.org, "Alpha").await;
    let b = seed_supplier(&pool, c.org, "Beta").await;
    let never = seed_supplier(&pool, c.org, "Gamma").await;

    order(&app, &pool, &c, Some(a), flour, "kg", 1.0, 4500, 10).await;
    let newest = order(&app, &pool, &c, Some(b), flour, "kg", 1.0, 5000, 1).await;

    // A supplier that never sold flour: the newest price from anyone.
    let v = last_prices(&app, &c, &format!("?supplier_id={never}")).await;
    let p = for_ingredient(&v, flour).unwrap();
    assert_eq!(p.source, LastPriceSource::Any);
    assert_eq!(p.supplier_id, Some(b));
    assert_eq!(p.po_id, newest.order.id);
    assert_eq!(price_in(p, "kg"), Some(5000.0));

    // No supplier asked: the same fallback.
    let v = last_prices(&app, &c, "").await;
    assert_eq!(for_ingredient(&v, flour).unwrap().source, LastPriceSource::Any);

    // Sugar was never purchased: no default at all.
    assert!(for_ingredient(&v, sugar).is_none());

    // The ingredient filter.
    let v = last_prices(&app, &c, &format!("?ingredient_ids={sugar}")).await;
    assert!(v.is_empty());
    let v = last_prices(&app, &c, &format!("?ingredient_ids={sugar},{flour}")).await;
    assert_eq!(v.len(), 1);
}

#[sqlx::test]
async fn converts_the_price_to_every_unit_a_line_can_use(pool: PgPool) {
    let app = app!(pool);
    let c = setup(&pool).await;
    let flour = seed_ingredient_g(&pool, c.org, "Flour").await;
    // 3 kg for 135.00: 4500 piastres per kg.
    order(&app, &pool, &c, None, flour, "kg", 3.0, 13500, 1).await;

    let v = last_prices(&app, &c, "").await;
    let p = for_ingredient(&v, flour).unwrap();
    assert_eq!(p.unit, "kg");
    assert_eq!(price_in(p, "kg"), Some(4500.0));
    assert_eq!(price_in(p, "g"), Some(4.5));
    // The named pack: 25 kg.
    assert_eq!(price_in(p, "sack"), Some(112_500.0));
    // Another measure does not convert: no default in it.
    assert_eq!(price_in(p, "ml"), None);
    assert_eq!(price_in(p, "pcs"), None);

    // Bought by the sack, defaulted per gram.
    let semolina = seed_ingredient_g(&pool, c.org, "Semolina").await;
    order(&app, &pool, &c, None, semolina, "sack", 2.0, 200_000, 1).await;
    let v = last_prices(&app, &c, "").await;
    let p = for_ingredient(&v, semolina).unwrap();
    assert_eq!(p.unit, "sack");
    assert_eq!(price_in(p, "sack"), Some(100_000.0));
    assert_eq!(price_in(p, "g"), Some(4.0));
    assert_eq!(price_in(p, "kg"), Some(4000.0));
}

#[sqlx::test]
async fn a_received_line_wins_at_its_invoiced_price(pool: PgPool) {
    let app = app!(pool);
    let c = setup(&pool).await;
    let flour = seed_ingredient_g(&pool, c.org, "Flour").await;
    let a = seed_supplier(&pool, c.org, "Alpha").await;

    // Ordered at 4500/kg ten days ago, delivered invoiced at 4800/kg.
    let received = order(&app, &pool, &c, Some(a), flour, "kg", 2.0, 9000, 10).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/purchasing/orders/{}/receive", received.order.id))
            .insert_header(("Authorization", format!("Bearer {}", c.token)))
            .set_json(json!({"lines": [{
                "line_id": received.lines[0].id,
                "quantity_received": 2.0,
                "line_cost": 9600
            }]}))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());

    // A newer draft at 4000/kg and a newer still cancelled one at 3000/kg.
    order(&app, &pool, &c, Some(a), flour, "kg", 1.0, 4000, 3).await;
    let cancelled = order(&app, &pool, &c, Some(a), flour, "kg", 1.0, 3000, 1).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/purchasing/orders/{}/cancel", cancelled.order.id))
            .insert_header(("Authorization", format!("Bearer {}", c.token)))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());

    let v = last_prices(&app, &c, &format!("?supplier_id={a}")).await;
    let p = for_ingredient(&v, flour).unwrap();
    assert_eq!(p.po_id, received.order.id);
    assert!(p.received);
    assert!(p.received_at.is_some());
    assert_eq!(p.po_status, "received");
    // The invoiced price, not the ordered one.
    assert_eq!(price_in(p, "kg"), Some(4800.0));
    assert_eq!(price_in(p, "g"), Some(4.8));
}

#[sqlx::test]
async fn an_ordered_line_beats_a_cancelled_one(pool: PgPool) {
    let app = app!(pool);
    let c = setup(&pool).await;
    let flour = seed_ingredient_g(&pool, c.org, "Flour").await;
    let draft = order(&app, &pool, &c, None, flour, "kg", 1.0, 4000, 5).await;
    let cancelled = order(&app, &pool, &c, None, flour, "kg", 1.0, 3000, 1).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/purchasing/orders/{}/cancel", cancelled.order.id))
            .insert_header(("Authorization", format!("Bearer {}", c.token)))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());

    let v = last_prices(&app, &c, "").await;
    let p = for_ingredient(&v, flour).unwrap();
    assert_eq!(p.po_id, draft.order.id);
    assert!(!p.received);
    assert_eq!(price_in(p, "kg"), Some(4000.0));
}

#[sqlx::test]
async fn last_prices_are_tenant_scoped(pool: PgPool) {
    let app = app!(pool);
    let c1 = setup(&pool).await;
    let c2 = setup(&pool).await;
    let flour1 = seed_ingredient_g(&pool, c1.org, "Flour").await;
    let flour2 = seed_ingredient_g(&pool, c2.org, "Flour").await;
    let s2 = seed_supplier(&pool, c2.org, "Theirs").await;
    order(&app, &pool, &c1, None, flour1, "kg", 1.0, 4500, 2).await;
    order(&app, &pool, &c2, Some(s2), flour2, "kg", 1.0, 9900, 1).await;

    // Org 1 sees its own price only, even asking for org 2's ingredient and
    // naming org 2's supplier.
    let v = last_prices(
        &app,
        &c1,
        &format!("?supplier_id={s2}&ingredient_ids={flour1},{flour2}"),
    )
    .await;
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].org_ingredient_id, flour1);
    assert_eq!(v[0].source, LastPriceSource::Any);
    assert_eq!(price_in(&v[0], "kg"), Some(4500.0));

    // Org 1's token on org 2's path: refused.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/purchasing/orgs/{}/last-prices", c2.org))
            .insert_header(("Authorization", format!("Bearer {}", c1.token)))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 403);
}

#[sqlx::test]
async fn last_prices_need_purchasing_read(pool: PgPool) {
    let app = app!(pool);
    let c = setup(&pool).await;
    deny_user(&pool, c.user, "purchase_orders", "read").await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/purchasing/orgs/{}/last-prices", c.org))
            .insert_header(("Authorization", format!("Bearer {}", c.token)))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 403);

    // A malformed ingredient id is the caller's mistake.
    let c2 = setup(&pool).await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/purchasing/orgs/{}/last-prices?ingredient_ids=nope", c2.org))
            .insert_header(("Authorization", format!("Bearer {}", c2.token)))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400);
}
