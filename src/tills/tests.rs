use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::tills::handlers::{
    CashMovement, CashMovementRequest, CloseTillRequest as CloseShiftRequest, ForceCloseRequest,
};
use crate::tills::legacy::*;
use crate::tills::legacy_routes as routes;

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
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)")
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

/// V32 — cash continuity: a new shift must open with the previous shift's
/// DECLARED closing cash; a deviation needs a reason and is recorded as an edit
/// (server-derived, not the client flag).
#[sqlx::test]
async fn test_open_shift_cash_continuity(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "tills", "read").await;
    grant_permission(&pool, "org_admin", "tills", "create").await;
    grant_permission(&pool, "org_admin", "tills", "update").await;
    let token = generate_org_admin_token(user_id, org_id);

    let open = |opening: i32, reason: Option<String>| {
        let app = &app;
        let token = token.clone();
        async move {
            let req = test::TestRequest::post()
                .uri(&format!("/shifts/branches/{}/open", branch_id))
                .insert_header(("Authorization", format!("Bearer {}", token)))
                .set_json(&OpenShiftRequest {
                    till_id: None,
                    id: None,
                    opening_cash: opening,
                    opening_cash_edited: None,
                    edit_reason: reason,
                    opened_at: None,
                })
                .to_request();
            test::call_service(app, req).await
        }
    };
    let close = |shift_id: Uuid, declared: i32| {
        let app = &app;
        let token = token.clone();
        async move {
            let req = test::TestRequest::post()
                .uri(&format!("/shifts/{}/close", shift_id))
                .insert_header(("Authorization", format!("Bearer {}", token)))
                .set_json(&CloseShiftRequest {
                    device_id: None,
                    reconciliation: None,
                    closing_cash_declared: declared,
                    cash_note: None,
                    closed_at: None,
                })
                .to_request();
            test::call_service(app, req).await
        }
    };

    // 1. First shift — no predecessor, so any opening is the starting float,
    //    not an edit, and there is no carryover baseline.
    let r = open(1000, None).await;
    assert_eq!(r.status(), 201, "first shift opens without a carryover");
    let s1: Shift = test::read_body_json(r).await;
    assert_eq!(s1.opening_cash, 1000);
    assert!(!s1.opening_cash_was_edited);
    assert_eq!(s1.opening_cash_original, None);
    assert_eq!(close(s1.id, 1500).await.status(), 200);

    // 2. Next shift opens with the carryover (1500) — clean, no reason needed.
    let r = open(1500, None).await;
    assert_eq!(r.status(), 201, "matching the carryover opens cleanly");
    let s2: Shift = test::read_body_json(r).await;
    assert!(!s2.opening_cash_was_edited);
    assert_eq!(s2.opening_cash_original, Some(1500));
    assert_eq!(close(s2.id, 2000).await.status(), 200);

    // 3. Deviating from the carryover (2000) WITHOUT a reason → rejected.
    assert_eq!(
        open(1800, None).await.status(),
        400,
        "silent deviation from the declared carryover must be rejected"
    );

    // 4. Same deviation WITH a reason → allowed and recorded as an edit, with
    //    the expected carryover preserved in opening_cash_original.
    let r = open(1800, Some("Owner pulled 200 float".into())).await;
    assert_eq!(r.status(), 201);
    let s3: Shift = test::read_body_json(r).await;
    assert_eq!(s3.opening_cash, 1800);
    assert!(s3.opening_cash_was_edited);
    assert_eq!(s3.opening_cash_original, Some(2000));
    assert_eq!(
        s3.opening_cash_edit_reason.as_deref(),
        Some("Owner pulled 200 float")
    );
}

#[sqlx::test]
async fn test_open_shift_and_get_current(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;

    grant_permission(&pool, "org_admin", "tills", "read").await;
    grant_permission(&pool, "org_admin", "tills", "create").await;

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
        till_id: None,
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
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;

    grant_permission(&pool, "org_admin", "tills", "read").await;
    grant_permission(&pool, "org_admin", "tills", "create").await;
    grant_permission(&pool, "org_admin", "tills", "update").await;

    let token = generate_org_admin_token(user_id, org_id);

    // Open shift
    let shift_id = Uuid::new_v4();
    let req_body = OpenShiftRequest {
        till_id: None,
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
    // No kind sent — the clients in the field still speak only in signed
    // amounts, and a negative one has always meant a pay-out.
    let move_req = CashMovementRequest {
        device_id: None,
        amount: -500,
        kind: None,
        corrects_id: None,
        note: "Paid vendor".into(),
        created_at: None,
        client_ref: None,
    };
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
    assert_eq!(
        movements[0].kind, "pay_out",
        "an unlabelled negative amount is a pay-out, as it always was"
    );
    assert_eq!(movements[0].corrects_id, None);
}

#[sqlx::test]
async fn test_close_and_force_close_shift(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_admin = seed_user(&pool, org_id, "org_admin").await;
    let user_teller = seed_user(&pool, org_id, "teller").await;
    assign_user_to_branch(&pool, user_teller, branch_id).await;

    grant_permission(&pool, "org_admin", "tills", "read").await;
    grant_permission(&pool, "org_admin", "tills", "create").await;
    grant_permission(&pool, "org_admin", "tills", "update").await;
    grant_permission(&pool, "teller", "tills", "read").await;
    grant_permission(&pool, "teller", "tills", "update").await;

    let admin_token = generate_org_admin_token(user_admin, org_id);
    let teller_token = generate_teller_token(user_teller, org_id);

    // Open shift
    let shift_id = Uuid::new_v4();
    let req_open = test::TestRequest::post()
        .uri(&format!("/shifts/branches/{}/open", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", admin_token)))
        .set_json(&OpenShiftRequest {
            till_id: None,
            id: Some(shift_id),
            opening_cash: 5000,
            opening_cash_edited: None,
            edit_reason: None,
            opened_at: None,
        })
        .to_request();
    test::call_service(&app, req_open).await;

    // Teller attempts to force close -> Forbidden
    let req_force = test::TestRequest::post()
        .uri(&format!("/shifts/{}/force-close", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", teller_token)))
        .set_json(&ForceCloseRequest {
            device_id: None,
            reason: Some("Forgot".into()),
        })
        .to_request();
    let resp_force = test::call_service(&app, req_force).await;
    assert_eq!(resp_force.status().as_u16(), 403);

    // Admin force closes
    let req_force2 = test::TestRequest::post()
        .uri(&format!("/shifts/{}/force-close", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", admin_token)))
        .set_json(&ForceCloseRequest {
            device_id: None,
            reason: Some("Forgot".into()),
        })
        .to_request();
    let resp_force2 = test::call_service(&app, req_force2).await;
    assert!(resp_force2.status().is_success());
    let shift: Shift = test::read_body_json(resp_force2).await;
    assert_eq!(shift.status, "force_closed");
}

// Offline-first P0: a replayed cash movement (same client_ref) must apply ONCE.
#[sqlx::test]
async fn test_cash_movement_client_ref_idempotent(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "tills", "read").await;
    grant_permission(&pool, "org_admin", "tills", "create").await;
    grant_permission(&pool, "org_admin", "tills", "update").await;
    let token = generate_org_admin_token(user_id, org_id);

    let shift_id = Uuid::new_v4();
    test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/branches/{}/open", branch_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&OpenShiftRequest {
                till_id: None,
                id: Some(shift_id),
                opening_cash: 5000,
                opening_cash_edited: None,
                edit_reason: None,
                opened_at: None,
            })
            .to_request(),
    )
    .await;

    // Same client_ref sent twice (a replayed offline movement).
    let cref = Uuid::new_v4();
    let body = CashMovementRequest {
        device_id: None,
        amount: -500,
        kind: None,
        corrects_id: None,
        note: "Paid vendor".into(),
        created_at: None,
        client_ref: Some(cref),
    };

    let first = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/cash-movements", shift_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&body)
            .to_request(),
    )
    .await;
    assert!(first.status().is_success());
    let m1: CashMovement = test::read_body_json(first).await;
    assert_eq!(m1.client_ref, Some(cref), "server must echo client_ref");

    let second = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/cash-movements", shift_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&body)
            .to_request(),
    )
    .await;
    assert!(second.status().is_success());
    let m2: CashMovement = test::read_body_json(second).await;

    assert_eq!(
        m1.id, m2.id,
        "same client_ref must return the same movement"
    );

    // Exactly one movement exists despite the duplicate request.
    let list = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/shifts/{}/cash-movements", shift_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request(),
    )
    .await;
    let movements: Vec<CashMovement> = test::read_body_json(list).await;
    assert_eq!(
        movements.len(),
        1,
        "duplicate client_ref must not create a second movement"
    );
}

