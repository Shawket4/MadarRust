use actix_web::{test, App, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::orgs::routes;
use crate::orgs::handlers::Org;

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn generate_token(user_id: Uuid, org_id: Option<Uuid>, role: UserRole) -> String {
    crate::auth::jwt::create_token(&get_secret(), user_id, org_id, role, None, 24).unwrap()
}

fn generate_super_admin_token() -> String {
    generate_token(Uuid::new_v4(), None, UserRole::SuperAdmin)
}

fn generate_org_admin_token(org_id: Uuid) -> String {
    generate_token(Uuid::new_v4(), Some(org_id), UserRole::OrgAdmin)
}

fn multipart_body(fields: &[(&str, &str)]) -> String {
    let mut body = String::new();
    for (name, val) in fields {
        body.push_str("--boundary\r\n");
        body.push_str(&format!("Content-Disposition: form-data; name=\"{}\"\r\n\r\n", name));
        body.push_str(val);
        body.push_str("\r\n");
    }
    body.push_str("--boundary--\r\n");
    body
}

#[sqlx::test]
async fn test_create_org_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let token = generate_super_admin_token();
    let body = multipart_body(&[
        ("name", "Test Organization"),
        ("slug", "test-org"),
        ("currency_code", "USD"),
        ("tax_rate", "0.05"),
    ]);

    let req = test::TestRequest::post()
        .uri("/orgs")
        .insert_header(("Content-Type", "multipart/form-data; boundary=boundary"))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "Response was {:?}", resp.status());

    let org: Org = test::read_body_json(resp).await;
    assert_eq!(org.name, "Test Organization");
    assert_eq!(org.slug, "test-org");
    assert_eq!(org.currency_code, "USD");
    // Depending on DB mapping, tax_rate could be parsed differently, but it should succeed.
}

#[sqlx::test]
async fn test_create_org_conflict(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let token = generate_super_admin_token();

    // First org
    let body = multipart_body(&[("name", "Org 1"), ("slug", "shared-slug")]);
    let req = test::TestRequest::post()
        .uri("/orgs")
        .insert_header(("Content-Type", "multipart/form-data; boundary=boundary"))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // Second org with same slug
    let body2 = multipart_body(&[("name", "Org 2"), ("slug", "shared-slug")]);
    let req2 = test::TestRequest::post()
        .uri("/orgs")
        .insert_header(("Content-Type", "multipart/form-data; boundary=boundary"))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(body2)
        .to_request();
    let resp2 = test::call_service(&app, req2).await;
    assert_eq!(resp2.status(), actix_web::http::StatusCode::CONFLICT);
}

