//! Archiving inventory ingredients (2026-10-18).
//!
//! An archive hides an ingredient from pickers, reorder suggestions and (once
//! no branch holds stock) count sheets. It must change nothing else: recipes
//! keep deducting and costing it, every till keeps it on its mirror, history
//! keeps showing it, no movement is posted and the name stays reserved.
#![allow(unused_imports, dead_code)]
mod common;

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::JwtSecret;
use madar_rust::inventory::handlers::{
    BranchStockRow, IngredientUsage, OrgIngredient, StockMovement,
};
use madar_rust::models::UserRole;
use madar_rust::orders::handlers::{CreateOrderRequest, OrderFull, OrderItemInput};

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn token(user: Uuid, org: Uuid, role: UserRole, branch: Option<Uuid>) -> String {
    madar_rust::auth::jwt::create_token(&secret(), user, Some(org), role, branch, 24).unwrap()
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .configure(madar_rust::inventory::routes::configure)
                .configure(madar_rust::orders::routes::configure)
                .configure(madar_rust::stocktakes::routes::configure)
                .configure(|cfg| {
                    madar_rust::reports::routes::configure(cfg, web::Data::new($pool.clone()))
                })
                .configure(madar_rust::purchasing::routes::configure),
        )
        .await
    };
}

macro_rules! call {
    ($app:expr, $req:expr, $tok:expr) => {
        test::call_service(
            &$app,
            $req.insert_header(("Authorization", format!("Bearer {}", $tok)))
                .to_request(),
        )
        .await
    };
}

struct Shop {
    org: Uuid,
    branch: Uuid,
    admin: Uuid,
    token: String,
}

async fn shop(pool: &PgPool) -> Shop {
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Archive Org', $2)")
        .bind(org)
        .bind(format!("archive-{org}"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) \
         VALUES ($1, 'cash', '{}', 'emerald', 'payments_outlined', true, true)",
    )
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    let branch = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Main')")
        .bind(branch)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    let admin = user(pool, org, "org_admin").await;
    for (res, act) in [
        ("inventory", "read"),
        ("inventory", "create"),
        ("inventory", "update"),
        ("inventory", "delete"),
        ("stocktakes", "create"),
        ("stocktakes", "read"),
        ("purchase_orders", "read"),
        ("orders", "create"),
        ("orders", "read"),
    ] {
        grant(pool, "org_admin", res, act).await;
    }
    let token = token(admin, org, UserRole::OrgAdmin, None);
    Shop {
        org,
        branch,
        admin,
        token,
    }
}

async fn user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, 'U', $3, 'h', $4::user_role)",
    )
    .bind(id)
    .bind(org)
    .bind(format!("u-{id}@t.com"))
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn grant(pool: &PgPool, role: &str, res: &str, act: &str) {
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ($1::user_role, $2::permission_resource, $3::permission_action, true) ON CONFLICT DO NOTHING",
    )
    .bind(role)
    .bind(res)
    .bind(act)
    .execute(pool)
    .await
    .unwrap();
}

async fn ingredient(pool: &PgPool, org: Uuid, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO org_ingredients (id, org_id, name, unit, cost_per_unit, category_id) \
         VALUES ($1, $2, $3, 'g'::inventory_unit, 100, ingredient_category_id($2, 'general'))",
    )
    .bind(id)
    .bind(org)
    .bind(name)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Stock arrives the only way it can: a ledger movement.
async fn stock_in(pool: &PgPool, branch: Uuid, ing: Uuid, qty: f64) {
    sqlx::query(
        "INSERT INTO inventory_movements (branch_id, org_ingredient_id, type, quantity, source_type) \
         VALUES ($1, $2, 'purchase_in', $3, 'seed')",
    )
    .bind(branch)
    .bind(ing)
    .bind(qty)
    .execute(pool)
    .await
    .unwrap();
}