// Offline-first P0: a replayed force-close returns the terminal shift (200), not 400.
#[sqlx::test]
async fn test_force_close_idempotent(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_admin = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "tills", "create").await;
    grant_permission(&pool, "org_admin", "tills", "update").await;
    let token = generate_org_admin_token(user_admin, org_id);

    let shift_id = Uuid::new_v4();
    test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/branches/{}/open", branch_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&OpenShiftRequest {
                till_id: None,
                id: Some(shift_id),
                opening_cash: 5000,
                opening_cash_edited: None,
                edit_reason: None,
                opened_at: None,
            })
            .to_request(),
    )
    .await;

    let r1 = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/force-close", shift_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&ForceCloseRequest {
                device_id: None,
                reason: Some("Forgot".into()),
            })
            .to_request(),
    )
    .await;
    assert!(r1.status().is_success());
    let s1: Shift = test::read_body_json(r1).await;
    assert_eq!(s1.status, "force_closed");

    // Replay must be idempotent: 200 + the same terminal shift, not a 400.
    let r2 = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/force-close", shift_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&ForceCloseRequest {
                device_id: None,
                reason: Some("Forgot".into()),
            })
            .to_request(),
    )
    .await;
    assert_eq!(
        r2.status().as_u16(),
        200,
        "replayed force-close must be idempotent"
    );
    let s2: Shift = test::read_body_json(r2).await;
    assert_eq!(s2.status, "force_closed");
    assert_eq!(s1.id, s2.id);
}

