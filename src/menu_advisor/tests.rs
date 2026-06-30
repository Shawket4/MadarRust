//! HTTP + DB integration tests for the Menu Advisor.
//!
//! Suggestions are seeded through `persistence::save_completed_report`, so
//! every test also exercises the payload-JSONB storage and the generated
//! filter columns. The IDOR suite is the heart of the rebuild: every route
//! must refuse another org's resources.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use actix_web::{App, test, web};
use chrono::{Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    auth::jwt::{JwtSecret, create_token},
    menu_advisor::{
        adapter,
        dto::{
            Action, AdvisorReport, AnalysisConfig, BundleAssociation, BundleForecast,
            BundleItemPair, BundleSuggestion, BundleSuggestionRecord, Classification, CmQuadrant,
            Confidence, CreateRunBody, DecisionRecord, GuardClip, ItemKey, ModeSummary,
            PeerComparison, PeerPosition, PersistedRun, PriceAnchors, PriceSuggestion,
            PriceSuggestionRecord, PromoteBundleBody, RecordDecisionBody, RemovalRecommendation,
            RemovalScenario, RemovalScenarioRecord, RunStatus, SuggestionKind, Triplet,
        },
        persistence, routes,
    },
    models::UserRole,
};

// ─────────────────────────────────────────────────────────────────────
// Seeding helpers
// ─────────────────────────────────────────────────────────────────────

async fn seed_org(pool: &PgPool) -> Uuid {
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Test Org', $2)")
        .bind(org_id)
        .bind(Uuid::new_v4().to_string())
        .execute(pool)
        .await
        .unwrap();
    org_id
}

async fn seed_branch(pool: &PgPool, org_id: Uuid) -> Uuid {
    let branch_id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(branch_id)
        .bind(org_id)
        .bind(format!("Branch {branch_id}"))
        .execute(pool)
        .await
        .unwrap();
    branch_id
}

async fn seed_user(pool: &PgPool, org_id: Uuid, role: &str) -> Uuid {
    let user_id = Uuid::new_v4();
    // chk_super_admin_no_org: super admins must carry NULL org.
    let user_org = (role != "super_admin").then_some(org_id);
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, 'Test User', $3, 'hash', $4::user_role)",
    )
    .bind(user_id)
    .bind(user_org)
    .bind(format!("{}@example.com", Uuid::new_v4()))
    .bind(role)
    .execute(pool)
    .await
    .unwrap();

    for action in ["read", "update"] {
        sqlx::query(
            "INSERT INTO role_permissions (role, resource, action) \
             VALUES ($1::user_role, 'menu_items'::permission_resource, $2::permission_action) \
             ON CONFLICT DO NOTHING",
        )
        .bind(role)
        .bind(action)
        .execute(pool)
        .await
        .unwrap();
    }
    user_id
}

async fn assign_branch(pool: &PgPool, user_id: Uuid, branch_id: Uuid) {
    sqlx::query(
        "INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2) \
         ON CONFLICT DO NOTHING",
    )
    .bind(user_id)
    .bind(branch_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_category(pool: &PgPool, org_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'Drinks')")
        .bind(id)
        .bind(org_id)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn seed_menu_item(pool: &PgPool, org_id: Uuid, name: &str, base_price: i64) -> Uuid {
    let item_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO menu_items (id, org_id, name, base_price, is_active) \
         VALUES ($1, $2, $3, $4, true)",
    )
    .bind(item_id)
    .bind(org_id)
    .bind(name)
    .bind(base_price as i32)
    .execute(pool)
    .await
    .unwrap();
    item_id
}

async fn seed_bundle(pool: &PgPool, org_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO bundles (id, org_id, name, price) VALUES ($1, $2, 'Combo', 9000)")
        .bind(id)
        .bind(org_id)
        .execute(pool)
        .await
        .unwrap();
    id
}

fn token(user_id: Uuid, org_id: Option<Uuid>, role: UserRole) -> String {
    let secret = JwtSecret("test_secret".to_string());
    create_token(&secret, user_id, org_id, role, None, 24).unwrap()
}

macro_rules! advisor_app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(JwtSecret("test_secret".to_string())))
                .configure(routes::configure),
        )
        .await
    };
}

