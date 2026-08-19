//! Held orders + occupancy + transfer waitlist tests: the park/claim/re-park
//! cross-device walk, offline-first table-conflict semantics, tombstone sync,
//! the atomic swap across entity kinds, the ticket-side arbitration, and the
//! transfer queue lifecycle (incl. auto-fulfil and auto-cancel).

use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::{JwtSecret, create_token};
use crate::held_orders::{
    HeldOrderParkResponse, HeldOrderView, HeldOrdersSyncResponse, TransferView,
};
use crate::models::UserRole;
use crate::realtime::hub::BranchEventHub;
use crate::tickets::OpenTicketView;

fn secret() -> JwtSecret {
    JwtSecret("secret".into())
}
fn token(uid: Uuid, org: Uuid, role: UserRole) -> String {
    create_token(&secret(), uid, Some(org), role, None, 24).unwrap()
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
async fn seed_branch(pool: &PgPool, org: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Branch')")
        .bind(id)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn seed_user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, $3, $4, 'h', $5::user_role)",
    )
    .bind(id)
    .bind(org)
    .bind(format!("{role}-{id}"))
    .bind(format!("{id}@t.com"))
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    id
}
async fn seed_menu_item(pool: &PgPool, org: Uuid, price: i32) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO menu_items (id, org_id, name, base_price) VALUES ($1, $2, 'Burger', $3)",
    )
    .bind(id)
    .bind(org)
    .bind(price)
    .execute(pool)
    .await
    .unwrap();
    id
}
async fn seed_section(pool: &PgPool, org: Uuid, branch: Uuid, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO floor_sections (id, org_id, branch_id, name) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(org)
        .bind(branch)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn seed_table(
    pool: &PgPool,
    org: Uuid,
    branch: Uuid,
    section: Option<Uuid>,
    label: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO branch_tables (id, org_id, branch_id, section_id, label) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(org)
    .bind(branch)
    .bind(section)
    .bind(label)
    .execute(pool)
    .await
    .unwrap();
    id
}
async fn table_status(pool: &PgPool, table: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM branch_tables WHERE id = $1")
        .bind(table)
        .fetch_one(pool)
        .await
        .unwrap()
}
/// An open shift, returning its id (the settle path needs one to bank into).
async fn open_shift_row(pool: &PgPool, branch: Uuid, teller: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO shifts (branch_id, teller_id, status, opening_cash) \
         VALUES ($1,$2,'open',0) RETURNING id",
    )
    .bind(branch)
    .bind(teller)
    .fetch_one(pool)
    .await
    .unwrap()
}
async fn seed_cash_method(pool: &PgPool, org: Uuid) {
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, color, icon, is_cash, is_active) \
         VALUES ($1, 'cash', '#000', 'cash', true, true)",
    )
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
}
async fn shift_row(pool: &PgPool, branch: Uuid, teller: Uuid) {
    sqlx::query(
        "INSERT INTO shifts (branch_id, teller_id, status, opening_cash) VALUES ($1,$2,'open',0)",
    )
    .bind(branch)
    .bind(teller)
    .execute(pool)
    .await
    .unwrap();
}
async fn grant(pool: &PgPool, role: &str, resource: &str, action: &str) {
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ($1::user_role, $2::permission_resource, $3::permission_action, true) ON CONFLICT DO NOTHING",
    )
    .bind(role).bind(resource).bind(action).execute(pool).await.unwrap();
}
async fn grant_defaults(pool: &PgPool) {
    for (resource, action) in [
        ("held_orders", "create"),
        ("held_orders", "read"),
        ("held_orders", "update"),
        ("table_transfers", "create"),
        ("table_transfers", "read"),
        ("table_transfers", "update"),
        ("open_tickets", "read"),
        ("open_tickets", "update"),
        // The host ops the seeder really grants a teller — table state included,
        // which is how a checked-out table gets cleared from the POS.
        ("floor_plan", "read"),
        ("reservations", "read"),
        ("reservations", "update"),
    ] {
        grant(pool, "teller", resource, action).await;
    }
    for (resource, action) in [
        ("open_tickets", "create"),
        ("open_tickets", "read"),
        ("open_tickets", "update"),
        ("table_transfers", "create"),
        ("table_transfers", "read"),
        ("table_transfers", "update"),
        ("held_orders", "read"),
    ] {
        grant(pool, "waiter", resource, action).await;
    }
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(BranchEventHub::new()))
                .configure(crate::held_orders::routes::configure)
                .configure(crate::tickets::routes::configure)
                .configure(crate::sync::routes::configure),
        )
        .await
    };
}

