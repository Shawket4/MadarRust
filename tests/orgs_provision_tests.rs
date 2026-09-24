//! Phase 7: a new org provisioned from a template, with the locked default
//! limits; existing orgs untouched.

use actix_web::{App, http::StatusCode, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::authz::{Cap, Decision, Request, decide};
use madar_rust::models::UserRole;

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn super_admin() -> String {
    create_token(
        &secret(),
        Uuid::new_v4(),
        None,
        UserRole::SuperAdmin,
        None,
        24,
    )
    .unwrap()
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .configure(madar_rust::orgs::routes::configure),
        )
        .await
    };
}

fn body(slug: &str, template: &str) -> Value {
    json!({
        "name": "Drops", "slug": slug, "template": template,
        "branch": {"name": "Zamalek"},
        "owner": {"name": "Mona", "email": format!("{slug}@drops.test"), "password": "long-enough-1", "pin": "482913"}
    })
}

async fn post(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    bearer: &str,
    b: &Value,
) -> (StatusCode, Value) {
    let resp = test::call_service(
        app,
        test::TestRequest::post()
            .uri("/orgs/provision")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(b)
            .to_request(),
    )
    .await;
    let s = resp.status();
    let v = test::read_body(resp).await;
    (s, serde_json::from_slice(&v).unwrap_or(Value::Null))
}

async fn add_user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO users (org_id, name, email, password_hash, role) \
         VALUES ($1, $2, $3, 'h', $4::user_role) RETURNING id",
    )
    .bind(org)
    .bind(format!("{role}-{}", Uuid::new_v4()))
    .bind(format!("{}@t.com", Uuid::new_v4()))
    .bind(role)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn a_cafe_is_provisioned_whole_with_the_new_org_limits(pool: PgPool) {
    let app = app!(pool);
    let (s, v) = post(&app, &super_admin(), &body("drops-cafe", "cafe")).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let org: Uuid = v["org"]["id"].as_str().unwrap().parse().unwrap();
    let owner: Uuid = v["owner_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        v["org"]["tax_rate"].as_f64(),
        Some(0.0),
        "locked: new orgs start at 0% tax"
    );

    let (roles, cafe): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE template_key = 'cafe') FROM org_roles WHERE org_id = $1 AND is_system",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((roles, cafe), (5, 5));
    let methods: i64 =
        sqlx::query_scalar("SELECT count(*) FROM org_payment_methods WHERE org_id = $1")
            .bind(org)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(methods, 5);
    let cats: Vec<String> = sqlx::query_scalar(
        "SELECT slug FROM ingredient_categories WHERE org_id = $1 ORDER BY slug",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(cats, vec!["coffee_bean", "general", "milk"]);

    let owner_eff = madar_rust::authz::require::effective(&pool, owner, None)
        .await
        .unwrap();
    assert!(owner_eff.owner);

    let teller = add_user(&pool, org, "teller").await;
    let t = madar_rust::authz::require::effective(&pool, teller, None)
        .await
        .unwrap();
    let void = t.limits_of(Cap::OrdersVoid);
    assert!(void.own);
    assert_eq!(void.max_age_minutes, Some(10));
    assert!(matches!(
        decide(&t, &Request::of(Cap::OrdersVoid).own(true).age_minutes(3)),
        Decision::Allow
    ));
    assert!(matches!(
        decide(&t, &Request::of(Cap::OrdersVoid).own(true).age_minutes(30)),
        Decision::NeedsApproval(_)
    ));
    assert!(matches!(
        decide(&t, &Request::of(Cap::RefundsCreate).amount(100)),
        Decision::NeedsApproval(_)
    ));
    assert!(!t.can(Cap::BookingsRead), "a café teller has no bookings");

    // A percent limit is in basis points (E2E B-SETUP-4): a branch manager's
    // advances go up to half a month's salary, 5000 bp, not 50 (0.5%).
    let manager = add_user(&pool, org, "branch_manager").await;
    let m = madar_rust::authz::require::effective(&pool, manager, None)
        .await
        .unwrap();
    assert_eq!(m.limits_of(Cap::HrAdvancesDecide).max_percent, Some(5000));

    let waiter = add_user(&pool, org, "waiter").await;
    let w = madar_rust::authz::require::effective(&pool, waiter, None)
        .await
        .unwrap();
    assert!(matches!(
        decide(&w, &Request::of(Cap::RefundsCreate).amount(100)),
        Decision::Deny(_)
    ));

    // A later edit of the global legacy role matrix leaves the template alone.
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) VALUES ('teller', 'bookings', 'read', true)
         ON CONFLICT (role, resource, action) DO UPDATE SET granted = true",
    )
    .execute(&pool)
    .await
    .unwrap();
    let t = madar_rust::authz::require::effective(&pool, teller, None)
        .await
        .unwrap();
    assert!(!t.can(Cap::BookingsRead));
}

#[sqlx::test]
async fn provisioning_is_for_super_admins_and_refuses_bad_input(pool: PgPool) {
    let app = app!(pool);
    let org_admin = create_token(
        &secret(),
        Uuid::new_v4(),
        Some(Uuid::new_v4()),
        UserRole::OrgAdmin,
        None,
        24,
    )
    .unwrap();
    let (s, _) = post(&app, &org_admin, &body("nope", "cafe")).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = post(&app, &super_admin(), &body("pizza-place", "pizzeria")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = post(&app, &super_admin(), &body("rue-one", "restaurant")).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, _) = post(&app, &super_admin(), &body("rue-one", "restaurant")).await;
    assert_eq!(s, StatusCode::CONFLICT, "slug taken");
    let mut short = body("rue-short", "restaurant");
    short["owner"]["password"] = json!("short");
    let (s, _) = post(&app, &super_admin(), &short).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// Locked decision: the defaults are for NEW orgs. An org that grew up under
/// the legacy role defaults keeps unlimited voids.
#[sqlx::test]
async fn an_existing_org_keeps_unlimited_voids(pool: PgPool) {
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Old', $2)")
        .bind(org)
        .bind(format!("old-{org}"))
        .execute(&pool)
        .await
        .unwrap();
    madar_rust::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();
    let teller = add_user(&pool, org, "teller").await;
    let t = madar_rust::authz::require::effective(&pool, teller, None)
        .await
        .unwrap();
    assert_eq!(
        t.limits_of(Cap::OrdersVoid),
        madar_rust::authz::Limits::UNLIMITED
    );
}
