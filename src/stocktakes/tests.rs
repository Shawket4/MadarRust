#![allow(unused_imports, unused_variables, dead_code)]
use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::stocktakes::handlers::{Stocktake, StocktakeFull, VarianceReport};
use crate::stocktakes::routes;

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn org_admin_token(user_id: Uuid, org_id: Uuid) -> String {
    crate::auth::jwt::create_token(
        &get_secret(),
        user_id,
        Some(org_id),
        UserRole::OrgAdmin,
        None,
        24,
    )
    .unwrap()
}

async fn seed_org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(id)
        .bind(format!("org-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn seed_branch(pool: &PgPool, org_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(org_id)
        .bind(format!("Branch {id}"))
        .execute(pool)
        .await
        .unwrap();
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
async fn grant_all(pool: &PgPool) {
    for a in ["create", "read", "update"] {
        grant(pool, "stocktakes", a).await;
    }
}
/// Get-or-create a category by slug (the DB function the app uses too).
async fn category(pool: &PgPool, org_id: Uuid, slug: &str) -> Uuid {
    sqlx::query_scalar("SELECT ingredient_category_id($1, $2)")
        .bind(org_id)
        .bind(slug)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn seed_ing(pool: &PgPool, org_id: Uuid, name: &str, cat: &str, cost: Option<i64>) -> Uuid {
    let id = Uuid::new_v4();
    let cat_id = category(pool, org_id, cat).await;
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, category_id, cost_per_unit) VALUES ($1,$2,$3,'ml'::inventory_unit,$4,$5)")
        .bind(id).bind(org_id).bind(name).bind(cat_id).bind(cost).execute(pool).await.unwrap();
    id
}
async fn seed_ingredient(pool: &PgPool, org_id: Uuid) -> Uuid {
    seed_ing(pool, org_id, "Milk", "dairy", Some(300)).await
}
/// Opening balance the way real stock arrives: through the ledger.
async fn seed_stock(pool: &PgPool, branch_id: Uuid, ing_id: Uuid, qty: f64) {
    sqlx::query("INSERT INTO inventory_movements (branch_id, org_ingredient_id, type, quantity, source_type) VALUES ($1, $2, 'purchase_in', $3, 'seed')")
        .bind(branch_id).bind(ing_id).bind(qty).execute(pool).await.unwrap();
}
async fn on_hand(pool: &PgPool, branch_id: Uuid, ing_id: Uuid) -> Option<f64> {
    sqlx::query_scalar(
        "SELECT on_hand::float8 FROM branch_stock WHERE branch_id = $1 AND org_ingredient_id = $2",
    )
    .bind(branch_id)
    .bind(ing_id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

macro_rules! init_app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(get_secret()))
                .configure(routes::configure),
        )
        .await
    };
}

