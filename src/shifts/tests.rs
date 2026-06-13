use actix_web::{test, App, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::shifts::routes;
use crate::shifts::handlers::*;

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn generate_token(user_id: Uuid, org_id: Option<Uuid>, role: UserRole) -> String {
    crate::auth::jwt::create_token(&get_secret(), user_id, org_id, role, None, 24).unwrap()
}

fn generate_org_admin_token(user_id: Uuid, org_id: Uuid) -> String {
    generate_token(user_id, Some(org_id), UserRole::OrgAdmin)
}

fn generate_teller_token(user_id: Uuid, org_id: Uuid) -> String {
    generate_token(user_id, Some(org_id), UserRole::Teller)
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

async fn seed_branch(pool: &PgPool, org_id: Uuid) -> Uuid {
    let branch_id = Uuid::new_v4();
    let name = format!("Test Branch {}", branch_id);
    sqlx::query(
        "INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)"
    )
    .bind(branch_id)
    .bind(org_id)
    .bind(name)
    .execute(pool)
    .await
    .unwrap();
    branch_id
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

async fn assign_user_to_branch(pool: &PgPool, user_id: Uuid, branch_id: Uuid) {
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(user_id)
        .bind(branch_id)
        .execute(pool)
        .await
        .unwrap();
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
async fn test_open_shift_and_get_current(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    
    grant_permission(&pool, "org_admin", "shifts", "read").await;
    grant_permission(&pool, "org_admin", "shifts", "create").await;

    let token = generate_org_admin_token(user_id, org_id);

    // 1. Get current shift - should be none, suggested 0
    let req = test::TestRequest::get()
        .uri(&format!("/shifts/branches/{}/current", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let prefill: ShiftPreFill = test::read_body_json(resp).await;
    assert!(!prefill.has_open_shift);
    assert_eq!(prefill.suggested_opening_cash, 0);

    // 2. Open shift
    let req_body = OpenShiftRequest {
        id: None,
        opening_cash: 5000,
        opening_cash_edited: Some(true),
        edit_reason: Some("Manager authorized".into()),
        opened_at: None,
    };
    let req2 = test::TestRequest::post()
        .uri(&format!("/shifts/branches/{}/open", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();
    let resp2 = test::call_service(&app, req2).await;
    assert!(resp2.status().is_success());
    let shift: Shift = test::read_body_json(resp2).await;
    assert_eq!(shift.opening_cash, 5000);
    assert_eq!(shift.status, "open");

    // 3. Try to open another shift -> Conflict
    let req3 = test::TestRequest::post()
        .uri(&format!("/shifts/branches/{}/open", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();
    let resp3 = test::call_service(&app, req3).await;
    assert_eq!(resp3.status().as_u16(), 409);

    // 4. Get current shift again -> Should return the open shift
    let req4 = test::TestRequest::get()
        .uri(&format!("/shifts/branches/{}/current", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp4 = test::call_service(&app, req4).await;
    let prefill2: ShiftPreFill = test::read_body_json(resp4).await;
    assert!(prefill2.has_open_shift);
    assert_eq!(prefill2.open_shift.unwrap().id, shift.id);
}

#[sqlx::test]
async fn test_cash_movements(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    
    grant_permission(&pool, "org_admin", "shifts", "read").await;
    grant_permission(&pool, "org_admin", "shifts", "create").await;
    grant_permission(&pool, "org_admin", "shifts", "update").await;

    let token = generate_org_admin_token(user_id, org_id);

    // Open shift
    let shift_id = Uuid::new_v4();
    let req_body = OpenShiftRequest {
        id: Some(shift_id),
        opening_cash: 5000,
        opening_cash_edited: None,
        edit_reason: None,
        opened_at: None,
    };
    let req_open = test::TestRequest::post()
        .uri(&format!("/shifts/branches/{}/open", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&req_body)
        .to_request();
    test::call_service(&app, req_open).await;

    // 1. Add cash movement
    let move_req = CashMovementRequest { amount: -500, note: "Paid vendor".into() };
    let req_move = test::TestRequest::post()
        .uri(&format!("/shifts/{}/cash-movements", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&move_req)
        .to_request();
    let resp_move = test::call_service(&app, req_move).await;
    assert!(resp_move.status().is_success());

    // 2. List cash movements
    let req_list = test::TestRequest::get()
        .uri(&format!("/shifts/{}/cash-movements", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp_list = test::call_service(&app, req_list).await;
    assert!(resp_list.status().is_success());
    let movements: Vec<CashMovement> = test::read_body_json(resp_list).await;
    assert_eq!(movements.len(), 1);
    assert_eq!(movements[0].amount, -500);
}

#[sqlx::test]
async fn test_close_and_force_close_shift(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_admin = seed_user(&pool, org_id, "org_admin").await;
    let user_teller = seed_user(&pool, org_id, "teller").await;
    assign_user_to_branch(&pool, user_teller, branch_id).await;
    
    grant_permission(&pool, "org_admin", "shifts", "read").await;
    grant_permission(&pool, "org_admin", "shifts", "create").await;
    grant_permission(&pool, "org_admin", "shifts", "update").await;
    grant_permission(&pool, "teller", "shifts", "read").await;
    grant_permission(&pool, "teller", "shifts", "update").await;

    let admin_token = generate_org_admin_token(user_admin, org_id);
    let teller_token = generate_teller_token(user_teller, org_id);

    // Open shift
    let shift_id = Uuid::new_v4();
    let req_open = test::TestRequest::post()
        .uri(&format!("/shifts/branches/{}/open", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", admin_token)))
        .set_json(&OpenShiftRequest { id: Some(shift_id), opening_cash: 5000, opening_cash_edited: None, edit_reason: None, opened_at: None })
        .to_request();
    test::call_service(&app, req_open).await;

    // Teller attempts to force close -> Forbidden
    let req_force = test::TestRequest::post()
        .uri(&format!("/shifts/{}/force-close", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", teller_token)))
        .set_json(&ForceCloseRequest { reason: Some("Forgot".into()) })
        .to_request();
    let resp_force = test::call_service(&app, req_force).await;
    assert_eq!(resp_force.status().as_u16(), 403);

    // Admin force closes
    let req_force2 = test::TestRequest::post()
        .uri(&format!("/shifts/{}/force-close", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", admin_token)))
        .set_json(&ForceCloseRequest { reason: Some("Forgot".into()) })
        .to_request();
    let resp_force2 = test::call_service(&app, req_force2).await;
    assert!(resp_force2.status().is_success());
    let shift: Shift = test::read_body_json(resp_force2).await;
    assert_eq!(shift.status, "force_closed");
}

#[sqlx::test]
async fn test_normal_close_and_report(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    
    grant_permission(&pool, "org_admin", "shifts", "read").await;
    grant_permission(&pool, "org_admin", "shifts", "create").await;
    grant_permission(&pool, "org_admin", "shifts", "update").await;

    let token = generate_org_admin_token(user_id, org_id);

    // Open
    let shift_id = Uuid::new_v4();
    let req_open = test::TestRequest::post()
        .uri(&format!("/shifts/branches/{}/open", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&OpenShiftRequest { id: Some(shift_id), opening_cash: 1000, opening_cash_edited: None, edit_reason: None, opened_at: None })
        .to_request();
    test::call_service(&app, req_open).await;

    // Close
    let req_close = test::TestRequest::post()
        .uri(&format!("/shifts/{}/close", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&CloseShiftRequest { closing_cash_declared: 1000, cash_note: None, closed_at: None })
        .to_request();
    let resp_close = test::call_service(&app, req_close).await;
    assert!(resp_close.status().is_success());
    let close_resp: CloseShiftResponse = test::read_body_json(resp_close).await;
    assert_eq!(close_resp.shift.status, "closed");
    assert_eq!(close_resp.shift.closing_cash_declared.unwrap(), 1000);
    assert_eq!(close_resp.shift.closing_cash_system.unwrap(), 1000);

    // Report
    let req_rep = test::TestRequest::get()
        .uri(&format!("/shifts/{}/report", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp_rep = test::call_service(&app, req_rep).await;
    assert!(resp_rep.status().is_success());
    let rep: ShiftReportResponse = test::read_body_json(resp_rep).await;
    assert_eq!(rep.shift.id, shift_id);
    assert_eq!(rep.total_payments, 0);
}

#[sqlx::test]
async fn test_delete_shift_forbidden(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_teller = seed_user(&pool, org_id, "teller").await;
    assign_user_to_branch(&pool, user_teller, branch_id).await;
    let user_admin = seed_user(&pool, org_id, "org_admin").await;
    
    grant_permission(&pool, "org_admin", "shifts", "create").await;

    let admin_token = generate_org_admin_token(user_admin, org_id);
    let teller_token = generate_teller_token(user_teller, org_id);

    // Open
    let shift_id = Uuid::new_v4();
    let req_open = test::TestRequest::post()
        .uri(&format!("/shifts/branches/{}/open", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", admin_token)))
        .set_json(&OpenShiftRequest { id: Some(shift_id), opening_cash: 1000, opening_cash_edited: None, edit_reason: None, opened_at: None })
        .to_request();
    test::call_service(&app, req_open).await;

    // Delete by teller -> Forbidden
    let req_del = test::TestRequest::delete()
        .uri(&format!("/shifts/{}", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", teller_token)))
        .to_request();
    let resp_del = test::call_service(&app, req_del).await;
    assert_eq!(resp_del.status().as_u16(), 403);
    
    // Delete by admin -> Success
    let req_del2 = test::TestRequest::delete()
        .uri(&format!("/shifts/{}", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", admin_token)))
        .to_request();
    let resp_del2 = test::call_service(&app, req_del2).await;
    assert!(resp_del2.status().is_success());
}

#[sqlx::test]
async fn test_teller_cannot_open_shift_at_two_branches(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;
    let teller = seed_user(&pool, org_id, "teller").await;
    assign_user_to_branch(&pool, teller, branch_a).await;
    assign_user_to_branch(&pool, teller, branch_b).await;
    for a in ["create", "read", "update"] { grant_permission(&pool, "teller", "shifts", a).await; }
    let token = generate_teller_token(teller, org_id);

    let open = |branch: Uuid| test::TestRequest::post()
        .uri(&format!("/shifts/branches/{branch}/open"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(serde_json::json!({"opening_cash": 0}))
        .to_request();

    // Opens at branch A.
    assert_eq!(test::call_service(&app, open(branch_a)).await.status(), 201);
    // The same teller may NOT open a second shift at another branch.
    let resp = test::call_service(&app, open(branch_b)).await;
    assert_eq!(resp.status(), 409);
    // DB enforces it too: exactly one open shift for this teller.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM shifts WHERE teller_id=$1 AND status='open'").bind(teller).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test]
async fn test_teller_token_is_bound_to_login_branch(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;
    let teller = seed_user(&pool, org_id, "teller").await;
    assign_user_to_branch(&pool, teller, branch_a).await;
    assign_user_to_branch(&pool, teller, branch_b).await;
    grant_permission(&pool, "teller", "shifts", "read").await;
    // Token minted for branch A (as login does for this device).
    let token = crate::auth::jwt::create_token(&get_secret(), teller, Some(org_id), UserRole::Teller, Some(branch_a), 24).unwrap();

    // Bound to A → cannot read branch B, even though assigned to both.
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/shifts/branches/{branch_b}/current"))
        .insert_header(("Authorization", format!("Bearer {token}"))).to_request()).await;
    assert_eq!(resp.status(), 403);

    // Its own branch works.
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/shifts/branches/{branch_a}/current"))
        .insert_header(("Authorization", format!("Bearer {token}"))).to_request()).await;
    assert_eq!(resp.status(), 200);
}
