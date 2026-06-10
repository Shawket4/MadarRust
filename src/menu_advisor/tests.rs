#![allow(unused_imports, unused_variables, dead_code)]
use actix_web::{test, web, App, HttpMessage};
use sqlx::PgPool;
use uuid::Uuid;
use chrono::Utc;
use std::collections::HashMap;

use crate::{
    auth::jwt::{create_token, JwtSecret},
    menu_advisor::{
        engine::{
            AdvisorReport, AnalysisConfig, ModeSummary, Classification, CmQuadrant, RevenueClass,
            Action, Confidence, GuardClip, ItemKey, PriceSuggestion, BundleSuggestion,
            RemovalScenario, PriceAnchors, PeerComparison, PeerPosition, BundleAssociation,
            BundleForecast, Triplet, WilsonInterval, AbsorbedBy, ComplementaryLoss,
            RemovalRecommendation
        },
        handlers::{CreateRunBody, PromoteBundleBody, RecordDecisionBody},
        persistence::{
            Decision, DecisionRecord, PersistedRun, PriceSuggestionRecord, BundleSuggestionRecord,
            RemovalScenarioRecord, RunStatus, SuggestionKind
        },
        routes,
    },
};

// -----------------------------------------------------------------------------
// Seeding Helpers
// -----------------------------------------------------------------------------

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
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Main Branch')")
        .bind(branch_id)
        .bind(org_id)
        .execute(pool)
        .await
        .unwrap();
    branch_id
}

async fn seed_user(pool: &PgPool, org_id: Uuid, role: &str) -> Uuid {
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES ($1, $2, 'Test User', $3, 'hash', $4::user_role)",
    )
    .bind(user_id)
    .bind(org_id)
    .bind(format!("{}@example.com", Uuid::new_v4()))
    .bind(role)
    .execute(pool)
    .await
    .unwrap();

    seed_permission(pool, org_id, role, "menu_items", "read").await;
    seed_permission(pool, org_id, role, "menu_items", "update").await;
    
    user_id
}

async fn seed_permission(pool: &PgPool, _org_id: Uuid, role_name: &str, resource: &str, action: &str) {
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action) VALUES ($1::user_role, $2::permission_resource, $3::permission_action) ON CONFLICT DO NOTHING",
    )
    .bind(role_name)
    .bind(resource)
    .bind(action)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_menu_item(pool: &PgPool, org_id: Uuid) -> Uuid {
    let item_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO menu_items (id, org_id, name, is_active) VALUES ($1, $2, 'Test Item', true)",
    )
    .bind(item_id)
    .bind(org_id)
    .execute(pool)
    .await
    .unwrap();
    item_id
}

fn generate_org_admin_token(user_id: Uuid, org_id: Uuid) -> String {
    let secret = JwtSecret("test_secret".to_string());
    create_token(
        &secret,
        user_id,
        Some(org_id),
        crate::models::UserRole::OrgAdmin,
        None,
        24,
    )
    .unwrap()
}

// -----------------------------------------------------------------------------
// Helper to seed a fake run and suggestions
// -----------------------------------------------------------------------------