macro_rules! post_json {
    ($app:expr, $tok:expr, $uri:expr, $body:expr) => {
        test::call_service(
            &$app,
            test::TestRequest::post()
                .uri($uri)
                .insert_header(("Authorization", format!("Bearer {}", $tok)))
                .set_json(&$body)
                .to_request(),
        )
        .await
    };
}
macro_rules! get_req {
    ($app:expr, $tok:expr, $uri:expr) => {
        test::call_service(
            &$app,
            test::TestRequest::get()
                .uri($uri)
                .insert_header(("Authorization", format!("Bearer {}", $tok)))
                .to_request(),
        )
        .await
    };
}

fn cart() -> serde_json::Value {
    serde_json::json!({ "lines": [{ "item_id": "latte", "qty": 2 }], "discount_id": null })
}

// ── Park / claim / re-park across devices ────────────────────────────────────

#[sqlx::test]
async fn park_claim_and_repark_across_devices(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    grant_defaults(&pool).await;
    let t = token(teller, org, UserRole::Teller);

    // Park from till A.
    let id = Uuid::new_v4();
    let resp = post_json!(
        app,
        t,
        "/held-orders",
        serde_json::json!({
            "id": id, "branch_id": branch, "name": "Sara", "cart": cart(), "device_id": "till-a"
        })
    );
    assert_eq!(resp.status(), 200);
    let parked: HeldOrderParkResponse = test::read_body_json(resp).await;
    assert_eq!(parked.held_order.status, "held");
    assert_eq!(parked.held_order.cart, cart(), "cart round-trips verbatim");
    assert!(!parked.table_conflict);

    // The branch list shows it.
    let resp = get_req!(app, t, &format!("/held-orders?branch_id={branch}"));
    assert_eq!(resp.status(), 200);
    let list: HeldOrdersSyncResponse = test::read_body_json(resp).await;
    assert_eq!(list.held_orders.len(), 1);

    // Till B claims it (resume).
    let resp = post_json!(
        app,
        t,
        &format!("/held-orders/{id}/claim"),
        serde_json::json!({ "device_id": "till-b" })
    );
    assert_eq!(resp.status(), 200);
    let claimed: HeldOrderView = test::read_body_json(resp).await;
    assert_eq!(claimed.status, "resumed");
    assert_eq!(claimed.claimed_by_device.as_deref(), Some("till-b"));

    // Till A can't claim (or re-park) while B holds it…
    let resp = post_json!(
        app,
        t,
        &format!("/held-orders/{id}/claim"),
        serde_json::json!({ "device_id": "till-a" })
    );
    assert_eq!(resp.status(), 409, "second till may not steal silently");
    let resp = post_json!(
        app,
        t,
        "/held-orders",
        serde_json::json!({
            "id": id, "branch_id": branch, "name": "Sara", "cart": cart(), "device_id": "till-a"
        })
    );
    assert_eq!(
        resp.status(),
        409,
        "re-park from a non-claiming device conflicts"
    );

    // …but a force claim recovers a dead till.
    let resp = post_json!(
        app,
        t,
        &format!("/held-orders/{id}/claim"),
        serde_json::json!({ "device_id": "till-a", "force": true })
    );
    assert_eq!(resp.status(), 200);

    // Till A re-parks with an edited cart → back to held, revision advanced.
    let new_cart = serde_json::json!({ "lines": [{ "item_id": "latte", "qty": 3 }] });
    let resp = post_json!(
        app,
        t,
        "/held-orders",
        serde_json::json!({
            "id": id, "branch_id": branch, "name": "Sara+", "cart": new_cart, "device_id": "till-a"
        })
    );
    assert_eq!(resp.status(), 200);
    let reparked: HeldOrderParkResponse = test::read_body_json(resp).await;
    assert_eq!(reparked.held_order.status, "held");
    assert_eq!(reparked.held_order.name, "Sara+");
    assert_eq!(reparked.held_order.cart, new_cart);
    assert!(reparked.held_order.revision > claimed.revision);

    // Release is the "never mind" path: claim again, release, still held.
    let resp = post_json!(
        app,
        t,
        &format!("/held-orders/{id}/claim"),
        serde_json::json!({ "device_id": "till-b" })
    );
    assert_eq!(resp.status(), 200);
    let resp = post_json!(
        app,
        t,
        &format!("/held-orders/{id}/release"),
        serde_json::json!({ "device_id": "till-b" })
    );
    assert_eq!(resp.status(), 200);
    let released: HeldOrderView = test::read_body_json(resp).await;
    assert_eq!(released.status, "held");
    assert!(released.claimed_by_device.is_none());
}