#[sqlx::test]
async fn test_create_org_unauthorized(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let body = multipart_body(&[("name", "Org"), ("slug", "slug")]);
    let req = test::TestRequest::post()
        .uri("/orgs")
        .insert_header(("Content-Type", "multipart/form-data; boundary=boundary"))
        .set_payload(body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn test_list_orgs(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    // Seed orgs
    sqlx::query!("INSERT INTO organizations (name, slug) VALUES ('A', 'a'), ('B', 'b')")
        .execute(&pool)
        .await
        .unwrap();

    let token = generate_super_admin_token();
    let req = test::TestRequest::get()
        .uri("/orgs")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let orgs: Vec<Org> = test::read_body_json(resp).await;
    assert_eq!(orgs.len(), 2);
    // Ordered by name
    assert_eq!(orgs[0].name, "A");
    assert_eq!(orgs[1].name, "B");
}

#[sqlx::test]
async fn test_get_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Test Org', 'test')", org_id)
        .execute(&pool)
        .await
        .unwrap();

    // SuperAdmin
    let token = generate_super_admin_token();
    let req = test::TestRequest::get()
        .uri(&format!("/orgs/{}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let org: Org = test::read_body_json(resp).await;
    assert_eq!(org.id, org_id);

    sqlx::query!("INSERT INTO role_permissions (role, resource, action, granted) VALUES ('org_admin'::user_role, 'orgs'::permission_resource, 'read'::permission_action, true)")
        .execute(&pool)
        .await
        .unwrap();

    // Same Org Admin
    let admin_token = generate_org_admin_token(org_id);
    let req2 = test::TestRequest::get()
        .uri(&format!("/orgs/{}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", admin_token)))
        .to_request();
    let resp2 = test::call_service(&app, req2).await;
    assert!(resp2.status().is_success());

    // Different Org Admin
    let other_admin_token = generate_org_admin_token(Uuid::new_v4());
    let req3 = test::TestRequest::get()
        .uri(&format!("/orgs/{}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", other_admin_token)))
        .to_request();
    let resp3 = test::call_service(&app, req3).await;
    assert_eq!(resp3.status(), actix_web::http::StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn test_update_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Original Name', 'orig-slug')", org_id)
        .execute(&pool)
        .await
        .unwrap();

    let token = generate_super_admin_token();
    let req = test::TestRequest::patch()
        .uri(&format!("/orgs/{}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "name": "Updated Name",
            "slug": "updated-slug"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let org: Org = test::read_body_json(resp).await;
    assert_eq!(org.name, "Updated Name");
    assert_eq!(org.slug, "updated-slug");
}

#[sqlx::test]
async fn test_update_org_conflict(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org1_id = Uuid::new_v4();
    let org2_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org 1', 'slug-1')", org1_id)
        .execute(&pool).await.unwrap();
    sqlx::query!("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org 2', 'slug-2')", org2_id)
        .execute(&pool).await.unwrap();

    let token = generate_super_admin_token();
    let req = test::TestRequest::patch()
        .uri(&format!("/orgs/{}", org1_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&serde_json::json!({
            "slug": "slug-2"
        }))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::CONFLICT);
}

#[sqlx::test]
async fn test_delete_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO organizations (id, name, slug) VALUES ($1, 'To Delete', 'to-del')", org_id)
        .execute(&pool)
        .await
        .unwrap();

    let token = generate_super_admin_token();
    let req = test::TestRequest::delete()
        .uri(&format!("/orgs/{}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // Verify it's deleted (fetch again)
    let req2 = test::TestRequest::get()
        .uri(&format!("/orgs/{}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp2 = test::call_service(&app, req2).await;
    assert_eq!(resp2.status(), actix_web::http::StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn test_delete_org_not_found(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let token = generate_super_admin_token();
    let req = test::TestRequest::delete()
        .uri(&format!("/orgs/{}", Uuid::new_v4()))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn test_upload_org_logo(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;

    let org_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Logo Org', 'logo-org')", org_id)
        .execute(&pool)
        .await
        .unwrap();

    let token = generate_super_admin_token();
    
    // Simulate image upload
    let mut body = String::new();
    body.push_str("--boundary\r\n");
    body.push_str("Content-Disposition: form-data; name=\"logo\"; filename=\"test.png\"\r\n");
    body.push_str("Content-Type: image/png\r\n\r\n");
    body.push_str("fake-image-bytes");
    body.push_str("\r\n--boundary--\r\n");

    let req = test::TestRequest::put()
        .uri(&format!("/orgs/{}/logo", org_id))
        .insert_header(("Content-Type", "multipart/form-data; boundary=boundary"))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_payload(body)
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let org: Org = test::read_body_json(resp).await;
    assert!(org.logo_url.is_some());
}

// ═══════════════════════════════════════════════════════════════════
// Onboarding — derived checklist + completion flag
// ═══════════════════════════════════════════════════════════════════

async fn seed_org_row(pool: &PgPool) -> Uuid {
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Onb', $2)")
        .bind(org_id)
        .bind(format!("onb-{org_id}"))
        .execute(pool)
        .await
        .unwrap();
    org_id
}

async fn grant_org_permission(pool: &PgPool, action: &str) {
    sqlx::query(&format!(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ('org_admin'::user_role, 'orgs'::permission_resource, '{action}'::permission_action, true) \
         ON CONFLICT DO NOTHING"
    ))
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test]
async fn test_onboarding_checklist_and_complete(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org_row(&pool).await;
    grant_org_permission(&pool, "read").await;
    grant_org_permission(&pool, "update").await;
    let token = generate_org_admin_token(org_id);

    // Fresh org: nothing set up → not completable, not completed.
    let req = test::TestRequest::get()
        .uri(&format!("/orgs/{org_id}/onboarding"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "got {:?}", resp.status());
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["completed"], false);
    assert_eq!(body["can_complete"], false);
    let steps = body["steps"].as_array().unwrap();
    assert!(steps.iter().all(|s| s["done"] == false));

    // Satisfy the required steps: branch + payment method + category + item.
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'B')")
        .bind(Uuid::new_v4()).bind(org_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_active) VALUES ($1, 'cash', '{}'::jsonb, '#000', 'cash', true)")
        .bind(org_id).execute(&pool).await.unwrap();
    let cat = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'C')")
        .bind(cat).bind(org_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES ($1, $2, $3, 'Latte', 7000, true)")
        .bind(Uuid::new_v4()).bind(org_id).bind(cat).execute(&pool).await.unwrap();

    let req = test::TestRequest::get()
        .uri(&format!("/orgs/{org_id}/onboarding"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let body: serde_json::Value =
        test::read_body_json(test::call_service(&app, req).await).await;
    assert_eq!(body["can_complete"], true);
    assert_eq!(body["completed"], false);

    // Complete — idempotent, persists, returns the fresh status.
    for _ in 0..2 {
        let req = test::TestRequest::post()
            .uri(&format!("/orgs/{org_id}/onboarding/complete"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["completed"], true);
        assert!(body["completed_at"].is_string());
    }
}
