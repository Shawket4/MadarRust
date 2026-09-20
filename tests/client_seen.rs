
mod common;

use madar_rust::client_seen::forget_throttle;

use std::time::{Duration, Instant};

use actix_web::http::Method;
use actix_web::http::header::HeaderMap;
use actix_web::{App, http::StatusCode, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::client_seen::*;
use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::models::UserRole;

const SECRET: &str = "test_secret";
const ORG: &str = "10000000-0000-4000-8000-000000000001";
const BRANCH_A: &str = "10000000-0000-4000-8000-0000000000a1";
const ADMIN: &str = "10000000-0000-4000-8000-00000000ad01";
const TELLER_A: &str = "10000000-0000-4000-8000-00000000ee0a";
const ITEM: &str = "10000000-0000-4000-8000-0000000e0001";

fn uid(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap()
}

fn bearer(user: &str, org: Uuid, role: UserRole) -> (&'static str, String) {
    let tok = create_token(
        &JwtSecret(SECRET.into()),
        uid(user),
        Some(org),
        role,
        None,
        24,
    )
    .unwrap();
    ("Authorization", format!("Bearer {tok}"))
}

fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        h.insert(
            actix_web::http::header::HeaderName::from_static(k),
            actix_web::http::header::HeaderValue::from_str(v).unwrap(),
        );
    }
    h
}

// ── pure ─────────────────────────────────────────────────────────────────────

#[core::prelude::v1::test]
fn classifies_legacy_routes_by_path() {
    assert_eq!(
        route_kind("/shifts/branches/x/current"),
        Some(KIND_SHIFTS_ROUTE)
    );
    assert_eq!(route_kind("/shifts"), Some(KIND_SHIFTS_ROUTE));
    assert_eq!(
        route_kind("/refunds/shift/abc"),
        Some(KIND_REFUNDS_SHIFT_ROUTE)
    );
    assert_eq!(
        route_kind("/reports/shifts/abc/summary"),
        Some(KIND_REPORTS_SHIFTS_ROUTE)
    );
    assert_eq!(route_kind("/tills/branches/x/current"), None);
    assert_eq!(route_kind("/shiftsx"), None);
    assert_eq!(
        route_kind("/staff/shifts"),
        None,
        "HR work shifts are not legacy"
    );
    assert_eq!(route_kind("/reports/tills/abc/summary"), None);
}