async fn on_hand(pool: &PgPool, branch: Uuid, ing: Uuid) -> f64 {
    sqlx::query_scalar(
        "SELECT COALESCE((SELECT on_hand::float8 FROM branch_stock WHERE branch_id = $1 AND org_ingredient_id = $2), 0)",
    )
    .bind(branch)
    .bind(ing)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn movement_count(pool: &PgPool, ing: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM inventory_movements WHERE org_ingredient_id = $1")
        .bind(ing)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn menu_item_with_recipe(pool: &PgPool, org: Uuid, ing: Uuid, grams: f64) -> Uuid {
    let cat = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1, $2, 'Drinks')")
        .bind(cat)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    let item = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) \
         VALUES ($1, $2, $3, 'Espresso', 500, true)",
    )
    .bind(item)
    .bind(org)
    .bind(cat)
    .execute(pool)
    .await
    .unwrap();
    common::legacy_catalog::item_recipe(pool, item, "one_size", ing, grams, "g").await;
    item
}

fn catalog_url(org: Uuid, id: Uuid, tail: &str) -> String {
    format!("/inventory/orgs/{org}/catalog/{id}/{tail}")
}

macro_rules! catalog {
    ($app:expr, $s:expr, $status:expr) => {{
        let status: Option<&str> = $status;
        let q = status.map(|v| format!("?status={v}")).unwrap_or_default();
        let resp = call!(
            $app,
            test::TestRequest::get().uri(&format!("/inventory/orgs/{}/catalog{q}", $s.org)),
            $s.token
        );
        assert!(resp.status().is_success(), "{:?}", resp.status());
        let rows: Vec<OrgIngredient> = test::read_body_json(resp).await;
        rows
    }};
}

#[sqlx::test]
async fn archive_and_unarchive_round_trip(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let ing = ingredient(&pool, s.org, "Old Syrup").await;
    let keep = ingredient(&pool, s.org, "Sugar").await;

    // Unused and unstocked: archives without a confirmation, and with no body.
    let resp = call!(app, test::TestRequest::post().uri(&catalog_url(s.org, ing, "archive")), s.token);
    assert_eq!(resp.status(), 200);
    let row: OrgIngredient = test::read_body_json(resp).await;
    let first = row.archived_at.expect("archived");
    assert_eq!(row.archived_by, Some(s.admin));
    assert!(row.is_active, "is_active is not the archive flag (the POS feed reads it)");

    // Idempotent: a second archive keeps the first time.
    let resp = call!(
        app,
        test::TestRequest::post().uri(&catalog_url(s.org, ing, "archive")).set_json(json!({"confirm": true})),
        s.token
    );
    let again: OrgIngredient = test::read_body_json(resp).await;
    assert_eq!(again.archived_at, Some(first));

    // Lists: omitted status = all (what old clients read), active, archived.
    let all = catalog!(app, s, None);
    assert_eq!(all.len(), 2);
    assert!(all.iter().find(|r| r.id == ing).unwrap().archived_at.is_some());
    let active = catalog!(app, s, Some("active"));
    assert_eq!(active.iter().map(|r| r.id).collect::<Vec<_>>(), vec![keep]);
    let archived = catalog!(app, s, Some("archived"));
    assert_eq!(archived.iter().map(|r| r.id).collect::<Vec<_>>(), vec![ing]);
    assert_eq!(catalog!(app, s, Some("all")).len(), 2);

    // Unarchive brings it back (also idempotent).
    for _ in 0..2 {
        let resp = call!(app, test::TestRequest::post().uri(&catalog_url(s.org, ing, "unarchive")), s.token);
        assert_eq!(resp.status(), 200);
        let row: OrgIngredient = test::read_body_json(resp).await;
        assert!(row.archived_at.is_none());
        assert!(row.archived_by.is_none());
    }
    assert_eq!(catalog!(app, s, Some("active")).len(), 2);

    // An unknown status is a 400, an unknown id a 404.
    let resp = call!(
        app,
        test::TestRequest::get().uri(&format!("/inventory/orgs/{}/catalog?status=gone", s.org)),
        s.token
    );
    assert_eq!(resp.status(), 400);
    let resp = call!(
        app,
        test::TestRequest::post().uri(&catalog_url(s.org, Uuid::new_v4(), "archive")),
        s.token
    );
    assert_eq!(resp.status(), 404);
}