// Small wrappers so tests read as intent, not plumbing.
async fn get_json<T: serde::de::DeserializeOwned>(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    uri: &str,
    tok: &str,
) -> (actix_web::http::StatusCode, Option<T>) {
    let req = test::TestRequest::get()
        .uri(uri)
        .insert_header(("Authorization", format!("Bearer {tok}")))
        .to_request();
    let resp = test::call_service(app, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await;
    (status, serde_json::from_slice::<T>(&body).ok())
}

async fn get_status(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    uri: &str,
    tok: &str,
) -> actix_web::http::StatusCode {
    let req = test::TestRequest::get()
        .uri(uri)
        .insert_header(("Authorization", format!("Bearer {tok}")))
        .to_request();
    test::call_service(app, req).await.status()
}

async fn post_json(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    uri: &str,
    tok: &str,
    body: &impl serde::Serialize,
) -> (actix_web::http::StatusCode, Vec<u8>) {
    let req = test::TestRequest::post()
        .uri(uri)
        .insert_header(("Authorization", format!("Bearer {tok}")))
        .set_json(body)
        .to_request();
    let resp = test::call_service(app, req).await;
    let status = resp.status();
    let body = test::read_body(resp).await.to_vec();
    (status, body)
}

// ─────────────────────────────────────────────────────────────────────
// Report fixture (fully populated, exercises every nested shape)
// ─────────────────────────────────────────────────────────────────────

fn item_key(id: Uuid) -> ItemKey {
    ItemKey {
        menu_item_id: id,
        size_label: "one_size".into(),
    }
}

fn sample_report(latte: Uuid, croissant: Uuid) -> AdvisorReport {
    let price_cm = PriceSuggestion {
        key: item_key(latte),
        item_name: "Latte".into(),
        classification: Classification::Cm {
            quadrant: CmQuadrant::Star,
        },
        current_price: 10_000,
        units_sold_raw: 120.0,
        effective_price: 9_900.0,
        popularity_share: 0.4,
        cm_per_unit: Some(6_000.0),
        margin_pct: Some(0.6),
        food_cost_pct: Some(0.4),
        anchors: PriceAnchors {
            cost_plus: Some(13_333.0),
            peer_median: 11_000.0,
            status_quo: 10_000.0,
        },
        suggested_price: Some(11_000),
        suggested_delta_abs: Some(1_000),
        suggested_delta_pct: Some(0.10),
        action: Action::RaisePrice,
        confidence: Confidence::High,
        explanation: "Star priced below peers.".into(),
        guard_clips: vec![GuardClip::CulturalRounding],
        peer_comparison: Some(PeerComparison {
            same_category_count: 3,
            median_effective_price_peers: 11_000.0,
            median_margin_pct_peers: Some(0.55),
            median_cm_per_unit_peers: Some(5_500.0),
            your_position: PeerPosition::Below,
        }),
        price_changed_in_window: false,
        cost_reduction_whatif_margin: None,
        cost_missing: false,
    };
    let price_revenue = PriceSuggestion {
        key: item_key(croissant),
        item_name: "Croissant".into(),
        classification: Classification::Revenue {
            class: crate::menu_advisor::dto::RevenueClass::Quiet,
        },
        current_price: 4_000,
        units_sold_raw: 25.0,
        effective_price: 4_000.0,
        popularity_share: 0.1,
        cm_per_unit: None,
        margin_pct: None,
        food_cost_pct: None,
        anchors: PriceAnchors {
            cost_plus: None,
            peer_median: 4_500.0,
            status_quo: 4_000.0,
        },
        suggested_price: None,
        suggested_delta_abs: None,
        suggested_delta_pct: None,
        action: Action::Monitor,
        confidence: Confidence::Low,
        explanation: "Quiet item.".into(),
        guard_clips: vec![],
        peer_comparison: None,
        price_changed_in_window: false,
        cost_reduction_whatif_margin: None,
        cost_missing: true,
    };
    let bundle = BundleSuggestion {
        focus_item: item_key(croissant),
        bundle_items: vec![item_key(latte), item_key(croissant)],
        bundle_list_price: 14_000,
        bundle_suggested_price: 12_500,
        bundle_discount_pct: 0.107,
        bundle_cost: Some(5_000),
        bundle_cm: Some(7_500),
        bundle_margin_pct: Some(0.6),
        association: BundleAssociation {
            pair_lifts: vec![BundleItemPair {
                item_a: item_key(croissant),
                item_b: item_key(latte),
                lift: 1.8,
                support: 0.12,
                confidence_ab: 0.65,
            }],
            composite_score: 0.28,
        },
        forecast: BundleForecast {
            expected_velocity: Triplet {
                lo: 0.4,
                mid: 0.5,
                hi: 0.75,
            },
            inside_bundle_units_x: 15.0,
            halo_units_x: 1.9,
            total_units_uplift_x: 16.9,
            incremental_cm: Some(Triplet {
                lo: -500.0,
                mid: 900.0,
                hi: 2_100.0,
            }),
        },
        guard_clips: vec![GuardClip::CulturalRounding],
        explanation: "Croissant + Latte combo.".into(),
        missing_costs: false,
    };
    let removal = RemovalScenario {
        key: item_key(croissant),
        item_name: "Croissant".into(),
        baseline_cm: 1_500.0,
        absorbed_by: vec![crate::menu_advisor::dto::AbsorbedBy {
            key: item_key(latte),
            absorbed_units: 12.0,
            absorbed_cm: 7_200.0,
        }],
        complementary_losses: vec![crate::menu_advisor::dto::ComplementaryLoss {
            key: item_key(latte),
            lost_units: 3.0,
            lost_cm: 1_800.0,
        }],
        net_cm_change: 3_900.0,
        net_cm_change_lo: 900.0,
        net_cm_change_hi: 6_900.0,
        recommendation: RemovalRecommendation::Remove,
        explanation: "Removal frees CM.".into(),
    };
    AdvisorReport {
        generated_at: Utc::now(),
        window_days: 30.0,
        mode_summary: ModeSummary {
            items_total: 2,
            items_cm_tracked: 1,
            items_revenue_only: 1,
            items_insufficient: 0,
        },
        price_suggestions: vec![price_cm, price_revenue],
        bundle_suggestions: vec![bundle],
        removal_scenarios: vec![removal],
    }
}

struct SeededRun {
    run_id: Uuid,
    price_id: Uuid, // the Latte (cm/star) suggestion
    bundle_id: Uuid,
    removal_id: Uuid,
    latte: Uuid,
    category_id: Uuid,
}

/// Create a run and persist the fixture report through the real write path.
async fn seed_completed_run(pool: &PgPool, org_id: Uuid, branch_id: Uuid) -> SeededRun {
    let category_id = seed_category(pool, org_id).await;
    let latte = seed_menu_item(pool, org_id, "Latte", 10_000).await;
    let croissant = seed_menu_item(pool, org_id, "Croissant", 4_000).await;

    let run_id = persistence::create_run(pool, org_id, branch_id, &AnalysisConfig::default())
        .await
        .unwrap();
    let report = sample_report(latte, croissant);
    let mut category_by_key = std::collections::HashMap::new();
    category_by_key.insert(item_key(latte), Some(category_id));
    category_by_key.insert(item_key(croissant), None);
    persistence::save_completed_report(pool, run_id, branch_id, &category_by_key, &report)
        .await
        .unwrap();

    let price_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM menu_advisor_price_suggestions WHERE run_id = $1 AND item_name = 'Latte'",
    )
    .bind(run_id)
    .fetch_one(pool)
    .await
    .unwrap();
    let bundle_id: Uuid =
        sqlx::query_scalar("SELECT id FROM menu_advisor_bundle_suggestions WHERE run_id = $1")
            .bind(run_id)
            .fetch_one(pool)
            .await
            .unwrap();
    let removal_id: Uuid =
        sqlx::query_scalar("SELECT id FROM menu_advisor_removal_scenarios WHERE run_id = $1")
            .bind(run_id)
            .fetch_one(pool)
            .await
            .unwrap();

    SeededRun {
        run_id,
        price_id,
        bundle_id,
        removal_id,
        latte,
        category_id,
    }
}