#[core::prelude::v1::test]
fn detects_shift_id_in_queries_and_bodies() {
    assert!(query_uses_shift_id(
        &Method::GET,
        "/orders",
        "branch_id=a&shift_id=b"
    ));
    assert!(query_uses_shift_id(
        &Method::GET,
        "/orders/export",
        "shift_id=b"
    ));
    assert!(!query_uses_shift_id(
        &Method::GET,
        "/orders",
        "till_id=b&note=shift_id"
    ));
    assert!(!query_uses_shift_id(&Method::POST, "/orders", "shift_id=b"));

    for p in [
        "/orders",
        "/refunds",
        "/open-tickets/1/settle",
        "/delivery-orders/1/finalize",
    ] {
        assert!(body_may_alias_shift_id(&Method::POST, p), "{p}");
    }
    assert!(!body_may_alias_shift_id(&Method::GET, "/orders"));
    assert!(!body_may_alias_shift_id(&Method::POST, "/orders/1/void"));
    assert!(body_names_shift_id(br#"{"branch_id":"x","shift_id":"y"}"#));
    assert!(!body_names_shift_id(
        br#"{"till_id":"y","note":"shift id"}"#
    ));
}

#[core::prelude::v1::test]
fn client_string_version_and_legacy_pos() {
    let h = headers(&[
        ("x-madar-client", "pos/0.7.2 (ios)"),
        ("user-agent", "madar-core/0.1.0"),
    ]);
    assert_eq!(client_string(&h).as_deref(), Some("pos/0.7.2 (ios)"));
    assert_eq!(app_version(&h).as_deref(), Some("0.7.2"));
    assert!(!is_legacy_pos_request(&h));
    assert_eq!(
        app_version(&headers(&[("x-madar-client", "kds/1.2.3")])).as_deref(),
        Some("1.2.3")
    );

    let old = headers(&[("user-agent", "madar-core/0.6.1")]);
    assert_eq!(client_string(&old).as_deref(), Some("madar-core/0.6.1"));
    assert_eq!(
        app_version(&old),
        None,
        "a User-Agent is never an app version (that is the crate's)"
    );
    assert_eq!(
        app_version(&headers(&[("user-agent", "Dart/3.4 (dart:io)")])),
        None
    );
    assert!(
        is_legacy_pos_request(&old),
        "no X-Madar-Client = a pre-0.7 POS"
    );
    assert!(is_legacy_pos_request(&headers(&[(
        "x-madar-client",
        "pos/0.6.9"
    )])));
    let browser = headers(&[(
        "user-agent",
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 Version/17.0 Safari/605.1.15",
    )]);
    assert!(!is_legacy_pos_request(&browser), "the dashboard");
    assert_eq!(client_string(&browser).as_deref(), Some(DASHBOARD_CLIENT));
    assert_eq!(
        app_version(&browser),
        None,
        "a browser's Mozilla/5.0 is not a version"
    );
    let dash = headers(&[
        ("x-madar-client", "dashboard/2026.9"),
        ("user-agent", "Mozilla/5.0"),
    ]);
    assert!(!is_legacy_pos_request(&dash));
    assert_eq!(app_version(&dash), None, "the dashboard has no app version");
    assert_eq!(app_version(&headers(&[("x-madar-client", "pos")])), None);
    assert!(is_native_client(&h) && !is_native_client(&old) && !is_native_client(&dash));
    let b = Uuid::new_v4();
    assert_eq!(
        branch_header(&headers(&[("x-madar-branch", &b.to_string())])),
        Some(b)
    );
    assert_eq!(branch_header(&headers(&[("x-madar-branch", "nope")])), None);
    assert_eq!(client_string(&HeaderMap::new()), None);
}

#[core::prelude::v1::test]
fn classifies_mirror_list_reads() {
    let g = Method::GET;
    for (path, q) in [
        ("/menu-items", "branch_id=x&full=true"),
        ("/categories", ""),
        ("/bundles", "status=active&per_page=500"),
        ("/payment-methods", ""),
        ("/branches/abc", ""),
        ("/floor/transfers", "since=2026-09-01T00:00:00Z"),
        ("/open-tickets", "status=open"),
        ("/kitchen/orders", ""),
        ("/delivery-orders", "limit=200"),
        ("/orgs/o/offline-auth-bundle", ""),
        ("/tills/branches/b", ""),
        ("/tills/branches/b/open", ""),
        ("/tills/t/cash-movements", ""),
        ("/refunds/order/o", ""),
        ("/orders", "till_id=t&page=1"),
    ] {
        assert!(mirror_list_route(&g, path, q), "{path}?{q}");
    }
    for (path, q) in [
        ("/menu-items", "branch_id=x"),
        ("/floor/transfers", ""),
        ("/orders", "branch_id=x"),
        ("/tills/branches/b/current", ""),
        ("/branches", ""),
        ("/orders/o", ""),
        ("/auth/me", ""),
        ("/sync/pull", ""),
    ] {
        assert!(!mirror_list_route(&g, path, q), "{path}?{q}");
    }
    assert!(!mirror_list_route(&Method::POST, "/categories", ""));
}

#[core::prelude::v1::test]
fn seen_key_prefers_the_device() {
    let d = Uuid::new_v4();
    let b = Uuid::new_v4();
    assert_eq!(seen_key(Some(d), Some(b), Some("x")), format!("d:{d}"));
    assert_eq!(
        seen_key(None, Some(b), Some("pos/0.6")),
        format!("c:{b}:pos/0.6")
    );
    assert_eq!(seen_key(None, None, None), "c:-:-");
}

#[core::prelude::v1::test]
fn throttle_lets_one_through_per_minute() {
    let key = format!("throttle-test-{}", Uuid::new_v4());
    let t0 = Instant::now();
    assert!(throttle_allows(&key, t0));
    assert!(!throttle_allows(&key, t0 + Duration::from_secs(59)));
    assert!(throttle_allows(
        &key,
        t0 + THROTTLE + Duration::from_secs(1)
    ));
    assert!(
        throttle_allows(&format!("{key}|other"), t0),
        "keys are independent"
    );
}

#[tokio::test]
async fn deep_sites_report_through_the_task_local() {
    let ((), hits) = collect_hits(async {
        assert!(madar_rust::analytics::schema::dataset("shifts").is_some());
        assert!(madar_rust::analytics::schema::dataset("tills").is_some());
        assert!(madar_rust::analytics::presets::preset("shift_cash_summary").is_some());
    })
    .await;
    let kinds: Vec<_> = hits.iter().map(|h| (h.kind, h.site)).collect();
    assert_eq!(
        kinds,
        vec![
            (KIND_ANALYTICS_ALIAS, Some("dataset_shifts")),
            (KIND_ANALYTICS_ALIAS, Some("preset_shift_cash_summary"))
        ]
    );
    // Outside a scope a hit is only logged.
    legacy_hit(KIND_SHIFTS_ROUTE);
}

#[core::prelude::v1::test]
fn replay_shift_id_detection() {
    use madar_rust::sync::handlers::replay_names_shift_id;
    assert!(replay_names_shift_id(
        &json!({"op": "close_shift", "shift_id": "x", "request": {}})
    ));
    assert!(replay_names_shift_id(
        &json!({"op": "create_order", "request": {"shift_id": "x"}})
    ));
    assert!(!replay_names_shift_id(
        &json!({"op": "close_till", "till_id": "x", "request": {"till_id": "x"}})
    ));
}

// ── DB ───────────────────────────────────────────────────────────────────────

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
}

type Row = (
    String,
    Option<Uuid>,
    Option<Uuid>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Vec<String>,
);

async fn row(pool: &PgPool, org: Uuid, key: &str) -> Option<Row> {
    sqlx::query_as(
        "SELECT seen_key, branch_id, device_id, client, app_version, last_legacy_kind, last_legacy_path, legacy_kinds \
           FROM client_seen WHERE org_id = $1 AND seen_key = $2",
    )
    .bind(org)
    .bind(key)
    .fetch_optional(pool)
    .await
    .unwrap()
}

/// The middleware writes in the background: wait for `pred` on the row.
async fn wait_for(pool: &PgPool, org: Uuid, key: &str, pred: impl Fn(&Row) -> bool) -> Row {
    for _ in 0..100 {
        if let Some(r) = row(pool, org, key).await
            && pred(&r)
        {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "client_seen {key} never reached the expected state: {:?}",
        row(pool, org, key).await
    );
}

#[sqlx::test]
async fn upsert_keeps_first_seen_and_accumulates_legacy_kinds(pool: PgPool) {
    seeded(&pool).await;
    let org = uid(ORG);
    let device = Uuid::new_v4();
    let base = Sighting {
        org_id: org,
        branch_id: Some(uid(BRANCH_A)),
        branch_hint: None,
        device_id: Some(device),
        client: Some("madar-core/0.6.0".into()),
        app_version: Some("0.6.0".into()),
        legacy_kinds: vec![],
        path: "/orders".into(),
    };
    upsert(&pool, &base).await.unwrap();
    let key = format!("d:{device}");
    let r = row(&pool, org, &key).await.unwrap();
    assert_eq!((r.5.as_deref(), r.6.as_deref(), r.7.len()), (None, None, 0));
    let first: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT first_seen_at FROM client_seen WHERE seen_key = $1")
            .bind(&key)
            .fetch_one(&pool)
            .await
            .unwrap();

    upsert(
        &pool,
        &Sighting {
            legacy_kinds: vec![KIND_SHIFTS_ROUTE],
            path: "/shifts/x/report".into(),
            ..base.clone()
        },
    )
    .await
    .unwrap();
    upsert(
        &pool,
        &Sighting {
            legacy_kinds: vec![KIND_REPLAY_LEGACY_OP],
            path: "/sync/replay".into(),
            ..base.clone()
        },
    )
    .await
    .unwrap();
    // A plain sighting later does not erase the legacy trail; a newer client string wins.
    upsert(
        &pool,
        &Sighting {
            client: Some("pos/0.7.1 (android)".into()),
            app_version: Some("0.7.1".into()),
            ..base.clone()
        },
    )
    .await
    .unwrap();

    let r = row(&pool, org, &key).await.unwrap();
    assert_eq!(r.3.as_deref(), Some("pos/0.7.1 (android)"));
    assert_eq!(r.4.as_deref(), Some("0.7.1"));
    assert_eq!(r.5.as_deref(), Some(KIND_REPLAY_LEGACY_OP));
    assert_eq!(r.6.as_deref(), Some("/sync/replay"));
    assert_eq!(
        r.7,
        vec![
            KIND_SHIFTS_ROUTE.to_string(),
            KIND_REPLAY_LEGACY_OP.to_string()
        ]
    );
    let (first2, n): (chrono::DateTime<chrono::Utc>, i64) =
        sqlx::query_as("SELECT min(first_seen_at), count(*) FROM client_seen WHERE org_id = $1")
            .bind(org)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((first2, n), (first, 1));
}

#[sqlx::test]
async fn client_seen_is_tenant_isolated(pool: PgPool) {
    seeded(&pool).await;
    let org = uid(ORG);
    upsert(
        &pool,
        &Sighting {
            org_id: org,
            branch_id: None,
            branch_hint: None,
            device_id: None,
            client: Some("madar-core/0.5.1".into()),
            app_version: Some("0.5.1".into()),
            legacy_kinds: vec![KIND_SHIFTS_ROUTE],
            path: "/shifts".into(),
        },
    )
    .await
    .unwrap();
    let mine = madar_rust::db::tenant_pool(&pool, org).await;
    let other = madar_rust::db::tenant_pool(&pool, Uuid::new_v4()).await;
    let n = |p: PgPool| async move {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM client_seen")
            .fetch_one(&p)
            .await
            .unwrap()
    };
    assert_eq!(n(mine).await, 1);
    assert_eq!(n(other).await, 0);
}

macro_rules! telemetry_app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .wrap(actix_web::middleware::from_fn(record))
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(JwtSecret(SECRET.into())))
                .app_data(web::Data::new(madar_rust::realtime::hub::BranchEventHub::new()))
                .configure(madar_rust::auth::routes::configure)
                .configure(madar_rust::tills::legacy_routes::configure)
                .configure(madar_rust::devices::routes::configure)
                .configure(madar_rust::tills::routes::configure)
                .configure(madar_rust::sync::routes::configure)
                .configure(madar_rust::orders::routes::configure)
                .configure(madar_rust::refunds::routes::configure)
                .configure(madar_rust::menu::routes::configure)
                .configure(|c| madar_rust::reports::routes::configure(c, web::Data::new($pool.clone()))),
        )
        .await
    };
}

