#![allow(unused_imports, unused_variables, dead_code)]
use actix_web::{test, App, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::purchasing::routes;
use crate::purchasing::handlers::{Supplier, PurchaseOrderFull, PurchaseOrder};

fn get_secret() -> JwtSecret { JwtSecret("secret".to_string()) }
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
/// Ingredient stocked in grams, cost 300 piastres/g.
async fn seed_ingredient_g(pool: &PgPool, org_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, category, cost_per_unit) VALUES ($1, $2, 'Flour', 'g'::inventory_unit, 'dry', 300)")
        .bind(id).bind(org_id).execute(pool).await.unwrap();
    id
}

#[sqlx::test]
async fn test_receive_purchase_updates_stock_cost_and_movement(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    for a in ["create", "read", "update"] { grant(&pool, "purchase_orders", a).await; }
    let ing = seed_ingredient_g(&pool, org_id).await;
    let token = org_admin_token(user_id, org_id);

    // PO: buy 2 kg of a g-stocked ingredient at 5000 piastres/kg.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/branches/{branch_id}/orders"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(serde_json::json!({
            "lines": [{
                "org_ingredient_id": ing,
                "purchase_unit": "kg",
                "quantity_ordered": 2.0,
                "unit_cost": 5000
            }]
        }))
        .to_request()).await;
    assert_eq!(resp.status(), 201);
    let po: PurchaseOrderFull = test::read_body_json(resp).await;
    assert_eq!(po.lines.len(), 1);
    // kg → g derived a ×1000 pack factor.
    assert_eq!(po.lines[0].units_per_purchase_unit, 1000.0);
    let line_id = po.lines[0].id;

    // Receive the 2 kg.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/orders/{}/receive", po.order.id))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(serde_json::json!({"lines": [{"line_id": line_id, "quantity_received": 2.0}]}))
        .to_request()).await;
    assert!(resp.status().is_success());
    let received: PurchaseOrderFull = test::read_body_json(resp).await;
    assert_eq!(received.order.status, "received");

    // Stock is now 2000 g.
    let stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE branch_id=$1 AND org_ingredient_id=$2")
        .bind(branch_id).bind(ing).fetch_one(&pool).await.unwrap();
    assert_eq!(stock, 2000.0);

    // A purchase_in movement was posted for +2000 g.
    let (mtype, mqty): (String, f64) = sqlx::query_as("SELECT type::text, quantity::float8 FROM inventory_movements WHERE source_type='purchase' AND source_id=$1")
        .bind(po.order.id).fetch_one(&pool).await.unwrap();
    assert_eq!(mtype, "purchase_in");
    assert_eq!(mqty, 2000.0);

    // Weighted average: prior on-hand was 0 → cost becomes the purchase price
    // per gram = 5000 / 1000 = 5 piastres/g.
    let cost: f64 = sqlx::query_scalar("SELECT cost_per_unit::float8 FROM org_ingredients WHERE id=$1")
        .bind(ing).fetch_one(&pool).await.unwrap();
    assert_eq!(cost, 5.0);
}