// ─────────────────────────────────────────────────────────────────────
// Run lifecycle
// ─────────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn create_run_succeeds_and_conflicts_while_active(pool: PgPool) {
    let app = advisor_app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    let tok = token(user, Some(org), UserRole::OrgAdmin);

    let uri = format!("/menu-advisor/branches/{branch}/runs");
    let (status, body) = post_json(&app, &uri, &tok, &CreateRunBody { config: None }).await;
    assert_eq!(status, 202);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["run_id"].is_string());

    // The spawned task may complete quickly; freeze an in-progress run
    // deterministically instead of racing it.
    sqlx::query("UPDATE menu_advisor_runs SET status = 'in_progress', completed_at = NULL")
        .execute(&pool)
        .await
        .unwrap();

    let (status, _) = post_json(&app, &uri, &tok, &CreateRunBody { config: None }).await;
    assert_eq!(status, 409, "fresh active run must 409");
}

#[sqlx::test]
async fn stale_run_takeover(pool: PgPool) {
    let app = advisor_app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    let tok = token(user, Some(org), UserRole::OrgAdmin);

    let stale_id = persistence::create_run(&pool, org, branch, &AnalysisConfig::default())
        .await
        .unwrap();
    sqlx::query(
        "UPDATE menu_advisor_runs SET started_at = now() - interval '20 minutes' WHERE id = $1",
    )
    .bind(stale_id)
    .execute(&pool)
    .await
    .unwrap();

    let uri = format!("/menu-advisor/branches/{branch}/runs");
    let (status, _) = post_json(&app, &uri, &tok, &CreateRunBody { config: None }).await;
    assert_eq!(status, 202, "stale run must be taken over");

    let stale = persistence::get_run(&pool, stale_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stale.status, RunStatus::Failed);
    assert!(stale.error_message.unwrap().contains("abandoned"));
}

#[sqlx::test]
async fn concurrent_create_run_blocked_by_unique_index(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let cfg = AnalysisConfig::default();
    persistence::create_run(&pool, org, branch, &cfg)
        .await
        .unwrap();
    // Bypasses the handler's pre-check entirely — the DB must still refuse.
    let second = persistence::create_run(&pool, org, branch, &cfg).await;
    match second {
        Err(crate::errors::AppError::Conflict(_)) => {}
        other => panic!("expected Conflict, got {other:?}"),
    }
}

#[sqlx::test]
async fn run_reads_pagination_latest_active_and_404(pool: PgPool) {
    let app = advisor_app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    let tok = token(user, Some(org), UserRole::OrgAdmin);

    // Three runs: completed (old), failed (newer), in_progress (newest).
    let r1 = persistence::create_run(&pool, org, branch, &AnalysisConfig::default())
        .await
        .unwrap();
    sqlx::query("UPDATE menu_advisor_runs SET status='completed', completed_at = now() - interval '2 hours', started_at = now() - interval '3 hours' WHERE id = $1")
        .bind(r1).execute(&pool).await.unwrap();
    let r2 = persistence::create_run(&pool, org, branch, &AnalysisConfig::default())
        .await
        .unwrap();
    sqlx::query("UPDATE menu_advisor_runs SET status='failed', error_message='boom', completed_at = now() - interval '1 hour', started_at = now() - interval '90 minutes' WHERE id = $1")
        .bind(r2).execute(&pool).await.unwrap();
    let r3 = persistence::create_run(&pool, org, branch, &AnalysisConfig::default())
        .await
        .unwrap();

    // List: newest first.
    let (status, runs) =
        get_json::<Vec<PersistedRun>>(&app, &format!("/menu-advisor/branches/{branch}/runs"), &tok)
            .await;
    assert_eq!(status, 200);
    let runs = runs.unwrap();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0].id, r3);

    // Pagination: before the newest run's started_at → 2 results.
    let before = runs[0].started_at.to_rfc3339();
    let (_, page) = get_json::<Vec<PersistedRun>>(
        &app,
        &format!(
            "/menu-advisor/branches/{branch}/runs?limit=10&before={}",
            urlencoding(&before)
        ),
        &tok,
    )
    .await;
    assert_eq!(page.unwrap().len(), 2);

    // latest (default): the completed one.
    let (_, latest) = get_json::<Option<PersistedRun>>(
        &app,
        &format!("/menu-advisor/branches/{branch}/runs/latest"),
        &tok,
    )
    .await;
    assert_eq!(latest.unwrap().unwrap().id, r1);

    // latest?any_status=true: the newest regardless of status.
    let (_, latest_any) = get_json::<Option<PersistedRun>>(
        &app,
        &format!("/menu-advisor/branches/{branch}/runs/latest?any_status=true"),
        &tok,
    )
    .await;
    assert_eq!(latest_any.unwrap().unwrap().id, r3);

    // active: the in-progress one.
    let (_, active) = get_json::<Option<PersistedRun>>(
        &app,
        &format!("/menu-advisor/branches/{branch}/runs/active"),
        &tok,
    )
    .await;
    assert_eq!(active.unwrap().unwrap().id, r3);

    // get by id 404.
    let status = get_status(
        &app,
        &format!("/menu-advisor/runs/{}", Uuid::new_v4()),
        &tok,
    )
    .await;
    assert_eq!(status, 404);
}

