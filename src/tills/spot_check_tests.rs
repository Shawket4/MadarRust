//! Cash spot checks (owner design 2026-09-16 evening, item 5): the live routes,
//! their permission gates, replay with and without a manager's approval, the
//! Z report, and the blind close that flags its discrepancy.
use actix_web::{App, http::StatusCode, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::{JwtSecret, create_token};
use crate::models::UserRole;

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
    crate::permissions::seeder::seed_role_permissions(pool)
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
                .app_data(web::Data::new(crate::realtime::hub::BranchEventHub::new()))
                .configure(crate::tills::legacy_routes::configure)
                .configure(crate::tills::routes::configure)
                .configure(crate::orders::routes::configure)
                .configure(crate::sync::routes::configure),
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

/// Teller A's open till at branch A with a 1000 float and one 5000 cash sale
/// and one 5000 card sale: expected cash 6000.
async fn till_with_sales<S, B>(app: &S) -> Uuid
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
    for method in ["cash", "card"] {
        let (s, _) = call(
            app,
            test::TestRequest::post().uri("/orders").set_json(json!({
                "branch_id": BRANCH_A, "till_id": till, "payment_method": method,
                "idempotency_key": Uuid::new_v4(),
                "items": [{ "menu_item_id": ITEM, "quantity": 1, "unit_price": 5000, "addons": [], "optional_field_ids": [] }],
                "subtotal": 5000, "tax_amount": 0, "total_amount": 5000
            })),
            bearer(TELLER_A, UserRole::Teller),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED);
    }
    till
}

#[::core::prelude::v1::test]
fn plan_methods_puts_cash_first_and_computes_discrepancies() {
    use crate::tills::reconcile::MethodTotal;
    use crate::tills::spot_checks::{SpotCheckMethodInput, plan_methods};
    let server = vec![
        MethodTotal {
            method: "cash".into(),
            payment_method_id: None,
            is_cash: true,
            system_total: 6000,
            order_count: 1,
        },
        MethodTotal {
            method: "card".into(),
            payment_method_id: None,
            is_cash: false,
            system_total: 5000,
            order_count: 1,
        },
    ];
    let lines = plan_methods(&server, None, 5900, 6000);
    assert_eq!(lines.len(), 2);
    assert!(lines[0].is_cash);
    assert_eq!(lines[0].discrepancy, Some(-100));
    assert_eq!(lines[1].counted, None);
    assert_eq!(lines[1].expected, 5000);

    let inputs = vec![
        SpotCheckMethodInput {
            method: "card".into(),
            is_cash: false,
            expected: Some(4000),
            counted: Some(4500),
        },
        SpotCheckMethodInput {
            method: "cash".into(),
            is_cash: true,
            expected: Some(6000),
            counted: Some(6000),
        },
    ];
    let lines = plan_methods(&server, Some(&inputs), 6100, 6000);
    assert_eq!(lines[0].method, "cash");
    assert_eq!(lines[0].discrepancy, Some(100));
    assert_eq!(lines[1].expected, 4000, "the counter's snapshot wins");
    assert_eq!(lines[1].discrepancy, Some(500));
}

#[sqlx::test]
async fn a_manager_records_a_spot_check_and_it_reaches_the_report(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = till_with_sales(&app).await;
    let id = Uuid::new_v4();
    let body = json!({ "id": id, "counted_cash": 5900, "note": "short a note" });
    let (s, check) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{till}/spot-checks"))
            .set_json(&body),
        bearer(MANAGER, UserRole::BranchManager),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{check}");
    assert_eq!(check["expected_cash"], 6000);
    assert_eq!(check["cash_discrepancy"], -100);
    assert_eq!(check["checked_by"], MANAGER);
    assert!(check["approved_by"].is_null());
    assert_eq!(check["methods"][0]["is_cash"], true);
    assert_eq!(check["methods"][1]["method"], "card");
    assert_eq!(check["methods"][1]["expected"], 5000);

    // Same id again: one check.
    let (s, _) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{till}/spot-checks"))
            .set_json(&body),
        bearer(MANAGER, UserRole::BranchManager),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // The owner too (defaults "om").
    let (s, _) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{till}/spot-checks"))
            .set_json(json!({ "counted_cash": 6000, "expected_cash": 6000 })),
        bearer(ADMIN, UserRole::OrgAdmin),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);

    // The teller whose till it is may read them (till.read).
    let (s, list) = call(
        &app,
        test::TestRequest::get().uri(&format!("/tills/{till}/spot-checks")),
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
    assert_eq!(report["spot_checks"].as_array().unwrap().len(), 2);
    assert_eq!(report["expected_cash"], 6000, "a count never moves the drawer");

    // A closed till is not counted live.
    let (s, _) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{till}/close"))
            .set_json(json!({ "closing_cash_declared": 6000 })),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, err) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{till}/spot-checks"))
            .set_json(json!({ "counted_cash": 1 })),
        bearer(MANAGER, UserRole::BranchManager),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{err}");
}

