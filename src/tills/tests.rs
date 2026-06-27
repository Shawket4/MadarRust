use actix_web::{test, web, App};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::tills::handlers::Till;
use crate::tills::routes;

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn org_admin_token(user_id: Uuid, org_id: Uuid) -> String {
    crate::auth::jwt::create_token(&get_secret(), user_id, Some(org_id), UserRole::OrgAdmin, None, 24)
        .unwrap()
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Test Org', $2)")
        .bind(org_id)
        .bind(format!("org-{org_id}"))
        .execute(pool)
        .await
        .unwrap();
    org_id
}

async fn seed_branch(pool: &PgPool, org_id: Uuid) -> Uuid {
    let branch_id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Branch')")
        .bind(branch_id)
        .bind(org_id)
        .execute(pool)
        .await
        .unwrap();
    branch_id
}

async fn seed_teller(pool: &PgPool, org_id: Uuid) -> Uuid {
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, 'Teller', $3, 'hash', 'teller'::user_role)",
    )
    .bind(user_id)
    .bind(org_id)
    .bind(format!("teller-{user_id}@test.com"))
    .execute(pool)
    .await
    .unwrap();
    user_id
}

async fn grant(pool: &PgPool, role: &str, action: &str) {
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ($1::user_role, 'branches'::permission_resource, $2::permission_action, true) \
         ON CONFLICT DO NOTHING",
    )
    .bind(role)
    .bind(action)
    .execute(pool)
    .await
    .unwrap();
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

#[sqlx::test]
async fn create_and_list_tills(pool: PgPool) {
    let app = app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    grant(&pool, "org_admin", "create").await;
    grant(&pool, "org_admin", "read").await;
    let token = org_admin_token(Uuid::new_v4(), org_id);

    let req = test::TestRequest::post()
        .uri("/tills")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(&serde_json::json!({ "branch_id": branch_id, "name": "Front", "is_default": true }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success(), "create failed: {:?}", resp.status());
    let till: Till = test::read_body_json(resp).await;
    assert_eq!(till.name, "Front");
    assert!(till.is_default);

    let req = test::TestRequest::get()
        .uri(&format!("/tills?branch_id={branch_id}"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let tills: Vec<Till> = test::read_body_json(resp).await;
    assert_eq!(tills.len(), 1);
    assert_eq!(tills[0].id, till.id);
}

#[sqlx::test]
async fn only_one_default_till_per_branch(pool: PgPool) {
    let app = app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    grant(&pool, "org_admin", "create").await;
    let token = org_admin_token(Uuid::new_v4(), org_id);

    for name in ["A", "B"] {
        let req = test::TestRequest::post()
            .uri("/tills")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(&serde_json::json!({ "branch_id": branch_id, "name": name, "is_default": true }))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success(), "create {name} failed: {:?}", resp.status());
    }

    // The second default must have demoted the first — exactly one default remains.
    let defaults: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tills WHERE branch_id = $1 AND is_default AND deleted_at IS NULL",
    )
    .bind(branch_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(defaults, 1, "expected exactly one default till");
}

#[sqlx::test]
async fn duplicate_name_rejected(pool: PgPool) {
    let app = app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    grant(&pool, "org_admin", "create").await;
    let token = org_admin_token(Uuid::new_v4(), org_id);

    let body = serde_json::json!({ "branch_id": branch_id, "name": "Till X" });
    let mk = || {
        test::TestRequest::post()
            .uri("/tills")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(&body)
            .to_request()
    };
    assert!(test::call_service(&app, mk()).await.status().is_success());
    let resp = test::call_service(&app, mk()).await;
    assert!(resp.status().is_client_error(), "dup name should be 4xx, got {:?}", resp.status());
}

#[sqlx::test]
async fn cannot_delete_till_with_open_shift(pool: PgPool) {
    let app = app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    grant(&pool, "org_admin", "create").await;
    grant(&pool, "org_admin", "delete").await;
    let token = org_admin_token(Uuid::new_v4(), org_id);

    let req = test::TestRequest::post()
        .uri("/tills")
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(&serde_json::json!({ "branch_id": branch_id, "name": "Till D" }))
        .to_request();
    let till: Till = test::read_body_json(test::call_service(&app, req).await).await;

    // Open a shift on this till (direct insert).
    let teller_id = seed_teller(&pool, org_id).await;
    sqlx::query("INSERT INTO shifts (branch_id, teller_id, till_id, opening_cash, status) VALUES ($1,$2,$3,0,'open')")
        .bind(branch_id)
        .bind(teller_id)
        .bind(till.id)
        .execute(&pool)
        .await
        .unwrap();

    let req = test::TestRequest::delete()
        .uri(&format!("/tills/{}", till.id))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 409, "delete with open shift should 409");
}