#[sqlx::test]
async fn active_run_returns_literal_null_when_none(pool: PgPool) {
    let app = advisor_app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    let tok = token(user, Some(org), UserRole::OrgAdmin);

    for uri in [
        format!("/menu-advisor/branches/{branch}/runs/active"),
        format!("/menu-advisor/branches/{branch}/runs/latest"),
    ] {
        let req = test::TestRequest::get()
            .uri(&uri)
            .insert_header(("Authorization", format!("Bearer {tok}")))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body = test::read_body(resp).await;
        assert_eq!(&body[..], b"null", "{uri} must return literal null");
    }
}

fn urlencoding(s: &str) -> String {
    s.replace('+', "%2B").replace(':', "%3A")
}

// ─────────────────────────────────────────────────────────────────────
// Suggestion reads: round-trip fidelity + filters
// ─────────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn round_trip_preserves_flattened_wire_shape(pool: PgPool) {
    let app = advisor_app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    let tok = token(user, Some(org), UserRole::OrgAdmin);
    let seeded = seed_completed_run(&pool, org, branch).await;

    let (status, body) = get_json::<serde_json::Value>(
        &app,
        &format!("/menu-advisor/runs/{}/price-suggestions", seeded.run_id),
        &tok,
    )
    .await;
    assert_eq!(status, 200);
    let arr = body.unwrap();
    let arr = arr.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    // Ordered by popularity DESC → Latte first.
    let latte = &arr[0];
    assert_eq!(latte["item_name"], "Latte");
    // Flattened: suggestion fields are TOP-LEVEL, next to id/run_id.
    assert_eq!(latte["id"], serde_json::json!(seeded.price_id.to_string()));
    assert_eq!(
        latte["run_id"],
        serde_json::json!(seeded.run_id.to_string())
    );
    assert_eq!(latte["classification"]["mode"], "cm");
    assert_eq!(latte["classification"]["quadrant"], "star");
    assert_eq!(latte["action"], "raise_price");
    assert_eq!(latte["current_price"], 10_000);
    assert_eq!(latte["suggested_price"], 11_000);
    assert_eq!(latte["key"]["size_label"], "one_size");
    assert_eq!(latte["guard_clips"][0], "cultural_rounding");
    assert_eq!(latte["peer_comparison"]["your_position"], "below");
    assert_eq!(latte["decision"], serde_json::Value::Null);
    assert!(latte.get("suggestion").is_none(), "flatten must not nest");

    // Bundle + removal round-trip.
    let (_, bundles) = get_json::<Vec<BundleSuggestionRecord>>(
        &app,
        &format!("/menu-advisor/runs/{}/bundle-suggestions", seeded.run_id),
        &tok,
    )
    .await;
    let bundles = bundles.unwrap();
    assert_eq!(bundles.len(), 1);
    assert_eq!(bundles[0].suggestion.bundle_suggested_price, 12_500);
    assert_eq!(
        bundles[0].suggestion.forecast.incremental_cm.unwrap().mid,
        900.0
    );

    let (_, removals) = get_json::<Vec<RemovalScenarioRecord>>(
        &app,
        &format!("/menu-advisor/runs/{}/removal-scenarios", seeded.run_id),
        &tok,
    )
    .await;
    let removals = removals.unwrap();
    assert_eq!(removals.len(), 1);
    assert_eq!(
        removals[0].scenario.recommendation,
        RemovalRecommendation::Remove
    );

    // Single getters.
    for (uri, expect) in [
        (
            format!("/menu-advisor/price-suggestions/{}", seeded.price_id),
            200,
        ),
        (
            format!("/menu-advisor/bundle-suggestions/{}", seeded.bundle_id),
            200,
        ),
        (
            format!("/menu-advisor/removal-scenarios/{}", seeded.removal_id),
            200,
        ),
        (
            format!("/menu-advisor/price-suggestions/{}", Uuid::new_v4()),
            404,
        ),
    ] {
        assert_eq!(get_status(&app, &uri, &tok).await, expect, "{uri}");
    }
}

#[sqlx::test]
async fn price_suggestion_filter_matrix(pool: PgPool) {
    let app = advisor_app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    let tok = token(user, Some(org), UserRole::OrgAdmin);
    let seeded = seed_completed_run(&pool, org, branch).await;
    let run = seeded.run_id;

    let count = |filters: &'static str| {
        let app = &app;
        let tok = tok.clone();
        async move {
            let (status, v) = get_json::<Vec<serde_json::Value>>(
                app,
                &format!("/menu-advisor/runs/{run}/price-suggestions?{filters}"),
                &tok,
            )
            .await;
            assert_eq!(status, 200, "{filters}");
            v.unwrap().len()
        }
    };

    assert_eq!(count("classification_mode=cm").await, 1);
    assert_eq!(count("classification_mode=revenue").await, 1);
    assert_eq!(count("cm_quadrant=star").await, 1);
    assert_eq!(count("cm_quadrant=dog").await, 0);
    assert_eq!(count("revenue_class=quiet").await, 1);
    assert_eq!(count("action=raise_price").await, 1);
    assert_eq!(count("confidence=high").await, 1);
    assert_eq!(count("search=latte").await, 1);
    assert_eq!(count("search=LATTE").await, 1);
    assert_eq!(count("search=nope").await, 0);
    assert_eq!(count("decision_status=pending").await, 2);
    assert_eq!(count("decision_status=accepted").await, 0);

    // category filter uses the insert-time category column.
    let (_, by_cat) = get_json::<Vec<serde_json::Value>>(
        &app,
        &format!(
            "/menu-advisor/runs/{run}/price-suggestions?category_id={}",
            seeded.category_id
        ),
        &tok,
    )
    .await;
    assert_eq!(by_cat.unwrap().len(), 1);

    // Bundle + removal filters.
    let (_, b) = get_json::<Vec<serde_json::Value>>(
        &app,
        &format!("/menu-advisor/runs/{run}/bundle-suggestions?missing_costs=false"),
        &tok,
    )
    .await;
    assert_eq!(b.unwrap().len(), 1);
    let (_, b2) = get_json::<Vec<serde_json::Value>>(
        &app,
        &format!(
            "/menu-advisor/runs/{run}/bundle-suggestions?focus_menu_item_id={}",
            seeded.latte
        ),
        &tok,
    )
    .await;
    assert_eq!(
        b2.unwrap().len(),
        0,
        "focus is the croissant, not the latte"
    );
    let (_, r) = get_json::<Vec<serde_json::Value>>(
        &app,
        &format!("/menu-advisor/runs/{run}/removal-scenarios?recommendation=remove"),
        &tok,
    )
    .await;
    assert_eq!(r.unwrap().len(), 1);
    let (_, r2) = get_json::<Vec<serde_json::Value>>(
        &app,
        &format!("/menu-advisor/runs/{run}/removal-scenarios?recommendation=no_strong_signal"),
        &tok,
    )
    .await;
    assert_eq!(r2.unwrap().len(), 0);
}