#[sqlx::test]
async fn test_list_orders_all_branches(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    ).await;

    let org_id   = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_id).await;
    let branch_b = {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1,$2,'Branch B')")
            .bind(id).bind(org_id).execute(&pool).await.unwrap();
        id
    };
    let user_id = seed_user(&pool, org_id).await;
    for a in ["create", "read"] { grant(&pool, "purchase_orders", a).await; }
    let ing = seed_ingredient_g(&pool, org_id).await;
    let token = org_admin_token(user_id, org_id);

    let mk_po = |branch: Uuid| test::TestRequest::post()
        .uri(&format!("/purchasing/branches/{branch}/orders"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(serde_json::json!({
            "lines": [{"org_ingredient_id": ing, "purchase_unit": "kg", "quantity_ordered": 1.0, "unit_cost": 5000}]
        })).to_request();
    assert_eq!(test::call_service(&app, mk_po(branch_a)).await.status(), 201);
    assert_eq!(test::call_service(&app, mk_po(branch_b)).await.status(), 201);

    // A different org's PO must never appear in this org's all-branches view.
    let other_org    = seed_org(&pool).await;
    let other_branch = seed_branch(&pool, other_org).await;
    let other_user   = seed_user(&pool, other_org).await;
    let other_ing    = seed_ingredient_g(&pool, other_org).await;
    let other_token  = org_admin_token(other_user, other_org);
    test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/branches/{other_branch}/orders"))
        .insert_header(("Authorization", format!("Bearer {other_token}")))
        .set_json(serde_json::json!({
            "lines": [{"org_ingredient_id": other_ing, "purchase_unit": "kg", "quantity_ordered": 1.0, "unit_cost": 5000}]
        })).to_request()).await;

    let auth = ("Authorization", format!("Bearer {token}"));

    // All branches (nil UUID): both org branches' POs, branch-labelled, org-isolated.
    let nil = Uuid::nil();
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/purchasing/branches/{nil}/orders")).insert_header(auth.clone()).to_request()).await;
    assert_eq!(resp.status(), 200);
    let pos: Vec<PurchaseOrder> = test::read_body_json(resp).await;
    assert_eq!(pos.len(), 2, "all-branches sees both org branches' purchase orders");
    assert!(pos.iter().all(|p| p.branch_name.is_some()), "rows carry a branch label");
    let seen: std::collections::HashSet<_> = pos.iter().map(|p| p.branch_id).collect();
    assert!(seen.contains(&branch_a) && seen.contains(&branch_b));

    // A specific branch still scopes to that one branch.
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/purchasing/branches/{branch_a}/orders")).insert_header(auth.clone()).to_request()).await;
    assert_eq!(resp.status(), 200);
    let just_a: Vec<PurchaseOrder> = test::read_body_json(resp).await;
    assert_eq!(just_a.len(), 1);
    assert_eq!(just_a[0].branch_id, branch_a);
}

#[sqlx::test]
async fn test_partial_receive_then_complete(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(get_secret()))
            .configure(routes::configure),
    ).await;

    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    for a in ["create", "read", "update"] { grant(&pool, "purchase_orders", a).await; }
    let ing = seed_ingredient_g(&pool, org_id).await;
    let token = org_admin_token(user_id, org_id);

    // PO: 2 kg of a g-stocked ingredient.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/branches/{branch_id}/orders"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(serde_json::json!({
            "lines": [{"org_ingredient_id": ing, "purchase_unit": "kg", "quantity_ordered": 2.0, "unit_cost": 5000}]
        })).to_request()).await;
    let po: PurchaseOrderFull = test::read_body_json(resp).await;
    let line_id = po.lines[0].id;
    let po_id = po.order.id;

    let receive = |qty: f64| test::TestRequest::post()
        .uri(&format!("/purchasing/orders/{po_id}/receive"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(serde_json::json!({"lines": [{"line_id": line_id, "quantity_received": qty}]}))
        .to_request();

    // First shipment: 1 of 2 kg → partially_received, 1000 g on hand.
    let resp = test::call_service(&app, receive(1.0)).await;
    assert!(resp.status().is_success());
    let r1: PurchaseOrderFull = test::read_body_json(resp).await;
    assert_eq!(r1.order.status, "partially_received");
    assert_eq!(r1.lines[0].quantity_received, 1.0);
    let stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE branch_id=$1 AND org_ingredient_id=$2")
        .bind(branch_id).bind(ing).fetch_one(&pool).await.unwrap();
    assert_eq!(stock, 1000.0);

    // Second shipment: remaining 1 kg → received, 2000 g on hand.
    let resp = test::call_service(&app, receive(1.0)).await;
    assert!(resp.status().is_success());
    let r2: PurchaseOrderFull = test::read_body_json(resp).await;
    assert_eq!(r2.order.status, "received");
    assert_eq!(r2.lines[0].quantity_received, 2.0);
    let stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE branch_id=$1 AND org_ingredient_id=$2")
        .bind(branch_id).bind(ing).fetch_one(&pool).await.unwrap();
    assert_eq!(stock, 2000.0);
}

// ──────────────────────────────────────────────────────────────
// Suppliers
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

#[sqlx::test]
async fn test_supplier_crud_and_validation(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id).await;
    for a in ["create", "read", "update", "delete"] { grant(&pool, "suppliers", a).await; }
    let token = org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    // Empty name → 400.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/orgs/{org_id}/suppliers")).insert_header(auth.clone())
        .set_json(serde_json::json!({"name": "  "})).to_request()).await;
    assert_eq!(resp.status(), 400);

    // Create OK.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/orgs/{org_id}/suppliers")).insert_header(auth.clone())
        .set_json(serde_json::json!({"name": "Cairo Dairy", "phone": "0100"})).to_request()).await;
    assert_eq!(resp.status(), 201);
    let sup: Supplier = test::read_body_json(resp).await;
    assert_eq!(sup.name, "Cairo Dairy");
    assert!(sup.is_active);

    // List shows it.
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/purchasing/orgs/{org_id}/suppliers")).insert_header(auth.clone())
        .to_request()).await;
    let list: Vec<Supplier> = test::read_body_json(resp).await;
    assert_eq!(list.len(), 1);

    // Update name + deactivate.
    let resp = test::call_service(&app, test::TestRequest::patch()
        .uri(&format!("/purchasing/suppliers/{}", sup.id)).insert_header(auth.clone())
        .set_json(serde_json::json!({"name": "Cairo Dairy Co", "is_active": false})).to_request()).await;
    assert_eq!(resp.status(), 200);
    let updated: Supplier = test::read_body_json(resp).await;
    assert_eq!(updated.name, "Cairo Dairy Co");
    assert!(!updated.is_active);

    // Delete (soft) → 204, then absent from list.
    let resp = test::call_service(&app, test::TestRequest::delete()
        .uri(&format!("/purchasing/suppliers/{}", sup.id)).insert_header(auth.clone())
        .to_request()).await;
    assert_eq!(resp.status(), 204);
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/purchasing/orgs/{org_id}/suppliers")).insert_header(auth.clone())
        .to_request()).await;
    let list: Vec<Supplier> = test::read_body_json(resp).await;
    assert_eq!(list.len(), 0);
}