#[sqlx::test]
async fn test_normal_close_and_report(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;

    grant_permission(&pool, "org_admin", "tills", "read").await;
    grant_permission(&pool, "org_admin", "tills", "create").await;
    grant_permission(&pool, "org_admin", "tills", "update").await;

    let token = generate_org_admin_token(user_id, org_id);

    // Open
    let shift_id = Uuid::new_v4();
    let req_open = test::TestRequest::post()
        .uri(&format!("/shifts/branches/{}/open", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&OpenShiftRequest {
            till_id: None,
            id: Some(shift_id),
            opening_cash: 1000,
            opening_cash_edited: None,
            edit_reason: None,
            opened_at: None,
        })
        .to_request();
    test::call_service(&app, req_open).await;

    // Close
    let req_close = test::TestRequest::post()
        .uri(&format!("/shifts/{}/close", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&CloseShiftRequest {
            device_id: None,
            reconciliation: None,
            closing_cash_declared: 1000,
            cash_note: None,
            closed_at: None,
        })
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
    // Closed shift → expected_cash is the snapshot taken at close.
    assert_eq!(rep.expected_cash, 1000);
}

#[sqlx::test]
async fn test_delete_shift_forbidden(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_teller = seed_user(&pool, org_id, "teller").await;
    assign_user_to_branch(&pool, user_teller, branch_id).await;
    let user_admin = seed_user(&pool, org_id, "org_admin").await;

    grant_permission(&pool, "org_admin", "tills", "create").await;
    grant_permission(&pool, "org_admin", "tills", "update").await;

    let admin_token = generate_org_admin_token(user_admin, org_id);
    let teller_token = generate_teller_token(user_teller, org_id);

    // Open
    let shift_id = Uuid::new_v4();
    let req_open = test::TestRequest::post()
        .uri(&format!("/shifts/branches/{}/open", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", admin_token)))
        .set_json(&OpenShiftRequest {
            till_id: None,
            id: Some(shift_id),
            opening_cash: 1000,
            opening_cash_edited: None,
            edit_reason: None,
            opened_at: None,
        })
        .to_request();
    test::call_service(&app, req_open).await;

    // Delete by teller -> Forbidden (role check)
    let req_del = test::TestRequest::delete()
        .uri(&format!("/shifts/{}", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", teller_token)))
        .to_request();
    let resp_del = test::call_service(&app, req_del).await;
    assert_eq!(resp_del.status().as_u16(), 403);

    // Even an admin may NOT delete an OPEN shift — it must be force-closed first
    // so live orders are never silently destroyed.
    let req_del_open = test::TestRequest::delete()
        .uri(&format!("/shifts/{}", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", admin_token)))
        .to_request();
    assert_eq!(
        test::call_service(&app, req_del_open)
            .await
            .status()
            .as_u16(),
        409
    );

    // Force-close it (admin), then the empty shift can be deleted.
    let req_fc = test::TestRequest::post()
        .uri(&format!("/shifts/{}/force-close", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", admin_token)))
        .set_json(&ForceCloseRequest {
            device_id: None,
            reason: Some("cleanup".into()),
        })
        .to_request();
    assert!(test::call_service(&app, req_fc).await.status().is_success());

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
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;
    let teller = seed_user(&pool, org_id, "teller").await;
    assign_user_to_branch(&pool, teller, branch_a).await;
    assign_user_to_branch(&pool, teller, branch_b).await;
    for a in ["create", "read", "update"] {
        grant_permission(&pool, "teller", "tills", a).await;
    }
    let token = generate_teller_token(teller, org_id);

    let open = |branch: Uuid| {
        test::TestRequest::post()
            .uri(&format!("/shifts/branches/{branch}/open"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(serde_json::json!({"opening_cash": 0}))
            .to_request()
    };

    // Opens at branch A.
    assert_eq!(test::call_service(&app, open(branch_a)).await.status(), 201);
    // The same teller may NOT open a second shift at another branch.
    let resp = test::call_service(&app, open(branch_b)).await;
    assert_eq!(resp.status(), 409);
    // DB enforces it too: exactly one open shift for this teller.
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tills WHERE teller_id=$1 AND status='open'")
            .bind(teller)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test]
async fn test_list_shifts_all_branches(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;
    let admin = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "tills", "read").await;
    let token = generate_org_admin_token(admin, org_id);

    // One closed shift in each branch (closed → no one-open-per-teller clash).
    for branch in [branch_a, branch_b] {
        sqlx::query(
            "INSERT INTO tills (id, branch_id, teller_id, status, opening_cash, closing_cash_declared, closed_at)
             VALUES ($1,$2,$3,'closed',10000,10000,NOW())")
            .bind(Uuid::new_v4()).bind(branch).bind(admin).execute(&pool).await.unwrap();
    }
    // A different org's shift must never appear in this org's all-branches view.
    let other_org = seed_org(&pool).await;
    let other_branch = seed_branch(&pool, other_org).await;
    let other_admin = seed_user(&pool, other_org, "org_admin").await;
    sqlx::query("INSERT INTO tills (id, branch_id, teller_id, status, opening_cash) VALUES ($1,$2,$3,'open',5000)")
        .bind(Uuid::new_v4()).bind(other_branch).bind(other_admin).execute(&pool).await.unwrap();

    let auth = ("Authorization", format!("Bearer {token}"));

    // All branches (nil UUID): both org branches' shifts, branch-labelled, org-isolated.
    // No pagination params → one page holding everything (dashboard-compatible).
    let nil = Uuid::nil();
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/shifts/branches/{nil}"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let page: PaginatedShifts = test::read_body_json(resp).await;
    assert_eq!(page.total, 2, "all-branches sees both org branches' shifts");
    assert_eq!(page.total_pages, 1, "no pagination params → single page");
    let shifts = page.data;
    assert_eq!(shifts.len(), 2);
    assert!(
        shifts.iter().all(|s| s.branch_name.is_some()),
        "rows carry a branch label"
    );
    let seen: std::collections::HashSet<_> = shifts.iter().map(|s| s.branch_id).collect();
    assert!(seen.contains(&branch_a) && seen.contains(&branch_b));

    // Opt-in pagination: per_page=1 slices the result while reporting the full total.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/shifts/branches/{nil}?page=1&per_page=1"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let paged: PaginatedShifts = test::read_body_json(resp).await;
    assert_eq!(paged.total, 2);
    assert_eq!(paged.per_page, 1);
    assert_eq!(paged.total_pages, 2);
    assert_eq!(paged.data.len(), 1, "one row per page");

    // A specific branch still scopes to that one branch.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/shifts/branches/{branch_a}"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let just_a: PaginatedShifts = test::read_body_json(resp).await;
    assert_eq!(just_a.total, 1);
    assert_eq!(just_a.data.len(), 1);
    assert_eq!(just_a.data[0].branch_id, branch_a);
}

#[sqlx::test]
async fn test_teller_token_org_scoped_across_branches(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;
    let teller = seed_user(&pool, org_id, "teller").await;
    assign_user_to_branch(&pool, teller, branch_a).await;
    assign_user_to_branch(&pool, teller, branch_b).await;
    grant_permission(&pool, "teller", "tills", "read").await;
    // Token minted for branch A (as login does for this device).
    let token = crate::auth::jwt::create_token(
        &get_secret(),
        teller,
        Some(org_id),
        UserRole::Teller,
        Some(branch_a),
        24,
    )
    .unwrap();

    // D13: org-scoped — a teller token minted for branch A may read branch B in
    // the SAME org (the token-branch binding is gone).
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/shifts/branches/{branch_b}/current"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);

    // Its own branch works too.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/shifts/branches/{branch_a}/current"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
}

/// V30: a closed shift's cash uses the SALE-TIME is_cash snapshot, so flipping
/// is_cash (or renaming) the payment method afterward does NOT change history.
#[sqlx::test]
async fn test_close_cash_uses_is_cash_snapshot(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    sqlx::query("INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) VALUES ($1,'cash','{}','e','i',true,true)")
        .bind(org_id).execute(&pool).await.unwrap();
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "tills", "create").await;
    grant_permission(&pool, "org_admin", "tills", "update").await;
    let token = generate_org_admin_token(user_id, org_id);

    // Open a shift with 1000 opening cash.
    let shift_id = Uuid::new_v4();
    let open = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/branches/{}/open", branch_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&OpenShiftRequest {
                till_id: None,
                id: Some(shift_id),
                opening_cash: 1000,
                opening_cash_edited: None,
                edit_reason: None,
                opened_at: None,
            })
            .to_request(),
    )
    .await;
    assert!(open.status().is_success());

    // A completed CASH order of 500, with order_payments.is_cash snapshotted true.
    let order_id = Uuid::new_v4();
    sqlx::query("INSERT INTO orders (id, branch_id, teller_id, till_id, idempotency_key, subtotal, tax_amount, total_amount, status, order_number, payment_method, order_ref) VALUES ($1,$2,$3,$4, gen_random_uuid(), 500,0,500,'completed',1,'cash', gen_random_uuid()::text)")
        .bind(order_id).bind(branch_id).bind(user_id).bind(shift_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ($1,'cash',500,true)")
        .bind(order_id).execute(&pool).await.unwrap();

    // CORRUPTION: the 'cash' method is later flipped to NOT cash.
    sqlx::query("UPDATE org_payment_methods SET is_cash=false WHERE org_id=$1 AND name='cash'")
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();

    // Close: system cash must still be opening 1000 + the cash order 500 = 1500,
    // because is_cash was snapshotted at sale time (not read from current config).
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/close", shift_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&CloseShiftRequest {
                device_id: None,
                reconciliation: None,
                closing_cash_declared: 1500,
                cash_note: None,
                closed_at: None,
            })
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());
    let closed: CloseShiftResponse = test::read_body_json(resp).await;
    assert_eq!(
        closed.shift.closing_cash_system.unwrap(),
        1500,
        "cash order must still count via the sale-time snapshot"
    );
}

/// A teller may close ONLY their own shift — closing settles cash, so it must be
/// attributed to the right person. A second teller (same branch) is rejected.
#[sqlx::test]
async fn test_teller_cannot_close_another_tellers_shift(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    // Two tellers in the same org need distinct names (unique teller name/org).
    let teller_a = Uuid::new_v4();
    let teller_b = Uuid::new_v4();
    for (id, nm) in [(teller_a, "Teller A"), (teller_b, "Teller B")] {
        sqlx::query("INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES ($1,$2,$3,$4,'hash','teller'::user_role)")
            .bind(id).bind(org_id).bind(nm).bind(format!("{}@test.com", id))
            .execute(&pool).await.unwrap();
    }
    assign_user_to_branch(&pool, teller_a, branch_id).await;
    assign_user_to_branch(&pool, teller_b, branch_id).await;
    for a in ["create", "read", "update"] {
        grant_permission(&pool, "teller", "tills", a).await;
    }
    let token_a = generate_teller_token(teller_a, org_id);
    let token_b = generate_teller_token(teller_b, org_id);

    // Teller A opens the (only) shift for the branch.
    let shift_id = Uuid::new_v4();
    let open = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/branches/{}/open", branch_id))
            .insert_header(("Authorization", format!("Bearer {}", token_a)))
            .set_json(&OpenShiftRequest {
                till_id: None,
                id: Some(shift_id),
                opening_cash: 0,
                opening_cash_edited: None,
                edit_reason: None,
                opened_at: None,
            })
            .to_request(),
    )
    .await;
    assert!(open.status().is_success());

    // Teller B (same branch) cannot close A's shift.
    let resp_b = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/close", shift_id))
            .insert_header(("Authorization", format!("Bearer {}", token_b)))
            .set_json(&CloseShiftRequest {
                device_id: None,
                reconciliation: None,
                closing_cash_declared: 0,
                cash_note: None,
                closed_at: None,
            })
            .to_request(),
    )
    .await;
    assert_eq!(
        resp_b.status().as_u16(),
        403,
        "a teller cannot close another teller's shift"
    );

    // The shift is still open afterwards.
    let still_open: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tills WHERE id=$1 AND status='open')")
            .bind(shift_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(still_open);

    // Its owner CAN close it.
    let resp_a = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/close", shift_id))
            .insert_header(("Authorization", format!("Bearer {}", token_a)))
            .set_json(&CloseShiftRequest {
                device_id: None,
                reconciliation: None,
                closing_cash_declared: 0,
                cash_note: None,
                closed_at: None,
            })
            .to_request(),
    )
    .await;
    assert!(resp_a.status().is_success());
}

/// delete_shift must never destroy recorded sales: a shift that still has a
/// non-voided order cannot be deleted even by an admin, even after close.
#[sqlx::test]
async fn test_delete_shift_with_orders_blocked(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let admin = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "tills", "create").await;
    grant_permission(&pool, "org_admin", "tills", "update").await;
    let token = generate_org_admin_token(admin, org_id);

    let shift_id = Uuid::new_v4();
    let open = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/branches/{}/open", branch_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&OpenShiftRequest {
                till_id: None,
                id: Some(shift_id),
                opening_cash: 0,
                opening_cash_edited: None,
                edit_reason: None,
                opened_at: None,
            })
            .to_request(),
    )
    .await;
    assert!(open.status().is_success());

    // A recorded (non-voided) order on the shift.
    sqlx::query("INSERT INTO orders (id, branch_id, teller_id, till_id, idempotency_key, subtotal, tax_amount, total_amount, status, order_number, payment_method, order_ref) VALUES (gen_random_uuid(),$1,$2,$3, gen_random_uuid(), 500,0,500,'completed',1,'cash', gen_random_uuid()::text)")
        .bind(branch_id).bind(admin).bind(shift_id).execute(&pool).await.unwrap();

    // Force-close so the only barrier left is the recorded-order guard.
    let fc = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/force-close", shift_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&ForceCloseRequest {
                device_id: None,
                reason: Some("x".into()),
            })
            .to_request(),
    )
    .await;
    assert!(fc.status().is_success());

    // Delete is refused — the sale is part of the financial record.
    let del = test::call_service(
        &app,
        test::TestRequest::delete()
            .uri(&format!("/shifts/{}", shift_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request(),
    )
    .await;
    assert_eq!(
        del.status().as_u16(),
        409,
        "cannot delete a shift with recorded orders"
    );

    // The shift and its order are still there.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM tills WHERE id=$1")
        .bind(shift_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

/// A force-close FREEZES `closing_cash_system` (same formula as a normal close),
/// so a force-closed shift has an immutable expected-cash audit figure.
#[sqlx::test]
async fn test_force_close_snapshots_system_cash(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    sqlx::query("INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) VALUES ($1,'cash','{}','e','i',true,true)")
        .bind(org_id).execute(&pool).await.unwrap();
    let branch_id = seed_branch(&pool, org_id).await;
    let admin = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "tills", "create").await;
    grant_permission(&pool, "org_admin", "tills", "update").await;
    let token = generate_org_admin_token(admin, org_id);

    // Open with 1000 float.
    let shift_id = Uuid::new_v4();
    let open = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/branches/{}/open", branch_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&OpenShiftRequest {
                till_id: None,
                id: Some(shift_id),
                opening_cash: 1000,
                opening_cash_edited: None,
                edit_reason: None,
                opened_at: None,
            })
            .to_request(),
    )
    .await;
    assert!(open.status().is_success());

    // A 500 cash sale lands in the drawer.
    let order_id = Uuid::new_v4();
    sqlx::query("INSERT INTO orders (id, branch_id, teller_id, till_id, idempotency_key, subtotal, tax_amount, total_amount, status, order_number, payment_method, order_ref) VALUES ($1,$2,$3,$4, gen_random_uuid(), 500,0,500,'completed',1,'cash', gen_random_uuid()::text)")
        .bind(order_id).bind(branch_id).bind(admin).bind(shift_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ($1,'cash',500,true)")
        .bind(order_id).execute(&pool).await.unwrap();

    // Force-close (no declared count collected) still snapshots system cash = 1500.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/force-close", shift_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&ForceCloseRequest {
                device_id: None,
                reason: Some("absent teller".into()),
            })
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());
    let shift: Shift = test::read_body_json(resp).await;
    assert_eq!(shift.status, "force_closed");
    assert_eq!(
        shift.closing_cash_system.unwrap(),
        1500,
        "force-close must freeze expected cash"
    );
    assert!(
        shift.closing_cash_declared.is_none(),
        "no declared count at force-close"
    );
}