async fn seed_fake_completed_run(pool: &PgPool, branch_id: Uuid, org_id: Uuid, menu_item_id: Uuid) -> (Uuid, Uuid, Uuid, Uuid) {
    let run_id = Uuid::new_v4();
    let config_json = serde_json::to_value(AnalysisConfig::default()).unwrap();
    
    // Seed Run
    sqlx::query(
        r#"
        INSERT INTO menu_advisor_runs (
            id, branch_id, org_id, status, config_json, started_at, completed_at,
            items_total, items_cm_tracked, items_revenue_only, items_insufficient, window_days
        ) VALUES ($1, $2, $3, 'completed', $4, NOW(), NOW(), 10, 8, 2, 0, 30.0)
        "#,
    )
    .bind(run_id).bind(branch_id).bind(org_id).bind(config_json)
    .execute(pool).await.unwrap();

    // Price Suggestion
    let p_sug_id = Uuid::new_v4();
    let anchors = serde_json::to_value(PriceAnchors {
        cost_plus: Some(100.0),
        peer_median: 120.0,
        status_quo: 90.0,
    }).unwrap();
    
    sqlx::query(
        r#"
        INSERT INTO menu_advisor_price_suggestions (
            id, run_id, branch_id, menu_item_id, size_label, item_name,
            classification_mode, cm_quadrant, current_price, units_sold_raw,
            effective_price, popularity_share, cm_per_unit, margin_pct, food_cost_pct,
            anchors_json, suggested_price, suggested_delta_abs, suggested_delta_pct,
            action, confidence, explanation, guard_clips_json, price_changed_in_window,
            cost_missing, created_at
        ) VALUES (
            $1, $2, $3, $4, 'one_size', 'Test Item',
            'cm', 'star', 90, 100, 90.0, 0.1, 40.0, 0.44, 0.56,
            $5, 110, 20, 0.22, 'raise_price', 'high', 'Test', '[]', false, false, NOW()
        )
        "#
    )
    .bind(p_sug_id).bind(run_id).bind(branch_id).bind(menu_item_id).bind(anchors)
    .execute(pool).await.unwrap();

    // Bundle Suggestion
    let b_sug_id = Uuid::new_v4();
    let components = serde_json::to_value(vec![ItemKey { menu_item_id, size_label: "one_size".to_string() }]).unwrap();
    let assoc = serde_json::to_value(BundleAssociation { pair_lifts: vec![], composite_score: 1.5 }).unwrap();
    let forecast = serde_json::to_value(BundleForecast {
        expected_velocity: Triplet { lo: 10.0, mid: 20.0, hi: 30.0 },
        inside_bundle_units_x: 5.0,
        halo_units_x: 2.0,
        total_units_uplift_x: 7.0,
        incremental_cm: None,
    }).unwrap();

    sqlx::query(
        r#"
        INSERT INTO menu_advisor_bundle_suggestions (
            id, run_id, branch_id, focus_menu_item_id, focus_size_label, components_json,
            bundle_list_price, bundle_suggested_price, bundle_discount_pct, association_json, forecast_json,
            guard_clips_json, explanation, missing_costs, created_at
        ) VALUES (
            $1, $2, $3, $4, 'one_size', $5,
            150, 120, 0.2, $6, $7, '[]', 'Test Bundle', false, NOW()
        )
        "#
    )
    .bind(b_sug_id).bind(run_id).bind(branch_id).bind(menu_item_id)
    .bind(components).bind(assoc).bind(forecast)
    .execute(pool).await.unwrap();

    // Removal Scenario
    let r_sug_id = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO menu_advisor_removal_scenarios (
            id, run_id, branch_id, menu_item_id, size_label, item_name,
            baseline_cm, absorbed_by_json, complementary_losses_json,
            net_cm_change, net_cm_change_lo, net_cm_change_hi,
            recommendation, explanation, created_at
        ) VALUES (
            $1, $2, $3, $4, 'one_size', 'Test Dog',
            100.0, '[]', '[]', -10.0, -20.0, 0.0, 'remove', 'Test Remove', NOW()
        )
        "#
    )
    .bind(r_sug_id).bind(run_id).bind(branch_id).bind(menu_item_id)
    .execute(pool).await.unwrap();

    (run_id, p_sug_id, b_sug_id, r_sug_id)
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[sqlx::test]
async fn test_create_run_success(pool: PgPool) {
    let app = actix_web::test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(JwtSecret("test_secret".to_string())))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);

    let req = actix_web::test::TestRequest::post()
        .uri(&format!("/menu-advisor/branches/{}/runs", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&CreateRunBody { config: None })
        .to_request();

    let resp = actix_web::test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::ACCEPTED);

    // Verify DB
    let runs = crate::menu_advisor::persistence::list_runs(&pool, branch_id, 10, None).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::InProgress);
}