#[sqlx::test]
async fn archive_of_a_used_or_stocked_ingredient_needs_confirmation_and_moves_nothing(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let beans = ingredient(&pool, s.org, "Beans").await;
    let item = menu_item_with_recipe(&pool, s.org, beans, 18.0).await;
    stock_in(&pool, s.branch, beans, 1000.0).await;

    let resp = call!(app, test::TestRequest::get().uri(&catalog_url(s.org, beans, "usage")), s.token);
    assert_eq!(resp.status(), 200);
    let usage: IngredientUsage = test::read_body_json(resp).await;
    assert!(usage.needs_confirmation);
    assert!(!usage.never_used);
    assert_eq!(usage.usages.len(), 1);
    assert_eq!(usage.usages[0].kind, "recipe");
    assert_eq!(usage.usages[0].id, item);
    assert_eq!(usage.stock.len(), 1);
    assert_eq!(usage.stock[0].branch_id, s.branch);
    assert!((usage.stock[0].on_hand - 1000.0).abs() < 1e-9);

    let movements_before = movement_count(&pool, beans).await;

    // Without confirmation: refused with a code, nothing archived.
    let resp = call!(app, test::TestRequest::post().uri(&catalog_url(s.org, beans, "archive")), s.token);
    assert_eq!(resp.status(), 409);
    let body: Value = test::read_body_json(resp).await;
    assert_eq!(body["code"], "ARCHIVE_NEEDS_CONFIRMATION");
    assert_eq!(body["vars"]["usage_count"], 1);
    assert_eq!(body["vars"]["branches_with_stock"], 1);
    let archived: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT archived_at FROM org_ingredients WHERE id = $1")
            .bind(beans)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(archived.is_none());

    // Confirmed: archived, and the recipe line, balance and ledger are untouched.
    let resp = call!(
        app,
        test::TestRequest::post().uri(&catalog_url(s.org, beans, "archive")).set_json(json!({"confirm": true})),
        s.token
    );
    assert_eq!(resp.status(), 200);
    assert!((on_hand(&pool, s.branch, beans).await - 1000.0).abs() < 1e-9);
    assert_eq!(movement_count(&pool, beans).await, movements_before);
    let lines: i64 = sqlx::query_scalar("SELECT count(*) FROM recipe_lines WHERE ingredient_id = $1")
        .bind(beans)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(lines, 1);

    // An unused ingredient needs no confirmation and may be deleted.
    let spare = ingredient(&pool, s.org, "Spare").await;
    let resp = call!(app, test::TestRequest::get().uri(&catalog_url(s.org, spare, "usage")), s.token);
    let usage: IngredientUsage = test::read_body_json(resp).await;
    assert!(!usage.needs_confirmation);
    assert!(usage.never_used);
}