#[sqlx::test]
async fn middleware_records_devices_and_legacy_paths(pool: PgPool) {
    seeded(&pool).await;
    let org = uid(ORG);
    let app = telemetry_app!(pool);
    let teller = bearer(TELLER_A, org, UserRole::Teller);
    let old_device = Uuid::new_v4();
    let new_device = Uuid::new_v4();

    // An old tablet: /shifts adapter, its own device id, no X-Madar-Client.
    let shift = Uuid::new_v4();
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/shifts/branches/{BRANCH_A}/open"))
            .insert_header(teller.clone())
            .insert_header(("X-Madar-Device", old_device.to_string()))
            .insert_header(("User-Agent", "madar-core/0.6.0"))
            .set_json(json!({ "id": shift, "opening_cash": 0 }))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let key = format!("d:{old_device}");
    let got = wait_for(&pool, org, &key, |r| r.5.is_some()).await;
    assert_eq!(got.2, Some(old_device));
    assert_eq!(got.3.as_deref(), Some("madar-core/0.6.0"));
    assert_eq!(
        got.4, None,
        "the old tablet's User-Agent is its core crate, not an app version"
    );
    assert_eq!(got.5.as_deref(), Some(KIND_SHIFTS_ROUTE));
    assert_eq!(
        got.6.as_deref(),
        Some(&*format!("/shifts/branches/{BRANCH_A}/open"))
    );

    // The same tablet sells with a `shift_id` body: the body still reaches the handler intact.
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/orders")
            .insert_header(teller.clone())
            .insert_header(("X-Madar-Device", old_device.to_string()))
            .set_json(json!({
                "branch_id": BRANCH_A, "shift_id": shift, "payment_method": "cash", "idempotency_key": Uuid::new_v4(),
                "items": [{ "menu_item_id": ITEM, "quantity": 1, "unit_price": 5000, "addons": [], "optional_field_ids": [] }],
                "subtotal": 5000, "tax_amount": 0, "total_amount": 5000, "amount_tendered": 5000, "change_given": 0
            }))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let order: Value = test::read_body_json(r).await;
    assert_eq!(order["till_id"], json!(shift));
    wait_for(&pool, org, &key, |r| {
        r.7.iter().any(|k| k == KIND_SHIFT_ID_BODY)
    })
    .await;

    // Query alias, legacy replay op, and the permissions payload read by an old POS.
    let r = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/orders?branch_id={BRANCH_A}&shift_id={shift}"))
            .insert_header(teller.clone())
            .insert_header(("X-Madar-Device", old_device.to_string()))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(teller.clone())
            .insert_header(("X-Madar-Device", old_device.to_string()))
            .set_json(
                json!({ "op": "close_shift", "teller_id": TELLER_A, "shift_id": shift,
                              "request": { "closing_cash_declared": 5000, "cash_note": null } }),
            )
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let r = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/auth/permissions")
            .insert_header(teller.clone())
            .insert_header(("X-Madar-Device", old_device.to_string()))
            .insert_header(("User-Agent", "madar-core/0.6.0"))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let got = wait_for(&pool, org, &key, |r| {
        [
            KIND_SHIFT_ID_QUERY,
            KIND_REPLAY_LEGACY_OP,
            KIND_REPLAY_SHIFT_ID_FIELD,
            KIND_PERM_PAYLOAD_OLD,
        ]
        .iter()
        .all(|k| r.7.iter().any(|x| x == k))
    })
    .await;
    assert!(got.7.iter().any(|k| k == KIND_SHIFTS_ROUTE));

    // A post-rework tablet on live routes: seen, never legacy.
    let r = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/auth/permissions")
            .insert_header(teller.clone())
            .insert_header(("X-Madar-Device", new_device.to_string()))
            .insert_header(("X-Madar-Client", "pos/0.7.3 (ios)"))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let fresh = wait_for(&pool, org, &format!("d:{new_device}"), |_| true).await;
    assert_eq!(fresh.4.as_deref(), Some("0.7.3"));
    assert_eq!(fresh.5, None);
    assert!(fresh.7.is_empty(), "{:?}", fresh.7);

    // The admin view lists the legacy tablet only; tellers may not read it.
    let r = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/devices/client-versions")
            .insert_header(bearer(ADMIN, org, UserRole::OrgAdmin))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let rows: Vec<Value> = test::read_body_json(r).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["device_id"], json!(old_device));
    assert_eq!(
        rows[0]["branch_name"], "Golden A",
        "no branch claim on a teller token: resolved from the device row the open registered"
    );
    assert!(rows[0]["legacy_kinds"].as_array().unwrap().len() >= 5);
    let r = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/devices/client-versions?legacy_only=false&days=30")
            .insert_header(bearer(ADMIN, org, UserRole::OrgAdmin))
            .to_request(),
    )
    .await;
    let all: Vec<Value> = test::read_body_json(r).await;
    assert!(
        all.iter()
            .any(|r| r["device_id"] == json!(new_device) && r["app_version"] == "0.7.3")
    );
    let r = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/devices/client-versions?days=0")
            .insert_header(bearer(ADMIN, org, UserRole::OrgAdmin))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let r = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/devices/client-versions")
            .insert_header(teller.clone())
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn middleware_records_legacy_report_and_refund_routes_without_a_device(pool: PgPool) {
    seeded(&pool).await;
    let org = uid(ORG);
    let app = telemetry_app!(pool);
    let admin = bearer(ADMIN, org, UserRole::OrgAdmin);
    let missing = Uuid::new_v4();
    for path in [
        format!("/reports/shifts/{missing}/summary"),
        format!("/refunds/shift/{missing}"),
    ] {
        let r = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&path)
                .insert_header(admin.clone())
                .insert_header(("User-Agent", "Dart/3.4 (dart:io)"))
                .to_request(),
        )
        .await;
        assert!(r.status().is_client_error(), "{path}: {}", r.status());
    }
    // Anonymous client: keyed by branch + client string.
    let got = wait_for(&pool, org, "c:-:Dart/3.4 (dart:io)", |r| r.7.len() == 2).await;
    assert_eq!(
        got.7,
        vec![
            KIND_REFUNDS_SHIFT_ROUTE.to_string(),
            KIND_REPORTS_SHIFTS_ROUTE.to_string()
        ]
    );
    assert_eq!(got.4, None, "Dart/3.4 is the runtime, not an app version");
}

