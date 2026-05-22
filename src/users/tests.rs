use actix_web::{test, App, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::{UserRole, UserPublic};
use crate::users::routes;
use crate::users::handlers::{CreateUserResponse, UserBranch};

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn generate_token(user_id: Uuid, org_id: Option<Uuid>, role: UserRole) -> String {
    crate::auth::jwt::create_token(&get_secret(), user_id, org_id, role, None, 24).unwrap()
}

fn generate_super_admin_token(user_id: Uuid) -> String {
    generate_token(user_id, None, UserRole::SuperAdmin)
}

fn generate_org_admin_token(user_id: Uuid, org_id: Uuid) -> String {
    generate_token(user_id, Some(org_id), UserRole::OrgAdmin)
}

fn generate_branch_manager_token(user_id: Uuid, org_id: Uuid) -> String {
    generate_token(user_id, Some(org_id), UserRole::BranchManager)
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let org_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Test Org', 'test-org')", org_id)
        .execute(pool)
        .await
        .unwrap();
    org_id
}

async fn seed_branch(pool: &PgPool, org_id: Uuid) -> Uuid {
    let branch_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Test Branch')", branch_id, org_id)
        .execute(pool).await.unwrap();
    branch_id
}

async fn grant_permission(pool: &PgPool, role: &str, resource: &str, action: &str) {
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) VALUES ($1::user_role, $2::permission_resource, $3::permission_action, true) ON CONFLICT DO NOTHING"
    )
    .bind(role)
    .bind(resource)
    .bind(action)
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test]
async fn test_create_user_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "org_admin", "users", "create").await;

    let admin_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO users (id, org_id, name, role, email, password_hash) VALUES ($1, $2, 'Admin', 'org_admin'::user_role, 'admin@t.com', 'h')", admin_id, org_id).execute(&pool).await.unwrap();
    let token = generate_org_admin_token(admin_id, org_id);

    let branch_id = seed_branch(&pool, org_id).await;

    let req = test::TestRequest::post()
        .uri("/users")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "org_id": org_id,
            "name": "New Teller",
            "role": "teller",
            "pin": "1234",
            "branch_ids": [branch_id]
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let body: CreateUserResponse = test::read_body_json(resp).await;
    assert_eq!(body.user.name, "New Teller");
    assert_eq!(body.user.role, UserRole::Teller);
}

#[sqlx::test]
async fn test_create_user_forbidden_promotion(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "org_admin", "users", "create").await;

    let token = generate_org_admin_token(Uuid::new_v4(), org_id);

    let req = test::TestRequest::post()
        .uri("/users")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "org_id": org_id,
            "name": "Sneaky Admin",
            "role": "super_admin",
            "email": "hacker@test.com",
            "password": "pass"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn test_create_user_teller_requires_pin(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "org_admin", "users", "create").await;

    let token = generate_org_admin_token(Uuid::new_v4(), org_id);

    let req = test::TestRequest::post()
        .uri("/users")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "org_id": org_id,
            "name": "No Pin Teller",
            "role": "teller"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn test_list_users(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "org_admin", "users", "read").await;

    sqlx::query!("INSERT INTO users (id, org_id, name, role, email, password_hash) VALUES ($1, $2, 'User 1', 'org_admin'::user_role, 'u1@test.com', 'h')", Uuid::new_v4(), org_id)
        .execute(&pool).await.unwrap();
    sqlx::query!("INSERT INTO users (id, org_id, name, role, email, password_hash) VALUES ($1, $2, 'User 2', 'org_admin'::user_role, 'u2@test.com', 'h')", Uuid::new_v4(), org_id)
        .execute(&pool).await.unwrap();

    let token = generate_org_admin_token(Uuid::new_v4(), org_id);

    let req = test::TestRequest::get()
        .uri(&format!("/users?org_id={}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let users: Vec<UserPublic> = test::read_body_json(resp).await;
    assert_eq!(users.len(), 2);
}

#[sqlx::test]
async fn test_get_user(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "org_admin", "users", "read").await;

    let user_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO users (id, org_id, name, role, email, password_hash) VALUES ($1, $2, 'Get Me', 'org_admin'::user_role, 'getme@test.com', 'h')", user_id, org_id)
        .execute(&pool).await.unwrap();

    let token = generate_org_admin_token(Uuid::new_v4(), org_id);

    let req = test::TestRequest::get()
        .uri(&format!("/users/{}", user_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let user: UserPublic = test::read_body_json(resp).await;
    assert_eq!(user.name, "Get Me");
}

#[sqlx::test]
async fn test_update_user(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "org_admin", "users", "update").await;

    let user_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO users (id, org_id, name, role, email, password_hash) VALUES ($1, $2, 'Update Me', 'org_admin'::user_role, 'u@t.com', 'h')", user_id, org_id)
        .execute(&pool).await.unwrap();

    let token = generate_org_admin_token(Uuid::new_v4(), org_id);

    let req = test::TestRequest::patch()
        .uri(&format!("/users/{}", user_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "name": "Updated Name"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let user: UserPublic = test::read_body_json(resp).await;
    assert_eq!(user.name, "Updated Name");
}

#[sqlx::test]
async fn test_delete_user(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "org_admin", "users", "delete").await;

    let user_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO users (id, org_id, name, role, email, password_hash) VALUES ($1, $2, 'Delete Me', 'org_admin'::user_role, 'del@t.com', 'h')", user_id, org_id)
        .execute(&pool).await.unwrap();

    let token = generate_org_admin_token(Uuid::new_v4(), org_id);

    let req = test::TestRequest::delete()
        .uri(&format!("/users/{}", user_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // verify
    let deleted = sqlx::query!("SELECT deleted_at FROM users WHERE id = $1", user_id).fetch_one(&pool).await.unwrap();
    assert!(deleted.deleted_at.is_some());
}

#[sqlx::test]
async fn test_assign_unassign_branch(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "org_admin", "users", "update").await;
    grant_permission(&pool, "org_admin", "users", "read").await;

    let admin_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO users (id, org_id, name, role, email, password_hash) VALUES ($1, $2, 'Admin', 'org_admin'::user_role, 'admin2@t.com', 'h')", admin_id, org_id).execute(&pool).await.unwrap();
    let token = generate_org_admin_token(admin_id, org_id);

    let target_user_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO users (id, org_id, name, role, email, password_hash) VALUES ($1, $2, 'Target User', 'teller'::user_role, 't@t.com', 'h')", target_user_id, org_id)
        .execute(&pool).await.unwrap();

    let branch_id = seed_branch(&pool, org_id).await;

    // Assign
    let req = test::TestRequest::post()
        .uri(&format!("/users/{}/branches", target_user_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "branch_id": branch_id
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // List
    let req2 = test::TestRequest::get()
        .uri(&format!("/users/{}/branches", target_user_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp2 = test::call_service(&app, req2).await;
    assert!(resp2.status().is_success());
    let branches: Vec<UserBranch> = test::read_body_json(resp2).await;
    assert_eq!(branches.len(), 1);
    assert_eq!(branches[0].branch_id, branch_id);

    // Unassign
    let req3 = test::TestRequest::delete()
        .uri(&format!("/users/{}/branches/{}", target_user_id, branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp3 = test::call_service(&app, req3).await;
    assert!(resp3.status().is_success());
}