#[sqlx::test]
async fn test_supplier_cross_org_forbidden(pool: PgPool) {
    let app = init_app!(pool);
    let org_a = seed_org(&pool).await;
    let org_b = seed_org(&pool).await;
    let user_a = seed_user(&pool, org_a).await;
    grant(&pool, "suppliers", "create").await;
    let token = org_admin_token(user_a, org_a);

    // Admin of org A cannot create a supplier under org B's path.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/orgs/{org_b}/suppliers"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(serde_json::json!({"name": "X"})).to_request()).await;
    assert_eq!(resp.status(), 403);
}

async fn deny_user(pool: &PgPool, user_id: Uuid, resource: &str, action: &str) {
    sqlx::query("INSERT INTO permissions (user_id, resource, action, granted) VALUES ($1, $2::permission_resource, $3::permission_action, false)")
        .bind(user_id).bind(resource).bind(action).execute(pool).await.unwrap();
}

#[sqlx::test]
async fn test_supplier_permission_denied(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let user_id = seed_user(&pool, org_id).await;
    // org_admin is granted by the seed migration; a per-user deny override wins.
    deny_user(&pool, user_id, "suppliers", "read").await;
    let token = org_admin_token(user_id, org_id);
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/purchasing/orgs/{org_id}/suppliers"))
        .insert_header(("Authorization", format!("Bearer {token}"))).to_request()).await;
    assert_eq!(resp.status(), 403);
}