/// Client shift timestamps: a future opened_at/closed_at is rejected (clock guard),
/// while a PAST opened_at is honored verbatim (offline backdating).
#[sqlx::test]
async fn test_shift_timestamp_guards(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "tills", "create").await;
    grant_permission(&pool, "org_admin", "tills", "update").await;
    grant_permission(&pool, "org_admin", "tills", "read").await;
    let token = generate_org_admin_token(user_id, org_id);

    let open = |id: Uuid, opened_at: Option<chrono::DateTime<chrono::Utc>>| {
        test::TestRequest::post()
            .uri(&format!("/shifts/branches/{}/open", branch_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&OpenShiftRequest {
                till_id: None,
                id: Some(id),
                opening_cash: 1000,
                opening_cash_edited: None,
                edit_reason: None,
                opened_at,
            })
            .to_request()
    };

    // Future opened_at -> rejected (no shift created).
    let resp = test::call_service(
        &app,
        open(
            Uuid::new_v4(),
            Some(chrono::Utc::now() + chrono::Duration::minutes(30)),
        ),
    )
    .await;
    assert_eq!(resp.status(), 400, "future opened_at must be rejected");

    // Past opened_at -> honored verbatim.
    let sid = Uuid::new_v4();
    let backdated = chrono::Utc::now() - chrono::Duration::hours(6);
    let resp = test::call_service(&app, open(sid, Some(backdated))).await;
    assert!(
        resp.status().is_success(),
        "past opened_at must be honored: {:?}",
        resp.status()
    );
    let stored: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT opened_at FROM tills WHERE id=$1")
            .bind(sid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        stored.timestamp(),
        backdated.timestamp(),
        "opened_at must round-trip"
    );

    // Close with a future closed_at -> rejected.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/close", sid))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&CloseShiftRequest {
                device_id: None,
                reconciliation: None,
                closing_cash_declared: 1000,
                cash_note: None,
                closed_at: Some(chrono::Utc::now() + chrono::Duration::minutes(30)),
            })
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400, "future closed_at must be rejected");
}

/// Omitting opened_at makes the server stamp ~now (the online path).
#[sqlx::test]
async fn test_shift_opened_at_defaults_to_now(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "tills", "create").await;
    let token = generate_org_admin_token(user_id, org_id);

    let sid = Uuid::new_v4();
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/branches/{}/open", branch_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&OpenShiftRequest {
                till_id: None,
                id: Some(sid),
                opening_cash: 1000,
                opening_cash_edited: None,
                edit_reason: None,
                opened_at: None,
            })
            .to_request(),
    )
    .await;
    assert!(
        resp.status().is_success(),
        "open without opened_at must succeed: {:?}",
        resp.status()
    );
    let stored: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT opened_at FROM tills WHERE id=$1")
            .bind(sid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        (chrono::Utc::now() - stored).num_seconds().abs() < 120,
        "server-stamped near now"
    );
}

