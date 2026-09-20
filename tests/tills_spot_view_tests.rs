//! Cash spot report views (owner design 2026-09-16 item 5, corrected
//! 2026-09-17): the audit trail of who viewed / printed the live till report,
//! the permission gate (live route honours a one-time unlock), replay with and
//! without an approval, the Z report, and the blind close's review queue.
use actix_web::{App, http::StatusCode, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::models::UserRole;

const SECRET: &str = "test_secret";
const ORG: &str = "10000000-0000-4000-8000-000000000001";
const BRANCH_A: &str = "10000000-0000-4000-8000-0000000000a1";
const ADMIN: &str = "10000000-0000-4000-8000-00000000ad01";
const TELLER_A: &str = "10000000-0000-4000-8000-00000000ee0a";
const WAITER: &str = "10000000-0000-4000-8000-00000000aa01";
const MANAGER: &str = "10000000-0000-4000-8000-00000000bb01";
const ITEM: &str = "10000000-0000-4000-8000-0000000e0001";

fn uid(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap()
}

fn bearer(user: &str, role: UserRole) -> (&'static str, String) {
    let tok = create_token(
        &JwtSecret(SECRET.into()),
        uid(user),
        Some(uid(ORG)),
        role,
        None,
        24,
    )
    .unwrap();
    ("Authorization", format!("Bearer {tok}"))
}

async fn seeded(pool: &PgPool) {
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    let seed = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/legacy_till_golden/seed.sql"
    ))
    .unwrap();
    sqlx::raw_sql(&seed).execute(pool).await.expect("seed.sql");
    sqlx::raw_sql(&format!(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES
           ('{MANAGER}', '{ORG}', 'Manager Mike', 'mike@golden.test', 'x', 'branch_manager');
         INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ('{MANAGER}', '{BRANCH_A}');"
    ))
    .execute(pool)
    .await
    .expect("manager");
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(JwtSecret(SECRET.into())))
                .app_data(web::Data::new(madar_rust::realtime::hub::BranchEventHub::new()))
                .configure(madar_rust::tills::legacy_routes::configure)
                .configure(madar_rust::tills::routes::configure)
                .configure(madar_rust::orders::routes::configure)
                .configure(madar_rust::sync::routes::configure),
        )
        .await
    };
}