#[sqlx::test]
async fn unauthenticated_requests_are_not_recorded(pool: PgPool) {
    seeded(&pool).await;
    let app = telemetry_app!(pool);
    let r = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/shifts/branches/{BRANCH_A}/current"))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM client_seen")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn handler_sites_report_their_kind() {
    let (res, hits) = collect_hits(madar_rust::tills::legacy_routes::till_entity_gone()).await;
    assert!(res.is_err());
    let (_, wording) = collect_hits(async {
        madar_rust::tills::legacy_routes::legacy_error(madar_rust::errors::AppError::Coded {
            status: 400,
            code: "X",
            reason: "till".into(),
        })
    })
    .await;
    let (_, untouched) = collect_hits(async {
        madar_rust::tills::legacy_routes::legacy_error(madar_rust::errors::AppError::NotFound("x".into()))
    })
    .await;
    assert_eq!(
        hits.iter().map(|h| h.kind).collect::<Vec<_>>(),
        vec![KIND_TILLS_ENTITY_GONE]
    );
    assert_eq!(
        wording.iter().map(|h| (h.kind, h.site)).collect::<Vec<_>>(),
        vec![(KIND_ERROR_WORDING, Some("legacy_error_till_to_shift"))]
    );
    assert!(untouched.is_empty());
}

