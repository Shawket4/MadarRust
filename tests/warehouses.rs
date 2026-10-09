//! Warehouses and stock transfers (WAREHOUSE_DESIGN.md): the transfer
//! lifecycle through the ledger, who may act on which side, the selling
//! guard, replenishment, and a branch's kind and the warehouse limit.
use actix_web::{App, test, web};
use madar_inventory::api::{BranchKind, ReplenishmentRow, StockTransfer, TransferDifferenceRow};
use madar_inventory::transfer::TransferStatus;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::JwtSecret;
use madar_rust::models::UserRole;

fn secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn token(user: Uuid, org: Uuid, role: UserRole) -> String {
    madar_rust::auth::jwt::create_token(&secret(), user, Some(org), role, None, 24).unwrap()
}

async fn org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(id)
        .bind(format!("org-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn location(pool: &PgPool, org: Uuid, kind: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO branches (id, org_id, name, kind) VALUES ($1, $2, $3, $4::branch_kind)",
    )
    .bind(id)
    .bind(org)
    .bind(format!("{kind} {id}"))
    .bind(kind)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, 'U', $3, 'hash', $4::user_role)",
    )
    .bind(id)
    .bind(org)
    .bind(format!("u-{id}@test.com"))
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn ingredient(pool: &PgPool, org: Uuid, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO org_ingredients (id, org_id, name, unit, category_id, cost_per_unit) \
         VALUES ($1, $2, $3, 'kg', ingredient_category_id($2, 'general'), 5)",
    )
    .bind(id)
    .bind(org)
    .bind(name)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Stock arrives the only way it can: through the ledger, then a known cost.
async fn stock(pool: &PgPool, branch: Uuid, ing: Uuid, qty: f64, cost: Option<f64>) {
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
    if let Some(c) = cost {
        sqlx::query("UPDATE branch_stock SET cost_per_unit = $3 WHERE branch_id = $1 AND org_ingredient_id = $2")
            .bind(branch)
            .bind(ing)
            .bind(c)
            .execute(pool)
            .await
            .unwrap();
    }
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

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .configure(madar_rust::inventory::routes::configure)
                .configure(madar_rust::branches::routes::configure),
        )
        .await
    };
}

macro_rules! call {
    ($app:expr, $method:ident, $uri:expr, $tok:expr) => {
        test::call_service(
            &$app,
            test::TestRequest::$method()
                .uri(&*$uri)
                .insert_header(("Authorization", format!("Bearer {}", $tok)))
                .to_request(),
        )
        .await
    };
    ($app:expr, $method:ident, $uri:expr, $tok:expr, $body:expr) => {
        test::call_service(
            &$app,
            test::TestRequest::$method()
                .uri(&*$uri)
                .insert_header(("Authorization", format!("Bearer {}", $tok)))
                .set_json($body)
                .to_request(),
        )
        .await
    };
}

struct World {
    org: Uuid,
    wh: Uuid,
    shop: Uuid,
    beans: Uuid,
    owner: String,
}

async fn world(pool: &PgPool) -> World {
    let org = org(pool).await;
    let wh = location(pool, org, "warehouse").await;
    let shop = location(pool, org, "branch").await;
    let beans = ingredient(pool, org, "Beans").await;
    stock(pool, wh, beans, 20.0, Some(10.0)).await;
    let owner = user(pool, org, "org_admin").await;
    World {
        org,
        wh,
        shop,
        beans,
        owner: token(owner, org, UserRole::OrgAdmin),
    }
}

/// A draft of `qty` beans from the warehouse to the shop, by the owner.
macro_rules! draft {
    ($app:expr, $w:expr, $qty:expr) => {{
        let resp = call!($app, post, "/inventory/transfers", $w.owner, json!({
            "source_branch_id": $w.wh, "destination_branch_id": $w.shop,
            "lines": [{ "org_ingredient_id": $w.beans, "quantity": $qty }]
        }));
        assert_eq!(resp.status(), 201);
        let t: StockTransfer = test::read_body_json(resp).await;
        t
    }};
}