macro_rules! start_stocktake {
    ($app:expr, $branch:expr, $token:expr) => {{
        let resp = test::call_service(
            &$app,
            test::TestRequest::post()
                .uri(&format!("/stocktakes/branches/{}", $branch))
                .insert_header(("Authorization", format!("Bearer {}", $token)))
                .set_json(serde_json::json!({}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 201);
        let full: StocktakeFull = test::read_body_json(resp).await;
        full
    }};
}

macro_rules! count {
    ($app:expr, $id:expr, $token:expr, $items:tt) => {{
        let resp = test::call_service(
            &$app,
            test::TestRequest::put()
                .uri(&format!("/stocktakes/{}/items", $id))
                .insert_header(("Authorization", format!("Bearer {}", $token)))
                .set_json(serde_json::json!({ "items": $items }))
                .to_request(),
        )
        .await;
        resp
    }};
}

macro_rules! finalize {
    ($app:expr, $id:expr, $token:expr) => {{
        test::call_service(
            &$app,
            test::TestRequest::post()
                .uri(&format!("/stocktakes/{}/finalize", $id))
                .insert_header(("Authorization", format!("Bearer {}", $token)))
                .to_request(),
        )
        .await
    }};
}

macro_rules! variance {
    ($app:expr, $id:expr, $token:expr) => {{
        let resp = test::call_service(
            &$app,
            test::TestRequest::get()
                .uri(&format!("/stocktakes/{}/variance-report", $id))
                .insert_header(("Authorization", format!("Bearer {}", $token)))
                .to_request(),
        )
        .await;
        assert!(resp.status().is_success());
        let report: VarianceReport = test::read_body_json(resp).await;
        report
    }};
}

#[sqlx::test]
async fn test_stocktake_reconciles_stock_and_posts_variance(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    grant_all(&pool).await;
    let ing = seed_ingredient(&pool, org_id).await;
    seed_stock(&pool, branch_id, ing, 100.0).await;
    let token = org_admin_token(user_id, org_id);

    let full = start_stocktake!(app, branch_id, token);
    assert_eq!(full.items.len(), 1);
    assert_eq!(full.items[0].opening_qty, 100.0);
    assert_eq!(full.items[0].book_qty, 100.0);
    assert!(!full.items[0].is_new);
    let stocktake_id = full.stocktake.id;

    // Count 92 (shrinkage of 8).
    assert!(
        count!(app, stocktake_id, token, [{"org_ingredient_id": ing, "counted_qty": 92.0}])
            .status()
            .is_success()
    );
    let resp = finalize!(app, stocktake_id, token);
    assert!(resp.status().is_success());
    let finalized: StocktakeFull = test::read_body_json(resp).await;
    assert_eq!(finalized.stocktake.status, "finalized");
    assert_eq!(
        finalized.items[0].book_qty, 100.0,
        "baseline frozen at finalize"
    );
    assert_eq!(finalized.items[0].variance, Some(-8.0));

    assert_eq!(on_hand(&pool, branch_id, ing).await, Some(92.0));

    let (mtype, mqty, bal): (String, f64, f64) = sqlx::query_as("SELECT type::text, quantity::float8, balance_after::float8 FROM inventory_movements WHERE source_type = 'stocktake' AND source_id = $1")
        .bind(stocktake_id).fetch_one(&pool).await.unwrap();
    assert_eq!(mtype, "stock_count");
    assert_eq!(mqty, -8.0);
    assert_eq!(bal, 92.0, "trigger stamps the resulting balance");

    let last_counted: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT last_counted_at FROM branch_stock WHERE branch_id=$1 AND org_ingredient_id=$2",
    )
    .bind(branch_id)
    .bind(ing)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(last_counted.is_some());

    let report = variance!(app, stocktake_id, token);
    assert_eq!(report.total_shrinkage_value, 2400); // 8 × 300
    assert_eq!(report.net_variance_value, -2400);
    assert_eq!(report.rows[0].book_qty, 100.0);
}

/// The headline of inventory v2: a branch that has never tracked anything can
/// be counted from the catalog alone. Every ingredient appears (0 on hand,
/// `is_new`), counting one creates its balance row, and the uncounted rest
/// stays untracked.
#[sqlx::test]
async fn test_count_on_branch_with_no_stock_rows_lists_whole_catalog(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    grant_all(&pool).await;
    let milk = seed_ing(&pool, org_id, "Milk", "dairy", Some(300)).await;
    let flour = seed_ing(&pool, org_id, "Flour", "dry", Some(5)).await;
    let sugar = seed_ing(&pool, org_id, "Sugar", "dry", None).await;
    let token = org_admin_token(user_id, org_id);

    let full = start_stocktake!(app, branch_id, token);
    assert_eq!(full.items.len(), 3, "the whole catalog is in a full count");
    assert!(
        full.items
            .iter()
            .all(|i| i.is_new && i.opening_qty == 0.0 && i.book_qty == 0.0)
    );
    let id = full.stocktake.id;

    // Count milk only: appears-from-zero is flagged, so a reason is required.
    assert!(
        count!(app, id, token, [{"org_ingredient_id": milk, "counted_qty": 12.0}])
            .status()
            .is_success()
    );
    assert_eq!(finalize!(app, id, token).status(), 409);
    assert!(count!(app, id, token, [{"org_ingredient_id": milk, "counted_qty": 12.0, "variance_reason": "miscount"}]).status().is_success());
    assert!(finalize!(app, id, token).status().is_success());

    assert_eq!(on_hand(&pool, branch_id, milk).await, Some(12.0));
    assert_eq!(
        on_hand(&pool, branch_id, flour).await,
        None,
        "uncounted stays untracked"
    );
    assert_eq!(on_hand(&pool, branch_id, sugar).await, None);

    // The count row was created by the ledger, not by a direct write.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM inventory_movements WHERE branch_id=$1 AND org_ingredient_id=$2 AND type='stock_count'")
        .bind(branch_id).bind(milk).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1);
}