// ──────────────────────────────────────────────────────────────
// Purchase order create — validation
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_po_create_validations(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    for a in ["create", "read", "update"] { grant(&pool, "purchase_orders", a).await; }
    let ing = seed_ingredient_g(&pool, org_id).await;
    let token = org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));
    let url = format!("/purchasing/branches/{branch_id}/orders");

    // Empty lines → 400.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&url).insert_header(auth.clone())
        .set_json(serde_json::json!({"lines": []})).to_request()).await;
    assert_eq!(resp.status(), 400);

    // quantity_ordered <= 0 → 400.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&url).insert_header(auth.clone())
        .set_json(serde_json::json!({"lines": [{"org_ingredient_id": ing, "purchase_unit": "g", "quantity_ordered": 0.0, "unit_cost": 5}]})).to_request()).await;
    assert_eq!(resp.status(), 400);

    // unit_cost < 0 → 400.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&url).insert_header(auth.clone())
        .set_json(serde_json::json!({"lines": [{"org_ingredient_id": ing, "purchase_unit": "g", "quantity_ordered": 1.0, "unit_cost": -1}]})).to_request()).await;
    assert_eq!(resp.status(), 400);

    // Ingredient not in this org → 400.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&url).insert_header(auth.clone())
        .set_json(serde_json::json!({"lines": [{"org_ingredient_id": Uuid::new_v4(), "purchase_unit": "g", "quantity_ordered": 1.0, "unit_cost": 5}]})).to_request()).await;
    assert_eq!(resp.status(), 400);

    // Free-text pack unit (e.g. "case") is rejected — must be a real stock unit.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&url).insert_header(auth.clone())
        .set_json(serde_json::json!({"lines": [{"org_ingredient_id": ing, "purchase_unit": "case", "units_per_purchase_unit": 24.0, "quantity_ordered": 2.0, "unit_cost": 4800}]})).to_request()).await;
    assert_eq!(resp.status(), 400);

    // Cross-measure purchase unit (litres for a gram ingredient) is rejected.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&url).insert_header(auth.clone())
        .set_json(serde_json::json!({"lines": [{"org_ingredient_id": ing, "purchase_unit": "l", "quantity_ordered": 1.0, "unit_cost": 5}]})).to_request()).await;
    assert_eq!(resp.status(), 400);

    // Same-measure unit (kg for a gram ingredient) is accepted; factor derived = 1000.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&url).insert_header(auth.clone())
        .set_json(serde_json::json!({"lines": [{"org_ingredient_id": ing, "purchase_unit": "kg", "quantity_ordered": 2.0, "unit_cost": 4800}]})).to_request()).await;
    assert_eq!(resp.status(), 201);
    let po: PurchaseOrderFull = test::read_body_json(resp).await;
    assert_eq!(po.lines[0].units_per_purchase_unit, 1000.0);
    assert_eq!(po.order.status, "draft");
}

#[sqlx::test]
async fn test_po_create_supplier_wrong_org_rejected(pool: PgPool) {
    let app = init_app!(pool);
    let org_a = seed_org(&pool).await;
    let org_b = seed_org(&pool).await;
    let branch_a = seed_branch(&pool, org_a).await;
    let user_a = seed_user(&pool, org_a).await;
    for a in ["create", "read", "update"] { grant(&pool, "purchase_orders", a).await; }
    let ing = seed_ingredient_g(&pool, org_a).await;
    // Supplier belonging to org B.
    let sup_b = Uuid::new_v4();
    sqlx::query("INSERT INTO suppliers (id, org_id, name) VALUES ($1, $2, 'Other')")
        .bind(sup_b).bind(org_b).execute(&pool).await.unwrap();
    let token = org_admin_token(user_a, org_a);

    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/branches/{branch_a}/orders"))
        .insert_header(("Authorization", format!("Bearer {token}")))
        .set_json(serde_json::json!({"supplier_id": sup_b, "lines": [{"org_ingredient_id": ing, "purchase_unit": "g", "quantity_ordered": 1.0, "unit_cost": 5}]})).to_request()).await;
    assert_eq!(resp.status(), 400);
}

// ──────────────────────────────────────────────────────────────
// Purchase order list / get / filters
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_po_list_branch_org_and_filters(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    for a in ["create", "read", "update"] { grant(&pool, "purchase_orders", a).await; }
    let ing = seed_ingredient_g(&pool, org_id).await;
    let token = org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    let mk_po = || test::TestRequest::post()
        .uri(&format!("/purchasing/branches/{branch_id}/orders")).insert_header(auth.clone())
        .set_json(serde_json::json!({"lines": [{"org_ingredient_id": ing, "purchase_unit": "g", "quantity_ordered": 10.0, "unit_cost": 5}]}))
        .to_request();
    let po1: PurchaseOrderFull = test::read_body_json(test::call_service(&app, mk_po()).await).await;
    let _po2: PurchaseOrderFull = test::read_body_json(test::call_service(&app, mk_po()).await).await;

    // Cancel po1 → status cancelled.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/orders/{}/cancel", po1.order.id)).insert_header(auth.clone())
        .to_request()).await;
    assert_eq!(resp.status(), 200);

    // Branch list → both.
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/purchasing/branches/{branch_id}/orders")).insert_header(auth.clone()).to_request()).await;
    let all: Vec<PurchaseOrder> = test::read_body_json(resp).await;
    assert_eq!(all.len(), 2);

    // Org list → both.
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/purchasing/orgs/{org_id}/orders")).insert_header(auth.clone()).to_request()).await;
    let org_all: Vec<PurchaseOrder> = test::read_body_json(resp).await;
    assert_eq!(org_all.len(), 2);

    // Org list filtered to cancelled → 1.
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/purchasing/orgs/{org_id}/orders?status=cancelled")).insert_header(auth.clone()).to_request()).await;
    let cancelled: Vec<PurchaseOrder> = test::read_body_json(resp).await;
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].status, "cancelled");

    // Get not found → 404.
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/purchasing/orders/{}", Uuid::new_v4())).insert_header(auth.clone()).to_request()).await;
    assert_eq!(resp.status(), 404);
}

