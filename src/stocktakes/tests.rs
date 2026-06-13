#![allow(unused_imports, unused_variables, dead_code)]
use actix_web::{test, App, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::stocktakes::routes;
use crate::stocktakes::handlers::{Stocktake, StocktakeFull, VarianceReport};

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn org_admin_token(user_id: Uuid, org_id: Uuid) -> String {
    crate::auth::jwt::create_token(&get_secret(), user_id, Some(org_id), UserRole::OrgAdmin, None, 24).unwrap()
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(id).bind(format!("org-{id}")).execute(pool).await.unwrap();
    id
}
async fn seed_branch(pool: &PgPool, org_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Branch')")
        .bind(id).bind(org_id).execute(pool).await.unwrap();
    id
}
async fn seed_user(pool: &PgPool, org_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES ($1, $2, 'U', $3, 'h', 'org_admin'::user_role)")
        .bind(id).bind(org_id).bind(format!("u-{id}@t.com")).execute(pool).await.unwrap();
    id
}
async fn grant(pool: &PgPool, resource: &str, action: &str) {
    sqlx::query("INSERT INTO role_permissions (role, resource, action, granted) VALUES ('org_admin'::user_role, $1::permission_resource, $2::permission_action, true) ON CONFLICT DO NOTHING")
        .bind(resource).bind(action).execute(pool).await.unwrap();
}
async fn seed_ingredient(pool: &PgPool, org_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, category, cost_per_unit) VALUES ($1, $2, 'Milk', 'ml'::inventory_unit, 'dairy', 300)")
        .bind(id).bind(org_id).execute(pool).await.unwrap();
    id
}
async fn seed_stock(pool: &PgPool, branch_id: Uuid, ing_id: Uuid, qty: f64) {
    sqlx::query("INSERT INTO branch_inventory (branch_id, org_ingredient_id, current_stock, reorder_threshold) VALUES ($1, $2, $3, 0)")
        .bind(branch_id).bind(ing_id).bind(qty).execute(pool).await.unwrap();
}