#[sqlx::test]
async fn dispatch_then_short_receive_moves_stock_through_the_ledger(pool: PgPool) {
    let app = app!(pool);
    let w = world(&pool).await;

    let t = draft!(app, w, 10.0);
    assert_eq!(t.status, TransferStatus::Draft);
    assert_eq!(t.reference, "TR-1");
    assert_eq!(
        (t.source_kind, t.destination_kind),
        (BranchKind::Warehouse, BranchKind::Branch)
    );
    assert_eq!(
        on_hand(&pool, w.wh, w.beans).await,
        20.0,
        "a draft moves nothing"
    );

    let resp = call!(
        app,
        post,
        format!("/inventory/transfers/{}/dispatch", t.id),
        w.owner
    );
    assert_eq!(resp.status(), 200);
    let t: StockTransfer = test::read_body_json(resp).await;
    assert_eq!(t.status, TransferStatus::Dispatched);
    assert_eq!(t.lines[0].unit_cost, Some(10.0), "cost frozen at dispatch");
    assert_eq!(on_hand(&pool, w.wh, w.beans).await, 10.0);
    assert_eq!(
        on_hand(&pool, w.shop, w.beans).await,
        0.0,
        "in transit is nowhere"
    );

    // Two broken bags: received short.
    let resp = call!(
        app,
        post,
        format!("/inventory/transfers/{}/receive", t.id),
        w.owner,
        json!({
            "lines": [{ "line_id": t.lines[0].id, "qty_received": 8.0, "note": "2 bags torn" }]
        })
    );
    assert_eq!(resp.status(), 200);
    let t: StockTransfer = test::read_body_json(resp).await;
    assert_eq!(t.status, TransferStatus::Received);
    assert_eq!(t.lines[0].qty_received, Some(8.0));
    assert_eq!(
        on_hand(&pool, w.wh, w.beans).await,
        10.0,
        "the source lost the full send"
    );
    assert_eq!(on_hand(&pool, w.shop, w.beans).await, 8.0);
    let shop_cost: f64 = sqlx::query_scalar(
        "SELECT cost_per_unit::float8 FROM branch_stock WHERE branch_id = $1 AND org_ingredient_id = $2",
    )
    .bind(w.shop)
    .bind(w.beans)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(shop_cost, 10.0, "cost travels with the goods");

    // Final: nothing more can happen to it.
    let resp = call!(
        app,
        post,
        format!("/inventory/transfers/{}/cancel", t.id),
        w.owner,
        json!({})
    );
    assert_eq!(resp.status(), 409);

    let resp = call!(
        app,
        get,
        format!("/inventory/orgs/{}/transfer-differences", w.org),
        w.owner
    );
    assert_eq!(resp.status(), 200);
    let diffs: Vec<TransferDifferenceRow> = test::read_body_json(resp).await;
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].difference, -2.0);
    assert_eq!(diffs[0].value_difference, Some(-20));
}

#[sqlx::test]
async fn dispatch_beyond_on_hand_is_refused_and_moves_nothing(pool: PgPool) {
    let app = app!(pool);
    let w = world(&pool).await;
    let t = draft!(app, w, 25.0);
    let resp = call!(
        app,
        post,
        format!("/inventory/transfers/{}/dispatch", t.id),
        w.owner
    );
    assert_eq!(resp.status(), 409);
    assert_eq!(on_hand(&pool, w.wh, w.beans).await, 20.0);
    let status: String =
        sqlx::query_scalar("SELECT status::text FROM stock_transfers WHERE id = $1")
            .bind(t.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "draft");
}

#[sqlx::test]
async fn cancelling_in_transit_returns_the_stock(pool: PgPool) {
    let app = app!(pool);
    let w = world(&pool).await;
    let t = draft!(app, w, 6.0);
    call!(
        app,
        post,
        format!("/inventory/transfers/{}/dispatch", t.id),
        w.owner
    );
    assert_eq!(on_hand(&pool, w.wh, w.beans).await, 14.0);
    let resp = call!(
        app,
        post,
        format!("/inventory/transfers/{}/cancel", t.id),
        w.owner,
        json!({ "note": "truck broke down" })
    );
    assert_eq!(resp.status(), 200);
    let t: StockTransfer = test::read_body_json(resp).await;
    assert_eq!(t.status, TransferStatus::Cancelled);
    assert_eq!(t.note.as_deref(), Some("truck broke down"));
    assert_eq!(on_hand(&pool, w.wh, w.beans).await, 20.0);
    assert_eq!(on_hand(&pool, w.shop, w.beans).await, 0.0);
}