#[sqlx::test]
async fn test_create_run_conflict(pool: PgPool) {
    let app = actix_web::test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(JwtSecret("test_secret".to_string())))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);

    crate::menu_advisor::persistence::create_run(&pool, org_id, branch_id, &AnalysisConfig::default()).await.unwrap();

    let req = actix_web::test::TestRequest::post()
        .uri(&format!("/menu-advisor/branches/{}/runs", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&CreateRunBody { config: None })
        .to_request();

    let resp = actix_web::test::call_service(&app, req).await;
    assert_eq!(resp.status(), actix_web::http::StatusCode::CONFLICT);
}

#[sqlx::test]
async fn test_list_runs(pool: PgPool) {
    let app = actix_web::test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(JwtSecret("test_secret".to_string())))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);

    let menu_item_id = seed_menu_item(&pool, org_id).await;
    seed_fake_completed_run(&pool, branch_id, org_id, menu_item_id).await;
    crate::menu_advisor::persistence::create_run(&pool, org_id, branch_id, &AnalysisConfig::default()).await.unwrap();

    let req = actix_web::test::TestRequest::get()
        .uri(&format!("/menu-advisor/branches/{}/runs?limit=10", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();

    let resp = actix_web::test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    let body: Vec<PersistedRun> = actix_web::test::read_body_json(resp).await;
    assert_eq!(body.len(), 2);
}

#[sqlx::test]
async fn test_get_run_endpoints(pool: PgPool) {
    let app = actix_web::test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(JwtSecret("test_secret".to_string())))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);

    let menu_item_id = seed_menu_item(&pool, org_id).await;
    let (run_id, _, _, _) = seed_fake_completed_run(&pool, branch_id, org_id, menu_item_id).await;

    // Get specific run
    let req = actix_web::test::TestRequest::get()
        .uri(&format!("/menu-advisor/runs/{}", run_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = actix_web::test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // Get latest run
    let req = actix_web::test::TestRequest::get()
        .uri(&format!("/menu-advisor/branches/{}/runs/latest", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = actix_web::test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // Get active run (should be None)
    let req = actix_web::test::TestRequest::get()
        .uri(&format!("/menu-advisor/branches/{}/runs/active", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = actix_web::test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let bytes = actix_web::test::read_body(resp).await;
    assert_eq!(bytes, actix_web::web::Bytes::from_static(b"null"));
}

#[sqlx::test]
async fn test_suggestions_retrieval(pool: PgPool) {
    let app = actix_web::test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(JwtSecret("test_secret".to_string())))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);

    let menu_item_id = seed_menu_item(&pool, org_id).await;
    let (run_id, p_id, b_id, r_id) = seed_fake_completed_run(&pool, branch_id, org_id, menu_item_id).await;

    let req = actix_web::test::TestRequest::get()
        .uri(&format!("/menu-advisor/runs/{}/price-suggestions", run_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = actix_web::test::call_service(&app, req).await;
    let status = resp.status();
    if !status.is_success() {
        let body = actix_web::test::read_body(resp).await;
        panic!("Failed to get price suggestions: {} - {:?}", status, body);
    }
    let suggestions: Vec<PriceSuggestionRecord> = actix_web::test::read_body_json(resp).await;
    assert_eq!(suggestions.len(), 1);

    // Get Single Price Suggestion
    let req = actix_web::test::TestRequest::get()
        .uri(&format!("/menu-advisor/price-suggestions/{}", p_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = actix_web::test::call_service(&app, req).await;
    let status = resp.status();
    if !status.is_success() {
        let body = actix_web::test::read_body(resp).await;
        panic!("Failed to get single price suggestion: {} - {:?}", status, body);
    }

    // List Bundle Suggestions
    let req = actix_web::test::TestRequest::get()
        .uri(&format!("/menu-advisor/runs/{}/bundle-suggestions", run_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = actix_web::test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let bundles: Vec<BundleSuggestionRecord> = actix_web::test::read_body_json(resp).await;
    assert_eq!(bundles.len(), 1);

    // Get Single Bundle Suggestion
    let req = actix_web::test::TestRequest::get()
        .uri(&format!("/menu-advisor/bundle-suggestions/{}", b_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = actix_web::test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // List Removal Scenarios
    let req = actix_web::test::TestRequest::get()
        .uri(&format!("/menu-advisor/runs/{}/removal-scenarios", run_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = actix_web::test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let removals: Vec<RemovalScenarioRecord> = actix_web::test::read_body_json(resp).await;
    assert_eq!(removals.len(), 1);

    // Get Single Removal Scenario
    let req = actix_web::test::TestRequest::get()
        .uri(&format!("/menu-advisor/removal-scenarios/{}", r_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = actix_web::test::call_service(&app, req).await;
    assert!(resp.status().is_success());
}

#[sqlx::test]
async fn test_decisions_and_calibration(pool: PgPool) {
    let app = actix_web::test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(JwtSecret("test_secret".to_string())))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);

    let menu_item_id = seed_menu_item(&pool, org_id).await;
    let (_, p_id, b_id, _) = seed_fake_completed_run(&pool, branch_id, org_id, menu_item_id).await;

    // Record decision
    let req = actix_web::test::TestRequest::post()
        .uri("/menu-advisor/decisions")
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&RecordDecisionBody {
            suggestion_id: p_id,
            suggestion_kind: SuggestionKind::Price,
            branch_id,
            decision: "accepted".to_string(),
            notes: Some("Looks good".to_string()),
        })
        .to_request();
    let resp = actix_web::test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // List decisions
    let req = actix_web::test::TestRequest::get()
        .uri(&format!("/menu-advisor/branches/{}/decisions", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = actix_web::test::call_service(&app, req).await;
    assert!(resp.status().is_success());
    let decisions: Vec<DecisionRecord> = actix_web::test::read_body_json(resp).await;
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].decision, Decision::Accepted);

    // Promote bundle
    let new_bundle_id = Uuid::new_v4();
    let req = actix_web::test::TestRequest::post()
        .uri(&format!("/menu-advisor/bundle-suggestions/{}/promote", b_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .set_json(&PromoteBundleBody { bundle_id: new_bundle_id })
        .to_request();
    let resp = actix_web::test::call_service(&app, req).await;
    assert!(resp.status().is_success());

    // Get Calibration
    let req = actix_web::test::TestRequest::get()
        .uri(&format!("/menu-advisor/branches/{}/calibration", branch_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = actix_web::test::call_service(&app, req).await;
    let status = resp.status();
    if !status.is_success() {
        let body = actix_web::test::read_body(resp).await;
        panic!("Failed to get calibration: {} - {:?}", status, body);
    }
}

#[sqlx::test]
async fn test_latest_kpi(pool: PgPool) {
    let app = actix_web::test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(JwtSecret("test_secret".to_string())))
            .configure(routes::configure),
    )
    .await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id, "org_admin").await;
    let token = generate_org_admin_token(user_id, org_id);

    let menu_item_id = seed_menu_item(&pool, org_id).await;
    let (run_id, _, _, _) = seed_fake_completed_run(&pool, branch_id, org_id, menu_item_id).await;

    // To test KPI, we need a price suggestion
    let p_sugs = crate::menu_advisor::persistence::list_price_suggestions(&pool, run_id, &Default::default()).await.unwrap();
    let item_id = p_sugs[0].suggestion.key.menu_item_id;

    // Get KPI
    let req = actix_web::test::TestRequest::get()
        .uri(&format!("/menu-advisor/branches/{}/items/{}/sizes/one_size/latest-kpi", branch_id, item_id))
        .insert_header(("Authorization", format!("Bearer {}", token)))
        .to_request();
    let resp = actix_web::test::call_service(&app, req).await;
    assert!(resp.status().is_success());
}

// ═══════════════════════════════════════════════════════════════════
// Adapter — cost sourcing in piastres
// ═══════════════════════════════════════════════════════════════════

#[sqlx::test]
async fn test_adapter_costs_in_piastres_with_snapshot_priority(pool: PgPool) {
    use chrono::Utc;

    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'O', $2)")
        .bind(org_id).bind(format!("adp-{org_id}")).execute(&pool).await.unwrap();
    let branch_id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'B')")
        .bind(branch_id).bind(org_id).execute(&pool).await.unwrap();
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES ($1, $2, 'U', $3, 'h', 'teller'::user_role)")
        .bind(user_id).bind(org_id).bind(format!("a-{user_id}@t.com")).execute(&pool).await.unwrap();
    let shift_id = Uuid::new_v4();
    sqlx::query("INSERT INTO shifts (id, branch_id, teller_id, status, opening_cash) VALUES ($1, $2, $3, 'open', 0)")
        .bind(shift_id).bind(branch_id).bind(user_id).execute(&pool).await.unwrap();

    let cat = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'C')")
        .bind(cat).bind(org_id).execute(&pool).await.unwrap();
    let item = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES ($1, $2, $3, 'Latte', 7000, true)")
        .bind(item).bind(org_id).bind(cat).execute(&pool).await.unwrap();

    // Recipe: 10 g @ 2.50 EGP/g → snapshot rollup must be 2 500 piastres,
    // resolved via the org_ingredients fallback (no history rows seeded).
    let ing = Uuid::new_v4();
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category) VALUES ($1, $2, 'Beans', 'g'::inventory_unit, 2.50, 'coffee_bean')")
        .bind(ing).bind(org_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO menu_item_recipes (menu_item_id, org_ingredient_id, quantity_used, size_label, ingredient_name, ingredient_unit) VALUES ($1, $2, 10.0, 'one_size', 'Beans', 'g')")
        .bind(item).bind(ing).execute(&pool).await.unwrap();

    // Completed order with an explicit sale-time snapshot (unit_cost = 3 000
    // piastres ≠ current rollup) — adapter must prefer the snapshot.
    let order = Uuid::new_v4();
    sqlx::query("INSERT INTO orders (id, branch_id, shift_id, teller_id, order_number, payment_method, subtotal, discount_value, discount_amount, tax_amount, total_amount, status) VALUES ($1, $2, $3, $4, 1, 'cash', 7000, 0, 0, 0, 7000, 'completed')")
        .bind(order).bind(branch_id).bind(shift_id).bind(user_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO order_items (id, order_id, menu_item_id, item_name, quantity, unit_price, line_total, unit_cost, line_cost, cost_missing) VALUES ($1, $2, $3, 'Latte', 1, 7000, 7000, 3000, 3000, false)")
        .bind(Uuid::new_v4()).bind(order).bind(item).execute(&pool).await.unwrap();

    // Legacy line without snapshot — adapter must reconstruct 2 500 piastres
    // from the recipe rollup (× 100 fix), not 25.
    let order2 = Uuid::new_v4();
    sqlx::query("INSERT INTO orders (id, branch_id, shift_id, teller_id, order_number, payment_method, subtotal, discount_value, discount_amount, tax_amount, total_amount, status) VALUES ($1, $2, $3, $4, 2, 'cash', 7000, 0, 0, 0, 7000, 'completed')")
        .bind(order2).bind(branch_id).bind(shift_id).bind(user_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO order_items (id, order_id, menu_item_id, item_name, quantity, unit_price, line_total) VALUES ($1, $2, $3, 'Latte', 1, 7000, 7000)")
        .bind(Uuid::new_v4()).bind(order2).bind(item).execute(&pool).await.unwrap();

    let config = AnalysisConfig::default();
    let inputs = crate::menu_advisor::adapter::load_inputs(&pool, org_id, branch_id, Utc::now(), &config)
        .await
        .unwrap();

    let snap = inputs
        .snapshots
        .iter()
        .find(|s| s.key.menu_item_id == item)
        .unwrap();
    assert_eq!(snap.cost_per_serving, Some(2_500), "snapshot rollup must be piastres");

    let mut costs: Vec<Option<i64>> = inputs
        .sales
        .iter()
        .filter(|s| s.key.menu_item_id == item)
        .map(|s| s.unit_cost_at_sale)
        .collect();
    costs.sort();
    assert_eq!(costs, vec![Some(2_500), Some(3_000)], "snapshot preferred, fallback in piastres");
}