// ─────────────────────────────────────────────────────────────────────
// Decisions
// ─────────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn decision_flow_latest_wins_history_retained(pool: PgPool) {
    let app = advisor_app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    let tok = token(user, Some(org), UserRole::OrgAdmin);
    let seeded = seed_completed_run(&pool, org, branch).await;

    let decide = |decision: &'static str| {
        let app = &app;
        let tok = tok.clone();
        let body = RecordDecisionBody {
            suggestion_id: seeded.price_id,
            suggestion_kind: SuggestionKind::Price,
            branch_id: branch,
            decision: decision.into(),
            notes: None,
        };
        async move { post_json(app, "/menu-advisor/decisions", &tok, &body).await }
    };

    let (status, body) = decide("accepted").await;
    assert_eq!(status, 200);
    let rec: DecisionRecord = serde_json::from_slice(&body).unwrap();
    assert_eq!(rec.suggestion_id, seeded.price_id);
    assert_eq!(rec.branch_id, branch);

    // Second decision supersedes in reads; history stays in the table.
    let (status, _) = decide("rejected").await;
    assert_eq!(status, 200);
    let (_, list) = get_json::<Vec<PriceSuggestionRecord>>(
        &app,
        &format!("/menu-advisor/runs/{}/price-suggestions", seeded.run_id),
        &tok,
    )
    .await;
    let latte = list
        .unwrap()
        .into_iter()
        .find(|s| s.id == seeded.price_id)
        .unwrap();
    assert_eq!(latte.decision.unwrap().decision.as_str(), "rejected");
    let history: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM menu_advisor_decisions WHERE suggestion_id = $1")
            .bind(seeded.price_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(history, 2);

    // decision_status filter now sees the latest decision.
    let (_, rejected) = get_json::<Vec<serde_json::Value>>(
        &app,
        &format!(
            "/menu-advisor/runs/{}/price-suggestions?decision_status=rejected",
            seeded.run_id
        ),
        &tok,
    )
    .await;
    assert_eq!(rejected.unwrap().len(), 1);

    // Invalid decision → 400.
    let bad = RecordDecisionBody {
        suggestion_id: seeded.price_id,
        suggestion_kind: SuggestionKind::Price,
        branch_id: branch,
        decision: "maybe".into(),
        notes: None,
    };
    let (status, _) = post_json(&app, "/menu-advisor/decisions", &tok, &bad).await;
    assert_eq!(status, 400);

    // Nonexistent suggestion → 404.
    let missing = RecordDecisionBody {
        suggestion_id: Uuid::new_v4(),
        suggestion_kind: SuggestionKind::Price,
        branch_id: branch,
        decision: "accepted".into(),
        notes: None,
    };
    let (status, _) = post_json(&app, "/menu-advisor/decisions", &tok, &missing).await;
    assert_eq!(status, 404);

    // Mismatched branch_id → 400 (the suggestion is the source of truth).
    let other_branch = seed_branch(&pool, org).await;
    let mismatch = RecordDecisionBody {
        suggestion_id: seeded.price_id,
        suggestion_kind: SuggestionKind::Price,
        branch_id: other_branch,
        decision: "accepted".into(),
        notes: None,
    };
    let (status, _) = post_json(&app, "/menu-advisor/decisions", &tok, &mismatch).await;
    assert_eq!(status, 400);

    // list_decisions with since.
    let (_, all) = get_json::<Vec<DecisionRecord>>(
        &app,
        &format!("/menu-advisor/branches/{branch}/decisions"),
        &tok,
    )
    .await;
    assert_eq!(all.unwrap().len(), 2);
    let future = (Utc::now() + Duration::hours(1)).to_rfc3339();
    let (_, none) = get_json::<Vec<DecisionRecord>>(
        &app,
        &format!(
            "/menu-advisor/branches/{branch}/decisions?since={}",
            urlencoding(&future)
        ),
        &tok,
    )
    .await;
    assert_eq!(none.unwrap().len(), 0);
}

// ─────────────────────────────────────────────────────────────────────
// Calibration
// ─────────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn calibration_links_accepted_decisions_to_price_epochs(pool: PgPool) {
    let app = advisor_app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    let tok = token(user, Some(org), UserRole::OrgAdmin);
    let seeded = seed_completed_run(&pool, org, branch).await;

    persistence::record_decision(
        &pool,
        seeded.price_id,
        SuggestionKind::Price,
        branch,
        crate::menu_advisor::dto::Decision::Accepted,
        None,
        user,
    )
    .await
    .unwrap();

    // Owner reprices to 11,000 (exactly the suggestion) after the decision.
    sqlx::query(
        "INSERT INTO menu_item_price_epochs (menu_item_id, size_label, price, effective_from) \
         VALUES ($1, NULL, 11000, now() + interval '1 minute')",
    )
    .bind(seeded.latte)
    .execute(&pool)
    .await
    .unwrap();

    let (status, calib) = get_json::<serde_json::Value>(
        &app,
        &format!("/menu-advisor/branches/{branch}/calibration"),
        &tok,
    )
    .await;
    assert_eq!(status, 200);
    let calib = calib.unwrap();
    let points = calib["points_cm"].as_array().unwrap();
    assert_eq!(points.len(), 1);
    assert_eq!(points[0]["realized_price"], 11_000);
    assert_eq!(points[0]["previous_price"], 10_000);
    // Sample below 10 → pct stays null.
    assert!(calib["cm_in_range_pct"].is_null());
    assert!(calib["revenue_in_range_pct"].is_null());
}