/// Counting an item as zero on a branch that never tracked it still records
/// the fact (a balance row with last_counted_at) without posting a movement.
#[sqlx::test]
async fn test_counting_zero_on_new_item_records_count_without_movement(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    grant_all(&pool).await;
    let ing = seed_ingredient(&pool, org_id).await;
    let token = org_admin_token(user_id, org_id);

    let full = start_stocktake!(app, branch_id, token);
    let id = full.stocktake.id;
    assert!(
        count!(app, id, token, [{"org_ingredient_id": ing, "counted_qty": 0.0}])
            .status()
            .is_success()
    );
    assert!(finalize!(app, id, token).status().is_success());

    let (oh, counted): (f64, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as("SELECT on_hand::float8, last_counted_at FROM branch_stock WHERE branch_id=$1 AND org_ingredient_id=$2")
        .bind(branch_id).bind(ing).fetch_one(&pool).await.unwrap();
    assert_eq!(oh, 0.0);
    assert!(counted.is_some());
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM inventory_movements WHERE branch_id=$1 AND org_ingredient_id=$2",
    )
    .bind(branch_id)
    .bind(ing)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 0);
}

/// A count reconciles to BOOK stock, so a legitimate sale during the count
/// window is not mislabeled as shrinkage — and the open count shows the live
/// book figure while it is in progress.
#[sqlx::test]
async fn test_finalize_reconciles_to_live_not_snapshot(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    grant_all(&pool).await;
    let ing = seed_ingredient(&pool, org_id).await;
    seed_stock(&pool, branch_id, ing, 100.0).await;
    let token = org_admin_token(user_id, org_id);

    let full = start_stocktake!(app, branch_id, token);
    let stocktake_id = full.stocktake.id;

    // While the count is open, 8 units are legitimately sold → book = 92.
    sqlx::query("INSERT INTO inventory_movements (branch_id, org_ingredient_id, type, quantity, source_type) VALUES ($1, $2, 'sale', -8, 'order')")
        .bind(branch_id).bind(ing).execute(&pool).await.unwrap();

    // The open count reflects live book stock (opening stays 100 for reference).
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/stocktakes/{stocktake_id}"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    let live: StocktakeFull = test::read_body_json(resp).await;
    assert_eq!(live.items[0].opening_qty, 100.0);
    assert_eq!(live.items[0].book_qty, 92.0);

    // Physical count finds 90 (2 genuinely missing on top of the 8 sold).
    assert!(
        count!(app, stocktake_id, token, [{"org_ingredient_id": ing, "counted_qty": 90.0}])
            .status()
            .is_success()
    );
    assert!(finalize!(app, stocktake_id, token).status().is_success());

    assert_eq!(on_hand(&pool, branch_id, ing).await, Some(90.0));
    let mqty: f64 = sqlx::query_scalar("SELECT quantity::float8 FROM inventory_movements WHERE source_type = 'stocktake' AND source_id = $1")
        .bind(stocktake_id).fetch_one(&pool).await.unwrap();
    assert_eq!(mqty, -2.0);

    let report = variance!(app, stocktake_id, token);
    assert_eq!(report.total_shrinkage_value, 600);
    assert_eq!(report.rows[0].opening_qty, 100.0);
    assert_eq!(report.rows[0].book_qty, 92.0);
    assert_eq!(report.rows[0].variance, Some(-2.0));
}