#[sqlx::test]
async fn over_receive_needs_a_note_and_every_line_once(pool: PgPool) {
    let app = app!(pool);
    let w = world(&pool).await;
    let t = draft!(app, w, 5.0);
    call!(
        app,
        post,
        format!("/inventory/transfers/{}/dispatch", t.id),
        w.owner
    );
    let uri = format!("/inventory/transfers/{}/receive", t.id);
    let line = t.lines[0].id;

    let resp = call!(app, post, uri, w.owner, json!({ "lines": [] }));
    assert_eq!(resp.status(), 400, "every line must be answered");
    let resp = call!(
        app,
        post,
        uri,
        w.owner,
        json!({ "lines": [{ "line_id": line, "qty_received": 6.0 }] })
    );
    assert_eq!(resp.status(), 400, "over without a note");
    assert_eq!(on_hand(&pool, w.shop, w.beans).await, 0.0);
    let resp = call!(
        app,
        post,
        uri,
        w.owner,
        json!({ "lines": [{ "line_id": line, "qty_received": 6.0, "note": "miscounted" }] })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(on_hand(&pool, w.shop, w.beans).await, 6.0);
}

/// A branch manager asks; only someone at the warehouse answers and sends.
#[sqlx::test]
async fn requests_are_answered_by_the_sending_side(pool: PgPool) {
    let app = app!(pool);
    let w = world(&pool).await;
    // A migrated test DB carries no role defaults (the seeder runs at boot):
    // give managers what the template gives them (`inventory.transfers.*` "om").
    for action in ["create", "read", "update"] {
        sqlx::query(
            "INSERT INTO role_permissions (role, resource, action, granted) \
             VALUES ('branch_manager', 'inventory_transfers', $1::permission_action, true) ON CONFLICT DO NOTHING",
        )
        .bind(action)
        .execute(&pool)
        .await
        .unwrap();
    }
    let mgr = user(&pool, w.org, "branch_manager").await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(mgr)
        .bind(w.shop)
        .execute(&pool)
        .await
        .unwrap();
    let mgr = token(mgr, w.org, UserRole::BranchManager);

    let resp = call!(
        app,
        post,
        "/inventory/transfers",
        mgr,
        json!({
            "source_branch_id": w.wh, "destination_branch_id": w.shop, "request": true,
            "lines": [{ "org_ingredient_id": w.beans, "quantity": 4.0 }]
        })
    );
    assert_eq!(resp.status(), 201);
    let t: StockTransfer = test::read_body_json(resp).await;
    assert_eq!(t.status, TransferStatus::Requested);
    assert!(t.requested.is_some());

    // The manager doesn't work at the warehouse: can't send without a request,
    // can't accept their own request.
    let resp = call!(
        app,
        post,
        "/inventory/transfers",
        mgr,
        json!({
            "source_branch_id": w.wh, "destination_branch_id": w.shop,
            "lines": [{ "org_ingredient_id": w.beans, "quantity": 1.0 }]
        })
    );
    assert_eq!(resp.status(), 403);
    let resp = call!(
        app,
        post,
        format!("/inventory/transfers/{}/accept", t.id),
        mgr,
        json!({})
    );
    assert_eq!(resp.status(), 403);

    // Declining needs a reason.
    let resp = call!(
        app,
        post,
        format!("/inventory/transfers/{}/decline", t.id),
        w.owner,
        json!({})
    );
    assert_eq!(resp.status(), 400);

    // The owner accepts with fewer, then sends; the manager receives.
    let resp = call!(
        app,
        post,
        format!("/inventory/transfers/{}/accept", t.id),
        w.owner,
        json!({
            "lines": [{ "org_ingredient_id": w.beans, "quantity": 3.0 }]
        })
    );
    assert_eq!(resp.status(), 200);
    let t: StockTransfer = test::read_body_json(resp).await;
    assert_eq!(
        (t.status, t.lines[0].qty_sent),
        (TransferStatus::Draft, 3.0)
    );
    let resp = call!(
        app,
        post,
        format!("/inventory/transfers/{}/dispatch", t.id),
        mgr
    );
    assert_eq!(resp.status(), 403, "dispatch is the warehouse's");
    call!(
        app,
        post,
        format!("/inventory/transfers/{}/dispatch", t.id),
        w.owner
    );
    let resp = call!(
        app,
        post,
        format!("/inventory/transfers/{}/receive", t.id),
        mgr,
        json!({
            "lines": [{ "line_id": t.lines[0].id, "qty_received": 3.0 }]
        })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(on_hand(&pool, w.shop, w.beans).await, 3.0);

    // The branch's list shows it; its status filter works.
    let resp = call!(
        app,
        get,
        format!("/inventory/branches/{}/transfers?status=received", w.shop),
        mgr
    );
    let rows: Vec<StockTransfer> = test::read_body_json(resp).await;
    assert_eq!(rows.len(), 1);
}

#[sqlx::test]
async fn a_warehouse_never_sells(pool: PgPool) {
    let w = world(&pool).await;
    let err =
        sqlx::query("INSERT INTO devices (id, org_id, branch_id, code) VALUES ($1, $2, $3, 'D1')")
            .bind(Uuid::new_v4())
            .bind(w.org)
            .bind(w.wh)
            .execute(&pool)
            .await
            .unwrap_err();
    assert!(err.to_string().contains("WAREHOUSE_CANNOT_SELL"), "{err}");
    let err =
        sqlx::query("INSERT INTO branch_tables (org_id, branch_id, label) VALUES ($1, $2, 'T1')")
            .bind(w.org)
            .bind(w.wh)
            .execute(&pool)
            .await
            .unwrap_err();
    assert!(err.to_string().contains("WAREHOUSE_CANNOT_SELL"), "{err}");
    // A branch still can.
    sqlx::query("INSERT INTO branch_tables (org_id, branch_id, label) VALUES ($1, $2, 'T1')")
        .bind(w.org)
        .bind(w.shop)
        .execute(&pool)
        .await
        .unwrap();
}

#[sqlx::test]
async fn replenishment_fills_to_par_from_what_the_warehouse_has(pool: PgPool) {
    let app = app!(pool);
    let w = world(&pool).await;
    stock(&pool, w.shop, w.beans, 2.0, None).await;
    sqlx::query("UPDATE branch_stock SET par_min = 5, par_max = 30 WHERE branch_id = $1 AND org_ingredient_id = $2")
        .bind(w.shop)
        .bind(w.beans)
        .execute(&pool)
        .await
        .unwrap();
    let uri = format!(
        "/inventory/warehouses/{}/replenishment?branch_id={}",
        w.wh, w.shop
    );
    let resp = call!(app, get, uri, w.owner);
    assert_eq!(resp.status(), 200);
    let rows: Vec<ReplenishmentRow> = test::read_body_json(resp).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].need, rows[0].available, rows[0].suggested),
        (28.0, 20.0, 20.0)
    );

    // A draft to the branch counts as coming, and claims the warehouse's stock.
    draft!(app, w, 15.0);
    let resp = call!(app, get, uri, w.owner);
    let rows: Vec<ReplenishmentRow> = test::read_body_json(resp).await;
    assert_eq!(
        (rows[0].need, rows[0].available, rows[0].suggested),
        (13.0, 5.0, 5.0)
    );

    // Only a warehouse replenishes.
    let resp = call!(
        app,
        get,
        format!(
            "/inventory/warehouses/{}/replenishment?branch_id={}",
            w.shop, w.wh
        ),
        w.owner
    );
    assert_eq!(resp.status(), 400);
}