#[sqlx::test]
async fn branch_resolves_from_the_device_row_then_the_header(pool: PgPool) {
    seeded(&pool).await;
    let org = uid(ORG);
    let app = telemetry_app!(pool);
    // Admin tokens carry no branch claim.
    let admin = bearer(ADMIN, org, UserRole::OrgAdmin);
    let registered = Uuid::new_v4();
    sqlx::query("INSERT INTO devices (id, org_id, branch_id, code) VALUES ($1, $2, $3, 'AB1')")
        .bind(registered)
        .bind(org)
        .bind(uid("10000000-0000-4000-8000-0000000000b1"))
        .execute(&pool)
        .await
        .unwrap();
    let call = |device: Option<Uuid>, branch: Option<String>, client: &'static str| {
        let mut r = test::TestRequest::get()
            .uri("/auth/permissions")
            .insert_header(admin.clone())
            .insert_header(("X-Madar-Client", client));
        if let Some(d) = device {
            r = r.insert_header(("X-Madar-Device", d.to_string()));
        }
        if let Some(b) = branch {
            r = r.insert_header((BRANCH_HEADER, b));
        }
        r.to_request()
    };
    // The device row wins over the header.
    let r = test::call_service(
        &app,
        call(
            Some(registered),
            Some(BRANCH_A.into()),
            "pos/0.7.4 (android)",
        ),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let got = wait_for(&pool, org, &format!("d:{registered}"), |_| true).await;
    assert_eq!(got.1, Some(uid("10000000-0000-4000-8000-0000000000b1")));
    assert_eq!(got.4.as_deref(), Some("0.7.4"));

    // An unregistered device: the header names the branch.
    let unknown = Uuid::new_v4();
    test::call_service(
        &app,
        call(Some(unknown), Some(BRANCH_A.into()), "pos/0.7.4 (ios)"),
    )
    .await;
    assert_eq!(
        wait_for(&pool, org, &format!("d:{unknown}"), |_| true)
            .await
            .1,
        Some(uid(BRANCH_A))
    );

    // A branch of another org is ignored.
    let other_org = Uuid::new_v4();
    let foreign = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug, tax_rate) VALUES ($1, 'Other', 'other-telemetry', 0)").bind(other_org).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO branches (id, org_id, name, code, latitude, longitude) VALUES ($1, $2, 'X', 'OTHX', 0, 0)")
        .bind(foreign)
        .bind(other_org)
        .execute(&pool)
        .await
        .unwrap();
    let stray = Uuid::new_v4();
    test::call_service(
        &app,
        call(Some(stray), Some(foreign.to_string()), "kds/0.7.4"),
    )
    .await;
    let got = wait_for(&pool, org, &format!("d:{stray}"), |_| true).await;
    assert_eq!(got.1, None);
}