// ─────────────────────────────────────────────────────────────────────
// Promote
// ─────────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn promote_validates_bundle_org(pool: PgPool) {
    let app = advisor_app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    let tok = token(user, Some(org), UserRole::OrgAdmin);
    let seeded = seed_completed_run(&pool, org, branch).await;
    let uri = format!(
        "/menu-advisor/bundle-suggestions/{}/promote",
        seeded.bundle_id
    );

    // Nonexistent bundle → 404.
    let (status, _) = post_json(
        &app,
        &uri,
        &tok,
        &PromoteBundleBody {
            bundle_id: Uuid::new_v4(),
        },
    )
    .await;
    assert_eq!(status, 404);

    // Bundle from another org → 403.
    let other_org = seed_org(&pool).await;
    let foreign_bundle = seed_bundle(&pool, other_org).await;
    let (status, _) = post_json(
        &app,
        &uri,
        &tok,
        &PromoteBundleBody {
            bundle_id: foreign_bundle,
        },
    )
    .await;
    assert_eq!(status, 403);

    // Same-org bundle → 200 and the link is stored.
    let bundle = seed_bundle(&pool, org).await;
    let (status, _) = post_json(&app, &uri, &tok, &PromoteBundleBody { bundle_id: bundle }).await;
    assert_eq!(status, 200);
    let (_, rec) = get_json::<BundleSuggestionRecord>(
        &app,
        &format!("/menu-advisor/bundle-suggestions/{}", seeded.bundle_id),
        &tok,
    )
    .await;
    assert_eq!(rec.unwrap().promoted_bundle_id, Some(bundle));
}

// ─────────────────────────────────────────────────────────────────────
// latest-kpi
// ─────────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn latest_kpi_uses_completed_runs_only(pool: PgPool) {
    let app = advisor_app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let user = seed_user(&pool, org, "org_admin").await;
    let tok = token(user, Some(org), UserRole::OrgAdmin);
    let seeded = seed_completed_run(&pool, org, branch).await;

    let uri = format!(
        "/menu-advisor/branches/{branch}/items/{}/sizes/one_size/latest-kpi",
        seeded.latte
    );
    let (status, kpi) = get_json::<Option<PriceSuggestionRecord>>(&app, &uri, &tok).await;
    assert_eq!(status, 200);
    assert_eq!(kpi.unwrap().unwrap().id, seeded.price_id);

    // Failed runs don't count.
    sqlx::query("UPDATE menu_advisor_runs SET status = 'failed' WHERE id = $1")
        .bind(seeded.run_id)
        .execute(&pool)
        .await
        .unwrap();
    let req = test::TestRequest::get()
        .uri(&uri)
        .insert_header(("Authorization", format!("Bearer {tok}")))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let body = test::read_body(resp).await;
    assert_eq!(&body[..], b"null");
}

// ─────────────────────────────────────────────────────────────────────
// IDOR regression suite — the heart of the rebuild
// ─────────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn cross_org_access_is_refused_on_every_route(pool: PgPool) {
    let app = advisor_app!(pool);
    let org_a = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_a).await;
    seed_user(&pool, org_a, "org_admin").await;
    let seeded = seed_completed_run(&pool, org_a, branch_a).await;

    // Org B admin with full menu_items permissions — but the WRONG org.
    let org_b = seed_org(&pool).await;
    let intruder = seed_user(&pool, org_b, "org_admin").await;
    let tok_b = token(intruder, Some(org_b), UserRole::OrgAdmin);

    let branch_reads = [
        format!("/menu-advisor/branches/{branch_a}/runs"),
        format!("/menu-advisor/branches/{branch_a}/runs/latest"),
        format!("/menu-advisor/branches/{branch_a}/runs/active"),
        format!("/menu-advisor/branches/{branch_a}/decisions"),
        format!("/menu-advisor/branches/{branch_a}/calibration"),
        format!(
            "/menu-advisor/branches/{branch_a}/items/{}/sizes/one_size/latest-kpi",
            seeded.latte
        ),
    ];
    for uri in &branch_reads {
        assert_eq!(get_status(&app, uri, &tok_b).await, 403, "GET {uri}");
    }

    let record_reads = [
        format!("/menu-advisor/runs/{}", seeded.run_id),
        format!("/menu-advisor/runs/{}/price-suggestions", seeded.run_id),
        format!("/menu-advisor/runs/{}/bundle-suggestions", seeded.run_id),
        format!("/menu-advisor/runs/{}/removal-scenarios", seeded.run_id),
        format!("/menu-advisor/price-suggestions/{}", seeded.price_id),
        format!("/menu-advisor/bundle-suggestions/{}", seeded.bundle_id),
        format!("/menu-advisor/removal-scenarios/{}", seeded.removal_id),
    ];
    for uri in &record_reads {
        assert_eq!(get_status(&app, uri, &tok_b).await, 403, "GET {uri}");
    }

    // Writes.
    let (status, _) = post_json(
        &app,
        &format!("/menu-advisor/branches/{branch_a}/runs"),
        &tok_b,
        &CreateRunBody { config: None },
    )
    .await;
    assert_eq!(status, 403, "create run cross-org");

    let (status, _) = post_json(
        &app,
        "/menu-advisor/decisions",
        &tok_b,
        &RecordDecisionBody {
            suggestion_id: seeded.price_id,
            suggestion_kind: SuggestionKind::Price,
            branch_id: branch_a,
            decision: "accepted".into(),
            notes: None,
        },
    )
    .await;
    assert_eq!(status, 403, "decision against another org's suggestion");

    let bundle_b = seed_bundle(&pool, org_b).await;
    let (status, _) = post_json(
        &app,
        &format!(
            "/menu-advisor/bundle-suggestions/{}/promote",
            seeded.bundle_id
        ),
        &tok_b,
        &PromoteBundleBody {
            bundle_id: bundle_b,
        },
    )
    .await;
    assert_eq!(status, 403, "promote another org's suggestion");
}