// ── Tables: park-with-table, conflicts, assignment choreography ──────────────

#[sqlx::test]
async fn park_with_table_seats_it_and_conflicts_drop_the_table(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    grant_defaults(&pool).await;
    let t = token(teller, org, UserRole::Teller);
    let t1 = seed_table(&pool, org, branch, None, "T1").await;
    let t2 = seed_table(&pool, org, branch, None, "T2").await;

    // Park A onto T1 → assigned + seated.
    let a = Uuid::new_v4();
    let resp = post_json!(
        app,
        t,
        "/held-orders",
        serde_json::json!({
            "id": a, "branch_id": branch, "name": "A", "cart": cart(), "table_id": t1
        })
    );
    assert_eq!(resp.status(), 200);
    let parked: HeldOrderParkResponse = test::read_body_json(resp).await;
    assert_eq!(parked.held_order.table_id, Some(t1));
    assert_eq!(parked.held_order.table_label.as_deref(), Some("T1"));
    assert!(!parked.table_conflict);
    assert_eq!(table_status(&pool, t1).await, "seated");

    // Park B onto the SAME table → the park SUCCEEDS, the table is dropped.
    let b = Uuid::new_v4();
    let resp = post_json!(
        app,
        t,
        "/held-orders",
        serde_json::json!({
            "id": b, "branch_id": branch, "name": "B", "cart": cart(), "table_id": t1
        })
    );
    assert_eq!(
        resp.status(),
        200,
        "offline-first: data wins, position loses"
    );
    let parked: HeldOrderParkResponse = test::read_body_json(resp).await;
    assert!(parked.table_conflict);
    assert_eq!(parked.held_order.table_id, None);

    // Interactive assign of B onto T1 → loud 409; onto free T2 → seated.
    let resp = post_json!(
        app,
        t,
        &format!("/held-orders/{b}/table"),
        serde_json::json!({ "table_id": t1 })
    );
    assert_eq!(resp.status(), 409);
    let resp = post_json!(
        app,
        t,
        &format!("/held-orders/{b}/table"),
        serde_json::json!({ "table_id": t2 })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(table_status(&pool, t2).await, "seated");

    // Moving A off T1 (unassign) buses it.
    let resp = post_json!(
        app,
        t,
        &format!("/held-orders/{a}/table"),
        serde_json::json!({ "table_id": null })
    );
    assert_eq!(resp.status(), 200);
    let view: HeldOrderView = test::read_body_json(resp).await;
    assert_eq!(view.table_id, None);
    assert_eq!(table_status(&pool, t1).await, "free");

    // A foreign/unknown table id on park is dropped too (stale layout).
    let c = Uuid::new_v4();
    let resp = post_json!(
        app,
        t,
        "/held-orders",
        serde_json::json!({
            "id": c, "branch_id": branch, "name": "C", "cart": cart(), "table_id": Uuid::new_v4()
        })
    );
    assert_eq!(resp.status(), 200);
    let parked: HeldOrderParkResponse = test::read_body_json(resp).await;
    assert!(parked.table_conflict);
    assert_eq!(parked.held_order.table_id, None);
}

#[sqlx::test]
async fn tombstones_free_tables_and_sync_by_cursor(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    grant_defaults(&pool).await;
    let t = token(teller, org, UserRole::Teller);
    let t1 = seed_table(&pool, org, branch, None, "T1").await;

    let a = Uuid::new_v4();
    let resp = post_json!(
        app,
        t,
        "/held-orders",
        serde_json::json!({
            "id": a, "branch_id": branch, "name": "A", "cart": cart(), "table_id": t1
        })
    );
    assert_eq!(resp.status(), 200);

    // Cursor BEFORE the discard.
    let resp = get_req!(app, t, &format!("/held-orders?branch_id={branch}"));
    let before: HeldOrdersSyncResponse = test::read_body_json(resp).await;
    assert_eq!(before.held_orders.len(), 1);

    // Discard → tombstone + the table goes straight back to the room (no
    // party ever sat: nothing to bus).
    let resp = post_json!(
        app,
        t,
        &format!("/held-orders/{a}/discard"),
        serde_json::json!({})
    );
    assert_eq!(resp.status(), 200);
    let view: HeldOrderView = test::read_body_json(resp).await;
    assert_eq!(view.status, "discarded");
    assert_eq!(view.table_id, None);
    assert_eq!(table_status(&pool, t1).await, "free");

    // Discard again → idempotent; complete after discard → conflict.
    let resp = post_json!(
        app,
        t,
        &format!("/held-orders/{a}/discard"),
        serde_json::json!({})
    );
    assert_eq!(resp.status(), 200);
    let resp = post_json!(
        app,
        t,
        &format!("/held-orders/{a}/complete"),
        serde_json::json!({})
    );
    assert_eq!(resp.status(), 409);

    // The live board no longer shows it; the `since` pull carries the tombstone.
    let resp = get_req!(app, t, &format!("/held-orders?branch_id={branch}"));
    let live: HeldOrdersSyncResponse = test::read_body_json(resp).await;
    assert!(live.held_orders.is_empty());
    let since = before
        .server_time
        .to_rfc3339()
        .replace('+', "%2B")
        .replace(':', "%3A");
    let resp = get_req!(
        app,
        t,
        &format!("/held-orders?branch_id={branch}&since={since}")
    );
    assert_eq!(resp.status(), 200);
    let synced: HeldOrdersSyncResponse = test::read_body_json(resp).await;
    assert_eq!(synced.held_orders.len(), 1);
    assert_eq!(synced.held_orders[0].status, "discarded");

    // Complete walks the same tombstone path (with the order link), but a
    // CHECKOUT leaves the table needing a bus — `dirty`, not `free`. Only a
    // human (the POS prompt, or the tables screen) hands it back to the room.
    let b = Uuid::new_v4();
    post_json!(
        app,
        t,
        "/held-orders",
        serde_json::json!({
            "id": b, "branch_id": branch, "name": "B", "cart": cart(),
            "table_id": t1
        })
    );
    assert_eq!(table_status(&pool, t1).await, "seated");
    let resp = post_json!(
        app,
        t,
        &format!("/held-orders/{b}/complete"),
        serde_json::json!({})
    );
    assert_eq!(resp.status(), 200);
    let view: HeldOrderView = test::read_body_json(resp).await;
    assert_eq!(view.status, "completed");
    assert_eq!(table_status(&pool, t1).await, "dirty");
    // …and the teller clearing it by hand is what makes it available again.
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/floor/tables/{t1}/state"))
            .insert_header(("Authorization", format!("Bearer {t}")))
            .set_json(&serde_json::json!({ "status": "free" }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(table_status(&pool, t1).await, "free");
    // Idempotent replay of the complete.
    let resp = post_json!(
        app,
        t,
        &format!("/held-orders/{b}/complete"),
        serde_json::json!({})
    );
    assert_eq!(resp.status(), 200);
}

// ── Swap: held×held, held×empty, held×ticket, permission boundary ────────────

#[sqlx::test]
async fn swap_covers_moves_and_cross_entity_exchanges(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let item = seed_menu_item(&pool, org, 1000).await;
    shift_row(&pool, branch, teller).await;
    grant_defaults(&pool).await;
    let t = token(teller, org, UserRole::Teller);
    let w = token(waiter, org, UserRole::Waiter);
    let t1 = seed_table(&pool, org, branch, None, "T1").await;
    let t2 = seed_table(&pool, org, branch, None, "T2").await;
    let t3 = seed_table(&pool, org, branch, None, "T3").await;
    let t4 = seed_table(&pool, org, branch, None, "T4").await;

    // Two held orders on T1/T2.
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    post_json!(
        app,
        t,
        "/held-orders",
        serde_json::json!({
        "id": a, "branch_id": branch, "name": "A", "cart": cart(), "table_id": t1 })
    );
    post_json!(
        app,
        t,
        "/held-orders",
        serde_json::json!({
        "id": b, "branch_id": branch, "name": "B", "cart": cart(), "table_id": t2 })
    );

    // Swap the two held orders.
    let resp = post_json!(
        app,
        t,
        "/floor/tables/swap",
        serde_json::json!({ "branch_id": branch, "table_a": t1, "table_b": t2 })
    );
    assert_eq!(resp.status(), 200);
    let ta: Option<Uuid> = sqlx::query_scalar("SELECT table_id FROM held_orders WHERE id=$1")
        .bind(a)
        .fetch_one(&pool)
        .await
        .unwrap();
    let tb: Option<Uuid> = sqlx::query_scalar("SELECT table_id FROM held_orders WHERE id=$1")
        .bind(b)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        (ta, tb),
        (Some(t2), Some(t1)),
        "the two parties exchanged tables"
    );
    assert_eq!(table_status(&pool, t1).await, "seated");
    assert_eq!(table_status(&pool, t2).await, "seated");

    // Swap with an EMPTY table degenerates to a move (old side bused).
    let resp = post_json!(
        app,
        t,
        "/floor/tables/swap",
        serde_json::json!({ "branch_id": branch, "table_a": t1, "table_b": t3 })
    );
    assert_eq!(resp.status(), 200);
    let tb: Option<Uuid> = sqlx::query_scalar("SELECT table_id FROM held_orders WHERE id=$1")
        .bind(b)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tb, Some(t3));
    assert_eq!(table_status(&pool, t1).await, "free");
    assert_eq!(table_status(&pool, t3).await, "seated");

    // Both empty → a clean 400.
    let resp = post_json!(
        app,
        t,
        "/floor/tables/swap",
        serde_json::json!({ "branch_id": branch, "table_a": t1, "table_b": t4 })
    );
    assert_eq!(resp.status(), 400);

    // A waiter ticket on T4, then a cross-entity swap (held B on T3 ⇄ ticket on T4).
    let resp = post_json!(
        app,
        w,
        "/open-tickets",
        serde_json::json!({
            "branch_id": branch, "table_id": t4,
            "items": [{ "menu_item_id": item, "quantity": 1 }]
        })
    );
    assert_eq!(resp.status(), 201);
    let ticket: OpenTicketView = test::read_body_json(resp).await;
    assert_eq!(ticket.table_id, Some(t4));

    // The WAITER may not move the teller's held order…
    let resp = post_json!(
        app,
        w,
        "/floor/tables/swap",
        serde_json::json!({ "branch_id": branch, "table_a": t3, "table_b": t4 })
    );
    assert_eq!(resp.status(), 403, "waiter lacks held_orders:update");
    // …the teller (holding both permissions) may.
    let resp = post_json!(
        app,
        t,
        "/floor/tables/swap",
        serde_json::json!({ "branch_id": branch, "table_a": t3, "table_b": t4 })
    );
    assert_eq!(resp.status(), 200);
    let tb: Option<Uuid> = sqlx::query_scalar("SELECT table_id FROM held_orders WHERE id=$1")
        .bind(b)
        .fetch_one(&pool)
        .await
        .unwrap();
    let tt: Option<Uuid> = sqlx::query_scalar("SELECT table_id FROM open_tickets WHERE id=$1")
        .bind(ticket.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((tb, tt), (Some(t4), Some(t3)));
}

// ── Ticket-side arbitration ──────────────────────────────────────────────────

#[sqlx::test]
async fn ticket_fire_drops_occupied_table_and_move_conflicts(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let item = seed_menu_item(&pool, org, 1000).await;
    shift_row(&pool, branch, teller).await;
    grant_defaults(&pool).await;
    let t = token(teller, org, UserRole::Teller);
    let w = token(waiter, org, UserRole::Waiter);
    let t1 = seed_table(&pool, org, branch, None, "T1").await;
    let t2 = seed_table(&pool, org, branch, None, "T2").await;

    // A held order owns T1.
    let a = Uuid::new_v4();
    post_json!(
        app,
        t,
        "/held-orders",
        serde_json::json!({
        "id": a, "branch_id": branch, "name": "A", "cart": cart(), "table_id": t1 })
    );

    // A fire onto the occupied T1 still succeeds — table-less (never dead-letters).
    let resp = post_json!(
        app,
        w,
        "/open-tickets",
        serde_json::json!({
            "branch_id": branch, "table_id": t1,
            "items": [{ "menu_item_id": item, "quantity": 1 }]
        })
    );
    assert_eq!(resp.status(), 201);
    let ticket: OpenTicketView = test::read_body_json(resp).await;
    assert_eq!(
        ticket.table_id, None,
        "occupied table is dropped, not fatal"
    );

    // Fire onto free T2 seats it.
    let resp = post_json!(
        app,
        w,
        "/open-tickets",
        serde_json::json!({
            "branch_id": branch, "table_id": t2,
            "items": [{ "menu_item_id": item, "quantity": 1 }]
        })
    );
    let ticket2: OpenTicketView = test::read_body_json(resp).await;
    assert_eq!(ticket2.table_id, Some(t2));
    assert_eq!(table_status(&pool, t2).await, "seated");

    // The interactive ticket move onto the held order's table is a loud 409.
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/open-tickets/{}/table", ticket2.id))
            .insert_header(("Authorization", format!("Bearer {w}")))
            .set_json(&serde_json::json!({ "table_id": t1 }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 409);

    // Voiding the T2 ticket hands the table straight back: a voided ticket
    // never served the party, so there is nothing to bus.
    let resp = post_json!(
        app,
        w,
        &format!("/open-tickets/{}/void", ticket2.id),
        serde_json::json!({ "reason": "test" })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(table_status(&pool, t2).await, "free");
}

/// SETTLING a dine-in ticket is a checkout: the party paid and left their
/// plates, so the table lands `dirty` and waits for a human. This is the
/// contract the POS's post-checkout prompt and its one-tap clear are built on
/// — if settle went back to `free`, both would be lying about the room.
#[sqlx::test]
async fn settling_a_ticket_buses_its_table(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let item = seed_menu_item(&pool, org, 1000).await;
    let shift = open_shift_row(&pool, branch, teller).await;
    seed_cash_method(&pool, org).await;
    grant_defaults(&pool).await;
    for (resource, action) in [
        ("orders", "create"),
        ("payments", "create"),
        ("kitchen_orders", "read"),
        ("kitchen_orders", "update"),
    ] {
        grant(&pool, "teller", resource, action).await;
    }
    let t = token(teller, org, UserRole::Teller);
    let w = token(waiter, org, UserRole::Waiter);
    let t1 = seed_table(&pool, org, branch, None, "T1").await;

    let resp = post_json!(
        app,
        w,
        "/open-tickets",
        serde_json::json!({
            "branch_id": branch, "table_id": t1,
            "items": [{ "menu_item_id": item, "quantity": 1 }]
        })
    );
    assert_eq!(resp.status(), 201);
    let ticket: OpenTicketView = test::read_body_json(resp).await;
    assert_eq!(ticket.table_id, Some(t1));
    assert_eq!(table_status(&pool, t1).await, "seated");

    let resp = post_json!(
        app,
        t,
        &format!("/open-tickets/{}/settle", ticket.id),
        serde_json::json!({ "shift_id": shift, "payment_method": "cash" })
    );
    assert_eq!(resp.status(), 200, "cashier settles the ticket");
    assert_eq!(
        table_status(&pool, t1).await,
        "dirty",
        "a checked-out table needs a bus — it is NOT handed back automatically"
    );

    // Only a human clearing it makes it available again.
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/floor/tables/{t1}/state"))
            .insert_header(("Authorization", format!("Bearer {t}")))
            .set_json(&serde_json::json!({ "status": "free" }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(table_status(&pool, t1).await, "free");
}

// ── Transfer waitlist ────────────────────────────────────────────────────────

#[sqlx::test]
async fn transfer_waitlist_lifecycle(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    grant_defaults(&pool).await;
    let t = token(teller, org, UserRole::Teller);
    let outside = seed_section(&pool, org, branch, "Outside").await;
    let inside = seed_section(&pool, org, branch, "Inside").await;
    let t_out = seed_table(&pool, org, branch, Some(outside), "O1").await;
    let t_in = seed_table(&pool, org, branch, Some(inside), "I1").await;

    // A party parked on the outside table wants "anywhere inside".
    let a = Uuid::new_v4();
    post_json!(
        app,
        t,
        "/held-orders",
        serde_json::json!({
        "id": a, "branch_id": branch, "name": "A", "cart": cart(), "table_id": t_out })
    );
    let wish = Uuid::new_v4();
    let resp = post_json!(
        app,
        t,
        "/floor/transfers",
        serde_json::json!({
            "id": wish, "branch_id": branch, "occupant_kind": "held_order", "occupant_id": a,
            "target_section_id": inside, "note": "crowded out here"
        })
    );
    assert_eq!(resp.status(), 200);
    let view: TransferView = test::read_body_json(resp).await;
    assert_eq!(view.status, "waiting");
    assert_eq!(
        view.from_table_id,
        Some(t_out),
        "current table derived server-side"
    );
    assert_eq!(view.occupant_label.as_deref(), Some("A"));

    // Retrying the SAME create dedups; a SECOND wish for the party conflicts.
    let resp = post_json!(
        app,
        t,
        "/floor/transfers",
        serde_json::json!({
            "id": wish, "branch_id": branch, "occupant_kind": "held_order", "occupant_id": a,
            "target_section_id": inside
        })
    );
    assert_eq!(resp.status(), 200);
    let resp = post_json!(
        app,
        t,
        "/floor/transfers",
        serde_json::json!({
            "id": Uuid::new_v4(), "branch_id": branch, "occupant_kind": "held_order", "occupant_id": a,
            "target_section_id": inside
        })
    );
    assert_eq!(resp.status(), 409);

    // Fulfilling onto a table OUTSIDE the wished section is rejected.
    let resp = post_json!(
        app,
        t,
        &format!("/floor/transfers/{wish}/fulfill"),
        serde_json::json!({ "table_id": t_out })
    );
    assert_eq!(resp.status(), 400);

    // Fulfil onto I1: the party moves, O1 is bused, the wish resolves.
    let resp = post_json!(
        app,
        t,
        &format!("/floor/transfers/{wish}/fulfill"),
        serde_json::json!({ "table_id": t_in })
    );
    assert_eq!(resp.status(), 200);
    let view: TransferView = test::read_body_json(resp).await;
    assert_eq!(view.status, "fulfilled");
    assert_eq!(view.fulfilled_table_id, Some(t_in));
    let ta: Option<Uuid> = sqlx::query_scalar("SELECT table_id FROM held_orders WHERE id=$1")
        .bind(a)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ta, Some(t_in));
    assert_eq!(table_status(&pool, t_out).await, "free");
    assert_eq!(table_status(&pool, t_in).await, "seated");

    // Replayed fulfil is idempotent; cancelling a fulfilled wish conflicts.
    let resp = post_json!(
        app,
        t,
        &format!("/floor/transfers/{wish}/fulfill"),
        serde_json::json!({ "table_id": t_in })
    );
    assert_eq!(resp.status(), 200);
    let resp = post_json!(
        app,
        t,
        &format!("/floor/transfers/{wish}/cancel"),
        serde_json::json!({})
    );
    assert_eq!(resp.status(), 409);
}

#[sqlx::test]
async fn assigning_into_the_wished_section_autofulfills(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    grant_defaults(&pool).await;
    let t = token(teller, org, UserRole::Teller);
    let inside = seed_section(&pool, org, branch, "Inside").await;
    let t_in = seed_table(&pool, org, branch, Some(inside), "I1").await;

    // A table-less ("outside the café") order queues for inside.
    let a = Uuid::new_v4();
    post_json!(
        app,
        t,
        "/held-orders",
        serde_json::json!({
        "id": a, "branch_id": branch, "name": "A", "cart": cart() })
    );
    let wish = Uuid::new_v4();
    let resp = post_json!(
        app,
        t,
        "/floor/transfers",
        serde_json::json!({
            "id": wish, "branch_id": branch, "occupant_kind": "held_order", "occupant_id": a,
            "target_section_id": inside
        })
    );
    assert_eq!(resp.status(), 200);
    let view: TransferView = test::read_body_json(resp).await;
    assert_eq!(
        view.from_table_id, None,
        "an outside order has no from-table"
    );

    // A plain table assignment into the wished section resolves the wish.
    let resp = post_json!(
        app,
        t,
        &format!("/held-orders/{a}/table"),
        serde_json::json!({ "table_id": t_in })
    );
    assert_eq!(resp.status(), 200);
    let status: String =
        sqlx::query_scalar("SELECT status FROM table_transfer_requests WHERE id=$1")
            .bind(wish)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "fulfilled");

    // Discarding the order would have cancelled a waiting wish (auto-cancel).
    let b = Uuid::new_v4();
    post_json!(
        app,
        t,
        "/held-orders",
        serde_json::json!({
        "id": b, "branch_id": branch, "name": "B", "cart": cart() })
    );
    let wish_b = Uuid::new_v4();
    post_json!(
        app,
        t,
        "/floor/transfers",
        serde_json::json!({
            "id": wish_b, "branch_id": branch, "occupant_kind": "held_order", "occupant_id": b,
            "target_section_id": inside
        })
    );
    let resp = post_json!(
        app,
        t,
        &format!("/held-orders/{b}/discard"),
        serde_json::json!({})
    );
    assert_eq!(resp.status(), 200);
    let status: String =
        sqlx::query_scalar("SELECT status FROM table_transfer_requests WHERE id=$1")
            .bind(wish_b)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "cancelled");
}

// ── Replay (offline outbox) ──────────────────────────────────────────────────

#[sqlx::test]
async fn replay_parks_and_swaps_with_role_boundaries(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let waiter = seed_user(&pool, org, "waiter").await;
    grant_defaults(&pool).await;
    let t = token(teller, org, UserRole::Teller);
    let t1 = seed_table(&pool, org, branch, None, "T1").await;
    let t2 = seed_table(&pool, org, branch, None, "T2").await;

    // A queued offline park replays and applies (attributed to the teller).
    let a = Uuid::new_v4();
    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({
            "op": "park_held_order", "teller_id": teller,
            "request": { "id": a, "branch_id": branch, "name": "Q", "cart": cart(), "table_id": t1 }
        })
    );
    assert_eq!(resp.status(), 200);
    let created_by: Option<Uuid> =
        sqlx::query_scalar("SELECT created_by FROM held_orders WHERE id=$1")
            .bind(a)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        created_by,
        Some(teller),
        "attributed to the op's embedded teller"
    );
    assert_eq!(table_status(&pool, t1).await, "seated");

    // Replaying the identical park again (lost ack) is harmless.
    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({
            "op": "park_held_order", "teller_id": teller,
            "request": { "id": a, "branch_id": branch, "name": "Q", "cart": cart(), "table_id": t1 }
        })
    );
    assert_eq!(resp.status(), 200);

    // A WAITER-attributed park is rejected (held orders are the teller's flow).
    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({
            "op": "park_held_order", "teller_id": waiter,
            "request": { "id": Uuid::new_v4(), "branch_id": branch, "name": "X", "cart": cart() }
        })
    );
    assert_eq!(resp.status(), 403);

    // A queued swap (move to the empty T2) replays through the same core.
    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({
            "op": "swap_tables", "teller_id": teller,
            "request": { "branch_id": branch, "table_a": t1, "table_b": t2 }
        })
    );
    assert_eq!(resp.status(), 200);
    let ta: Option<Uuid> = sqlx::query_scalar("SELECT table_id FROM held_orders WHERE id=$1")
        .bind(a)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ta, Some(t2));
}

