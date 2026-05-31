#![allow(unused_imports, unused_variables, dead_code)]
use actix_web::{test, App, web};
use sqlx::PgPool;
use uuid::Uuid;
use std::env;
use std::path::PathBuf;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::uploads::routes;
use crate::uploads::handlers::UploadResponse;

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn generate_token(user_id: Uuid, org_id: Option<Uuid>, role: UserRole) -> String {
    crate::auth::jwt::create_token(&get_secret(), user_id, org_id, role, None, 24).unwrap()
}

fn generate_org_admin_token(user_id: Uuid, org_id: Uuid) -> String {
    generate_token(user_id, Some(org_id), UserRole::OrgAdmin)
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let org_id = Uuid::new_v4();
    let slug = format!("test-org-{}", org_id);
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Test Org', $2)")
        .bind(org_id)
        .bind(slug)
        .execute(pool)
        .await
        .unwrap();
    org_id
}

async fn seed_user(pool: &PgPool, org_id: Uuid, role: &str) -> Uuid {
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES ($1, $2, 'Test User', $3, 'hash', $4::user_role)"
    )
    .bind(user_id)
    .bind(org_id)
    .bind(format!("user-{}@test.com", user_id))
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    user_id
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

async fn seed_category(pool: &PgPool, org_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name, display_order) VALUES ($1, $2, 'Cat', 0)")
        .bind(id)
        .bind(org_id)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_menu_item(pool: &PgPool, org_id: Uuid, cat_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES ($1, $2, $3, 'Item', 100, true)")
        .bind(id)
        .bind(org_id)
        .bind(cat_id)
        .execute(pool)
        .await
        .unwrap();
    id
}

// 1x1 pixel valid JPEG image
const TINY_JPEG: &[u8] = &[
    0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x01, 0x00, 0x48,
    0x00, 0x48, 0x00, 0x00, 0xff, 0xdb, 0x00, 0x43, 0x00, 0x03, 0x02, 0x02, 0x02, 0x02, 0x02, 0x03,
    0x02, 0x02, 0x02, 0x03, 0x03, 0x03, 0x03, 0x04, 0x06, 0x04, 0x04, 0x04, 0x04, 0x04, 0x08, 0x06,
    0x06, 0x05, 0x06, 0x09, 0x08, 0x0a, 0x0a, 0x09, 0x08, 0x09, 0x09, 0x0a, 0x0c, 0x0f, 0x0c, 0x0a,
    0x0b, 0x0e, 0x0b, 0x09, 0x09, 0x0d, 0x11, 0x0d, 0x0e, 0x0f, 0x10, 0x10, 0x11, 0x10, 0x0a, 0x0c,
    0x12, 0x13, 0x12, 0x10, 0x13, 0x0f, 0x10, 0x10, 0x10, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00, 0x01,
    0x00, 0x01, 0x01, 0x01, 0x11, 0x00, 0xff, 0xc4, 0x00, 0x14, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0xff, 0xc4, 0x00, 0x14,
    0x10, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0xff, 0xda, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3f, 0x00, 0x2f, 0x00, 0xff, 0xd9,
];

fn setup_env_vars() -> tempfile::TempDir {
    let tmp_dir = tempfile::Builder::new().prefix("sufrix-test-uploads").tempdir().unwrap();
    unsafe {
        env::set_var("UPLOADS_DIR", tmp_dir.path().to_str().unwrap());
        env::set_var("UPLOADS_BASE_URL", "http://localhost:8080/uploads");
    }
    tmp_dir
}

fn create_multipart_body(boundary: &str, field_name: &str, file_name: &str, content_type: &str, data: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
    body.extend_from_slice(format!("Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n", field_name, file_name).as_bytes());
    body.extend_from_slice(format!("Content-Type: {}\r\n\r\n", content_type).as_bytes());
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{}--\r\n", boundary).as_bytes());
    body
}

#[sqlx::test]
async fn test_upload_menu_item_image_success(pool: PgPool) {
    let _tmp_dir = setup_env_vars();
    
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let token = generate_org_admin_token(user_id, org_id);

    let cat_id = seed_category(&pool, org_id).await;
    let item_id = seed_menu_item(&pool, org_id, cat_id).await;

    let boundary = "boundary123";
    let body = create_multipart_body(boundary, "image", "test.jpg", "image/jpeg", TINY_JPEG);

    let req = test::TestRequest::post()
        .uri(&format!("/uploads/menu-items/{}", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .insert_header(("Content-Type", format!("multipart/form-data; boundary={}", boundary)))
        .set_payload(body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "Expected success, got {:?}", resp.status());

    let upload_resp: UploadResponse = test::read_body_json(resp).await;
    assert!(upload_resp.image_url.starts_with("http://localhost:8080/uploads/"));

    // Check DB
    let db_url: Option<String> = sqlx::query_scalar("SELECT image_url FROM menu_items WHERE id = $1")
        .bind(item_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    
    assert!(db_url.is_some());
    let db_url = db_url.unwrap();
    assert_eq!(upload_resp.image_url, db_url);
}

#[sqlx::test]
async fn test_upload_menu_item_image_wrong_org(pool: PgPool) {
    let _tmp_dir = setup_env_vars();
    
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org1 = seed_org(&pool).await;
    let org2 = seed_org(&pool).await;

    // User is in org1
    let user_id = seed_user(&pool, org1, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let token = generate_org_admin_token(user_id, org1);

    // Item is in org2
    let cat_id = seed_category(&pool, org2).await;
    let item_id = seed_menu_item(&pool, org2, cat_id).await;

    let boundary = "boundary123";
    let body = create_multipart_body(boundary, "image", "test.jpg", "image/jpeg", TINY_JPEG);

    let req = test::TestRequest::post()
        .uri(&format!("/uploads/menu-items/{}", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .insert_header(("Content-Type", format!("multipart/form-data; boundary={}", boundary)))
        .set_payload(body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 403);
}

#[sqlx::test]
async fn test_upload_menu_item_image_invalid_mime(pool: PgPool) {
    let _tmp_dir = setup_env_vars();
    
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let token = generate_org_admin_token(user_id, org_id);

    let cat_id = seed_category(&pool, org_id).await;
    let item_id = seed_menu_item(&pool, org_id, cat_id).await;

    let boundary = "boundary123";
    // Mime is text/plain instead of an allowed image type
    let body = create_multipart_body(boundary, "image", "test.txt", "text/plain", b"hello world");

    let req = test::TestRequest::post()
        .uri(&format!("/uploads/menu-items/{}", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .insert_header(("Content-Type", format!("multipart/form-data; boundary={}", boundary)))
        .set_payload(body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 400);
}

#[sqlx::test]
async fn test_upload_menu_item_image_invalid_image_data(pool: PgPool) {
    let _tmp_dir = setup_env_vars();
    
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    let token = generate_org_admin_token(user_id, org_id);

    let cat_id = seed_category(&pool, org_id).await;
    let item_id = seed_menu_item(&pool, org_id, cat_id).await;

    let boundary = "boundary123";
    // Content-Type is valid image/jpeg, but data is garbage
    let body = create_multipart_body(boundary, "image", "test.jpg", "image/jpeg", b"not an image really");

    let req = test::TestRequest::post()
        .uri(&format!("/uploads/menu-items/{}", item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .insert_header(("Content-Type", format!("multipart/form-data; boundary={}", boundary)))
        .set_payload(body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 400); // Invalid image
}
