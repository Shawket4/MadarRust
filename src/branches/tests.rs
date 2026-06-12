use actix_web::{test, App, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::branches::routes;
use crate::branches::handlers::{Branch, PrinterBrand};

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn generate_token(user_id: Uuid, org_id: Option<Uuid>, role: UserRole) -> String {
    crate::auth::jwt::create_token(&get_secret(), user_id, org_id, role, None, 24).unwrap()
}

fn generate_super_admin_token() -> String {
    generate_token(Uuid::new_v4(), None, UserRole::SuperAdmin)
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
async fn test_create_branch_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "org_admin", "branches", "create").await;

    let user_id = Uuid::new_v4();
    let token = generate_org_admin_token(user_id, org_id);

    let req = test::TestRequest::post()
        .uri("/branches")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "org_id": org_id,
            "name": "Downtown Branch",
            "address": "123 Main St",
            "printer_brand": "star",
            "printer_ip": "192.168.1.100"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "Failed: {:?}", resp.status());

    let branch: Branch = test::read_body_json(resp).await;
    assert_eq!(branch.name, "Downtown Branch");
    assert_eq!(branch.org_id, org_id);
    assert_eq!(branch.printer_brand, Some(PrinterBrand::Star));
    assert_eq!(branch.printer_ip.unwrap(), "192.168.1.100/32");
    assert_eq!(branch.printer_port, Some(9100)); // Default port
}

#[sqlx::test]
async fn test_create_branch_unauthorized(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let req = test::TestRequest::post()
        .uri("/branches")
        .set_json(&serde_json::json!({
            "org_id": Uuid::new_v4(),
            "name": "Hacker Branch"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn test_create_branch_foreign_key_missing(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let token = generate_super_admin_token();
    let missing_org_id = Uuid::new_v4();

    let req = test::TestRequest::post()
        .uri("/branches")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "org_id": missing_org_id,
            "name": "Nowhere Branch"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::INTERNAL_SERVER_ERROR);
}

#[sqlx::test]
async fn test_list_branches_org_admin(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "org_admin", "branches", "read").await;

    sqlx::query!("INSERT INTO branches (org_id, name) VALUES ($1, 'Branch A'), ($1, 'Branch B')", org_id)
        .execute(&pool).await.unwrap();

    let token = generate_org_admin_token(Uuid::new_v4(), org_id);

    let req = test::TestRequest::get()
        .uri(&format!("/branches?org_id={}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let branches: Vec<Branch> = test::read_body_json(resp).await;
    assert_eq!(branches.len(), 2);
    assert_eq!(branches[0].name, "Branch A");
}

#[sqlx::test]
async fn test_list_branches_branch_manager(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "branch_manager", "branches", "read").await;

    let branch1_id = Uuid::new_v4();
    let branch2_id = Uuid::new_v4();
    
    sqlx::query!("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Assigned')", branch1_id, org_id).execute(&pool).await.unwrap();
    sqlx::query!("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Unassigned')", branch2_id, org_id).execute(&pool).await.unwrap();

    let user_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO users (id, org_id, name, role, password_hash) VALUES ($1, $2, 'Test Manager', 'branch_manager'::user_role, 'hash')", user_id, org_id)
        .execute(&pool).await.unwrap();
    // Insert user assignment
    sqlx::query!("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)", user_id, branch1_id)
        .execute(&pool).await.unwrap();

    let token = generate_branch_manager_token(user_id, org_id);

    let req = test::TestRequest::get()
        .uri(&format!("/branches?org_id={}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let branches: Vec<Branch> = test::read_body_json(resp).await;
    assert_eq!(branches.len(), 1);
    assert_eq!(branches[0].name, "Assigned");
}

#[sqlx::test]
async fn test_get_branch(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "org_admin", "branches", "read").await;

    let branch_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Get Me')", branch_id, org_id)
        .execute(&pool).await.unwrap();

    let token = generate_org_admin_token(Uuid::new_v4(), org_id);

    let req = test::TestRequest::get()
        .uri(&format!("/branches/{}", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let branch: Branch = test::read_body_json(resp).await;
    assert_eq!(branch.id, branch_id);
}

#[sqlx::test]
async fn test_update_branch(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "org_admin", "branches", "update").await;
    // get_branch needs read to fetch the branch! Wait, Update branch route checks "update"
    let branch_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO branches (id, org_id, name, printer_ip) VALUES ($1, $2, 'Update Me', '10.0.0.1')", branch_id, org_id)
        .execute(&pool).await.unwrap();

    let token = generate_org_admin_token(Uuid::new_v4(), org_id);

    let req = test::TestRequest::put()
        .uri(&format!("/branches/{}", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "name": "Updated Name",
            "printer_ip": null // This should clear the printer_ip
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "Failed: {:?}", resp.status());

    let branch: Branch = test::read_body_json(resp).await;
    assert_eq!(branch.name, "Updated Name");
    assert_eq!(branch.printer_ip, None);
}

#[sqlx::test]
async fn test_delete_branch(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "org_admin", "branches", "delete").await;

    let branch_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Delete Me')", branch_id, org_id)
        .execute(&pool).await.unwrap();

    let token = generate_org_admin_token(Uuid::new_v4(), org_id);

    let req = test::TestRequest::delete()
        .uri(&format!("/branches/{}", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // Verify it is deleted
    grant_permission(&pool, "org_admin", "branches", "read").await;
    let req2 = test::TestRequest::get()
        .uri(&format!("/branches/{}", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp2 = test::call_service(&app, req2).await;
    assert_eq!(resp2.status(), actix_web::http::StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn test_delete_branch_not_found(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let token = generate_super_admin_token();

    let req = test::TestRequest::delete()
        .uri(&format!("/branches/{}", Uuid::new_v4()))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
}