/// The two tests that expect a fresh DASHBOARD sighting share one throttle
/// key (same seeded org), so they run one at a time and start unthrottled.
static DASHBOARD_SIGHTING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[sqlx::test]
async fn browsers_are_the_dashboard_without_a_version(pool: PgPool) {
    seeded(&pool).await;
    let org = uid(ORG);
    let _one_at_a_time = DASHBOARD_SIGHTING.lock().await;
    forget_throttle(&format!("{org}|"));
    let app = telemetry_app!(pool);
    let r = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/auth/permissions")
            .insert_header(bearer(ADMIN, org, UserRole::OrgAdmin))
            .insert_header(("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/128.0.0.0 Safari/537.36"))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let got = wait_for(&pool, org, "c:-:dashboard", |_| true).await;
    assert_eq!(
        (got.3.as_deref(), got.4.as_deref()),
        (Some(DASHBOARD_CLIENT), None)
    );
    assert!(
        got.7.is_empty(),
        "the dashboard never takes the permissions mirror path"
    );
}

#[sqlx::test]
async fn mirror_lists_and_catalog_sync_are_recorded_for_native_clients(pool: PgPool) {
    seeded(&pool).await;
    let org = uid(ORG);
    let _one_at_a_time = DASHBOARD_SIGHTING.lock().await;
    forget_throttle(&format!("{org}|"));
    let app = telemetry_app!(pool);
    let admin = bearer(ADMIN, org, UserRole::OrgAdmin);
    let device = Uuid::new_v4();
    // A dashboard read of the same list is not a mirror hit.
    let r = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/categories?org_id={ORG}"))
            .insert_header(admin.clone())
            .insert_header(("User-Agent", "Mozilla/5.0"))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    for uri in [
        format!("/categories?org_id={ORG}"),
        format!("/catalog/sync?branch_id={BRANCH_A}"),
    ] {
        let r = test::call_service(
            &app,
            test::TestRequest::get()
                .uri(&uri)
                .insert_header(admin.clone())
                .insert_header(("X-Madar-Device", device.to_string()))
                .insert_header(("X-Madar-Client", "pos/0.7.4 (android)"))
                .to_request(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK, "{uri}");
    }
    let got = wait_for(&pool, org, &format!("d:{device}"), |r| r.7.len() == 2).await;
    assert_eq!(
        got.7,
        vec![
            KIND_CATALOG_SYNC.to_string(),
            KIND_MIRROR_LIST_POS.to_string()
        ]
    );
    // The sighting is written in the background after the response; a fixed
    // 200 ms nap lost that race under a loaded single-process `cargo test`
    // (CI), so wait for the row like every other assertion here does.
    let dash = wait_for(&pool, org, "c:-:dashboard", |_| true).await;
    assert!(dash.7.is_empty(), "{:?}", dash.7);
}

// (Layout nudge: see CLAUDE.md on XProtect and large test binaries.)
