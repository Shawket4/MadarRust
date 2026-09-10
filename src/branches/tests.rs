use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::branches::handlers::{Branch, PrinterBrand};
use crate::branches::routes;
use crate::models::UserRole;

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
    sqlx::query!(
        "INSERT INTO organizations (id, name, slug) VALUES ($1, 'Test Org', 'test-org')",
        org_id
    )
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

/// A branch's tax override has to come back as a NUMBER, and `null` when it
/// inherits — which is not the same as zero.
///
/// The column is `numeric` and the field is a `BigDecimal`, and bigdecimal's
/// serde writes that as a JSON string. A dashboard reading `"0.1400"` through
/// `Number.isFinite` gets false and shows zero, so the override looks like it
/// saved and did nothing. Nothing failed; the bytes just disagreed with the
/// schema, which said `f64` all along.
#[sqlx::test]
async fn a_branch_rate_comes_back_as_a_json_number(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "org_admin", "branches", "create").await;
    grant_permission(&pool, "org_admin", "branches", "update").await;
    let token = generate_org_admin_token(Uuid::new_v4(), org_id);

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/branches")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&serde_json::json!({
                "org_id": org_id,
                "name": "Inherits",
                "address": "1 High St",
            }))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());
    let body: serde_json::Value = test::read_body_json(resp).await;
    let branch_id = body["id"].as_str().unwrap().to_string();
    assert!(body["tax_rate"].is_null(), "inherit is null, never 0");

    let resp = test::call_service(
        &app,
        test::TestRequest::put()
            .uri(&format!("/branches/{branch_id}"))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&serde_json::json!({
                "tax_rate": 0.14,
                "service_charge_rate": 0.125,
            }))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success(), "got {}", resp.status());
    let body: serde_json::Value = test::read_body_json(resp).await;
    for field in ["tax_rate", "service_charge_rate"] {
        assert!(
            body[field].is_number(),
            "{field} came back as {:?}",
            body[field]
        );
    }
    assert_eq!(body["tax_rate"].as_f64().unwrap(), 0.14);
    assert_eq!(body["service_charge_rate"].as_f64().unwrap(), 0.125);
}

#[sqlx::test]
async fn test_create_branch_success(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

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
            .configure(routes::configure),
    )
    .await;

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
            .configure(routes::configure),
    )
    .await;

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
    // A FK violation (org_id doesn't exist) is a client error, surfaced as 409
    // Conflict per the documented convention — not a 500. (Previously every sqlx
    // error mapped to 500; API fuzzing caught the whole class.)
    assert_eq!(resp.status(), actix_web::http::StatusCode::CONFLICT);
}