/// Cash movements: server-stamps when omitted, honors a past created_at (offline),
/// rejects a future one.
#[sqlx::test]
async fn test_cash_movement_timestamp_contract(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "tills", "create").await;
    grant_permission(&pool, "org_admin", "tills", "update").await;
    let token = generate_org_admin_token(user_id, org_id);

    let sid = Uuid::new_v4();
    test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/branches/{}/open", branch_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&OpenShiftRequest {
                till_id: None,
                id: Some(sid),
                opening_cash: 1000,
                opening_cash_edited: None,
                edit_reason: None,
                opened_at: None,
            })
            .to_request(),
    )
    .await;

    let movement = |created_at: Option<chrono::DateTime<chrono::Utc>>| {
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/cash-movements", sid))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&CashMovementRequest {
                device_id: None,
                amount: -500,
                kind: None,
                corrects_id: None,
                note: "vendor".into(),
                created_at,
                client_ref: None,
            })
            .to_request()
    };

    // Omitted -> server-stamped near now.
    let resp = test::call_service(&app, movement(None)).await;
    assert!(resp.status().is_success());
    let m: CashMovement = test::read_body_json(resp).await;
    assert!(
        (chrono::Utc::now() - m.created_at).num_seconds().abs() < 120,
        "server-stamped near now"
    );

    // Past -> honored.
    let past = chrono::Utc::now() - chrono::Duration::hours(3);
    let resp = test::call_service(&app, movement(Some(past))).await;
    assert!(
        resp.status().is_success(),
        "past created_at must be honored: {:?}",
        resp.status()
    );
    let m: CashMovement = test::read_body_json(resp).await;
    assert_eq!(
        m.created_at.timestamp(),
        past.timestamp(),
        "created_at round-trips"
    );

    // Future -> rejected.
    let resp = test::call_service(
        &app,
        movement(Some(chrono::Utc::now() + chrono::Duration::minutes(30))),
    )
    .await;
    assert_eq!(resp.status(), 400, "future created_at must be rejected");
}

// ── Multi-teller / tills ──────────────────────────────────────

async fn seed_till(
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    name: &str,
    is_default: bool,
) -> Uuid {
    // The drawer entity is gone (TILLS_DECISIONS): legacy till_id is accepted and ignored.
    let _ = (pool, org_id, branch_id, name, is_default);
    Uuid::new_v4()
}

/// Several tills may be open at one branch at once (one per drawer), but a single
/// till holds only one open shift — the per-till index, not the branch, is the guard.

/// Cash continuity is per-TILL (the drawer), not per teller: a handover keeps the
/// float. A fresh, never-used till has no carryover.

// ── Cash movement kinds ───────────────────────────────────────

/// Helper for the kind tests: an open shift under an org admin, with the
/// permissions the shift routes check. Returns (token, shift_id, branch_id, user_id).
async fn open_admin_shift(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    pool: &PgPool,
    org_id: Uuid,
    branch_id: Uuid,
    till_id: Option<Uuid>,
    opening_cash: i32,
) -> (String, Uuid, Uuid) {
    let user_id = seed_user(pool, org_id, "org_admin").await;
    grant_permission(pool, "org_admin", "tills", "read").await;
    grant_permission(pool, "org_admin", "tills", "create").await;
    grant_permission(pool, "org_admin", "tills", "update").await;
    let token = generate_org_admin_token(user_id, org_id);
    let shift_id = Uuid::new_v4();
    let resp = test::call_service(
        app,
        test::TestRequest::post()
            .uri(&format!("/shifts/branches/{}/open", branch_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&OpenShiftRequest {
                till_id,
                id: Some(shift_id),
                opening_cash,
                opening_cash_edited: None,
                edit_reason: None,
                opened_at: None,
            })
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201, "shift must open");
    (token, shift_id, user_id)
}

fn movement_json(amount: i32, kind: Option<&str>, corrects_id: Option<Uuid>) -> serde_json::Value {
    serde_json::json!({
        "amount": amount,
        "kind": kind,
        "corrects_id": corrects_id,
        "note": "test",
    })
}

/// The kind fixes the sign and the report counts by kind: a safe drop leaves the
/// drawer but is not spend, and a correction nets against the row it reverses
/// instead of showing the same money as cash in AND cash out. For a shift with
/// only pay-ins and pay-outs the in/out figures are exactly what they were.
#[sqlx::test]
async fn test_cash_movement_kinds_drive_the_report(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let (token, shift_id, _) = open_admin_shift(&app, &pool, org_id, branch_id, None, 1000).await;

    let post = |body: serde_json::Value| {
        let token = token.clone();
        let app = &app;
        async move {
            test::call_service(
                app,
                test::TestRequest::post()
                    .uri(&format!("/shifts/{}/cash-movements", shift_id))
                    .insert_header(("Authorization", format!("Bearer {}", token)))
                    .set_json(&body)
                    .to_request(),
            )
            .await
        }
    };

    // The kind and the sign must agree — a 400 with words, never a CHECK 500.
    for (amount, kind) in [(-100, "pay_in"), (100, "pay_out"), (100, "safe_drop")] {
        let resp = post(movement_json(amount, Some(kind), None)).await;
        assert_eq!(resp.status(), 400, "{kind} of {amount} must be rejected");
    }
    // Only a correction may say what it corrects.
    let resp = post(movement_json(-100, Some("pay_out"), Some(Uuid::new_v4()))).await;
    assert_eq!(resp.status(), 400, "corrects_id on a pay-out is rejected");

    // A real day: an owner tops up the float, the teller buys milk, drops cash
    // to the safe, then records a pay-out by mistake and reverses it.
    let resp = post(movement_json(2000, Some("pay_in"), None)).await;
    assert_eq!(resp.status(), 201);
    let resp = post(movement_json(-300, Some("pay_out"), None)).await;
    assert_eq!(resp.status(), 201);
    let resp = post(movement_json(-1500, Some("safe_drop"), None)).await;
    assert_eq!(resp.status(), 201);
    let resp = post(movement_json(-800, Some("pay_out"), None)).await;
    assert_eq!(resp.status(), 201);
    let mistake: CashMovement = test::read_body_json(resp).await;

    // A correction must reverse the row EXACTLY …
    let resp = post(movement_json(500, Some("correction"), Some(mistake.id))).await;
    assert_eq!(resp.status(), 400, "a partial reversal is not a correction");
    // … of a row that exists …
    let resp = post(movement_json(800, Some("correction"), Some(Uuid::new_v4()))).await;
    assert_eq!(resp.status(), 404);
    // … and then it lands.
    let resp = post(movement_json(800, Some("correction"), Some(mistake.id))).await;
    assert_eq!(resp.status(), 201, "exact reversal is accepted");
    let fix: CashMovement = test::read_body_json(resp).await;
    assert_eq!(fix.kind, "correction");
    assert_eq!(fix.corrects_id, Some(mistake.id));
    // A row is corrected once: the money is already back.
    let resp = post(movement_json(800, Some("correction"), Some(mistake.id))).await;
    assert_eq!(
        resp.status(),
        409,
        "a second correction of the same row is refused"
    );

    // A correction of something never recorded (a miscounted float).
    let resp = post(movement_json(50, Some("correction"), None)).await;
    assert_eq!(resp.status(), 201);

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/shifts/{}/report", shift_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());
    let rep: ShiftReportResponse = test::read_body_json(resp).await;

    assert_eq!(rep.cash_movements_in, 2000, "pay-ins only");
    assert_eq!(
        rep.cash_movements_out, 300,
        "spend excludes the safe drop and the reversed mistake"
    );
    assert_eq!(rep.safe_drops, 1500, "the safe drop stands on its own");
    assert_eq!(
        rep.cash_adjustments, 50,
        "an unlinked correction is an adjustment"
    );
    // The drawer does not care about kinds: every note moved.
    let net = 2000 - 300 - 1500 - 800 + 800 + 50;
    assert_eq!(rep.cash_movements_net, net);
    assert_eq!(
        rep.expected_cash,
        1000 + net,
        "expected cash follows the drawer"
    );
    assert_eq!(rep.cash_movements.len(), 6);
    let fix_row = rep
        .cash_movements
        .iter()
        .find(|m| m.id == fix.id)
        .expect("correction listed");
    assert_eq!(fix_row.corrects_kind.as_deref(), Some("pay_out"));
    // No till float set → nothing proposed.
    assert_eq!(rep.standard_float, None);
    assert_eq!(rep.suggested_safe_drop, None);
}

/// A correction is scoped to its shift: pointing it at another drawer's row
/// would net nothing anyone can see and silently move this drawer's cash.
#[sqlx::test]
async fn test_correction_must_stay_on_its_shift(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let till_a = seed_till(&pool, org_id, branch_id, "A", true).await;
    let till_b = seed_till(&pool, org_id, branch_id, "B", false).await;
    let (token_a, shift_a, _) =
        open_admin_shift(&app, &pool, org_id, branch_id, Some(till_a), 0).await;
    let (token_b, shift_b, _) =
        open_admin_shift(&app, &pool, org_id, branch_id, Some(till_b), 0).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/cash-movements", shift_a))
            .insert_header(("Authorization", format!("Bearer {}", token_a)))
            .set_json(&movement_json(-400, Some("pay_out"), None))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let on_a: CashMovement = test::read_body_json(resp).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/cash-movements", shift_b))
            .insert_header(("Authorization", format!("Bearer {}", token_b)))
            .set_json(&movement_json(400, Some("correction"), Some(on_a.id)))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400, "cross-shift correction is refused");
}