#[sqlx::test]
async fn test_stocktake_reconciles_stock_and_posts_variance(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    for (r, a) in [("stocktakes", "create"), ("stocktakes", "read"), ("stocktakes", "update")] {
        grant(&pool, r, a).await;
    }
    let ing = seed_ingredient(&pool, org_id).await;
    seed_stock(&pool, branch_id, ing, 100.0).await; // expected 100
    let token = org_admin_token(user_id, org_id);

    // Create stocktake — snapshots expected_qty = 100.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/stocktakes/branches/{branch_id}"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(serde_json::json!({"note": "monthly"}))
        .to_request()).await;
    assert_eq!(resp.status(), 201);
    let full: StocktakeFull = test::read_body_json(resp).await;
    assert_eq!(full.items.len(), 1);
    let stocktake_id = full.stocktake.id;

    // Count 92 (shrinkage of 8).
    let resp = test::call_service(&app, test::TestRequest::put()
        .uri(&format!("/stocktakes/{stocktake_id}/items"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(serde_json::json!({"items": [{"org_ingredient_id": ing, "counted_qty": 92.0}]}))
        .to_request()).await;
    assert!(resp.status().is_success());

    // Finalize → reconcile stock to 92 + post a stock_count movement.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/stocktakes/{stocktake_id}/finalize"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request()).await;
    assert!(resp.status().is_success());
    let finalized: StocktakeFull = test::read_body_json(resp).await;
    assert_eq!(finalized.stocktake.status, "finalized");

    // Branch stock is now the counted value.
    let stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE branch_id = $1 AND org_ingredient_id = $2")
        .bind(branch_id).bind(ing).fetch_one(&pool).await.unwrap();
    assert_eq!(stock, 92.0);

    // A stock_count movement was recorded with the variance (-8).
    let (mtype, mqty): (String, f64) = sqlx::query_as("SELECT type::text, quantity::float8 FROM inventory_movements WHERE source_type = 'stocktake' AND source_id = $1")
        .bind(stocktake_id).fetch_one(&pool).await.unwrap();
    assert_eq!(mtype, "stock_count");
    assert_eq!(mqty, -8.0);

    // Variance report values the shrinkage (8 × 300 piastres = 2400).
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/stocktakes/{stocktake_id}/variance-report"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request()).await;
    assert!(resp.status().is_success());
    let report: VarianceReport = test::read_body_json(resp).await;
    assert_eq!(report.total_shrinkage_value, 2400);
    assert_eq!(report.net_variance_value, -2400);
}

#[sqlx::test]
async fn test_only_one_open_stocktake_per_branch(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    grant(&pool, "stocktakes", "create").await;
    let token = org_admin_token(user_id, org_id);

    let mk = || test::TestRequest::post()
        .uri(&format!("/stocktakes/branches/{branch_id}"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(serde_json::json!({}))
        .to_request();

    assert_eq!(test::call_service(&app, mk()).await.status(), 201);
    // Second open stocktake on same branch is rejected.
    assert_eq!(test::call_service(&app, mk()).await.status(), 409);
}

#[sqlx::test]
async fn test_finalize_requires_reason_for_large_variance(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    for (r, a) in [("stocktakes", "create"), ("stocktakes", "read"), ("stocktakes", "update")] {
        grant(&pool, r, a).await;
    }
    let ing = seed_ingredient(&pool, org_id).await;
    seed_stock(&pool, branch_id, ing, 100.0).await; // expected 100
    let token = org_admin_token(user_id, org_id);

    // Start — default org threshold is 10%.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/stocktakes/branches/{branch_id}"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(serde_json::json!({})).to_request()).await;
    let full: StocktakeFull = test::read_body_json(resp).await;
    assert_eq!(full.variance_threshold_pct, 10.0);
    let stocktake_id = full.stocktake.id;

    // Count 80 → 20% shrinkage = flagged, but no reason given.
    let count = |reason: Option<&'static str>| {
        let mut item = serde_json::json!({"org_ingredient_id": ing, "counted_qty": 80.0});
        if let Some(r) = reason { item["variance_reason"] = serde_json::json!(r); }
        test::TestRequest::put()
            .uri(&format!("/stocktakes/{stocktake_id}/items"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(serde_json::json!({"items": [item]})).to_request()
    };
    assert!(test::call_service(&app, count(None)).await.status().is_success());

    // Finalize is blocked (409) until the flagged row carries a reason.
    let finalize = || test::TestRequest::post()
        .uri(&format!("/stocktakes/{stocktake_id}/finalize"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request();
    assert_eq!(test::call_service(&app, finalize()).await.status(), 409);

    // Provide a reason, then finalize succeeds.
    assert!(test::call_service(&app, count(Some("spoilage"))).await.status().is_success());
    assert!(test::call_service(&app, finalize()).await.status().is_success());

    // The reason is carried onto the stock_count movement.
    let reason: Option<String> = sqlx::query_scalar(
        "SELECT reason FROM inventory_movements WHERE source_type = 'stocktake' AND source_id = $1")
        .bind(stocktake_id).fetch_one(&pool).await.unwrap();
    assert_eq!(reason.as_deref(), Some("spoilage"));

    // Variance report flags the row and echoes the reason.
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/stocktakes/{stocktake_id}/variance-report"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .to_request()).await;
    let report: VarianceReport = test::read_body_json(resp).await;
    assert!(report.rows[0].is_flagged);
    assert_eq!(report.rows[0].variance_reason.as_deref(), Some("spoilage"));
}

// ──────────────────────────────────────────────────────────────
// Additional intensive coverage
// ──────────────────────────────────────────────────────────────

macro_rules! init_app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(get_secret()))
                .configure(routes::configure),
        ).await
    };
}

async fn seed_ing(pool: &PgPool, org_id: Uuid, name: &str, cost: Option<i64>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, category, cost_per_unit) VALUES ($1,$2,$3,'ml'::inventory_unit,'dairy',$4)")
        .bind(id).bind(org_id).bind(name).bind(cost).execute(pool).await.unwrap();
    id
}

async fn grant_all(pool: &PgPool) {
    for a in ["create", "read", "update"] { grant(pool, "stocktakes", a).await; }
}

macro_rules! start_stocktake {
    ($app:expr, $branch:expr, $token:expr) => {{
        let resp = test::call_service(&$app, test::TestRequest::post()
            .uri(&format!("/stocktakes/branches/{}", $branch))
            .insert_header(("Authorization", format!("Bearer {}", $token)))
            .set_json(serde_json::json!({})).to_request()).await;
        assert_eq!(resp.status(), 201);
        let full: StocktakeFull = test::read_body_json(resp).await;
        full
    }};
}

#[sqlx::test]
async fn test_list_and_get_stocktake(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    grant_all(&pool).await;
    let ing = seed_ingredient(&pool, org_id).await;
    seed_stock(&pool, branch_id, ing, 100.0).await;
    let token = org_admin_token(user_id, org_id);

    let full = start_stocktake!(app, branch_id, token);
    assert_eq!(full.variance_threshold_pct, 10.0);
    let id = full.stocktake.id;

    // List → 1.
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/stocktakes/branches/{branch_id}"))
        .insert_header(("Authorization", format!("Bearer {token}"))).to_request()).await;
    let list: Vec<Stocktake> = test::read_body_json(resp).await;
    assert_eq!(list.len(), 1);

    // Get → full with the snapshot item.
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/stocktakes/{id}"))
        .insert_header(("Authorization", format!("Bearer {token}"))).to_request()).await;
    let got: StocktakeFull = test::read_body_json(resp).await;
    assert_eq!(got.items.len(), 1);
    assert_eq!(got.items[0].expected_qty.to_string().parse::<f64>().unwrap(), 100.0);
}