/// A balance row that appears mid-count (first purchase received while the
/// count is open) is picked up at finalize: the difference is against the
/// live book, never against the stale open-time snapshot.
#[sqlx::test]
async fn test_row_appearing_mid_count_is_reconciled_against_live(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    grant_all(&pool).await;
    let ing = seed_ingredient(&pool, org_id).await;
    let token = org_admin_token(user_id, org_id);

    let full = start_stocktake!(app, branch_id, token);
    assert!(full.items[0].is_new);
    let id = full.stocktake.id;

    // A delivery lands during the count → book 50.
    seed_stock(&pool, branch_id, ing, 50.0).await;
    // Shelf shows 48.
    assert!(
        count!(app, id, token, [{"org_ingredient_id": ing, "counted_qty": 48.0}])
            .status()
            .is_success()
    );
    assert!(finalize!(app, id, token).status().is_success());

    assert_eq!(on_hand(&pool, branch_id, ing).await, Some(48.0));
    let mqty: f64 = sqlx::query_scalar("SELECT quantity::float8 FROM inventory_movements WHERE source_type='stocktake' AND source_id=$1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(mqty, -2.0, "delta is vs the 50 that arrived, not vs 0");
}

/// Ledger is truth: nothing may write on_hand except the movement trigger.
#[sqlx::test]
async fn test_direct_on_hand_write_is_rejected(pool: PgPool) {
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let ing = seed_ingredient(&pool, org_id).await;
    seed_stock(&pool, branch_id, ing, 10.0).await;

    let err = sqlx::query(
        "UPDATE branch_stock SET on_hand = 999 WHERE branch_id=$1 AND org_ingredient_id=$2",
    )
    .bind(branch_id)
    .bind(ing)
    .execute(&pool)
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("inventory_movements"),
        "guard message: {err}"
    );

    // Par levels are still editable directly (only on_hand is guarded).
    sqlx::query("UPDATE branch_stock SET par_min = 5 WHERE branch_id=$1 AND org_ingredient_id=$2")
        .bind(branch_id)
        .bind(ing)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(on_hand(&pool, branch_id, ing).await, Some(10.0));
}