#[sqlx::test]
async fn sale_of_a_recipe_using_an_archived_ingredient_still_deducts_and_costs(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let beans = ingredient(&pool, s.org, "Beans").await; // 100 piastres / g
    let item = menu_item_with_recipe(&pool, s.org, beans, 20.0).await;
    stock_in(&pool, s.branch, beans, 1000.0).await;

    let resp = call!(
        app,
        test::TestRequest::post().uri(&catalog_url(s.org, beans, "archive")).set_json(json!({"confirm": true})),
        s.token
    );
    assert_eq!(resp.status(), 200);

    let teller = user(&pool, s.org, "teller").await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(teller)
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();
    grant(&pool, "teller", "orders", "create").await;
    let till = Uuid::new_v4();
    sqlx::query("INSERT INTO tills (id, branch_id, teller_id, status, opening_cash) VALUES ($1, $2, $3, 'open', 0)")
        .bind(till)
        .bind(s.branch)
        .bind(teller)
        .execute(&pool)
        .await
        .unwrap();
    let teller_token = token(teller, s.org, UserRole::Teller, Some(s.branch));

    let order = CreateOrderRequest {
        branch_id: s.branch,
        till_id: till,
        payment_method: "cash".to_string(),
        items: vec![OrderItemInput {
            menu_item_id: Some(item),
            quantity: 2,
            ..Default::default()
        }],
        ..Default::default()
    };
    let resp = call!(app, test::TestRequest::post().uri("/orders").set_json(&order), teller_token);
    assert!(resp.status().is_success(), "order: {:?}", resp.status());
    let full: OrderFull = test::read_body_json(resp).await;
    let line = &full.items[0].item;
    assert_eq!(line.unit_cost, Some(2_000), "20 g × 100 piastres");
    assert_eq!(line.line_cost, Some(4_000));
    assert!(!line.cost_missing);

    // Stock deducted through the ledger: 1000 − 2 × 20.
    assert!((on_hand(&pool, s.branch, beans).await - 960.0).abs() < 1e-9);
    let sale: f64 = sqlx::query_scalar(
        "SELECT SUM(quantity)::float8 FROM inventory_movements \
          WHERE org_ingredient_id = $1 AND type = 'sale' AND source_id = $2",
    )
    .bind(beans)
    .bind(full.order.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!((sale + 40.0).abs() < 1e-9);
}

#[sqlx::test]
async fn history_and_stock_keep_an_archived_ingredient_with_its_flag(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let milk = ingredient(&pool, s.org, "Old Milk").await;
    let empty = ingredient(&pool, s.org, "Old Cups").await;
    stock_in(&pool, s.branch, milk, 50.0).await;
    // Waste some, so the waste log has a row.
    let resp = call!(
        app,
        test::TestRequest::post()
            .uri(&format!("/inventory/branches/{}/waste", s.branch))
            .set_json(json!({"org_ingredient_id": milk, "quantity": 5.0, "reason": "spoiled"})),
        s.token
    );
    assert!(resp.status().is_success(), "waste: {:?}", resp.status());
    for id in [milk, empty] {
        let resp = call!(
            app,
            test::TestRequest::post().uri(&catalog_url(s.org, id, "archive")).set_json(json!({"confirm": true})),
            s.token
        );
        assert_eq!(resp.status(), 200);
    }

    let resp = call!(
        app,
        test::TestRequest::get().uri(&format!("/inventory/branches/{}/movements", s.branch)),
        s.token
    );
    let moves: Vec<StockMovement> = test::read_body_json(resp).await;
    assert_eq!(moves.len(), 2);
    assert!(moves.iter().all(|m| m.ingredient_name == "Old Milk" && m.ingredient_archived_at.is_some()));

    let resp = call!(
        app,
        test::TestRequest::get().uri(&format!("/inventory/branches/{}/waste", s.branch)),
        s.token
    );
    let waste: Vec<StockMovement> = test::read_body_json(resp).await;
    assert_eq!(waste.len(), 1);
    assert!(waste[0].ingredient_archived_at.is_some());

    // Branch stock: omitted = all (old clients), flagged; active hides them.
    let resp = call!(
        app,
        test::TestRequest::get().uri(&format!("/inventory/branches/{}/stock", s.branch)),
        s.token
    );
    let rows: Vec<BranchStockRow> = test::read_body_json(resp).await;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.archived_at.is_some()));
    let resp = call!(
        app,
        test::TestRequest::get().uri(&format!("/inventory/branches/{}/stock?status=active", s.branch)),
        s.token
    );
    let rows: Vec<BranchStockRow> = test::read_body_json(resp).await;
    assert!(rows.is_empty());

    // Valuation still counts the archived stock (45 g × 100).
    let resp = call!(
        app,
        test::TestRequest::get().uri(&format!("/reports/branches/{}/inventory-valuation", s.branch)),
        s.token
    );
    assert!(resp.status().is_success(), "valuation: {:?}", resp.status());
    let body: Value = test::read_body_json(resp).await;
    assert!(body.to_string().contains("Old Milk"), "valuation keeps the archived ingredient: {body}");

    // Branch stock report: the archived one with stock stays, the empty one goes.
    let resp = call!(
        app,
        test::TestRequest::get().uri(&format!("/reports/branches/{}/stock", s.branch)),
        s.token
    );
    let body: Value = test::read_body_json(resp).await;
    let text = body.to_string();
    assert!(text.contains("Old Milk"));
    assert!(!text.contains("Old Cups"));
}