#[sqlx::test]
async fn branch_assignment_gates_non_admins_and_super_admin_bypasses(pool: PgPool) {
    let app = advisor_app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    seed_user(&pool, org, "org_admin").await;
    let seeded = seed_completed_run(&pool, org, branch).await;
    let uri = format!("/menu-advisor/runs/{}/price-suggestions", seeded.run_id);

    // Same-org branch manager WITHOUT an assignment → 403.
    let manager = seed_user(&pool, org, "branch_manager").await;
    let tok_mgr = token(manager, Some(org), UserRole::BranchManager);
    assert_eq!(get_status(&app, &uri, &tok_mgr).await, 403);

    // With the assignment → 200.
    assign_branch(&pool, manager, branch).await;
    assert_eq!(get_status(&app, &uri, &tok_mgr).await, 200);

    // Super admin from nowhere → 200.
    let root = seed_user(&pool, org, "super_admin").await;
    let tok_root = token(root, None, UserRole::SuperAdmin);
    assert_eq!(get_status(&app, &uri, &tok_root).await, 200);
}

// ─────────────────────────────────────────────────────────────────────
// Adapter tests (real schema → engine inputs)
// ─────────────────────────────────────────────────────────────────────

struct OrderSeeder {
    branch: Uuid,
    shift: Uuid,
    teller: Uuid,
    counter: i32,
}

impl OrderSeeder {
    async fn new(pool: &PgPool, org: Uuid, branch: Uuid) -> Self {
        let teller = seed_user(pool, org, "teller").await;
        let shift: Uuid = sqlx::query_scalar(
            "INSERT INTO shifts (branch_id, teller_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(branch)
        .bind(teller)
        .fetch_one(pool)
        .await
        .unwrap();
        Self {
            branch,
            shift,
            teller,
            counter: 0,
        }
    }

    async fn order(&mut self, pool: &PgPool, days_ago: i32) -> Uuid {
        self.counter += 1;
        sqlx::query_scalar(
            "INSERT INTO orders (branch_id, shift_id, teller_id, order_number, status, \
                                 payment_method, subtotal, total_amount, created_at, order_ref) \
             VALUES ($1, $2, $3, $4, 'completed', 'cash', 1000, 1000, \
                     now() - make_interval(days => $5), gen_random_uuid()::text) \
             RETURNING id",
        )
        .bind(self.branch)
        .bind(self.shift)
        .bind(self.teller)
        .bind(self.counter)
        .bind(days_ago)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Standalone order line; `unit_cost` is the sale-time piastre snapshot.
    async fn line(
        &self,
        pool: &PgPool,
        order: Uuid,
        item: Uuid,
        qty: i32,
        unit_price: i32,
        unit_cost: Option<i64>,
    ) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO order_items (order_id, menu_item_id, item_name, size_label, \
                                      unit_price, quantity, line_total, unit_cost, cost_missing) \
             VALUES ($1, $2, 'x', NULL, $3, $4, $5, $6, $7) RETURNING id",
        )
        .bind(order)
        .bind(item)
        .bind(unit_price)
        .bind(qty)
        .bind(unit_price * qty)
        .bind(unit_cost)
        .bind(unit_cost.is_none())
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Bundle purchase: one order line with menu_item_id NULL + component rows.
    async fn bundle_line(&self, pool: &PgPool, order: Uuid, bundle: Uuid, components: &[Uuid]) {
        let line_id: Uuid = sqlx::query_scalar(
            "INSERT INTO order_items (order_id, menu_item_id, item_name, unit_price, \
                                      quantity, line_total, bundle_id) \
             VALUES ($1, NULL, 'Combo', 9000, 1, 9000, $2) RETURNING id",
        )
        .bind(order)
        .bind(bundle)
        .fetch_one(pool)
        .await
        .unwrap();
        for item in components {
            sqlx::query(
                "INSERT INTO order_line_bundle_components (order_line_id, item_id, quantity, size_label) \
                 VALUES ($1, $2, 1, NULL)",
            )
            .bind(line_id)
            .bind(item)
            .execute(pool)
            .await
            .unwrap();
        }
    }
}

async fn seed_recipe_with_cost(pool: &PgPool, org: Uuid, item: Uuid, qty: f64, piastre_cost: f64) {
    let ingredient: Uuid = sqlx::query_scalar(
        "INSERT INTO org_ingredients (org_id, name, unit, cost_per_unit) \
         VALUES ($1, $2, 'g'::inventory_unit, $3) RETURNING id",
    )
    .bind(org)
    .bind(format!("ing-{}", Uuid::new_v4()))
    .bind(rust_decimal::Decimal::try_from(piastre_cost).unwrap())
    .fetch_one(pool)
    .await
    .unwrap();
    // Open cost-history epoch (covers all timestamps in the tests).
    sqlx::query(
        "INSERT INTO ingredient_cost_history (org_ingredient_id, cost_per_unit, effective_from) \
         VALUES ($1, $2, now() - interval '400 days')",
    )
    .bind(ingredient)
    .bind(rust_decimal::Decimal::try_from(piastre_cost).unwrap())
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO menu_item_recipes (menu_item_id, size_label, quantity_used, \
                                        ingredient_name, ingredient_unit, org_ingredient_id) \
         VALUES ($1, 'one_size', $2, 'ing', 'g', $3)",
    )
    .bind(item)
    .bind(rust_decimal::Decimal::try_from(qty).unwrap())
    .bind(ingredient)
    .execute(pool)
    .await
    .unwrap();
}