#[sqlx::test]
async fn test_only_one_open_stocktake_per_branch(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    grant(&pool, "stocktakes", "create").await;
    let token = org_admin_token(user_id, org_id);

    let mk = || {
        test::TestRequest::post()
            .uri(&format!("/stocktakes/branches/{branch_id}"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(serde_json::json!({}))
            .to_request()
    };
    assert_eq!(test::call_service(&app, mk()).await.status(), 201);
    assert_eq!(test::call_service(&app, mk()).await.status(), 409);
}

#[sqlx::test]
async fn test_finalize_requires_reason_for_large_variance(pool: PgPool) {
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

    // 100 → 80 = 20% > 10% → flagged; no reason → finalize blocked.
    assert!(
        count!(app, id, token, [{"org_ingredient_id": ing, "counted_qty": 80.0}])
            .status()
            .is_success()
    );
    assert_eq!(finalize!(app, id, token).status(), 409);
    assert!(count!(app, id, token, [{"org_ingredient_id": ing, "counted_qty": 80.0, "variance_reason": "spoilage"}]).status().is_success());
    assert!(finalize!(app, id, token).status().is_success());

    let reason: Option<String> = sqlx::query_scalar(
        "SELECT reason FROM inventory_movements WHERE source_type = 'stocktake' AND source_id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reason.as_deref(), Some("spoilage"));

    let report = variance!(app, id, token);
    assert!(report.rows[0].is_flagged);
    assert_eq!(report.rows[0].variance_reason.as_deref(), Some("spoilage"));
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
    assert_eq!(full.stocktake.scope["kind"], "full");
    let id = full.stocktake.id;
    assert!(
        count!(app, id, token, [{"org_ingredient_id": ing, "counted_qty": 100.0}])
            .status()
            .is_success()
    );

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/stocktakes/branches/{branch_id}"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    let list: Vec<Stocktake> = test::read_body_json(resp).await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].counted_items, Some(1));
    assert_eq!(list[0].total_items, Some(1));

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/stocktakes/{id}"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    let got: StocktakeFull = test::read_body_json(resp).await;
    assert_eq!(got.items.len(), 1);
    assert_eq!(got.items[0].opening_qty, 100.0);
    assert_eq!(got.items[0].category_name, "Dairy");
}

#[sqlx::test]
async fn test_cancel_open_then_cancel_finalized_conflict(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    grant_all(&pool).await;
    let token = org_admin_token(user_id, org_id);

    let full = start_stocktake!(app, branch_id, token);
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/stocktakes/{}/cancel", full.stocktake.id))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let cancelled: Stocktake = test::read_body_json(resp).await;
    assert_eq!(cancelled.status, "cancelled");

    let full2 = start_stocktake!(app, branch_id, token);
    assert!(
        finalize!(app, full2.stocktake.id, token)
            .status()
            .is_success()
    );
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/stocktakes/{}/cancel", full2.stocktake.id))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .to_request(),
    )
    .await;
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
    let token = org_admin_token(user_id, org_id);
    let id = start_stocktake!(app, branch_id, token).stocktake.id;

    assert_eq!(
        count!(app, id, token, [{"org_ingredient_id": ing, "counted_qty": -5.0}]).status(),
        400
    );
    assert_eq!(count!(app, id, token, [{"org_ingredient_id": ing, "counted_qty": 90.0, "variance_reason": "bogus"}]).status(), 400);
    // An ingredient from another org is rejected too.
    let other = seed_org(&pool).await;
    let foreign = seed_ing(&pool, other, "Foreign", "general", None).await;
    assert_eq!(
        count!(app, id, token, [{"org_ingredient_id": foreign, "counted_qty": 1.0}]).status(),
        400
    );
}

#[sqlx::test]
async fn test_partial_count_leaves_uncounted_untouched(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    grant_all(&pool).await;
    let milk = seed_ingredient(&pool, org_id).await;
    let sugar = seed_ing(&pool, org_id, "Sugar", "dry", Some(50)).await;
    seed_stock(&pool, branch_id, milk, 100.0).await;
    seed_stock(&pool, branch_id, sugar, 50.0).await;
    let token = org_admin_token(user_id, org_id);
    let id = start_stocktake!(app, branch_id, token).stocktake.id;

    assert!(
        count!(app, id, token, [{"org_ingredient_id": milk, "counted_qty": 95.0}])
            .status()
            .is_success()
    );
    assert!(finalize!(app, id, token).status().is_success());

    assert_eq!(on_hand(&pool, branch_id, milk).await, Some(95.0));
    assert_eq!(on_hand(&pool, branch_id, sugar).await, Some(50.0));
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM inventory_movements WHERE source_type='stocktake' AND source_id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test]
async fn test_variance_report_overage_and_unknown_cost(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    grant_all(&pool).await;
    let known = seed_ing(&pool, org_id, "Known", "dairy", Some(300)).await;
    let unknown = seed_ing(&pool, org_id, "Unknown", "dairy", None).await;
    seed_stock(&pool, branch_id, known, 100.0).await;
    seed_stock(&pool, branch_id, unknown, 100.0).await;
    let token = org_admin_token(user_id, org_id);
    let id = start_stocktake!(app, branch_id, token).stocktake.id;

    assert!(
        count!(app, id, token, [
            {"org_ingredient_id": known, "counted_qty": 110.0},
            {"org_ingredient_id": unknown, "counted_qty": 80.0}
        ])
        .status()
        .is_success()
    );

    let report = variance!(app, id, token);
    assert_eq!(report.total_overage_value, 3000);
    assert_eq!(report.total_shrinkage_value, 0);
    assert_eq!(report.unknown_cost_count, 1);
    let unknown_row = report
        .rows
        .iter()
        .find(|r| r.org_ingredient_id == unknown)
        .unwrap();
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
    let id = start_stocktake!(app, branch_id, token).stocktake.id;
    assert!(finalize!(app, id, token).status().is_success());
    assert_eq!(finalize!(app, id, token).status(), 409);
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

    let denied_user = seed_user(&pool, org_a).await;
    deny_user(&pool, denied_user, "stocktakes", "create").await;
    let denied_token = org_admin_token(denied_user, org_a);
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/stocktakes/branches/{branch_a}"))
            .insert_header(("Authorization", format!("Bearer {denied_token}")))
            .set_json(serde_json::json!({}))
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status().as_u16(),
        403,
        "missing permission must be forbidden"
    );

    let user_a = seed_user(&pool, org_a).await;
    let token = org_admin_token(user_a, org_a);
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/stocktakes/branches/{branch_b}"))
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(serde_json::json!({}))
            .to_request(),
    )
    .await;
    assert!(
        matches!(resp.status().as_u16(), 403 | 404),
        "cross-org/branch must be denied, got {}",
        resp.status()
    );
}