// ── Standard float ────────────────────────────────────────────

/// The till knows what should stay in the drawer, so the pre-close report can
/// propose "leave the float, drop the rest" instead of relying on memory.
#[sqlx::test]
async fn test_standard_float_proposes_the_safe_drop(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    sqlx::query("INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) VALUES ($1,'cash','{}','e','i',true,true)")
        .bind(org_id).execute(&pool).await.unwrap();
    let branch_id = seed_branch(&pool, org_id).await;
    let till = seed_till(&pool, org_id, branch_id, "Front", true).await;
    sqlx::query("UPDATE branches SET standard_float = 5000 WHERE id = $1")
        .bind(branch_id)
        .execute(&pool)
        .await
        .unwrap();
    let (token, shift_id, user_id) =
        open_admin_shift(&app, &pool, org_id, branch_id, Some(till), 1000).await;

    let report = |shift: Uuid| {
        let token = token.clone();
        let app = &app;
        async move {
            let resp = test::call_service(
                app,
                test::TestRequest::get()
                    .uri(&format!("/shifts/{}/report", shift))
                    .insert_header(("Authorization", format!("Bearer {}", token)))
                    .to_request(),
            )
            .await;
            assert!(resp.status().is_success());
            let rep: ShiftReportResponse = test::read_body_json(resp).await;
            rep
        }
    };

    // Under the float: nothing to drop, but the float is still shown.
    let rep = report(shift_id).await;
    assert_eq!(rep.standard_float, Some(5000));
    assert_eq!(
        rep.suggested_safe_drop,
        Some(0),
        "a drawer under its float drops nothing"
    );

    // A 6000 cash sale → 7000 in the drawer → drop 2000 to close at 5000.
    let order_id = Uuid::new_v4();
    sqlx::query("INSERT INTO orders (id, branch_id, teller_id, till_id, idempotency_key, subtotal, tax_amount, total_amount, status, order_number, payment_method, order_ref) VALUES ($1,$2,$3,$4, gen_random_uuid(), 6000,0,6000,'completed',1,'cash', gen_random_uuid()::text)")
        .bind(order_id).bind(branch_id).bind(user_id).bind(shift_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ($1,'cash',6000,true)")
        .bind(order_id).execute(&pool).await.unwrap();
    let rep = report(shift_id).await;
    assert_eq!(rep.expected_cash, 7000);
    assert_eq!(rep.suggested_safe_drop, Some(2000));

    // Closed: the float is history, no action is proposed.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/close", shift_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&CloseShiftRequest {
                device_id: None,
                reconciliation: None,
                closing_cash_declared: 5000,
                cash_note: None,
                closed_at: None,
            })
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());
    let rep = report(shift_id).await;
    assert_eq!(rep.standard_float, Some(5000));
    assert_eq!(rep.suggested_safe_drop, None);
}

// ── Ruling 3: a branch manager works the till ─────────────────

/// A branch manager opens a shift, is handed their OWN shift back as the
/// current one even when another drawer opened later, moves cash, closes, and
/// force-closes an absent teller's shift — with no approval step anywhere.
#[sqlx::test]
async fn test_branch_manager_works_the_till(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let till_a = seed_till(&pool, org_id, branch_id, "A", true).await;
    let till_b = seed_till(&pool, org_id, branch_id, "B", false).await;
    // What `permissions::seeder` promises a branch manager.
    for action in ["create", "read", "update"] {
        grant_permission(&pool, "branch_manager", "tills", action).await;
        grant_permission(&pool, "teller", "tills", action).await;
    }
    let manager = seed_user(&pool, org_id, "branch_manager").await;
    assign_user_to_branch(&pool, manager, branch_id).await;
    let manager_token = generate_token(manager, Some(org_id), UserRole::BranchManager);
    let teller = seed_user(&pool, org_id, "teller").await;
    let teller_token = generate_teller_token(teller, org_id);

    let open = |token: &str, till: Uuid| {
        let token = token.to_string();
        let app = &app;
        async move {
            test::call_service(
                app,
                test::TestRequest::post()
                    .uri(&format!("/shifts/branches/{}/open", branch_id))
                    .insert_header(("Authorization", format!("Bearer {}", token)))
                    .set_json(&OpenShiftRequest {
                        till_id: Some(till),
                        id: None,
                        opening_cash: 0,
                        opening_cash_edited: None,
                        edit_reason: None,
                        opened_at: None,
                    })
                    .to_request(),
            )
            .await
        }
    };

    // The manager opens Till A; a teller opens Till B afterwards.
    let resp = open(&manager_token, till_a).await;
    assert_eq!(resp.status(), 201, "a branch manager may open a shift");
    let managers_shift: Shift = test::read_body_json(resp).await;
    assert_eq!(managers_shift.teller_id, manager);
    let resp = open(&teller_token, till_b).await;
    assert_eq!(resp.status(), 201);
    let tellers_shift: Shift = test::read_body_json(resp).await;

    // The device asks for the current shift with no till: the manager gets
    // THEIR shift, not the teller's newer one.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/shifts/branches/{}/current", branch_id))
            .insert_header(("Authorization", format!("Bearer {}", manager_token)))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());
    let prefill: ShiftPreFill = test::read_body_json(resp).await;
    assert_eq!(
        prefill.open_shift.map(|s| s.id),
        Some(managers_shift.id),
        "a manager at the till adopts their own open shift"
    );

    // Cash moves and the shift closes under the manager's own name.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/cash-movements", managers_shift.id))
            .insert_header(("Authorization", format!("Bearer {}", manager_token)))
            .set_json(&movement_json(-250, Some("pay_out"), None))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/close", managers_shift.id))
            .insert_header(("Authorization", format!("Bearer {}", manager_token)))
            .set_json(&CloseShiftRequest {
                device_id: None,
                reconciliation: None,
                closing_cash_declared: 0,
                cash_note: None,
                closed_at: None,
            })
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let closed: CloseShiftResponse = test::read_body_json(resp).await;
    assert_eq!(closed.shift.closed_by, Some(manager));

    // The teller has gone home: the manager force-closes their shift.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/force-close", tellers_shift.id))
            .insert_header(("Authorization", format!("Bearer {}", manager_token)))
            .set_json(&ForceCloseRequest {
                device_id: None,
                reason: Some("went home".into()),
            })
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200, "a branch manager may force-close");
    let forced: Shift = test::read_body_json(resp).await;
    assert_eq!(forced.status, "force_closed");
    assert_eq!(forced.force_closed_by, Some(manager));
}

// ── Kitchen tickets close with the shift ──────────────────────