// ──────────────────────────────────────────────────────────────
// Receiving — errors + weighted-average cost
// ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn test_receive_errors(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    for a in ["create", "read", "update"] { grant(&pool, "purchase_orders", a).await; }
    let ing = seed_ingredient_g(&pool, org_id).await;
    let token = org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    let po: PurchaseOrderFull = test::read_body_json(test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/branches/{branch_id}/orders")).insert_header(auth.clone())
        .set_json(serde_json::json!({"lines": [{"org_ingredient_id": ing, "purchase_unit": "g", "quantity_ordered": 10.0, "unit_cost": 5}]}))
        .to_request()).await).await;

    // Line id from a different PO → 400.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/orders/{}/receive", po.order.id)).insert_header(auth.clone())
        .set_json(serde_json::json!({"lines": [{"line_id": Uuid::new_v4(), "quantity_received": 5.0}]})).to_request()).await;
    assert_eq!(resp.status(), 400);

    // Cancel then receive → 409.
    test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/orders/{}/cancel", po.order.id)).insert_header(auth.clone()).to_request()).await;
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/orders/{}/receive", po.order.id)).insert_header(auth.clone())
        .set_json(serde_json::json!({"lines": [{"line_id": po.lines[0].id, "quantity_received": 5.0}]})).to_request()).await;
    assert_eq!(resp.status(), 409);

    // Cancel an already-cancelled is fine-ish, but cancelling a received PO → 409 (covered below).
}

#[sqlx::test]
async fn test_receive_weighted_average_cost_blends(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    for a in ["create", "read", "update"] { grant(&pool, "purchase_orders", a).await; }
    let ing = seed_ingredient_g(&pool, org_id).await;
    // Prior state: cost 10 piastres/g, 1000 g on hand.
    sqlx::query("UPDATE org_ingredients SET cost_per_unit = 10 WHERE id = $1").bind(ing).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO branch_inventory (branch_id, org_ingredient_id, current_stock, reorder_threshold) VALUES ($1, $2, 1000, 0)")
        .bind(branch_id).bind(ing).execute(&pool).await.unwrap();
    let token = org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    // Receive 1000 g at 20 piastres/g.
    let po: PurchaseOrderFull = test::read_body_json(test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/branches/{branch_id}/orders")).insert_header(auth.clone())
        .set_json(serde_json::json!({"lines": [{"org_ingredient_id": ing, "purchase_unit": "g", "quantity_ordered": 1000.0, "unit_cost": 20}]}))
        .to_request()).await).await;
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/orders/{}/receive", po.order.id)).insert_header(auth.clone())
        .set_json(serde_json::json!({"lines": [{"line_id": po.lines[0].id, "quantity_received": 1000.0}]})).to_request()).await;
    assert!(resp.status().is_success());

    // (1000*10 + 1000*20) / 2000 = 15.
    let cost: f64 = sqlx::query_scalar("SELECT cost_per_unit::float8 FROM org_ingredients WHERE id=$1").bind(ing).fetch_one(&pool).await.unwrap();
    assert_eq!(cost, 15.0);
    // Stock now 2000 g.
    let stock: f64 = sqlx::query_scalar("SELECT current_stock::float8 FROM branch_inventory WHERE branch_id=$1 AND org_ingredient_id=$2")
        .bind(branch_id).bind(ing).fetch_one(&pool).await.unwrap();
    assert_eq!(stock, 2000.0);
}