#[sqlx::test]
async fn warehouse_limit_and_kind_change(pool: PgPool) {
    let app = app!(pool);
    let w = world(&pool).await;
    sqlx::query("UPDATE organizations SET max_warehouses = 1 WHERE id = $1")
        .bind(w.org)
        .execute(&pool)
        .await
        .unwrap();

    let resp = call!(
        app,
        post,
        "/branches",
        w.owner,
        json!({ "org_id": w.org, "name": "WH 2", "kind": "warehouse" })
    );
    assert_eq!(resp.status(), 409, "the org already has its one warehouse");

    // Lift the limit; a branch with a paired device can't become a warehouse.
    sqlx::query("UPDATE organizations SET max_warehouses = NULL WHERE id = $1")
        .bind(w.org)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO devices (id, org_id, branch_id, code) VALUES ($1, $2, $3, 'D1')")
        .bind(Uuid::new_v4())
        .bind(w.org)
        .bind(w.shop)
        .execute(&pool)
        .await
        .unwrap();
    let resp = call!(
        app,
        patch,
        format!("/branches/{}", w.shop),
        w.owner,
        json!({ "kind": "warehouse" })
    );
    assert_eq!(resp.status(), 409);
    sqlx::query("UPDATE devices SET retired_at = now() WHERE branch_id = $1")
        .bind(w.shop)
        .execute(&pool)
        .await
        .unwrap();
    // Pickup still on: online orders would land at a warehouse.
    sqlx::query(
        "INSERT INTO branch_delivery_settings (branch_id, pickup_enabled) VALUES ($1, true)",
    )
    .bind(w.shop)
    .execute(&pool)
    .await
    .unwrap();
    let resp = call!(
        app,
        patch,
        format!("/branches/{}", w.shop),
        w.owner,
        json!({ "kind": "warehouse" })
    );
    assert_eq!(resp.status(), 409);
    sqlx::query("UPDATE branch_delivery_settings SET pickup_enabled = false WHERE branch_id = $1")
        .bind(w.shop)
        .execute(&pool)
        .await
        .unwrap();
    let resp = call!(
        app,
        patch,
        format!("/branches/{}", w.shop),
        w.owner,
        json!({ "kind": "warehouse" })
    );
    assert_eq!(resp.status(), 200);

    // And back; the list filters by kind.
    let resp = call!(
        app,
        patch,
        format!("/branches/{}", w.wh),
        w.owner,
        json!({ "kind": "branch" })
    );
    assert_eq!(resp.status(), 200);
    let resp = call!(
        app,
        get,
        format!("/branches?org_id={}&kind=warehouse", w.org),
        w.owner
    );
    let rows: Vec<serde_json::Value> = test::read_body_json(resp).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], json!(w.shop));
    assert_eq!(rows[0]["kind"], json!("warehouse"));

    // A client that predates warehouses (a POS picking its branch) asks
    // without the flag and sees only branches; the dashboards ask for both.
    let ids = |rows: Vec<serde_json::Value>| {
        let mut v: Vec<String> = rows
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect();
        v.sort();
        v
    };
    let resp = call!(app, get, format!("/branches?org_id={}", w.org), w.owner);
    assert_eq!(
        ids(test::read_body_json(resp).await),
        vec![w.wh.to_string()]
    );
    let resp = call!(
        app,
        get,
        format!("/branches?org_id={}&include_warehouses=true", w.org),
        w.owner
    );
    let mut both = vec![w.wh.to_string(), w.shop.to_string()];
    both.sort();
    assert_eq!(ids(test::read_body_json(resp).await), both);
}

