//! Legacy `/shifts` + `/tills` entity adapters vs the golden responses POS
//! v0.5.1 / v0.6.0 decode (`tests/fixtures/legacy_till_api`, captured from the
//! pre-rename backend). Data differs from the capture, so the guard is SHAPE:
//! every key the golden body carries (recursively; arrays by their first
//! element) must be present in the adapter's response, and a non-null golden
//! scalar must not come back with a different JSON type.
use actix_web::{App, test, web};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/legacy_till_api");

const DATA_KEYED: &[&str] = &["$.revenue_by_method"];

fn golden(file: &str) -> Value {
    let text = std::fs::read_to_string(format!("{FIXTURES}/{file}")).unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    v.get("body").cloned().unwrap_or(v)
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn assert_shape(path: &str, golden: &Value, actual: &Value, missing: &mut Vec<String>) {
    match (golden, actual) {
        // Maps keyed by data (payment method names), not by schema.
        (Value::Object(_), Value::Object(_)) if DATA_KEYED.contains(&path) => {}
        (Value::Object(g), Value::Object(a)) => {
            for (k, gv) in g {
                match a.get(k) {
                    None => missing.push(format!("{path}.{k} (missing)")),
                    Some(av) => assert_shape(&format!("{path}.{k}"), gv, av, missing),
                }
            }
        }
        (Value::Array(g), Value::Array(a)) => {
            if let (Some(g0), Some(a0)) = (g.first(), a.first()) {
                assert_shape(&format!("{path}[0]"), g0, a0, missing);
            }
        }
        (Value::Null, _) | (_, Value::Null) => {}
        (g, a) if kind(g) != kind(a) => {
            missing.push(format!("{path} ({} → {})", kind(g), kind(a)))
        }
        _ => {}
    }
}

fn check(file: &str, actual: &Value) {
    let mut missing = Vec::new();
    assert_shape("$", &golden(file), actual, &mut missing);
    assert!(missing.is_empty(), "{file}: legacy shape drift: {missing:?}");
}

async fn seed(pool: &PgPool) -> (Uuid, Uuid, Uuid, String) {
    let org: Uuid = sqlx::query_scalar("INSERT INTO organizations (name, slug) VALUES ('O', $1) RETURNING id")
        .bind(format!("o-{}", Uuid::new_v4()))
        .fetch_one(pool)
        .await
        .unwrap();
    let branch: Uuid = sqlx::query_scalar(
        "INSERT INTO branches (org_id, name) VALUES ($1, 'B') RETURNING id",
    )
    .bind(org)
    .fetch_one(pool)
    .await
    .unwrap();
    let user: Uuid = sqlx::query_scalar(
        "INSERT INTO users (org_id, name, email, password_hash, role) \
         VALUES ($1, 'Admin', $2, 'x', 'org_admin') RETURNING id",
    )
    .bind(org)
    .bind(format!("{}@t.io", Uuid::new_v4()))
    .fetch_one(pool)
    .await
    .unwrap();
    for (res, act) in [
        ("tills", "read"),
        ("tills", "create"),
        ("tills", "update"),
        ("tills", "delete"),
        ("refunds", "read"),
        ("reports", "read"),
        ("branches", "read"),
    ] {
        sqlx::query(
            "INSERT INTO role_permissions (role, resource, action, granted) \
             VALUES ('org_admin', $1::permission_resource, $2::permission_action, true) ON CONFLICT DO NOTHING",
        )
        .bind(res)
        .bind(act)
        .execute(pool)
        .await
        .unwrap();
    }
    let token = crate::auth::jwt::create_token(&JwtSecret("test_secret".into()), user, Some(org), UserRole::OrgAdmin, None, 24).unwrap();
    (org, branch, user, token)
}

#[sqlx::test]
async fn legacy_shift_routes_keep_the_old_client_shapes(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(JwtSecret("test_secret".into())))
            .configure(crate::tills::legacy_routes::configure)
            .configure(crate::tills::routes::configure)
            .configure(crate::refunds::routes::configure)
            .configure(|c| crate::reports::routes::configure(c, web::Data::new(pool.clone()))),
    )
    .await;
    let (_org, branch, _user, token) = seed(&pool).await;
    let auth = ("Authorization", format!("Bearer {token}"));

    macro_rules! call {
        ($req:expr) => {{
            let resp = test::call_service(&app, $req.insert_header(auth.clone()).to_request()).await;
            let status = resp.status().as_u16();
            let body: Value = test::read_body_json(resp).await;
            (status, body)
        }};
    }

    let (s, opened) = call!(test::TestRequest::post()
        .uri(&format!("/shifts/branches/{branch}/open"))
        .set_json(serde_json::json!({ "opening_cash": 1000, "till_id": null })));
    assert_eq!(s, 201, "{opened}");
    check("replay_open_shift.json", &opened);
    let id = opened["id"].as_str().unwrap().to_string();

    let (s, cm) = call!(test::TestRequest::post()
        .uri(&format!("/shifts/{id}/cash-movements"))
        .set_json(serde_json::json!({ "amount": 500, "note": "float" })));
    assert_eq!(s, 201, "{cm}");
    check("replay_cash_movement.json", &cm);

    let (s, b) = call!(test::TestRequest::get().uri(&format!("/shifts/branches/{branch}/current")));
    assert_eq!(s, 200, "{b}");
    check("shifts_current.json", &b);

    let (s, b) = call!(test::TestRequest::get().uri(&format!("/shifts/branches/{branch}")));
    assert_eq!(s, 200, "{b}");
    check("shifts_list_branch.json", &b);

    let (s, b) = call!(test::TestRequest::get().uri(&format!("/shifts/{id}")));
    assert_eq!(s, 200, "{b}");
    check("shifts_get_open.json", &b);

    let (s, b) = call!(test::TestRequest::get().uri(&format!("/shifts/{id}/report")));
    assert_eq!(s, 200, "{b}");
    check("shifts_report_open.json", &b);

    let (s, b) = call!(test::TestRequest::get().uri(&format!("/shifts/{id}/cash-movements")));
    assert_eq!(s, 200, "{b}");
    check("shifts_cash_movements.json", &b);

    let (s, b) = call!(test::TestRequest::get().uri(&format!("/refunds/shift/{id}")));
    assert_eq!(s, 200, "{b}");
    check("refunds_by_shift_empty.json", &b);

    let (s, b) = call!(test::TestRequest::get().uri(&format!("/reports/shifts/{id}/summary")));
    assert_eq!(s, 200, "{b}");
    check("reports_shift_summary.json", &b);

    let (s, b) = call!(test::TestRequest::get().uri(&format!("/tills?branch_id={branch}")));
    assert_eq!(s, 200, "{b}");
    check("tills_list.json", &b);

    let (s, b) = call!(test::TestRequest::post()
        .uri(&format!("/shifts/{id}/close"))
        .set_json(serde_json::json!({ "closing_cash_declared": 1500, "cash_note": null })));
    assert_eq!(s, 200, "{b}");
    check("replay_close_shift.json", &b);

    let (s, b) = call!(test::TestRequest::get().uri(&format!("/shifts/{id}/report")));
    assert_eq!(s, 200, "{b}");
    check("shifts_report_closed.json", &b);

    let (s, b) = call!(test::TestRequest::get().uri(&format!("/shifts/branches/{branch}/current")));
    assert_eq!(s, 200, "{b}");
    check("shifts_current_after_close.json", &b);

    // Force-close a second session.
    let (s, second) = call!(test::TestRequest::post()
        .uri(&format!("/shifts/branches/{branch}/open"))
        .set_json(serde_json::json!({ "opening_cash": 1500 })));
    assert_eq!(s, 201, "{second}");
    let id2 = second["id"].as_str().unwrap().to_string();
    let (s, b) = call!(test::TestRequest::post()
        .uri(&format!("/shifts/{id2}/force-close"))
        .set_json(serde_json::json!({ "reason": "test" })));
    assert_eq!(s, 200, "{b}");
    check("shifts_force_close.json", &b);

    // Entity CRUD is gone.
    let (s, _) = call!(test::TestRequest::post().uri("/tills").set_json(serde_json::json!({ "name": "x" })));
    assert_eq!(s, 410);
}