/// Piastre rollups (no currency conversion) + sale-time snapshot priority.
#[sqlx::test]
async fn adapter_costs_in_piastres_with_snapshot_priority(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, "Latte", 10_000).await;
    // Recipe: 3 units × 200 piastres = 600 piastres.
    seed_recipe_with_cost(&pool, org, item, 3.0, 200.0).await;

    let mut seeder = OrderSeeder::new(&pool, org, branch).await;
    let o1 = seeder.order(&pool, 1).await;
    seeder.line(&pool, o1, item, 1, 10_000, Some(555)).await; // snapshot wins
    let o2 = seeder.order(&pool, 2).await;
    seeder.line(&pool, o2, item, 1, 10_000, None).await; // reconstructed

    let inputs = adapter::load_inputs(&pool, org, branch, Utc::now(), &AnalysisConfig::default())
        .await
        .unwrap();

    let snap = inputs
        .snapshots
        .iter()
        .find(|s| s.key.menu_item_id == item)
        .unwrap();
    assert_eq!(
        snap.cost_per_serving,
        Some(600),
        "current rollup must be piastres"
    );
    assert_eq!(snap.current_price, 10_000);

    let mut costs: Vec<Option<i64>> = inputs
        .sales
        .iter()
        .filter(|s| s.key.menu_item_id == item)
        .map(|s| s.unit_cost_at_sale)
        .collect();
    costs.sort();
    assert_eq!(
        costs,
        vec![Some(555), Some(600)],
        "snapshot first, rollup fallback"
    );
}

#[sqlx::test]
async fn adapter_baskets_include_bundle_components_and_detect_bundle_only(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let latte = seed_menu_item(&pool, org, "Latte", 10_000).await;
    let cookie = seed_menu_item(&pool, org, "Cookie", 3_000).await; // bundle-only
    let bundle = seed_bundle(&pool, org).await;

    let mut seeder = OrderSeeder::new(&pool, org, branch).await;
    let o = seeder.order(&pool, 1).await;
    seeder.line(&pool, o, latte, 1, 10_000, None).await; // standalone latte
    seeder.bundle_line(&pool, o, bundle, &[cookie]).await; // cookie only via bundle

    let inputs = adapter::load_inputs(&pool, org, branch, Utc::now(), &AnalysisConfig::default())
        .await
        .unwrap();

    // Basket contains BOTH the standalone latte and the bundle's cookie.
    assert_eq!(inputs.baskets.len(), 1);
    let basket = &inputs.baskets[0];
    assert!(basket.iter().any(|k| k.menu_item_id == latte));
    assert!(
        basket.iter().any(|k| k.menu_item_id == cookie),
        "components count in baskets"
    );

    // Cookie is bundle_only; latte is not. Sales contain only the latte.
    let cookie_snap = inputs
        .snapshots
        .iter()
        .find(|s| s.key.menu_item_id == cookie)
        .unwrap();
    assert!(cookie_snap.bundle_only);
    let latte_snap = inputs
        .snapshots
        .iter()
        .find(|s| s.key.menu_item_id == latte)
        .unwrap();
    assert!(!latte_snap.bundle_only);
    assert!(inputs.sales.iter().all(|s| s.key.menu_item_id != cookie));
}

#[sqlx::test]
async fn adapter_first_price_epoch_is_not_a_change(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, "Latte", 10_000).await;

    // Creation-seeded epoch (inside the window) — must NOT flag.
    sqlx::query(
        "INSERT INTO menu_item_price_epochs (menu_item_id, size_label, price, effective_from) \
         VALUES ($1, NULL, 10000, now() - interval '5 days')",
    )
    .bind(item)
    .execute(&pool)
    .await
    .unwrap();

    let inputs = adapter::load_inputs(&pool, org, branch, Utc::now(), &AnalysisConfig::default())
        .await
        .unwrap();
    assert!(
        inputs.price_changed_keys.is_empty(),
        "item creation must not count as a price change"
    );

    // A SECOND epoch is a genuine change — must flag.
    sqlx::query(
        "INSERT INTO menu_item_price_epochs (menu_item_id, size_label, price, effective_from) \
         VALUES ($1, NULL, 11000, now() - interval '2 days')",
    )
    .bind(item)
    .execute(&pool)
    .await
    .unwrap();
    let inputs = adapter::load_inputs(&pool, org, branch, Utc::now(), &AnalysisConfig::default())
        .await
        .unwrap();
    assert!(inputs.price_changed_keys.contains(&item_key(item)));
}

#[sqlx::test]
async fn adapter_size_epoch_flags_only_that_size(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, "Latte", 10_000).await;
    for (label, price) in [("small", 8_000), ("large", 12_000)] {
        sqlx::query(
            "INSERT INTO item_sizes (menu_item_id, label, price_override) \
             VALUES ($1, $2, $3)",
        )
        .bind(item)
        .bind(label)
        .bind(price as i32)
        .execute(&pool)
        .await
        .unwrap();
    }
    // Pre-window baseline epoch + in-window change, both for 'small' only.
    sqlx::query(
        "INSERT INTO menu_item_price_epochs (menu_item_id, size_label, price, effective_from) \
         VALUES ($1, 'small', 8000, now() - interval '100 days'), \
                ($1, 'small', 8500, now() - interval '2 days')",
    )
    .bind(item)
    .execute(&pool)
    .await
    .unwrap();

    let inputs = adapter::load_inputs(&pool, org, branch, Utc::now(), &AnalysisConfig::default())
        .await
        .unwrap();
    let small = ItemKey {
        menu_item_id: item,
        size_label: "small".into(),
    };
    let large = ItemKey {
        menu_item_id: item,
        size_label: "large".into(),
    };
    assert!(inputs.price_changed_keys.contains(&small));
    assert!(
        !inputs.price_changed_keys.contains(&large),
        "no fan-out across sizes"
    );

    // Both sizes exist as snapshots with their override prices.
    let small_snap = inputs.snapshots.iter().find(|s| s.key == small).unwrap();
    assert_eq!(small_snap.current_price, 8_000);
}
