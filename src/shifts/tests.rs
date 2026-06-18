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
    grant_permission(&pool, "org_admin", "shifts", "read").await;
    grant_permission(&pool, "org_admin", "shifts", "create").await;
    grant_permission(&pool, "org_admin", "shifts", "update").await;
    let token = generate_org_admin_token(user_id, org_id);

    let open = |opening: i32, reason: Option<String>| {
        let app = &app;
        let token = token.clone();
        async move {
            let req = test::TestRequest::post()
                .uri(&format!("/shifts/branches/{}/open", branch_id))
                .insert_header(("Authorization", format!("Bearer {}", token)))
                .set_json(&OpenShiftRequest {
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
    let move_req = CashMovementRequest { amount: -500, note: "Paid vendor".into(), created_at: None };
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
    // Closed shift → expected_cash is the snapshot taken at close.
    assert_eq!(rep.expected_cash, 1000);
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
    grant_permission(&pool, "org_admin", "shifts", "update").await;

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
    assert_eq!(test::call_service(&app, req_del_open).await.status().as_u16(), 409);

    // Force-close it (admin), then the empty shift can be deleted.
    let req_fc = test::TestRequest::post()
        .uri(&format!("/shifts/{}/force-close", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", admin_token)))
        .set_json(&ForceCloseRequest { reason: Some("cleanup".into()) })
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
async fn test_list_shifts_all_branches(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    ).await;

    let org_id   = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;
    let admin    = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "shifts", "read").await;
    let token = generate_org_admin_token(admin, org_id);

    // One closed shift in each branch (closed → no one-open-per-teller clash).
    for branch in [branch_a, branch_b] {
        sqlx::query(
            "INSERT INTO shifts (id, branch_id, teller_id, status, opening_cash, closing_cash_declared, closed_at)
             VALUES ($1,$2,$3,'closed',10000,10000,NOW())")
            .bind(Uuid::new_v4()).bind(branch).bind(admin).execute(&pool).await.unwrap();
    }
    // A different org's shift must never appear in this org's all-branches view.
    let other_org    = seed_org(&pool).await;
    let other_branch = seed_branch(&pool, other_org).await;
    let other_admin  = seed_user(&pool, other_org, "org_admin").await;
    sqlx::query("INSERT INTO shifts (id, branch_id, teller_id, status, opening_cash) VALUES ($1,$2,$3,'open',5000)")
        .bind(Uuid::new_v4()).bind(other_branch).bind(other_admin).execute(&pool).await.unwrap();

    let auth = ("Authorization", format!("Bearer {token}"));

    // All branches (nil UUID): both org branches' shifts, branch-labelled, org-isolated.
    // No pagination params → one page holding everything (dashboard-compatible).
    let nil = Uuid::nil();
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/shifts/branches/{nil}")).insert_header(auth.clone()).to_request()).await;
    assert_eq!(resp.status(), 200);
    let page: PaginatedShifts = test::read_body_json(resp).await;
    assert_eq!(page.total, 2, "all-branches sees both org branches' shifts");
    assert_eq!(page.total_pages, 1, "no pagination params → single page");
    let shifts = page.data;
    assert_eq!(shifts.len(), 2);
    assert!(shifts.iter().all(|s| s.branch_name.is_some()), "rows carry a branch label");
    let seen: std::collections::HashSet<_> = shifts.iter().map(|s| s.branch_id).collect();
    assert!(seen.contains(&branch_a) && seen.contains(&branch_b));

    // Opt-in pagination: per_page=1 slices the result while reporting the full total.
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/shifts/branches/{nil}?page=1&per_page=1")).insert_header(auth.clone()).to_request()).await;
    assert_eq!(resp.status(), 200);
    let paged: PaginatedShifts = test::read_body_json(resp).await;
    assert_eq!(paged.total, 2);
    assert_eq!(paged.per_page, 1);
    assert_eq!(paged.total_pages, 2);
    assert_eq!(paged.data.len(), 1, "one row per page");

    // A specific branch still scopes to that one branch.
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/shifts/branches/{branch_a}")).insert_header(auth.clone()).to_request()).await;
    assert_eq!(resp.status(), 200);
    let just_a: PaginatedShifts = test::read_body_json(resp).await;
    assert_eq!(just_a.total, 1);
    assert_eq!(just_a.data.len(), 1);
    assert_eq!(just_a.data[0].branch_id, branch_a);
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

/// V30: a closed shift's cash uses the SALE-TIME is_cash snapshot, so flipping
/// is_cash (or renaming) the payment method afterward does NOT change history.
#[sqlx::test]
async fn test_close_cash_uses_is_cash_snapshot(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;
    let org_id = seed_org(&pool).await;
    sqlx::query("INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) VALUES ($1,'cash','{}','e','i',true,true)")
        .bind(org_id).execute(&pool).await.unwrap();
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "shifts", "create").await;
    grant_permission(&pool, "org_admin", "shifts", "update").await;
    let token = generate_org_admin_token(user_id, org_id);

    // Open a shift with 1000 opening cash.
    let shift_id = Uuid::new_v4();
    let open = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/shifts/branches/{}/open", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&OpenShiftRequest { id: Some(shift_id), opening_cash: 1000, opening_cash_edited: None, edit_reason: None, opened_at: None })
        .to_request()).await;
    assert!(open.status().is_success());

    // A completed CASH order of 500, with order_payments.is_cash snapshotted true.
    let order_id = Uuid::new_v4();
    sqlx::query("INSERT INTO orders (id, branch_id, teller_id, shift_id, idempotency_key, subtotal, tax_amount, total_amount, status, order_number, payment_method, order_ref) VALUES ($1,$2,$3,$4, gen_random_uuid(), 500,0,500,'completed',1,'cash', gen_random_uuid()::text)")
        .bind(order_id).bind(branch_id).bind(user_id).bind(shift_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ($1,'cash',500,true)")
        .bind(order_id).execute(&pool).await.unwrap();

    // CORRUPTION: the 'cash' method is later flipped to NOT cash.
    sqlx::query("UPDATE org_payment_methods SET is_cash=false WHERE org_id=$1 AND name='cash'")
        .bind(org_id).execute(&pool).await.unwrap();

    // Close: system cash must still be opening 1000 + the cash order 500 = 1500,
    // because is_cash was snapshotted at sale time (not read from current config).
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/shifts/{}/close", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&CloseShiftRequest { closing_cash_declared: 1500, cash_note: None, closed_at: None })
        .to_request()).await;
    assert!(resp.status().is_success());
    let closed: CloseShiftResponse = test::read_body_json(resp).await;
    assert_eq!(closed.shift.closing_cash_system.unwrap(), 1500, "cash order must still count via the sale-time snapshot");
}

/// A teller may close ONLY their own shift — closing settles cash, so it must be
/// attributed to the right person. A second teller (same branch) is rejected.
#[sqlx::test]
async fn test_teller_cannot_close_another_tellers_shift(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure)
    ).await;
    let org_id   = seed_org(&pool).await;
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
    for a in ["create", "read", "update"] { grant_permission(&pool, "teller", "shifts", a).await; }
    let token_a = generate_teller_token(teller_a, org_id);
    let token_b = generate_teller_token(teller_b, org_id);

    // Teller A opens the (only) shift for the branch.
    let shift_id = Uuid::new_v4();
    let open = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/shifts/branches/{}/open", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token_a)))
        .set_json(&OpenShiftRequest { id: Some(shift_id), opening_cash: 0, opening_cash_edited: None, edit_reason: None, opened_at: None })
        .to_request()).await;
    assert!(open.status().is_success());

    // Teller B (same branch) cannot close A's shift.
    let resp_b = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/shifts/{}/close", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", token_b)))
        .set_json(&CloseShiftRequest { closing_cash_declared: 0, cash_note: None, closed_at: None })
        .to_request()).await;
    assert_eq!(resp_b.status().as_u16(), 403, "a teller cannot close another teller's shift");

    // The shift is still open afterwards.
    let still_open: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM shifts WHERE id=$1 AND status='open')")
        .bind(shift_id).fetch_one(&pool).await.unwrap();
    assert!(still_open);

    // Its owner CAN close it.
    let resp_a = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/shifts/{}/close", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", token_a)))
        .set_json(&CloseShiftRequest { closing_cash_declared: 0, cash_note: None, closed_at: None })
        .to_request()).await;
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
            .configure(routes::configure)
    ).await;
    let org_id    = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let admin     = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "shifts", "create").await;
    grant_permission(&pool, "org_admin", "shifts", "update").await;
    let token = generate_org_admin_token(admin, org_id);

    let shift_id = Uuid::new_v4();
    let open = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/shifts/branches/{}/open", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&OpenShiftRequest { id: Some(shift_id), opening_cash: 0, opening_cash_edited: None, edit_reason: None, opened_at: None })
        .to_request()).await;
    assert!(open.status().is_success());

    // A recorded (non-voided) order on the shift.
    sqlx::query("INSERT INTO orders (id, branch_id, teller_id, shift_id, idempotency_key, subtotal, tax_amount, total_amount, status, order_number, payment_method, order_ref) VALUES (gen_random_uuid(),$1,$2,$3, gen_random_uuid(), 500,0,500,'completed',1,'cash', gen_random_uuid()::text)")
        .bind(branch_id).bind(admin).bind(shift_id).execute(&pool).await.unwrap();

    // Force-close so the only barrier left is the recorded-order guard.
    let fc = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/shifts/{}/force-close", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&ForceCloseRequest { reason: Some("x".into()) })
        .to_request()).await;
    assert!(fc.status().is_success());

    // Delete is refused — the sale is part of the financial record.
    let del = test::call_service(&app, test::TestRequest::delete()
        .uri(&format!("/shifts/{}", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request()).await;
    assert_eq!(del.status().as_u16(), 409, "cannot delete a shift with recorded orders");

    // The shift and its order are still there.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM shifts WHERE id=$1").bind(shift_id).fetch_one(&pool).await.unwrap();
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
            .configure(routes::configure)
    ).await;
    let org_id = seed_org(&pool).await;
    sqlx::query("INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) VALUES ($1,'cash','{}','e','i',true,true)")
        .bind(org_id).execute(&pool).await.unwrap();
    let branch_id = seed_branch(&pool, org_id).await;
    let admin = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "shifts", "create").await;
    grant_permission(&pool, "org_admin", "shifts", "update").await;
    let token = generate_org_admin_token(admin, org_id);

    // Open with 1000 float.
    let shift_id = Uuid::new_v4();
    let open = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/shifts/branches/{}/open", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&OpenShiftRequest { id: Some(shift_id), opening_cash: 1000, opening_cash_edited: None, edit_reason: None, opened_at: None })
        .to_request()).await;
    assert!(open.status().is_success());

    // A 500 cash sale lands in the drawer.
    let order_id = Uuid::new_v4();
    sqlx::query("INSERT INTO orders (id, branch_id, teller_id, shift_id, idempotency_key, subtotal, tax_amount, total_amount, status, order_number, payment_method, order_ref) VALUES ($1,$2,$3,$4, gen_random_uuid(), 500,0,500,'completed',1,'cash', gen_random_uuid()::text)")
        .bind(order_id).bind(branch_id).bind(admin).bind(shift_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ($1,'cash',500,true)")
        .bind(order_id).execute(&pool).await.unwrap();

    // Force-close (no declared count collected) still snapshots system cash = 1500.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/shifts/{}/force-close", shift_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&ForceCloseRequest { reason: Some("absent teller".into()) })
        .to_request()).await;
    assert!(resp.status().is_success());
    let shift: Shift = test::read_body_json(resp).await;
    assert_eq!(shift.status, "force_closed");
    assert_eq!(shift.closing_cash_system.unwrap(), 1500, "force-close must freeze expected cash");
    assert!(shift.closing_cash_declared.is_none(), "no declared count at force-close");
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
    ).await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "shifts", "create").await;
    grant_permission(&pool, "org_admin", "shifts", "update").await;
    grant_permission(&pool, "org_admin", "shifts", "read").await;
    let token = generate_org_admin_token(user_id, org_id);

    let open = |id: Uuid, opened_at: Option<chrono::DateTime<chrono::Utc>>| {
        test::TestRequest::post()
            .uri(&format!("/shifts/branches/{}/open", branch_id))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&OpenShiftRequest {
                id: Some(id), opening_cash: 1000, opening_cash_edited: None, edit_reason: None, opened_at,
            })
            .to_request()
    };

    // Future opened_at -> rejected (no shift created).
    let resp = test::call_service(&app, open(Uuid::new_v4(), Some(chrono::Utc::now() + chrono::Duration::minutes(30)))).await;
    assert_eq!(resp.status(), 400, "future opened_at must be rejected");

    // Past opened_at -> honored verbatim.
    let sid = Uuid::new_v4();
    let backdated = chrono::Utc::now() - chrono::Duration::hours(6);
    let resp = test::call_service(&app, open(sid, Some(backdated))).await;
    assert!(resp.status().is_success(), "past opened_at must be honored: {:?}", resp.status());
    let stored: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT opened_at FROM shifts WHERE id=$1").bind(sid).fetch_one(&pool).await.unwrap();
    assert_eq!(stored.timestamp(), backdated.timestamp(), "opened_at must round-trip");

    // Close with a future closed_at -> rejected.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/shifts/{}/close", sid))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&CloseShiftRequest {
            closing_cash_declared: 1000, cash_note: None,
            closed_at: Some(chrono::Utc::now() + chrono::Duration::minutes(30)),
        })
        .to_request()).await;
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
    ).await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "shifts", "create").await;
    let token = generate_org_admin_token(user_id, org_id);

    let sid = Uuid::new_v4();
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/shifts/branches/{}/open", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&OpenShiftRequest {
            id: Some(sid), opening_cash: 1000, opening_cash_edited: None, edit_reason: None, opened_at: None,
        })
        .to_request()).await;
    assert!(resp.status().is_success(), "open without opened_at must succeed: {:?}", resp.status());
    let stored: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT opened_at FROM shifts WHERE id=$1").bind(sid).fetch_one(&pool).await.unwrap();
    assert!((chrono::Utc::now() - stored).num_seconds().abs() < 120, "server-stamped near now");
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
    ).await;
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    grant_permission(&pool, "org_admin", "shifts", "create").await;
    grant_permission(&pool, "org_admin", "shifts", "update").await;
    let token = generate_org_admin_token(user_id, org_id);

    let sid = Uuid::new_v4();
    test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/shifts/branches/{}/open", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&OpenShiftRequest {
            id: Some(sid), opening_cash: 1000, opening_cash_edited: None, edit_reason: None, opened_at: None,
        })
        .to_request()).await;

    let movement = |created_at: Option<chrono::DateTime<chrono::Utc>>| {
        test::TestRequest::post()
            .uri(&format!("/shifts/{}/cash-movements", sid))
            .insert_header(("Authorization", format!("Bearer {}", token)))
            .set_json(&CashMovementRequest { amount: -500, note: "vendor".into(), created_at })
            .to_request()
    };

    // Omitted -> server-stamped near now.
    let resp = test::call_service(&app, movement(None)).await;
    assert!(resp.status().is_success());
    let m: CashMovement = test::read_body_json(resp).await;
    assert!((chrono::Utc::now() - m.created_at).num_seconds().abs() < 120, "server-stamped near now");

    // Past -> honored.
    let past = chrono::Utc::now() - chrono::Duration::hours(3);
    let resp = test::call_service(&app, movement(Some(past))).await;
    assert!(resp.status().is_success(), "past created_at must be honored: {:?}", resp.status());
    let m: CashMovement = test::read_body_json(resp).await;
    assert_eq!(m.created_at.timestamp(), past.timestamp(), "created_at round-trips");

    // Future -> rejected.
    let resp = test::call_service(&app, movement(Some(chrono::Utc::now() + chrono::Duration::minutes(30)))).await;
    assert_eq!(resp.status(), 400, "future created_at must be rejected");
}