#[sqlx::test]
async fn test_cancel_received_conflict(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    for a in ["create", "read", "update"] { grant(&pool, "purchase_orders", a).await; }
    let ing = seed_ingredient_g(&pool, org_id).await;
    let token = org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    let po: PurchaseOrderFull = test::read_body_json(test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/branches/{branch_id}/orders")).insert_header(auth.clone())
        .set_json(serde_json::json!({"lines": [{"org_ingredient_id": ing, "purchase_unit": "g", "quantity_ordered": 5.0, "unit_cost": 5}]}))
        .to_request()).await).await;
    // Fully receive → received.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/orders/{}/receive", po.order.id)).insert_header(auth.clone())
        .set_json(serde_json::json!({"lines": [{"line_id": po.lines[0].id, "quantity_received": 5.0}]})).to_request()).await;
    let received: PurchaseOrderFull = test::read_body_json(resp).await;
    assert_eq!(received.order.status, "received");
    // Cancel a received PO → 409.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/orders/{}/cancel", po.order.id)).insert_header(auth.clone()).to_request()).await;
    assert_eq!(resp.status(), 409);
}

#[sqlx::test]
async fn test_po_permission_denied(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    // Per-user deny override beats the seeded org_admin default.
    deny_user(&pool, user_id, "purchase_orders", "read").await;
    let token = org_admin_token(user_id, org_id);
    let resp = test::call_service(&app, test::TestRequest::get()
        .uri(&format!("/purchasing/branches/{branch_id}/orders"))
        .insert_header(("Authorization", format!("Bearer {token}"))).to_request()).await;
    assert_eq!(resp.status(), 403);
}

// ── Audit regression tests ───────────────────────────────────────────────

/// V10: a cheap-per-base-unit purchase must NOT round the catalog cost to 0
/// ("free"). 400 piastres/kg = 0.40 piastres/g must persist as 0.40 in the
/// numeric(15,2) cost_per_unit, not be truncated to integer piastres.
#[sqlx::test]
async fn test_receive_cheap_cost_not_rounded_to_zero(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    for a in ["create","read","update"] { grant(&pool, "purchase_orders", a).await; }
    // Ingredient stocked in grams, cost UNKNOWN.
    let ing = Uuid::new_v4();
    sqlx::query("INSERT INTO org_ingredients (id, org_id, name, unit, category, cost_per_unit) VALUES ($1,$2,'Salt','g'::inventory_unit,'dry',NULL)")
        .bind(ing).bind(org_id).execute(&pool).await.unwrap();
    let token = org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    // Buy 1 kg at 400 piastres/kg = 0.40 piastres/g.
    let po: PurchaseOrderFull = test::read_body_json(test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/branches/{branch_id}/orders")).insert_header(auth.clone())
        .set_json(serde_json::json!({"lines":[{"org_ingredient_id":ing,"purchase_unit":"kg","quantity_ordered":1.0,"unit_cost":400}]}))
        .to_request()).await).await;
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/orders/{}/receive", po.order.id)).insert_header(auth.clone())
        .set_json(serde_json::json!({"lines":[{"line_id":po.lines[0].id,"quantity_received":1.0}]})).to_request()).await;
    assert!(resp.status().is_success());

    let cost: f64 = sqlx::query_scalar("SELECT cost_per_unit::float8 FROM org_ingredients WHERE id=$1").bind(ing).fetch_one(&pool).await.unwrap();
    assert!((cost - 0.40).abs() < 1e-9, "cheap-per-gram cost must persist as 0.40, got {cost}");
}

/// V8: a receive request with the same line_id twice is rejected (would
/// otherwise double-apply stock and cost).
#[sqlx::test]
async fn test_receive_rejects_duplicate_line_id(pool: PgPool) {
    let app = init_app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let user_id = seed_user(&pool, org_id).await;
    for a in ["create","read","update"] { grant(&pool, "purchase_orders", a).await; }
    let ing = seed_ingredient_g(&pool, org_id).await;
    let token = org_admin_token(user_id, org_id);
    let auth = ("Authorization", format!("Bearer {token}"));

    let po: PurchaseOrderFull = test::read_body_json(test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/branches/{branch_id}/orders")).insert_header(auth.clone())
        .set_json(serde_json::json!({"lines":[{"org_ingredient_id":ing,"purchase_unit":"g","quantity_ordered":10.0,"unit_cost":5}]}))
        .to_request()).await).await;
    let lid = po.lines[0].id;
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri(&format!("/purchasing/orders/{}/receive", po.order.id)).insert_header(auth.clone())
        .set_json(serde_json::json!({"lines":[{"line_id":lid,"quantity_received":5.0},{"line_id":lid,"quantity_received":5.0}]})).to_request()).await;
    assert_eq!(resp.status(), 400, "duplicate line_id in one receive must be rejected");
}
