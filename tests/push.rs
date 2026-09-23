//! `/push/token`: register, rebind to a new user, sign-out, and cleanup on
//! Dawam's phone revocation.

use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::JwtSecret;
use madar_rust::models::UserRole;

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn token_for(user: Uuid, org: Uuid) -> String {
    madar_rust::auth::jwt::create_token(&secret(), user, Some(org), UserRole::Teller, None, 24)
        .unwrap()
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .configure(madar_rust::push::routes::configure),
        )
        .await
    };
}

macro_rules! call {
    ($app:expr, $method:ident, $uri:expr, $token:expr, $body:expr) => {{
        let req = test::TestRequest::$method()
            .uri($uri)
            .insert_header(("Authorization", format!("Bearer {}", $token)))
            .set_json(&$body)
            .to_request();
        test::call_service(&$app, req).await
    }};
}

async fn org_and_user(pool: &PgPool) -> (Uuid, Uuid) {
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Cafe', $2)")
        .bind(org)
        .bind(format!("org-{org}"))
        .execute(pool)
        .await
        .unwrap();
    let u = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, 'A', $3, 'hash', 'teller')",
    )
    .bind(u)
    .bind(org)
    .bind(format!("{u}@test.com"))
    .execute(pool)
    .await
    .unwrap();
    (org, u)
}

async fn live_row(pool: &PgPool, token: &str) -> Option<(Uuid, String, String)> {
    sqlx::query_as(
        "SELECT user_id, app, locale FROM push_devices WHERE token = $1 AND revoked_at IS NULL",
    )
    .bind(token)
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn registering_a_token_creates_a_live_device(pool: PgPool) {
    let (org, u) = org_and_user(&pool).await;
    let app = app!(pool);
    let resp = call!(
        &app,
        put,
        "/push/token",
        token_for(u, org),
        serde_json::json!({"app": "dawam", "token": "tok-1", "locale": "en", "platform": "ios"})
    );
    assert_eq!(resp.status(), 204);
    let row = live_row(&pool, "tok-1").await.unwrap();
    assert_eq!(row, (u, "dawam".to_string(), "en".to_string()));
}

#[sqlx::test]
async fn the_same_token_rebinds_to_whoever_registers_it_next(pool: PgPool) {
    let (org, u1) = org_and_user(&pool).await;
    let (_, u2) = org_and_user(&pool).await;
    let app = app!(pool);
    call!(
        &app,
        put,
        "/push/token",
        token_for(u1, org),
        serde_json::json!({"app": "dawam", "token": "shared", "locale": "ar"})
    );
    call!(
        &app,
        put,
        "/push/token",
        token_for(u2, org),
        serde_json::json!({"app": "dawam", "token": "shared", "locale": "ar"})
    );
    let row = live_row(&pool, "shared").await.unwrap();
    assert_eq!(
        row.0, u2,
        "the token now belongs to whoever registered it last"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM push_devices WHERE token = 'shared'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "no duplicate row left behind");
}

#[sqlx::test]
async fn signing_out_revokes_only_that_device(pool: PgPool) {
    let (org, u) = org_and_user(&pool).await;
    let app = app!(pool);
    call!(
        &app,
        put,
        "/push/token",
        token_for(u, org),
        serde_json::json!({"app": "dawam", "token": "tok-a"})
    );
    call!(
        &app,
        put,
        "/push/token",
        token_for(u, org),
        serde_json::json!({"app": "dashboard", "token": "tok-b"})
    );
    let resp = call!(
        &app,
        delete,
        "/push/token",
        token_for(u, org),
        serde_json::json!({"app": "dawam", "token": "tok-a"})
    );
    assert_eq!(resp.status(), 204);
    assert!(live_row(&pool, "tok-a").await.is_none());
    assert!(live_row(&pool, "tok-b").await.is_some());
}

#[sqlx::test]
async fn revoking_a_dawam_phone_also_revokes_its_push_device(pool: PgPool) {
    let (_org, u) = org_and_user(&pool).await;
    sqlx::query(
        "INSERT INTO push_devices (org_id, user_id, app, token, locale) \
         SELECT org_id, $1, 'dawam', 'phone-tok', 'ar' FROM users WHERE id = $1",
    )
    .bind(u)
    .execute(&pool)
    .await
    .unwrap();
    madar_rust::staff::dawam::revoke_devices(&pool, u)
        .await
        .unwrap();
    assert!(live_row(&pool, "phone-tok").await.is_none());
}