#[sqlx::test]
async fn without_the_permission_the_routes_refuse_before_looking_at_the_till(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = till_with_sales(&app).await;
    // A teller does not hold till.cash_spot_check by default.
    let (s, _) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{till}/spot-checks"))
            .set_json(json!({ "counted_cash": 6000 })),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Permission first: an unknown till and a broken body still answer 403.
    let (s, _) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{}/spot-checks", Uuid::new_v4()))
            .set_json(json!({ "nonsense": true })),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // A waiter cannot read tills.
    let (s, _) = call(
        &app,
        test::TestRequest::get().uri(&format!("/tills/{}/spot-checks", Uuid::new_v4())),
        bearer(WAITER, UserRole::Waiter),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // No token at all.
    let r = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/tills/{till}/spot-checks"))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM till_spot_checks")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

fn spot_op(till: Uuid, id: Uuid, approver: Option<&str>) -> Value {
    let mut op = json!({
        "op": "cash_spot_check",
        "teller_id": TELLER_A,
        "till_id": till,
        "request": {
            "id": id, "counted_cash": 5500, "expected_cash": 6000,
            "methods": [
                { "method": "cash", "is_cash": true, "expected": 6000, "counted": 5500 },
                { "method": "card", "is_cash": false, "expected": 5000, "counted": 5000 }
            ],
            "checked_at": "2026-09-17T09:00:00Z"
        }
    });
    if let Some(a) = approver {
        op["approval"] = json!({ "id": Uuid::new_v4(), "capability": "till.cash_spot_check", "approver_id": a });
    }
    op
}

#[sqlx::test]
async fn a_tellers_queued_spot_check_is_kept_and_flagged_without_an_approval(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = till_with_sales(&app).await;
    let id = Uuid::new_v4();
    let (s, row) = call(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .set_json(spot_op(till, id, None)),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "accepted: the count happened: {row}");
    assert_eq!(row["cash_discrepancy"], -500);
    assert_eq!(row["methods"][1]["discrepancy"], 0);
    let flags: Vec<(String, String)> = sqlx::query_as(
        "SELECT op, capability FROM authz_replay_flags WHERE author_id = $1",
    )
    .bind(uid(TELLER_A))
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        flags,
        vec![("CashSpotCheck".to_string(), "till.cash_spot_check".to_string())]
    );
    // Replayed twice: one row, one flag.
    let (s, _) = call(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .set_json(spot_op(till, id, None)),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM till_spot_checks WHERE till_id = $1")
        .bind(till)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test]
async fn a_managers_pin_unlocks_one_spot_check_for_a_teller(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = till_with_sales(&app).await;

    // Self-approval approves nothing: kept, but flagged, and no approver named.
    let (s, row) = call(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .set_json(spot_op(till, Uuid::new_v4(), Some(TELLER_A))),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert!(row["approved_by"].is_null());

    let (s, row) = call(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .set_json(spot_op(till, Uuid::new_v4(), Some(MANAGER))),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{row}");
    assert_eq!(row["approved_by"], MANAGER);
    assert_eq!(row["approved_by_name"], "Manager Mike");
    assert_eq!(row["checked_by"], TELLER_A);
    let flags: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM authz_replay_flags WHERE author_id = $1 AND capability = 'till.cash_spot_check'",
    )
    .bind(uid(TELLER_A))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(flags, 1, "only the self-approved one is flagged");
    let verified: Vec<bool> = sqlx::query_scalar(
        "SELECT verified FROM approvals WHERE subject_user_id = $1 AND op = 'CashSpotCheck' ORDER BY verified",
    )
    .bind(uid(TELLER_A))
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(verified, vec![false, true]);
}

#[sqlx::test]
async fn a_blind_close_with_a_discrepancy_lands_in_the_review_queue(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = till_with_sales(&app).await;
    // The teller counts blind (no spot check, no preview) and closes 300 short.
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
    let flagged = list["data"].as_array().unwrap();
    assert!(
        flagged
            .iter()
            .any(|t| t["id"] == till.to_string() && t["reconciliation_status"] == "disagreed"),
        "{list}"
    );
    // The finished report the teller previews after closing.
    let (s, report) = call(
        &app,
        test::TestRequest::get().uri(&format!("/tills/{till}/report")),
        bearer(TELLER_A, UserRole::Teller),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(report["till"]["cash_discrepancy"], -300);
}