/// Someone who works at one shop picks the other side from every location,
/// but "all locations" lists only transfers touching where they work.
#[sqlx::test]
async fn a_shop_manager_sees_every_location_and_only_their_transfers(pool: PgPool) {
    let app = app!(pool);
    let w = world(&pool).await;
    let other = location(&pool, w.org, "branch").await;
    for action in ["create", "read"] {
        sqlx::query(
            "INSERT INTO role_permissions (role, resource, action, granted) \
             VALUES ('branch_manager', 'inventory_transfers', $1::permission_action, true) ON CONFLICT DO NOTHING",
        )
        .bind(action)
        .execute(&pool)
        .await
        .unwrap();
    }
    let mgr = user(&pool, w.org, "branch_manager").await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(mgr)
        .bind(w.shop)
        .execute(&pool)
        .await
        .unwrap();
    let mgr = token(mgr, w.org, UserRole::BranchManager);

    let resp = call!(
        app,
        get,
        format!("/inventory/orgs/{}/transfer-locations", w.org),
        mgr
    );
    assert_eq!(resp.status(), 200);
    let places: Vec<serde_json::Value> = test::read_body_json(resp).await;
    let ids: Vec<String> = places
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_string())
        .collect();
    for id in [w.wh, w.shop, other] {
        assert!(ids.contains(&id.to_string()), "{places:?}");
    }

    let mine = draft!(app, w, 2.0);
    let resp = call!(
        app,
        post,
        "/inventory/transfers",
        w.owner,
        json!({
            "source_branch_id": w.wh, "destination_branch_id": other,
            "lines": [{ "org_ingredient_id": w.beans, "quantity": 1.0 }]
        })
    );
    assert_eq!(resp.status(), 201);
    let resp = call!(
        app,
        get,
        format!("/inventory/branches/{}/transfers", Uuid::nil()),
        mgr
    );
    assert_eq!(resp.status(), 200);
    let seen: Vec<StockTransfer> = test::read_body_json(resp).await;
    assert_eq!(seen.iter().map(|t| t.id).collect::<Vec<_>>(), vec![mine.id]);
    let resp = call!(
        app,
        get,
        format!("/inventory/branches/{}/transfers", Uuid::nil()),
        w.owner
    );
    let all: Vec<StockTransfer> = test::read_body_json(resp).await;
    assert_eq!(all.len(), 2, "the owner sees the whole org");
}

#[sqlx::test]
async fn an_ingredient_on_an_open_transfer_cannot_be_deleted(pool: PgPool) {
    let app = app!(pool);
    let w = world(&pool).await;
    let t = draft!(app, w, 20.0);
    let resp = call!(
        app,
        post,
        format!("/inventory/transfers/{}/dispatch", t.id),
        w.owner,
        json!({})
    );
    assert_eq!(resp.status(), 200);
    // The warehouse is empty now; the beans are on the road.
    let resp = call!(
        app,
        delete,
        format!("/inventory/orgs/{}/catalog/{}", w.org, w.beans),
        w.owner
    );
    assert_eq!(resp.status(), 409);
}