#[sqlx::test]
async fn test_list_branches_org_admin(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "org_admin", "branches", "read").await;

    sqlx::query!(
        "INSERT INTO branches (org_id, name) VALUES ($1, 'Branch A'), ($1, 'Branch B')",
        org_id
    )
    .execute(&pool)
    .await
    .unwrap();

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
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "branch_manager", "branches", "read").await;

    let branch1_id = Uuid::new_v4();
    let branch2_id = Uuid::new_v4();

    sqlx::query!(
        "INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Assigned')",
        branch1_id,
        org_id
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query!(
        "INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Unassigned')",
        branch2_id,
        org_id
    )
    .execute(&pool)
    .await
    .unwrap();

    let user_id = Uuid::new_v4();
    sqlx::query!("INSERT INTO users (id, org_id, name, role, password_hash) VALUES ($1, $2, 'Test Manager', 'branch_manager'::user_role, 'hash')", user_id, org_id)
        .execute(&pool).await.unwrap();
    // Insert user assignment
    sqlx::query!(
        "INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)",
        user_id,
        branch1_id
    )
    .execute(&pool)
    .await
    .unwrap();

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
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "org_admin", "branches", "read").await;

    let branch_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Get Me')",
        branch_id,
        org_id
    )
    .execute(&pool)
    .await
    .unwrap();

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
            .configure(routes::configure),
    )
    .await;

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
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "org_admin", "branches", "delete").await;

    let branch_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Delete Me')",
        branch_id,
        org_id
    )
    .execute(&pool)
    .await
    .unwrap();

    let token = generate_org_admin_token(Uuid::new_v4(), org_id);

    let req = test::TestRequest::delete()
        .uri(&format!("/branches/{}", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert!(
        resp.status().is_success(),
        "{}: {}",
        resp.status(),
        String::from_utf8_lossy(&test::read_body(resp).await)
    );

    // Verify it is deleted
    grant_permission(&pool, "org_admin", "branches", "read").await;
    let req2 = test::TestRequest::get()
        .uri(&format!("/branches/{}", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp2 = test::call_service(&app, req2).await;
    assert_eq!(resp2.status(), actix_web::http::StatusCode::NOT_FOUND);
}

/// Deleting a branch that is still trading is refused, and the refusal names
/// what is in the way. An open shift is the important case: its Z-report has
/// not been taken and its float has not been counted, so nothing may close it
/// on the branch's behalf.
#[sqlx::test]
async fn test_delete_branch_refused_while_a_shift_is_open(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "org_admin", "branches", "delete").await;

    let branch_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Still Trading')",
        branch_id,
        org_id
    )
    .execute(&pool)
    .await
    .unwrap();

    let teller_id = Uuid::new_v4();
    sqlx::query!(
        "INSERT INTO users (id, org_id, name, pin_hash, role) \
         VALUES ($1, $2, 'Teller On Shift', 'x', 'teller')",
        teller_id,
        org_id
    )
    .execute(&pool)
    .await
    .unwrap();

    // `closed_at IS NULL` — the shift is still running. (The default till is
    // filled in by a trigger.)
    sqlx::query("INSERT INTO shifts (branch_id, teller_id) VALUES ($1, $2)")
        .bind(branch_id)
        .bind(teller_id)
        .execute(&pool)
        .await
        .unwrap();

    let token = generate_org_admin_token(Uuid::new_v4(), org_id);
    let req = test::TestRequest::delete()
        .uri(&format!("/branches/{}", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::CONFLICT);

    let body: serde_json::Value = test::read_body_json(resp).await;
    let message = body["error"].as_str().unwrap_or_default().to_string();
    assert!(
        message.contains("1 open shift"),
        "the refusal should name the shift, got: {message}"
    );

    // And the branch is still there.
    let alive: bool = sqlx::query_scalar("SELECT deleted_at IS NULL FROM branches WHERE id = $1")
        .bind(branch_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        alive,
        "a refused delete must not have soft-deleted the branch"
    );
}

#[sqlx::test]
async fn test_delete_branch_not_found(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let token = generate_super_admin_token();

    let req = test::TestRequest::delete()
        .uri(&format!("/branches/{}", Uuid::new_v4()))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::NOT_FOUND);
}

/// V1 (defense-in-depth): a branch timezone that PostgreSQL does not recognize
/// must be rejected at write time, so it can never reach the reports query.
#[sqlx::test]
async fn test_create_branch_rejects_invalid_timezone(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    grant_permission(&pool, "org_admin", "branches", "create").await;
    let token = generate_org_admin_token(Uuid::new_v4(), org_id);

    // An injection-style / non-IANA timezone is rejected.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/branches")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&serde_json::json!({
                "org_id": org_id,
                "name": "Bad TZ Branch",
                "timezone": "Africa/Cairo' UNION SELECT version() --"
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400, "invalid timezone must be rejected");

    // A valid IANA timezone is accepted.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/branches")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&serde_json::json!({
                "org_id": org_id,
                "name": "Good TZ Branch",
                "timezone": "America/New_York"
            }))
            .to_request(),
    )
    .await;
    assert!(
        resp.status().is_success(),
        "valid timezone must be accepted: {:?}",
        resp.status()
    );
}

/// A branch with no timezone of its own INHERITS the org's timezone (the
/// effective tz returned by the API is COALESCE(branch.timezone, org.timezone)).
/// An explicit branch timezone overrides the org default.
#[sqlx::test]
async fn test_branch_inherits_org_timezone(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    // Give the org a non-default timezone.
    sqlx::query!(
        "UPDATE organizations SET timezone = 'Asia/Riyadh' WHERE id = $1",
        org_id
    )
    .execute(&pool)
    .await
    .unwrap();
    grant_permission(&pool, "org_admin", "branches", "create").await;
    let token = generate_org_admin_token(Uuid::new_v4(), org_id);

    // No branch timezone → inherits the org's.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/branches")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&serde_json::json!({ "org_id": org_id, "name": "Inheriting Branch" }))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());
    let branch: Branch = test::read_body_json(resp).await;
    assert_eq!(
        branch.timezone, "Asia/Riyadh",
        "branch with no tz must inherit org tz"
    );

    // Explicit branch timezone overrides the org default.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/branches")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&serde_json::json!({
                "org_id": org_id, "name": "Explicit Branch", "timezone": "America/New_York"
            }))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());
    let branch: Branch = test::read_body_json(resp).await;
    assert_eq!(
        branch.timezone, "America/New_York",
        "explicit branch tz must override org tz"
    );
}

/// GET /timezones returns the controlled vocabulary (the timezone_name enum
/// labels) that the dashboard select is built from.
#[sqlx::test]
async fn test_list_timezones(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let token = generate_org_admin_token(Uuid::new_v4(), org_id);

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/timezones")
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let zones: Vec<String> = test::read_body_json(resp).await;
    assert!(
        zones.len() > 100,
        "expected the full IANA list, got {}",
        zones.len()
    );
    assert!(zones.contains(&"Africa/Cairo".to_string()));
    assert!(zones.contains(&"America/New_York".to_string()));
    assert!(
        !zones.iter().any(|z| z.starts_with("posix/")),
        "posix aliases must be filtered out"
    );
}
