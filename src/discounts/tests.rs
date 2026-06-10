use actix_web::{test, App, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::discounts::routes;
use crate::discounts::handlers::*;

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



#[sqlx::test]
async fn test_discounts_crud_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    
    // Discounts use the 'menu_items' permission resource
    grant_permission(&pool, "org_admin", "menu_items", "create").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;
    grant_permission(&pool, "org_admin", "menu_items", "update").await;
    grant_permission(&pool, "org_admin", "menu_items", "delete").await;

    let token = generate_org_admin_token(user_id, org_id);

    // 1. Create Discount
    let req_create = test::TestRequest::post()
        .uri("/discounts")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&CreateDiscountRequest {
            name_translations: None,
            org_id,
            name: "Summer Sale".into(),
            dtype: "percentage".into(),
            value: 20,
            is_active: None, // defaults to true
        })
        .to_request();
    let resp_create = test::call_service(&app, req_create).await;
    assert!(resp_create.status().is_success());
    let discount: Discount = test::read_body_json(resp_create).await;
    assert_eq!(discount.name, "Summer Sale");
    assert_eq!(discount.dtype, "percentage");
    assert_eq!(discount.value, 20);
    assert!(discount.is_active);

    // 2. List Discounts
    let req_list = test::TestRequest::get()
        .uri(&format!("/discounts?org_id={}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp_list = test::call_service(&app, req_list).await;
    assert!(resp_list.status().is_success());
    let discounts: Vec<Discount> = test::read_body_json(resp_list).await;
    assert_eq!(discounts.len(), 1);

    // 3. Update Discount
    let req_update = test::TestRequest::patch()
        .uri(&format!("/discounts/{}", discount.id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&UpdateDiscountRequest {
            name_translations: None,
            name: Some("Winter Sale".into()),
            dtype: Some("fixed".into()),
            value: Some(500), // e.g. $5.00
            is_active: Some(false),
        })
        .to_request();
    let resp_update = test::call_service(&app, req_update).await;
    assert!(resp_update.status().is_success());
    let updated: Discount = test::read_body_json(resp_update).await;
    assert_eq!(updated.name, "Winter Sale");
    assert_eq!(updated.dtype, "fixed");
    assert_eq!(updated.value, 500);
    assert!(!updated.is_active);

    // 4. Delete Discount
    let req_delete = test::TestRequest::delete()
        .uri(&format!("/discounts/{}", discount.id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp_delete = test::call_service(&app, req_delete).await;
    assert!(resp_delete.status().is_success());

    // 5. Verify Deletion
    let req_list2 = test::TestRequest::get()
        .uri(&format!("/discounts?org_id={}", org_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp_list2 = test::call_service(&app, req_list2).await;
    let discounts2: Vec<Discount> = test::read_body_json(resp_list2).await;
    assert!(discounts2.is_empty());
}

#[sqlx::test]
async fn test_discounts_validation_failures(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "menu_items", "create").await;

    let token = generate_org_admin_token(user_id, org_id);

    // 1. Invalid DType
    let req1 = test::TestRequest::post()
        .uri("/discounts")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&CreateDiscountRequest {
            name_translations: None,
            org_id,
            name: "Sale".into(),
            dtype: "magic".into(), // Invalid
            value: 20,
            is_active: None,
        })
        .to_request();
    let resp1 = test::call_service(&app, req1).await;
    assert_eq!(resp1.status().as_u16(), 400);

    // 2. Negative Value
    let req2 = test::TestRequest::post()
        .uri("/discounts")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&CreateDiscountRequest {
            name_translations: None,
            org_id,
            name: "Sale".into(),
            dtype: "fixed".into(),
            value: -500, // Invalid
            is_active: None,
        })
        .to_request();
    let resp2 = test::call_service(&app, req2).await;
    assert_eq!(resp2.status().as_u16(), 400);

    // 3. Percentage > 100
    let req3 = test::TestRequest::post()
        .uri("/discounts")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&CreateDiscountRequest {
            name_translations: None,
            org_id,
            name: "Sale".into(),
            dtype: "percentage".into(),
            value: 150, // Invalid (> 100)
            is_active: None,
        })
        .to_request();
    let resp3 = test::call_service(&app, req3).await;
    assert_eq!(resp3.status().as_u16(), 400);
}

#[sqlx::test]
async fn test_discounts_wrong_org(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;
    let org_id_a = seed_org(&pool).await;
    let org_id_b = seed_org(&pool).await;
    let user_a = seed_user(&pool, org_id_a, "org_admin").await;
    
    grant_permission(&pool, "org_admin", "menu_items", "create").await;
    grant_permission(&pool, "org_admin", "menu_items", "read").await;

    let token_a = generate_org_admin_token(user_a, org_id_a);

    // Try to create discount for Org B
    let req_create = test::TestRequest::post()
        .uri("/discounts")
        .insert_header(("Authorization", format!("Bearer {}", token_a)))
        .set_json(&CreateDiscountRequest {
            name_translations: None,
            org_id: org_id_b,
            name: "Sale".into(),
            dtype: "percentage".into(),
            value: 10,
            is_active: None,
        })
        .to_request();
    let resp_create = test::call_service(&app, req_create).await;
    assert_eq!(resp_create.status().as_u16(), 403); // Forbidden

    // Try to list discounts for Org B
    let req_list = test::TestRequest::get()
        .uri(&format!("/discounts?org_id={}", org_id_b))
        .insert_header(("Authorization", format!("Bearer {}", token_a)))
        .to_request();
    let resp_list = test::call_service(&app, req_list).await;
    assert_eq!(resp_list.status().as_u16(), 403); // Forbidden
}
