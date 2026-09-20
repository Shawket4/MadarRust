//! The server half of the widened `till.cash_spot_check` (owner, 2026-09-19):
//! an OPEN till's expected figures are refused to a new POS client that does
//! not hold the capability, allowed with it, allowed with a valid one-time
//! manager-PIN unlock (and recorded), and — this is the part that must never
//! slip — served exactly as before to every older client and to the dashboard.
use actix_web::{App, http::StatusCode, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::models::UserRole;
use madar_rust::tills::figures_guard::APPROVAL_HEADER;

const SECRET: &str = "test_secret";
const ORG: &str = "10000000-0000-4000-8000-000000000001";
const OTHER_ORG: &str = "10000000-0000-4000-8000-000000000002";
const BRANCH_A: &str = "10000000-0000-4000-8000-0000000000a1";
const TELLER_A: &str = "10000000-0000-4000-8000-00000000ee0a";
const MANAGER: &str = "10000000-0000-4000-8000-00000000bb01";
const FOREIGN: &str = "10000000-0000-4000-8000-00000000cc01";
const TELLER_B: &str = "10000000-0000-4000-8000-00000000ee0b";
const ITEM: &str = "10000000-0000-4000-8000-0000000e0001";

/// The build the rule applies to, and the two that it must not touch.
const NEW_POS: &str = "pos/0.7.11 (android)";
const FIELD_POS: &str = "pos/0.7.10 (android)";
const OLD_POS: &str = "pos/0.7.8 (ios)";

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
         INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ('{MANAGER}', '{BRANCH_A}');
         INSERT INTO organizations (id, name) VALUES ('{OTHER_ORG}', 'Elsewhere')
           ON CONFLICT (id) DO NOTHING;
         INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES
           ('{FOREIGN}', '{OTHER_ORG}', 'Manager Elsewhere', 'far@golden.test', 'x', 'branch_manager');
         -- Teller Bravo works branch A as well, so 'another teller' is
         -- refused the unlock over the RULE, not over branch access.
         INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ('{TELLER_B}', '{BRANCH_A}')
           ON CONFLICT DO NOTHING;"
    ))
    .execute(pool)
    .await
    .expect("manager + a manager of another org");
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(JwtSecret(SECRET.into())))
                .app_data(web::Data::new(madar_rust::realtime::hub::BranchEventHub::new()))
                .configure(madar_rust::tills::routes::configure)
                .configure(madar_rust::orders::routes::configure),
        )
        .await
    };
}

