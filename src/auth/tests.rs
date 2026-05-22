use actix_web::{test, App, web};
use sqlx::PgPool;
use uuid::Uuid;
use serde_json::json;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::auth::routes;
use crate::auth::handlers::{LoginResponse, MeResponse, AuthPermissionsResponse};

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn generate_token(user_id: Uuid, org_id: Option<Uuid>, role: UserRole) -> String {
    crate::auth::jwt::create_token(&get_secret(), user_id, org_id, role, None, 24).unwrap()
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let org_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Test Org', 'test-auth-org')", org_id)
        .execute(pool)
        .await
        .unwrap();
    org_id
}

#[sqlx::test]
async fn test_login_email_password_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = Uuid::new_v4();
    let hash = bcrypt::hash("password123", bcrypt::DEFAULT_COST).unwrap();

    sqlx::query!("INSERT INTO users (id, org_id, name, role, email, password_hash) VALUES ($1, $2, 'Admin', 'org_admin'::user_role, 'admin@test.com', $3)", user_id, org_id, hash)
        .execute(&pool).await.unwrap();

    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({
            "email": "admin@test.com",
            "password": "password123"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let body: LoginResponse = test::read_body_json(resp).await;
    assert_eq!(body.user.email.unwrap(), "admin@test.com");
    assert!(!body.token.is_empty());
}

#[sqlx::test]
async fn test_login_email_password_failure(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = Uuid::new_v4();
    let hash = bcrypt::hash("password123", bcrypt::DEFAULT_COST).unwrap();

    sqlx::query!("INSERT INTO users (id, org_id, name, role, email, password_hash) VALUES ($1, $2, 'Admin', 'org_admin'::user_role, 'admin@test.com', $3)", user_id, org_id, hash)
        .execute(&pool).await.unwrap();

    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({
            "email": "admin@test.com",
            "password": "wrongpassword"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn test_login_pin_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = Uuid::new_v4();
    let hash = bcrypt::hash("1234", bcrypt::DEFAULT_COST).unwrap();

    sqlx::query!("INSERT INTO users (id, org_id, name, role, pin_hash) VALUES ($1, $2, 'Teller One', 'teller'::user_role, $3)", user_id, org_id, hash)
        .execute(&pool).await.unwrap();

    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({
            "name": "Teller One",
            "pin": "1234"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let body: LoginResponse = test::read_body_json(resp).await;
    assert_eq!(body.user.name, "Teller One");
}

#[sqlx::test]
async fn test_login_pin_failure(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = Uuid::new_v4();
    let hash = bcrypt::hash("1234", bcrypt::DEFAULT_COST).unwrap();

    sqlx::query!("INSERT INTO users (id, org_id, name, role, pin_hash) VALUES ($1, $2, 'Teller One', 'teller'::user_role, $3)", user_id, org_id, hash)
        .execute(&pool).await.unwrap();

    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({
            "name": "Teller One",
            "pin": "0000"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn test_login_disabled_account(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = Uuid::new_v4();
    let hash = bcrypt::hash("password123", bcrypt::DEFAULT_COST).unwrap();

    sqlx::query!("INSERT INTO users (id, org_id, name, role, email, password_hash, is_active) VALUES ($1, $2, 'Admin', 'org_admin'::user_role, 'dis@test.com', $3, false)", user_id, org_id, hash)
        .execute(&pool).await.unwrap();

    let req = test::TestRequest::post()
        .uri("/auth/login")
        .set_json(&json!({
            "email": "dis@test.com",
            "password": "password123"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn test_me_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO users (id, org_id, name, role, email, password_hash) VALUES ($1, $2, 'Me User', 'org_admin'::user_role, 'me@test.com', 'h')", user_id, org_id)
        .execute(&pool).await.unwrap();

    let token = generate_token(user_id, Some(org_id), UserRole::OrgAdmin);

    let req = test::TestRequest::get()
        .uri("/auth/me")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let body: MeResponse = test::read_body_json(resp).await;
    assert_eq!(body.user.name, "Me User");
}

#[sqlx::test]
async fn test_permissions_super_admin(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let user_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO users (id, name, role, email, password_hash) VALUES ($1, 'Super Admin', 'super_admin'::user_role, 'super@test.com', 'h')", user_id)
        .execute(&pool).await.unwrap();

    let token = generate_token(user_id, None, UserRole::SuperAdmin);

    let req = test::TestRequest::get()
        .uri("/auth/permissions")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let body: AuthPermissionsResponse = test::read_body_json(resp).await;
    assert!(!body.permissions.is_empty());
    for perm in body.permissions {
        assert!(perm.granted, "SuperAdmin should have all permissions granted");
    }
}

#[sqlx::test]
async fn test_permissions_with_overrides(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO users (id, org_id, name, role, pin_hash) VALUES ($1, $2, 'Teller Perm', 'teller'::user_role, 'h')", user_id, org_id)
        .execute(&pool).await.unwrap();

    // Default: teller cannot create orgs. Let's make sure role defaults exist.
    sqlx::query!("INSERT INTO role_permissions (role, resource, action, granted) VALUES ('teller'::user_role, 'orgs'::permission_resource, 'create'::permission_action, false)")
        .execute(&pool).await.unwrap();

    sqlx::query!("INSERT INTO permissions (user_id, resource, action, granted) VALUES ($1, 'orgs'::permission_resource, 'create'::permission_action, true)", user_id)
        .execute(&pool).await.unwrap();

    let token = generate_token(user_id, Some(org_id), UserRole::Teller);

    let req = test::TestRequest::get()
        .uri("/auth/permissions")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let body: AuthPermissionsResponse = test::read_body_json(resp).await;
    let perm = body.permissions.iter().find(|p| p.resource == "orgs" && p.action == "create").unwrap();
    assert!(perm.granted, "Override should be applied");
}
