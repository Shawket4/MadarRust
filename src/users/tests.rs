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

// ── Audit regression tests ───────────────────────────────────────────────

macro_rules! users_app {
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

/// V3: an org_admin must not assign a branch/user that belongs to another org.
#[sqlx::test]
async fn test_assign_branch_cross_org_forbidden(pool: PgPool) {
    let app = users_app!(pool);
    let org_a = seed_org(&pool).await;
    let org_b = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1,'Org B','org-b-xtenant')")
        .bind(org_b).execute(&pool).await.unwrap();
    grant_permission(&pool, "org_admin", "users", "update").await;

    let admin_a = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, name, role, email, password_hash) VALUES ($1,$2,'AdminA','org_admin'::user_role,'aa@t.com','h')")
        .bind(admin_a).bind(org_a).execute(&pool).await.unwrap();
    let token = generate_org_admin_token(admin_a, org_a);

    // Target user + branch both in ORG B.
    let target_b = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, name, role, email, password_hash) VALUES ($1,$2,'TB','teller'::user_role,'tb@t.com','h')")
        .bind(target_b).bind(org_b).execute(&pool).await.unwrap();
    let branch_b = seed_branch(&pool, org_b).await;

    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/users/{}/branches", target_b))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({"branch_id": branch_b})).to_request()).await;
    assert_eq!(resp.status(), 403, "cross-org assign must be forbidden");

    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM user_branch_assignments WHERE user_id=$1 AND branch_id=$2)")
        .bind(target_b).bind(branch_b).fetch_one(&pool).await.unwrap();
    assert!(!exists, "no assignment row may be written");
}

/// V4: a branch_manager must not reset the password of an org_admin, even when
/// they share a branch (vertical privilege escalation / account takeover).
#[sqlx::test]
async fn test_branch_manager_cannot_reset_org_admin_password(pool: PgPool) {
    let app = users_app!(pool);
    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "branch_manager", "users", "update").await;
    grant_permission(&pool, "branch_manager", "users", "read").await;
    let branch_id = seed_branch(&pool, org_id).await;

    let attacker = Uuid::new_v4(); // branch_manager
    sqlx::query("INSERT INTO users (id, org_id, name, role, email, password_hash) VALUES ($1,$2,'BM','branch_manager'::user_role,'bm@t.com','h')")
        .bind(attacker).bind(org_id).execute(&pool).await.unwrap();
    let victim = Uuid::new_v4(); // org_admin
    sqlx::query("INSERT INTO users (id, org_id, name, role, email, password_hash) VALUES ($1,$2,'OA','org_admin'::user_role,'oa@t.com','origpw')")
        .bind(victim).bind(org_id).execute(&pool).await.unwrap();
    // Both assigned to the SAME branch (so the shared-branch gate would otherwise open).
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1,$3),($2,$3)")
        .bind(attacker).bind(victim).bind(branch_id).execute(&pool).await.unwrap();
    let token = generate_branch_manager_token(attacker, org_id);

    let resp = test::call_service(&app, test::TestRequest::patch()
        .uri(&format!("/users/{}", victim))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({"password": "pwned"})).to_request()).await;
    assert_eq!(resp.status(), 403, "branch_manager must not reset an org_admin's credentials");

    let pw: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id=$1")
        .bind(victim).fetch_one(&pool).await.unwrap();
    assert_eq!(pw, "origpw", "victim password must be unchanged");
}