#[sqlx::test]
async fn test_cancel_open_then_cancel_finalized_conflict(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    grant_all(&pool).await;
    let token = org_admin_token(user_id, org_id);

    // Cancel an open one → 200.
    let full = start_stocktake!(app, branch_id, token);
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/stocktakes/{}/cancel", full.stocktake.id))
        .insert_header(("Authorization", format!("Bearer {token}"))).to_request()).await;
    assert_eq!(resp.status(), 200);

    // New one, finalize (no counts → ok), then cancel → 409.
    let full2 = start_stocktake!(app, branch_id, token);
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/stocktakes/{}/finalize", full2.stocktake.id))
        .insert_header(("Authorization", format!("Bearer {token}"))).to_request()).await;
    assert!(resp.status().is_success());
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/stocktakes/{}/cancel", full2.stocktake.id))
        .insert_header(("Authorization", format!("Bearer {token}"))).to_request()).await;
    assert_eq!(resp.status(), 409);
}

#[sqlx::test]
async fn test_upsert_negative_and_invalid_reason_rejected(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    grant_all(&pool).await;
    let ing = seed_ingredient(&pool, org_id).await;
    seed_stock(&pool, branch_id, ing, 100.0).await;
    let token = org_admin_token(user_id, org_id);
    let full = start_stocktake!(app, branch_id, token);
    let id = full.stocktake.id;

    // Negative counted → 400.
    let resp = test::call_service(&app, test::TestRequest::put()
        .uri(&format!("/stocktakes/{id}/items")).insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(serde_json::json!({"items": [{"org_ingredient_id": ing, "counted_qty": -5.0}]})).to_request()).await;
    assert_eq!(resp.status(), 400);

    // Invalid variance reason → 400.
    let resp = test::call_service(&app, test::TestRequest::put()
        .uri(&format!("/stocktakes/{id}/items")).insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(serde_json::json!({"items": [{"org_ingredient_id": ing, "counted_qty": 90.0, "variance_reason": "bogus"}]})).to_request()).await;
    assert_eq!(resp.status(), 400);
}