/// At a branch with no kitchen screen nothing is ever bumped, so the till
/// queue closes with the branch's last open shift (`kitchen::
/// retire_unbumped_at_shift_close`): `settled` by the closer where the bill
/// was paid, `retired` by nobody where the order was voided. A ticket behind a
/// KDS is left alone.
#[sqlx::test]
async fn test_close_shift_closes_unbumped_kitchen_tickets_in_till_mode(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    // No routing override and no stations → effective mode `till`.
    let branch_id = seed_branch(&pool, org_id).await;
    let kds_branch = seed_branch(&pool, org_id).await;
    sqlx::query("UPDATE branches SET kitchen_routing_mode = 'kds' WHERE id = $1")
        .bind(kds_branch)
        .execute(&pool)
        .await
        .unwrap();

    let (token, shift_id, user_id) =
        open_admin_shift(&app, &pool, org_id, branch_id, None, 0).await;
    let (_, other_shift, other_user) =
        open_admin_shift(&app, &pool, org_id, kds_branch, None, 0).await;

    let order = |branch: Uuid, shift: Uuid, teller: Uuid, n: i32, voided: bool| {
        let pool = pool.clone();
        async move {
            let id = Uuid::new_v4();
            let status = if voided { "voided" } else { "completed" };
            sqlx::query(
                "INSERT INTO orders (id, branch_id, teller_id, till_id, idempotency_key, subtotal, tax_amount, total_amount, status, order_number, payment_method, order_ref, voided_at, voided_by) \
                 VALUES ($1,$2,$3,$4, gen_random_uuid(), 100,0,100,$5::order_status,$6,'cash', gen_random_uuid()::text, \
                         CASE WHEN $7 THEN now() END, CASE WHEN $7 THEN $3 END)",
            )
            .bind(id).bind(branch).bind(teller).bind(shift).bind(status).bind(n).bind(voided)
            .execute(&pool).await.unwrap();
            let kt = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO kitchen_tickets (id, org_id, branch_id, order_id) VALUES ($1,$2,$3,$4)",
            )
            .bind(kt).bind(org_id).bind(branch).bind(id)
            .execute(&pool).await.unwrap();
            kt
        }
    };
    let paid = order(branch_id, shift_id, user_id, 1, false).await;
    let voided = order(branch_id, shift_id, user_id, 2, true).await;
    let elsewhere = order(kds_branch, other_shift, other_user, 3, false).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/close", shift_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&CloseShiftRequest {
                device_id: None,
                reconciliation: None,
                closing_cash_declared: 100,
                cash_note: None,
                closed_at: None,
            })
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);

    let state = |kt: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (Option<String>, Option<Uuid>, String)>(
                "SELECT close_reason::text, closed_by, status::text FROM kitchen_tickets WHERE id = $1",
            )
            .bind(kt)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(
        state(paid).await,
        (Some("settled".into()), Some(user_id), "firing".into()),
        "a paid order's ticket closes as settled by the closer; the cooking state is untouched"
    );
    assert_eq!(
        state(voided).await,
        (Some("retired".into()), None, "firing".into()),
        "a voided order's ticket is retired by nobody"
    );
    assert_eq!(
        state(elsewhere).await,
        (None, None, "firing".into()),
        "another shift's ticket, behind a KDS, stays live"
    );
}

// ── Refunds and the drawer ────────────────────────────────────

/// The Z-report's cash side has to add up after a refund. `payment_summary`
/// is a revenue figure and drops a fully refunded sale by status, exactly as
/// the sales report does; the drawer took that sale's notes all the same and
/// then handed some back. The report says both, in their own lines, and
/// `expected_cash` is their sum: float + cash bucket + cash tips +
/// cash_in_refunded_sales + movements − refunds_issued_cash.
#[sqlx::test]
async fn test_shift_report_reconciles_refunds_against_the_drawer(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    sqlx::query("INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) VALUES ($1,'cash','{}','e','i',true,true)")
        .bind(org_id).execute(&pool).await.unwrap();
    let branch_id = seed_branch(&pool, org_id).await;
    let (token, shift_id, user_id) =
        open_admin_shift(&app, &pool, org_id, branch_id, None, 1000).await;

    let report = |shift: Uuid| {
        let token = token.clone();
        let app = &app;
        async move {
            let resp = test::call_service(
                app,
                test::TestRequest::get()
                    .uri(&format!("/shifts/{}/report", shift))
                    .insert_header(("Authorization", format!("Bearer {}", token)))
                    .to_request(),
            )
            .await;
            assert!(resp.status().is_success());
            let rep: ShiftReportResponse = test::read_body_json(resp).await;
            rep
        }
    };
    let cash_sale = |number: i32, total: i32| {
        let pool = pool.clone();
        async move {
            let order_id = Uuid::new_v4();
            sqlx::query("INSERT INTO orders (id, branch_id, teller_id, till_id, idempotency_key, subtotal, tax_amount, total_amount, status, order_number, payment_method, order_ref) VALUES ($1,$2,$3,$4, gen_random_uuid(), $5,0,$5,'completed',$6,'cash', gen_random_uuid()::text)")
                .bind(order_id).bind(branch_id).bind(user_id).bind(shift_id).bind(total).bind(number).execute(&pool).await.unwrap();
            sqlx::query("INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ($1,'cash',$2,true)")
                .bind(order_id).bind(total).execute(&pool).await.unwrap();
            order_id
        }
    };
    let refund = |order_id: Uuid, amount: i32| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO order_refunds (order_id, till_id, amount, method, is_cash, reason, issued_by) \
                 VALUES ($1, $2, $3, 'cash', true, 'quality_issue', $4)",
            )
            .bind(order_id).bind(shift_id).bind(amount).bind(user_id).execute(&pool).await.unwrap();
        }
    };

    // Two cash sales: 6000 and 2000 → 9000 in the drawer.
    let big = cash_sale(1, 6000).await;
    let small = cash_sale(2, 2000).await;
    let rep = report(shift_id).await;
    assert_eq!(rep.expected_cash, 9000);
    assert_eq!(rep.refunds_issued_count, 0);
    assert_eq!(rep.cash_in_refunded_sales, 0);

    // 500 back on the big one (partial: the sale stays sold) and the small one
    // refunded in full (its status flips; it leaves the revenue lines).
    refund(big, 500).await;
    refund(small, 2000).await;
    let rep = report(shift_id).await;

    let cash_bucket: i64 = rep
        .payment_summary
        .iter()
        .filter(|r| r.is_cash)
        .map(|r| r.total)
        .sum();
    assert_eq!(
        cash_bucket, 6000,
        "the revenue bucket drops the fully refunded sale and keeps the partial one whole"
    );
    assert_eq!(rep.refunds_issued_count, 2);
    assert_eq!(rep.refunds_issued_amount, 2500);
    assert_eq!(rep.refunds_issued_cash, 2500);
    assert_eq!(
        rep.cash_in_refunded_sales, 2000,
        "the notes from the fully refunded sale still went into the drawer"
    );
    assert_eq!(
        rep.expected_cash,
        1000 + 6000 + 2000 - 2500,
        "float + cash sales (all of them) − cash refunded"
    );
    assert_eq!(
        rep.expected_cash,
        rep.shift.opening_cash as i64
            + cash_bucket
            + rep.cash_tips
            + rep.cash_in_refunded_sales
            + rep.cash_movements_net
            - rep.refunds_issued_cash,
        "the sheet adds up from its own lines"
    );
}