#[sqlx::test]
async fn test_threshold_is_configurable(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    grant_all(&pool).await;
    sqlx::query("UPDATE organizations SET stocktake_variance_threshold_pct = 5 WHERE id=$1")
        .bind(org_id)
        .execute(&pool)
        .await
        .unwrap();
    let ing = seed_ingredient(&pool, org_id).await;
    seed_stock(&pool, branch_id, ing, 100.0).await;
    let token = org_admin_token(user_id, org_id);

    let full = start_stocktake!(app, branch_id, token);
    assert_eq!(full.variance_threshold_pct, 5.0);
    let id = full.stocktake.id;
    assert!(
        count!(app, id, token, [{"org_ingredient_id": ing, "counted_qty": 92.0}])
            .status()
            .is_success()
    );
    assert_eq!(finalize!(app, id, token).status(), 409);
}

#[sqlx::test]
async fn test_list_stocktakes_all_branches(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = seed_branch(&pool, org_id).await;
    let admin = seed_user(&pool, org_id).await;
    grant(&pool, "stocktakes", "read").await;
    let token = org_admin_token(admin, org_id);

    for branch in [branch_a, branch_b] {
        sqlx::query("INSERT INTO stocktakes (id, org_id, branch_id, status, started_by, finalized_by, finalized_at) VALUES ($1,$2,$3,'finalized',$4,$4,NOW())")
            .bind(Uuid::new_v4()).bind(org_id).bind(branch).bind(admin).execute(&pool).await.unwrap();
    }
    let other_org = seed_org(&pool).await;
    let other_branch = seed_branch(&pool, other_org).await;
    let other_admin = seed_user(&pool, other_org).await;
    sqlx::query("INSERT INTO stocktakes (id, org_id, branch_id, status, started_by) VALUES ($1,$2,$3,'in_progress',$4)")
        .bind(Uuid::new_v4()).bind(other_org).bind(other_branch).bind(other_admin).execute(&pool).await.unwrap();

    let auth = ("Authorization", format!("Bearer {token}"));
    let nil = Uuid::nil();
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/stocktakes/branches/{nil}"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let rows: Vec<Stocktake> = test::read_body_json(resp).await;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|s| s.branch_name.is_some()));
    let seen: std::collections::HashSet<_> = rows.iter().map(|s| s.branch_id).collect();
    assert!(seen.contains(&branch_a) && seen.contains(&branch_b));

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/stocktakes/branches/{branch_a}"))
            .insert_header(auth.clone())
            .to_request(),
    )
    .await;
    let just_a: Vec<Stocktake> = test::read_body_json(resp).await;
    assert_eq!(just_a.len(), 1);
    assert_eq!(just_a[0].branch_id, branch_a);
}

/// Cycle-count scope (by category) limits the snapshot; a found item outside
/// the scope can still be counted in (added with its live book baseline).
#[sqlx::test]
async fn test_cycle_count_scope_and_found_item(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    grant_all(&pool).await;
    let token = org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    let dairy = seed_ing(&pool, org_id, "Milk", "dairy", Some(300)).await;
    let dry = seed_ing(&pool, org_id, "Flour", "dry", Some(5)).await;
    seed_stock(&pool, branch_id, dairy, 100.0).await;
    seed_stock(&pool, branch_id, dry, 5000.0).await;
    let dairy_cat = category(&pool, org_id, "dairy").await;

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/stocktakes/branches/{branch_id}"))
            .insert_header(auth.clone())
            .set_json(serde_json::json!({"category_id": dairy_cat}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let full: StocktakeFull = test::read_body_json(resp).await;
    assert_eq!(full.items.len(), 1, "only the dairy ingredient is in scope");
    assert_eq!(full.items[0].org_ingredient_id, dairy);
    assert_eq!(full.stocktake.scope["kind"], "category");
    let st_id = full.stocktake.id;

    let resp = count!(app, st_id, token, [{"org_ingredient_id": dry, "counted_qty": 4900.0}]);
    assert!(resp.status().is_success());
    let updated: StocktakeFull = test::read_body_json(resp).await;
    assert_eq!(updated.items.len(), 2, "found item added to the count");
    let flour = updated
        .items
        .iter()
        .find(|i| i.org_ingredient_id == dry)
        .unwrap();
    assert_eq!(flour.opening_qty, 5000.0);
    assert_eq!(flour.counted_qty, Some(4900.0));
    assert_eq!(flour.variance, Some(-100.0));

    // Explicit item scope works too, and a foreign category is rejected.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/stocktakes/branches/{branch_id}"))
            .insert_header(auth.clone())
            .set_json(serde_json::json!({"category_id": Uuid::new_v4()}))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400);
}