#[sqlx::test]
async fn test_partial_count_leaves_uncounted_untouched(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    grant_all(&pool).await;
    let milk = seed_ingredient(&pool, org_id).await;          // 'Milk'
    let sugar = seed_ing(&pool, org_id, "Sugar", Some(50)).await;
    seed_stock(&pool, branch_id, milk, 100.0).await;
    seed_stock(&pool, branch_id, sugar, 50.0).await;
    let token = org_admin_token(user_id, org_id);
    let full = start_stocktake!(app, branch_id, token);
    let id = full.stocktake.id;

    // Count only Milk, 100 → 95 (5% < 10% threshold, not flagged).
    let resp = test::call_service(&app, test::TestRequest::put()
        .uri(&format!("/stocktakes/{id}/items")).insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(serde_json::json!({"items": [{"org_ingredient_id": milk, "counted_qty": 95.0}]})).to_request()).await;
    assert!(resp.status().is_success());

    // Finalize.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/stocktakes/{id}/finalize")).insert_header(("Authorization", format!("Bearer {token}"))).to_request()).await;
    assert!(resp.status().is_success());

    // Milk reconciled to 95; Sugar (uncounted) untouched at 50.
    let milk_stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE branch_id=$1 AND org_ingredient_id=$2").bind(branch_id).bind(milk).fetch_one(&pool).await.unwrap();
    let sugar_stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE branch_id=$1 AND org_ingredient_id=$2").bind(branch_id).bind(sugar).fetch_one(&pool).await.unwrap();
    assert_eq!(milk_stock, 95.0);
    assert_eq!(sugar_stock, 50.0);

    // Exactly one stock_count movement (Milk only).
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM inventory_movements WHERE source_type='stocktake' AND source_id=$1").bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test]
async fn test_variance_report_overage_and_unknown_cost(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    grant_all(&pool).await;
    let known = seed_ing(&pool, org_id, "Known", Some(300)).await;   // cost 300
    let unknown = seed_ing(&pool, org_id, "Unknown", None).await;    // NULL cost
    seed_stock(&pool, branch_id, known, 100.0).await;
    seed_stock(&pool, branch_id, unknown, 100.0).await;
    let token = org_admin_token(user_id, org_id);
    let full = start_stocktake!(app, branch_id, token);
    let id = full.stocktake.id;

    // Known overage +10 (110); Unknown shrinkage -20 (80, but cost unknown).
    let resp = test::call_service(&app, test::TestRequest::put()
        .uri(&format!("/stocktakes/{id}/items")).insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(serde_json::json!({"items": [
            {"org_ingredient_id": known, "counted_qty": 110.0},
            {"org_ingredient_id": unknown, "counted_qty": 80.0}
        ]})).to_request()).await;
    assert!(resp.status().is_success());

    // Variance report (no finalize needed).
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/stocktakes/{id}/variance-report")).insert_header(("Authorization", format!("Bearer {token}"))).to_request()).await;
    let report: VarianceReport = test::read_body_json(resp).await;
    assert_eq!(report.total_overage_value, 3000);     // +10 × 300
    assert_eq!(report.total_shrinkage_value, 0);       // unknown cost excluded
    assert_eq!(report.unknown_cost_count, 1);
    let unknown_row = report.rows.iter().find(|r| r.org_ingredient_id == unknown).unwrap();
    assert!(unknown_row.variance_value.is_none());
}

#[sqlx::test]
async fn test_finalize_already_finalized_conflict(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    grant_all(&pool).await;
    let token = org_admin_token(user_id, org_id);
    let full = start_stocktake!(app, branch_id, token);
    let id = full.stocktake.id;

    let fin = || test::TestRequest::post()
        .uri(&format!("/stocktakes/{id}/finalize")).insert_header(("Authorization", format!("Bearer {token}"))).to_request();
    assert!(test::call_service(&app, fin()).await.status().is_success());
    assert_eq!(test::call_service(&app, fin()).await.status(), 409);
}

async fn deny_user(pool: &PgPool, user_id: Uuid, resource: &str, action: &str) {
    sqlx::query("INSERT INTO permissions (user_id, resource, action, granted) VALUES ($1, $2::permission_resource, $3::permission_action, false)")
        .bind(user_id).bind(resource).bind(action).execute(pool).await.unwrap();
}

#[sqlx::test]
async fn test_permission_denied_and_branch_isolation(pool: PgPool) {
    let app = init_app!(pool);
    let org_a = seed_org(&pool).await;
    let org_b = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_a).await;
    let branch_b = seed_branch(&pool, org_b).await;

    // (1) Permission denied: a per-user deny override beats the seeded default.
    let denied_user = seed_user(&pool, org_a).await;
    deny_user(&pool, denied_user, "stocktakes", "create").await;
    let denied_token = org_admin_token(denied_user, org_a);
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/stocktakes/branches/{branch_a}"))
        .insert_header(("Authorization", format!("Bearer {denied_token}")))
        .set_json(serde_json::json!({})).to_request()).await;
    assert_eq!(resp.status(), 403);

    // (2) Branch isolation: an org-A admin (with permission) cannot start a
    // count on an org-B branch.
    let user_a = seed_user(&pool, org_a).await;
    let token = org_admin_token(user_a, org_a);
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/stocktakes/branches/{branch_b}"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(serde_json::json!({})).to_request()).await;
    assert_eq!(resp.status(), 403);
}

#[sqlx::test]
async fn test_threshold_is_configurable(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    grant_all(&pool).await;
    // Tighten tolerance to 5%.
    sqlx::query("UPDATE organizations SET stocktake_variance_threshold_pct = 5 WHERE id=$1").bind(org_id).execute(&pool).await.unwrap();
    let ing = seed_ingredient(&pool, org_id).await;
    seed_stock(&pool, branch_id, ing, 100.0).await;
    let token = org_admin_token(user_id, org_id);

    let full = start_stocktake!(app, branch_id, token);
    assert_eq!(full.variance_threshold_pct, 5.0);
    let id = full.stocktake.id;

    // 100 → 92 = 8% > 5% → flagged. No reason → finalize blocked (409).
    test::call_service(&app, test::TestRequest::put()
        .uri(&format!("/stocktakes/{id}/items")).insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(serde_json::json!({"items": [{"org_ingredient_id": ing, "counted_qty": 92.0}]})).to_request()).await;
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/stocktakes/{id}/finalize")).insert_header(("Authorization", format!("Bearer {token}"))).to_request()).await;
    assert_eq!(resp.status(), 409);
}