async fn call<S, B>(
    app: &S,
    req: test::TestRequest,
    who: (&'static str, String),
) -> (StatusCode, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    let r = test::call_service(app, req.insert_header(who).to_request()).await;
    let status = r.status();
    let body = test::read_body(r).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// Teller A's open till at branch A with one cash sale.
async fn open_till<S, B>(app: &S) -> Uuid
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    let till = Uuid::new_v4();
    let (s, _) = call(
        app,
        test::TestRequest::post()
            .uri(&format!("/tills/branches/{BRANCH_A}/open"))
            .set_json(json!({ "id": till, "opening_cash": 1000 })),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert!(s.is_success(), "open: {s}");
    let (s, _) = call(
        app,
        test::TestRequest::post().uri("/orders").set_json(json!({
            "branch_id": BRANCH_A, "till_id": till, "payment_method": "cash",
            "idempotency_key": Uuid::new_v4(),
            "items": [{ "menu_item_id": ITEM, "quantity": 1, "unit_price": 5000, "addons": [], "optional_field_ids": [] }],
            "subtotal": 5000, "tax_amount": 0, "total_amount": 5000
        })),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    till
}

#[sqlx::test]
async fn a_manager_views_and_prints_the_spot_report_and_the_z_report_lists_it(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = open_till(&app).await;
    let id = Uuid::new_v4();
    let (s, row) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{till}/spot-views"))
            .set_json(json!({ "id": id })),
        bearer(MANAGER, UserRole::BranchManager),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{row}");
    assert_eq!(row["viewed_by"], MANAGER);
    assert_eq!(row["printed"], false);
    assert!(row["approved_by"].is_null());
    assert!(
        row.get("counted_cash").is_none(),
        "no amounts in a spot view"
    );

    // The print of the same view marks the same row.
    let (s, row) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{till}/spot-views"))
            .set_json(json!({ "id": id, "printed": true })),
        bearer(MANAGER, UserRole::BranchManager),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(row["printed"], true);
    assert!(row["printed_at"].is_string());

    // The owner too (defaults "om").
    let (s, _) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{till}/spot-views"))
            .set_json(json!({})),
        bearer(ADMIN, UserRole::OrgAdmin),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);

    let (s, list) = call(
        &app,
        test::TestRequest::get().uri(&format!("/tills/{till}/spot-views")),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert_eq!(list[0]["id"], id.to_string());

    let (s, report) = call(
        &app,
        test::TestRequest::get().uri(&format!("/tills/{till}/report")),
        bearer(ADMIN, UserRole::OrgAdmin),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(report["spot_views"].as_array().unwrap().len(), 2);
    assert_eq!(
        report["expected_cash"], 6000,
        "a view never moves the drawer"
    );

    // A closed till has no live spot report.
    let (s, _) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{till}/close"))
            .set_json(json!({ "closing_cash_declared": 6000 })),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{till}/spot-views"))
            .set_json(json!({})),
        bearer(MANAGER, UserRole::BranchManager),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn without_the_permission_the_routes_refuse_before_looking_at_the_till(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = open_till(&app).await;
    let (s, _) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{till}/spot-views"))
            .set_json(json!({})),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Permission first: an unknown till and a broken body still answer 403.
    let (s, _) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{}/spot-views", Uuid::new_v4()))
            .set_payload("not json"),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // An approval the teller made for themself unlocks nothing.
    let (s, _) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{till}/spot-views"))
            .set_json(json!({ "approval": { "id": Uuid::new_v4(), "capability": "till.cash_spot_check", "approver_id": TELLER_A } })),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(
        &app,
        test::TestRequest::get().uri(&format!("/tills/{}/spot-views", Uuid::new_v4())),
        bearer(WAITER, UserRole::Waiter),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let r = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/tills/{till}/spot-views"))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM till_spot_views")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[sqlx::test]
async fn the_live_route_honours_a_one_time_unlock_once(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = open_till(&app).await;
    let approval = Uuid::new_v4();
    let unlock =
        json!({ "id": approval, "capability": "till.cash_spot_check", "approver_id": MANAGER });
    let (s, row) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{till}/spot-views"))
            .set_json(json!({ "approval": unlock, "printed": true })),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{row}");
    assert_eq!(row["viewed_by"], TELLER_A);
    assert_eq!(row["approved_by"], MANAGER);
    assert_eq!(row["approved_by_name"], "Manager Mike");
    assert_eq!(row["printed"], true);
    let verified: bool = sqlx::query_scalar(
        "SELECT verified FROM approvals WHERE id = $1 AND op = 'SpotReportView'",
    )
    .bind(approval)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(verified);
    // The same unlock for a second view is spent.
    let (s, _) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{till}/spot-views"))
            .set_json(json!({ "approval": unlock })),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

fn view_op(till: Uuid, id: Uuid, approver: Option<&str>) -> Value {
    let mut op = json!({
        "op": "spot_report_view",
        "teller_id": TELLER_A,
        "till_id": till,
        "request": { "id": id, "printed": true, "viewed_at": "2026-09-17T09:00:00Z" }
    });
    if let Some(a) = approver {
        op["approval"] =
            json!({ "id": Uuid::new_v4(), "capability": "till.cash_spot_check", "approver_id": a });
    }
    op
}

#[sqlx::test]
async fn a_tellers_queued_view_is_kept_and_flagged_without_an_approval(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = open_till(&app).await;
    let id = Uuid::new_v4();
    let (s, row) = call(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .set_json(view_op(till, id, None)),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "accepted: it was seen: {row}");
    let flags: Vec<(String, String)> =
        sqlx::query_as("SELECT op, capability FROM authz_replay_flags WHERE author_id = $1")
            .bind(uid(TELLER_A))
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        flags,
        vec![(
            "SpotReportView".to_string(),
            "till.cash_spot_check".to_string()
        )]
    );
    let (s, _) = call(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .set_json(view_op(till, id, None)),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM till_spot_views WHERE till_id = $1")
        .bind(till)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test]
async fn a_managers_pin_unlocks_one_queued_view_for_a_teller(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = open_till(&app).await;
    let (s, row) = call(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .set_json(view_op(till, Uuid::new_v4(), Some(TELLER_A))),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert!(
        row["approved_by"].is_null(),
        "self-approval approves nothing"
    );
    let (s, row) = call(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .set_json(view_op(till, Uuid::new_v4(), Some(MANAGER))),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{row}");
    assert_eq!(row["approved_by"], MANAGER);
    let flags: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM authz_replay_flags WHERE author_id = $1 AND capability = 'till.cash_spot_check'",
    )
    .bind(uid(TELLER_A))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(flags, 1, "only the self-approved one is flagged");
}

#[sqlx::test]
async fn a_blind_close_with_a_discrepancy_lands_in_the_review_queue(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = open_till(&app).await;
    let (s, out) = call(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .set_json(json!({
                "op": "close_till", "teller_id": TELLER_A, "till_id": till,
                "request": { "closing_cash_declared": 5700 }
            })),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert!(s.is_success(), "{out}");
    let (s, list) = call(
        &app,
        test::TestRequest::get().uri(&format!("/tills/branches/{BRANCH_A}?flagged=true")),
        bearer(ADMIN, UserRole::OrgAdmin),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == till.to_string() && t["reconciliation_status"] == "disagreed"),
        "{list}"
    );
    let (s, report) = call(
        &app,
        test::TestRequest::get().uri(&format!("/tills/{till}/report")),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(report["till"]["cash_discrepancy"], -300);
}