type Headers = Vec<(&'static str, String)>;

async fn call<S, B>(app: &S, req: test::TestRequest, headers: Headers) -> (StatusCode, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    let mut req = req;
    for h in headers {
        req = req.insert_header(h);
    }
    let r = test::call_service(app, req.to_request()).await;
    let status = r.status();
    let body = test::read_body(r).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// `as_who`, on `client` (none = no `X-Madar-Client` at all).
fn who(user: &str, role: UserRole, client: Option<&str>) -> Headers {
    let mut h = vec![bearer(user, role)];
    if let Some(c) = client {
        h.push(("X-Madar-Client", c.to_string()));
    }
    h
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
    let (s, b) = call(
        app,
        test::TestRequest::post()
            .uri(&format!("/tills/branches/{BRANCH_A}/open"))
            .set_json(json!({ "id": till, "opening_cash": 1000 })),
        who(TELLER_A, UserRole::Teller, None),
    )
    .await;
    assert!(s.is_success(), "open: {s} {b}");
    let (s, _) = call(
        app,
        test::TestRequest::post().uri("/orders").set_json(json!({
            "branch_id": BRANCH_A, "till_id": till, "payment_method": "cash",
            "idempotency_key": Uuid::new_v4(),
            "items": [{ "menu_item_id": ITEM, "quantity": 1, "unit_price": 5000, "addons": [], "optional_field_ids": [] }],
            "subtotal": 5000, "tax_amount": 0, "total_amount": 5000
        })),
        who(TELLER_A, UserRole::Teller, None),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    till
}

fn unlock(approver: &str) -> (&'static str, String) {
    (
        APPROVAL_HEADER,
        json!({ "id": Uuid::new_v4(), "capability": "till.cash_spot_check", "approver_id": approver })
            .to_string(),
    )
}

// ── the rule itself ──────────────────────────────────────────────────────────

#[sqlx::test]
async fn a_new_pos_without_the_capability_is_refused_a_live_tills_figures(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = open_till(&app).await;

    for uri in [
        format!("/tills/{till}/report"),
        format!("/tills/{till}/close-preview"),
    ] {
        let (s, _) = call(
            &app,
            test::TestRequest::get().uri(&uri),
            who(TELLER_A, UserRole::Teller, Some(NEW_POS)),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{uri} without the grant");
    }

    // The manager holds `till.cash_spot_check` by default ("om").
    for uri in [
        format!("/tills/{till}/report"),
        format!("/tills/{till}/close-preview"),
    ] {
        let (s, b) = call(
            &app,
            test::TestRequest::get().uri(&uri),
            who(MANAGER, UserRole::BranchManager, Some(NEW_POS)),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{uri} with the grant: {b}");
    }
}

#[sqlx::test]
async fn owner_decision_1_a_closed_tills_finished_report_is_never_gated(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = open_till(&app).await;
    let (s, b) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{till}/close"))
            .set_json(json!({ "closing_cash_declared": 6000 })),
        who(TELLER_A, UserRole::Teller, Some(NEW_POS)),
    )
    .await;
    assert!(s.is_success(), "close: {s} {b}");

    // The blind teller who just counted gets the finished report — on screen,
    // printed, and into the device's own mirror.
    let (s, report) = call(
        &app,
        test::TestRequest::get().uri(&format!("/tills/{till}/report")),
        who(TELLER_A, UserRole::Teller, Some(NEW_POS)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{report}");
    assert_eq!(report["expected_cash"], 6000);
}

#[sqlx::test]
async fn a_one_time_manager_pin_unlock_opens_the_live_figures_and_is_recorded(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = open_till(&app).await;
    let key = unlock(MANAGER);
    let approval_id = serde_json::from_str::<Value>(&key.1).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let mut h = who(TELLER_A, UserRole::Teller, Some(NEW_POS));
    h.push(key.clone());
    let (s, report) = call(
        &app,
        test::TestRequest::get().uri(&format!("/tills/{till}/report")),
        h.clone(),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "the unlock opens it: {report}");
    assert_eq!(report["expected_cash"], 6000);

    // Recorded, verified, attributed — the owner's review queue sees the read.
    let row: (String, Uuid, Uuid, bool) = sqlx::query_as(
        "SELECT op, subject_user_id, approver_user_id, verified FROM approvals WHERE id = $1",
    )
    .bind(uid(&approval_id))
    .fetch_one(&pool)
    .await
    .expect("the read was recorded");
    assert_eq!(row.0, madar_rust::tills::figures_guard::OP_REPORT);
    assert_eq!(row.1, uid(TELLER_A));
    assert_eq!(row.2, uid(MANAGER));
    assert!(row.3);

    // The same person may finish the look (a retry, the close figures, the
    // print) on the same unlock, inside the window.
    let (s, _) = call(
        &app,
        test::TestRequest::get().uri(&format!("/tills/{till}/close-preview")),
        h,
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "a retry of the same look is not a replay"
    );

    // But nobody else may spend it — that is the replay that matters.
    let mut theirs = who(TELLER_B, UserRole::Teller, Some(NEW_POS));
    theirs.push(key);
    let (s, _) = call(
        &app,
        test::TestRequest::get().uri(&format!("/tills/{till}/report")),
        theirs,
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "an unlock is not passed round the shop"
    );

    let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM approvals WHERE id = $1")
        .bind(uid(&approval_id))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows.0, 1, "one row, however many reads it covered");
}

#[sqlx::test]
async fn a_bad_unlock_is_refused_self_approval_a_stranger_and_the_wrong_act(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = open_till(&app).await;

    let cases: Vec<(&str, String)> = vec![
        // The teller "approving" themselves.
        ("self-approval", unlock(TELLER_A).1),
        // A manager of ANOTHER org.
        ("another org", unlock(FOREIGN).1),
        // An approver who does not hold this act (teller A's colleague would
        // do; the waiter in the golden seed does not hold cap 203 either).
        (
            "the wrong capability",
            json!({ "id": Uuid::new_v4(), "capability": "orders.void", "approver_id": MANAGER })
                .to_string(),
        ),
        ("not json", "{".to_string()),
    ];
    for (why, raw) in cases {
        let mut h = who(TELLER_A, UserRole::Teller, Some(NEW_POS));
        h.push((APPROVAL_HEADER, raw));
        let (s, _) = call(
            &app,
            test::TestRequest::get().uri(&format!("/tills/{till}/report")),
            h,
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{why}");
    }
}

// ── the part that must never slip ────────────────────────────────────────────

#[sqlx::test]
async fn old_clients_and_the_dashboard_are_untouched(pool: PgPool) {
    seeded(&pool).await;
    let app = app!(pool);
    let till = open_till(&app).await;

    // A pre-0.7 tablet sends no X-Madar-Client at all; v0.7.8 and the v0.7.10
    // build in the field send one but cannot carry an unlock on a GET. None of
    // them changes behaviour, full stop.
    for client in [None, Some(OLD_POS), Some(FIELD_POS)] {
        for uri in [
            format!("/tills/{till}/report"),
            format!("/tills/{till}/close-preview"),
        ] {
            let (s, b) = call(
                &app,
                test::TestRequest::get().uri(&uri),
                who(TELLER_A, UserRole::Teller, client),
            )
            .await;
            assert_eq!(s, StatusCode::OK, "{client:?} on {uri}: {b}");
        }
    }

    // The dashboard is a browser: it sends a Mozilla User-Agent and no client
    // header, and its till report sheet keeps working for everyone who may
    // read a till.
    let mut h = who(TELLER_A, UserRole::Teller, None);
    h.push((
        "User-Agent",
        "Mozilla/5.0 (Macintosh) AppleWebKit/537.36".to_string(),
    ));
    let (s, _) = call(
        &app,
        test::TestRequest::get().uri(&format!("/tills/{till}/report")),
        h,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "the dashboard");
}

#[actix_web::test]
async fn only_a_pos_or_kds_build_at_0_7_11_or_later_is_held_to_the_rule() {
    use madar_rust::tills::figures_guard::enforced_client;
    let h = |v: &str| {
        let mut m = actix_web::http::header::HeaderMap::new();
        m.insert(
            actix_web::http::header::HeaderName::from_static("x-madar-client"),
            actix_web::http::header::HeaderValue::from_str(v).unwrap(),
        );
        m
    };
    assert!(enforced_client(&h("pos/0.7.11 (android)")));
    assert!(enforced_client(&h("pos/0.8.0")));
    assert!(enforced_client(&h("kds/1.0.0")));
    assert!(!enforced_client(&h("pos/0.7.10 (android)")), "in the field");
    assert!(!enforced_client(&h("pos/0.7.8")));
    assert!(!enforced_client(&h("pos")), "no version");
    assert!(!enforced_client(&h("dashboard")));
    assert!(
        !enforced_client(&actix_web::http::header::HeaderMap::new()),
        "no header at all"
    );
}