#[sqlx::test]
async fn counts_and_reorders_leave_archived_ingredients_out(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let live = ingredient(&pool, s.org, "Live").await;
    let gone_empty = ingredient(&pool, s.org, "Gone Empty").await;
    let gone_stocked = ingredient(&pool, s.org, "Gone Stocked").await;
    stock_in(&pool, s.branch, gone_stocked, 3.0).await;
    // Both below par, so the reorder views would list them.
    for id in [live, gone_stocked] {
        let resp = call!(
            app,
            test::TestRequest::put()
                .uri(&format!("/inventory/branches/{}/stock/{id}/par", s.branch))
                .set_json(json!({"par_min": 10.0, "par_max": 20.0})),
            s.token
        );
        assert_eq!(resp.status(), 200);
    }
    for id in [gone_empty, gone_stocked] {
        let resp = call!(
            app,
            test::TestRequest::post().uri(&catalog_url(s.org, id, "archive")).set_json(json!({"confirm": true})),
            s.token
        );
        assert_eq!(resp.status(), 200);
    }

    // A full count lists the live one and the archived one still holding stock.
    let resp = call!(
        app,
        test::TestRequest::post()
            .uri(&format!("/stocktakes/branches/{}", s.branch))
            .set_json(json!({})),
        s.token
    );
    assert!(resp.status().is_success(), "count: {:?}", resp.status());
    let body: Value = test::read_body_json(resp).await;
    let mut names: Vec<String> = body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|i| i["ingredient_name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["Gone Stocked", "Live"]);

    // Low stock and reorder suggestions skip archived ingredients.
    let resp = call!(
        app,
        test::TestRequest::get().uri(&format!("/reports/branches/{}/low-stock", s.branch)),
        s.token
    );
    let low: Value = test::read_body_json(resp).await;
    let low = low.to_string();
    assert!(low.contains("Live") && !low.contains("Gone Stocked"), "{low}");
    let resp = call!(
        app,
        test::TestRequest::get().uri(&format!("/reports/orgs/{}/low-stock", s.org)),
        s.token
    );
    let low: Value = test::read_body_json(resp).await;
    let low = low.to_string();
    assert!(low.contains("Live") && !low.contains("Gone Stocked"), "{low}");
    let resp = call!(
        app,
        test::TestRequest::get().uri(&format!("/purchasing/branches/{}/reorder-suggestions", s.branch)),
        s.token
    );
    assert!(resp.status().is_success());
    let sug: Value = test::read_body_json(resp).await;
    let sug = sug.to_string();
    assert!(sug.contains("Live") && !sug.contains("Gone Stocked"), "{sug}");
}

#[sqlx::test]
async fn archive_needs_the_inventory_edit_capability(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let ing = ingredient(&pool, s.org, "Beans").await;
    // A teller can see stock but may not edit stock items.
    grant(&pool, "teller", "inventory", "read").await;
    let teller = user(&pool, s.org, "teller").await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(teller)
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();
    let t = token(teller, s.org, UserRole::Teller, Some(s.branch));

    let resp = call!(app, test::TestRequest::get().uri(&catalog_url(s.org, ing, "usage")), t);
    assert_eq!(resp.status(), 200, "reading usage needs only inventory read");
    for tail in ["archive", "unarchive"] {
        let resp = call!(app, test::TestRequest::post().uri(&catalog_url(s.org, ing, tail)), t);
        assert_eq!(resp.status(), 403, "{tail}");
    }
}

#[sqlx::test]
async fn archive_is_tenant_isolated(pool: PgPool) {
    let app = app!(pool);
    let a = shop(&pool).await;
    let b = shop(&pool).await;
    let ing = ingredient(&pool, a.org, "A's Beans").await;

    // Org B's admin, against org A's path or with A's ingredient under B's path.
    for url in [
        catalog_url(a.org, ing, "archive"),
        catalog_url(a.org, ing, "unarchive"),
        catalog_url(a.org, ing, "usage"),
    ] {
        let req = if url.ends_with("usage") {
            test::TestRequest::get().uri(&url)
        } else {
            test::TestRequest::post().uri(&url)
        };
        let resp = call!(app, req, b.token);
        assert_eq!(resp.status(), 403, "{url}");
    }
    for (tail, get) in [("archive", false), ("unarchive", false), ("usage", true)] {
        let url = catalog_url(b.org, ing, tail);
        let req = if get { test::TestRequest::get().uri(&url) } else { test::TestRequest::post().uri(&url) };
        let resp = call!(app, req, b.token);
        assert_eq!(resp.status(), 404, "{url}");
    }

    // RLS: a tenant session of B sees none of A's ingredients, archived or not.
    let resp = call!(
        app,
        test::TestRequest::post().uri(&catalog_url(a.org, ing, "archive")),
        a.token
    );
    assert_eq!(resp.status(), 200);
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.org_id', $1, true)")
        .bind(b.org.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SET LOCAL ROLE madar_app").execute(&mut *tx).await.unwrap();
    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM org_ingredients WHERE id = $1")
        .bind(ing)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(seen, 0);
    sqlx::query("SELECT set_config('app.org_id', $1, true)")
        .bind(a.org.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    let archived: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT archived_at FROM org_ingredients WHERE id = $1")
            .bind(ing)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert!(archived.is_some());
}

#[sqlx::test]
async fn an_archived_name_is_not_reused(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let old = ingredient(&pool, s.org, "Hazelnut Syrup").await;
    let other = ingredient(&pool, s.org, "Caramel Syrup").await;
    let resp = call!(app, test::TestRequest::post().uri(&catalog_url(s.org, old, "archive")), s.token);
    assert_eq!(resp.status(), 200);

    // Creating it again: refused, pointing at the archived one.
    let resp = call!(
        app,
        test::TestRequest::post()
            .uri(&format!("/inventory/orgs/{}/catalog", s.org))
            .set_json(json!({"name": "Hazelnut Syrup", "unit": "ml"})),
        s.token
    );
    assert_eq!(resp.status(), 409);
    let body: Value = test::read_body_json(resp).await;
    assert_eq!(body["code"], "INGREDIENT_NAME_ARCHIVED");
    assert_eq!(body["vars"]["ingredient_id"], old.to_string());

    // Renaming another one onto it: the same refusal.
    let resp = call!(
        app,
        test::TestRequest::patch()
            .uri(&format!("/inventory/orgs/{}/catalog/{other}", s.org))
            .set_json(json!({"name": "Hazelnut Syrup"})),
        s.token
    );
    assert_eq!(resp.status(), 409);
    let body: Value = test::read_body_json(resp).await;
    assert_eq!(body["code"], "INGREDIENT_NAME_ARCHIVED");

    // An active holder keeps the plain conflict, without the archive code.
    let resp = call!(
        app,
        test::TestRequest::post()
            .uri(&format!("/inventory/orgs/{}/catalog", s.org))
            .set_json(json!({"name": "Caramel Syrup", "unit": "ml"})),
        s.token
    );
    assert_eq!(resp.status(), 409);
    let body: Value = test::read_body_json(resp).await;
    assert!(body.get("code").is_none() || body["code"].is_null());

    // After unarchiving, the original is simply back under its name.
    let resp = call!(app, test::TestRequest::post().uri(&catalog_url(s.org, old, "unarchive")), s.token);
    assert_eq!(resp.status(), 200);
    let active = catalog!(app, s, Some("active"));
    assert!(active.iter().any(|r| r.id == old && r.name == "Hazelnut Syrup"));
}

#[sqlx::test]
async fn an_archived_ingredient_stays_on_the_till_feed(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let ing = ingredient(&pool, s.org, "Beans").await;
    let resp = call!(app, test::TestRequest::post().uri(&catalog_url(s.org, ing, "archive")), s.token);
    assert_eq!(resp.status(), 200);

    // The archive re-emits the ingredient as an upsert, never a delete.
    let last_op: String = sqlx::query_scalar(
        "SELECT op FROM sync_changes WHERE type = 'ingredient' AND entity_id = $1 ORDER BY seq DESC LIMIT 1",
    )
    .bind(ing)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(last_op, "upsert");

    // Still projected, with the old fields and the additive archived_at.
    let mut conn = pool.acquire().await.unwrap();
    let rows = madar_rust::sync::pull::projection::project(&mut conn, s.org, s.branch, "ingredient", &[ing])
        .await
        .unwrap();
    let row = rows.get(&ing).expect("an archived ingredient is still projected");
    assert_eq!(row["name"], "Beans");
    assert_eq!(row["is_active"], true);
    assert!(row["archived_at"].is_string());
}