// ── Table state (POS operational edits: status walk + zone move) ─────────────

#[sqlx::test]
async fn table_state_updates_status_and_section(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let waiter = seed_user(&pool, org, "waiter").await;
    grant_defaults(&pool).await;
    grant(&pool, "teller", "reservations", "update").await;
    grant(&pool, "waiter", "reservations", "update").await;
    let t = token(teller, org, UserRole::Teller);
    let inside = seed_section(&pool, org, branch, "Inside").await;
    let outside = seed_section(&pool, org, branch, "Outside").await;
    let t1 = seed_table(&pool, org, branch, Some(outside), "T1").await;

    // A stale status set by hand still clears (dirty → free).
    sqlx::query("UPDATE branch_tables SET status='dirty' WHERE id=$1")
        .bind(t1)
        .execute(&pool)
        .await
        .unwrap();
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/floor/tables/{t1}/state"))
            .insert_header(("Authorization", format!("Bearer {t}")))
            .set_json(&serde_json::json!({ "status": "free" }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(table_status(&pool, t1).await, "free");

    // Move the physical table inside (zone move keeps everything else).
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/floor/tables/{t1}/state"))
            .insert_header(("Authorization", format!("Bearer {t}")))
            .set_json(&serde_json::json!({ "section_id": inside }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let section: Option<Uuid> =
        sqlx::query_scalar("SELECT section_id FROM branch_tables WHERE id=$1")
            .bind(t1)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(section, Some(inside));

    // clear_section pulls it out of every zone; a bogus status is a 400.
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/floor/tables/{t1}/state"))
            .insert_header(("Authorization", format!("Bearer {t}")))
            .set_json(&serde_json::json!({ "clear_section": true }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let section: Option<Uuid> =
        sqlx::query_scalar("SELECT section_id FROM branch_tables WHERE id=$1")
            .bind(t1)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(section, None);
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/floor/tables/{t1}/state"))
            .insert_header(("Authorization", format!("Bearer {t}")))
            .set_json(&serde_json::json!({ "status": "sticky" }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400);

    // A WAITER replays a queued bussing op (offline outbox) successfully.
    sqlx::query("UPDATE branch_tables SET status='dirty' WHERE id=$1")
        .bind(t1)
        .execute(&pool)
        .await
        .unwrap();
    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({
            "op": "update_table_state", "teller_id": waiter,
            "table_id": t1, "request": { "status": "free" }
        })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(table_status(&pool, t1).await, "free");
}