/// Tills are per person: several tellers can each have a till open at the same
/// branch at the same time (no per-drawer or per-branch limit).
#[sqlx::test]
async fn many_tills_open_at_once_in_one_branch(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(crate::tills::routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    for a in ["create", "read", "update"] {
        grant_permission(&pool, "teller", "tills", a).await;
    }
    let mut opened = Vec::new();
    for _ in 0..3 {
        let teller = seed_user(&pool, org_id, "teller").await;
        sqlx::query("UPDATE users SET name = $2 WHERE id = $1")
            .bind(teller)
            .bind(format!("Teller {teller}"))
            .execute(&pool)
            .await
            .unwrap();
        assign_user_to_branch(&pool, teller, branch_id).await;
        let token = generate_teller_token(teller, org_id);
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri(&format!("/tills/branches/{branch_id}/open"))
                .insert_header(("Authorization", format!("Bearer {token}")))
                .insert_header(("X-Madar-Device", Uuid::new_v4().to_string()))
                .set_json(serde_json::json!({ "opening_cash": 0 }))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 201);
        let till: serde_json::Value = test::read_body_json(resp).await;
        opened.push((till["id"].as_str().unwrap().to_string(), token));
    }
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/tills/branches/{branch_id}/open"))
            .insert_header(("Authorization", format!("Bearer {}", opened[0].1)))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let open: Vec<serde_json::Value> = test::read_body_json(resp).await;
    assert_eq!(open.len(), 3);
    for (id, _) in &opened {
        assert!(open.iter().any(|t| t["id"] == id.as_str()));
        assert!(open.iter().all(|t| t["status"] == "open"));
    }
}

/// At most one open till per person (live): re-opening on the same device
/// resumes the same till, another device at the branch is refused with
/// `TILL_OPEN_ELSEWHERE`, another branch with `TILL_OPEN_AT_OTHER_BRANCH`.
#[sqlx::test]
async fn at_most_one_open_till_per_person(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(crate::tills::routes::configure),
    )
    .await;
    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;
    let teller = seed_user(&pool, org_id, "teller").await;
    assign_user_to_branch(&pool, teller, branch_a).await;
    assign_user_to_branch(&pool, teller, branch_b).await;
    for a in ["create", "read", "update"] {
        grant_permission(&pool, "teller", "tills", a).await;
    }
    let token = generate_teller_token(teller, org_id);
    let device = Uuid::new_v4();
    let open = |branch: Uuid, device: Uuid| {
        test::TestRequest::post()
            .uri(&format!("/tills/branches/{branch}/open"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .insert_header(("X-Madar-Device", device.to_string()))
            .set_json(serde_json::json!({ "opening_cash": 0 }))
            .to_request()
    };

    let resp = test::call_service(&app, open(branch_a, device)).await;
    assert_eq!(resp.status(), 201);
    let first: serde_json::Value = test::read_body_json(resp).await;

    let resp = test::call_service(&app, open(branch_a, device)).await;
    assert_eq!(resp.status(), 200, "same device resumes");
    let again: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(again["id"], first["id"]);

    let resp = test::call_service(&app, open(branch_a, Uuid::new_v4())).await;
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["code"], "TILL_OPEN_ELSEWHERE");
    assert_eq!(body["till"]["id"], first["id"]);

    let resp = test::call_service(&app, open(branch_b, device)).await;
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["code"], "TILL_OPEN_AT_OTHER_BRANCH");
    assert_eq!(body["till"]["id"], first["id"]);

    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tills WHERE teller_id=$1 AND status='open'")
            .bind(teller)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 1);
}

/// Decision 7 / R1: an offline open that could not be checked is ACCEPTED on
/// replay even though the person already has an open till at the branch; the
/// replayed one is FLAGGED and linked, both stay open, and both are visible —
/// on T1 `/current` (`open_at_branch`, `open_elsewhere`) and on the dashboard's
/// T3 list with `flagged=true`.
#[sqlx::test]
async fn replay_open_till_duplicate_flags_and_both_are_visible(pool: PgPool) {
    use crate::sync::ActingContext;
    use crate::tills::handlers::{
        OpenMeta, OpenTillRequest, PaginatedTills, TillPreFill, open_till_inner,
    };

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "tills", "read").await;
    let (dev_a, dev_b) = (Uuid::new_v4(), Uuid::new_v4());
    let open = |replay: bool, device: Uuid| OpenTillRequest {
        id: Some(Uuid::new_v4()),
        opening_cash: 1000,
        opening_cash_edited: None,
        edit_reason: Some("float".into()),
        opened_at: None,
        device_id: Some(device),
        verification: if replay {
            Some("unverified".into())
        } else {
            None
        },
    };
    let actor = |replay: bool| ActingContext {
        teller_id: user_id,
        org_id,
        role: UserRole::OrgAdmin,
        replay,
    };

    let (first, created) = open_till_inner(
        &pool,
        None,
        branch_id,
        open(false, dev_a),
        actor(false),
        OpenMeta::default(),
    )
    .await
    .unwrap();
    assert!(created && !first.opened_while_another_open);

    // Live, the second open is refused…
    let live = open_till_inner(
        &pool,
        None,
        branch_id,
        open(false, dev_b),
        actor(false),
        OpenMeta::default(),
    )
    .await;
    assert!(matches!(
        live,
        Err(crate::errors::AppError::RefusedWith {
            code: "TILL_OPEN_ELSEWHERE",
            ..
        })
    ));
    // …but the offline one, replayed, is accepted and flagged.
    let (second, created) = open_till_inner(
        &pool,
        None,
        branch_id,
        open(true, dev_b),
        actor(true),
        OpenMeta::default(),
    )
    .await
    .unwrap();
    assert!(created);
    assert!(
        second.opened_while_another_open,
        "the replayed second open is flagged"
    );
    assert_eq!(
        second.other_till_id,
        Some(first.id),
        "and linked to the till that was already open"
    );
    assert!(second.flagged_at.is_some());
    let open_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM tills WHERE teller_id = $1 AND branch_id = $2 AND status = 'open'",
    )
    .bind(user_id)
    .bind(branch_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(open_count, 2, "both stay open");

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(crate::tills::routes::configure),
    )
    .await;
    let token = generate_org_admin_token(user_id, org_id);

    // T1 from device A: resumes A, and still shows the flagged B.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/tills/branches/{branch_id}/current"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .insert_header(("X-Madar-Device", dev_a.to_string()))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success());
    let pre: TillPreFill = test::read_body_json(resp).await;
    assert_eq!(pre.open_till.as_ref().map(|t| t.id), Some(first.id));
    let at_branch: Vec<(Uuid, bool)> = pre
        .open_at_branch
        .iter()
        .map(|t| (t.id, t.opened_while_another_open))
        .collect();
    assert_eq!(
        at_branch,
        vec![(second.id, true), (first.id, false)],
        "both, newest first, flag visible"
    );
    assert!(
        pre.open_elsewhere
            .iter()
            .any(|t| t.id == second.id && t.opened_while_another_open)
    );

    // T1 with no device header: still both.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/tills/branches/{branch_id}/current"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    let pre: TillPreFill = test::read_body_json(resp).await;
    assert_eq!(pre.open_at_branch.len(), 2);

    // Dashboard (T3): the flagged filter finds the second till; the open list has both.
    let list = |q: &'static str| {
        let req = test::TestRequest::get()
            .uri(&format!("/tills/branches/{branch_id}?{q}"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request();
        test::call_service(&app, req)
    };
    let flagged: PaginatedTills =
        test::read_body_json(list("status=open&flagged=true").await).await;
    assert_eq!(
        flagged.data.iter().map(|t| t.id).collect::<Vec<_>>(),
        vec![second.id]
    );
    assert_eq!(flagged.data[0].other_till_id, Some(first.id));
    let all_open: PaginatedTills = test::read_body_json(list("status=open").await).await;
    assert_eq!(all_open.data.len(), 2);
}
